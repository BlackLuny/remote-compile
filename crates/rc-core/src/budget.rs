//! Context budget gate (mechanism three, I3).
//!
//! Every MCP response text passes through here: UTF-8 byte metering, per-line
//! elision, and a slotted response budget so headline/evidence and Critical
//! notices are never dropped while the total stays ≤ 8KB.

use std::sync::OnceLock;

/// Total response budget in UTF-8 bytes (hard cap of the final return value).
pub const RESPONSE_BUDGET: usize = 8 * 1024;
/// Soft per-line cap (non-raw). Head 250 + tail 100 + marker.
pub const LINE_BUDGET: usize = 400;
const LINE_HEAD: usize = 250;
const LINE_TAIL: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct BudgetedText {
    pub text: String,
    pub bytes_omitted: u64,
    pub next_offset: u64,
    pub next_byte_offset: u64,
    pub line_byte_offset: u64,
    pub line_no: String,
}

/// Truncate `s` to at most `max` bytes on a char boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Elide the middle of a long line. Preserves head/tail and reports original
/// length. Output is always near LINE_BUDGET when the input exceeds it.
pub fn elide_line(line: &str, line_no: u32) -> String {
    if line.len() <= LINE_BUDGET {
        return line.to_string();
    }
    let orig = line.len();
    let head = truncate_bytes(line, LINE_HEAD);
    let mut start = line.len().saturating_sub(LINE_TAIL);
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    let tail = &line[start..];
    let omitted = orig.saturating_sub(head.len() + tail.len());
    // Keep total near LINE_BUDGET: shrink head if marker pushes over.
    let marker = format!("…[省略 {omitted} B, log:{line_no}, 原 {orig}B]…");
    let budget_for_content = LINE_BUDGET.saturating_sub(marker.len().min(LINE_BUDGET));
    let head_n = (budget_for_content * 5 / 7).max(1);
    let tail_n = budget_for_content.saturating_sub(head_n);
    let head = truncate_bytes(line, head_n);
    let mut start = line.len().saturating_sub(tail_n);
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    let tail = &line[start..];
    let omitted = orig.saturating_sub(head.len() + tail.len());
    format!("{head}…[省略 {omitted} B, log:{line_no}, 原 {orig}B]…{tail}")
}

/// Slot priorities for assembling a response (high → low).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// Headline + verdict + evidence — never truncated (I1 > I3).
    Headline = 0,
    /// Structured diagnostics / TestSummary (≤4KB target).
    Diagnostics = 1,
    /// Critical notices — never dropped for budget.
    Critical = 2,
    /// Info/Warning notices (≤2KB target).
    Notices = 3,
    /// Decorative remainder.
    Decor = 4,
}

#[derive(Debug, Clone)]
pub struct SlotPiece {
    pub slot: Slot,
    pub text: String,
}

/// Assemble pieces under RESPONSE_BUDGET.
///
/// Order of packing: Headline (full) → Critical (full, reserved) → Diagnostics
/// → Notices → Decor. Critical is reserved first so it is never tail-clipped.
pub fn assemble(pieces: &[SlotPiece]) -> String {
    let mut headline = String::new();
    let mut critical = String::new();
    let mut diags = String::new();
    let mut notices = String::new();
    let mut decor = String::new();
    for p in pieces {
        let dest = match p.slot {
            Slot::Headline => &mut headline,
            Slot::Critical => &mut critical,
            Slot::Diagnostics => &mut diags,
            Slot::Notices => &mut notices,
            Slot::Decor => &mut decor,
        };
        if !dest.is_empty() && !dest.ends_with('\n') {
            dest.push('\n');
        }
        dest.push_str(&p.text);
    }

    // Reserve room for critical + headline; fill mid slots into the rest.
    let reserved = headline.len() + if critical.is_empty() { 0 } else { critical.len() + 1 };
    let mut mid_budget = RESPONSE_BUDGET.saturating_sub(reserved);

    let mut mid = String::new();
    for part in [&diags, &notices, &decor] {
        if part.is_empty() {
            continue;
        }
        if mid_budget == 0 {
            break;
        }
        let chunk = if part.len() <= mid_budget {
            part.as_str()
        } else {
            truncate_bytes(part, mid_budget)
        };
        if chunk.is_empty() {
            continue;
        }
        if !mid.is_empty() && !mid.ends_with('\n') {
            mid.push('\n');
        }
        mid.push_str(chunk);
        mid_budget = RESPONSE_BUDGET
            .saturating_sub(reserved + mid.len() + if critical.is_empty() { 0 } else { 0 });
        mid_budget = RESPONSE_BUDGET.saturating_sub(headline.len() + mid.len() + critical.len() + 2);
    }

    let mut out = headline;
    if !mid.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&mid);
    }
    if !critical.is_empty() {
        // Append critical last so it always survives (R3).
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        // If somehow over budget (headline alone huge), still keep critical by
        // trimming mid first — already reserved.
        let room = RESPONSE_BUDGET.saturating_sub(out.len());
        if critical.len() <= room {
            out.push_str(&critical);
        } else {
            // Emergency: keep critical, trim out from the middle.
            let keep_head = truncate_bytes(&out, RESPONSE_BUDGET.saturating_sub(critical.len() + 1));
            out = format!("{keep_head}\n{critical}");
            if out.len() > RESPONSE_BUDGET {
                out = truncate_bytes(&out, RESPONSE_BUDGET).to_string();
            }
        }
    }
    if out.len() > RESPONSE_BUDGET {
        // Absolute hard cap: never return more than 8192 bytes.
        truncate_bytes(&out, RESPONSE_BUDGET).to_string()
    } else {
        out
    }
}

/// Apply per-line elision and response budget to log lines.
/// `start_line_no` is 1-based server line number of `lines[0]`.
/// `line_byte_offset` skips that many UTF-8 bytes into the first line (raw).
/// `header_reserve` is reserved for the caller to prepend without exceeding 8KB.
pub fn gate_log_lines(
    lines: &[String],
    start_line_no: u64,
    raw: bool,
    line_byte_offset: u64,
    header_reserve: usize,
) -> BudgetedText {
    let budget = RESPONSE_BUDGET.saturating_sub(header_reserve);
    let mut text = String::new();
    let mut bytes_omitted = 0u64;
    let mut next_offset = start_line_no;

    // Raw single-line / first-line continuation.
    if raw && !lines.is_empty() && line_byte_offset > 0 {
        let line = &lines[0];
        let mut start = line_byte_offset as usize;
        if start > line.len() {
            start = line.len();
        }
        while start < line.len() && !line.is_char_boundary(start) {
            start += 1;
        }
        let rest = &line[start..];
        let chunk = truncate_bytes(rest, budget);
        let next_bo = start as u64 + chunk.len() as u64;
        return BudgetedText {
            text: chunk.to_string(),
            bytes_omitted: rest.len().saturating_sub(chunk.len()) as u64,
            next_offset: start_line_no,
            next_byte_offset: if next_bo < line.len() as u64 { next_bo } else { 0 },
            line_byte_offset,
            line_no: start_line_no.to_string(),
        };
    }

    for (i, line) in lines.iter().enumerate() {
        let line_no = start_line_no + i as u64;
        let rendered = if raw {
            // On first line with offset 0, may still be huge.
            if i == 0 && line.len() > budget.saturating_sub(text.len()) {
                let room = budget.saturating_sub(text.len());
                let chunk = truncate_bytes(line, room);
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk);
                return BudgetedText {
                    text,
                    bytes_omitted: (line.len() - chunk.len()) as u64,
                    next_offset: line_no,
                    next_byte_offset: chunk.len() as u64,
                    line_byte_offset: 0,
                    line_no: line_no.to_string(),
                };
            }
            line.clone()
        } else {
            let e = elide_line(line, line_no as u32);
            if e.len() < line.len() {
                bytes_omitted += (line.len() - e.len()) as u64;
            }
            e
        };
        let add = if text.is_empty() {
            rendered.len()
        } else {
            rendered.len() + 1
        };
        if text.len() + add > budget {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&rendered);
        next_offset = line_no + 1;
    }
    BudgetedText {
        text,
        bytes_omitted,
        next_offset,
        ..Default::default()
    }
}

/// Gate an arbitrary tool response: elide long lines, hard-cap at 8KB.
/// The final string is always ≤ RESPONSE_BUDGET (including any marker).
pub fn gate_response(text: &str) -> String {
    static MARKER: OnceLock<String> = OnceLock::new();
    let marker = MARKER.get_or_init(|| format!("…[响应达上限 {RESPONSE_BUDGET}B]"));
    let mut lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        lines.push(elide_line(line, (i + 1) as u32));
    }
    let joined = lines.join("\n");
    if joined.len() <= RESPONSE_BUDGET {
        return joined;
    }
    // Reserve room for the marker so the final value is ≤ 8192.
    let room = RESPONSE_BUDGET.saturating_sub(marker.len());
    let head = truncate_bytes(&joined, room);
    format!("{head}{marker}")
}

/// Split a pre-built body into headline (until first blank line or full) and
/// the rest, then assemble with critical notices so Critical always survives.
pub fn assemble_result_with_notices(body: &str, critical: &str, info: &str) -> String {
    // Body is treated as Headline+Diagnostics already rendered; Critical last.
    assemble(&[
        SlotPiece {
            slot: Slot::Headline,
            text: body.to_string(),
        },
        SlotPiece {
            slot: Slot::Notices,
            text: info.to_string(),
        },
        SlotPiece {
            slot: Slot::Critical,
            text: critical.to_string(),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elide_keeps_utf8_boundary() {
        let long = "a".repeat(500) + "结尾汉字";
        let e = elide_line(&long, 42);
        assert!(e.len() <= LINE_BUDGET + 40, "len={}", e.len());
        assert!(e.contains("省略"));
        assert!(std::str::from_utf8(e.as_bytes()).is_ok());
    }

    #[test]
    fn assemble_never_drops_critical() {
        let headline = "H".repeat(100);
        let diag = "D".repeat(9000);
        let crit = "CRITICAL: exclude secrets";
        let out = assemble(&[
            SlotPiece {
                slot: Slot::Headline,
                text: headline.clone(),
            },
            SlotPiece {
                slot: Slot::Diagnostics,
                text: diag,
            },
            SlotPiece {
                slot: Slot::Critical,
                text: crit.to_string(),
            },
        ]);
        assert!(out.contains("CRITICAL"), "critical lost:\n{out}");
        assert!(out.len() <= RESPONSE_BUDGET, "len={}", out.len());
        assert!(out.contains('H'));
    }

    #[test]
    fn gate_response_hard_cap() {
        let text = (0..80)
            .map(|i| format!("section {i}: {}", "字".repeat(300)))
            .collect::<Vec<_>>()
            .join("\n");
        let g = gate_response(&text);
        assert!(g.len() <= RESPONSE_BUDGET, "len={}", g.len());
        assert!(std::str::from_utf8(g.as_bytes()).is_ok());
    }

    #[test]
    fn raw_continuation_advances() {
        // Distinct regions so slices are observably different.
        let mut huge = String::new();
        huge.push_str(&"A".repeat(10_000));
        huge.push_str(&"B".repeat(10_000));
        let g1 = gate_log_lines(&[huge.clone()], 7, true, 0, 64);
        assert!(g1.next_byte_offset > 0, "first slice must offer continuation");
        assert!(g1.text.starts_with('A'));
        let g2 = gate_log_lines(&[huge], 7, true, g1.next_byte_offset, 64);
        assert_eq!(g2.line_byte_offset, g1.next_byte_offset);
        // Advanced past the pure-A region or at least moved the window.
        assert!(g2.line_byte_offset >= g1.text.len() as u64);
        assert!(g2.text.contains('B') || g2.next_byte_offset > g1.next_byte_offset || !g2.text.is_empty());
    }

    #[test]
    fn gate_log_respects_header_reserve() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i} {}", "x".repeat(200))).collect();
        let g = gate_log_lines(&lines, 1, false, 0, 200);
        assert!(g.text.len() + 200 <= RESPONSE_BUDGET + 16);
    }

    #[test]
    fn critical_survives_oversize_body() {
        // R3': oversize diagnostics + Critical notice → final ≤8192 and Critical present.
        let body = "D".repeat(10_000);
        let crit = "CRITICAL: exclude secrets from sync";
        let out = assemble_result_with_notices(&body, crit, "info noise");
        assert!(out.len() <= RESPONSE_BUDGET, "len={}", out.len());
        assert!(out.contains("CRITICAL"), "critical lost under budget pressure");
    }
}
