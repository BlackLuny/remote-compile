//! Worker selection (§6).
//!
//! Kept as a pure function over plain data so the placement rules — which are
//! the difference between "compiles in 8 seconds" and "recompiles the world" —
//! can be tested without a fleet.

use crate::config::Policy;

/// Everything the scheduler knows about one worker.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub worker_id: String,
    pub arch: String,
    pub status: String,
    pub cpu_load: f64,
    pub disk_free_gb: u64,
    pub free_slots: u32,
    /// Worktrees whose target volume lives on this worker.
    pub cached_worktrees: Vec<String>,
    pub cached_projects: Vec<String>,
    pub cached_images: Vec<String>,
    /// Worktrees currently executing here — a second task for the same
    /// worktree would just block on cargo's file lock (§6.2).
    pub busy_worktrees: Vec<String>,
    /// Optional features the worker reports on every heartbeat.
    pub capabilities: Vec<String>,
}

/// What a task needs.
#[derive(Debug, Clone, Default)]
pub struct Demand {
    pub worktree_id: String,
    pub project_id: String,
    pub image: String,
    /// Required architecture; empty means "any".
    pub arch: String,
    /// Estimated disk requirement in GB, from this project's history.
    pub est_disk_gb: u64,
    /// Workers that already failed this task — a retry must land elsewhere.
    pub excluded: Vec<String>,
    /// Capabilities without which this task cannot run correctly.
    pub required_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub worker_id: String,
    pub score: f64,
}

/// Why a worker was excluded — surfaced in the admin UI when nothing matches,
/// because "task stuck in queue" with no explanation is the worst outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Reject {
    NotOnline,
    ArchMismatch,
    NoFreeSlot,
    InsufficientDisk,
    AlreadyTried,
    WorktreeBusy,
    /// The worker predates something this task needs. Silently running it
    /// anyway would corrupt the workspace rather than fail cleanly.
    MissingCapability(String),
}

pub fn evaluate(c: &Candidate, d: &Demand, p: &Policy) -> Result<f64, Reject> {
    if c.status != "online" {
        return Err(Reject::NotOnline);
    }
    if !d.arch.is_empty() && !c.arch.is_empty() && c.arch != d.arch {
        return Err(Reject::ArchMismatch);
    }
    if d.excluded.iter().any(|w| w == &c.worker_id) {
        return Err(Reject::AlreadyTried);
    }
    if let Some(missing) = d
        .required_capabilities
        .iter()
        .find(|need| !c.capabilities.iter().any(|has| has == *need))
    {
        return Err(Reject::MissingCapability(missing.clone()));
    }
    if c.free_slots == 0 {
        return Err(Reject::NoFreeSlot);
    }
    // §6.2: same worktree, same worker => serialize.
    if c.busy_worktrees.iter().any(|w| w == &d.worktree_id) {
        return Err(Reject::WorktreeBusy);
    }
    let needed = ((d.est_disk_gb as f64) * 1.5).ceil() as u64;
    if c.disk_free_gb < p.min_disk_free_gb.max(needed) {
        return Err(Reject::InsufficientDisk);
    }

    // disk_fit saturates: three times the estimate is as good as ten.
    let headroom = (d.est_disk_gb.max(1) as f64) * 3.0;
    let disk_fit = ((c.disk_free_gb as f64) / headroom).min(1.0);
    let cpu_fit = (1.0 - c.cpu_load).clamp(0.0, 1.0);
    let cache_affinity = if c.cached_worktrees.iter().any(|w| w == &d.worktree_id) {
        1.0
    } else if c.cached_projects.iter().any(|w| w == &d.project_id) {
        0.6
    } else {
        0.0
    };
    let image_affinity = if !d.image.is_empty() && c.cached_images.iter().any(|i| i == &d.image) {
        1.0
    } else {
        0.0
    };

    Ok(p.w_disk * disk_fit
        + p.w_cpu * cpu_fit
        + p.w_cache_affinity * cache_affinity
        + p.w_image_affinity * image_affinity)
}

/// Best worker for a task, or `None` when nothing qualifies.
pub fn pick(candidates: &[Candidate], d: &Demand, p: &Policy) -> Option<Scored> {
    let mut best: Option<Scored> = None;
    for c in candidates {
        let Ok(score) = evaluate(c, d, p) else { continue };
        let better = match &best {
            // Deterministic tie-break by id keeps placement stable and tests
            // reproducible.
            Some(b) => score > b.score + f64::EPSILON || (score >= b.score && c.worker_id < b.worker_id),
            None => true,
        };
        if better {
            best = Some(Scored {
                worker_id: c.worker_id.clone(),
                score,
            });
        }
    }
    best
}

/// Collected rejection reasons, for the "why is my task queued" answer.
pub fn explain(candidates: &[Candidate], d: &Demand, p: &Policy) -> Vec<(String, Reject)> {
    candidates
        .iter()
        .filter_map(|c| evaluate(c, d, p).err().map(|r| (c.worker_id.clone(), r)))
        .collect()
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn worker(id: &str) -> Candidate {
        Candidate {
            worker_id: id.into(),
            arch: "x86_64".into(),
            status: "online".into(),
            cpu_load: 0.1,
            disk_free_gb: 500,
            free_slots: 4,
            ..Default::default()
        }
    }

    fn demand() -> Demand {
        Demand {
            worktree_id: "w1".into(),
            project_id: "p1".into(),
            image: "img@sha256:a".into(),
            arch: "x86_64".into(),
            est_disk_gb: 20,
            excluded: vec![],
            required_capabilities: vec![],
        }
    }

    #[test]
    fn warm_worktree_cache_beats_a_cold_but_idle_worker() {
        let p = Policy::default();
        let mut cold = worker("a");
        cold.cpu_load = 0.0;
        let mut warm = worker("b");
        warm.cpu_load = 0.7;
        warm.cached_worktrees = vec!["w1".into()];
        // Reusing the target volume is worth far more than a quiet CPU.
        assert_eq!(pick(&[cold, warm], &demand(), &p).unwrap().worker_id, "b");
    }

    #[test]
    fn project_cache_is_worth_less_than_worktree_cache() {
        let p = Policy::default();
        let mut project_only = worker("a");
        project_only.cached_projects = vec!["p1".into()];
        let mut worktree = worker("b");
        worktree.cached_worktrees = vec!["w1".into()];
        assert_eq!(pick(&[project_only, worktree], &demand(), &p).unwrap().worker_id, "b");
    }

    #[test]
    fn a_busy_worktree_is_skipped_not_queued_behind() {
        // §6.2 / risk #8: two tasks for one worktree on one worker just wait
        // on cargo's lock. Prefer any other machine.
        let p = Policy::default();
        let mut busy = worker("a");
        busy.cached_worktrees = vec!["w1".into()];
        busy.busy_worktrees = vec!["w1".into()];
        let idle = worker("b");
        assert_eq!(pick(&[busy.clone(), idle], &demand(), &p).unwrap().worker_id, "b");
        assert_eq!(evaluate(&busy, &demand(), &p), Err(Reject::WorktreeBusy));
    }

    #[test]
    fn a_worker_that_cannot_do_multi_root_is_never_given_a_multi_root_task() {
        // It would not fail cleanly: it would extract the git baseline one
        // directory too high and produce a tree that looks like CAS corruption.
        let p = Policy::default();
        let old = worker("a");
        let mut new = worker("b");
        new.capabilities = vec!["multi-root".into()];

        let mut d = demand();
        d.required_capabilities = vec!["multi-root".into()];
        assert_eq!(
            evaluate(&old, &d, &p),
            Err(Reject::MissingCapability("multi-root".into()))
        );
        assert_eq!(pick(&[old.clone(), new], &d, &p).unwrap().worker_id, "b");

        // Ordinary tasks still go anywhere.
        assert!(evaluate(&old, &demand(), &p).is_ok());
    }

    #[test]
    fn a_task_needing_a_capability_nobody_has_stays_queued_with_a_reason() {
        let p = Policy::default();
        let mut d = demand();
        d.required_capabilities = vec!["multi-root".into()];
        let only = worker("a");
        assert!(pick(std::slice::from_ref(&only), &d, &p).is_none());
        assert_eq!(
            explain(&[only], &d, &p),
            vec![("a".to_string(), Reject::MissingCapability("multi-root".into()))]
        );
    }

    #[test]
    fn a_retry_never_returns_to_the_worker_that_failed() {
        // §6.2: infra retries must change machine or they just fail again.
        let p = Policy::default();
        let mut d = demand();
        d.excluded = vec!["a".into()];
        let mut failed = worker("a");
        failed.cached_worktrees = vec!["w1".into()];
        assert_eq!(pick(&[failed, worker("b")], &d, &p).unwrap().worker_id, "b");
    }

    #[test]
    fn hard_filters_exclude_before_scoring() {
        let p = Policy::default();
        let d = demand();

        let mut offline = worker("a");
        offline.status = "draining".into();
        assert_eq!(evaluate(&offline, &d, &p), Err(Reject::NotOnline));

        let mut wrong_arch = worker("b");
        wrong_arch.arch = "aarch64".into();
        assert_eq!(evaluate(&wrong_arch, &d, &p), Err(Reject::ArchMismatch));

        let mut full = worker("c");
        full.free_slots = 0;
        assert_eq!(evaluate(&full, &d, &p), Err(Reject::NoFreeSlot));

        let mut tiny = worker("d");
        tiny.disk_free_gb = 25; // below est(20) * 1.5
        assert_eq!(evaluate(&tiny, &d, &p), Err(Reject::InsufficientDisk));
    }

    #[test]
    fn an_unset_arch_matches_anything() {
        let p = Policy::default();
        let mut d = demand();
        d.arch = String::new();
        let mut arm = worker("a");
        arm.arch = "aarch64".into();
        assert!(evaluate(&arm, &d, &p).is_ok());
    }

    #[test]
    fn no_eligible_worker_yields_none_with_reasons() {
        let p = Policy::default();
        let mut full = worker("a");
        full.free_slots = 0;
        assert!(pick(&[full.clone()], &demand(), &p).is_none());
        assert_eq!(explain(&[full], &demand(), &p), vec![("a".to_string(), Reject::NoFreeSlot)]);
    }

    #[test]
    fn image_affinity_breaks_an_otherwise_even_tie() {
        let p = Policy::default();
        let plain = worker("b");
        let mut has_image = worker("c");
        has_image.cached_images = vec!["img@sha256:a".into()];
        assert_eq!(pick(&[plain, has_image], &demand(), &p).unwrap().worker_id, "c");
    }

    #[test]
    fn placement_is_deterministic_for_identical_workers() {
        let p = Policy::default();
        let a = pick(&[worker("b"), worker("a")], &demand(), &p).unwrap();
        let b = pick(&[worker("a"), worker("b")], &demand(), &p).unwrap();
        assert_eq!(a.worker_id, b.worker_id);
        assert_eq!(a.worker_id, "a");
    }

    #[test]
    fn weights_are_honoured() {
        let mut p = Policy::default();
        p.w_cache_affinity = 0.0;
        let mut cold = worker("a");
        cold.cpu_load = 0.0;
        let mut warm = worker("b");
        warm.cpu_load = 0.9;
        warm.cached_worktrees = vec!["w1".into()];
        // With cache affinity disabled the idle machine wins again.
        assert_eq!(pick(&[cold, warm], &demand(), &p).unwrap().worker_id, "a");
    }
}
