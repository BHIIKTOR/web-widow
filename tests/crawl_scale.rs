//!   — 500-page crawl through the single writer: page latency, write
//! overhead and the absence of lock errors. A reference measurement, not a
//! budget (§8 discipline).

mod common;

use std::time::{Duration, Instant};

use common::{TestEnv, start_fixture, test_config};
use web_widow::crawl::{CrawlLimitsInput, CrawlRenderPolicy, CrawlStartInput};

fn percentile(mut samples: Vec<f64>, percentile: f64) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if samples.is_empty() {
        return 0.0;
    }
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index.min(samples.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_hundred_page_crawl_reports_latency_and_no_lock_errors() {
    let fixture = start_fixture().await;
    let mut config = test_config();
    // The fixture is local: keep the politeness floor low and the job rate high
    // so this measures the engine, not a deliberate wait.
    config.fetch.min_interval_ms = 1;
    config.crawl.lease_s = 10;
    config.crawl.idle_poll_ms = 20;
    let env = TestEnv::new(config).await;

    let seed = fixture.url("127.0.0.1", "/bulk/");
    let started = env
        .crawler
        .start(CrawlStartInput {
            start_urls: vec![seed],
            sitemap_urls: Vec::new(),
            scope: None,
            schema: None,
            render_policy: Some(CrawlRenderPolicy::Never),
            limits: Some(CrawlLimitsInput {
                max_pages: Some(500),
                per_host_rate: Some(200.0),
                max_duration_s: Some(180),
            }),
            name: Some("scale".to_string()),
        })
        .await
        .expect("crawl starts");

    let wall = Instant::now();
    let status = loop {
        let status = env.crawler.status(&started.job_id).await.expect("status");
        if !matches!(status.state.as_str(), "queued" | "running") {
            break status;
        }
        assert!(
            wall.elapsed() < Duration::from_secs(180),
            "500-page crawl did not finish: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let wall = wall.elapsed();

    assert_eq!(status.state, "done", "{status:?}");
    assert_eq!(status.counters.fetched, 500, "{status:?}");
    assert_eq!(status.counters.failed, 0, "{status:?}");
    assert!(
        status.error_samples.is_empty(),
        "no errors expected on a healthy fixture: {:?}",
        status.error_samples
    );

    // Page latency comes from the stored fetch events, not from wall time.
    let db = env.db();
    let mut statement = db
        .prepare(
            "SELECT fe.duration_ms
             FROM fetch_events fe
             JOIN pages p ON p.id = fe.page_id
             JOIN frontier f ON f.url_hash = p.url_hash
             WHERE f.job_id = ?1 AND fe.mode = 'crawl' AND fe.http_status = 200",
        )
        .unwrap();
    let durations: Vec<f64> = statement
        .query_map([&started.job_id], |row| row.get::<_, i64>(0))
        .unwrap()
        .flatten()
        .map(|ms| ms as f64)
        .collect();
    assert_eq!(durations.len(), 500, "one duration per fetched page");

    let p50 = percentile(durations.clone(), 0.50);
    let p95 = percentile(durations.clone(), 0.95);
    let sum: f64 = durations.iter().sum();
    let wall_ms = wall.as_millis() as f64;
    // Everything that is not the fetch itself is scheduling plus the SQLite
    // writer (pages, events, blobs row, counters and frontier updates).
    let persistence_ms_per_page = ((wall_ms - sum) / 500.0).max(0.0);

    println!(
        "CRAWL_PERF pages=500 wall_ms={:.0} page_p50_ms={p50:.1} page_p95_ms={p95:.1} \\
         fetch_sum_ms={sum:.0} persistence_ms_per_page={persistence_ms_per_page:.2}",
        wall_ms
    );

    assert!(
        p95 < 2000.0,
        "page p95 {p95:.1} ms exceeded the reference ceiling"
    );

    // The fixture has 501 URLs (index + 500 pages), so a 500-page limit stops
    // with exactly one URL unfetched — reported, not silently dropped.
    let settled = env
        .crawler
        .status(&started.job_id)
        .await
        .expect("status after finish");
    assert_eq!(settled.limits_hit, vec!["max_pages".to_string()]);
    assert_eq!(
        settled.counters.queued, 1,
        "the 501st URL is reported, not hidden"
    );
    let mut cursor: Option<String> = None;
    let mut rows = 0usize;
    loop {
        let page = env
            .crawler
            .results(web_widow::crawl::CrawlResultsInput {
                job_id: started.job_id.clone(),
                limit: Some(500),
                cursor,
            })
            .await
            .expect("results");
        rows += page.results.len();
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(rows, 500);
}
