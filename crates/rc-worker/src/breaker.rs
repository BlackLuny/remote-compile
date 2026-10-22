//! Circuit breaker for repeated infrastructure failures (§6.2, §8.1).
//!
//! A worker that cannot create containers is worse than an absent one. Every
//! task it accepts becomes an infra error the control plane answers by retrying
//! elsewhere (§6.2), so the queue churns, each task pays a second dispatch, and
//! the real fault hides behind "retried on another machine".
//!
//! That is not hypothetical: a host whose CPUs had been pinned to one
//! (`systemctl set-property system.slice AllowedCPUs=0`) made
//! `docker create --cpus 4` fail with "Range of CPUs is from 0.01 to 1.00, as
//! there are only 1 CPUs available". Every task for 22 hours was accepted,
//! failed in two seconds and was re-dispatched — 202 of them — while the
//! console showed nothing but a worker with no output.
//!
//! The breaker takes such a worker out of the fleet for a cooldown and lets it
//! back in by itself. It is deliberately a *timed* box rather than a sticky
//! drain: the local drain flag has no Resume on the wire (see
//! `resume_worker` in rc-server, which can only clear the control plane's pin),
//! so a latching breaker would leave every affected machine needing a manual
//! restart.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Three in a row is a pattern, not noise: a transient blob or proxy failure
/// costs one retry, a broken sandbox costs all of them.
pub const DEFAULT_FAILURE_LIMIT: u32 = 3;
/// Long enough to stop the churn, short enough that a fixed host rejoins
/// without an operator. Recovery is automatic, so erring short is cheap.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub struct CircuitBreaker {
    limit: u32,
    cooldown: Duration,
    failures: AtomicU32,
    /// `Some(t)` while open; the breaker closes once `t` has passed.
    open_until: Mutex<Option<Instant>>,
}

impl CircuitBreaker {
    pub fn new(limit: u32, cooldown: Duration) -> Self {
        CircuitBreaker {
            limit: limit.max(1),
            cooldown,
            failures: AtomicU32::new(0),
            open_until: Mutex::new(None),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn cooldown(&self) -> Duration {
        self.cooldown
    }

    #[cfg(test)]
    pub fn failures(&self) -> u32 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Count one infrastructure failure.
    ///
    /// Returns `true` exactly once per opening, so the caller logs the cause a
    /// single time instead of on every task it happens to bounce.
    pub fn record_failure(&self) -> bool {
        // A failure reported after the breaker already opened (a task that was
        // in flight) must not extend the cooldown, or a busy worker could stay
        // out of the fleet forever.
        if self.is_open() {
            return false;
        }
        let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n < self.limit {
            return false;
        }
        self.failures.store(0, Ordering::Relaxed);
        let mut slot = self.open_until.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(Instant::now() + self.cooldown);
        true
    }

    /// A task that reached a result proves the sandbox works — including a
    /// compile error, which is the worker doing its job.
    pub fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }

    /// Whether the worker should currently refuse work.
    pub fn is_open(&self) -> bool {
        let mut slot = self.open_until.lock().unwrap_or_else(|e| e.into_inner());
        match *slot {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                // Cooldown elapsed: rejoin and give the sandbox a fresh streak.
                *slot = None;
                self.failures.store(0, Ordering::Relaxed);
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_breaker() -> CircuitBreaker {
        CircuitBreaker::new(3, Duration::from_secs(60))
    }

    #[test]
    fn stays_closed_below_the_limit() {
        let b = open_breaker();
        assert!(!b.record_failure()); // 1
        assert!(!b.record_failure()); // 2
        assert!(!b.is_open());
        assert_eq!(b.failures(), 2);
    }

    #[test]
    fn opens_on_the_limit_and_says_so_once() {
        let b = open_breaker();
        assert!(!b.record_failure());
        assert!(!b.record_failure());
        assert!(b.record_failure(), "third failure opens it");
        assert!(b.is_open());
        assert!(!b.record_failure(), "already open: do not log again");
        assert!(!b.record_failure());
    }

    #[test]
    fn a_result_clears_the_streak() {
        let b = open_breaker();
        b.record_failure();
        b.record_failure();
        b.record_success();
        assert!(!b.record_failure());
        assert!(!b.is_open(), "two failures, then a success, is not a pattern");
    }

    #[test]
    fn rejoins_by_itself_after_the_cooldown() {
        let b = CircuitBreaker::new(1, Duration::from_millis(20));
        assert!(b.record_failure());
        assert!(b.is_open());
        std::thread::sleep(Duration::from_millis(40));
        assert!(!b.is_open(), "cooldown expired; the worker rejoins unattended");
        assert_eq!(b.failures(), 0);
    }

    #[test]
    fn a_latent_failure_does_not_extend_the_cooldown() {
        let b = CircuitBreaker::new(2, Duration::from_millis(60));
        b.record_failure();
        assert!(b.record_failure());
        // A task that was already in flight reports late.
        std::thread::sleep(Duration::from_millis(40));
        assert!(!b.record_failure());
        std::thread::sleep(Duration::from_millis(30));
        assert!(!b.is_open(), "the late failure must not push the window out");
    }

    #[test]
    fn limit_is_at_least_one() {
        let b = CircuitBreaker::new(0, Duration::from_millis(10));
        assert_eq!(b.limit(), 1);
    }
}
