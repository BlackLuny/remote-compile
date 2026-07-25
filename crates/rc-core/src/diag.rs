//! Diagnostic extraction and result classification.
//!
//! Getting `result.kind` right matters more than the diagnostics themselves:
//! a coding agent told "compile_error" will edit source code, so reporting a
//! missing linker that way sends it off to break working code (§3.5, risk #4).

use crate::ansi;
use crate::model::{ResultKind, TaskType};
use crate::pb::Diagnostic;

/// Parse `cargo --message-format=json` output. Non-JSON lines (human output
/// that leaked onto stdout) are ignored.
pub fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("error");
        // `failure-note` and `note` are noise unless attached to an error.
        if matches!(level, "note" | "help" | "failure-note") {
            continue;
        }
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let text = msg.get("message").and_then(|m| m.as_str()).unwrap_or_default();
        let rendered = msg.get("rendered").and_then(|r| r.as_str()).unwrap_or_default();
        let (file, line_no, col) = primary_span(msg);
        out.push(Diagnostic {
            level: level.to_string(),
            code: code.to_string(),
            message: text.to_string(),
            file,
            line: line_no,
            column: col,
            rendered: ansi::strip(rendered),
        });
    }
    out
}

fn primary_span(msg: &serde_json::Value) -> (String, u32, u32) {
    let spans = msg.get("spans").and_then(|s| s.as_array());
    let Some(spans) = spans else {
        return (String::new(), 0, 0);
    };
    let pick = spans
        .iter()
        .find(|s| s.get("is_primary").and_then(|p| p.as_bool()).unwrap_or(false))
        .or_else(|| spans.first());
    match pick {
        Some(s) => (
            s.get("file_name").and_then(|f| f.as_str()).unwrap_or_default().to_string(),
            s.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
            s.get("column_start").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
        ),
        None => (String::new(), 0, 0),
    }
}

/// Fallback for adapters with no machine-readable output (§10.3): pull
/// `file:line:col: level: message` shaped lines out of the text.
pub fn parse_generic(text: &str) -> Vec<Diagnostic> {
    static PATTERN: &str = r"(?m)^\s*(?P<file>[^\s:][^:\n]*):(?P<line>\d+)(?::(?P<col>\d+))?:\s*(?P<level>error|warning|fatal error)\b:?\s*(?P<msg>.*)$";
    let re = match regex::Regex::new(PATTERN) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let clean = ansi::strip(text);
    re.captures_iter(&clean)
        .map(|c| Diagnostic {
            level: match c.name("level").map(|m| m.as_str()) {
                Some("warning") => "warning".into(),
                _ => "error".into(),
            },
            code: String::new(),
            message: c.name("msg").map(|m| m.as_str().trim().to_string()).unwrap_or_default(),
            file: c.name("file").map(|m| m.as_str().to_string()).unwrap_or_default(),
            line: c.name("line").and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
            column: c.name("col").and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
            rendered: c.get(0).map(|m| m.as_str().trim().to_string()).unwrap_or_default(),
        })
        .collect()
}

/// Signals that the *environment*, not the code, is broken. Each of these has
/// bitten a real build; the agent must go fix an image, not a source file.
const ENV_ERROR_MARKERS: &[&str] = &[
    "linker `cc` not found",
    "linker `link.exe` not found",
    "cannot find -l",
    "is not installed",
    "no such subcommand",
    "command not found",
    "could not execute process",
    "failed to run custom build command",
    "the `",             // "the `x86_64-...` target may not be installed"
    "toolchain",         // "toolchain '1.85.0' is not installed"
    "error: could not find `Cargo.toml`",
    "permission denied (os error 13)",
    "no space left on device",
    "unable to get packages from source",
    "failed to authenticate",
    "network failure",
    "failed to download",
    "pkg-config",
    "protoc",
    "openssl",
];

/// A marker only counts as an environment problem when it is *not* also a
/// well-formed compiler diagnostic; `parse_*` output wins.
fn looks_like_env_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    ENV_ERROR_MARKERS.iter().any(|m| {
        let m = m.to_lowercase();
        // The bare-word markers need a nearby "not found"/"error" to count.
        match m.as_str() {
            "the `" | "toolchain" | "pkg-config" | "protoc" | "openssl" => {
                lower.contains(&m) && (lower.contains("not found") || lower.contains("not installed") || lower.contains("no such file"))
            }
            _ => lower.contains(&m),
        }
    })
}

/// Whether a parsed "error" is really the environment's fault. The rendered
/// form is checked in preference to the message because that is where the
/// `fatal error:` prefix survives.
///
/// Only the two shapes a *compiler or linker* actually emits count here — an
/// unopenable header and an unresolvable `-l`. The looser prose rules must not
/// reach this decision: `error[E0432]: unresolved import` renders as ``could
/// not find `libfoo` in `bar` ``, which reads as a library to a rule working
/// from prose, and would turn a genuine code error into an environment one.
/// That direction is the more dangerous of the two, because the compile
/// diagnostics the agent needs stop being presented as the thing to fix.
///
/// The text searched is the message plus rustc's own `= note:` / `= help:`
/// annotations — never the quoted source. `error: linking with \`cc\` failed`
/// carries the real cause only in a note (`= note: /usr/bin/ld: cannot find
/// -lssl`), so notes have to be read; but the rendered block also quotes the
/// offending line, and `let _: i32 = "cannot find -lssl";` must not be allowed
/// to make its own type error look like a missing library.
///
/// A diagnostic about a `.rs` file is never one of these. A C header cannot go
/// missing in Rust source, so `compile_error!("openssl/ssl.h: No such file")`
/// — whose message is indistinguishable from the real thing — is excluded by
/// where it comes from rather than by what it says.
///
/// This does cost one real case: a proc macro that wraps a C parser can report
/// a genuinely missing header at its invocation site, in a `.rs` file, and that
/// falls back to `compile_error`. The trade is deliberate. `compile_error!`
/// with arbitrary text is in every crate that validates its feature flags,
/// while a proc macro surfacing a C header is rare; and the fallback is what
/// this code did before it existed, so the rare case loses nothing it had.
fn is_environment_diagnostic(d: &Diagnostic) -> bool {
    use crate::envdep::DepKind;
    if d.file.ends_with(".rs") {
        return false;
    }
    crate::envdep::analyze(&diagnostic_evidence(d))
        .iter()
        .any(|dep| matches!(dep.kind, DepKind::Header | DepKind::Library))
}

/// The part of a diagnostic that may be searched for a missing dependency:
/// its message and rustc's own annotations, never the source it quotes.
///
/// A note runs on past its first line — rustc indents the continuations
/// instead of repeating `= note:`, and for a link failure the useful line is
/// usually one of those:
///
/// ```text
///   = note: /usr/bin/ld: warning: unsupported property
///           /usr/bin/ld: cannot find -lssl
/// ```
///
/// so the note is followed until something that is plainly not part of it: a
/// new annotation, or the source-quoting forms (`12 | …`, `| …`, `--> …`).
fn diagnostic_evidence(d: &Diagnostic) -> String {
    let mut text = d.message.clone();
    let mut in_note = false;
    for line in d.rendered.lines() {
        let t = line.trim_start();
        let starts_note = t.starts_with("= note:") || t.starts_with("= help:");
        // A source snippet is always introduced by `-->`, so that alone ends a
        // note. Testing for the gutter (`|`, `12 | …`) instead would cut the
        // note short on its own content: linker wrappers print tables, and the
        // line that names the missing library often comes after one.
        if starts_note {
            in_note = true;
        } else if t.starts_with("-->") || t.starts_with('=') || !line.starts_with(char::is_whitespace)
        {
            in_note = false;
        }
        if in_note {
            text.push('\n');
            text.push_str(t);
        }
    }
    text
}

pub struct Classification {
    pub kind: ResultKind,
    pub summary: String,
    pub error_count: u32,
    pub warning_count: u32,
    /// For `EnvError` only: what the log says is missing, rendered for an
    /// agent. Empty everywhere else — a compile error is not fixed by adding a
    /// package, and offering one would send the agent down the wrong path.
    pub env_hints: Vec<String>,
}

impl Classification {
    fn new(kind: ResultKind, summary: String, error_count: u32, warning_count: u32) -> Self {
        Classification { kind, summary, error_count, warning_count, env_hints: Vec::new() }
    }
}

/// Decide what actually happened, given the process outcome and whatever the
/// adapter managed to parse.
pub fn classify(
    task_type: TaskType,
    exit_code: i32,
    timed_out: bool,
    diagnostics: &[Diagnostic],
    raw_output: &str,
) -> Classification {
    let error_count = diagnostics.iter().filter(|d| d.level == "error").count() as u32;
    let warning_count = diagnostics.iter().filter(|d| d.level == "warning").count() as u32;

    if timed_out {
        return Classification::new(ResultKind::Timeout, "任务超时被终止".into(), error_count, warning_count);
    }

    if exit_code == 0 {
        let summary = if warning_count > 0 {
            format!("success, {warning_count} warnings")
        } else {
            "success".into()
        };
        return Classification::new(ResultKind::Success, summary, 0, warning_count);
    }

    // Real compiler errors are the strongest signal available — but not
    // everything a compiler reports as an error is a code problem. A missing
    // header is `foo.c:3:10: fatal error: openssl/ssl.h: No such file`, which
    // the generic adapter (§10.3) parses into a perfectly well-formed error
    // diagnostic. Calling that `compile_error` is the failure this module
    // exists to prevent: it sends the agent to edit source that is not wrong.
    //
    // Only when *every* error is of that shape. A log with one missing header
    // and twenty real type errors is a code problem, and hiding those behind
    // an environment verdict would be the same mistake pointing the other way.
    // Built here because both the branch below and the ones after it need it.
    // Telling the agent *what* to add is the difference between one
    // `prepare_env` call and paging a three-thousand-line log for the one line
    // that names the library.
    let env = |summary: String| {
        // Both sources, together. Some evidence is only in the raw stream and
        // some only in a parsed diagnostic — a link failure keeps its cause in
        // a note — and analysing them jointly is what lets one rank against
        // the other. Taking the diagnostics only when the raw stream came up
        // empty would let any incidental finding there hide the real cause.
        let evidence = diagnostics.iter().map(diagnostic_evidence).collect::<Vec<_>>().join("\n");
        let deps = crate::envdep::analyze_parts(&[raw_output, &evidence]);
        Classification {
            kind: ResultKind::EnvError,
            summary,
            error_count,
            warning_count,
            env_hints: crate::envdep::hint_lines(&deps),
        }
    };

    if error_count > 0 {
        let all_environmental = diagnostics
            .iter()
            .filter(|d| d.level == "error")
            .all(is_environment_diagnostic);
        if !all_environmental {
            return Classification::new(
                ResultKind::CompileError,
                format!("{error_count} errors, {warning_count} warnings"),
                error_count,
                warning_count,
            );
        }
        // Returned here rather than falling through, so that a `test` run is
        // not re-labelled a failing test on the way past.
        return env(format!("环境错误（exit {exit_code}）：{}", first_error_line(raw_output)));
    }

    if looks_like_env_error(raw_output) {
        return env(format!("环境错误（exit {exit_code}）：{}", first_error_line(raw_output)));
    }

    // Non-zero with no diagnostics: for test runs that is a failing test
    // (a code problem); otherwise the toolchain fell over in a way we cannot
    // attribute, which is an environment problem the agent should inspect.
    match task_type {
        TaskType::Test => Classification::new(
            ResultKind::CompileError,
            format!("测试失败（exit {exit_code}）：{}", first_error_line(raw_output)),
            error_count.max(1),
            warning_count,
        ),
        _ => env(format!(
            "命令以 exit {exit_code} 结束且无结构化诊断：{}",
            first_error_line(raw_output)
        )),
    }
}

/// Most informative single line for a one-line summary.
pub fn first_error_line(text: &str) -> String {
    let clean = ansi::strip(text);
    clean
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error") || l.contains("error:") || l.starts_with("fatal"))
        .or_else(|| clean.lines().map(str::trim).rev().find(|l| !l.is_empty()))
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// Tail of the log, used as the L1 summary when no adapter could parse
/// anything structured (§10.3).
pub fn tail_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| ansi::strip(l))
        .collect()
}

/// Errors first, then warnings, truncated to `limit` (§11 L1).
pub fn top_diagnostics(diags: &[Diagnostic], limit: usize) -> (Vec<Diagnostic>, u32) {
    let mut sorted: Vec<Diagnostic> = diags.to_vec();
    sorted.sort_by_key(|d| match d.level.as_str() {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    });
    let truncated = sorted.len().saturating_sub(limit) as u32;
    sorted.truncate(limit);
    (sorted, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_ERR: &str = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/main.rs","line_start":7,"column_start":9,"is_primary":true}],"rendered":"\u001b[31merror[E0308]\u001b[0m: mismatched types"}}
{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"unused variable: `x`","spans":[{"file_name":"src/lib.rs","line_start":2,"column_start":5,"is_primary":true}],"rendered":"warning: unused variable"}}
{"reason":"build-finished","success":false}"#;

    #[test]
    fn parses_cargo_diagnostics() {
        let d = parse_cargo_json(CARGO_ERR);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].code, "E0308");
        assert_eq!(d[0].file, "src/main.rs");
        assert_eq!(d[0].line, 7);
        // rendered must be ANSI-free before it reaches an agent's context
        assert!(!d[0].rendered.contains('\u{1b}'));
    }

    #[test]
    fn ignores_non_json_noise() {
        let mixed = format!("   Compiling foo v0.1.0\n{CARGO_ERR}\nerror: could not compile");
        assert_eq!(parse_cargo_json(&mixed).len(), 2);
    }

    #[test]
    fn compiler_errors_classify_as_compile_error() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `foo`");
        assert_eq!(c.kind, ResultKind::CompileError);
        assert_eq!(c.error_count, 1);
        assert_eq!(c.warning_count, 1);
    }

    #[test]
    fn a_missing_linker_is_an_env_error_not_a_code_error() {
        // Risk #4: misreporting this makes the agent edit working source.
        let c = classify(TaskType::Check, 101, false, &[], "error: linker `cc` not found");
        assert_eq!(c.kind, ResultKind::EnvError);
    }

    #[test]
    fn an_env_error_carries_what_the_log_says_is_missing() {
        let log = "error: failed to run custom build command for `rrd-sys v0.1.3`\n  Could not find librrd";
        let c = classify(TaskType::Check, 101, false, &[], log);
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("librrd-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn a_missing_header_is_not_a_code_problem_even_though_it_parses_as_one() {
        // The generic adapter (§10.3) scrapes `foo.c:3:10: fatal error: …` into
        // a well-formed error diagnostic. Reporting that as `compile_error`
        // sends the agent to edit source that is not wrong — risk #4 exactly.
        let d = parse_generic("wrapper.c:3:10: fatal error: openssl/ssl.h: No such file or directory");
        assert_eq!(d.len(), 1);
        let c = classify(TaskType::Build, 2, false, &d, "make: *** [all] Error 1");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("libssl-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn real_errors_alongside_a_missing_header_stay_a_code_problem() {
        // Hiding twenty type errors behind an environment verdict is the same
        // misclassification pointing the other way.
        let d = parse_generic(
            "wrapper.c:3:10: fatal error: openssl/ssl.h: No such file or directory\n\
             src/x.c:9:1: error: expected ';' before '}' token",
        );
        let c = classify(TaskType::Build, 2, false, &d, "");
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.env_hints.is_empty());
    }

    #[test]
    fn an_unresolved_import_that_looks_like_a_library_stays_a_code_problem() {
        // `use libfoo::x;` that does not resolve renders as "could not find
        // `libfoo` in `bar`". A rule reading prose sees a library there. If
        // that reached the classifier the agent would be told to go build a
        // Docker image instead of fixing the import.
        let d = vec![Diagnostic {
            level: "error".into(),
            code: "E0432".into(),
            message: "unresolved import `bar::libfoo`".into(),
            rendered: "error[E0432]: unresolved import `bar::libfoo`\n  could not find `libfoo` in `bar`".into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::CompileError);
    }

    #[test]
    fn source_quoted_in_a_diagnostic_is_not_read_as_a_missing_library() {
        // `let _: i32 = "cannot find -lssl";` puts that text into the rendered
        // block of its own type error. Reading the rendered form would turn a
        // plain E0308 into an env_error advising libssl-dev, and bury the type
        // error the agent actually has to fix.
        let d = vec![Diagnostic {
            level: "error".into(),
            code: "E0308".into(),
            message: "mismatched types".into(),
            file: "src/main.rs".into(),
            line: 2,
            rendered: "error[E0308]: mismatched types\n 2 |     let _: i32 = \"cannot find -lssl\";\n   |                  ^^^^^^^^^^^^^^^^^^^ expected `i32`, found `&str`".into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.env_hints.is_empty());
    }

    #[test]
    fn a_link_failure_finds_its_cause_in_the_notes() {
        // Cargo reports `error: linking with `cc` failed` and puts the actual
        // cause in a note. The note is rustc's own text, not quoted source, so
        // it can be read without reopening the hole above.
        let d = vec![Diagnostic {
            level: "error".into(),
            message: "linking with `cc` failed: exit status: 1".into(),
            rendered: "error: linking with `cc` failed: exit status: 1\n  = note: /usr/bin/ld: cannot find -lssl\n".into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Build, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("libssl-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn a_note_is_followed_past_its_first_line() {
        // rustc indents a note's continuations instead of repeating the
        // prefix, and for a link failure the useful line is rarely the first.
        let d = vec![Diagnostic {
            level: "error".into(),
            message: "linking with `cc` failed: exit status: 1".into(),
            rendered: "error: linking with `cc` failed: exit status: 1\n  \
                       = note: /usr/bin/ld: warning: unsupported property\n          \
                       /usr/bin/ld: cannot find -lssl\n          \
                       collect2: error: ld returned 1 exit status\n"
                .into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Build, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("libssl-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn a_note_containing_a_table_is_not_cut_short_by_it() {
        // Linker wrappers print tabular output inside a note, and the line
        // naming the missing library comes after it. Ending the note at the
        // first `|` discarded exactly that line.
        let d = vec![Diagnostic {
            level: "error".into(),
            message: "linking with `cc` failed: exit status: 1".into(),
            rendered: "error: linking with `cc` failed\n  \
                       = note: linker-wrapper output:\n          \
                       | target | status |\n          \
                       /usr/bin/ld: cannot find -lssl\n"
                .into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Build, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("libssl-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn quoted_source_is_still_excluded_when_a_note_precedes_it() {
        // Following a note must not run on into the source-quoting block.
        let d = vec![Diagnostic {
            level: "error".into(),
            code: "E0308".into(),
            message: "mismatched types".into(),
            file: "src/main.rs".into(),
            rendered: "error[E0308]: mismatched types\n  \
                       = note: expected type `i32`\n \
                       --> src/main.rs:2:18\n  \
                       |\n\
                       2 |     let _: i32 = \"cannot find -lssl\";\n  \
                       |                  ^^^^\n"
                .into(),
            ..Default::default()
        }];
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::CompileError);
    }

    #[test]
    fn evidence_in_a_note_is_not_hidden_by_an_unrelated_finding_in_the_raw_log() {
        // The raw stream naming some other failing crate must not stop the
        // diagnostic's note from being read — merging the two is the point.
        let d = vec![Diagnostic {
            level: "error".into(),
            message: "linking with `cc` failed: exit status: 1".into(),
            rendered: "error: linking with `cc` failed\n  = note: /usr/bin/ld: cannot find -lssl\n"
                .into(),
            ..Default::default()
        }];
        let raw = "error: failed to run custom build command for `other-sys v1`\n\
                   error: could not compile `app`";
        let c = classify(TaskType::Build, 101, false, &d, raw);
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(c.env_hints.join("\n").contains("libssl-dev"), "{:?}", c.env_hints);
    }

    #[test]
    fn a_rust_diagnostic_is_never_read_as_a_missing_c_header() {
        // `compile_error!("openssl/ssl.h: No such file or directory")` produces
        // a message identical to the real thing. What separates them is where
        // it came from: a C header cannot go missing in a `.rs` file.
        let d = vec![Diagnostic {
            level: "error".into(),
            message: "openssl/ssl.h: No such file or directory".into(),
            file: "src/main.rs".into(),
            line: 2,
            ..Default::default()
        }];
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `x`");
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.env_hints.is_empty());
    }

    #[test]
    fn a_compile_error_is_never_offered_a_package_to_install() {
        // The log of a genuine compile error can still mention openssl or a
        // linker; suggesting a package there would send the agent to build an
        // image instead of fixing the code it actually broke.
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Check, 101, false, &d, "cannot find -lssl\nerror: could not compile");
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.env_hints.is_empty());
    }

    #[test]
    fn a_missing_cargo_subcommand_is_an_env_error() {
        let c = classify(TaskType::Clippy, 101, false, &[], "error: no such subcommand: `clippy`");
        assert_eq!(c.kind, ResultKind::EnvError);
    }

    #[test]
    fn timeouts_win_over_everything() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Build, 137, true, &d, "");
        assert_eq!(c.kind, ResultKind::Timeout);
    }

    #[test]
    fn warnings_alone_still_succeed() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Check, 0, false, &d, "");
        assert_eq!(c.kind, ResultKind::Success);
        assert_eq!(c.warning_count, 1);
        assert_eq!(c.summary, "success, 1 warnings");
    }

    #[test]
    fn failing_tests_are_a_code_problem() {
        let c = classify(TaskType::Test, 1, false, &[], "test result: FAILED. 1 passed; 1 failed");
        assert_eq!(c.kind, ResultKind::CompileError);
    }

    #[test]
    fn generic_parser_handles_gcc_style() {
        let d = parse_generic("src/x.c:12:5: error: expected ';' before '}' token\nsrc/y.c:3:1: warning: unused");
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].file, "src/x.c");
        assert_eq!(d[0].line, 12);
        assert_eq!(d[1].level, "warning");
    }

    #[test]
    fn top_diagnostics_puts_errors_first_and_reports_truncation() {
        let mut ds = vec![Diagnostic { level: "warning".into(), ..Default::default() }; 12];
        ds.push(Diagnostic { level: "error".into(), message: "boom".into(), ..Default::default() });
        let (top, truncated) = top_diagnostics(&ds, 10);
        assert_eq!(top[0].level, "error");
        assert_eq!(top.len(), 10);
        assert_eq!(truncated, 3);
    }
}
