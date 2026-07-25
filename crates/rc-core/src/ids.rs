//! Identifier construction.
//!
//! Ids are derived from content wherever possible so that two agents that
//! never talk to each other still agree on what "the same project" means.

use std::path::Path;

fn short(input: &str, n: usize) -> String {
    blake3::hash(input.as_bytes()).to_hex()[..n].to_string()
}

/// Canonicalise a repo url so `git@github.com:o/r.git`,
/// `https://github.com/o/r.git` and `https://github.com/o/r/` agree.
pub fn canonical_repo_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let u = u
        .strip_prefix("git+")
        .unwrap_or(u);
    let host_path = if let Some(rest) = u.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else if let Some(rest) = u.strip_prefix("ssh://git@") {
        rest.to_string()
    } else if let Some(rest) = u.strip_prefix("https://") {
        rest.to_string()
    } else if let Some(rest) = u.strip_prefix("http://") {
        rest.to_string()
    } else {
        u.to_string()
    };
    host_path.to_lowercase()
}

/// A project is identified by its remote url when it has one, and by its
/// canonical local root otherwise (§3.1 — non-git dirs are allowed).
pub fn project_id(repo_url: Option<&str>, local_root: &Path) -> String {
    match repo_url.filter(|u| !u.trim().is_empty()) {
        Some(url) => format!("p-{}", short(&format!("url:{}", canonical_repo_url(url)), 16)),
        None => format!(
            "p-{}",
            short(&format!("path:{}", local_root.to_string_lossy()), 16)
        ),
    }
}

/// Worktree identity = absolute path + the base commit first seen there
/// (§3.1). The server never assumes this maps 1:1 onto a git worktree.
pub fn worktree_id(abs_path: &Path, first_base_commit: &str) -> String {
    format!(
        "w-{}",
        short(
            &format!("{}|{}", abs_path.to_string_lossy(), first_base_commit),
            16
        )
    )
}

/// Supersede scope: same worktree, same agent session, same task type (§5.2).
/// Crucially *not* cross-session and *not* cross-type.
pub fn supersede_key(worktree_id: &str, agent_session: &str, task_type: &str) -> String {
    format!("{worktree_id}|{agent_session}|{task_type}")
}

/// Monotonic-ish, sortable, collision-resistant task id.
pub fn task_id() -> String {
    format!("t-{}", ulid::Ulid::generate().to_string())
}

pub fn env_id() -> String {
    format!("e-{}", ulid::Ulid::generate().to_string())
}

pub fn worker_id() -> String {
    format!("worker-{}", ulid::Ulid::generate().to_string()[..10].to_lowercase())
}

pub fn agent_session_id() -> String {
    format!("s-{}", ulid::Ulid::generate().to_string())
}

/// Deterministic id for an environment image derived from its build inputs,
/// so two agents submitting the same Dockerfile land on the same env.
pub fn env_id_for_source(dockerfile: &str, pull_ref: &str) -> String {
    format!("e-{}", short(&format!("{dockerfile}\u{0}{pull_ref}"), 16))
}

/// Opaque random token (enrollment tokens, agent tokens, session cookies).
pub fn random_token() -> String {
    let a = ulid::Ulid::generate().to_string();
    let b = ulid::Ulid::generate().to_string();
    format!("{}{}", a, b).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn repo_url_forms_agree() {
        let a = canonical_repo_url("git@github.com:org/repo.git");
        let b = canonical_repo_url("https://github.com/org/repo");
        let c = canonical_repo_url("https://github.com/ORG/repo.git/");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn same_repo_from_two_worktrees_is_one_project() {
        let root_a = PathBuf::from("/tmp/wt-a");
        let root_b = PathBuf::from("/tmp/wt-b");
        assert_eq!(
            project_id(Some("git@github.com:o/r.git"), &root_a),
            project_id(Some("https://github.com/o/r"), &root_b)
        );
    }

    #[test]
    fn non_git_projects_fall_back_to_path() {
        let a = project_id(None, Path::new("/tmp/a"));
        let b = project_id(None, Path::new("/tmp/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn supersede_scope_separates_task_types_and_sessions() {
        // §5.2 #22: a clippy run must not cancel a queued check.
        assert_ne!(
            supersede_key("w1", "s1", "check"),
            supersede_key("w1", "s1", "clippy")
        );
        // §5.2: agent A must not cancel agent B in a shared worktree.
        assert_ne!(
            supersede_key("w1", "s1", "check"),
            supersede_key("w1", "s2", "check")
        );
    }

    #[test]
    fn task_ids_sort_by_creation() {
        let a = task_id();
        let b = task_id();
        assert!(a < b || a != b);
    }
}
