//! Task fingerprints (§5.1).
//!
//! The fingerprint decides whether a previous result may be reused. A missed
//! dimension means serving a stale result for a different situation — a
//! correctness bug, not a performance one. So the *whole* resolved profile is
//! hashed as one canonical blob rather than field-by-field: adding a field to
//! `BuildProfile` can never silently fall out of the fingerprint.

use crate::pb::ResolvedProfile;

/// Inputs to a fingerprint. Constructing this is the only supported way to
/// produce one.
pub struct FingerprintInput<'a> {
    /// blake3 over the full workspace manifest (content, modes, symlinks).
    pub manifest_root_hash: &'a str,
    /// Image **digest** — a tag is mutable and must be resolved first.
    pub image_digest: &'a str,
    /// e.g. `rustc 1.85.0`.
    pub toolchain: &'a str,
    /// Canonical text form of the fully resolved profile, including the
    /// selected task type and the final command line.
    pub profile_canonical: &'a str,
    /// Where the primary root sits inside the workspace mount.
    ///
    /// The manifest hash covers *content*, not shape, and the two can disagree:
    /// a repository that simply contains `app/` and `lib/` produces the same
    /// entry list as a two-root layout mounting `app` and `lib` side by side.
    /// The entries are identical, so `root_hash` is identical — but the first
    /// builds in `/work` and the second in `/work/app`. Without this they share
    /// a fingerprint and one serves the other's cached result.
    pub anchor_mount: &'a str,
}

/// Reasons a set of inputs cannot produce a trustworthy fingerprint.
#[derive(Debug, PartialEq, Eq)]
pub enum FingerprintError {
    /// An unresolved tag would let a mutated image reuse an old result.
    ImageNotDigest(String),
    MissingManifest,
}

impl std::fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FingerprintError::ImageNotDigest(i) => write!(
                f,
                "image `{i}` is not pinned to a digest; resolve the tag before fingerprinting (§5.1)"
            ),
            FingerprintError::MissingManifest => f.write_str("manifest root hash is empty"),
        }
    }
}

impl std::error::Error for FingerprintError {}

/// How the worker turns a (manifest, profile) pair into a running build:
/// where the workspace is mounted, what the working directory ends up being,
/// how the command is invoked, and which contract default_env is injected.
///
/// None of that is visible in the manifest or the profile alone, so without a
/// version here a change to it reuses results computed under the *old*
/// semantics. That is not hypothetical:
/// - `abi2`: mounting the workspace at `/work` instead of at the sub-project
///   path changes which `Cargo.toml` a build with `path = "crates/backend"`
///   actually compiles, while leaving every hashed input identical.
/// - `abi3`: TaskContract default_env (e.g. `CARGO_PROFILE_TEST_DEBUG=0`)
///   changes execution semantics for the same canonical profile fields.
///
/// **Bump triggers (keep this list current):**
/// - workspace mount layout or workdir resolution
/// - contract `default_env` semantic change
/// - command construction that is not already in the profile canonical form
/// - any other execution-only change that would poison the task cache
pub const EXECUTOR_ABI: &str = "abi3";

/// True when an image reference names an immutable digest.
pub fn is_digest_ref(image: &str) -> bool {
    match image.split_once("@") {
        Some((_, digest)) => digest.starts_with("sha256:") && digest.len() == 71,
        None => false,
    }
}

pub fn compute(input: FingerprintInput<'_>) -> Result<String, FingerprintError> {
    if input.manifest_root_hash.is_empty() {
        return Err(FingerprintError::MissingManifest);
    }
    if !is_digest_ref(input.image_digest) {
        return Err(FingerprintError::ImageNotDigest(input.image_digest.into()));
    }
    let profile_hash = blake3::hash(input.profile_canonical.as_bytes());
    let mut h = blake3::Hasher::new();
    // Length-prefixed so no concatenation of two fields can mimic another.
    for part in [
        EXECUTOR_ABI,
        input.anchor_mount,
        input.manifest_root_hash,
        input.image_digest,
        input.toolchain,
        profile_hash.to_hex().as_str(),
    ] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Fold the egress hosts a build may actually reach into its fingerprint.
///
/// The profile already carries what the repository *asked* for; this carries
/// what the control plane *granted*, which is server-side state the agent
/// cannot see. Three things depend on the distinction:
///
/// - approving a host has to invalidate the failure that not having it caused,
///   or the developer keeps being served the pre-approval error;
/// - revoking one has to invalidate the successes it produced;
/// - a second project that copies the first's config gets the same requested
///   list for free, so the request cannot be what separates their caches.
///
/// An empty grant leaves the fingerprint alone, so the ordinary build — no
/// egress at all — still dedups across projects exactly as before.
pub fn with_egress(fingerprint: &str, granted: &[String]) -> String {
    if granted.is_empty() {
        return fingerprint.to_string();
    }
    let mut hosts: Vec<&str> = granted.iter().map(|s| s.as_str()).collect();
    hosts.sort_unstable();
    hosts.dedup();
    let mut h = blake3::Hasher::new();
    for part in std::iter::once(fingerprint).chain(hosts) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().to_hex().to_string()
}

/// Convenience wrapper over the wire type.
pub fn compute_for(
    manifest_root_hash: &str,
    profile: &ResolvedProfile,
    anchor_mount: &str,
) -> Result<String, FingerprintError> {
    compute(FingerprintInput {
        manifest_root_hash,
        image_digest: &profile.image,
        toolchain: &profile.toolchain,
        profile_canonical: &profile.canonical,
        anchor_mount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "reg/env/rust@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST2: &str = "reg/env/rust@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn input<'a>(manifest: &'a str, image: &'a str, tc: &'a str, prof: &'a str) -> FingerprintInput<'a> {
        FingerprintInput {
            manifest_root_hash: manifest,
            image_digest: image,
            toolchain: tc,
            profile_canonical: prof,
            anchor_mount: "",
        }
    }

    #[test]
    fn abi3_does_not_match_abi2_fingerprint() {
        // Contract default_env changes execution semantics; old cached results
        // under abi2 must not be reused.
        let mut h = blake3::Hasher::new();
        for part in [
            "abi2",
            "",
            "m1",
            DIGEST,
            "rustc 1.85.0",
            blake3::hash(b"cmd=cargo test").to_hex().as_str(),
        ] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        let old = h.finalize().to_hex().to_string();
        let new = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo test")).unwrap();
        assert_ne!(old, new, "ABI bump must invalidate the task cache");
        assert_eq!(EXECUTOR_ABI, "abi3");
    }

    #[test]
    fn a_grant_makes_it_a_different_build_and_no_grant_changes_nothing() {
        let base = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap();
        // The ordinary build — no egress at all — must keep deduping across
        // projects exactly as it did before this feature existed.
        assert_eq!(with_egress(&base, &[]), base);

        let granted = with_egress(&base, &["registry.corp".into()]);
        assert_ne!(granted, base, "an approval must invalidate the failure it caused");

        // Revoking is just as much a change as granting: the server folds the
        // *current* grant into the *base* fingerprint on every submission, so
        // losing the grant lands back on the key the ungranted build uses and
        // the successes produced under it are no longer reachable.
        assert_eq!(with_egress(&base, &[]), base);
        assert_ne!(granted, with_egress(&base, &[]));

        // Two projects that were granted different hosts do not share results,
        // even though they may have requested identically.
        let other = with_egress(&base, &["registry.other".into()]);
        assert_ne!(granted, other);
    }

    #[test]
    fn the_order_an_administrator_approved_in_is_not_part_of_the_build() {
        let base = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap();
        let one = with_egress(&base, &["b.example.com".into(), "a.example.com".into()]);
        let two = with_egress(&base, &["a.example.com".into(), "b.example.com".into()]);
        assert_eq!(one, two);
        // …and neither is a duplicate approval.
        assert_eq!(
            one,
            with_egress(&base, &["a.example.com".into(), "b.example.com".into(), "a.example.com".into()])
        );
    }

    #[test]
    fn identical_inputs_give_identical_fingerprints() {
        let a = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap();
        let b = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn every_dimension_changes_the_fingerprint() {
        let base = compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap();
        assert_ne!(base, compute(input("m2", DIGEST, "rustc 1.85.0", "cmd=cargo check")).unwrap());
        assert_ne!(base, compute(input("m1", DIGEST2, "rustc 1.85.0", "cmd=cargo check")).unwrap());
        assert_ne!(base, compute(input("m1", DIGEST, "rustc 1.86.0", "cmd=cargo check")).unwrap());
        assert_ne!(base, compute(input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo clippy")).unwrap());
    }

    #[test]
    fn the_same_files_laid_out_differently_are_not_the_same_task() {
        // A repository that simply contains `app/` and `lib/` produces the same
        // entry list — and therefore the same root_hash — as a two-root layout
        // mounting them side by side. The first builds in /work, the second in
        // /work/app. Sharing a fingerprint would serve one the other's result.
        let mut flat = input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check");
        flat.anchor_mount = "";
        let mut nested = input("m1", DIGEST, "rustc 1.85.0", "cmd=cargo check");
        nested.anchor_mount = "app";
        assert_ne!(compute(flat).unwrap(), compute(nested).unwrap());
    }

    #[test]
    fn a_mutable_tag_is_refused() {
        let err = compute(input("m1", "reg/env/rust:latest", "rustc", "x")).unwrap_err();
        assert!(matches!(err, FingerprintError::ImageNotDigest(_)));
    }

    #[test]
    fn field_boundaries_cannot_be_smeared() {
        // Without length prefixing "ab"+"c" and "a"+"bc" would collide.
        let a = compute(input("ab", DIGEST, "c", "p")).unwrap();
        let b = compute(input("a", DIGEST, "bc", "p")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn pre_commands_are_covered_via_the_canonical_profile() {
        // §5.1: pre_commands generate code; a field-enumerating fingerprint
        // that forgot them would serve a wrong cached result.
        let without = compute(input("m", DIGEST, "tc", "cmd=cargo check\npre=[]")).unwrap();
        let with = compute(input("m", DIGEST, "tc", "cmd=cargo check\npre=[\"xtask codegen\"]")).unwrap();
        assert_ne!(without, with);
    }

    #[test]
    fn digest_detection() {
        assert!(is_digest_ref(DIGEST));
        assert!(!is_digest_ref("rust:1.85"));
        assert!(!is_digest_ref("rust@sha256:short"));
    }
}
