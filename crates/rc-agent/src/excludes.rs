//! What never leaves the machine — and, in `Includes`, the one thing that must
//! leave it despite git.
//!
//! Two kinds of exclusion meet here. The adapter's are structural — `target/`,
//! `.git/`, `node_modules/` — directories whose contents are build output or
//! version-control internals and would defeat the point of the system. The
//! user's come from `exclude` in `.remote-compile.toml` and are about content:
//! a key, a credential file, a customer dump that git happens to track.
//!
//! The distinction matters because of where they are enforced. Both keep an
//! entry out of the manifest, which is enough for anything travelling through
//! the CAS. It is **not** enough for a tracked file: those ride the L1 git
//! baseline, and the bundle the agent uploads carries the whole tree at that
//! commit — the file's content reaches the control plane inside a pack, having
//! never appeared in a manifest. So a user pattern matching a tracked path
//! turns the baseline off for that root; see `Excludes::hides_tracked_content`.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

#[derive(Debug)]
pub struct Excludes {
    /// Directory names matched at any depth (adapter defaults).
    dirs: Vec<String>,
    /// User patterns, gitignore syntax.
    user: Gitignore,
    user_empty: bool,
    pattern_text: Vec<String>,
}

impl Excludes {
    /// `dirs` are directory names; `patterns` are gitignore-style globs matched
    /// against root-relative paths.
    pub fn new(dirs: &[&str], patterns: &[String]) -> Result<Self, String> {
        // A relative root: patterns are matched against paths, not against the
        // filesystem, so the base only has to be consistent.
        let mut builder = GitignoreBuilder::new("");
        for p in patterns {
            builder
                .add_line(None, p)
                .map_err(|e| format!("exclude pattern `{p}` is not valid: {e}"))?;
        }
        let user = builder
            .build()
            .map_err(|e| format!("could not compile the exclude patterns: {e}"))?;
        Ok(Excludes {
            dirs: dirs.iter().map(|d| d.trim_end_matches('/').to_string()).collect(),
            user,
            user_empty: patterns.is_empty(),
            pattern_text: patterns.to_vec(),
        })
    }

    /// Just the structural exclusions, for callers with no repo config.
    pub fn structural(dirs: &[&str]) -> Self {
        Excludes::new(dirs, &[]).expect("no patterns cannot fail to compile")
    }

    pub fn matches(&self, path: &str) -> bool {
        self.matches_dirs(path) || self.matches_user(path)
    }

    /// Directory names a walk need never descend into, owned so that a
    /// `filter_entry` closure — which must be `'static` — can carry them.
    ///
    /// Only the structural set qualifies. A user pattern may name a single file
    /// inside a directory that is otherwise wanted, so pruning on one would
    /// withhold that directory's other files too.
    pub fn pruned_dir_names(&self) -> Vec<String> {
        rc_core::ALWAYS_EXCLUDE
            .iter()
            .map(|d| (*d).to_string())
            .chain(self.dirs.iter().cloned())
            .collect()
    }

    /// Whether a user pattern excludes this *directory* outright, so a walk can
    /// stop there.
    ///
    /// Safe as a pruning rule precisely because `matches_user` tests a file's
    /// parents: everything beneath an excluded directory is already withheld, so
    /// refusing to descend changes nothing but the time spent.
    pub fn prunes_user_dir(&self, path: &str) -> bool {
        if self.user_empty {
            return false;
        }
        self.user.matched(Path::new(path), true).is_ignore()
    }

    /// Build output and VCS internals, excluded whatever the config says.
    fn matches_dirs(&self, path: &str) -> bool {
        let first = path.split('/').next().unwrap_or("");
        if rc_core::ALWAYS_EXCLUDE.contains(&first) {
            return true;
        }
        self.dirs
            .iter()
            .any(|d| first == d || path.split('/').any(|c| c == d))
    }

    /// Only the user's patterns. Separated because these are the ones that
    /// force the baseline off.
    pub fn matches_user(&self, path: &str) -> bool {
        if self.user_empty {
            return false;
        }
        matches_path_or_parent(&self.user, path)
    }

    /// Whether the L1 git baseline may still be used.
    ///
    /// It may not, as soon as the user excludes anything at all. The baseline
    /// travels as a `git bundle`, and a bundle carries **reachable history** —
    /// not merely the tree at `base_commit`. So it is not enough to check that
    /// no currently-tracked path matches: a secret staged for deletion is
    /// already out of `git ls-files` while still in `HEAD`, and one deleted
    /// three commits ago is out of both while still reachable in the pack.
    /// `git bundle --not <known>` subtracts *objects*, not paths, so it does not
    /// help either.
    ///
    /// No matching over the current index can prove what an object graph
    /// contains, so nothing is claimed. Any exclusion turns the baseline off and
    /// the remaining files travel individually.
    pub fn forbids_baseline(&self) -> bool {
        !self.user_empty
    }

    /// The patterns themselves, for explaining what was withheld.
    pub fn user_patterns(&self) -> &[String] {
        &self.pattern_text
    }

    /// An owned copy carrying only the user patterns, for a `'static` walk
    /// filter. Recompiling a handful of globs costs nothing next to the walk it
    /// prunes.
    pub fn clone_user_matcher(&self) -> Excludes {
        Excludes::new(&[], &self.pattern_text).expect("these patterns already compiled once")
    }
}

/// What travels even though `.gitignore` hides it — `include` in
/// `.remote-compile.toml`.
///
/// Enumeration takes git as the authority on what exists (§4.3), which is right
/// for tracked and untracked files alike and wrong for exactly one case: a file
/// the build reads and `.gitignore` covers. Generated code is the usual one. It
/// appears in none of git's lists, so no amount of scanning finds it, and the
/// failure it produces remotely — a module that will not resolve — has no
/// counterpart in any diff.
///
/// This does *not* undo an exclusion: `Excludes` is applied after enumeration,
/// so a path matching both is still withheld. Nor does it cost the root its git
/// baseline; an included file is by definition untracked, so it travels through
/// the CAS like any other untracked file and the bundle is unaffected.
#[derive(Debug)]
pub struct Includes {
    patterns: Gitignore,
    empty: bool,
    pattern_text: Vec<String>,
}

impl Includes {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let mut builder = GitignoreBuilder::new("");
        for p in patterns {
            builder
                .add_line(None, p)
                .map_err(|e| format!("include pattern `{p}` is not valid: {e}"))?;
        }
        let compiled = builder
            .build()
            .map_err(|e| format!("could not compile the include patterns: {e}"))?;
        Ok(Includes {
            patterns: compiled,
            empty: patterns.is_empty(),
            pattern_text: patterns.to_vec(),
        })
    }

    /// No `include` at all — the ordinary case, and the one that must cost
    /// nothing: enumeration skips the extra walk entirely. Production builds one
    /// from the repo config even when that config is empty, so this exists for
    /// tests that scan without a repository config at all.
    #[cfg(test)]
    pub fn none() -> Self {
        Includes::new(&[]).expect("no patterns cannot fail to compile")
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }

    pub fn matches(&self, path: &str) -> bool {
        if self.empty {
            return false;
        }
        matches_path_or_parent(&self.patterns, path)
    }

    pub fn patterns(&self) -> &[String] {
        &self.pattern_text
    }
}

/// Match a file path against gitignore-syntax patterns, testing its parent
/// directories too.
///
/// Enumeration hands us one file path at a time, never a directory. Ask only
/// about the file and every directory pattern quietly does nothing: `secrets/`,
/// and plain `secrets`, match a *directory*, and git acts on that by never
/// descending into it. Someone writing `exclude = ["secrets"]`, seeing no
/// error, and having the files uploaded anyway is exactly the failure this must
/// not have; `include = ["generated"]` silently pulling in nothing is the same
/// bug pointed the other way.
///
/// Parents are tested outermost-first and a matching one wins outright. That is
/// git's rule — a file cannot be re-included once a parent directory is
/// excluded — and for `exclude` it is also the safe direction:
/// `matched_path_or_any_parents` would let `!secrets/public.txt` reopen the
/// directory, releasing more than the author of `secrets` asked to withhold.
fn matches_path_or_parent(patterns: &Gitignore, path: &str) -> bool {
    let mut prefix = String::new();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if patterns.matched(Path::new(&prefix), true).is_ignore() {
            return true;
        }
    }
    patterns.matched(Path::new(path), false).is_ignore()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(patterns: &[&str]) -> Excludes {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        Excludes::new(&["target"], &owned).unwrap()
    }

    #[test]
    fn structural_exclusions_apply_at_any_depth() {
        let e = ex(&[]);
        assert!(e.matches("target/debug/foo"));
        assert!(e.matches("crates/a/target/foo"));
        assert!(e.matches(".git/config"));
        assert!(e.matches("node_modules/x"));
        assert!(!e.matches("src/target_helper.rs"));
    }

    #[test]
    fn a_withheld_directory_is_pruned_but_a_withheld_file_is_not() {
        // The `include` walk asks this before descending. Testing it end to end
        // is impossible — `exclude` drops those files at the end of the walk
        // either way, so the manifest looks identical whether or not the walk
        // went in. The only observable difference is the time, so the rule is
        // pinned here instead.
        let dir_pattern = ex(&["dumps/"]);
        assert!(dir_pattern.prunes_user_dir("dumps"));
        assert!(!dir_pattern.prunes_user_dir("src"));

        // A pattern naming one file inside a directory must never prune the
        // directory: its siblings are wanted, and pruning would lose them.
        let file_pattern = ex(&["dumps/secret.txt"]);
        assert!(!file_pattern.prunes_user_dir("dumps"));
        assert!(file_pattern.matches("dumps/secret.txt"));
        assert!(!file_pattern.matches("dumps/wanted.txt"));

        assert!(!ex(&[]).prunes_user_dir("dumps"), "no patterns, nothing to prune");
    }

    #[test]
    fn an_include_matches_the_same_way_an_exclude_does() {
        let i = Includes::new(&["*.generated.rs".to_string(), "gen/".to_string()]).unwrap();
        assert!(i.matches("common/src/prisma.generated.rs"));
        assert!(i.matches("gen/deep/file.rs"), "a directory pattern reaches what is under it");
        assert!(!i.matches("src/main.rs"));
    }

    #[test]
    fn no_include_matches_nothing() {
        // The ordinary case. It must also be the cheap one — `is_empty` is what
        // lets enumeration skip the extra walk entirely.
        let i = Includes::none();
        assert!(i.is_empty());
        assert!(!i.matches("anything.rs"));
    }

    #[test]
    fn a_bad_include_pattern_is_reported_not_ignored() {
        let err = Includes::new(&["{unclosed".to_string()]).unwrap_err();
        assert!(err.contains("include pattern"), "{err}");
    }

    #[test]
    fn a_user_pattern_matches_by_glob() {
        let e = ex(&["*.pem", "secrets/**", "user_info.json"]);
        assert!(e.matches("private.pem"));
        assert!(e.matches("deep/nested/key.pem"));
        assert!(e.matches("secrets/prod/token"));
        assert!(e.matches("user_info.json"));
        assert!(!e.matches("src/main.rs"));
        assert!(!e.matches("notes/pem.txt"));
    }

    #[test]
    fn a_directory_pattern_excludes_everything_under_it() {
        // Enumeration only ever presents file paths, so a directory pattern
        // matched against the file alone would never fire — and the user would
        // see no error while the files were uploaded regardless.
        for pattern in ["secrets", "secrets/", "secrets/**"] {
            let e = ex(&[pattern]);
            assert!(
                e.matches("secrets/prod/token"),
                "`{pattern}` must exclude files beneath it"
            );
            assert!(!e.matches("src/main.rs"), "`{pattern}` must not over-match");
        }
    }

    #[test]
    fn an_excluded_directory_cannot_be_reopened_by_a_negation() {
        // git's rule: once a parent directory is excluded, nothing under it can
        // be re-included. Diverging here would produce an exclusion that leaks.
        let e = ex(&["secrets", "!secrets/public.txt"]);
        assert!(e.matches("secrets/public.txt"));
    }

    #[test]
    fn patterns_are_case_sensitive_like_gitignore() {
        // The workers are Linux; matching case-insensitively here would exclude
        // more than the user wrote.
        let e = ex(&["*.PEM"]);
        assert!(e.matches("key.PEM"));
        assert!(!e.matches("key.pem"));
    }

    #[test]
    fn an_anchored_pattern_only_matches_at_the_root() {
        let e = ex(&["/config.local"]);
        assert!(e.matches("config.local"));
        assert!(!e.matches("sub/config.local"));
    }

    #[test]
    fn a_negation_can_readmit_a_file() {
        // gitignore semantics, so `!` works as people expect.
        let e = ex(&["*.pem", "!public.pem"]);
        assert!(e.matches("private.pem"));
        assert!(!e.matches("public.pem"));
    }

    #[test]
    fn structural_and_user_exclusions_are_distinguishable() {
        // Only the user's kind forces the git baseline off; `target/` is not in
        // the baseline anyway and must not disable it.
        let e = ex(&["*.pem"]);
        assert!(e.matches("target/debug/x") && !e.matches_user("target/debug/x"));
        assert!(e.matches("a.pem") && e.matches_user("a.pem"));
    }

    #[test]
    fn any_exclusion_at_all_forbids_the_git_baseline() {
        // A bundle carries reachable history, so a path-by-path check against
        // the current index proves nothing: the secret may be staged for
        // deletion, or deleted three commits ago, and still be in the pack.
        assert!(ex(&["*.pem"]).forbids_baseline());
        assert!(ex(&["unrelated-name-that-matches-nothing"]).forbids_baseline());
        assert!(!ex(&[]).forbids_baseline(), "no patterns, no cost");
    }

    #[test]
    fn a_broken_pattern_is_an_error_rather_than_a_silent_no_op() {
        // Silently compiling to "matches nothing" would look identical to a
        // working exclusion right up until the file is uploaded.
        let err = Excludes::new(&[], &["{unclosed".to_string()]).unwrap_err();
        assert!(err.contains("{unclosed"), "{err}");
    }

    #[test]
    fn a_bracket_pattern_is_taken_literally_rather_than_rejected() {
        // gitignore tolerates these; behaving differently would surprise anyone
        // porting patterns over from .gitignore.
        let e = ex(&["[unclosed"]);
        assert!(e.matches("[unclosed"));
        assert!(!e.matches("unclosed"));
    }
}

