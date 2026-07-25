//! Deciding whether directories outside the repository may be uploaded.
//!
//! `check <path>` names one directory. If cargo reaches outside it, syncing
//! what it reaches means sending code the caller never mentioned to a CAS that
//! is unencrypted at rest (§16) — a sibling checkout can hold keys, customer
//! data, an unrelated employer's source.
//!
//! Telling the user afterwards is not consent: by then the upload has happened
//! and cannot be recalled. So the first time a root appears, the agent stops
//! and asks. The answer lives in `.remote-compile.toml`, which is versioned,
//! reviewable and travels with the branch — the same reasoning §3.2 applies to
//! everything else about how a repository builds.

use rc_core::profile::ExtraRoots;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Consent {
    /// These roots may be synced. Possibly empty.
    Approved(Vec<PathBuf>),
    /// Nothing may be synced until the user says so.
    Blocked { pending: Vec<PathBuf>, message: String },
}

/// Match discovered roots against what the repository permits.
///
/// `discovered` are the roots outside `repo_root`; roots inside it are the
/// repository's own business and never reach here.
pub fn evaluate(repo_root: &Path, discovered: &[PathBuf], policy: Option<&ExtraRoots>) -> Consent {
    if discovered.is_empty() {
        return Consent::Approved(Vec::new());
    }
    match policy {
        Some(p) if p.is_auto() => Consent::Approved(discovered.to_vec()),
        // An explicitly empty list is a decision, not an omission: sync nothing
        // outside the repository. The build then fails on the missing
        // dependency, which is a clear and intended outcome.
        Some(ExtraRoots::Allow(list)) if list.is_empty() => Consent::Approved(Vec::new()),
        Some(p) => {
            let allowed: Vec<PathBuf> = p
                .allowed()
                .iter()
                .map(|entry| resolve_relative(repo_root, entry))
                .collect();
            // Approving a directory approves what is inside it: the contents
            // travel as one tree, so a crate under an allowed root is already
            // covered by that decision.
            let unapproved: Vec<PathBuf> = discovered
                .iter()
                .filter(|d| !allowed.iter().any(|a| d.starts_with(a)))
                .cloned()
                .collect();
            if unapproved.is_empty() {
                Consent::Approved(discovered.to_vec())
            } else {
                Consent::Blocked {
                    message: block_message(repo_root, &unapproved, true),
                    pending: unapproved,
                }
            }
        }
        None => Consent::Blocked {
            message: block_message(repo_root, discovered, false),
            pending: discovered.to_vec(),
        },
    }
}

/// A listed path is relative to the repository root, so `../private_tun` means
/// what it looks like. Canonicalized so that it compares equal to what
/// discovery produced.
fn resolve_relative(repo_root: &Path, entry: &str) -> PathBuf {
    let raw = Path::new(entry);
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { repo_root.join(raw) };
    joined.canonicalize().unwrap_or(joined)
}

fn block_message(repo_root: &Path, pending: &[PathBuf], partial: bool) -> String {
    let mut out = String::new();
    out.push_str(if partial {
        "✗ 需要确认 — 发现了 .remote-compile.toml 未列出的仓库外目录。\n"
    } else {
        "✗ 需要确认 — 这个项目的构建需要仓库之外的目录。\n"
    });
    out.push_str(
        "它们会被上传到控制面的 CAS，而 CAS 静态不加密（§16）；\
         在你确认之前不会同步任何东西。\n\n",
    );
    for p in pending {
        out.push_str(&format!("  {}\n", p.display()));
    }
    out.push_str("\n确认方式：在 ");
    out.push_str(&repo_root.join(rc_core::profile::REPO_CONFIG_FILE).to_string_lossy());
    out.push_str(" 里写入\n\n");
    out.push_str("  extra_roots = [");
    let listed: Vec<String> = pending
        .iter()
        .map(|p| format!("\"{}\"", display_relative(repo_root, p)))
        .collect();
    out.push_str(&listed.join(", "));
    out.push_str("]\n\n");
    out.push_str(
        "或 extra_roots = \"auto\" 一律放行；extra_roots = [] 表示不同步任何仓库外目录\
         （构建会因缺少依赖而明确失败）。",
    );
    out
}

/// `../private_tun` rather than an absolute path, so it can be pasted into the
/// config as-is and stays correct on someone else's machine.
fn display_relative(repo_root: &Path, target: &Path) -> String {
    let mut ups = Vec::new();
    let mut base = repo_root;
    loop {
        if let Ok(rest) = target.strip_prefix(base) {
            let mut parts = ups.clone();
            let rest = rest.to_string_lossy().replace('\\', "/");
            if !rest.is_empty() {
                parts.push(rest);
            }
            if parts.is_empty() {
                return ".".to_string();
            }
            return parts.join("/");
        }
        match base.parent() {
            Some(p) => {
                ups.push("..".to_string());
                base = p;
            }
            None => return target.to_string_lossy().into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/u/code/app")
    }

    #[test]
    fn nothing_discovered_needs_no_permission() {
        assert_eq!(evaluate(&root(), &[], None), Consent::Approved(Vec::new()));
    }

    #[test]
    fn an_unconfigured_repository_blocks_and_says_exactly_what_to_paste() {
        let found = vec![PathBuf::from("/home/u/code/private_tun")];
        let Consent::Blocked { pending, message } = evaluate(&root(), &found, None) else {
            panic!("must block before uploading anything");
        };
        assert_eq!(pending, found);
        assert!(message.contains("/home/u/code/private_tun"));
        // The snippet is relative, so it survives being committed.
        assert!(
            message.contains("extra_roots = [\"../private_tun\"]"),
            "the message must be copy-pasteable:\n{message}"
        );
        assert!(message.contains("CAS"), "the reason has to be stated");
    }

    #[test]
    fn auto_approves_whatever_turns_up() {
        let found = vec![PathBuf::from("/home/u/code/private_tun")];
        let policy = ExtraRoots::Mode("auto".into());
        assert_eq!(evaluate(&root(), &found, Some(&policy)), Consent::Approved(found));
    }

    #[test]
    fn an_allowlist_admits_what_it_lists() {
        // Uses paths that exist, since matching canonicalizes.
        let base = std::env::temp_dir().join(format!("rc-consent-{}", ulid::Ulid::generate()));
        let app = base.join("app");
        let lib = base.join("lib");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        let app = app.canonicalize().unwrap();
        let lib = lib.canonicalize().unwrap();

        let policy = ExtraRoots::Allow(vec!["../lib".into()]);
        assert_eq!(
            evaluate(&app, std::slice::from_ref(&lib), Some(&policy)),
            Consent::Approved(vec![lib])
        );
    }

    #[test]
    fn a_root_appearing_later_is_blocked_even_though_others_were_approved() {
        // The dependency graph changes; a newly added sibling must not ride in
        // on an approval given for a different directory.
        let base = std::env::temp_dir().join(format!("rc-consent2-{}", ulid::Ulid::generate()));
        let app = base.join("app");
        let known = base.join("known");
        let fresh = base.join("fresh");
        for d in [&app, &known, &fresh] {
            std::fs::create_dir_all(d).unwrap();
        }
        let app = app.canonicalize().unwrap();
        let known = known.canonicalize().unwrap();
        let fresh = fresh.canonicalize().unwrap();

        let policy = ExtraRoots::Allow(vec!["../known".into()]);
        let Consent::Blocked { pending, message } =
            evaluate(&app, &[known, fresh.clone()], Some(&policy))
        else {
            panic!("the new root needs its own approval");
        };
        assert_eq!(pending, vec![fresh]);
        assert!(message.contains("未列出"));
    }

    #[test]
    fn an_empty_allowlist_syncs_nothing_external_rather_than_blocking() {
        // Documented as "do not sync anything outside the repository"; blocking
        // instead would make it impossible to say that at all.
        let policy = ExtraRoots::Allow(Vec::new());
        let found = vec![PathBuf::from("/home/u/code/private_tun")];
        assert_eq!(
            evaluate(&root(), &found, Some(&policy)),
            Consent::Approved(Vec::new())
        );
    }

    #[test]
    fn relative_display_walks_up_as_far_as_needed() {
        assert_eq!(
            display_relative(Path::new("/a/b/app"), Path::new("/a/b/lib")),
            "../lib"
        );
        assert_eq!(
            display_relative(Path::new("/a/b/c/app"), Path::new("/a/lib/x")),
            "../../../lib/x"
        );
        assert_eq!(
            display_relative(Path::new("/a/app"), Path::new("/a/app/inner")),
            "inner"
        );
    }

    #[test]
    fn the_config_snippet_parses_back_into_a_policy() {
        // Whatever the message tells the user to paste must actually work.
        let found = vec![PathBuf::from("/home/u/code/private_tun")];
        let Consent::Blocked { message, .. } = evaluate(&root(), &found, None) else {
            panic!("expected a block");
        };
        let line = message
            .lines()
            .find(|l| l.trim_start().starts_with("extra_roots = ["))
            .expect("a snippet is offered")
            .trim();
        let parsed = rc_core::profile::parse_toml(line).expect("the snippet must be valid TOML");
        assert!(parsed.unknown_keys.is_empty(), "extra_roots must be a known key");
        assert_eq!(
            parsed.profile.extra_roots,
            Some(ExtraRoots::Allow(vec!["../private_tun".into()]))
        );
    }

    #[test]
    fn auto_and_a_list_are_both_accepted_by_the_parser() {
        let auto = rc_core::profile::parse_toml("extra_roots = \"auto\"").unwrap();
        assert!(auto.profile.extra_roots.unwrap().is_auto());
        let list = rc_core::profile::parse_toml("extra_roots = [\"../x\"]").unwrap();
        assert!(!list.profile.extra_roots.unwrap().is_auto());
    }
}
