//! Host architecture helpers for worker placement and environment images.
//!
//! Two notions of "arch" meet in the fleet:
//!
//! * **Host arch** — what a worker reports (`std::env::consts::ARCH`:
//!   `x86_64`, `aarch64`, …). Tasks that run natively must land on a matching
//!   machine; images built on a worker are bound to that host arch.
//! * **Cargo target** — an optional triple on the profile (`aarch64-unknown-
//!   linux-gnu`). Its arch component is a strong hint about the intended host
//!   when the image itself has not recorded one yet.
//!
//! Empty demand arch still means "any worker" so a homogeneous fleet keeps
//! working before image metadata is filled in.

/// Canonical host-arch labels used on the wire and in SQLite.
pub fn normalize_host_arch(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => None,
        "x86_64" | "amd64" | "x64" => Some("x86_64".into()),
        "aarch64" | "arm64" => Some("aarch64".into()),
        "arm" | "armv7" | "armv7l" => Some("arm".into()),
        "riscv64" => Some("riscv64".into()),
        other if other
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            // Keep unknown-but-plausible labels so a new worker arch is not
            // silently dropped before the scheduler can match it.
            Some(other.to_string())
        }
        _ => None,
    }
}

/// Infer host arch from a Rust target triple (`aarch64-unknown-linux-gnu`).
pub fn host_arch_from_target(target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    let arch = t.split('-').next().unwrap_or("");
    normalize_host_arch(arch)
}

/// Split a stored image `arch` field (CSV / space / semicolon) into
/// normalised host arches.
pub fn parse_image_arches(arch_field: &str) -> Vec<String> {
    let mut out: Vec<String> = arch_field
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter_map(normalize_host_arch)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// When the image declares exactly one arch, that is a hard placement constraint.
pub fn single_image_arch(arch_field: &str) -> Option<String> {
    let mut arches = parse_image_arches(arch_field);
    if arches.len() == 1 {
        Some(arches.remove(0))
    } else {
        None
    }
}

/// Whether a worker's host arch is acceptable for an image row.
///
/// Empty image arch = any worker may build or run it (legacy / not yet stamped).
pub fn worker_matches_image_arch(worker_arch: &str, image_arch: &str) -> bool {
    let wanted = parse_image_arches(image_arch);
    if wanted.is_empty() {
        return true;
    }
    let Some(w) = normalize_host_arch(worker_arch) else {
        return false;
    };
    wanted.iter().any(|a| a == &w)
}

/// Resolve the architecture demand for task scheduling.
///
/// Priority:
/// 1. Single arch recorded on the environment image (ground truth for a digest);
/// 2. Arch prefix of the profile's cargo `target`;
/// 3. Empty — any online worker (homogeneous fleets, pre-metadata images).
pub fn resolve_demand_arch(image_arch: &str, target: &str) -> String {
    if let Some(a) = single_image_arch(image_arch) {
        return a;
    }
    if let Some(a) = host_arch_from_target(target) {
        return a;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_common_aliases() {
        assert_eq!(normalize_host_arch("amd64").as_deref(), Some("x86_64"));
        assert_eq!(normalize_host_arch("ARM64").as_deref(), Some("aarch64"));
        assert_eq!(normalize_host_arch("x86_64").as_deref(), Some("x86_64"));
        assert_eq!(normalize_host_arch("").as_deref(), None);
    }

    #[test]
    fn target_triple_yields_host_arch() {
        assert_eq!(
            host_arch_from_target("aarch64-unknown-linux-gnu").as_deref(),
            Some("aarch64")
        );
        assert_eq!(
            host_arch_from_target("x86_64-unknown-linux-musl").as_deref(),
            Some("x86_64")
        );
        assert_eq!(host_arch_from_target("").as_deref(), None);
    }

    #[test]
    fn single_image_arch_only_when_unambiguous() {
        assert_eq!(single_image_arch("aarch64").as_deref(), Some("aarch64"));
        assert_eq!(single_image_arch("x86_64, aarch64").as_deref(), None);
        assert_eq!(single_image_arch("").as_deref(), None);
    }

    #[test]
    fn resolve_prefers_image_over_target() {
        assert_eq!(
            resolve_demand_arch("aarch64", "x86_64-unknown-linux-gnu"),
            "aarch64"
        );
        assert_eq!(
            resolve_demand_arch("", "aarch64-unknown-linux-gnu"),
            "aarch64"
        );
        assert_eq!(resolve_demand_arch("", ""), "");
    }

    #[test]
    fn worker_match_respects_empty_and_listed_arches() {
        assert!(worker_matches_image_arch("aarch64", ""));
        assert!(worker_matches_image_arch("aarch64", "aarch64"));
        assert!(worker_matches_image_arch("aarch64", "x86_64,aarch64"));
        assert!(!worker_matches_image_arch("x86_64", "aarch64"));
        assert!(worker_matches_image_arch("arm64", "aarch64"));
    }
}
