//! Diagnostic baseline / delta (mechanism four).
//!
//! Read-time set difference over two result_json diagnostic lists. Keys use a
//! strict identity that strips only span fragments from the message.

use crate::pb::{DiagDelta, Diagnostic};
use std::sync::OnceLock;

fn re_file_span() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\S+\.rs:\d+:\d+").expect("static"))
}

/// Bare `:line:col` only at end of token / message (R10) — not mid-prose sizes.
fn re_bare_span() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r":\d+:\d+\s*$").expect("static"))
}

/// Strip only identifiable span fragments: `file.rs:12:3` and trailing
/// bare `:line:col`. Does **not** strip arbitrary numbers (F25.1).
pub fn normalize_spans(message: &str) -> String {
    let s = re_file_span().replace_all(message, "");
    let s = re_bare_span().replace_all(&s, "");
    // Collapse whitespace.
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn strict_key(d: &Diagnostic) -> String {
    let norm = normalize_spans(&d.message);
    let payload = format!(
        "{}\0{}\0{}\0{}",
        d.level, d.code, d.file, norm
    );
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

/// Fuzzy key ignores file_path (rename detection only; does not count as fixed).
fn fuzzy_key(d: &Diagnostic) -> String {
    let norm = normalize_spans(&d.message);
    let payload = format!("{}\0{}\0{}", d.level, d.code, norm);
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

#[derive(Debug, Clone)]
pub struct DeltaInput<'a> {
    pub current: &'a [Diagnostic],
    pub baseline: &'a [Diagnostic],
    pub baseline_task_id: &'a str,
    pub current_truncated: u32,
    pub baseline_truncated: u32,
}

pub fn compute_delta(input: DeltaInput<'_>) -> DiagDelta {
    use std::collections::HashMap;

    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for d in input.baseline {
        *base_counts.entry(strict_key(d)).or_default() += 1;
    }
    let mut cur_counts: HashMap<String, usize> = HashMap::new();
    for d in input.current {
        *cur_counts.entry(strict_key(d)).or_default() += 1;
    }

    let mut new_diags = Vec::new();
    let mut preexisting = 0u32;
    let mut fixed = 0u32;

    // Match current against baseline.
    let mut base_remaining = base_counts.clone();
    for d in input.current {
        let k = strict_key(d);
        let left = base_remaining.get(&k).copied().unwrap_or(0);
        if left > 0 {
            preexisting += 1;
            base_remaining.insert(k, left - 1);
        } else {
            new_diags.push(d.clone());
        }
    }
    for (_k, n) in base_remaining {
        fixed += n as u32;
    }

    // Approximate: truncated either side, empty codes (any level), or renames.
    let empty_code = input
        .current
        .iter()
        .chain(input.baseline.iter())
        .any(|d| d.code.is_empty() && (d.level == "error" || d.level == "warning"));
    let mut rename_hint = false;
    if !new_diags.is_empty() && fixed > 0 {
        // Fuzzy match leftover new vs fixed → suspected rename.
        let base_fuzzy: HashMap<String, usize> = {
            let mut m = HashMap::new();
            for d in input.baseline {
                *m.entry(fuzzy_key(d)).or_default() += 1;
            }
            m
        };
        for d in &new_diags {
            if base_fuzzy.contains_key(&fuzzy_key(d)) {
                rename_hint = true;
                break;
            }
        }
    }
    let approximate = input.current_truncated > 0
        || input.baseline_truncated > 0
        || empty_code
        || rename_hint;

    // Truncated: do not report fixed_count (F6.3).
    let fixed_count = if approximate && (input.current_truncated > 0 || input.baseline_truncated > 0)
    {
        0
    } else if approximate && rename_hint {
        // Still report fixed when only rename heuristic fired? Spec: truncated
        // suppresses fixed. Rename alone sets approximate but may still report.
        fixed
    } else {
        fixed
    };
    let fixed_count = if input.current_truncated > 0 || input.baseline_truncated > 0 {
        0
    } else {
        fixed_count
    };

    // Preexisting summary: one line per crate (directory prefix of file).
    let mut by_crate: HashMap<String, u32> = HashMap::new();
    let mut base_remaining2 = base_counts;
    for d in input.current {
        let k = strict_key(d);
        let left = base_remaining2.get(&k).copied().unwrap_or(0);
        if left > 0 {
            base_remaining2.insert(k, left - 1);
            let crate_name = d
                .file
                .split('/')
                .find(|s| *s != "src" && !s.is_empty())
                .unwrap_or("(unknown)")
                .to_string();
            *by_crate.entry(crate_name).or_default() += 1;
        }
    }
    let mut preexisting_summary: Vec<String> = by_crate
        .into_iter()
        .map(|(k, n)| format!("{k}: {n}"))
        .collect();
    preexisting_summary.sort();

    DiagDelta {
        new_count: new_diags.len() as u32,
        fixed_count,
        preexisting_count: preexisting,
        new_diagnostics: new_diags,
        preexisting_summary,
        baseline_task_id: input.baseline_task_id.to_string(),
        approximate,
    }
}

/// Render a short Chinese delta block for agent output.
pub fn render_delta(delta: &DiagDelta, max_new: usize) -> String {
    if delta.baseline_task_id.is_empty() && delta.new_count == 0 && delta.fixed_count == 0 {
        return String::new();
    }
    let mut s = String::new();
    let approx = if delta.approximate { "（近似）" } else { "" };
    s.push_str(&format!(
        "诊断增量{approx}：新增 {}，基线已有 {}{}",
        delta.new_count,
        delta.preexisting_count,
        if delta.fixed_count > 0 {
            format!("，已修复 {}", delta.fixed_count)
        } else {
            String::new()
        }
    ));
    if !delta.new_diagnostics.is_empty() {
        s.push_str("\n新增:\n");
        for d in delta.new_diagnostics.iter().take(max_new) {
            s.push_str(&format!(
                "  {}{}{} {}\n",
                if d.file.is_empty() {
                    String::new()
                } else {
                    format!("{}:{}:{} ", d.file, d.line, d.column)
                },
                if d.code.is_empty() {
                    String::new()
                } else {
                    format!("{} ", d.code)
                },
                d.level,
                d.message
            ));
        }
    }
    if !delta.preexisting_summary.is_empty() {
        s.push_str(&format!(
            "基线已有 {} 条（{}）——详情 get_log。\n",
            delta.preexisting_count,
            delta.preexisting_summary.join(", ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(level: &str, code: &str, file: &str, line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            level: level.into(),
            code: code.into(),
            file: file.into(),
            line,
            message: msg.into(),
            ..Default::default()
        }
    }

    #[test]
    fn normalize_strips_spans_not_array_sizes() {
        assert_eq!(
            normalize_spans("mismatched types at src/lib.rs:10:5 expected [u8; 32]"),
            "mismatched types at expected [u8; 32]"
        );
        // Digits in message stay.
        assert!(normalize_spans("expected 2 arguments").contains("2"));
    }

    #[test]
    fn same_diag_moved_line_is_preexisting() {
        let base = vec![d("error", "E0308", "src/a.rs", 10, "mismatched types")];
        let cur = vec![d("error", "E0308", "src/a.rs", 20, "mismatched types")];
        let delta = compute_delta(DeltaInput {
            current: &cur,
            baseline: &base,
            baseline_task_id: "t0",
            current_truncated: 0,
            baseline_truncated: 0,
        });
        assert_eq!(delta.new_count, 0);
        assert_eq!(delta.preexisting_count, 1);
        assert_eq!(delta.fixed_count, 0);
    }

    #[test]
    fn new_and_fixed() {
        let base = vec![
            d("error", "E0308", "src/a.rs", 1, "mismatched types"),
            d("error", "E0425", "src/b.rs", 1, "cannot find value"),
        ];
        let cur = vec![
            d("error", "E0308", "src/a.rs", 1, "mismatched types"),
            d("error", "E0001", "src/c.rs", 1, "new boom"),
        ];
        let delta = compute_delta(DeltaInput {
            current: &cur,
            baseline: &base,
            baseline_task_id: "t0",
            current_truncated: 0,
            baseline_truncated: 0,
        });
        assert_eq!(delta.new_count, 1);
        assert_eq!(delta.fixed_count, 1);
        assert_eq!(delta.preexisting_count, 1);
    }

    #[test]
    fn truncated_suppresses_fixed() {
        let base = vec![d("error", "E0308", "src/a.rs", 1, "mismatched types")];
        let cur = vec![d("error", "E0001", "src/c.rs", 1, "new")];
        let delta = compute_delta(DeltaInput {
            current: &cur,
            baseline: &base,
            baseline_task_id: "t0",
            current_truncated: 5,
            baseline_truncated: 0,
        });
        assert!(delta.approximate);
        assert_eq!(delta.fixed_count, 0);
    }

    #[test]
    fn empty_code_marks_approximate() {
        let base = vec![d("error", "", "src/a.rs", 1, "something")];
        let cur = vec![d("error", "", "src/a.rs", 1, "something")];
        let delta = compute_delta(DeltaInput {
            current: &cur,
            baseline: &base,
            baseline_task_id: "t0",
            current_truncated: 0,
            baseline_truncated: 0,
        });
        assert!(delta.approximate);
    }
}
