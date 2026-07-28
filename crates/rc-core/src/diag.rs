//! Diagnostic extraction and evidence-backed result classification.
//!
//! Getting attribution right matters more than the diagnostics themselves:
//! a coding agent told "code problem" will edit source, so reporting OOM or a
//! missing linker that way sends it off to break working code (§3.5, risk #4).
//!
//! Invariant (I1): never convict without evidence. ATTR_CODE is only produced
//! by rules that require hard evidence (structured diagnostics / libtest
//! summary). Everything else either names a specific non-code cause or falls
//! through to UNKNOWN.

use crate::ansi;
use crate::model::{ResultKind, TaskType};
use crate::pb::{
    Attribution, Diagnostic, Evidence, ExecEvidence, Status, TestSummary, Verdict,
};

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

// ---------------------------------------------------------------------------
// Libtest summary (minimal; used as classification evidence in PR1)
// ---------------------------------------------------------------------------

/// Accumulate libtest `test result:` lines and the `failures:` name list.
///
/// Binary name is taken from the most recent `Running … (…/deps/<name>-*)`
/// line so failed names can be reported as `binary::test_name`.
pub fn parse_test_summary(log: &str) -> TestSummary {
    let clean = ansi::strip(log);
    let summary_re = regex::Regex::new(
        r"(?m)^test result:\s*(ok|FAILED)\.\s*(\d+)\s+passed;\s*(\d+)\s+failed;\s*(\d+)\s+ignored",
    )
    .expect("static regex");
    let running_re =
        regex::Regex::new(r"(?m)^\s*Running\s+\S+\s+\([^)]*/deps/([^/\s-]+)-").expect("static regex");

    let mut summary = TestSummary {
        summary_seen: false,
        ..Default::default()
    };
    let mut current_binary = String::new();
    let mut in_failures_block = false;
    let mut failed_names: Vec<String> = Vec::new();

    for line in clean.lines() {
        if let Some(c) = running_re.captures(line) {
            current_binary = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            in_failures_block = false;
            continue;
        }
        if let Some(c) = summary_re.captures(line) {
            summary.summary_seen = true;
            summary.binaries = summary.binaries.saturating_add(1);
            summary.passed += c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            summary.failed += c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            summary.ignored += c.get(4).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            in_failures_block = false;
            continue;
        }
        // The name list is the block that starts with a lone `failures:` and
        // lists test names indented, ending at blank / `test result` / `----`.
        let trimmed = line.trim();
        if trimmed == "failures:" {
            in_failures_block = true;
            continue;
        }
        if in_failures_block {
            if trimmed.is_empty()
                || trimmed.starts_with("test result:")
                || trimmed.starts_with("----")
                || trimmed.starts_with("error:")
            {
                in_failures_block = false;
                continue;
            }
            // Skip the per-test stdout header form `---- name stdout ----`.
            if trimmed.contains(" stdout ----") || trimmed.contains(" stderr ----") {
                in_failures_block = false;
                continue;
            }
            let name = if current_binary.is_empty() {
                trimmed.to_string()
            } else {
                format!("{current_binary}::{trimmed}")
            };
            if failed_names.len() < 20 {
                failed_names.push(name);
            } else {
                summary.truncated = true;
            }
        }
    }
    summary.failed_names = failed_names;
    summary
}

// ---------------------------------------------------------------------------
// Environment markers (raw-log only; never override structured code errors)
// ---------------------------------------------------------------------------

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
    // "no space left on device" is rule 8 (INFRA), not PROJECT_CONFIG.
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
                lower.contains(&m)
                    && (lower.contains("not found")
                        || lower.contains("not installed")
                        || lower.contains("no such file"))
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

// ---------------------------------------------------------------------------
// Classification (ordered rule table, precision-first)
// ---------------------------------------------------------------------------

pub struct Classification {
    pub kind: ResultKind,
    pub summary: String,
    pub error_count: u32,
    pub warning_count: u32,
    /// For PROJECT_CONFIG env_error only: what the log says is missing,
    /// rendered for an agent. Empty everywhere else — a compile error is not
    /// fixed by adding a package, and offering one would send the agent down
    /// the wrong path.
    pub env_hints: Vec<String>,
    pub verdict: Verdict,
    pub test_summary: Option<TestSummary>,
}

impl Classification {
    fn with_verdict(
        kind: ResultKind,
        summary: String,
        error_count: u32,
        warning_count: u32,
        verdict: Verdict,
    ) -> Self {
        Classification {
            kind,
            summary,
            error_count,
            warning_count,
            env_hints: Vec::new(),
            verdict,
            test_summary: None,
        }
    }
}

/// Inputs to the ordered rule table. Callers that only have a log build
/// `log_tail` from its last 200 lines and may leave `exec` at defaults.
pub struct Facts<'a> {
    pub task_type: TaskType,
    pub exit_code: i32,
    pub timed_out: bool,
    pub exec: &'a ExecEvidence,
    pub diagnostics: &'a [Diagnostic],
    pub test_summary: Option<&'a TestSummary>,
    /// Last ≤200 lines of the combined log (references into a buffer).
    pub log_tail: &'a [&'a str],
    /// Full log only for env_hints analysis; classification rules use log_tail.
    pub raw_output: &'a str,
}

/// §1.3 mapping: status + attribution → legacy ResultKind.
pub fn kind_from_verdict(status: Status, attribution: Attribution) -> ResultKind {
    match status {
        Status::Success => ResultKind::Success,
        Status::Timeout => ResultKind::Timeout,
        Status::Canceled => ResultKind::InfraError, // should not produce TaskResult
        Status::Failed | Status::Unspecified => match attribution {
            Attribution::AttrCode => ResultKind::CompileError,
            Attribution::AttrInfra => ResultKind::InfraError,
            // PROJECT_CONFIG / RESOURCE / UNKNOWN all map to env_error
            // (non-cacheable; RESOURCE remediation is rendered separately).
            Attribution::AttrProjectConfig
            | Attribution::AttrResource
            | Attribution::AttrUnknown => ResultKind::EnvError,
        },
    }
}

/// Agent-facing next-step text keyed by attribution (new readers).
pub fn agent_hint_for(status: Status, attribution: Attribution) -> &'static str {
    match status {
        Status::Success => "编译通过，继续。",
        Status::Timeout => "任务超时：拆分任务或调大 profile 中的 timeout_secs。",
        Status::Canceled => "任务已取消。",
        Status::Failed | Status::Unspecified => match attribution {
            Attribution::AttrCode => "代码问题：按结构化诊断/失败测试修改源码。",
            Attribution::AttrProjectConfig => {
                "环境缺依赖：用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile。"
            }
            Attribution::AttrResource => {
                "非代码问题；资源不足（OOM/疑似 OOM）。可降配 debuginfo 后重试。"
            }
            Attribution::AttrInfra => {
                "基础设施故障，系统已自动换机重试并耗尽次数；无需修改代码，稍后重试。"
            }
            Attribution::AttrUnknown => "原因未知；请 get_log 查看证据摘录后再决定。",
        },
    }
}

fn truncate_excerpt(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn evidence(source: &str, excerpt: &str, line_no: u32) -> Option<Evidence> {
    Some(Evidence {
        source: source.into(),
        excerpt: truncate_excerpt(excerpt, 400),
        line_no,
    })
}

fn find_line_in_tail(log_tail: &[&str], needle: &str) -> Option<(usize, String)> {
    // line_no is 1-based index within the tail window when we only have the
    // tail; callers that care about absolute numbers pass a full log and set
    // offsets themselves. Absolute numbering is recovered in the worker by
    // scanning the full combined log for the same needle.
    for (i, line) in log_tail.iter().enumerate().rev() {
        if line.contains(needle) {
            return Some((i + 1, (*line).to_string()));
        }
    }
    None
}

/// Absolute 1-based line number of the first occurrence of `needle` in `log`.
pub fn line_no_of(log: &str, needle: &str) -> u32 {
    for (i, line) in log.lines().enumerate() {
        if line.contains(needle) {
            return (i + 1) as u32;
        }
    }
    0
}

fn make_verdict(
    st: Status,
    attr: Attribution,
    rule: &str,
    evidence: Option<Evidence>,
    remediation: Vec<String>,
) -> Verdict {
    Verdict {
        status: st as i32,
        attribution: attr as i32,
        evidence,
        remediation,
        rule: rule.into(),
    }
}

fn env_hints_from(diagnostics: &[Diagnostic], raw_output: &str) -> Vec<String> {
    let evidence = diagnostics
        .iter()
        .map(diagnostic_evidence)
        .collect::<Vec<_>>()
        .join("\n");
    let deps = crate::envdep::analyze_parts(&[raw_output, &evidence]);
    crate::envdep::hint_lines(&deps)
}

/// Decide what actually happened, given the process outcome and whatever the
/// adapter managed to parse. Ordered rule table — first match wins.
pub fn classify_facts(facts: Facts<'_>) -> Classification {
    let error_count = facts.diagnostics.iter().filter(|d| d.level == "error").count() as u32;
    let warning_count = facts.diagnostics.iter().filter(|d| d.level == "warning").count() as u32;
    let log_joined = facts.log_tail.join("\n");

    // Rule 0: success (carry TestSummary when the enabled TestContract parsed one)
    if facts.exit_code == 0 && !facts.timed_out {
        let (summary, test_summary) = if let Some(ts) = facts.test_summary {
            if ts.summary_seen {
                (
                    format!(
                        "测试通过：{} passed, {} ignored（{} 个二进制）",
                        ts.passed, ts.ignored, ts.binaries
                    ),
                    Some(ts.clone()),
                )
            } else {
                let s = if warning_count > 0 {
                    format!("success, {warning_count} warnings")
                } else {
                    "success".into()
                };
                (s, Some(ts.clone()))
            }
        } else {
            let s = if warning_count > 0 {
                format!("success, {warning_count} warnings")
            } else {
                "success".into()
            };
            (s, None)
        };
        let v = make_verdict(Status::Success, Attribution::AttrUnknown, "success", None, vec![]);
        let mut c = Classification::with_verdict(
            ResultKind::Success,
            summary,
            0,
            warning_count,
            v,
        );
        c.test_summary = test_summary;
        return c;
    }

    // Rule 1: timeout — beats everything, including diagnostics.
    if facts.timed_out {
        let v = make_verdict(
            Status::Timeout,
            Attribution::AttrInfra,
            "timeout",
            evidence("outcome", "task timed out (worker hard timeout)", 0),
            vec!["拆分任务或调大 profile 中的 timeout_secs".into()],
        );
        return Classification::with_verdict(
            ResultKind::Timeout,
            "任务超时被终止".into(),
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 2 / 3: structured error diagnostics
    if error_count > 0 {
        let all_environmental = facts
            .diagnostics
            .iter()
            .filter(|d| d.level == "error")
            .all(is_environment_diagnostic);
        if all_environmental {
            // Rule 2: env_diagnostics → PROJECT_CONFIG
            let summary = format!(
                "环境错误（exit {}）：{}",
                facts.exit_code,
                first_error_line(facts.raw_output)
            );
            let excerpt = first_error_line(facts.raw_output);
            let v = make_verdict(
                Status::Failed,
                Attribution::AttrProjectConfig,
                "env_diagnostics",
                evidence("diagnostic", &excerpt, 0),
                vec!["用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile".into()],
            );
            let mut c = Classification::with_verdict(
                ResultKind::EnvError,
                summary,
                error_count,
                warning_count,
                v,
            );
            c.env_hints = env_hints_from(facts.diagnostics, facts.raw_output);
            return c;
        }
        // Rule 3: compile_error → CODE (invariant: never reclassified by raw markers)
        let summary = format!("{error_count} errors, {warning_count} warnings");
        let first = facts
            .diagnostics
            .iter()
            .find(|d| d.level == "error")
            .map(|d| {
                if d.file.is_empty() {
                    d.message.clone()
                } else {
                    format!("{}:{} {}", d.file, d.line, d.message)
                }
            })
            .unwrap_or_default();
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrCode,
            "compile_error",
            evidence("diagnostic", &first, 0),
            vec!["按结构化诊断修改源码".into()],
        );
        return Classification::with_verdict(
            ResultKind::CompileError,
            summary,
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 4: test_failed — only when TestContract parser was enabled (facts
    // already carries that gated summary; never parse for non-test / overrides).
    if let Some(ts) = facts.test_summary {
        if ts.summary_seen && ts.failed > 0 && facts.task_type == TaskType::Test {
            let names = if ts.failed_names.is_empty() {
                String::new()
            } else {
                format!("：{}", ts.failed_names.iter().take(5).cloned().collect::<Vec<_>>().join(", "))
            };
            let summary = format!(
                "测试失败：{} passed, {} failed, {} ignored（{} 个二进制）{names}",
                ts.passed, ts.failed, ts.ignored, ts.binaries
            );
            let excerpt = format!(
                "test summary: {} failed of {} binaries",
                ts.failed, ts.binaries
            );
            let v = make_verdict(
                Status::Failed,
                Attribution::AttrCode,
                "test_failed",
                evidence("outcome", &excerpt, 0),
                vec!["按失败用例修改源码".into()],
            );
            let mut c = Classification::with_verdict(
                ResultKind::CompileError,
                summary,
                error_count.max(1),
                warning_count,
                v,
            );
            c.test_summary = Some(ts.clone());
            return c;
        }
    }

    // Rule 5: oom_killed (gold evidence from docker inspect)
    if facts.exec.oom_killed {
        let summary = format!(
            "进程被 OOM killer 终止（exit {}）",
            facts.exit_code
        );
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrResource,
            "oom_killed",
            evidence("docker_state", "OOMKilled=true", 0),
            vec!["非代码问题。可降配 debuginfo 后重试".into()],
        );
        return Classification::with_verdict(
            ResultKind::EnvError,
            summary,
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 6: sigkill_suspected_oom — cargo's exact signal format only
    const SIGKILL_CARGO: &str = "(signal: 9, SIGKILL: kill)";
    if log_joined.contains(SIGKILL_CARGO) {
        let (line_no, line) = find_line_in_tail(facts.log_tail, SIGKILL_CARGO)
            .map(|(n, l)| (line_no_of(facts.raw_output, SIGKILL_CARGO).max(n as u32), l))
            .unwrap_or((0, SIGKILL_CARGO.into()));
        let abs = line_no_of(facts.raw_output, SIGKILL_CARGO);
        let line_no = if abs > 0 { abs } else { line_no };
        let summary = format!(
            "编译时进程被 SIGKILL 杀死 [资源不足/疑似 OOM]（exit {}）",
            facts.exit_code
        );
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrResource,
            "sigkill_suspected_oom",
            evidence("log_line", &line, line_no),
            vec!["非代码问题。已允许以 CARGO_BUILD_JOBS=2 降并发自动补救".into()],
        );
        return Classification::with_verdict(
            ResultKind::EnvError,
            summary,
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 7: rustc_crash — SIGSEGV / SIGABRT both require compile-failure context
    const SIGSEGV: &str = "(signal: 11, SIGSEGV";
    const SIGABRT: &str = "(signal: 6, SIGABRT";
    let has_compile_ctx = log_joined.contains("error: could not compile");
    let crash_needle = if has_compile_ctx && log_joined.contains(SIGSEGV) {
        Some(SIGSEGV)
    } else if has_compile_ctx && log_joined.contains(SIGABRT) {
        Some(SIGABRT)
    } else {
        None
    };
    if let Some(needle) = crash_needle {
        let abs = line_no_of(facts.raw_output, needle);
        let line = find_line_in_tail(facts.log_tail, needle)
            .map(|(_, l)| l)
            .unwrap_or_else(|| needle.into());
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrInfra,
            "rustc_crash",
            evidence("log_line", &line, abs),
            vec!["工具链崩溃；重试或向运维报告".into()],
        );
        return Classification::with_verdict(
            ResultKind::InfraError,
            format!("工具链崩溃（exit {}）：{}", facts.exit_code, first_error_line(facts.raw_output)),
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 8: disk_full → INFRA (machine swap is useful; DESIGN §3.5)
    const DISK_FULL: &str = "No space left on device";
    if log_joined.to_lowercase().contains(&DISK_FULL.to_lowercase()) {
        let abs = line_no_of(facts.raw_output, DISK_FULL);
        let line = find_line_in_tail(facts.log_tail, DISK_FULL)
            .map(|(_, l)| l)
            .unwrap_or_else(|| DISK_FULL.into());
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrInfra,
            "disk_full",
            evidence("log_line", &line, abs),
            vec!["磁盘满；系统将换机重试".into()],
        );
        return Classification::with_verdict(
            ResultKind::InfraError,
            format!("磁盘满（exit {}）", facts.exit_code),
            error_count,
            warning_count,
            v,
        );
    }

    // Rule 9: env_markers_raw — only log_tail (last 200 lines); full raw for hints only
    if looks_like_env_error(&log_joined) {
        let excerpt = first_error_line(&log_joined);
        let summary = format!(
            "环境错误（exit {}）：{}",
            facts.exit_code, excerpt
        );
        // Line number relative to the full log when the excerpt appears there.
        let abs = line_no_of(facts.raw_output, &excerpt);
        let v = make_verdict(
            Status::Failed,
            Attribution::AttrProjectConfig,
            "env_markers_raw",
            evidence("log_line", &excerpt, abs),
            vec!["用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile".into()],
        );
        let mut c = Classification::with_verdict(
            ResultKind::EnvError,
            summary,
            error_count,
            warning_count,
            v,
        );
        c.env_hints = env_hints_from(facts.diagnostics, facts.raw_output);
        return c;
    }

    // Rule 10: unknown — never convict
    let tail_excerpt: Vec<&str> = facts.log_tail.iter().copied().rev().take(5).collect();
    let tail_excerpt: Vec<&str> = tail_excerpt.into_iter().rev().collect();
    let excerpt = tail_excerpt.join("\n");
    let summary = format!(
        "失败原因未知（exit {}）：{}",
        facts.exit_code,
        first_error_line(facts.raw_output)
    );
    let v = make_verdict(
        Status::Failed,
        Attribution::AttrUnknown,
        "unknown",
        evidence("outcome", &excerpt, 0),
        vec![format!(
            "原因未知（exit {}）；请 get_log 查看完整日志",
            facts.exit_code
        )],
    );
    let mut c = Classification::with_verdict(
        ResultKind::EnvError,
        summary,
        error_count,
        warning_count,
        v,
    );
    if let Some(ts) = facts.test_summary {
        c.test_summary = Some(ts.clone());
    }
    c
}

/// Convenience wrapper: assumes the default command (parser enabled for test).
pub fn classify(
    task_type: TaskType,
    exit_code: i32,
    timed_out: bool,
    diagnostics: &[Diagnostic],
    raw_output: &str,
) -> Classification {
    classify_with_exec(
        task_type,
        exit_code,
        timed_out,
        &ExecEvidence::default(),
        diagnostics,
        raw_output,
        /* command_is_default */ task_type == TaskType::Test,
    )
}

/// Like [`classify`], with docker-level evidence and the F24 parser gate.
///
/// TestSummary always comes from [`crate::contract::TaskContract::parse_outcome`]
/// so the production path and the trait stay wired together (R1').
///
/// `command_is_default` must be true only for `task=test` with no request or
/// profile command override — use [`crate::contract::command_is_default`].
pub fn classify_with_exec(
    task_type: TaskType,
    exit_code: i32,
    timed_out: bool,
    exec: &ExecEvidence,
    diagnostics: &[Diagnostic],
    raw_output: &str,
    command_is_default: bool,
) -> Classification {
    use crate::contract::{for_task, TaskOutcome};
    // Sole production call site for TaskContract::parse_outcome (R1').
    let test_summary = match for_task(task_type).parse_outcome(exit_code, raw_output, command_is_default)
    {
        TaskOutcome::Test { summary, parsed: true } if summary.summary_seen => Some(summary),
        _ => None,
    };
    let lines: Vec<&str> = raw_output.lines().collect();
    let tail_start = lines.len().saturating_sub(200);
    let log_tail = &lines[tail_start..];
    classify_facts(Facts {
        task_type,
        exit_code,
        timed_out,
        exec,
        diagnostics,
        test_summary: test_summary.as_ref(),
        log_tail,
        raw_output,
    })
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

    fn attr(c: &Classification) -> Attribution {
        Attribution::try_from(c.verdict.attribution).unwrap_or(Attribution::AttrUnknown)
    }
    fn rule(c: &Classification) -> &str {
        &c.verdict.rule
    }
    fn st(c: &Classification) -> Status {
        Status::try_from(c.verdict.status).unwrap_or(Status::Unspecified)
    }

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
        assert_eq!(attr(&c), Attribution::AttrCode);
        assert_eq!(rule(&c), "compile_error");
    }

    #[test]
    fn a_missing_linker_is_an_env_error_not_a_code_error() {
        // Risk #4: misreporting this makes the agent edit working source.
        let c = classify(TaskType::Check, 101, false, &[], "error: linker `cc` not found");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert_eq!(attr(&c), Attribution::AttrProjectConfig);
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
        assert_eq!(rule(&c), "env_diagnostics");
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
        assert_eq!(attr(&c), Attribution::AttrCode);
    }

    #[test]
    fn a_missing_cargo_subcommand_is_an_env_error() {
        let c = classify(TaskType::Clippy, 101, false, &[], "error: no such subcommand: `clippy`");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert_eq!(rule(&c), "env_markers_raw");
    }

    #[test]
    fn timeouts_win_over_everything() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Build, 137, true, &d, "");
        assert_eq!(c.kind, ResultKind::Timeout);
        assert_eq!(st(&c), Status::Timeout);
        assert_eq!(rule(&c), "timeout");
    }

    #[test]
    fn warnings_alone_still_succeed() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Check, 0, false, &d, "");
        assert_eq!(c.kind, ResultKind::Success);
        assert_eq!(c.warning_count, 1);
        assert_eq!(c.summary, "success, 1 warnings");
        assert_eq!(rule(&c), "success");
    }

    #[test]
    fn failing_tests_are_a_code_problem() {
        let c = classify(
            TaskType::Test,
            1,
            false,
            &[],
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
        );
        assert_eq!(c.kind, ResultKind::CompileError);
        assert_eq!(attr(&c), Attribution::AttrCode);
        assert_eq!(rule(&c), "test_failed");
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

    // ---- §1.7 fixture matrix ------------------------------------------------

    #[test]
    fn fixture_oom_killed_true_is_resource() {
        let exec = ExecEvidence {
            oom_killed: true,
            ..Default::default()
        };
        let c = classify_with_exec(TaskType::Test, 137, false, &exec, &[], "killed", false);
        assert_eq!(attr(&c), Attribution::AttrResource);
        assert_eq!(rule(&c), "oom_killed");
        assert_eq!(c.kind, ResultKind::EnvError);
        assert!(!c.kind.is_cacheable());
    }

    #[test]
    fn fixture_sigkill_log_without_oomkilled_is_suspected_oom() {
        let log = "   Compiling private_tun v0.1.0\n\
                   error: could not compile `private_tun` (lib) due to previous error\n\
                   Caused by:\n  process didn't exit successfully: `rustc ...` (signal: 9, SIGKILL: kill)";
        let c = classify(TaskType::Test, 101, false, &[], log);
        assert_eq!(attr(&c), Attribution::AttrResource);
        assert_eq!(rule(&c), "sigkill_suspected_oom");
        assert_eq!(c.kind, ResultKind::EnvError);
        let ev = c.verdict.evidence.as_ref().expect("evidence");
        assert_eq!(ev.source, "log_line");
        assert!(ev.excerpt.contains("SIGKILL"));
        assert!(ev.line_no > 0);
    }

    #[test]
    fn fixture_worker_timeout_137_is_not_oom() {
        // timed_out=true is the only signal that classifies timeout; exit 137
        // alone (or OOMKilled) must not produce timeout.
        let d = parse_cargo_json(CARGO_ERR);
        let exec = ExecEvidence {
            worker_killed: true,
            ..Default::default()
        };
        let c = classify_with_exec(TaskType::Build, 137, true, &exec, &d, "", false);
        assert_eq!(c.kind, ResultKind::Timeout);
        assert_eq!(rule(&c), "timeout");
        assert_ne!(attr(&c), Attribution::AttrResource);

        // Same 137 without timed_out and without OOM → not timeout, not resource
        // unless the log says SIGKILL.
        let c2 = classify_with_exec(TaskType::Build, 137, false, &ExecEvidence::default(), &[], "exit 137", false);
        assert_ne!(c2.kind, ResultKind::Timeout);
        assert_ne!(rule(&c2), "oom_killed");
        assert_ne!(rule(&c2), "sigkill_suspected_oom");
    }

    #[test]
    fn fixture_rustc_sigsegv_is_infra() {
        let log = "error: could not compile `foo` (lib)\n\
                   Caused by:\n  process didn't exit successfully: `rustc ...` (signal: 11, SIGSEGV: invalid memory reference)";
        let c = classify(TaskType::Check, 101, false, &[], log);
        assert_eq!(attr(&c), Attribution::AttrInfra);
        assert_eq!(rule(&c), "rustc_crash");
        assert_eq!(c.kind, ResultKind::InfraError);
        assert!(c.kind.is_retryable());
    }

    #[test]
    fn fixture_test_sigsegv_without_compile_context_is_unknown() {
        // Rule 7 requires compile context for both SIGSEGV and SIGABRT (R8).
        let log = "running 1 test\nerror: test failed, to rerun pass `--lib`\n\
                   Caused by:\n  process didn't exit successfully (signal: 11, SIGSEGV: invalid memory reference)";
        let c = classify(TaskType::Test, 101, false, &[], log);
        assert_eq!(rule(&c), "unknown");
        assert_eq!(attr(&c), Attribution::AttrUnknown);
        assert_ne!(attr(&c), Attribution::AttrCode);
        assert_ne!(attr(&c), Attribution::AttrInfra);
    }

    #[test]
    fn fixture_test_abort_without_summary_is_unknown() {
        // No structured diagnostics, no libtest summary → UNKNOWN, not CODE.
        let log = "running 1 test\nerror: test failed, to rerun pass `--lib`\n\
                   Caused by:\n  process didn't exit successfully (signal: 6, SIGABRT)";
        let c = classify(TaskType::Test, 101, false, &[], log);
        assert_eq!(rule(&c), "unknown");
        assert_eq!(attr(&c), Attribution::AttrUnknown);
    }

    #[test]
    fn green_test_carries_summary_counts() {
        let log = "test result: ok. 47 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out";
        let c = classify(TaskType::Test, 0, false, &[], log);
        assert_eq!(c.kind, ResultKind::Success);
        assert!(c.summary.contains("47 passed"), "{}", c.summary);
        assert!(c.summary.contains("2 ignored"), "{}", c.summary);
        let ts = c.test_summary.expect("summary");
        assert_eq!(ts.passed, 47);
        assert_eq!(ts.ignored, 2);
    }

    #[test]
    fn command_override_does_not_parse_libtest() {
        let log = "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out";
        let c = classify_with_exec(
            TaskType::Test,
            1,
            false,
            &ExecEvidence::default(),
            &[],
            log,
            false, // override
        );
        assert_ne!(rule(&c), "test_failed");
        assert_ne!(attr(&c), Attribution::AttrCode);
    }

    #[test]
    fn build_with_libtest_noise_is_not_code() {
        let log = "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n\
                   (this is a build log that happens to contain a test line)";
        let c = classify(TaskType::Build, 1, false, &[], log);
        assert_ne!(rule(&c), "test_failed");
        assert_ne!(attr(&c), Attribution::AttrCode);
    }

    #[test]
    fn env_marker_only_in_old_log_prefix_does_not_trigger_rule9() {
        // Rule 9 scans only the last 200 lines.
        let mut old = String::new();
        for i in 0..250 {
            old.push_str(&format!("line {i}: linker `cc` not found\n"));
        }
        // Recent lines are unrelated failure.
        old.push_str("final mystery failure with no markers\n");
        // Tail is last 200 lines — still has markers. Need markers only in the head.
        let mut log = String::new();
        log.push_str("early: linker `cc` not found\n");
        for i in 0..250 {
            log.push_str(&format!("noise {i}\n"));
        }
        log.push_str("final mystery failure\n");
        let c = classify(TaskType::Check, 1, false, &[], &log);
        assert_ne!(rule(&c), "env_markers_raw", "old markers must not classify: {}", c.verdict.rule);
    }

    #[test]
    fn fixture_disk_full_is_infra_not_resource() {
        let log = "error: failed to write to /rc/target/...\nNo space left on device";
        let c = classify(TaskType::Build, 101, false, &[], log);
        assert_eq!(attr(&c), Attribution::AttrInfra);
        assert_eq!(rule(&c), "disk_full");
        assert_eq!(c.kind, ResultKind::InfraError);
    }

    #[test]
    fn fixture_pure_compile_error() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(TaskType::Check, 101, false, &d, "error: could not compile `foo`");
        assert_eq!(attr(&c), Attribution::AttrCode);
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.kind.is_cacheable());
    }

    #[test]
    fn fixture_compile_error_plus_env_marker_stays_code() {
        let d = parse_cargo_json(CARGO_ERR);
        let c = classify(
            TaskType::Check,
            101,
            false,
            &d,
            "cannot find -lssl\nerror: linker `cc` not found\nerror: could not compile",
        );
        assert_eq!(attr(&c), Attribution::AttrCode);
        assert_eq!(c.kind, ResultKind::CompileError);
        assert!(c.env_hints.is_empty());
    }

    #[test]
    fn fixture_test_failed_summary_only() {
        let log = "     Running unittests src/lib.rs (target/debug/deps/mycrate-abc123)\n\
                   \n\
                   failures:\n\
                   \n\
                   ---- tests::boom stdout ----\n\
                   thread 'tests::boom' panicked at src/lib.rs:10:5:\n\
                   assertion failed\n\
                   \n\
                   failures:\n\
                       tests::boom\n\
                   \n\
                   test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let c = classify(TaskType::Test, 101, false, &[], log);
        assert_eq!(rule(&c), "test_failed");
        assert_eq!(attr(&c), Attribution::AttrCode);
        let ts = c.test_summary.as_ref().expect("summary");
        assert_eq!(ts.failed, 1);
        assert_eq!(ts.passed, 2);
        assert!(ts.failed_names.iter().any(|n| n.contains("tests::boom")), "{:?}", ts.failed_names);
    }

    #[test]
    fn fixture_all_env_diagnostics() {
        let d = parse_generic("wrapper.c:3:10: fatal error: openssl/ssl.h: No such file or directory");
        let c = classify(TaskType::Build, 2, false, &d, "make: *** [all] Error 1");
        assert_eq!(rule(&c), "env_diagnostics");
        assert_eq!(attr(&c), Attribution::AttrProjectConfig);
        assert!(!c.env_hints.is_empty());
    }

    #[test]
    fn kind_mapping_table_is_complete() {
        assert_eq!(kind_from_verdict(Status::Success, Attribution::AttrUnknown), ResultKind::Success);
        assert_eq!(kind_from_verdict(Status::Failed, Attribution::AttrCode), ResultKind::CompileError);
        assert_eq!(kind_from_verdict(Status::Failed, Attribution::AttrProjectConfig), ResultKind::EnvError);
        assert_eq!(kind_from_verdict(Status::Failed, Attribution::AttrResource), ResultKind::EnvError);
        assert_eq!(kind_from_verdict(Status::Failed, Attribution::AttrInfra), ResultKind::InfraError);
        assert_eq!(kind_from_verdict(Status::Failed, Attribution::AttrUnknown), ResultKind::EnvError);
        assert_eq!(kind_from_verdict(Status::Timeout, Attribution::AttrInfra), ResultKind::Timeout);
    }

    #[test]
    fn parse_test_summary_accumulates_binaries() {
        let log = "test result: ok. 10 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n\
                   test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let s = parse_test_summary(log);
        assert!(s.summary_seen);
        assert_eq!(s.binaries, 2);
        assert_eq!(s.passed, 13);
        assert_eq!(s.failed, 2);
        assert_eq!(s.ignored, 1);
    }

    #[test]
    fn parse_test_summary_unseen_when_no_lines() {
        let s = parse_test_summary("error: could not compile `foo`");
        assert!(!s.summary_seen);
        assert_eq!(s.failed, 0);
    }
}
