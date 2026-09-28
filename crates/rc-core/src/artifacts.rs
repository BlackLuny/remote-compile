//! Build artifacts shipped back to the agent
//! (docs/proposals/build-artifacts.md).
//!
//! Everything here is shared so the three processes agree on what a declared
//! path means: the agent normalises it into the fingerprint, the server
//! re-validates it, and the worker turns it into an in-sandbox copy.

use serde::{Deserialize, Serialize};

use crate::pb::ArtifactSpec;

/// Per-task cap when the declaration names none.
pub const DEFAULT_MAX_TOTAL_MB: u32 = 512;
/// Ceiling a declaration cannot raise past.
pub const HARD_MAX_TOTAL_MB: u32 = 2048;
/// Files per task. A glob matching a whole directory tree is a mistake, not a
/// deliverable.
pub const MAX_FILES: usize = 256;
/// Patterns per declaration.
pub const MAX_PATTERNS: usize = 64;
/// How long the control plane keeps artifacts after the task finished.
pub const DEFAULT_TTL_SECS: i64 = 24 * 3600;

/// Where the collect step writes, inside the sandbox.
pub const OUT_MOUNT: &str = "/rc/out";
/// The pattern list the collect step reads, inside the sandbox.
pub const LIST_MOUNT: &str = "/rc/artifact-list";

/// `[artifacts]` in `.remote-compile.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactsConfig {
    pub paths: Option<Vec<String>>,
    pub auto: Option<bool>,
    pub max_total_mb: Option<u32>,
}

impl ArtifactsConfig {
    pub fn to_spec(&self) -> ArtifactSpec {
        ArtifactSpec {
            paths: self.paths.clone().unwrap_or_default(),
            auto: self.auto.unwrap_or(false),
            max_total_mb: self.max_total_mb.unwrap_or(0),
        }
    }
}

/// Whether anything is to be collected at all.
pub fn is_declared(spec: Option<&ArtifactSpec>) -> bool {
    spec.is_some_and(|s| s.auto || !s.paths.is_empty())
}

/// Only tasks that produce something worth shipping collect artifacts; a
/// declaration on any other task type is ignored and kept out of its
/// fingerprint.
pub fn applies_to(task_type: crate::TaskType) -> bool {
    matches!(task_type, crate::TaskType::Build | crate::TaskType::Custom)
}

/// What the control plane accepts from the wire: normalised, or `None` when
/// the task type does not collect or nothing is declared.
pub fn effective_spec(
    spec: Option<&ArtifactSpec>,
    task_type: crate::TaskType,
) -> Result<Option<ArtifactSpec>, String> {
    match spec {
        Some(s) if applies_to(task_type) && is_declared(Some(s)) => normalize(s).map(Some),
        _ => Ok(None),
    }
}

/// One declared path, checked. Paths are relative to the sub-project and may
/// not climb out of it; the sandbox would contain the damage, but a `..` in a
/// declaration is always a mistake and saying so beats collecting nothing.
pub fn validate_pattern(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    let p = p.strip_prefix("./").unwrap_or(p);
    if p.is_empty() {
        return Err("empty artifact path".into());
    }
    if p.len() > 512 {
        return Err(format!("artifact path too long: {}…", &p[..64]));
    }
    if p.starts_with('/') {
        return Err(format!("artifact path must be relative: {p}"));
    }
    if p.chars().any(|c| c == '\n' || c == '\r' || c == '\0' || c == '\\') {
        return Err(format!("artifact path contains a forbidden character: {p:?}"));
    }
    if p.split('/').any(|seg| seg == "..") {
        return Err(format!("artifact path may not contain `..`: {p}"));
    }
    Ok(p.trim_end_matches('/').to_string())
}

/// Validated, sorted, deduplicated, limits clamped — the only form that is
/// hashed or executed.
pub fn normalize(spec: &ArtifactSpec) -> Result<ArtifactSpec, String> {
    let mut paths = Vec::with_capacity(spec.paths.len());
    for p in &spec.paths {
        paths.push(validate_pattern(p)?);
    }
    paths.sort();
    paths.dedup();
    if paths.len() > MAX_PATTERNS {
        return Err(format!("at most {MAX_PATTERNS} artifact paths"));
    }
    let max = match spec.max_total_mb {
        0 => DEFAULT_MAX_TOTAL_MB,
        n => n.min(HARD_MAX_TOTAL_MB),
    };
    Ok(ArtifactSpec {
        paths,
        auto: spec.auto,
        max_total_mb: max,
    })
}

/// Fingerprint lines. Emitted only when something is declared, so a profile
/// without artifacts hashes exactly as it did before this feature existed.
pub fn canonical_lines(spec: Option<&ArtifactSpec>) -> String {
    let Some(spec) = spec.filter(|s| is_declared(Some(s))) else {
        return String::new();
    };
    let n = normalize(spec).unwrap_or_else(|_| spec.clone());
    format!(
        "artifacts.paths={}\nartifacts.auto={}\nartifacts.max_total_mb={}\n",
        n.paths.join(","),
        n.auto,
        n.max_total_mb
    )
}

/// A declared pattern as the sandbox should expand it: `target/…` points at
/// the build's `CARGO_TARGET_DIR`, everything else stays relative to the
/// sub-project directory the collect step runs in.
pub fn container_pattern(pattern: &str, target_mount: Option<&str>) -> String {
    match target_mount {
        Some(t) if pattern == "target" => t.to_string(),
        Some(t) => match pattern.strip_prefix("target/") {
            Some(rest) => format!("{t}/{rest}"),
            None => pattern.to_string(),
        },
        None => pattern.to_string(),
    }
}

/// Executables produced by workspace members, from cargo's
/// `--message-format=json` stdout. Registry and git dependencies are skipped:
/// their build-script binaries are not what anybody asked for.
pub fn cargo_executables(stdout: &str, target_mount: &str) -> Vec<String> {
    let prefix = format!("{}/", target_mount.trim_end_matches('/'));
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') || !line.contains("compiler-artifact") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let pkg = v.get("package_id").and_then(|p| p.as_str()).unwrap_or("");
        if !pkg.contains("path+file://") {
            continue;
        }
        let Some(exe) = v.get("executable").and_then(|e| e.as_str()) else {
            continue;
        };
        // Build scripts report themselves too, as `build-script-build`.
        if v
            .pointer("/target/kind")
            .and_then(|k| k.as_array())
            .is_some_and(|k| k.iter().any(|x| x.as_str() == Some("custom-build")))
        {
            continue;
        }
        // The path becomes a glob in the collect step; a literal one it must
        // stay, whatever a dependency printed onto cargo's stdout.
        let globby = exe.contains(['*', '?', '[']);
        if exe.starts_with(&prefix) && !globby && validate_pattern(&exe[1..]).is_ok() {
            out.push(exe.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Notes kept per task. Past this, "and N more" — a build can create a
/// million files, and the note travels inside the result message.
pub const MAX_NOTES: usize = 20;

/// The collect step, run in a second sandbox after a successful build. It
/// reads patterns from `LIST_MOUNT` rather than having them spliced into the
/// script, so no declared path is ever parsed as shell. The unquoted `$pat`
/// is deliberate: it is how the glob gets expanded, and expansion is all the
/// shell does with it — no command substitution happens on a variable's value.
///
/// Links are dereferenced *here*, inside the container, which is the point: a
/// link to `/etc/shadow` resolves to the image's file, not the worker host's.
/// What a link may resolve to is still hostile — `/dev/zero` reports size 0
/// and never ends — so only regular files are taken, and every copy is capped
/// at the bytes left in the budget whatever the file claims to be. `PATH` is
/// pinned to the image's system directories: the target and registry volumes
/// are writable by the build, and a tool planted there must not be what runs.
pub fn collect_script(target_mount: Option<&str>) -> String {
    let t = target_mount.unwrap_or("/rc/target");
    format!(
        r#"set -u
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
nl='
'
IFS="$nl"
left=${{RC_ARTIFACT_MAX_BYTES:-0}}
files=0
notes=0
note() {{
  notes=$((notes + 1))
  if [ "$notes" -le {max_notes} ]; then echo "rc-artifact: $1"; fi
}}
while IFS= read -r pat; do
  [ -n "$pat" ] || continue
  found=0
  for m in $pat; do
    case "$m" in *"$nl"*|*"	"*) note "skipped a name with control characters"; continue ;; esac
    case "/$m/" in */../*|*/./*) note "skipped path with a dot segment: $m"; continue ;; esac
    [ -e "$m" ] || continue
    if [ -d "$m" ]; then note "skipped directory: $m"; continue; fi
    if [ ! -f "$m" ]; then note "skipped non-regular file: $m"; continue; fi
    case "$m" in
      {t}/*) d="target/${{m#{t}/}}" ;;
      /*) note "skipped path outside the project: $m"; continue ;;
      *) d="$m" ;;
    esac
    if [ "$files" -ge {max_files} ]; then note "over file limit: $d"; continue; fi
    mkdir -p "{out}/$(dirname "$d")" || {{ note "copy failed: $d"; continue; }}
    head -c "$((left + 1))" < "$m" > "{out}/$d" || {{ rm -f "{out}/$d"; note "copy failed: $d"; continue; }}
    s=$(wc -c < "{out}/$d")
    if [ "$s" -gt "$left" ]; then rm -f "{out}/$d"; note "over size limit: $d"; continue; fi
    if [ -x "$m" ]; then chmod 755 "{out}/$d"; else chmod 644 "{out}/$d"; fi
    left=$((left - s))
    files=$((files + 1))
    found=1
  done
  [ "$found" = 1 ] || note "not found: $pat"
done < {list}
if [ "$notes" -gt {max_notes} ]; then echo "rc-artifact: ... and $((notes - {max_notes})) more"; fi
"#,
        max_files = MAX_FILES,
        max_notes = MAX_NOTES,
        out = OUT_MOUNT,
        list = LIST_MOUNT,
    )
}

/// `rc-artifact:` lines the collect step printed, for the result note.
pub fn collect_notes(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("rc-artifact: "))
        .map(|s| s.to_string())
        .collect()
}

/// Cap a list of notes for the wire: the first `MAX_NOTES`, then a count.
pub fn cap_notes(mut notes: Vec<String>) -> Vec<String> {
    if notes.len() > MAX_NOTES {
        let more = notes.len() - MAX_NOTES;
        notes.truncate(MAX_NOTES);
        notes.push(format!("... and {more} more"));
    }
    for n in &mut notes {
        if n.len() > 300 {
            let mut cut = 300;
            while !n.is_char_boundary(cut) {
                cut -= 1;
            }
            n.truncate(cut);
            n.push('…');
        }
    }
    notes
}

/// A path from the wire that is about to become a path on disk.
pub fn is_safe_relative(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.chars().any(|c| c.is_control())
        && p.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_relative_and_cannot_climb() {
        assert_eq!(validate_pattern("./target/release/x").unwrap(), "target/release/x");
        assert_eq!(validate_pattern("dist/").unwrap(), "dist");
        assert!(validate_pattern("/etc/passwd").is_err());
        assert!(validate_pattern("../x").is_err());
        assert!(validate_pattern("a/../../x").is_err());
        assert!(validate_pattern("a\nb").is_err());
        assert!(validate_pattern("  ").is_err());
    }

    #[test]
    fn an_undeclared_spec_leaves_the_canonical_form_alone() {
        assert_eq!(canonical_lines(None), "");
        assert_eq!(canonical_lines(Some(&ArtifactSpec::default())), "");
        let a = canonical_lines(Some(&ArtifactSpec {
            paths: vec!["b".into(), "./a".into(), "b".into()],
            auto: false,
            max_total_mb: 0,
        }));
        let b = canonical_lines(Some(&ArtifactSpec {
            paths: vec!["a".into(), "b".into()],
            auto: false,
            max_total_mb: DEFAULT_MAX_TOTAL_MB,
        }));
        assert_eq!(a, b);
        assert!(a.contains("artifacts.paths=a,b"));
    }

    #[test]
    fn limits_are_clamped() {
        let n = normalize(&ArtifactSpec {
            paths: vec![],
            auto: true,
            max_total_mb: 100_000,
        })
        .unwrap();
        assert_eq!(n.max_total_mb, HARD_MAX_TOTAL_MB);
    }

    #[test]
    fn target_prefix_maps_to_the_target_mount() {
        let t = Some("/rc/target");
        assert_eq!(container_pattern("target/release/x", t), "/rc/target/release/x");
        assert_eq!(container_pattern("target", t), "/rc/target");
        assert_eq!(container_pattern("targets/x", t), "targets/x");
        assert_eq!(container_pattern("dist/*.wasm", t), "dist/*.wasm");
        assert_eq!(container_pattern("target/x", None), "target/x");
    }

    #[test]
    fn auto_takes_only_workspace_executables() {
        let stdout = [
            r#"{"reason":"compiler-artifact","package_id":"path+file:///work#app@0.1.0","target":{"kind":["bin"]},"executable":"/rc/target/release/app"}"#,
            r#"{"reason":"compiler-artifact","package_id":"path+file:///work#app@0.1.0","target":{"kind":["custom-build"]},"executable":"/rc/target/release/build/app-1/build-script-build"}"#,
            r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0","target":{"kind":["bin"]},"executable":"/rc/target/release/dep"}"#,
            r#"{"reason":"compiler-artifact","package_id":"lib 0.1.0 (path+file:///work/lib)","target":{"kind":["lib"]},"executable":null}"#,
            r#"{"reason":"compiler-artifact","package_id":"path+file:///work#evil@0.1.0","target":{"kind":["bin"]},"executable":"/etc/passwd"}"#,
            r#"{"reason":"compiler-artifact","package_id":"path+file:///work#evil@0.1.0","target":{"kind":["bin"]},"executable":"/rc/target/../../etc/passwd"}"#,
            "not json",
        ]
        .join("\n");
        assert_eq!(cargo_executables(&stdout, "/rc/target"), vec!["/rc/target/release/app"]);
    }

    #[test]
    fn wire_paths_are_checked_before_touching_disk() {
        assert!(is_safe_relative("target/release/app"));
        assert!(!is_safe_relative("../x"));
        assert!(!is_safe_relative("/x"));
        assert!(!is_safe_relative("a//b"));
        assert!(!is_safe_relative("a/./b"));
        assert!(!is_safe_relative(""));
        assert!(!is_safe_relative("a\nb"));
    }

    #[test]
    fn the_collect_script_never_embeds_a_declared_path() {
        let s = collect_script(Some("/rc/target"));
        assert!(s.contains("< /rc/artifact-list"));
        assert!(s.contains("/rc/target/*) d=\"target/${m#/rc/target/}\""));
        assert!(s.contains("[ ! -f \"$m\" ]"), "only regular files");
        assert!(s.contains("head -c"), "copies are bounded");
    }

    #[test]
    fn notes_are_capped_for_the_wire() {
        let notes: Vec<String> = (0..100).map(|i| format!("n{i}")).collect();
        let capped = cap_notes(notes);
        assert_eq!(capped.len(), MAX_NOTES + 1);
        assert_eq!(capped.last().unwrap(), "... and 80 more");
    }
}
