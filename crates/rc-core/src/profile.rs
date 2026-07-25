//! Build profiles (§3.2) and their resolution chain.

use crate::model::TaskType;
use crate::pb::ResolvedProfile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Filename looked for at the repo root / sub-project root. Living in the
/// repo means it is versioned, reviewable and travels with a branch.
pub const REPO_CONFIG_FILE: &str = ".remote-compile.toml";

/// A partially specified profile. Every field is optional so layers can be
/// stacked without a lower layer having to repeat what a higher one set.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildProfile {
    pub adapter: Option<String>,
    pub image: Option<String>,
    pub path: Option<String>,
    pub target: Option<String>,
    pub toolchain: Option<String>,
    pub timeout_secs: Option<u32>,
    pub features: Option<Vec<String>>,
    pub pre_commands: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub tasks: BTreeMap<String, String>,
    /// Directories outside this repository that the build needs — cargo `path`
    /// dependencies pointing at sibling checkouts, typically.
    ///
    /// Syncing them means uploading code the caller did not name, to a CAS that
    /// is not encrypted at rest (§16), so it is not something to infer. Absent,
    /// the agent reports what it found and waits.
    pub extra_roots: Option<ExtraRoots>,
}

/// What the repository permits beyond its own root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtraRoots {
    /// `extra_roots = ["../private_tun"]` — exactly these, relative to the repo
    /// root. Discovering anything else is an error rather than a silent
    /// upload. `[]` means "none", and a build needing one will fail plainly.
    Allow(Vec<String>),
    /// `extra_roots = "auto"` — whatever discovery finds, no further questions.
    Mode(String),
}

impl ExtraRoots {
    pub fn is_auto(&self) -> bool {
        matches!(self, ExtraRoots::Mode(m) if m.eq_ignore_ascii_case("auto"))
    }

    /// The listed paths, or none for `auto`.
    pub fn allowed(&self) -> &[String] {
        match self {
            ExtraRoots::Allow(v) => v,
            ExtraRoots::Mode(_) => &[],
        }
    }
}

/// Where a resolved profile ultimately came from — reported to the agent so
/// it can tell "the fleet already knows this project" from "I guessed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// Caller passed it in this call.
    Explicit,
    /// `.remote-compile.toml` in the repo.
    Repo,
    /// Stored on the control plane by another agent (fleet learning).
    Server,
    /// Adapter auto-detection.
    Detected,
}

impl ProfileSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileSource::Explicit => "explicit",
            ProfileSource::Repo => "repo",
            ProfileSource::Server => "server",
            ProfileSource::Detected => "detected",
        }
    }
}

#[derive(Debug)]
pub struct ParsedProfile {
    pub profile: BuildProfile,
    /// Keys we did not recognise — surfaced rather than silently dropped so a
    /// typo in a repo config does not turn into a mystery.
    pub unknown_keys: Vec<String>,
}

const KNOWN_KEYS: &[&str] = &[
    "adapter",
    "image",
    "path",
    "target",
    "toolchain",
    "timeout_secs",
    "features",
    "pre_commands",
    "env",
    "tasks",
    "extra_roots",
];

pub fn parse_toml(text: &str) -> Result<ParsedProfile, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut unknown_keys = Vec::new();
    if let Some(table) = value.as_table() {
        for k in table.keys() {
            if !KNOWN_KEYS.contains(&k.as_str()) {
                unknown_keys.push(k.clone());
            }
        }
    }
    let profile: BuildProfile = value.try_into().map_err(|e: toml::de::Error| e.to_string())?;
    Ok(ParsedProfile { profile, unknown_keys })
}

pub fn to_toml(p: &BuildProfile) -> String {
    toml::to_string_pretty(p).unwrap_or_default()
}

impl BuildProfile {
    /// Fill unset fields from a lower-priority layer. Maps merge key-wise so a
    /// repo config can add one env var without restating the server's.
    pub fn overlay(&mut self, lower: &BuildProfile) {
        macro_rules! fill {
            ($($f:ident),+) => { $( if self.$f.is_none() { self.$f = lower.$f.clone(); } )+ };
        }
        fill!(adapter, image, path, target, toolchain, timeout_secs, features, pre_commands, extra_roots);
        for (k, v) in &lower.env {
            self.env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &lower.tasks {
            self.tasks.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

/// Apply the priority chain of §3.2: explicit > repo > server > detected.
pub fn resolve(layers: Vec<(ProfileSource, BuildProfile)>) -> (BuildProfile, ProfileSource) {
    let mut merged = BuildProfile::default();
    // The winning source is the first layer that contributed anything.
    let mut winner = ProfileSource::Detected;
    let mut have_winner = false;
    for (source, layer) in layers {
        let empty = layer == BuildProfile::default();
        if !empty && !have_winner {
            winner = source;
            have_winner = true;
        }
        merged.overlay(&layer);
    }
    (merged, winner)
}

/// A profile plus the concrete task selected from it. This is what gets
/// hashed and what the worker executes.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub profile: BuildProfile,
    pub source: ProfileSource,
    pub task_type: TaskType,
    pub command: String,
    pub adapter: String,
    pub image_digest: String,
    pub toolchain: String,
}

impl Resolution {
    /// Deterministic text form fed to the fingerprint (§5.1). Maps are sorted
    /// and every field is emitted, including empty ones, so that adding a
    /// field can never be a silent no-op for the hash.
    pub fn canonical(&self) -> String {
        let p = &self.profile;
        let mut s = String::new();
        let mut push = |k: &str, v: &str| {
            s.push_str(k);
            s.push('=');
            s.push_str(v);
            s.push('\n');
        };
        push("adapter", &self.adapter);
        push("image", &self.image_digest);
        push("toolchain", &self.toolchain);
        push("path", p.path.as_deref().unwrap_or(""));
        push("target", p.target.as_deref().unwrap_or(""));
        push("timeout_secs", &p.timeout_secs.unwrap_or(crate::DEFAULT_TASK_TIMEOUT_SECS).to_string());
        push("task_type", self.task_type.as_str());
        push("command", &self.command);
        let mut features = p.features.clone().unwrap_or_default();
        features.sort();
        features.dedup();
        push("features", &features.join(","));
        // Order of pre_commands is semantically meaningful, so it is preserved.
        for (i, c) in p.pre_commands.clone().unwrap_or_default().iter().enumerate() {
            push(&format!("pre_commands[{i}]"), c);
        }
        for (k, v) in &p.env {
            push(&format!("env[{k}]"), v);
        }
        s
    }

    pub fn to_pb(&self) -> ResolvedProfile {
        let p = &self.profile;
        ResolvedProfile {
            adapter: self.adapter.clone(),
            image: self.image_digest.clone(),
            path: p.path.clone().unwrap_or_default(),
            env: p.env.clone().into_iter().collect(),
            features: p.features.clone().unwrap_or_default(),
            target: p.target.clone().unwrap_or_default(),
            pre_commands: p.pre_commands.clone().unwrap_or_default(),
            tasks: p.tasks.clone().into_iter().collect(),
            timeout_secs: p.timeout_secs.unwrap_or(crate::DEFAULT_TASK_TIMEOUT_SECS),
            toolchain: self.toolchain.clone(),
            canonical: self.canonical(),
            source: self.source.as_str().to_string(),
        }
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn p(toml: &str) -> BuildProfile {
        parse_toml(toml).unwrap().profile
    }

    #[test]
    fn parses_the_documented_example() {
        let parsed = parse_toml(
            r#"
adapter = "rust"
image = "rc-registry/env/rust-protoc:a3f9"
path = "crates/backend"
env = { RUSTFLAGS = "-C target-cpu=native" }
features = ["ssr"]
target = "x86_64-unknown-linux-musl"
pre_commands = ["cargo run -p xtask codegen"]

[tasks]
check  = "cargo check --workspace --all-targets"
test   = "cargo nextest run -p backend"
clippy = "cargo clippy -- -D warnings"
"#,
        )
        .unwrap();
        assert_eq!(parsed.profile.adapter.as_deref(), Some("rust"));
        assert_eq!(parsed.profile.tasks.len(), 3);
        assert_eq!(parsed.profile.env["RUSTFLAGS"], "-C target-cpu=native");
        assert!(parsed.unknown_keys.is_empty());
    }

    #[test]
    fn typos_are_reported_not_swallowed() {
        let parsed = parse_toml("adaptor = \"rust\"\n").unwrap();
        assert_eq!(parsed.unknown_keys, vec!["adaptor"]);
    }

    #[test]
    fn higher_layers_win_but_maps_merge() {
        let (merged, source) = resolve(vec![
            (ProfileSource::Repo, p("image = \"repo:1\"\nenv = { A = \"1\" }")),
            (ProfileSource::Server, p("image = \"srv:1\"\nadapter = \"rust\"\nenv = { A = \"9\", B = \"2\" }")),
        ]);
        assert_eq!(merged.image.as_deref(), Some("repo:1"));
        assert_eq!(merged.adapter.as_deref(), Some("rust"));
        assert_eq!(merged.env["A"], "1"); // repo wins
        assert_eq!(merged.env["B"], "2"); // server fills the gap
        assert_eq!(source, ProfileSource::Repo);
    }

    #[test]
    fn empty_layers_do_not_claim_authorship() {
        let (_, source) = resolve(vec![
            (ProfileSource::Explicit, BuildProfile::default()),
            (ProfileSource::Server, p("adapter = \"rust\"")),
        ]);
        assert_eq!(source, ProfileSource::Server);
    }

    fn resolution(profile: BuildProfile, task: TaskType, cmd: &str) -> Resolution {
        Resolution {
            profile,
            source: ProfileSource::Repo,
            task_type: task,
            command: cmd.into(),
            adapter: "rust".into(),
            image_digest: "img@sha256:x".into(),
            toolchain: "rustc 1.85.0".into(),
        }
    }

    #[test]
    fn canonical_form_is_stable_across_map_insertion_order() {
        let mut a = BuildProfile::default();
        a.env.insert("B".into(), "2".into());
        a.env.insert("A".into(), "1".into());
        let mut b = BuildProfile::default();
        b.env.insert("A".into(), "1".into());
        b.env.insert("B".into(), "2".into());
        assert_eq!(
            resolution(a, TaskType::Check, "cargo check").canonical(),
            resolution(b, TaskType::Check, "cargo check").canonical()
        );
    }

    #[test]
    fn canonical_form_reacts_to_every_meaningful_field() {
        let base = resolution(BuildProfile::default(), TaskType::Check, "cargo check").canonical();

        let mut with_pre = BuildProfile::default();
        with_pre.pre_commands = Some(vec!["cargo run -p xtask codegen".into()]);
        assert_ne!(base, resolution(with_pre, TaskType::Check, "cargo check").canonical());

        let mut with_feat = BuildProfile::default();
        with_feat.features = Some(vec!["ssr".into()]);
        assert_ne!(base, resolution(with_feat, TaskType::Check, "cargo check").canonical());

        assert_ne!(base, resolution(BuildProfile::default(), TaskType::Clippy, "cargo clippy").canonical());
    }

    #[test]
    fn pre_command_order_matters() {
        let mut a = BuildProfile::default();
        a.pre_commands = Some(vec!["one".into(), "two".into()]);
        let mut b = BuildProfile::default();
        b.pre_commands = Some(vec!["two".into(), "one".into()]);
        assert_ne!(
            resolution(a, TaskType::Check, "c").canonical(),
            resolution(b, TaskType::Check, "c").canonical()
        );
    }

    #[test]
    fn toml_roundtrips() {
        let original = p("adapter = \"rust\"\nimage = \"x:1\"\nfeatures = [\"a\"]\n");
        let round = p(&to_toml(&original));
        assert_eq!(original, round);
    }
}
