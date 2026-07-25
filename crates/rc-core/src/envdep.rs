//! What the build machine was missing.
//!
//! Classifying a failure as `env_error` keeps the agent from editing working
//! source (§3.5, risk #4), but on its own it only says *the environment is
//! wrong* — not what to add. The evidence is almost always in the log:
//! `Package librrd was not found in the pkg-config search path`, `cannot find
//! -lfoo`, `fatal error: foo.h: No such file or directory`. Buried at line 3245
//! of 3259, where an agent can only reach it by paging through the log — which
//! is exactly the context spend this system exists to avoid (§11).
//!
//! So the evidence is lifted out here, at classification time, and travels in
//! the result.
//!
//! Two rules govern what may be claimed, because a wrong answer costs more than
//! no answer — acting on one means asking a human to approve a Docker image
//! (§8.3) that fixes nothing.
//!
//! 1. **The name is read from the log; the package is inferred.** They are
//!    reported differently, and an inferred package says so. The
//!    `<x>-sys → lib<x>-dev` convention holds for `rrd-sys` and `zstd-sys` and
//!    breaks for `openssl-sys → libssl-dev` and `alsa-sys → libasound2-dev`.
//! 2. **A guess needs a shape, not just an absence.** "Could not find X" is
//!    ordinary English that appears all over cargo's own output, so X is only
//!    believed when it is a name we know or is visibly a library (`lib…`).
//!    A blacklist of English words cannot make that rule safe; a whitelist of
//!    shapes can.

use crate::ansi;

/// Beyond this many distinct findings the log is pathological — a broken
/// sysroot can emit thousands of `cannot find -l…` lines — and dumping them all
/// into an agent's context recreates the problem this system exists to solve.
pub const MAX_FINDINGS: usize = 12;

/// How many are held before ranking. Larger than the reporting cap so that
/// ordering in the log does not decide what survives, bounded so the linear
/// scans over it stay cheap on a log emitting thousands of failures.
const COLLECT_LIMIT: usize = MAX_FINDINGS * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// A pkg-config module, or a library named by a build script's own probe.
    PkgConfig,
    /// A library the linker could not resolve (`-lfoo`).
    Library,
    /// A C/C++ header the compiler could not open.
    Header,
    /// An executable that was not on `PATH`.
    Program,
    /// A crate whose build script failed. Carries no package: a `-sys` build
    /// script fails for vendored-source, configuration and internal reasons at
    /// least as often as for a missing library.
    BuildScript,
}

/// How much the log actually established. A later, better-evidenced sighting of
/// the same name replaces an earlier weak one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Confidence {
    /// Only that some crate's build script failed.
    Crate,
    /// A "could not find X" whose X is a name we recognise or a `lib…`.
    Named,
    /// A tool said this exact thing was missing.
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDep {
    /// Exactly what the log named — `librrd`, `foo.h`, `protoc`.
    pub name: String,
    pub kind: DepKind,
    /// Debian package that probably provides it.
    pub package: Option<String>,
    /// True when `package` came from the known-mappings table rather than from
    /// the `lib<x>-dev` naming convention.
    pub certain: bool,
}

/// Packages providing a **library**: pkg-config modules, `-l` names, header
/// stems. Kept apart from programs because the same word means different
/// packages in each — `curl` the command is `curl`, `curl` the library is
/// `libcurl4-openssl-dev`.
const LIBRARY_PACKAGES: &[(&str, &str)] = &[
    ("openssl", "libssl-dev"),
    ("ssl", "libssl-dev"),
    ("crypto", "libssl-dev"),
    ("z", "zlib1g-dev"),
    ("zlib", "zlib1g-dev"),
    ("zlib1g", "zlib1g-dev"),
    ("alsa", "libasound2-dev"),
    ("asound", "libasound2-dev"),
    ("dbus-1", "libdbus-1-dev"),
    ("dbus", "libdbus-1-dev"),
    ("udev", "libudev-dev"),
    ("systemd", "libsystemd-dev"),
    ("curl", "libcurl4-openssl-dev"),
    ("pq", "libpq-dev"),
    ("postgresql", "libpq-dev"),
    // Bookworm ships no `libmysqlclient-dev`; the metapackage is the one that
    // resolves.
    ("mysqlclient", "default-libmysqlclient-dev"),
    ("sqlite3", "libsqlite3-dev"),
    ("ssh2", "libssh2-1-dev"),
    ("xml2", "libxml2-dev"),
    ("libxml", "libxml2-dev"),
    ("ffi", "libffi-dev"),
    ("clang", "libclang-dev"),
    ("libclang", "libclang-dev"),
    ("glib-2.0", "libglib2.0-dev"),
    ("freetype2", "libfreetype6-dev"),
    ("freetype", "libfreetype6-dev"),
    ("fontconfig", "libfontconfig1-dev"),
    ("x11", "libx11-dev"),
    ("xcb", "libxcb1-dev"),
    ("gtk+-3.0", "libgtk-3-dev"),
    ("gtk-3.0", "libgtk-3-dev"),
    ("protobuf", "libprotobuf-dev"),
    ("stdc++", "build-essential"),
    // Headers, whose package is the -dev of an interpreter rather than of a
    // library with a matching name.
    ("python", "python3-dev"),
    ("python3", "python3-dev"),
];

/// Packages providing an **executable**.
const PROGRAM_PACKAGES: &[(&str, &str)] = &[
    ("protoc", "protobuf-compiler"),
    ("cmake", "cmake"),
    ("pkg-config", "pkg-config"),
    ("pkgconf", "pkg-config"),
    ("cc", "build-essential"),
    ("gcc", "build-essential"),
    ("g++", "build-essential"),
    ("ld", "build-essential"),
    ("make", "make"),
    ("ninja", "ninja-build"),
    ("nasm", "nasm"),
    ("yasm", "yasm"),
    ("perl", "perl"),
    // Unversioned `/usr/bin/python` is the compatibility symlink, which lives
    // in `python-is-python3` — installing `python3` alone leaves the command
    // still missing.
    ("python", "python-is-python3"),
    ("python3", "python3"),
    ("go", "golang"),
    ("git", "git"),
    ("curl", "curl"),
    ("clang", "clang"),
    ("autoconf", "autoconf"),
    ("automake", "automake"),
    // Debian splits the binary out of `libtool` into `libtool-bin`.
    ("libtool", "libtool-bin"),
    ("unzip", "unzip"),
    ("node", "nodejs"),
];

fn lookup(table: &[(&str, &str)], key: &str) -> Option<String> {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, pkg)| (*pkg).to_string())
}

/// `librrd` → `librrd-dev`, `glib-2.0` → `libglib2.0-dev`.
///
/// Debian drops the separator before a version suffix, which is why the second
/// case is not `libglib-2.0-dev`.
fn conventional_package(stem: &str) -> Option<String> {
    let bare = stem.strip_prefix("lib").unwrap_or(stem);
    if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c)) {
        return None;
    }
    if let Some(idx) = bare.rfind('-') {
        if bare[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            return Some(format!("lib{}{}-dev", &bare[..idx], &bare[idx + 1..]));
        }
    }
    Some(format!("lib{bare}-dev"))
}

/// Debian package that probably ships `name`, and whether we actually know.
fn package_for(name: &str, kind: DepKind) -> (Option<String>, bool) {
    let lower = name.trim().to_lowercase();

    // A crate name says nothing about a package. `aws-lc-sys` fails to build
    // for reasons that have no `libaws-lc-dev` to install.
    if kind == DepKind::BuildScript {
        return (None, false);
    }

    if kind == DepKind::Program {
        // No naming convention links a binary to the package shipping it, so
        // an unrecognised program gets no guess at all — `libprotoc-dev` would
        // be worse than saying nothing.
        return match lookup(PROGRAM_PACKAGES, &lower) {
            Some(p) => (Some(p), true),
            None => (None, false),
        };
    }

    if kind == DepKind::Header {
        // `openssl/ssl.h` is identified by its directory far more reliably than
        // by `ssl`; for a bare `sodium.h` the stem is all there is.
        let (dir, file) = match lower.rsplit_once('/') {
            Some((d, f)) => (d.rsplit('/').next().unwrap_or(d).to_string(), f.to_string()),
            None => (String::new(), lower.clone()),
        };
        let stem = file.trim_end_matches(".hpp").trim_end_matches(".h").to_string();
        for probe in [dir.as_str(), stem.as_str()] {
            if probe.is_empty() {
                continue;
            }
            if let Some(pkg) = lookup(LIBRARY_PACKAGES, probe) {
                return (Some(pkg), true);
            }
        }
        let basis = if dir.is_empty() { stem } else { dir };
        return (conventional_package(&basis), false);
    }

    for probe in [lower.as_str(), lower.strip_prefix("lib").unwrap_or(&lower)] {
        if !probe.is_empty() {
            if let Some(pkg) = lookup(LIBRARY_PACKAGES, probe) {
                return (Some(pkg), true);
            }
        }
    }
    (conventional_package(&lower), false)
}

/// Whether a "could not find X" is about a dependency at all.
///
/// This rule works from an *absence*, so it needs the name itself to carry
/// evidence. A recognised name qualifies; so does anything visibly a library.
/// Everything else — `directory`, `repository`, `Cargo.toml` — is prose, and
/// no list of English words could be relied on to enumerate it.
fn prose_kind(name: &str) -> Option<DepKind> {
    let lower = name.to_lowercase();
    // `unable to find library -lfoo` would otherwise offer `liblibrary-dev`.
    if matches!(lower.as_str(), "library" | "libraries" | "lib") {
        return None;
    }
    if lookup(PROGRAM_PACKAGES, &lower).is_some() {
        return Some(DepKind::Program);
    }
    // A recognised name, or one visibly a library — but `lib.rs` is a source
    // file that satisfies `starts_with("lib")` and would yield `lib.rs-dev`.
    const SOURCE_SUFFIXES: &[&str] = &[
        ".rs", ".toml", ".lock", ".json", ".yaml", ".yml", ".md", ".txt", ".cfg", ".ini", ".c",
        ".cc", ".cpp", ".py", ".sh", ".pc", ".cmake", ".m4", ".mk",
    ];
    if SOURCE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return None;
    }
    if lookup(LIBRARY_PACKAGES, &lower).is_some() || lower.starts_with("lib") {
        return Some(DepKind::PkgConfig);
    }
    None
}

/// What a finding is *about*, for deciding whether two of them are one fact.
/// A pkg-config module, a `-l` name and a header all describe a missing
/// library; a program is a separate thing that needs a separate package.
fn class(kind: DepKind) -> u8 {
    match kind {
        DepKind::PkgConfig | DepKind::Library | DepKind::Header => 0,
        DepKind::Program => 1,
        DepKind::BuildScript => 2,
    }
}

/// How useful a finding is to act on: a known package beats a guessed one,
/// and a guessed one beats none.
fn quality(d: &MissingDep) -> u8 {
    match (&d.package, d.certain) {
        (Some(_), true) => 2,
        (Some(_), false) => 1,
        (None, _) => 0,
    }
}

struct Collector {
    found: Vec<(MissingDep, Confidence)>,
}

impl Collector {
    fn push(&mut self, name: &str, kind: DepKind, confidence: Confidence) {
        let name = name.trim().trim_matches(['`', '\'', '"', ',', '.']).to_string();
        if name.is_empty() || name.len() > 64 {
            return;
        }
        let (package, certain) = package_for(&name, kind);
        let dep = MissingDep { name, kind, package, certain };

        // The same fact seen twice: keep the better-evidenced telling. Sameness
        // is by *class*, not by kind — a pkg-config module and an unresolved
        // `-l` of the same name are one missing library reported by two tools,
        // and listing both would spend the finding budget twice on it.
        if let Some(slot) = self
            .found
            .iter_mut()
            .find(|(d, _)| class(d.kind) == class(dep.kind) && d.name.eq_ignore_ascii_case(&dep.name))
        {
            // Equally-evidenced tellings of one fact are broken apart by which
            // resolves to a real package: `Python.h` read as a header maps to
            // python3-dev, read as a pkg-config module it invents
            // `libpython.h-dev`. Whichever arrives first must not decide that.
            if confidence > slot.1 || (confidence == slot.1 && quality(&dep) > quality(&slot.0)) {
                *slot = (dep, confidence);
            }
            return;
        }

        // Same name, different kind. Better evidence wins outright — `Could not
        // find ninja` followed by ``Is `ninja` installed?`` must end up as the
        // program, not as a guessed `libninja-dev`. Equal evidence means two
        // independent facts, and both are kept: `No package 'curl' found` and
        // `curl: command not found` are a missing library *and* a missing
        // command, needing different packages.
        if let Some(pos) = self.found.iter().position(|(d, _)| d.name.eq_ignore_ascii_case(&dep.name)) {
            match confidence.cmp(&self.found[pos].1) {
                std::cmp::Ordering::Greater => {
                    self.found[pos] = (dep, confidence);
                    return;
                }
                std::cmp::Ordering::Less => return,
                std::cmp::Ordering::Equal => {}
            }
        }

        // Collected above the reporting cap, then ranked. When the buffer is
        // full a stronger finding evicts the weakest one held, so no number of
        // vague sightings can lock out the line that actually names a library.
        if self.found.len() < COLLECT_LIMIT {
            self.found.push((dep, confidence));
            return;
        }
        let weakest = self
            .found
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, c))| *c)
            .map(|(i, (_, c))| (i, *c));
        if let Some((i, weakest_conf)) = weakest {
            if confidence > weakest_conf {
                self.found[i] = (dep, confidence);
            }
        }
    }

    /// Best-evidenced first, log order preserved within a tier.
    fn ranked(mut self) -> Vec<MissingDep> {
        self.found.sort_by(|a, b| b.1.cmp(&a.1));
        self.found.truncate(MAX_FINDINGS);
        self.found.into_iter().map(|(d, _)| d).collect()
    }
}

/// Pull every nameable missing dependency out of a build log. Capped at
/// [`MAX_FINDINGS`].
pub fn analyze(log: &str) -> Vec<MissingDep> {
    analyze_parts(&[log])
}

/// The same, over several sources at once — the raw stream and the text of the
/// parsed diagnostics, say. Ranking and de-duplication have to see all of it
/// together, so this is not the same as analysing each and concatenating.
pub fn analyze_parts(parts: &[&str]) -> Vec<MissingDep> {
    // Iterated rather than joined: a build log runs to tens of megabytes, and
    // concatenating would copy the whole thing a second time to no purpose.
    let cleaned: Vec<String> = parts.iter().map(|p| ansi::strip(p)).collect();
    let mut c = Collector { found: Vec::new() };

    // Compiled once; the log can be megabytes and these run per line.
    let re_pkg_notfound =
        regex::Regex::new(r"(?i)package '?([A-Za-z0-9_.+-]+)'? was not found in the pkg-config")
            .expect("static regex");
    let re_no_package = regex::Regex::new(r"No package '([^']+)' found").expect("static regex");
    let re_syslib = regex::Regex::new(r"(?i)system library [`']([^`']+)[`'] required by crate")
        .expect("static regex");
    let re_syslib_old =
        regex::Regex::new(r"could not find system library '([^']+)'").expect("static regex");
    let re_dash_l = regex::Regex::new(r"(?:cannot|unable to) find (?:library )?-l([A-Za-z0-9_.+-]+)")
        .expect("static regex");
    // rustc when a `#[link(name = "…")]` cannot be resolved — the same missing
    // library as `cannot find -lfoo`, reported by the compiler instead of ld.
    let re_native_lib =
        regex::Regex::new(r"could not find native static library [`']?([A-Za-z0-9_.+-]+)")
            .expect("static regex");
    // The `fatal error: ` prefix is deliberately not required: once the line
    // has been parsed into a diagnostic the prefix is gone, and the message
    // alone — `openssl/ssl.h: No such file or directory` — is what remains.
    let re_header = regex::Regex::new(
        r"(?:^|[\s:])([A-Za-z0-9_./+-]+\.h(?:pp)?): No such file|'([A-Za-z0-9_./+-]+\.h(?:pp)?)' file not found",
    )
    .expect("static regex");
    // `bash: foo: command not found`. The name must not be a bare line number:
    // zsh writes `zsh:1: command not found: foo`, where the token before the
    // colon is the line, not the program.
    let re_cmd_notfound =
        regex::Regex::new(r"(?:^|[:\s])([A-Za-z0-9_.+-]*[A-Za-z_.+-][A-Za-z0-9_.+-]*): command not found")
            .expect("static regex");
    // …and zsh's own form, which puts the name last.
    let re_zsh_notfound =
        regex::Regex::new(r"command not found: ([A-Za-z0-9_.+-]+)").expect("static regex");
    // Debian's `/bin/sh` is dash, which drops the word "command". `X: not
    // found` on its own is far too common in prose and in ordinary file
    // errors — `CMakeLists.txt: not found` is not a missing program — so this
    // form is only believed with a shell's own prefix in front of it:
    // `/bin/sh: 1: x: not found`, `sh: line 3: x: not found`, `/bin/ksh: x: not found`.
    let re_sh_notfound = regex::Regex::new(
        r"(?:^|/)(?:ba|z|da|k|mk|a)?sh: (?:(?:line )?\d+: )?([A-Za-z0-9_.+-]+): not found",
    )
    .expect("static regex");
    let re_is_installed = regex::Regex::new(r"Is [`']([^`']+)[`'] installed").expect("static regex");
    // cc: `failed to find tool "aarch64-linux-gnu-gcc": …`
    let re_failed_tool =
        regex::Regex::new(r#"failed to find tool "([^"]+)""#).expect("static regex");
    let re_linker = regex::Regex::new(r"linker [`']([^`']+)['`] not found").expect("static regex");
    // Prose. Each of these needs `prose_kind` to vouch for the name.
    let re_could_not_find =
        regex::Regex::new(r"[Cc]ould not find ([A-Za-z][A-Za-z0-9_.+-]{2,})\s*$").expect("static regex");
    let re_could_not_find_quoted =
        regex::Regex::new(r"[Cc]ould not find [`']([A-Za-z0-9_.+-]{2,})[`']").expect("static regex");
    // CMake's `find_package` failure, verbatim capitalisation.
    let re_cmake = regex::Regex::new(r"Could NOT find ([A-Za-z0-9_+-]{2,})").expect("static regex");
    // bindgen: `Unable to find libclang: …`
    let re_unable_to_find =
        regex::Regex::new(r"[Uu]nable to find ([A-Za-z][A-Za-z0-9_.+-]{2,})").expect("static regex");
    let re_build_cmd =
        regex::Regex::new(r"failed to run custom build command for `([^`]+)`").expect("static regex");
    let re_pkg_probe_args =
        regex::Regex::new(r#""pkg[-_]config"((?:\s+"[^"]*")+)"#).expect("static regex");

    let mut failing_crates: Vec<String> = Vec::new();
    // When pkg-config itself is absent every probe fails, and the modules those
    // probes named are not evidence of anything. The tool is the finding.
    let mut pkg_config_absent = false;

    for line in cleaned.iter().flat_map(|c| c.lines()) {
        let line = line.trim_end();

        // `--message-format=json` output shares the stream with human text, and
        // those lines are dense with package names, feature lists and paths:
        // a real log carries `"features":[…,"pkg-config","vcpkg"]`, one space
        // away from looking like a failing probe invocation. Every signal worth
        // having also appears in the human half, so skip the machine half
        // entirely — the same rule `parse_cargo_json` uses, inverted.
        if line.trim_start().starts_with('{') {
            continue;
        }

        // Only genuine absence. `pkg-config has not been configured to support
        // cross-compilation` is pkg-config 0.3's `CrossCompilation` error —
        // the binary is right there, and `apt-get install pkg-config` fixes
        // nothing while suppressing the module names that were real evidence.
        if line.contains("pkg-config command could not be found") {
            pkg_config_absent = true;
        }

        // ---- pkg-config ----
        if line.contains("pkg-config") || line.contains("pkg_config") {
            if let Some(m) = re_pkg_notfound.captures(line) {
                c.push(&m[1], DepKind::PkgConfig, Confidence::Explicit);
            }
            // The failing invocation itself names the module as its last
            // non-flag argument: `"pkg-config" "--libs" "--cflags" "librrd"`.
            if line.contains("did not exit successfully") || line.contains("exit status") {
                if let Some(m) = re_pkg_probe_args.captures(line) {
                    if let Some(arg) =
                        m[1].split('"').map(str::trim).rfind(|a| !a.is_empty() && !a.starts_with('-'))
                    {
                        c.push(arg, DepKind::PkgConfig, Confidence::Explicit);
                    }
                }
            }
        }
        for m in [re_no_package.captures(line), re_syslib.captures(line), re_syslib_old.captures(line)]
            .into_iter()
            .flatten()
        {
            c.push(&m[1], DepKind::PkgConfig, Confidence::Explicit);
        }

        // ---- linker ----
        for m in [re_dash_l.captures(line), re_native_lib.captures(line)].into_iter().flatten() {
            c.push(&m[1], DepKind::Library, Confidence::Explicit);
        }

        // ---- headers ----
        if let Some(m) = re_header.captures(line) {
            let name = m.get(1).or_else(|| m.get(2)).map(|x| x.as_str()).unwrap_or("");
            c.push(name, DepKind::Header, Confidence::Explicit);
        }

        // ---- programs ----
        for (m, definite) in [
            (re_cmd_notfound.captures(line), true),
            (re_zsh_notfound.captures(line), true),
            (re_sh_notfound.captures(line), true),
            (re_is_installed.captures(line), false),
            (re_failed_tool.captures(line), false),
            (re_linker.captures(line), false),
        ] {
            let Some(m) = m else { continue };
            c.push(&m[1], DepKind::Program, Confidence::Explicit);
            // Only a shell reporting the command missing establishes that
            // pkg-config is *absent*. ``Is `pkg-config` installed?`` is a build
            // script guessing, and CMake's `Could NOT find PkgConfig` is often
            // a version complaint about a pkg-config that is right there —
            // neither justifies deleting the module names below.
            if definite && matches!(&m[1], "pkg-config" | "pkgconf") {
                pkg_config_absent = true;
            }
        }

        // CMake names a *capability*, which is not always the thing to install.
        // `find_package(PkgConfig)` failing means the `pkg-config` executable is
        // absent — but `pkgconfig` is not itself a command, so this translation
        // belongs here and not in the executable table.
        if let Some(m) = re_cmake.captures(line) {
            if m[1].eq_ignore_ascii_case("pkgconfig") {
                c.push("pkg-config", DepKind::Program, Confidence::Named);
            }
        }

        // ---- prose, only for names that vouch for themselves ----
        for m in [
            re_could_not_find.captures(line),
            re_could_not_find_quoted.captures(line),
            re_cmake.captures(line),
            re_unable_to_find.captures(line),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(kind) = prose_kind(&m[1]) {
                c.push(&m[1], kind, Confidence::Named);
            }
        }

        if let Some(m) = re_build_cmd.captures(line) {
            let name = m[1].to_string();
            if !failing_crates.contains(&name) {
                failing_crates.push(name);
            }
        }
    }

    // Every crate whose build script failed is a fact worth reporting, and it
    // is reported whether or not something more specific was also found — the
    // alternative drops the second failing crate as soon as the first one
    // yields a library name.
    for krate in &failing_crates {
        c.push(krate, DepKind::BuildScript, Confidence::Crate);
    }

    let mut deps = c.ranked();
    if pkg_config_absent {
        // Keep only what does not depend on a working pkg-config: with the tool
        // absent every probe fails, so the modules those probes named are not
        // evidence that the modules themselves are missing.
        deps.retain(|d| d.kind != DepKind::PkgConfig && !d.name.eq_ignore_ascii_case("pkg-config"));
        deps.insert(
            0,
            MissingDep {
                name: "pkg-config".into(),
                kind: DepKind::Program,
                package: Some("pkg-config".into()),
                certain: true,
            },
        );
        deps.truncate(MAX_FINDINGS);
    }
    deps
}

/// One line per finding, plus an install line when anything can be named.
/// These go straight into the agent's context, so they say what was observed
/// and mark every guess as one.
pub fn hint_lines(deps: &[MissingDep]) -> Vec<String> {
    if deps.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["构建日志显示环境缺少以下依赖:".to_string()];
    for d in deps {
        let what = match d.kind {
            DepKind::PkgConfig => format!("pkg-config 模块 `{}` 未找到", d.name),
            DepKind::Library => format!("链接器找不到库 -l{}", d.name),
            DepKind::Header => format!("缺少头文件 {}", d.name),
            DepKind::Program => format!("可执行文件 `{}` 不在 PATH 中", d.name),
            DepKind::BuildScript => {
                format!("crate `{}` 的构建脚本失败（原因未必是缺依赖）", d.name)
            }
        };
        let suffix = match (&d.package, d.certain) {
            (Some(p), true) => format!(" → 需要 {p}"),
            (Some(p), false) => format!(" → 可能是 {p}（按命名惯例推测，需核实）"),
            (None, _) => String::new(),
        };
        lines.push(format!("  - {what}{suffix}"));
    }
    if deps.len() >= MAX_FINDINGS {
        lines.push(format!("  （仅列出前 {MAX_FINDINGS} 条，用 get_log 查看完整日志）"));
    }
    // Two findings often name one package — `cannot find -lsodium` and
    // `sodium.h: No such file` are the same missing dev package. `dedup` only
    // collapses neighbours, and they are not always neighbours.
    let mut packages: Vec<&str> = Vec::new();
    for p in deps.iter().filter_map(|d| d.package.as_deref()) {
        if !packages.contains(&p) {
            packages.push(p);
        }
    }
    if !packages.is_empty() {
        lines.push(format!("  安装建议: apt-get install -y {}", packages.join(" ")));
        // Only warn about guesses when there is one to warn about; saying it
        // unconditionally trains the reader to ignore it.
        let guessed = deps.iter().any(|d| d.package.is_some() && !d.certain);
        lines.push(if guessed {
            "  用 prepare_env 提交带这些包的 Dockerfile；标注为推测的包名请先核实。".into()
        } else {
            "  用 prepare_env 提交带这些包的 Dockerfile。".to_string()
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(log: &str) -> Vec<String> {
        analyze(log).into_iter().map(|d| d.name).collect()
    }

    fn find<'a>(deps: &'a [MissingDep], name: &str) -> &'a MissingDep {
        deps.iter().find(|d| d.name == name).unwrap_or_else(|| panic!("no `{name}` in {deps:?}"))
    }

    #[test]
    fn extracts_the_library_a_build_script_could_not_find() {
        // The failure that prompted all of this: rrd-sys probes for librrd,
        // does not find it, and panics. The name is in the log; nothing was
        // carrying it out.
        let log = "\
   Compiling rrd-sys v0.1.3
error: failed to run custom build command for `rrd-sys v0.1.3`

Caused by:
  process didn't exit successfully: `/work/target/debug/build/rrd-sys-1/build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at build.rs:37:9:
  Could not find librrd
";
        let deps = analyze(log);
        let rrd = find(&deps, "librrd");
        assert_eq!(rrd.kind, DepKind::PkgConfig);
        assert_eq!(rrd.package.as_deref(), Some("librrd-dev"));
        assert!(!rrd.certain, "the package name is a convention guess");
    }

    #[test]
    fn reads_the_standard_pkg_config_failure() {
        let log = "Package libssh2 was not found in the pkg-config search path.\n\
                   Perhaps you should add the directory containing `libssh2.pc'";
        let deps = analyze(log);
        assert_eq!(deps[0].name, "libssh2");
        assert_eq!(deps[0].package.as_deref(), Some("libssh2-1-dev"));
        assert!(deps[0].certain);
    }

    #[test]
    fn reads_the_module_out_of_a_failing_probe_invocation() {
        let log = r#"  run pkg_config fail: `PKG_CONFIG_ALLOW_SYSTEM_CFLAGS="1" "pkg-config" "--libs" "--cflags" "libudev"` did not exit successfully"#;
        assert_eq!(analyze(log)[0].package.as_deref(), Some("libudev-dev"));
    }

    #[test]
    fn a_missing_pkg_config_is_reported_instead_of_the_module_it_could_not_probe() {
        // Every probe fails when the tool is absent, so the module names those
        // probes carried prove nothing. Installing `libudev-dev` here fixes
        // nothing and costs a human image approval (§8.3).
        let log = "\
error: failed to run custom build command for `libudev-sys v0.1.4`
  Could not run `pkg-config --libs --cflags libudev`
  The pkg-config command could not be found.
";
        let deps = analyze(log);
        assert_eq!(deps[0].name, "pkg-config");
        assert_eq!(deps[0].package.as_deref(), Some("pkg-config"));
        assert!(deps[0].certain);
        assert!(!deps.iter().any(|d| d.name == "libudev"), "{deps:?}");

        // And it is named once even when the log also says so directly.
        let both = analyze(
            "sh: 1: pkg-config: command not found\nThe pkg-config command could not be found.",
        );
        assert_eq!(both.iter().filter(|d| d.name == "pkg-config").count(), 1, "{both:?}");
    }

    #[test]
    fn reads_linker_and_header_failures() {
        let log = "/usr/bin/ld: cannot find -lsodium: No such file or directory\n\
                   src/wrap.c:3:10: fatal error: sodium.h: No such file or directory";
        let deps = analyze(log);
        assert_eq!(deps[0].kind, DepKind::Library);
        assert_eq!(deps[0].package.as_deref(), Some("libsodium-dev"));
        assert_eq!(deps[1].name, "sodium.h");
        assert_eq!(deps[1].package.as_deref(), Some("libsodium-dev"));
    }

    #[test]
    fn a_header_is_identified_by_its_directory_when_it_has_one() {
        // `openssl/ssl.h` is openssl; the stem `ssl` alone is far weaker, and
        // `libxml/parser.h` reduced to `parser` is meaningless.
        assert_eq!(
            analyze("wrapper.c:3:10: fatal error: openssl/ssl.h: No such file or directory")[0]
                .package
                .as_deref(),
            Some("libssl-dev")
        );
        assert_eq!(
            analyze("x.c:1:1: fatal error: libxml/parser.h: No such file or directory")[0]
                .package
                .as_deref(),
            Some("libxml2-dev")
        );
    }

    #[test]
    fn the_same_word_maps_differently_for_a_program_and_a_library() {
        // Debian ships the `curl` command in `curl` and the library in
        // `libcurl4-openssl-dev`; one table for both would get one of them
        // wrong while claiming to be certain.
        assert_eq!(
            analyze("sh: 1: curl: command not found")[0].package.as_deref(),
            Some("curl")
        );
        assert_eq!(
            analyze("No package 'curl' found")[0].package.as_deref(),
            Some("libcurl4-openssl-dev")
        );
        // And `Python.h` comes from python3-dev, not from python3.
        assert_eq!(
            analyze("w.c:1:1: fatal error: Python.h: No such file or directory")[0]
                .package
                .as_deref(),
            Some("python3-dev")
        );
    }

    #[test]
    fn a_versioned_module_follows_debians_spelling() {
        // Debian drops the separator before the version: libglib2.0-dev, not
        // libglib-2.0-dev, which does not exist.
        assert_eq!(
            analyze("No package 'glib-2.0' found")[0].package.as_deref(),
            Some("libglib2.0-dev")
        );
    }

    #[test]
    fn a_missing_program_gets_no_invented_package() {
        let deps = analyze("sh: 1: some-vendor-tool: command not found");
        assert_eq!(deps[0].kind, DepKind::Program);
        assert_eq!(deps[0].package, None);
    }

    #[test]
    fn known_mappings_win_over_the_naming_convention() {
        for (module, pkg) in [
            ("openssl", "libssl-dev"),
            ("alsa", "libasound2-dev"),
            ("z", "zlib1g-dev"),
            ("dbus-1", "libdbus-1-dev"),
        ] {
            let deps = analyze(&format!("No package '{module}' found"));
            assert_eq!(deps[0].package.as_deref(), Some(pkg), "{module}");
            assert!(deps[0].certain, "{module}");
        }
    }

    #[test]
    fn recognises_the_other_common_probe_failures() {
        for (log, want, pkg) in [
            ("Could not find `protoc`. If protoc is available via PATH…", "protoc", "protobuf-compiler"),
            ("CMake Error at CMakeLists.txt:4 (find_package):\n  Could NOT find OpenSSL (missing: OPENSSL_LIBRARIES)", "OpenSSL", "libssl-dev"),
            ("thread 'main' panicked: Unable to find libclang: \"couldn't find any valid shared libraries\"", "libclang", "libclang-dev"),
            ("error: linker `cc` not found", "cc", "build-essential"),
            (r#"failed to find tool "aarch64-linux-gnu-gcc": No such file or directory (os error 2)"#, "aarch64-linux-gnu-gcc", ""),
            ("The system library `libfoo` required by crate `my-build-script` was not found.", "libfoo", "libfoo-dev"),
            ("ld.lld: error: unable to find library -lfoo", "foo", "libfoo-dev"),
        ] {
            let deps = analyze(log);
            let d = find(&deps, want);
            if pkg.is_empty() {
                assert_eq!(d.package, None, "{log}");
            } else {
                assert_eq!(d.package.as_deref(), Some(pkg), "{log}");
            }
        }
    }

    #[test]
    fn a_prose_match_needs_the_name_to_vouch_for_itself() {
        // This rule reasons from an absence, so the name has to carry the
        // evidence: recognised, or visibly a library. No blacklist of English
        // words could enumerate what else "could not find X" says.
        for noise in [
            "error: could not find `Cargo.toml` in `/work`",
            "could not find the requested Thing In A Sentence",
            "Could not find repository",
            "could not find directory",
            "Could NOT find Threads (missing: Threads_FOUND)",
            // `starts_with("lib")` is a shape check, not a proof: a source file
            // satisfies it and would be offered as `lib.rs-dev`.
            "Could not find `lib.rs`",
            "Could not find `libfoo.toml`",
            "Could not find libfooConfig.cmake",
        ] {
            assert!(names(noise).is_empty(), "`{noise}` must not name a dependency");
        }
    }

    #[test]
    fn cmakes_spelling_of_pkg_config_is_recognised() {
        // `find_package(PkgConfig)` failing means the executable is absent —
        // treating it as prose noise loses the one thing the log established.
        let deps = analyze("Could NOT find PkgConfig (missing: PKG_CONFIG_EXECUTABLE)");
        assert_eq!(deps[0].name, "pkg-config");
        assert_eq!(deps[0].kind, DepKind::Program);
        assert_eq!(deps[0].package.as_deref(), Some("pkg-config"));

        // …but only as CMake's capability name. There is no `pkgconfig`
        // command on Debian, so a shell reporting one missing is not fixed by
        // installing pkg-config, and must not be told that it is.
        let deps = analyze("sh: 1: pkgconfig: command not found");
        assert_eq!(deps[0].name, "pkgconfig");
        assert_eq!(deps[0].package, None, "no command by that name exists to install");
    }

    #[test]
    fn dash_reports_a_missing_command_differently_from_bash() {
        // Debian's /bin/sh is dash: `pkg-config: not found`, no "command".
        // Each of the shells a build might actually run under words this
        // differently, and zsh puts the name *after* the message — reading its
        // line number as the program name reported a missing `1`.
        for log in [
            "/bin/sh: 1: pkg-config: not found",
            "sh: line 3: pkg-config: not found",
            "bash: line 12: pkg-config: command not found",
            "zsh:1: command not found: pkg-config",
            "/bin/ksh: pkg-config: not found",
        ] {
            let deps = analyze(log);
            assert_eq!(deps[0].name, "pkg-config", "{log}");
            assert_eq!(deps[0].kind, DepKind::Program, "{log}");
            assert_eq!(deps[0].package.as_deref(), Some("pkg-config"), "{log}");
        }
    }

    #[test]
    fn a_missing_pkg_config_suppresses_modules_whatever_shell_reported_it() {
        // The module names carried by probes that could not run are not
        // evidence; recommending libssl-dev here fixes nothing.
        for prefix in ["zsh:1: command not found: pkg-config", "/bin/ksh: pkg-config: not found"] {
            let deps = analyze(&format!("{prefix}\nNo package 'openssl' found"));
            assert_eq!(deps[0].name, "pkg-config", "{prefix}");
            assert!(!deps.iter().any(|d| d.name == "openssl"), "{prefix}: {deps:?}");
        }
    }

    #[test]
    fn bare_not_found_needs_a_shell_in_front_of_it() {
        // `X: not found` without a shell prefix is prose or an ordinary file
        // error, and reporting a missing *program* for it is noise.
        assert!(names("CMakeLists.txt: not found").is_empty());
        assert!(names("error: package foo: not found in the dependency index").is_empty());
    }

    #[test]
    fn an_unsuitable_pkg_config_version_does_not_delete_the_real_findings() {
        // CMake reports a version complaint with the same "Could NOT find"
        // wording while telling you exactly where the binary is. Treating that
        // as absence both misadvises and throws away the module that is the
        // actual problem.
        let log = "\
Could NOT find PkgConfig: Found unsuitable version \"0.29.2\", but required is at least \"99\" (found /usr/bin/pkg-config)
No package 'libssl' found";
        let deps = analyze(log);
        assert!(deps.iter().any(|d| d.name == "libssl"), "{deps:?}");
    }

    #[test]
    fn the_reading_that_resolves_to_a_real_package_wins_a_tie() {
        // Both are Explicit and describe one missing library. As a header
        // `Python.h` maps to python3-dev; as a pkg-config module it invents
        // `libpython.h-dev`. Log order must not decide which the agent gets.
        let header_first = analyze(
            "wrapper.c:1:1: fatal error: Python.h: No such file or directory\nNo package 'Python.h' found",
        );
        let module_first = analyze(
            "No package 'Python.h' found\nwrapper.c:1:1: fatal error: Python.h: No such file or directory",
        );
        for deps in [header_first, module_first] {
            assert_eq!(deps.len(), 1, "{deps:?}");
            assert_eq!(deps[0].package.as_deref(), Some("python3-dev"), "{deps:?}");
        }
    }

    #[test]
    fn a_library_named_by_two_tools_is_one_finding_not_two() {
        // pkg-config and ld both report the same missing library. Counting it
        // twice spends the finding budget twice and can crowd out the one
        // library that is genuinely different.
        let mut log = String::new();
        for name in ["liba", "libb", "libc1", "libd", "libe", "libf"] {
            log.push_str(&format!("No package '{name}' found\nld: cannot find -l{name}\n"));
        }
        log.push_str("ld: cannot find -lssl\n");
        let deps = analyze(&log);
        assert_eq!(deps.iter().filter(|d| d.name == "liba").count(), 1, "{deps:?}");
        assert!(deps.iter().any(|d| d.name == "ssl"), "crowded out: {deps:?}");
    }

    #[test]
    fn a_cross_compilation_error_is_not_a_missing_pkg_config() {
        // pkg-config 0.3's `CrossCompilation` error: the binary is present.
        // Reporting it as absent both fixes nothing and deletes the module
        // names that were the real evidence.
        let log = "\
No package 'libudev' found
pkg-config has not been configured to support cross-compilation.
Install a sysroot for the target platform and configure it via
PKG_CONFIG_SYSROOT_DIR and PKG_CONFIG_PATH";
        let deps = analyze(log);
        assert!(deps.iter().any(|d| d.name == "libudev"), "{deps:?}");
        assert!(!deps.iter().any(|d| d.name == "pkg-config"), "{deps:?}");
    }

    #[test]
    fn one_name_can_be_two_independent_facts() {
        // `curl` the command and `curl` the library are separately missing and
        // need different packages; keying dedup on the name alone lost one.
        let deps = analyze("No package 'curl' found\nsh: 1: curl: command not found");
        assert_eq!(
            find(&deps, "curl").package.as_deref(),
            Some("libcurl4-openssl-dev"),
            "{deps:?}"
        );
        let packages: Vec<&str> = deps.iter().filter_map(|d| d.package.as_deref()).collect();
        assert!(packages.contains(&"curl"), "the command is also missing: {deps:?}");
    }

    #[test]
    fn stronger_evidence_replaces_weaker_for_the_same_name() {
        // `ninja` first appears in prose and then explicitly as a program. The
        // program reading is right, and arriving second must not lose to it.
        let deps = analyze("Could not find ninja\nIs `ninja` installed?");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].kind, DepKind::Program);
        assert_eq!(deps[0].package.as_deref(), Some("ninja-build"));
    }

    #[test]
    fn a_failing_crate_is_named_but_never_turned_into_a_package() {
        // A `-sys` build script fails for vendored-source, configuration and
        // internal reasons at least as often as for a missing library;
        // `libaws-lc-dev` does not exist and would waste an image approval.
        let deps = analyze("error: failed to run custom build command for `aws-lc-sys v0.30.0`");
        assert_eq!(deps[0].kind, DepKind::BuildScript);
        assert_eq!(deps[0].package, None);
        assert!(hint_lines(&deps).iter().all(|l| !l.contains("apt-get")), "{deps:?}");
    }

    #[test]
    fn every_failing_crate_is_reported_not_just_the_first() {
        // Reporting crates only when nothing else was found made the second
        // failure disappear as soon as the first one yielded a library.
        let log = "error: failed to run custom build command for `foo-sys v1`\n\
                   Could not find libfoo\n\
                   error: failed to run custom build command for `bar-sys v1`";
        let deps = analyze(log);
        assert!(deps.iter().any(|d| d.name == "libfoo"), "{deps:?}");
        assert!(deps.iter().any(|d| d.name == "foo-sys v1"), "{deps:?}");
        assert!(deps.iter().any(|d| d.name == "bar-sys v1"), "{deps:?}");
    }

    #[test]
    fn cargo_json_output_cannot_produce_a_finding() {
        // A real log is ~95% `--message-format=json`, and those lines are full
        // of package names and feature lists. `"features":[…,"pkg-config",
        // "vcpkg"]` is one space away from looking like a failing probe.
        let json = r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#pkg-config@0.3.33","features":["bundled","pkg-config","vcpkg"],"target":{"name":"libsqlite3_sys"},"message":"cannot find -lfoo (exit status: 101)"}"#;
        assert!(analyze(json).is_empty(), "{:?}", analyze(json));
    }

    #[test]
    fn findings_are_capped_so_a_broken_sysroot_cannot_flood_the_context() {
        let log: String =
            (0..500).map(|i| format!("ld: cannot find -lx{i:05}\n")).collect();
        let deps = analyze(&log);
        assert_eq!(deps.len(), MAX_FINDINGS);
        assert!(hint_lines(&deps).iter().any(|l| l.contains("仅列出前")), "truncation must be visible");
    }

    #[test]
    fn a_strong_finding_survives_a_crowd_of_weak_ones() {
        // The cap is a context-cost guard, not a filter. Twenty vague sightings
        // arriving first must not lock out the one line that actually names a
        // library the linker could not resolve.
        // Deliberately more than the internal collection buffer, not merely
        // more than the reporting cap: a bound that only moved the boundary
        // would still drop the linker failure that arrives last.
        let mut log: String =
            (0..COLLECT_LIMIT * 2).map(|i| format!("Could not find libvague{i}\n")).collect();
        log.push_str("/usr/bin/ld: cannot find -lsodium\n");
        let deps = analyze(&log);
        assert_eq!(deps.len(), MAX_FINDINGS);
        assert_eq!(deps[0].name, "sodium", "best evidence first, got {deps:?}");
    }

    #[test]
    fn one_package_is_suggested_once_even_from_several_findings() {
        let log = "/usr/bin/ld: cannot find -lsodium\n\
                   No package 'openssl' found\n\
                   src/w.c:1:1: fatal error: sodium.h: No such file or directory";
        let install = hint_lines(&analyze(log))
            .into_iter()
            .find(|l| l.contains("apt-get"))
            .expect("an install line");
        assert_eq!(install.matches("libsodium-dev").count(), 1, "{install}");
        assert!(install.contains("libssl-dev"), "{install}");
    }

    #[test]
    fn a_clean_log_yields_nothing() {
        assert!(analyze("   Compiling foo v0.1.0\n    Finished dev profile").is_empty());
        assert!(hint_lines(&[]).is_empty());
    }

    #[test]
    fn hints_mark_guesses_as_guesses() {
        // An agent acting on a guessed package must be able to tell it is
        // guessed, or it will report that the fix did not work without ever
        // suspecting the package name.
        let lines = hint_lines(&analyze("Could not find librrd")).join("\n");
        assert!(lines.contains("librrd") && lines.contains("推测"), "{lines}");
        assert!(lines.contains("apt-get install -y librrd-dev"), "{lines}");

        let lines = hint_lines(&analyze("No package 'openssl' found")).join("\n");
        assert!(lines.contains("需要 libssl-dev"), "{lines}");
        assert!(!lines.contains("推测"), "a known mapping is not a guess: {lines}");
    }

    #[test]
    fn ansi_escapes_do_not_hide_a_finding() {
        assert_eq!(names("\u{1b}[31merror\u{1b}[0m: /usr/bin/ld: cannot find -lfoo"), vec!["foo"]);
    }

    #[test]
    fn absurd_names_are_ignored() {
        assert!(names(&format!("cannot find -l{}", "x".repeat(200))).is_empty());
    }
}
