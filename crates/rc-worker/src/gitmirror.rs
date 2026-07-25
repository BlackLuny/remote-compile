//! Per-project git mirrors — the L1 baseline layer (§4.1).
//!
//! Workers never hold upstream credentials. A mirror gains commits from
//! exactly two sources: an anonymously reachable public remote, or a bundle
//! the agent uploaded through the CAS. Anything else falls back to full L2
//! sync, which CAS deduplication makes cheap after the first time.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Mirror {
    pub path: PathBuf,
}

/// How a base commit became available — reported into the task timeline so a
/// slow sync can be explained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BaselineSource {
    AlreadyPresent,
    Fetched,
    Bundle,
    /// Nothing worked; the caller must sync every file through the CAS.
    Unavailable,
}

impl Mirror {
    pub fn open(root: &Path, project_id: &str) -> Result<Self> {
        let path = root.join(format!("{project_id}.git"));
        if !path.join("HEAD").is_file() {
            std::fs::create_dir_all(&path)?;
            run(&path, &["init", "--bare", "--quiet"])
                .with_context(|| format!("git init {}", path.display()))?;
        }
        Ok(Mirror { path })
    }

    pub fn has_commit(&self, commit: &str) -> bool {
        if !is_sha_like(commit) {
            return false;
        }
        run(&self.path, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
    }

    /// §4.1 degradation chain. Each step is tried in order and the first that
    /// works wins; failing every step is expected, not exceptional.
    pub fn ensure_commit(
        &self,
        commit: &str,
        repo_url: Option<&str>,
        bundles: &[PathBuf],
    ) -> BaselineSource {
        if commit.is_empty() {
            return BaselineSource::Unavailable;
        }
        if self.has_commit(commit) {
            return BaselineSource::AlreadyPresent;
        }
        // Bundles first: they came from the agent, so they are the only source
        // that can carry unpushed local commits.
        for bundle in bundles {
            if self.import_bundle(bundle).is_ok() && self.has_commit(commit) {
                return BaselineSource::Bundle;
            }
        }
        if let Some(url) = repo_url.filter(|u| is_anonymous_url(u)) {
            if self.fetch(url, commit).is_ok() && self.has_commit(commit) {
                return BaselineSource::Fetched;
            }
        }
        BaselineSource::Unavailable
    }

    fn fetch(&self, url: &str, commit: &str) -> Result<()> {
        // Fetch the single object rather than the whole history where the
        // server supports it.
        if run(&self.path, &["fetch", "--quiet", "--no-tags", url, commit]).is_ok() {
            return Ok(());
        }
        run(&self.path, &["fetch", "--quiet", "--no-tags", url, "+refs/heads/*:refs/remotes/upstream/*"])?;
        Ok(())
    }

    pub fn import_bundle(&self, bundle: &Path) -> Result<()> {
        let path = bundle
            .to_str()
            .ok_or_else(|| anyhow!("bundle path is not valid UTF-8"))?;
        // A wildcard refspec cannot cover every case: a bundle built from
        // `HEAD` carries the literal ref `HEAD`, which `refs/*` never matches.
        // Enumerate what the bundle actually contains instead.
        let heads = run(&self.path, &["bundle", "list-heads", path])
            .with_context(|| format!("list heads of bundle {path}"))?;
        let specs: Vec<String> = heads
            .lines()
            .filter_map(|line| line.split_once(char::is_whitespace))
            .map(|(_, name)| name.trim())
            .filter(|name| !name.is_empty())
            .map(|name| {
                let dest = name.strip_prefix("refs/").unwrap_or(name);
                format!("+{name}:refs/bundles/{dest}")
            })
            .collect();
        if specs.is_empty() {
            return Err(anyhow!("bundle {path} contains no refs"));
        }
        let mut args = vec!["fetch", "--quiet", "--no-tags", path];
        args.extend(specs.iter().map(|s| s.as_str()));
        run(&self.path, &args).with_context(|| format!("import bundle {path}"))?;
        Ok(())
    }

    /// Materialize the baseline tree into `dest` without a working checkout.
    pub fn extract(&self, commit: &str, dest: &Path) -> Result<()> {
        if !is_sha_like(commit) {
            return Err(anyhow!("refusing to extract non-sha ref {commit}"));
        }
        std::fs::create_dir_all(dest)?;
        let tar = Command::new("git")
            .current_dir(&self.path)
            .args(["archive", "--format=tar", commit])
            .output()
            .context("git archive")?;
        if !tar.status.success() {
            return Err(anyhow!(
                "git archive failed: {}",
                String::from_utf8_lossy(&tar.stderr)
            ));
        }
        let mut child = Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(dest)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("spawn tar")?;
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("tar stdin unavailable"))?
                .write_all(&tar.stdout)?;
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(anyhow!("tar extraction failed"));
        }
        Ok(())
    }
}

/// Only urls a worker could plausibly reach without credentials (§4.1).
pub fn is_anonymous_url(url: &str) -> bool {
    let u = url.trim();
    (u.starts_with("https://") || u.starts_with("http://") || u.starts_with("git://"))
        && !u.contains('@')
}

pub fn is_sha_like(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("run git {args:?}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-{}-{tag}-{}", env!("CARGO_CRATE_NAME"), ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A source repo with one commit, standing in for a developer's worktree.
    fn seed_repo() -> (PathBuf, String) {
        let dir = scratch("src");
        git(&dir, &["init", "--quiet", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "--quiet", "-m", "init"]);
        let sha = Command::new("git")
            .current_dir(&dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        (dir, String::from_utf8_lossy(&sha.stdout).trim().to_string())
    }

    #[test]
    fn credentialed_urls_are_never_fetched() {
        // §4.1: workers hold no upstream credentials, so an authenticated URL
        // can only fail — and failing loudly beats hanging on a prompt.
        assert!(!is_anonymous_url("git@github.com:org/private.git"));
        assert!(!is_anonymous_url("https://user:token@github.com/org/private.git"));
        assert!(!is_anonymous_url("ssh://git@github.com/org/repo.git"));
        assert!(is_anonymous_url("https://github.com/org/public.git"));
    }

    #[test]
    fn sha_detection_rejects_refs_and_injection() {
        assert!(is_sha_like("a1b2c3d"));
        assert!(!is_sha_like("HEAD"));
        assert!(!is_sha_like("main"));
        assert!(!is_sha_like("abc; rm -rf /"));
    }

    #[test]
    fn a_bundle_delivers_an_unpushed_commit_into_the_mirror() {
        // §4.1 step 3 / risk #20: the commit exists nowhere but the dev box.
        let (src, sha) = seed_repo();
        let mirror_root = scratch("mirrors");
        let mirror = Mirror::open(&mirror_root, "p1").unwrap();
        assert!(!mirror.has_commit(&sha));

        let bundle = scratch("bundle").join("base.bundle");
        git(&src, &["bundle", "create", bundle.to_str().unwrap(), "HEAD"]);

        let source = mirror.ensure_commit(&sha, None, &[bundle]);
        assert_eq!(source, BaselineSource::Bundle);
        assert!(mirror.has_commit(&sha));
    }

    #[test]
    fn a_second_task_reuses_the_mirror_without_transfer() {
        let (src, sha) = seed_repo();
        let mirror_root = scratch("mirrors2");
        let mirror = Mirror::open(&mirror_root, "p1").unwrap();
        let bundle = scratch("bundle2").join("base.bundle");
        git(&src, &["bundle", "create", bundle.to_str().unwrap(), "HEAD"]);
        mirror.ensure_commit(&sha, None, &[bundle]);

        // Reopening must not re-init or lose objects.
        let again = Mirror::open(&mirror_root, "p1").unwrap();
        assert_eq!(again.ensure_commit(&sha, None, &[]), BaselineSource::AlreadyPresent);
    }

    #[test]
    fn an_unreachable_baseline_degrades_instead_of_failing() {
        // §4.1 step 4: fall through to full L2 rather than erroring out.
        let mirror_root = scratch("mirrors3");
        let mirror = Mirror::open(&mirror_root, "p1").unwrap();
        let missing = "0".repeat(40);
        assert_eq!(
            mirror.ensure_commit(&missing, Some("git@github.com:org/private.git"), &[]),
            BaselineSource::Unavailable
        );
    }

    #[test]
    fn extract_materializes_the_baseline_tree() {
        let (src, sha) = seed_repo();
        let mirror_root = scratch("mirrors4");
        let mirror = Mirror::open(&mirror_root, "p1").unwrap();
        let bundle = scratch("bundle4").join("base.bundle");
        git(&src, &["bundle", "create", bundle.to_str().unwrap(), "HEAD"]);
        mirror.ensure_commit(&sha, None, &[bundle]);

        let dest = scratch("extract");
        mirror.extract(&sha, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a.rs")).unwrap(), "fn main() {}\n");
        assert_eq!(std::fs::read_to_string(dest.join("src/lib.rs")).unwrap(), "pub fn f() {}\n");
    }

    #[test]
    fn extract_refuses_a_non_sha_ref() {
        let mirror_root = scratch("mirrors5");
        let mirror = Mirror::open(&mirror_root, "p1").unwrap();
        assert!(mirror.extract("HEAD", &scratch("dest")).is_err());
    }
}
