//! Unit progress parsing (mechanism five).
//!
//! Matches cargo's `   Compiling foo v1.0.0` / `    Checking bar v0.1.0`
//! lines. These are *start* events: `units_seen` counts observations, not
//! completions. `Fresh` lines are intentionally ignored (unstable without
//! verbose).

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitProgress {
    pub units_seen: u32,
    pub current_unit: String,
}

fn unit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s+(Compiling|Checking) (\S+) v").expect("static"))
}

/// Feed one log line; returns true if progress advanced.
pub fn observe_line(state: &mut UnitProgress, line: &str) -> bool {
    if let Some(c) = unit_re().captures(line) {
        let name = c.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            return false;
        }
        state.units_seen = state.units_seen.saturating_add(1);
        state.current_unit = name;
        return true;
    }
    false
}

/// Render a reference (not ETA) progress line for the agent.
pub fn render_progress(
    current_unit: &str,
    units_seen: u32,
    elapsed_secs: u64,
    history_units: Option<u32>,
    history_build_ms_p50: Option<u64>,
) -> String {
    let mut s = format!("⏳ building");
    if !current_unit.is_empty() {
        s.push_str(": ");
        s.push_str(current_unit);
    }
    s.push_str(&format!("（本次已见 {units_seen} 个单元"));
    if let (Some(u), Some(ms)) = (history_units, history_build_ms_p50) {
        s.push_str(&format!(
            "；上次同类任务共 {u} 个、耗时 p50 {}s，参考",
            ms / 1000
        ));
    }
    s.push_str(&format!("）已运行 {elapsed_secs}s"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_compiling_and_checking() {
        let mut s = UnitProgress::default();
        assert!(observe_line(&mut s, "   Compiling foo v0.1.0"));
        assert!(observe_line(&mut s, "    Checking bar v1.2.3"));
        assert!(!observe_line(&mut s, "     Fresh baz v0.1.0"));
        assert!(!observe_line(&mut s, "error: boom"));
        assert_eq!(s.units_seen, 2);
        assert_eq!(s.current_unit, "bar");
    }

    #[test]
    fn render_is_reference_not_eta() {
        let t = render_progress("rrd-core", 23, 96, Some(107), Some(180_000));
        assert!(t.contains("参考"));
        assert!(t.contains("23"));
        assert!(t.contains("rrd-core"));
        assert!(!t.contains("剩余"));
    }
}
