//! A11 — performance smoke against the fixture server: 50 sequential and 8
//! concurrent fetches of a ~100 KiB page. Reference, not a budget: the numbers
//! are a regression baseline for later batches (§8).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

fn percentile(mut samples: Vec<Duration>, percentile: f64) -> Duration {
    samples.sort();
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index.min(samples.len() - 1)]
}

fn peak_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let value = rest.trim().trim_end_matches(" kB").trim();
            return value.parse().unwrap_or(0);
        }
    }
    0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn performance_smoke_reports_p50_p95_and_peak_rss() {
    let fixture = common::start_fixture().await;
    let env = Arc::new(common::TestEnv::new(common::test_config()).await);
    let url = fixture.url("127.0.0.1", "/large");

    // Warm-up: robots.txt, connection pool, first parse.
    env.fetch(&url).await.expect("warm-up fetch");

    let mut sequential = Vec::with_capacity(50);
    for _ in 0..50 {
        let started = Instant::now();
        let result = env.fetch(&url).await.expect("sequential fetch");
        assert!(result.payload.markdown_chars > 5_000);
        sequential.push(started.elapsed());
    }

    let concurrent_started = Instant::now();
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let env = Arc::clone(&env);
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let started = Instant::now();
            let result = env.fetch(&url).await.expect("concurrent fetch");
            assert!(result.payload.markdown_chars > 5_000);
            started.elapsed()
        }));
    }
    let mut concurrent = Vec::with_capacity(8);
    for handle in handles {
        concurrent.push(handle.await.expect("task should not panic"));
    }
    let concurrent_wall = concurrent_started.elapsed();

    let sequential_p50 = percentile(sequential.clone(), 0.50);
    let sequential_p95 = percentile(sequential, 0.95);
    let concurrent_p50 = percentile(concurrent.clone(), 0.50);
    let concurrent_p95 = percentile(concurrent, 0.95);
    let rss = peak_rss_kb();

    println!(
        "PERF sequential_p50_ms={} sequential_p95_ms={} concurrent_p50_ms={} concurrent_p95_ms={} concurrent_wall_ms={} peak_rss_kb={} build={}",
        sequential_p50.as_millis(),
        sequential_p95.as_millis(),
        concurrent_p50.as_millis(),
        concurrent_p95.as_millis(),
        concurrent_wall.as_millis(),
        rss,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );

    // Reference ceiling from §8, applied to the local fixture.
    assert!(
        sequential_p95 < Duration::from_secs(2),
        "sequential p95 {:?} exceeded the 2 s reference",
        sequential_p95
    );
    assert!(
        concurrent_p95 < Duration::from_secs(2),
        "concurrent p95 {:?} exceeded the 2 s reference",
        concurrent_p95
    );
    assert!(rss > 0, "peak RSS should be readable from /proc");
}
