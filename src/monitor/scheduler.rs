//! Opt-in in-process monitor scheduler (M07 S1, IMP-DEC-048).
//!
//! The M01 decision chose no scheduler; IMP-ITEM-006 amends it with a resident
//! timer that is **off by default**, runs only in `serve`, and drives the
//! existing [`MonitorManager::run_due`] pass. Nothing in `check_one`/`run_due`
//! changes: the per-monitor lock and the `next_due_at` re-read keep a timer pass,
//! a `monitor_check` and `run-due --http` from double-checking one monitor.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Config;

use super::MonitorManager;

/// The per-tick pass bound: `[scheduler].max_checks_per_tick` clamped by
/// `[monitor].max_checks_per_call` (both validated >= 1).
pub fn pass_limit(config: &Config) -> usize {
    config
        .scheduler
        .max_checks_per_tick
        .min(config.monitor.max_checks_per_call)
        .max(1)
}

/// Spawn the timer task. The caller spawns it only in `serve` and only when
/// `[scheduler].enabled`; the defensive check makes a stray call a no-op.
///
/// One pass per fixed tick (`MissedTickBehavior::Delay`); `run_due` orders by
/// `next_due_at`, checks sequentially, and advances each monitor from its own
/// completion, so a late tick shifts the next check instead of queueing extras.
/// No catch-up burst: after downtime a monitor is simply due once.
pub fn spawn(
    monitors: Arc<MonitorManager>,
    config: Arc<Config>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let enabled = config.scheduler.enabled;
    let tick = Duration::from_millis(config.scheduler.tick_ms.max(1_000));
    let limit = pass_limit(&config);
    tracing::info!(
        enabled,
        tick_ms = tick.as_millis() as u64,
        max_checks_per_tick = limit,
        "monitor scheduler task"
    );
    tokio::spawn(async move {
        if !enabled {
            tracing::info!("monitor scheduler disabled; task exits");
            return;
        }
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("monitor scheduler stopped");
                    break;
                }
                _ = ticker.tick() => {
                    match monitors.run_due(limit).await {
                        Ok(summary) => tracing::debug!(
                            checked = summary.checked,
                            skipped = summary.skipped,
                            changed = summary.changed.len(),
                            failed = summary.failed.len(),
                            "monitor scheduler pass complete"
                        ),
                        Err(error) => {
                            tracing::warn!(error = %error, "monitor scheduler pass failed");
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pass_bound_is_the_minimum_of_both_knobs() {
        let mut config = Config::default();
        config.scheduler.max_checks_per_tick = 5;
        config.monitor.max_checks_per_call = 2;
        assert_eq!(pass_limit(&config), 2);
        config.monitor.max_checks_per_call = 500;
        assert_eq!(pass_limit(&config), 5);
        config.scheduler.max_checks_per_tick = 500;
        config.monitor.max_checks_per_call = 25;
        assert_eq!(pass_limit(&config), 25);
    }
}
