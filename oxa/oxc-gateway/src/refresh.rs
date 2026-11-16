//! Caller-driven refresh scheduling; no I/O, threads, timers, or request-path work.
//!
//! A background owner polls `try_start`, performs the refresh separately, and calls
//! `finish` exactly once. Request handling may only mark a credit/budget refusal;
//! it must never start or await refresh I/O. Use one throttle per cached provider.

use std::time::{Duration, Instant};

/// Result of a completed refresh (including cancellation as a failure).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    Success,
    Failure,
}

/// A single-owner state machine with an injected monotonic clock.
///
/// Ordinary refreshes are spaced from successful completion. Credit/budget
/// refusals bypass that interval, but never a failure backoff or an active refresh.
/// Multiple refusals coalesce; a refusal during a refresh schedules one follow-up.
/// No work runs unless the owner explicitly calls `try_start`.
pub struct RefreshThrottle<C> {
    clock: C,
    interval: Duration,
    backoff: Duration,
    finished: Option<(Instant, Duration, bool)>,
    in_flight: bool,
    refused: bool,
}

impl<C: Fn() -> Instant> RefreshThrottle<C> {
    /// Initially due. `interval` and `backoff` must be nonzero.
    /// The injected clock must be monotonic; tests can advance it without sleeping.
    ///
    /// # Panics
    /// Panics if either duration is zero.
    pub fn new(interval: Duration, backoff: Duration, clock: C) -> Self {
        assert!(!interval.is_zero(), "refresh interval must be nonzero");
        assert!(!backoff.is_zero(), "refresh backoff must be nonzero");
        Self {
            clock,
            interval,
            backoff,
            finished: None,
            in_flight: false,
            refused: false,
        }
    }

    /// Record a credit or budget refusal without doing I/O or reading the clock.
    pub fn mark_refused(&mut self) {
        self.refused = true;
    }

    /// Claim the only refresh slot if due; call only from the background owner.
    /// A `true` result must be paired with `finish`, even on failure/cancellation.
    pub fn try_start(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        if let Some((at, delay, failed)) = self.finished {
            let elapsed = (self.clock)().checked_duration_since(at).unwrap_or_default();
            if elapsed < delay && (failed || !self.refused) {
                return false;
            }
        }
        self.in_flight = true;
        self.refused = false;
        true
    }

    /// Release the slot and start the interval or fixed failure backoff now.
    /// Refusals cannot shorten or extend failure backoff. Returns `false` without
    /// changing anything when there is no refresh in flight.
    pub fn finish(&mut self, outcome: RefreshOutcome) -> bool {
        if !self.in_flight {
            return false;
        }
        let failed = outcome == RefreshOutcome::Failure;
        let delay = if failed { self.backoff } else { self.interval };
        self.finished = Some(((self.clock)(), delay, failed));
        self.in_flight = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    const INTERVAL: Duration = Duration::from_secs(300);
    const BACKOFF: Duration = Duration::from_secs(30);

    #[test]
    fn refresh_initially_due_and_only_one_in_flight() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(throttle.try_start());
        now.set(now.get() + INTERVAL);
        assert!(!throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.finish(RefreshOutcome::Failure));
    }

    #[test]
    fn refresh_interval_starts_at_successful_completion() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(throttle.try_start());
        now.set(now.get() + Duration::from_secs(10));
        assert!(throttle.finish(RefreshOutcome::Success));
        now.set(now.get() + INTERVAL - Duration::from_nanos(1));
        assert!(!throttle.try_start());
        now.set(now.get() + Duration::from_nanos(1));
        assert!(throttle.try_start());
    }

    #[test]
    fn refresh_refusal_makes_cached_success_due_immediately() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.try_start());
        throttle.mark_refused();
        assert!(throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.try_start());
    }

    #[test]
    fn refresh_refusal_during_flight_is_not_lost_or_started_concurrently() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(throttle.try_start());
        throttle.mark_refused();
        throttle.mark_refused();
        assert!(!throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.try_start());
    }

    #[test]
    fn refresh_failure_backs_off_even_when_refusals_repeat() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(throttle.try_start());
        throttle.mark_refused();
        assert!(throttle.finish(RefreshOutcome::Failure));
        throttle.mark_refused();
        assert!(!throttle.try_start());
        now.set(now.get() + BACKOFF - Duration::from_nanos(1));
        throttle.mark_refused();
        assert!(!throttle.try_start());
        now.set(now.get() + Duration::from_nanos(1));
        assert!(throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Failure));
        assert!(!throttle.try_start());
        now.set(now.get() + BACKOFF);
        assert!(throttle.try_start());
        assert!(throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.try_start());
    }

    #[test]
    fn refresh_completion_without_start_does_not_delay_initial_refresh() {
        let now = Cell::new(Instant::now());
        let mut throttle = RefreshThrottle::new(INTERVAL, BACKOFF, || now.get());
        assert!(!throttle.finish(RefreshOutcome::Success));
        assert!(!throttle.finish(RefreshOutcome::Failure));
        assert!(throttle.try_start());
    }
}
