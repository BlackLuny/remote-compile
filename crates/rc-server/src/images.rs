//! Environment image lifecycle (§8).
//!
//! An image is the one thing in this system that runs *before* the sandbox
//! exists: a Dockerfile executes arbitrary commands at build time, which the
//! runtime sandbox cannot retroactively contain. Hence approval-by-default
//! (§8.3) and a digest-only trust list.

use crate::app::App;
use crate::events::Event;
use anyhow::{anyhow, Result};
use rc_core::model::ImageStatus;
use rc_core::pb;
use rc_core::{ids, now_ms};
use std::sync::Arc;

/// Register an environment request. Always returns immediately — blocking an
/// agent on an image build is explicitly forbidden (§8.4).
pub fn prepare_env(app: &App, req: &pb::PrepareEnvReq) -> Result<pb::EnvStatus> {
    if req.dockerfile.trim().is_empty() && req.image_ref.trim().is_empty() {
        return Err(anyhow!("prepare_env needs either a dockerfile or an image reference"));
    }
    let env_id = ids::env_id_for_source(&req.dockerfile, &req.image_ref);

    if let Some(existing) = app.store.get_image(&env_id)? {
        // Identical request from another agent: share the outcome rather than
        // rebuilding (fleet learning, §1.1).
        return Ok(env_status(&existing));
    }

    let policy = app.policy();
    let status = if policy.require_image_approval {
        ImageStatus::PendingApproval
    } else {
        ImageStatus::Building
    };
    let row = crate::store::ImageRow {
        id: env_id.clone(),
        image_ref: if req.dockerfile.is_empty() {
            req.image_ref.clone()
        } else {
            format!("rc-registry/env/{}:latest", &env_id[2..10])
        },
        dockerfile: req.dockerfile.clone(),
        pull_ref: req.image_ref.clone(),
        status: status.as_str().to_string(),
        description: if req.description.is_empty() {
            req.reason.clone()
        } else {
            req.description.clone()
        },
        created_by: req.agent_session.clone(),
        ..Default::default()
    };
    app.store.upsert_image(&row)?;
    app.store.audit(
        &req.agent_session,
        "prepare_env",
        &env_id,
        &format!("project={} reason={}", req.project_id, req.reason),
    )?;
    app.events.publish(Event::ImageUpdated {
        env_id: env_id.clone(),
        status: row.status.clone(),
        message: "environment requested".into(),
        at: now_ms(),
    });
    Ok(env_status(&row))
}

pub fn env_status(row: &crate::store::ImageRow) -> pb::EnvStatus {
    pb::EnvStatus {
        env_id: row.id.clone(),
        status: row.status.clone(),
        message: if row.message.is_empty() {
            explain_status(&row.status)
        } else {
            row.message.clone()
        },
        image_ref: full_ref(row),
        digest: row.digest.clone(),
        build_log_ref: row.build_log_ref.clone(),
        health: Some(health_of(row)),
    }
}

/// Digest-pinned reference, which is the only form a task may use (§5.1).
pub fn full_ref(row: &crate::store::ImageRow) -> String {
    if row.digest.is_empty() {
        row.image_ref.clone()
    } else {
        let base = row
            .image_ref
            .split_once('@')
            .map(|(b, _)| b.to_string())
            .unwrap_or_else(|| row.image_ref.split(':').next().unwrap_or("").to_string());
        format!("{}@{}", base, row.digest)
    }
}

pub fn health_of(row: &crate::store::ImageRow) -> pb::EnvHealth {
    pb::EnvHealth {
        last_success_at: row.last_success_at,
        success_rate_7d: if row.total_count == 0 {
            0.0
        } else {
            row.success_count as f64 / row.total_count as f64
        },
        total_runs: row.total_count as u32,
    }
}

fn explain_status(status: &str) -> String {
    match status {
        "pending_approval" => "等待管理员审批（§8.3：镜像构建期是独立攻击面，运行时沙箱兜不住）".into(),
        "building" => "正在构建，构建完成后自动可用；不要阻塞等待".into(),
        "healthy" => "可用".into(),
        "failing" => "连续 env_error，已降权；请检查镜像或修 Dockerfile".into(),
        "rejected" => "管理员已拒绝".into(),
        _ => status.to_string(),
    }
}

/// Images the agent can pick from (§12 `list_envs`).
pub fn list_envs(app: &App, req: &pb::ListEnvsReq) -> Result<Vec<pb::EnvImage>> {
    let needles: Vec<String> = req
        .query
        .split_whitespace()
        .map(|s| s.to_lowercase())
        .collect();
    let mut out = Vec::new();
    for row in app.store.list_images(None)? {
        let haystack = format!(
            "{} {} {} {}",
            row.image_ref, row.description, row.dockerfile, row.pull_ref
        )
        .to_lowercase();
        if !needles.iter().all(|n| haystack.contains(n)) {
            continue;
        }
        if !req.arch.is_empty()
            && !row.arch.is_empty()
            && !rc_core::arch::worker_matches_image_arch(&req.arch, &row.arch)
        {
            continue;
        }
        if !req.target.is_empty() && !row.targets.is_empty() && !row.targets.contains(&req.target) {
            continue;
        }
        out.push(pb::EnvImage {
            env_id: row.id.clone(),
            image_ref: full_ref(&row),
            digest: row.digest.clone(),
            status: row.status.clone(),
            arch: split_csv(&row.arch),
            targets: split_csv(&row.targets),
            health: Some(health_of(&row)),
            used_by: app.store.image_usage(&row.digest).unwrap_or_default(),
            built_at: row.built_at,
            description: row.description.clone(),
        });
    }
    // Healthiest and most recently proven first — that is the one an agent
    // should reuse.
    out.sort_by(|a, b| {
        let rank = |e: &pb::EnvImage| match e.status.as_str() {
            "healthy" => 0,
            "building" => 1,
            "pending_approval" => 2,
            "failing" => 3,
            _ => 4,
        };
        rank(a)
            .cmp(&rank(b))
            .then(b.health.as_ref().map(|h| h.last_success_at).unwrap_or(0).cmp(
                &a.health.as_ref().map(|h| h.last_success_at).unwrap_or(0),
            ))
    });
    Ok(out)
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Best approved image for a project — what `get_build_profile` hands back so
/// the agent can pin a digest without guessing.
pub fn default_image_for(app: &App, adapter: &str) -> Result<Option<String>> {
    let mut best: Option<(i64, String)> = None;
    for row in app.store.list_images(Some("healthy"))? {
        if row.approved_by.is_empty() || row.digest.is_empty() {
            continue;
        }
        let hay = format!("{} {} {}", row.image_ref, row.description, row.pull_ref).to_lowercase();
        let matches_adapter = adapter.is_empty() || hay.contains(&adapter.to_lowercase());
        let score = row.last_success_at + if matches_adapter { 1 } else { 0 };
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, full_ref(&row)));
        }
    }
    Ok(best.map(|(_, r)| r))
}

/// Hand pending builds to a worker that can run BuildKit (§8.2). Building is
/// itself a sandboxed task.
///
/// When the image row already names a host arch, only a matching worker may
/// build it — otherwise the digest would be the wrong platform for every task
/// that later demands that arch.
pub async fn dispatch_pending_builds(app: &Arc<App>) -> Result<usize> {
    let pending = app.store.list_images(Some("building"))?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut sent = 0;
    for img in pending {
        if img.built_at > 0 {
            continue;
        }
        let Some(worker) = app.workers.snapshot().into_iter().find(|w| {
            w.status == "online"
                && w.free_slots() > 0
                && rc_core::arch::worker_matches_image_arch(&w.arch, &img.arch)
        }) else {
            // No eligible worker right now; try the next image (another may
            // need a different arch that is available).
            continue;
        };
        // `building` is a durable status, not evidence that nothing is running:
        // without this claim the two-second tick re-sends the same order for
        // the whole length of the build.
        if !app.claim_image_build(&img.id, &worker.id, &worker.arch) {
            continue;
        }
        let order = pb::ImageBuildOrder {
            env_id: img.id.clone(),
            dockerfile: img.dockerfile.clone(),
            image_ref: img.image_ref.clone(),
            pull_ref: img.pull_ref.clone(),
        };
        if app
            .workers
            .send(
                &worker.id,
                pb::ServerCmd {
                    body: Some(pb::server_cmd::Body::BuildImage(order)),
                },
            )
            .await
        {
            app.store.set_image_status(
                &img.id,
                "building",
                &format!("building on {} ({})", worker.id, worker.arch),
            )?;
            sent += 1;
        } else {
            app.release_image_build(&img.id);
        }
    }
    Ok(sent)
}

/// A worker finished building. The digest now becomes the trust anchor, and
/// the builder's host arch is written onto the image so placement can match.
pub fn on_build_done(app: &App, done: &pb::ImageBuildDone) -> Result<()> {
    let builder_arch = app
        .release_image_build(&done.env_id)
        .map(|(_worker, arch)| arch)
        .filter(|a| !a.is_empty());
    let arch = builder_arch
        .as_deref()
        .and_then(rc_core::arch::normalize_host_arch);
    app.store.finish_image_build(
        &done.env_id,
        &done.digest,
        &done.log_blob,
        done.ok,
        &done.message,
        arch.as_deref(),
    )?;
    app.metrics.incr("images_built_total", 1.0);
    if !done.ok {
        app.store
            .raise_alert(&format!("image_build:{}", done.env_id), "error", &done.message)?;
    }
    app.events.publish(Event::ImageUpdated {
        env_id: done.env_id.clone(),
        status: if done.ok { "healthy".into() } else { "failing".into() },
        message: done.message.clone(),
        at: now_ms(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ImageRow;

    fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("rc-img-{}", ulid::Ulid::generate()));
        App::new(crate::config::Config {
            data_dir: dir,
            http_addr: "127.0.0.1:0".into(),
            grpc_addr: "127.0.0.1:0".into(),
            allow_anonymous_agents: true,
            session_ttl_secs: 3600,
        })
        .unwrap()
    }

    #[test]
    fn prepare_env_returns_immediately_and_needs_approval() {
        // §8.4: it must never block the agent; §8.3: it must not be trusted yet.
        let a = app();
        let st = prepare_env(
            &a,
            &pb::PrepareEnvReq {
                dockerfile: "FROM rust:1\nRUN apt-get install -y protobuf-compiler".into(),
                agent_session: "s1".into(),
                reason: "needs protoc".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(st.status, "pending_approval");
        assert!(!st.env_id.is_empty());
        assert!(st.message.contains("审批"));
    }

    #[test]
    fn the_same_dockerfile_from_two_agents_is_one_environment() {
        let a = app();
        let req = pb::PrepareEnvReq {
            dockerfile: "FROM rust:1".into(),
            agent_session: "s1".into(),
            ..Default::default()
        };
        let first = prepare_env(&a, &req).unwrap();
        let mut second_req = req.clone();
        second_req.agent_session = "s2".into();
        let second = prepare_env(&a, &second_req).unwrap();
        assert_eq!(first.env_id, second.env_id);
    }

    #[test]
    fn prepare_env_rejects_an_empty_request() {
        assert!(prepare_env(&app(), &pb::PrepareEnvReq::default()).is_err());
    }

    #[test]
    fn approval_disabled_goes_straight_to_building() {
        let a = app();
        let mut p = a.policy();
        p.require_image_approval = false;
        a.set_policy(p).unwrap();
        let st = prepare_env(
            &a,
            &pb::PrepareEnvReq { dockerfile: "FROM rust:1".into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(st.status, "building");
    }

    #[test]
    fn full_ref_pins_the_digest() {
        let row = ImageRow {
            image_ref: "rc-registry/env/rust:latest".into(),
            digest: "sha256:abc".into(),
            ..Default::default()
        };
        assert_eq!(full_ref(&row), "rc-registry/env/rust@sha256:abc");
    }

    #[test]
    fn full_ref_leaves_an_unbuilt_image_alone() {
        let row = ImageRow { image_ref: "rust:1".into(), ..Default::default() };
        assert_eq!(full_ref(&row), "rust:1");
    }

    #[test]
    fn list_envs_filters_by_query_and_ranks_healthy_first() {
        let a = app();
        a.store
            .upsert_image(&ImageRow {
                id: "e1".into(),
                image_ref: "reg/rust-protoc:1".into(),
                digest: "sha256:1".into(),
                description: "rust with protoc".into(),
                status: "failing".into(),
                ..Default::default()
            })
            .unwrap();
        a.store
            .upsert_image(&ImageRow {
                id: "e2".into(),
                image_ref: "reg/rust-protoc:2".into(),
                digest: "sha256:2".into(),
                description: "rust with protoc, newer".into(),
                status: "healthy".into(),
                ..Default::default()
            })
            .unwrap();
        a.store
            .upsert_image(&ImageRow {
                id: "e3".into(),
                image_ref: "reg/golang:1".into(),
                status: "healthy".into(),
                ..Default::default()
            })
            .unwrap();

        let found = list_envs(&a, &pb::ListEnvsReq { query: "rust protoc".into(), ..Default::default() }).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].env_id, "e2", "healthy ranks above failing");
    }

    #[test]
    fn default_image_only_offers_approved_digests() {
        let a = app();
        a.store
            .upsert_image(&ImageRow {
                id: "e1".into(),
                image_ref: "reg/rust:1".into(),
                digest: "sha256:1".into(),
                status: "healthy".into(),
                description: "rust".into(),
                ..Default::default()
            })
            .unwrap();
        // Not approved yet -> not offered.
        assert!(default_image_for(&a, "rust").unwrap().is_none());
        a.store.approve_image("e1", "admin").unwrap();
        assert_eq!(
            default_image_for(&a, "rust").unwrap().as_deref(),
            Some("reg/rust@sha256:1")
        );
    }

    #[test]
    fn a_dispatched_build_is_not_handed_out_again_while_it_runs() {
        // The dispatcher ticks every two seconds and an image row stays
        // `building` for the whole build, so a second claim must be refused —
        // otherwise the worker starts one `docker build` per tick.
        let a = app();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        a.workers.connect("w1", "x86_64", "0.1.0", 4, tx);

        assert!(a.claim_image_build("e1", "w1", "x86_64"));
        assert!(!a.claim_image_build("e1", "w1", "x86_64"));
        assert!(
            !a.claim_image_build("e1", "w2", "x86_64"),
            "another worker may not take it either"
        );

        // A different environment is unaffected.
        assert!(a.claim_image_build("e2", "w1", "x86_64"));

        // Once it finishes the slot is free again, so a rebuild can happen.
        on_build_done(
            &a,
            &pb::ImageBuildDone {
                env_id: "e1".into(),
                ok: true,
                digest: "sha256:1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(a.claim_image_build("e1", "w1", "x86_64"));
    }

    #[test]
    fn successful_build_stamps_builder_arch_on_the_image() {
        let a = app();
        a.store
            .upsert_image(&ImageRow {
                id: "e1".into(),
                status: "building".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(a.claim_image_build("e1", "w-arm", "aarch64"));
        on_build_done(
            &a,
            &pb::ImageBuildDone {
                env_id: "e1".into(),
                ok: true,
                digest: "sha256:arm".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let row = a.store.get_image("e1").unwrap().unwrap();
        assert_eq!(row.arch, "aarch64");
        assert_eq!(row.digest, "sha256:arm");
        assert_eq!(row.status, "healthy");
    }

    #[test]
    fn list_envs_can_filter_by_stamped_arch() {
        let a = app();
        a.store
            .upsert_image(&ImageRow {
                id: "e-arm".into(),
                image_ref: "reg/rust:arm".into(),
                digest: "sha256:arm".into(),
                status: "healthy".into(),
                arch: "aarch64".into(),
                description: "rust".into(),
                ..Default::default()
            })
            .unwrap();
        a.store
            .upsert_image(&ImageRow {
                id: "e-x86".into(),
                image_ref: "reg/rust:x86".into(),
                digest: "sha256:x86".into(),
                status: "healthy".into(),
                arch: "x86_64".into(),
                description: "rust".into(),
                ..Default::default()
            })
            .unwrap();
        let arm = list_envs(
            &a,
            &pb::ListEnvsReq {
                query: "rust".into(),
                arch: "aarch64".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(arm.len(), 1);
        assert_eq!(arm[0].env_id, "e-arm");
    }

    #[test]
    fn a_failed_build_raises_an_alert() {
        let a = app();
        a.store
            .upsert_image(&ImageRow { id: "e1".into(), status: "building".into(), ..Default::default() })
            .unwrap();
        on_build_done(
            &a,
            &pb::ImageBuildDone {
                env_id: "e1".into(),
                ok: false,
                message: "apt-get failed".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(a.store.get_image("e1").unwrap().unwrap().status, "failing");
        assert_eq!(a.store.list_alerts(false).unwrap().len(), 1);
    }
}
