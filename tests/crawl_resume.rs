//! crash and resume with a real process: SIGKILL mid-crawl, restart
//! on the same data directory, and the frontier continues without re-fetching
//! what was already completed.

mod common;

use std::time::Duration;

use common::{Fixture, McpChild, start_fixture};

fn received(response: &serde_json::Value) -> &serde_json::Value {
 &response["result"]["structuredContent"]
}

async fn wait_for_requests(fixture: &Fixture, at_least: usize) {
 let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
 while fixture.log.count_matching("/bulk/") < at_least {
 assert!(
 tokio::time::Instant::now() < deadline,
 "fixture served only {} bulk pages",
 fixture.log.count_matching("/bulk/")
);
 tokio::time::sleep(Duration::from_millis(20)).await;
 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_mid_crawl_resumes_without_refetching_completed_urls() {
 let fixture = start_fixture().await;
 let dir = tempfile::tempdir().unwrap();
 // 2 s leases keep the recovery window short; the crawl itself is fast.
 let config = common::write_binary_config(dir.path(), "[crawl]\nlease_s = 2\nidle_poll_ms = 50");
 let data = dir.path().join("data");
 let seed = fixture.url("127.0.0.1", "/bulk/");
 let arguments = serde_json::json!({
 "start_urls": [seed],
 "render_policy": "never",
 "limits": { "max_pages": 120, "per_host_rate": 20.0, "max_duration_s": 120 },
 "name": "resume-test"
 });

 // --- run 1: start, wait for real progress, then SIGKILL ---------------
 let mut first = McpChild::spawn(&config, &data).await;
 let started = first.call("crawl_start", arguments.clone()).await;
 let job_id = received(&started)["job_id"]
.as_str()
.expect("job id")
.to_string();
 wait_for_requests(&fixture, 25).await;
 let before_kill: Vec<String> = fixture.log.entries();
 first.kill9().await;

 // --- run 2: same data dir, the job must resume ------------------------
 let mut second = McpChild::spawn(&config, &data).await;
 let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
 let terminal = loop {
 let status = second
.call("crawl_status", serde_json::json!({ "job_id": job_id }))
.await;
 let state = received(&status)["state"]
.as_str()
.unwrap_or_default()
.to_string();
 if !matches!(state.as_str(), "queued" | "running") {
 break status;
 }
 assert!(
 tokio::time::Instant::now() < deadline,
 "resumed job did not finish: {status}"
);
 tokio::time::sleep(Duration::from_millis(100)).await;
 };
 let status = received(&terminal);
 assert_eq!(status["state"], "done", "{status}");
 assert_eq!(status["counters"]["fetched"], 120, "{status}");
 assert_eq!(status["limits_hit"], serde_json::json!(["max_pages"]));
 assert_eq!(status["counters"]["failed"], 0, "{status}");

 // --- the URL set is complete and stable ------------------------------
 let mut cursor: Option<String> = None;
 let mut urls = std::collections::BTreeSet::new();
 loop {
 let page = second
.call(
 "crawl_results",
 serde_json::json!({ "job_id": job_id, "limit": 100, "cursor": cursor }))
.await;
 let payload = received(&page);
 for row in payload["results"].as_array().expect("results") {
 urls.insert(row["url"].as_str().unwrap_or_default().to_string());
 }
 match payload["next_cursor"].as_str() {
 Some(next) => cursor = Some(next.to_string()),
 None => break,
 }
 }
 assert_eq!(urls.len(), 120, "every crawled page has one result row");
 assert!(
 urls.iter().all(|url| url.contains("/bulk/")),
 "only bulk pages were crawled"
);

 // --- no completed URL was re-fetched ---------------------------------
 // One worker, so at most the page in flight at kill time is retried; a
 // completed page appearing twice would mean the frontier lost its state.
 let entries = fixture.log.entries();
 let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
 for entry in entries.iter() {
 if entry.contains("/bulk/") {
 *counts.entry(entry.as_str()).or_default() += 1;
 }
 }
 let repeated = counts.values().filter(|count| **count > 1).count();
 assert!(
 repeated <= 1,
 "at most the in-flight page may be retried, saw {repeated} repeats: {counts:?}"
);
 assert!(
 counts.len() >= 120,
 "every page was requested at least once"
);

 // The kill really happened before the end of the work.
 let served_before_kill = before_kill
.iter()
.filter(|entry| entry.contains("/bulk/"))
.count();
 assert!(
 (25..120).contains(&served_before_kill),
 "kill landed mid-crawl: {served_before_kill} served"
);

 second.shutdown().await;
}
