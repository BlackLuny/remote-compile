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
    /// Paths that must not leave this machine, gitignore-style.
    ///
    /// `.gitignore` is the only other lever, and it cannot help with a file git
    /// already tracks — a key committed years ago is synced on every check
    /// (§4.3 deliberately syncs what git sees). This is how to keep one back.
    pub exclude: Option<Vec<String>>,
    /// Paths to sync even though `.gitignore` hides them, gitignore-style.
    ///
    /// The mirror image of `exclude`, and it exists because §4.3 takes git as
    /// the definition of what exists: a generated file the build `include!`s but
    /// `.gitignore` covers is in none of git's three lists, so it never travels,
    /// and the remote failure — a missing module — names nothing that appears in
    /// any diff. Only the repository may declare it: like `exclude`, this
    /// decides what leaves the machine.
    ///
    /// `exclude` wins where the two overlap, so withholding a file is never
    /// undone by a broader include.
    pub include: Option<Vec<String>>,
    /// Hosts the build needs to reach, beyond the fleet's default allowlist
    /// (§7.1) — an internal registry, a private git host.
    ///
    /// A request, never a grant. The sandbox has no route anywhere except the
    /// worker's proxy, and the proxy answers for a host only once an
    /// administrator has approved it for this project: an allowlist entry is a
    /// hole in the sandbox that §16 shows cannot be closed again from inside.
    /// Like `exclude` and `include`, only the repository's own file may ask.
    pub egress: Option<Vec<String>>,
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
    "exclude",
    "include",
    "egress",
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
        // `exclude`, `include`, `egress` and `extra_roots` are deliberately
        // absent: all four decide what leaves the developer's machine — or what
        // the sandbox may reach — and only the repository's own file may answer
        // that. Inheriting one from a fleet-learned profile would let one
        // project's stored config change another's disclosure.
        fill!(adapter, image, path, target, toolchain, timeout_secs, features, pre_commands);
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
        // Two builds that could reach different networks are not the same
        // build, so a cached verdict must not carry across a change here.
        // Sorted like `features`: reordering the lines in a config file does not
        // change what the build can reach, and should not cost a rebuild.
        let mut egress = p.egress.clone().unwrap_or_default();
        egress.sort();
        egress.dedup();
        push("egress", &egress.join(","));
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
    fn a_server_layer_cannot_supply_include_or_exclude() {
        // Both decide what leaves this machine. A fleet-learned value doing so
        // for a repository that never asked is the one direction this chain
        // must not have — see `overlay`.
        let (merged, _) = resolve(vec![
            (ProfileSource::Repo, p("adapter = \"rust\"")),
            (
                ProfileSource::Server,
                p("include = [\"*.generated.rs\"]\nexclude = [\"*.pem\"]"),
            ),
        ]);
        assert_eq!(merged.include, None);
        assert_eq!(merged.exclude, None);
    }

    #[test]
    fn include_is_a_known_key() {
        let parsed = parse_toml("include = [\"common/src/prisma.generated.rs\"]\n").unwrap();
        assert!(parsed.unknown_keys.is_empty(), "{:?}", parsed.unknown_keys);
        assert_eq!(
            parsed.profile.include.as_deref(),
            Some(&["common/src/prisma.generated.rs".to_string()][..])
        );
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
        // The control plane strips `pre_commands` off a learned profile by
        // clearing the field and re-serialising, so "cleared" has to actually
        // mean "absent from the toml" — if the key survived as an empty value
        // an unapproved script would still be shipped to every other agent.
        let mut stripped = BuildProfile {
            adapter: Some("rust".into()),
            pre_commands: Some(vec!["cargo run -p xtask codegen".into()]),
            ..Default::default()
        };
        assert!(to_toml(&stripped).contains("pre_commands"));
        stripped.pre_commands = None;
        let text = to_toml(&stripped);
        assert!(!text.contains("pre_commands"), "{text}");
        assert!(parse_toml(&text).unwrap().profile.pre_commands.is_none());

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
