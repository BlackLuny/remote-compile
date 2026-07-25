//! Docker sandbox (§7.1).
//!
//! Everything an agent submits is untrusted: `build.rs` and proc-macros run
//! arbitrary code during a plain `cargo check`. The container therefore drops
//! all capabilities, gets a read-only root filesystem, hard memory/CPU/pid
//! caps, a hard timeout, and no route to the internet except the allowlisted
//! egress proxy.
//!
//! Note that holding the Docker socket is equivalent to root on this host, so
//! a worker machine must be dedicated to compilation and nothing else.

use anyhow::{anyhow, Context, Result};
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, VolumeCreateRequest};
use bollard::query_parameters::{
    BuildImageOptions, CreateContainerOptions, CreateImageOptions, InspectNetworkOptions,
    KillContainerOptions, ListContainersOptions, ListVolumesOptions, LogsOptions,
    RemoveContainerOptions, RemoveVolumeOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::Docker;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Label prefix on every object we create, so crash recovery and GC can find
/// our leftovers without touching anything else on the host (§8.1).
pub const LABEL_OWNER: &str = "rc.owner";
pub const LABEL_TASK: &str = "rc.task";
pub const LABEL_WORKTREE: &str = "rc.worktree";
pub const LABEL_PROJECT: &str = "rc.project";
pub const OWNER_VALUE: &str = "remote-compile";

/// The internal bridge build containers attach to. Docker adds no default
/// route out of an `internal` network, so the host gateway — where our
/// allowlist proxy listens — is the only reachable address.
pub const EGRESS_NETWORK: &str = "rc-egress";

/// Where the reconstructed workspace is mounted inside the container. The
/// build's working directory is derived from this plus the profile's
/// sub-project `path`; see `Sandbox::run`.
pub const WORKSPACE_MOUNT: &str = "/work";

pub struct Sandbox {
    docker: Docker,
}

#[derive(Debug, Clone)]
pub enum Network {
    /// Fully offline: registry caches already hold everything.
    None,
    /// Reachable only through the allowlist proxy at this address.
    Egress { proxy: String },
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub command: String,
    pub workspace: PathBuf,
    pub env: Vec<String>,
    /// (volume name, container path)
    pub volumes: Vec<(String, String)>,
    /// (host path, container path, read-only)
    pub binds: Vec<(PathBuf, String, bool)>,
    pub workdir: String,
    pub timeout_secs: u32,
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids_limit: i64,
    pub network: Network,
    pub labels: HashMap<String, String>,
}

/// `uid:gid` the build runs as — this process's own.
///
/// The build shares a bind-mounted workspace with the worker across tasks, and
/// §7.3 makes the worker responsible for deleting anything the manifest does
/// not list. A build running as container-root leaves root-owned directories
/// behind (rc-server's own `build.rs` writes `web/dist/` during a plain
/// `cargo check`), and an unprivileged worker can neither chmod nor empty
/// those — the next task on that worktree dies in `apply_deletions`.
///
/// Matching uids fixes that whole class, and dropping root inside the sandbox
/// is worth having on its own (§7.1). The cost is that an environment image
/// must keep its `/rc` mount points writable by an arbitrary uid; the reference
/// Dockerfile does.
pub fn build_user() -> String {
    // Safe: getuid/getgid cannot fail and touch no shared state.
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

#[derive(Debug, Default)]
pub struct RunOutput {
    pub exit_code: i32,
    pub timed_out: bool,
    /// Kept separate: cargo's machine-readable diagnostics go to stdout while
    /// human output goes to stderr (§10.2). Merging them breaks JSON parsing.
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    /// Combined view for classification and the stored log.
    pub fn combined(&self) -> String {
        format!("{}\n{}", self.stderr, self.stdout)
    }
}

impl Sandbox {
    pub fn connect() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("connect to the docker daemon (is it running, and is this user in the docker group?)")?;
        Ok(Sandbox { docker })
    }

    pub async fn ping(&self) -> Result<String> {
        let v = self.docker.version().await?;
        Ok(v.version.unwrap_or_else(|| "unknown".into()))
    }

    /// Container/volume labels we stamp on everything.
    pub fn base_labels() -> HashMap<String, String> {
        HashMap::from([(LABEL_OWNER.to_string(), OWNER_VALUE.to_string())])
    }

    /// Make `image` available locally and return the reference that actually
    /// resolves — which is not always the one we were handed.
    ///
    /// An environment built on a worker rather than pulled from a registry has
    /// no repo digest, so the control plane pins it by image id instead and
    /// hands back a `repo@sha256:<id>` that no registry can serve. The id alone
    /// resolves locally and is just as immutable a handle (§8.3), so try it
    /// before reaching for the network.
    pub async fn ensure_image(&self, image: &str) -> Result<String> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(image.to_string());
        }
        if let Some((_, id)) = image.split_once('@') {
            if self.docker.inspect_image(id).await.is_ok() {
                tracing::debug!(%image, %id, "resolved to an image built on this worker");
                return Ok(id.to_string());
            }
        }
        tracing::info!(%image, "pulling image");
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(image.to_string()),
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("pull {image}"))?;
        }
        Ok(image.to_string())
    }

    pub async fn ensure_volume(&self, name: &str, labels: HashMap<String, String>) -> Result<()> {
        self.docker
            .create_volume(VolumeCreateRequest {
                name: Some(name.to_string()),
                labels: Some(labels),
                ..Default::default()
            })
            .await
            .with_context(|| format!("create volume {name}"))?;
        Ok(())
    }

    /// Create the internal egress network if absent and return its gateway
    /// address — where the allowlist proxy must listen.
    pub async fn ensure_egress_network(&self) -> Result<String> {
        if self
            .docker
            .inspect_network(EGRESS_NETWORK, None::<InspectNetworkOptions>)
            .await
            .is_err()
        {
            self.docker
                .create_network(NetworkCreateRequest {
                    name: EGRESS_NETWORK.to_string(),
                    // No NAT out of this network: only the gateway is
                    // reachable, and that is the proxy.
                    internal: Some(true),
                    labels: Some(Self::base_labels()),
                    ..Default::default()
                })
                .await
                .context("create the rc-egress network")?;
        }
        let net = self
            .docker
            .inspect_network(EGRESS_NETWORK, None::<InspectNetworkOptions>)
            .await?;
        let gateway = net
            .ipam
            .and_then(|i| i.config)
            .and_then(|c| c.into_iter().find_map(|cfg| cfg.gateway))
            .ok_or_else(|| anyhow!("rc-egress network has no gateway address"))?;
        Ok(gateway)
    }

    pub async fn run(&self, spec: &RunSpec) -> Result<RunOutput> {
        let mut labels = Self::base_labels();
        labels.extend(spec.labels.clone());

        // The workspace is the tree named by the manifest, so it mounts at the
        // workspace root — never at `spec.workdir`. Mounting it at the workdir
        // aliased the repository root onto the sub-project's pathname, so
        // `path = "crates/backend"` compiled the whole workspace from a
        // directory that merely *looked* like the sub-project, and the real one
        // sat at `/work/crates/backend/crates/backend`. `workdir` selects where
        // the command runs inside that tree; it does not decide the mount.
        let mut binds: Vec<String> = vec![format!(
            "{}:{}",
            spec.workspace.to_string_lossy(),
            WORKSPACE_MOUNT
        )];
        for (name, path) in &spec.volumes {
            binds.push(format!("{name}:{path}"));
        }
        for (host, container, ro) in &spec.binds {
            binds.push(format!(
                "{}:{}{}",
                host.to_string_lossy(),
                container,
                if *ro { ":ro" } else { "" }
            ));
        }

        let mut env = spec.env.clone();
        let network_mode = match &spec.network {
            Network::None => "none".to_string(),
            Network::Egress { proxy } => {
                for key in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
                    env.push(format!("{key}={proxy}"));
                }
                // Loopback must not go through the proxy or cargo's own
                // subprocesses stall.
                env.push("no_proxy=localhost,127.0.0.1".to_string());
                EGRESS_NETWORK.to_string()
            }
        };

        let host_config = HostConfig {
            network_mode: Some(network_mode),
            binds: Some(binds),
            cap_drop: Some(vec!["ALL".to_string()]),
            // A read-only root plus writable mounts means a build script can
            // only dirty places we already intend to throw away.
            readonly_rootfs: Some(true),
            tmpfs: Some(HashMap::from([
                ("/tmp".to_string(), "rw,nosuid,nodev,exec,size=4g".to_string()),
                ("/rc/home".to_string(), "rw,nosuid,nodev,size=1g".to_string()),
            ])),
            memory: Some((spec.memory_mb * 1024 * 1024) as i64),
            nano_cpus: Some((spec.cpus * 1e9) as i64),
            // Without this a fork bomb in build.rs takes down the worker.
            pids_limit: Some(spec.pids_limit),
            security_opt: Some(vec!["no-new-privileges".to_string()]),
            auto_remove: Some(false),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(spec.image.clone()),
            // `-c`, never `-lc`: a login shell sources /etc/profile, which on
            // Debian resets PATH and drops everything the image put on it —
            // including /usr/local/cargo/bin, so every command becomes
            // `cargo: not found`. Docker already gives the container the
            // image's environment.
            cmd: Some(vec!["/bin/sh".into(), "-c".into(), spec.command.clone()]),
            user: Some(build_user()),
            env: Some(env),
            working_dir: Some(spec.workdir.clone()),
            labels: Some(labels),
            host_config: Some(host_config),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(spec.name.clone()),
                    ..Default::default()
                }),
                config,
            )
            .await
            .with_context(|| format!("create container for {}", spec.name))?;
        let id = created.id;

        let result = self.run_created(&id, spec).await;
        // Always clean up, even when the run failed: a leaked container pins
        // its volumes and the workspace bind.
        if let Err(e) = self
            .docker
            .remove_container(
                &id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    ..Default::default()
                }),
            )
            .await
        {
            tracing::warn!(container = %id, error = %e, "failed to remove container");
        }
        result
    }

    async fn run_created(&self, id: &str, spec: &RunSpec) -> Result<RunOutput> {
        self.docker
            .start_container(id, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("start container {id}"))?;

        let mut logs = self.docker.logs(
            id,
            Some(LogsOptions {
                stdout: true,
                stderr: true,
                follow: true,
                ..Default::default()
            }),
        );
        let mut out = RunOutput::default();
        let deadline = std::time::Duration::from_secs(spec.timeout_secs.max(1) as u64);
        let started = std::time::Instant::now();

        let collect = async {
            while let Some(chunk) = logs.next().await {
                match chunk {
                    Ok(bollard::container::LogOutput::StdOut { message }) => {
                        out.stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(bollard::container::LogOutput::StdErr { message }) => {
                        out.stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(other) => {
                        out.stdout.push_str(&other.to_string());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "log stream error");
                        break;
                    }
                }
            }
        };

        if tokio::time::timeout(deadline, collect).await.is_err() {
            out.timed_out = true;
            let _ = self
                .docker
                .kill_container(id, None::<KillContainerOptions>)
                .await;
            out.exit_code = 137;
            out.stderr.push_str(&format!(
                "\n[rc-worker] hard timeout after {}s — killed (§7.1)\n",
                spec.timeout_secs
            ));
            return Ok(out);
        }

        let remaining = deadline.saturating_sub(started.elapsed());
        let mut wait = self.docker.wait_container(id, None::<WaitContainerOptions>);
        match tokio::time::timeout(remaining.max(std::time::Duration::from_secs(1)), wait.next()).await {
            Ok(Some(Ok(res))) => out.exit_code = res.status_code as i32,
            Ok(Some(Err(e))) => {
                // A non-zero exit arrives here as an error variant on some
                // daemon versions; treat it as the exit status it is.
                if let bollard::errors::Error::DockerContainerWaitError { code, .. } = &e {
                    out.exit_code = *code as i32;
                } else {
                    return Err(e).context("wait for container");
                }
            }
            Ok(None) => out.exit_code = 0,
            Err(_) => {
                out.timed_out = true;
                out.exit_code = 137;
                let _ = self
                    .docker
                    .kill_container(id, None::<KillContainerOptions>)
                    .await;
            }
        }
        Ok(out)
    }

    /// Build an agent-submitted Dockerfile (§8.2). The build itself is a
    /// sandboxed task and its output is a digest we can later trust.
    pub async fn build_image(&self, tag: &str, dockerfile: &str) -> Result<(String, String)> {
        let context = tar_with_dockerfile(dockerfile)?;
        let mut stream = self.docker.build_image(
            BuildImageOptions {
                dockerfile: "Dockerfile".to_string(),
                t: Some(tag.to_string()),
                rm: true,
                forcerm: true,
                // The classic builder, deliberately. Asking for BuildKit over
                // this endpoint makes the daemon emit its progress trace as a
                // base64 blob in `aux`, where bollard expects an `ImageId`, and
                // every build dies on `invalid type: string "Cm8KR3No…"`.
                // Driving BuildKit properly needs a grpc session bollard only
                // offers behind its `buildkit` feature; environment images are
                // built once and then cached by digest, so the classic builder
                // is not worth that.
                version: bollard::query_parameters::BuilderVersion::BuilderV1,
                ..Default::default()
            },
            None,
            Some(bollard::body_full(context.into())),
        );
        let mut log = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(info) => {
                    if let Some(s) = info.stream {
                        log.push_str(&s);
                    }
                    if let Some(err) = info.error_detail.and_then(|d| d.message) {
                        log.push_str(&err);
                        return Err(anyhow!("image build failed: {err}\n{log}"));
                    }
                }
                Err(e) => return Err(anyhow!("image build failed: {e}\n{log}")),
            }
        }
        let digest = self.digest_of(tag).await?;
        Ok((digest, log))
    }

    /// The immutable identity of an image — what the fingerprint and the
    /// approval list are keyed on (§5.1/§8.3).
    pub async fn digest_of(&self, image: &str) -> Result<String> {
        let info = self
            .docker
            .inspect_image(image)
            .await
            .with_context(|| format!("inspect {image}"))?;
        if let Some(rd) = info.repo_digests.as_ref().and_then(|d| d.first()) {
            if let Some((_, digest)) = rd.split_once('@') {
                return Ok(digest.to_string());
            }
        }
        // A locally built image has no repo digest until it is pushed; its
        // content id is the next best immutable handle.
        info.id
            .ok_or_else(|| anyhow!("image {image} has neither a repo digest nor an id"))
    }

    /// Crash recovery (§8.1): remove containers we created that no longer
    /// belong to a live task.
    pub async fn reconcile(&self, live_tasks: &[String]) -> Result<usize> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{LABEL_OWNER}={OWNER_VALUE}")],
        )]);
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;
        let mut removed = 0;
        for c in containers {
            let task = c
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_TASK))
                .cloned()
                .unwrap_or_default();
            if !task.is_empty() && live_tasks.contains(&task) {
                continue;
            }
            let Some(id) = c.id else { continue };
            match self
                .docker
                .remove_container(
                    &id,
                    Some(RemoveContainerOptions {
                        force: true,
                        v: false,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(container = %id, %task, "removed orphaned container");
                }
                Err(e) => tracing::warn!(container = %id, error = %e, "reconcile removal failed"),
            }
        }
        Ok(removed)
    }

    /// Volumes we own, with their labels — the input to cache GC (§9).
    pub async fn our_volumes(&self) -> Result<Vec<(String, HashMap<String, String>)>> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{LABEL_OWNER}={OWNER_VALUE}")],
        )]);
        let list = self
            .docker
            .list_volumes(Some(ListVolumesOptions {
                filters: Some(filters),
            }))
            .await?;
        Ok(list
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|v| (v.name, v.labels))
            .collect())
    }

    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        self.docker
            .remove_volume(name, Some(RemoveVolumeOptions { force: true }))
            .await
            .with_context(|| format!("remove volume {name}"))?;
        Ok(())
    }

    pub async fn kill(&self, container_name: &str) -> Result<()> {
        self.docker
            .kill_container(container_name, None::<KillContainerOptions>)
            .await
            .ok();
        Ok(())
    }
}

/// Minimal uncompressed tar holding a single `Dockerfile` entry. Agent-supplied
/// Dockerfiles are self-contained by contract (§8.2), so there is no other
/// build context to carry.
fn tar_with_dockerfile(dockerfile: &str) -> Result<Vec<u8>> {
    let content = dockerfile.as_bytes();
    let mut header = [0u8; 512];

    let name = b"Dockerfile";
    header[..name.len()].copy_from_slice(name);
    write_octal(&mut header[100..108], 0o644, 7); // mode
    write_octal(&mut header[108..116], 0, 7); // uid
    write_octal(&mut header[116..124], 0, 7); // gid
    write_octal(&mut header[124..136], content.len() as u64, 11); // size
    write_octal(&mut header[136..148], 0, 11); // mtime
    header[156] = b'0'; // regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    // Checksum is computed with the checksum field itself read as spaces.
    for b in header[148..156].iter_mut() {
        *b = b' ';
    }
    let sum: u32 = header.iter().map(|b| *b as u32).sum();
    write_octal(&mut header[148..154], sum as u64, 6);
    header[154] = 0;
    header[155] = b' ';

    let mut out = Vec::with_capacity(512 * 4 + content.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(content);
    // Pad the body to a 512-byte boundary, then two zero blocks to end.
    let pad = (512 - content.len() % 512) % 512;
    out.extend(std::iter::repeat_n(0u8, pad));
    out.extend(std::iter::repeat_n(0u8, 1024));
    Ok(out)
}

fn write_octal(field: &mut [u8], value: u64, digits: usize) {
    let text = format!("{value:0digits$o}", digits = digits);
    let bytes = text.as_bytes();
    let n = bytes.len().min(field.len());
    field[..n].copy_from_slice(&bytes[..n]);
    if n < field.len() {
        field[n] = 0;
    }
}

/// Container name for a task, stable and collision-free.
pub fn container_name(task_id: &str) -> String {
    format!("rc-task-{}", task_id.replace(|c: char| !c.is_alphanumeric(), "-"))
}

pub fn target_volume(worktree_id: &str) -> String {
    format!("rc-target-{worktree_id}")
}

/// One rustup store for the whole worker: `rust-toolchain.toml` pins differ per
/// project but the toolchains themselves are shared, immutable and large.
pub fn rustup_volume() -> String {
    "rc-rustup".to_string()
}

pub fn registry_volume(project_id: &str) -> String {
    format!("rc-cargo-{project_id}")
}

pub fn workspace_dir(work_root: &Path, worktree_id: &str) -> PathBuf {
    work_root.join(worktree_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_context_is_a_valid_single_entry_tar() {
        let tar = tar_with_dockerfile("FROM rust:1\nRUN echo hi\n").unwrap();
        assert_eq!(&tar[..10], b"Dockerfile");
        assert_eq!(&tar[257..262], b"ustar");
        assert_eq!(tar[156], b'0', "must be typed as a regular file");
        // header + padded body + two terminating blocks
        assert_eq!(tar.len() % 512, 0);
        assert!(tar[tar.len() - 1024..].iter().all(|b| *b == 0));
    }

    #[test]
    fn the_tar_checksum_matches_what_a_reader_will_compute() {
        let tar = tar_with_dockerfile("FROM scratch\n").unwrap();
        let mut header = [0u8; 512];
        header.copy_from_slice(&tar[..512]);
        let stored = std::str::from_utf8(&header[148..154])
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim(), 8).ok())
            .expect("checksum field is octal");
        for b in header[148..156].iter_mut() {
            *b = b' ';
        }
        let computed: u32 = header.iter().map(|b| *b as u32).sum();
        assert_eq!(stored, computed);
    }

    #[test]
    fn the_declared_size_matches_the_payload() {
        let body = "FROM rust:1\n";
        let tar = tar_with_dockerfile(body).unwrap();
        let size = std::str::from_utf8(&tar[124..135])
            .ok()
            .and_then(|s| u64::from_str_radix(s.trim_end_matches('\0').trim(), 8).ok())
            .unwrap();
        assert_eq!(size as usize, body.len());
        assert_eq!(&tar[512..512 + body.len()], body.as_bytes());
    }

    #[test]
    fn object_names_are_derived_predictably() {
        assert_eq!(container_name("t-01J8XYZ"), "rc-task-t-01J8XYZ");
        assert_eq!(target_volume("w-abc"), "rc-target-w-abc");
        assert_eq!(registry_volume("p-abc"), "rc-cargo-p-abc");
    }

    #[test]
    fn container_names_never_contain_docker_hostile_characters() {
        let name = container_name("t-01/../evil:latest");
        assert!(name.chars().all(|c| c.is_alphanumeric() || c == '-'));
    }

    #[test]
    fn every_object_carries_the_ownership_label() {
        // §8.1: crash recovery finds our leftovers by label and nothing else's.
        assert_eq!(Sandbox::base_labels()[LABEL_OWNER], OWNER_VALUE);
    }

    #[test]
    fn combined_output_puts_stderr_first() {
        let out = RunOutput {
            stdout: "{\"reason\":\"x\"}".into(),
            stderr: "error: boom".into(),
            ..Default::default()
        };
        assert!(out.combined().starts_with("error: boom"));
    }
}
