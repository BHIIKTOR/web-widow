//! Per-host politeness gate: at most N in flight, and a minimum interval
//! between request starts.
//!
//! The interval is reserved at acquisition time, so concurrent callers queue
//! instead of all measuring "now". `Crawl-delay` raises the interval and can
//! never lower it (IMP-SAFE-001). Elapsed time uses `tokio::time::Instant` so
//! the paused-clock tests exercise the same clock as production.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::core::robots::effective_interval;

#[derive(Debug, Clone, Copy)]
pub struct PolitenessConfig {
    pub min_interval: Duration,
    pub jitter: Duration,
    pub max_concurrent: usize,
}

impl Default for PolitenessConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(1),
            jitter: Duration::from_millis(500),
            max_concurrent: 1,
        }
    }
}

/// Per-host timing state. `last_start` lets a newly learned `Crawl-delay` apply
/// to the *next* request even though the interval was shorter when the previous
/// request was admitted.
#[derive(Debug, Default)]
struct HostTiming {
    last_start: Option<Instant>,
    next_allowed: Option<Instant>,
}

#[derive(Debug)]
struct HostSlot {
    timing: Mutex<HostTiming>,
    permits: Arc<Semaphore>,
}

/// Held for the duration of one request; dropping it frees the concurrency slot.
#[derive(Debug)]
pub struct HostLease {
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct HostGate {
    policy: PolitenessConfig,
    slots: Mutex<HashMap<String, Arc<HostSlot>>>,
    counter: AtomicU64,
    seed: RandomState,
}

impl HostGate {
    pub fn new(policy: PolitenessConfig) -> Self {
        Self {
            policy,
            slots: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            seed: RandomState::new(),
        }
    }

    fn slot(&self, host: &str) -> Arc<HostSlot> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(slots.entry(host.to_string()).or_insert_with(|| {
            Arc::new(HostSlot {
                timing: Mutex::new(HostTiming::default()),
                permits: Arc::new(Semaphore::new(self.policy.max_concurrent)),
            })
        }))
    }

    fn jitter(&self) -> Duration {
        if self.policy.jitter.is_zero() {
            return Duration::ZERO;
        }
        let mut hasher = self.seed.build_hasher();
        hasher.write_u64(self.counter.fetch_add(1, Ordering::Relaxed));
        let span = self.policy.jitter.as_millis().max(1) as u64;
        Duration::from_millis(hasher.finish() % (span + 1))
    }

    /// Wait for a slot on `host`. The returned lease must be held until the
    /// request (including its body read) finishes.
    pub async fn acquire(&self, host: &str, crawl_delay: Option<Duration>) -> HostLease {
        let slot = self.slot(host);
        let permit = Arc::clone(&slot.permits)
            .acquire_owned()
            .await
            .expect("host gate semaphore is never closed");
        let interval = effective_interval(self.policy.min_interval, crawl_delay);
        let start = {
            let mut timing = slot
                .timing
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let mut floor = timing.next_allowed.unwrap_or(now);
            if let Some(last) = timing.last_start {
                floor = floor.max(last + interval);
            }
            let start = floor.max(now);
            timing.last_start = Some(start);
            timing.next_allowed = Some(start + interval + self.jitter());
            start
        };
        tokio::time::sleep_until(start).await;
        HostLease { _permit: permit }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(min_interval_ms: u64, jitter_ms: u64, concurrent: usize) -> HostGate {
        HostGate::new(PolitenessConfig {
            min_interval: Duration::from_millis(min_interval_ms),
            jitter: Duration::from_millis(jitter_ms),
            max_concurrent: concurrent,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn spaces_request_starts_on_the_same_host() {
        let gate = gate(100, 0, 1);
        let started = Instant::now();
        let first = gate.acquire("example.com", None).await;
        let after_first = started.elapsed();
        drop(first);
        let second = gate.acquire("example.com", None).await;
        let after_second = started.elapsed();
        drop(second);
        assert!(after_first < Duration::from_millis(50), "{after_first:?}");
        assert!(
            after_second >= Duration::from_millis(100),
            "{after_second:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn different_hosts_do_not_wait_for_each_other() {
        let gate = gate(500, 0, 1);
        let started = Instant::now();
        let first = gate.acquire("a.example", None).await;
        let second = gate.acquire("b.example", None).await;
        let elapsed = started.elapsed();
        drop(first);
        drop(second);
        assert!(elapsed < Duration::from_millis(50), "{elapsed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn crawl_delay_raises_the_interval() {
        let gate = gate(100, 0, 1);
        let started = Instant::now();
        let first = gate
            .acquire("slow.example", Some(Duration::from_millis(700)))
            .await;
        drop(first);
        let second = gate
            .acquire("slow.example", Some(Duration::from_millis(700)))
            .await;
        drop(second);
        assert!(
            started.elapsed() >= Duration::from_millis(700),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_limit_serializes_in_flight_requests() {
        let gate = Arc::new(gate(0, 0, 1));
        let first = gate.acquire("one.example", None).await;
        let second = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire("one.example", None).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "second lease issued while first held"
        );
        drop(first);
        let second = second.await.unwrap();
        drop(second);
    }

    #[tokio::test(start_paused = true)]
    async fn jitter_stays_within_the_configured_span() {
        let gate = gate(0, 40, 1);
        let mut maximum = Duration::ZERO;
        for _ in 0..50 {
            let jitter = gate.jitter();
            assert!(jitter <= Duration::from_millis(40), "{jitter:?}");
            maximum = maximum.max(jitter);
        }
        assert!(maximum > Duration::ZERO, "jitter never produced a delay");
    }
}
