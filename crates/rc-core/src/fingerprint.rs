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
/// how the command is invoked.
///
/// None of that is visible in the manifest or the profile, so without a version
/// here a change to it reuses results computed under the *old* semantics. That
/// is not hypothetical: `abi2` exists because mounting the workspace at `/work`
/// instead of at the sub-project path changes which `Cargo.toml` a build with
/// `path = "crates/backend"` actually compiles, while leaving every hashed
/// input identical.
///
/// **Bump this whenever execution semantics change**, even when no type does.
pub const EXECUTOR_ABI: &str = "abi2";

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

/// Convenience wrapper over the wire type.
pub fn compute_for(manifest_root_hash: &str, profile: &ResolvedProfile) -> Result<String, FingerprintError> {
    compute(FingerprintInput {
        manifest_root_hash,
        image_digest: &profile.image,
        toolchain: &profile.toolchain,
        profile_canonical: &profile.canonical,
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
        }
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
