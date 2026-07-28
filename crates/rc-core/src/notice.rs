//! Notice state machine (mechanism three, message side).
//!
//! Snapshot semantics: each call producers emit a full notice snapshot; the
//! machine compares against the previous one for (project_id, worktree_id,
//! category) and decides full text / compact / silence.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub category: &'static str,
    pub severity: NoticeSeverity,
    pub text: String,
    pub compact: String,
    /// blake3 of category ‖ structured fields (sorted) — identity for change detection.
    pub identity: [u8; 32],
}

impl Notice {
    pub fn new(
        category: &'static str,
        severity: NoticeSeverity,
        text: impl Into<String>,
        compact: impl Into<String>,
        identity_parts: &[&str],
    ) -> Self {
        let mut parts: Vec<&str> = identity_parts.to_vec();
        parts.sort_unstable();
        let mut h = blake3::Hasher::new();
        h.update(category.as_bytes());
        h.update(&[0]);
        for p in parts {
            h.update(p.as_bytes());
            h.update(&[0]);
        }
        Notice {
            category,
            severity,
            text: text.into(),
            compact: compact.into(),
            identity: *h.finalize().as_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NoticeKey {
    project_id: String,
    worktree_id: String,
    category: String,
}

/// Process-local notice memory. Forgotten on restart (safe bias: may re-speak).
#[derive(Debug, Default)]
pub struct NoticeState {
    last: HashMap<NoticeKey, [u8; 32]>,
}

impl NoticeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compare a snapshot of notices for one (project, worktree) and return
    /// the strings to present this turn.
    pub fn present(
        &mut self,
        project_id: &str,
        worktree_id: &str,
        snapshot: &[Notice],
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen_categories = Vec::new();
        for n in snapshot {
            seen_categories.push(n.category);
            let key = NoticeKey {
                project_id: project_id.to_string(),
                worktree_id: worktree_id.to_string(),
                category: n.category.to_string(),
            };
            let prev = self.last.get(&key).copied();
            let text = match (prev, n.severity) {
                (None, _) => n.text.clone(),
                (Some(id), _) if id != n.identity => n.text.clone(),
                (Some(_), NoticeSeverity::Critical) => n.compact.clone(),
                (Some(_), NoticeSeverity::Info | NoticeSeverity::Warning) => {
                    // Silent on unchanged non-critical.
                    continue;
                }
            };
            self.last.insert(key, n.identity);
            out.push(text);
        }
        // Categories that disappeared are simply absent next time (snapshot
        // semantics); recurrence is treated as first appearance.
        let live: std::collections::HashSet<&str> = seen_categories.into_iter().collect();
        self.last.retain(|k, _| {
            if k.project_id == project_id && k.worktree_id == worktree_id {
                live.contains(k.category.as_str())
            } else {
                true
            }
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(cat: &'static str, sev: NoticeSeverity, text: &str, host: &str) -> Notice {
        Notice::new(cat, sev, text, format!("[{cat}] {host}"), &[host])
    }

    #[test]
    fn first_is_full_second_info_is_silent_critical_is_compact() {
        let mut st = NoticeState::new();
        let info = n("exclude", NoticeSeverity::Info, "full exclude text", "a");
        let crit = n("egress_refused", NoticeSeverity::Critical, "full refused", "b");

        let first = st.present("p", "w", &[info.clone(), crit.clone()]);
        assert_eq!(first.len(), 2);
        assert!(first[0].contains("full exclude"));
        assert!(first[1].contains("full refused"));

        let second = st.present("p", "w", &[info.clone(), crit.clone()]);
        assert_eq!(second.len(), 1, "info silent, critical compact: {second:?}");
        assert!(second[0].contains("[egress_refused]"));
    }

    #[test]
    fn identity_change_reprises_full_text() {
        let mut st = NoticeState::new();
        let a = n("exclude", NoticeSeverity::Warning, "excluded: secrets", "secrets");
        st.present("p", "w", &[a]);
        let b = n("exclude", NoticeSeverity::Warning, "excluded: secrets, keys", "secrets,keys");
        let out = st.present("p", "w", &[b]);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("keys"));
    }

    #[test]
    fn projects_do_not_cross_talk() {
        let mut st = NoticeState::new();
        let n1 = n("exclude", NoticeSeverity::Info, "A", "x");
        st.present("p1", "w", &[n1.clone()]);
        // Same category on another project is first appearance.
        let out = st.present("p2", "w", &[n1]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "A");
    }

    #[test]
    fn disappearance_then_return_is_first() {
        let mut st = NoticeState::new();
        let n1 = n("exclude", NoticeSeverity::Info, "full", "x");
        st.present("p", "w", &[n1.clone()]);
        st.present("p", "w", &[]); // gone
        let out = st.present("p", "w", &[n1]);
        assert_eq!(out, vec!["full".to_string()]);
    }

    #[test]
    fn baseline_off_critical_repeats_compact() {
        let mut st = NoticeState::new();
        let n1 = Notice::new(
            "baseline_off",
            NoticeSeverity::Critical,
            "⚠ baseline disabled because exclude matched tracked files",
            "[baseline-off] exclude 关闭了 git 基线",
            &["exclude"],
        );
        let first = st.present("p", "w", &[n1.clone()]);
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("baseline disabled"));
        let second = st.present("p", "w", &[n1]);
        assert_eq!(second.len(), 1);
        assert!(second[0].contains("[baseline-off]"));
    }

    #[test]
    fn multi_warning_aggregated_identity_is_stable() {
        let mut st = NoticeState::new();
        let n1 = Notice::new(
            "scanner",
            NoticeSeverity::Warning,
            "⚠ a\n⚠ b",
            "[scanner] 2 条警告",
            &["a", "b"],
        );
        st.present("p", "w", &[n1.clone()]);
        let second = st.present("p", "w", &[n1]);
        assert!(second.is_empty(), "unchanged multi-warning should silence: {second:?}");
    }
}
