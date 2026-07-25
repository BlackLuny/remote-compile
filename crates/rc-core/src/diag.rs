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

pub struct Classification {
    pub kind: ResultKind,
    pub summary: String,
    pub error_count: u32,
    pub warning_count: u32,
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
        return Classification {
            kind: ResultKind::Timeout,
            summary: "任务超时被终止".into(),
            error_count,
            warning_count,
        };
    }

    if exit_code == 0 {
        let summary = if warning_count > 0 {
            format!("success, {warning_count} warnings")
        } else {
            "success".into()
        };
        return Classification {
            kind: ResultKind::Success,
            summary,
            error_count: 0,
            warning_count,
        };
    }

    // Real compiler errors are the strongest signal available.
    if error_count > 0 {
        return Classification {
            kind: ResultKind::CompileError,
            summary: format!("{error_count} errors, {warning_count} warnings"),
            error_count,
            warning_count,
        };
    }

    if looks_like_env_error(raw_output) {
        return Classification {
            kind: ResultKind::EnvError,
            summary: format!("环境错误（exit {exit_code}）：{}", first_error_line(raw_output)),
            error_count,
            warning_count,
        };
    }

    // Non-zero with no diagnostics: for test runs that is a failing test
    // (a code problem); otherwise the toolchain fell over in a way we cannot
    // attribute, which is an environment problem the agent should inspect.
    match task_type {
        TaskType::Test => Classification {
            kind: ResultKind::CompileError,
            summary: format!("测试失败（exit {exit_code}）：{}", first_error_line(raw_output)),
            error_count: error_count.max(1),
            warning_count,
        },
        _ => Classification {
            kind: ResultKind::EnvError,
            summary: format!("命令以 exit {exit_code} 结束且无结构化诊断：{}", first_error_line(raw_output)),
            error_count,
            warning_count,
        },
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
