//! Restart budget for the shared browser process.
//!
//! Three restarts inside ten minutes, then `render_unavailable` until the window
//! has drained. The clock is injected so the policy is testable without waiting.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The restart budget plus the one distinction it must not blur: the **first**
/// launch is not a restart. `restart_max = 0` therefore means "no relaunches",
/// not "rendering unavailable" (rider on the disposition).
#[derive(Debug)]
pub struct LaunchGate {
    launched_once: bool,
    budget: RestartBudget,
}

impl LaunchGate {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            launched_once: false,
            budget: RestartBudget::new(max, window),
        }
    }

    /// May a launch be attempted now? The first attempt always may; later ones
    /// need budget left in the window.
    pub fn may_launch(&mut self, now: Instant) -> bool {
        if !self.launched_once {
            return true;
        }
        !self.budget.exhausted(now)
    }

    /// A launch attempt has started: from now on every attempt is a relaunch.
    pub fn mark_attempted(&mut self) {
        self.launched_once = true;
    }

    /// A launch or an established session failed; consumes one restart slot.
    /// Returns false when the budget was already exhausted (no relaunch).
    pub fn note_failure(&mut self, now: Instant) -> bool {
        self.launched_once = true;
        self.budget.record(now)
    }

    pub fn restarts_used(&mut self, now: Instant) -> usize {
        self.budget.used(now)
    }
}

#[derive(Debug)]
pub struct RestartBudget {
    max: usize,
    window: Duration,
    restarts: VecDeque<Instant>,
}

impl RestartBudget {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            restarts: VecDeque::new(),
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(oldest) = self.restarts.front() {
            match now.checked_duration_since(*oldest) {
                Some(age) if age >= self.window => {
                    self.restarts.pop_front();
                }
                _ => break,
            }
        }
    }

    /// Restarts recorded inside the current window.
    pub fn used(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.restarts.len()
    }

    pub fn exhausted(&mut self, now: Instant) -> bool {
        self.used(now) >= self.max
    }

    /// Record one restart. Returns false when the budget is already exhausted;
    /// the caller must not launch in that case.
    pub fn record(&mut self, now: Instant) -> bool {
        self.prune(now);
        if self.restarts.len() >= self.max {
            return false;
        }
        self.restarts.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_restarts_are_allowed_then_the_fourth_is_refused() {
        let now = Instant::now();
        let mut budget = RestartBudget::new(3, Duration::from_secs(600));
        assert!(budget.record(now));
        assert!(budget.record(now + Duration::from_secs(1)));
        assert!(budget.record(now + Duration::from_secs(2)));
        assert!(budget.exhausted(now + Duration::from_secs(3)));
        assert!(!budget.record(now + Duration::from_secs(4)));
        assert_eq!(budget.used(now + Duration::from_secs(4)), 3);
    }

    #[test]
    fn the_window_drains_and_allows_a_restart_again() {
        let now = Instant::now();
        let mut budget = RestartBudget::new(3, Duration::from_secs(600));
        for offset in [0, 1, 2] {
            assert!(budget.record(now + Duration::from_secs(offset)));
        }
        assert!(budget.exhausted(now + Duration::from_secs(10)));
        assert!(!budget.exhausted(now + Duration::from_secs(600)));
        assert!(budget.record(now + Duration::from_secs(601)));
    }

    #[test]
    fn the_first_launch_is_allowed_even_with_a_zero_budget() {
        let now = Instant::now();
        let mut gate = LaunchGate::new(0, Duration::from_secs(60));
        assert!(
            gate.may_launch(now),
            "first launch must not consult the budget"
        );
        gate.mark_attempted();
        assert!(
            !gate.may_launch(now + Duration::from_secs(1)),
            "a relaunch with no budget must be refused"
        );
    }

    #[test]
    fn a_failed_first_launch_consumes_nothing_but_makes_later_attempts_relaunches() {
        let now = Instant::now();
        let mut gate = LaunchGate::new(0, Duration::from_secs(60));
        assert!(gate.may_launch(now));
        gate.mark_attempted();
        assert!(!gate.note_failure(now));
        assert!(!gate.may_launch(now + Duration::from_secs(1)));
        assert_eq!(gate.restarts_used(now + Duration::from_secs(1)), 0);
    }

    #[test]
    fn three_relaunches_then_refused_and_a_drained_window_allows_one_again() {
        let now = Instant::now();
        let mut gate = LaunchGate::new(3, Duration::from_secs(600));
        assert!(gate.may_launch(now));
        gate.mark_attempted();
        for offset in 0..3 {
            assert!(gate.may_launch(now + Duration::from_secs(offset)));
            assert!(gate.note_failure(now + Duration::from_secs(offset)));
        }
        assert!(!gate.may_launch(now + Duration::from_secs(10)));
        assert!(!gate.note_failure(now + Duration::from_secs(11)));
        assert!(gate.may_launch(now + Duration::from_secs(600)));
    }

    #[test]
    fn zero_max_refuses_every_restart() {
        let now = Instant::now();
        let mut budget = RestartBudget::new(0, Duration::from_secs(60));
        assert!(budget.exhausted(now));
        assert!(!budget.record(now));
    }
}
