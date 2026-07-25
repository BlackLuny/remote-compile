//! Multi-root layout: where several local directories sit relative to each
//! other, and where they go inside the container.
//!
//! A cargo `path` dependency may point outside the repository — `../private_tun`
//! is ordinary in workspaces with vendored forks or shared private crates.
//! Those literals live in the user's `Cargo.toml`, which we do not rewrite, so
//! the only way to make them resolve remotely is to **preserve the relative
//! positions of the directories involved**.
//!
//! The construction: take the deepest common ancestor of every root — the
//! *anchor* — and map it to the workspace mount. Each root then sits at its own
//! path relative to that anchor, exactly as it does locally.
//!
//! ```text
//! /home/u/code/            (anchor)   ->  /work
//! /home/u/code/app         (primary)  ->  /work/app     <- build runs here
//! /home/u/code/private_tun            ->  /work/private_tun
//! ```
//!
//! With a single root the anchor *is* that root, its mount is empty, and every
//! path is byte-identical to what a single-root sync has always produced. That
//! is deliberate: existing projects keep their manifests, their `root_hash` and
//! their cached results.

use std::path::{Path, PathBuf};

/// How deep a root may sit below the anchor. Roots spread across unrelated
/// trees (`/opt/lib` and `/home/u/app`) drag the anchor up towards `/`, and a
/// container layout of `/work/home/u/app` is a sign the inputs are wrong rather
/// than something to silently accept.
pub const MAX_MOUNT_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMount {
    /// Canonical local directory.
    pub path: PathBuf,
    /// Path relative to the anchor, '/' separated. Empty for the anchor itself,
    /// which happens exactly when there is one root.
    pub mount: String,
    pub primary: bool,
    /// True when this root sits inside another root of the same layout. Such a
    /// root is only scanned if the enclosing root's enumeration does **not**
    /// already cover it — see `Layout::enclosing`.
    pub nested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub anchor: PathBuf,
    /// Outermost first, so a root is always seen after anything enclosing it.
    pub roots: Vec<RootMount>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// A root is not below the computed anchor — cannot happen for a real
    /// common ancestor, so it means the inputs were not canonical.
    NotUnderAnchor(PathBuf),
    TooDeep { path: PathBuf, depth: usize },
    /// Only `..`-free, non-empty components can become a mount.
    UnusableComponent(PathBuf),
    NoRoots,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::NotUnderAnchor(p) => {
                write!(f, "{} is not below the common ancestor", p.display())
            }
            LayoutError::TooDeep { path, depth } => write!(
                f,
                "{} sits {depth} levels below the common ancestor of all roots (limit {MAX_MOUNT_DEPTH}); \
                 the directories involved are too far apart to lay out sensibly",
                path.display()
            ),
            LayoutError::UnusableComponent(p) => {
                write!(f, "{} contains a path component that cannot be mounted", p.display())
            }
            LayoutError::NoRoots => f.write_str("no roots to lay out"),
        }
    }
}

impl std::error::Error for LayoutError {}

impl Layout {
    pub fn primary(&self) -> &RootMount {
        self.roots
            .iter()
            .find(|r| r.primary)
            .expect("a layout always has a primary root")
    }

    /// The mount of the primary root — what the worker prefixes the L1 baseline
    /// with, and what the build's working directory is derived from.
    pub fn anchor_mount(&self) -> &str {
        &self.primary().mount
    }

    /// The root that encloses `index`, if any. Outermost-first ordering means
    /// the answer is always earlier in the list.
    pub fn enclosing(&self, index: usize) -> Option<&RootMount> {
        let me = &self.roots[index];
        self.roots[..index]
            .iter()
            .rev()
            .find(|other| me.path.starts_with(&other.path))
    }

    /// `path` expressed relative to `enclosing`, e.g. `local-crates/foo`.
    pub fn relative_to(inner: &RootMount, outer: &RootMount) -> Option<String> {
        inner
            .path
            .strip_prefix(&outer.path)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
    }
}

/// Lay out `primary` plus `extras`.
///
/// Inputs must already be canonical directories. Duplicates collapse; a root
/// listed twice, or listed alongside itself under another name, appears once.
pub fn compute(primary: &Path, extras: &[PathBuf]) -> Result<Layout, LayoutError> {
    let mut unique: Vec<PathBuf> = vec![primary.to_path_buf()];
    for e in extras {
        if !unique.iter().any(|u| u == e) {
            unique.push(e.clone());
        }
    }

    // Roots enclosed by another root cannot move the common ancestor, so they
    // are excluded from the computation. Including them would be harmless but
    // makes the reasoning harder to follow.
    let outermost: Vec<&PathBuf> = unique
        .iter()
        .filter(|p| !unique.iter().any(|o| o != *p && p.starts_with(o)))
        .collect();
    let anchor = common_ancestor(&outermost).ok_or(LayoutError::NoRoots)?;

    let mut roots = Vec::with_capacity(unique.len());
    for path in &unique {
        let mount = mount_for(&anchor, path)?;
        let nested = unique.iter().any(|o| o != path && path.starts_with(o));
        roots.push(RootMount {
            path: path.clone(),
            mount,
            primary: path == primary,
            nested,
        });
    }
    // Outermost first: shorter mounts enclose longer ones, and the empty mount
    // (single root) sorts first naturally.
    roots.sort_by(|a, b| {
        (a.mount.matches('/').count(), &a.mount).cmp(&(b.mount.matches('/').count(), &b.mount))
    });

    Ok(Layout { anchor, roots })
}

fn mount_for(anchor: &Path, path: &Path) -> Result<String, LayoutError> {
    let rel = path
        .strip_prefix(anchor)
        .map_err(|_| LayoutError::NotUnderAnchor(path.to_path_buf()))?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(s) => {
                let text = s.to_string_lossy();
                if text.is_empty() || text.contains('/') || text.contains('\\') {
                    return Err(LayoutError::UnusableComponent(path.to_path_buf()));
                }
                parts.push(text.into_owned());
            }
            // A canonical path below a canonical ancestor has nothing else in
            // it; anything here means the caller skipped canonicalization.
            _ => return Err(LayoutError::UnusableComponent(path.to_path_buf())),
        }
    }
    if parts.len() > MAX_MOUNT_DEPTH {
        return Err(LayoutError::TooDeep {
            path: path.to_path_buf(),
            depth: parts.len(),
        });
    }
    Ok(parts.join("/"))
}

fn common_ancestor(paths: &[&PathBuf]) -> Option<PathBuf> {
    let mut iter = paths.iter();
    let mut acc: PathBuf = (*iter.next()?).clone();
    for p in iter {
        acc = two_way_ancestor(&acc, p)?;
    }
    Some(acc)
}

fn two_way_ancestor(a: &Path, b: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) if x == y => out.push(x.as_os_str()),
            _ => break,
        }
    }
    (out.as_os_str().is_empty()).then_some(()).map_or(Some(out), |_| None)
}

/// `to` expressed relative to `from`, e.g. `../lib`, using `..` as needed.
/// Purely textual: no filesystem access, no symlink resolution.
pub fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - shared];
    parts.extend(
        to[shared..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Prefix a root-relative path with the root's mount, giving the
/// anchor-relative path that goes into the manifest.
pub fn anchored(mount: &str, path: &str) -> String {
    if mount.is_empty() {
        path.to_string()
    } else {
        format!("{mount}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn mounts(l: &Layout) -> Vec<String> {
        l.roots.iter().map(|r| r.mount.clone()).collect()
    }

    #[test]
    fn a_single_root_anchors_on_itself_and_mounts_at_the_top() {
        // The compatibility guarantee: every path stays exactly what a
        // single-root sync has always produced, so root_hash does not move.
        let l = compute(&p("/home/u/app"), &[]).unwrap();
        assert_eq!(l.anchor, p("/home/u/app"));
        assert_eq!(mounts(&l), vec![""]);
        assert_eq!(l.anchor_mount(), "");
        assert_eq!(anchored(l.anchor_mount(), "src/main.rs"), "src/main.rs");
    }

    #[test]
    fn siblings_anchor_on_their_parent_and_keep_their_relative_positions() {
        let l = compute(
            &p("/home/u/code/app"),
            &[p("/home/u/code/private_tun"), p("/home/u/code/shadow-tls")],
        )
        .unwrap();
        assert_eq!(l.anchor, p("/home/u/code"));
        assert_eq!(l.anchor_mount(), "app");
        assert_eq!(mounts(&l), vec!["app", "private_tun", "shadow-tls"]);

        // The whole point: `../private_tun` from inside the primary resolves to
        // the same place it does locally.
        assert_eq!(anchored("app", "common/Cargo.toml"), "app/common/Cargo.toml");
        assert_eq!(anchored("private_tun", "Cargo.toml"), "private_tun/Cargo.toml");
    }

    #[test]
    fn roots_at_different_depths_still_share_an_anchor() {
        let l = compute(&p("/a/b/c/app"), &[p("/a/lib")]).unwrap();
        assert_eq!(l.anchor, p("/a"));
        assert_eq!(l.anchor_mount(), "b/c/app");
        assert_eq!(mounts(&l), vec!["lib", "b/c/app"]);
    }

    #[test]
    fn duplicates_collapse() {
        let l = compute(&p("/a/app"), &[p("/a/lib"), p("/a/lib"), p("/a/app")]).unwrap();
        assert_eq!(l.roots.len(), 2);
    }

    #[test]
    fn a_nested_root_is_flagged_and_ordered_after_the_root_that_contains_it() {
        let l = compute(&p("/a/app"), &[p("/a/app/local-crates/foo")]).unwrap();
        // The nested root cannot move the anchor.
        assert_eq!(l.anchor, p("/a/app"));
        assert_eq!(mounts(&l), vec!["", "local-crates/foo"]);
        assert!(!l.roots[0].nested);
        assert!(l.roots[1].nested);
        assert_eq!(l.enclosing(1).unwrap().path, p("/a/app"));
        assert_eq!(
            Layout::relative_to(&l.roots[1], &l.roots[0]).unwrap(),
            "local-crates/foo"
        );
    }

    #[test]
    fn the_outermost_root_comes_first_so_coverage_can_be_decided_in_order() {
        let l = compute(&p("/a/app"), &[p("/a/app/x/y"), p("/a/app/x")]).unwrap();
        assert_eq!(mounts(&l), vec!["", "x", "x/y"]);
        assert_eq!(l.enclosing(2).unwrap().mount, "x");
        assert!(l.enclosing(0).is_none());
    }

    #[test]
    fn wildly_separated_roots_are_refused_rather_than_laid_out_under_slash() {
        // Nothing in common but `/`, so every component of the primary becomes
        // a mount component.
        let deep = p("/home/user/a/b/c/d/e/f/g/app");
        assert!(deep.components().count() - 1 > MAX_MOUNT_DEPTH);
        let err = compute(&deep, &[p("/opt/lib")]).unwrap_err();
        assert!(matches!(err, LayoutError::TooDeep { .. }), "{err:?}");
        assert!(err.to_string().contains("too far apart"));
    }

    #[test]
    fn separated_roots_within_the_depth_limit_are_allowed() {
        // Anchoring on `/` is ugly but workable when the paths are short.
        let l = compute(&p("/srv/app"), &[p("/opt/lib")]).unwrap();
        assert_eq!(l.anchor, p("/"));
        assert_eq!(mounts(&l), vec!["opt/lib", "srv/app"]);
    }

    #[test]
    fn a_root_that_is_the_anchor_of_others_mounts_at_the_top() {
        // Primary encloses the extra: anchor is the primary itself.
        let l = compute(&p("/a"), &[p("/a/b")]).unwrap();
        assert_eq!(l.anchor, p("/a"));
        assert_eq!(l.anchor_mount(), "");
        assert_eq!(mounts(&l), vec!["", "b"]);
    }

    #[test]
    fn the_primary_is_always_identifiable() {
        let l = compute(&p("/a/app"), &[p("/a/lib")]).unwrap();
        assert_eq!(l.primary().path, p("/a/app"));
        assert_eq!(l.roots.iter().filter(|r| r.primary).count(), 1);
    }

    #[test]
    fn mount_depth_is_measured_from_the_anchor_not_from_the_filesystem_root() {
        // Deep absolute paths are fine as long as the roots are close together.
        let l = compute(
            &p("/very/deep/absolute/path/that/keeps/going/code/app"),
            &[p("/very/deep/absolute/path/that/keeps/going/code/lib")],
        )
        .unwrap();
        assert_eq!(mounts(&l), vec!["app", "lib"]);
    }
}
