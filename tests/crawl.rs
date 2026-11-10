//! B04 — crawl traversal, dedup, politeness, limits, trap resistance,
//! cancellation and the mining path (IMP-…B4-4, B4-6…B4-9).
//!
//! Hermetic: every seed is the loopback fixture server. Crash/resume (B4-5) and
//! the 500-page latency run (B4-10) live in their own files because they need a
//! real process lifecycle and a longer budget.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::{Fixture, TestEnv, start_fixture, test_config};
use serde_json::json;
use web_widow::ErrorCode;
use web_widow::crawl::{
 CrawlLimitsInput, CrawlRenderPolicy, CrawlResultsInput, CrawlScopeInput, CrawlStartInput,
 CrawlStatusPayload,
};

fn start_input(urls: Vec<String>) -> CrawlStartInput {
 CrawlStartInput {
 start_urls: urls,
 sitemap_urls: Vec::new(),
 scope: None,
 schema: None,
 render_policy: Some(CrawlRenderPolicy::Never),
 limits: Some(CrawlLimitsInput {
 max_pages: None,
 per_host_rate: Some(50.0),
 max_duration_s: None,
 }),
 name: Some("fixture".to_string()),
 }
}

async fn env_with(patch: impl FnOnce(&mut web_widow::config::Config)) -> (Fixture, TestEnv) {
 let fixture = start_fixture().await;
 let mut config = test_config();
 config.crawl.lease_s = 2;
 config.crawl.idle_poll_ms = 25;
 patch(&mut config);
 (fixture, TestEnv::new(config).await)
}

async fn wait_terminal(env: &TestEnv, job_id: &str) -> CrawlStatusPayload {
 let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
 loop {
 let status = env.crawler.status(job_id).await.expect("status");
 if !matches!(status.state.as_str(), "queued" | "running") {
 return status;
 }
 assert!(
 tokio::time::Instant::now() < deadline,
 "crawl job did not finish: {status:?}"
);
 tokio::time::sleep(Duration::from_millis(25)).await;
 }
}

/// Every result row, following cursors (also proves pagination round-trips).
async fn all_results(env: &TestEnv, job_id: &str, limit: usize) -> (Vec<serde_json::Value>, usize) {
 let mut rows = Vec::new();
 let mut cursor: Option<String> = None;
 let mut pages = 0usize;
 loop {
 let page = env
.crawler
.results(CrawlResultsInput {
 job_id: job_id.to_string(),
 limit: Some(limit),
 cursor: cursor.clone(),
 })
.await
.expect("results page");
 pages += 1;
 for row in page.results {
 rows.push(serde_json::to_value(row).unwrap());
 }
 match page.next_cursor {
 Some(next) => cursor = Some(next),
 None => break,
 }
 assert!(pages < 100, "cursor loop");
 }
 (rows, pages)
}

fn urls(rows: &[serde_json::Value]) -> BTreeSet<String> {
 rows.iter()
.map(|row| row["url"].as_str().unwrap_or_default().to_string())
.collect()
}

// ---------------------------------------------------------------------------
// B4-1 — traversal and the exact fetched URL set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_site_fetches_exactly_the_expected_url_set() {
 let (fixture, env) = env_with(|_| {}).await;
 let seed = fixture.url("127.0.0.1", "/site/");
 let started = env
.crawler
.start(start_input(vec![seed.clone()]))
.await
.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;

 assert_eq!(status.state, "done", "{status:?}");
 assert!(status.limits_hit.is_empty(), "{:?}", status.limits_hit);

 let (rows, _) = all_results(&env, &started.job_id, 100).await;
 let mut expected: BTreeSet<String> = BTreeSet::new();
 expected.insert(seed.clone());
 for index in 1..=40 {
 expected.insert(fixture.url("127.0.0.1", &format!("/site/p{index}")));
 }
 for path in [
 "/site/page/2",
 "/site/dup-a",
 "/site/dup-b",
 "/site/dup-c",
 "/site/dup-next",
 "/site/redirect/1",
 "/site/404",
 "/site/500-once",
 "/private/secret",
 "/site/redirect-private",
 ] {
 expected.insert(fixture.url("127.0.0.1", path));
 }
 if urls(&rows) != expected {
 eprintln!("STATUS {status:?}");
 eprintln!("LOG {:?}", fixture.log.entries());
 eprintln!("ROWS {:?}", urls(&rows));
 }
 assert_eq!(urls(&rows), expected, "fetched URL set differs");

 // The 404 is the only terminal failure; the 500-once page recovered.
 assert_eq!(status.counters.failed, 1, "{status:?}");
 let missing = rows
.iter()
.find(|row| row["url"] == json!(fixture.url("127.0.0.1", "/site/404")))
.expect("404 row");
 assert_eq!(missing["status"], json!(404));
 assert!(missing["error"].as_str().is_some());
 assert_eq!(
 fixture.log.count_matching("/site/500-once"),
 2,
 "one 500 then a 200"
);
 // Duplicate variants collapse: only the first of the three follows its link.
 assert_eq!(fixture.log.count_matching("/site/dup-next"), 1);
 assert!(
 status.counters.skipped >= 2,
 "the duplicate variants and repeat links are skipped: {status:?}"
);
 assert_eq!(
 fixture.log.count_matching("/site/dup-b") + fixture.log.count_matching("/site/dup-c"),
 2,
 "the duplicate variants are fetched once each, then collapsed"
);
}

// ---------------------------------------------------------------------------
// B4-2 — canonicalization is enforced at the frontier
// ---------------------------------------------------------------------------

#[tokio::test]
async fn canonical_variants_fetch_once_and_trailing_slash_is_distinct() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.log.clear();
 let base = fixture.url("127.0.0.1", "/site/variant");
 let seeds = vec![
 format!("{base}?utm_source=news&utm_campaign=x"),
 base.clone(),
 format!("{base}?b=2&a=1"),
 format!("{base}?a=1&b=2"),
 format!("{base}/"),
 ];
 let started = env
.crawler
.start(start_input(seeds))
.await
.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "done");

 // Three distinct resources: the bare path (bare + utm seeds collapse), one
 // query resource (the two order variants collapse), and the trailing slash.
 let (rows, _) = all_results(&env, &started.job_id, 50).await;
 assert_eq!(
 rows.len(),
 3,
 "expected bare, ordered-query and trailing-slash rows: {:?} log {:?}",
 urls(&rows),
 fixture.log.entries()
);
 let fetched = urls(&rows);
 assert!(fetched.contains(&base));
 assert!(fetched.contains(&format!("{base}/")));
 assert!(fetched.contains(&format!("{base}?a=1&b=2")));
 assert_eq!(
 fixture.log.count_matching("/site/variant?a=1&b=2"),
 1,
 "query-order variants must reach the server once: {:?}",
 fixture.log.entries()
);
 assert_eq!(
 fixture
.log
.entries()
.iter()
.filter(|entry| entry.ends_with("/site/variant"))
.count(),
 1,
 "tracking-parameter variants must reach the server once: {:?}",
 fixture.log.entries()
);
 assert_eq!(fixture.log.count_matching("/site/variant/"), 1);
}

// ---------------------------------------------------------------------------
// B4-3 — robots survives the crawl, including redirects into a blocked subtree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn robots_disallowed_subtree_is_never_requested() {
 // 127.0.0.2 publishes `Disallow: /private`.
 let (fixture, env) = env_with(|_| {}).await;
 fixture.log.clear();
 let seed = fixture.url("127.0.0.2", "/site/");
 let started = env
.crawler
.start(start_input(vec![seed]))
.await
.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "done", "{status:?}");

 assert!(
 !fixture.log.any_matching("/private/secret"),
 "a disallowed path must not be requested: {:?}",
 fixture.log.entries()
);
 assert!(
 status.counters.skipped >= 2,
 "the direct link and the redirect target are skipped: {status:?}"
);
 let (rows, _) = all_results(&env, &started.job_id, 100).await;
 assert!(
 !urls(&rows).iter().any(|url| url.contains("/private/")),
 "no result row for a robots-blocked URL"
);
 // The redirect landed on the blocked path, so its own row is skipped too.
 assert!(!urls(&rows).contains(&fixture.url("127.0.0.2", "/site/redirect-private")));
}

// ---------------------------------------------------------------------------
// B4-4 — per-host rate applies to the crawl, measured server-side
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_host_interval_holds_across_a_hundred_requests() {
 let (fixture, env) = env_with(|_| {}).await;
 let seed = fixture.url("127.0.0.1", "/bulk/");
 let mut input = start_input(vec![seed]);
 input.limits = Some(CrawlLimitsInput {
 max_pages: Some(100),
 per_host_rate: Some(20.0), // 50 ms interval
 max_duration_s: Some(60),
 });
 let started = env.crawler.start(input).await.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "done");
 assert_eq!(status.counters.fetched, 100, "{status:?}");

 let stamps = fixture.log.stamps_matching("/bulk/");
 assert!(
 stamps.len() >= 100,
 "expected 100 page requests, got {}",
 stamps.len()
);
 let gaps: Vec<Duration> = stamps
.windows(2)
.map(|pair| pair[1].duration_since(pair[0]))
.collect();
 let too_fast = gaps
.iter()
.filter(|gap| **gap < Duration::from_millis(45))
.count();
 assert_eq!(
 too_fast,
 0,
 "per-host interval violated in {too_fast} of {} gaps",
 gaps.len()
);
 let mut sorted = gaps.clone();
 sorted.sort();
 let median = sorted[sorted.len() / 2];
 assert!(
 median >= Duration::from_millis(45) && median < Duration::from_millis(400),
 "median gap {median:?} is not the configured 50 ms"
);
}

// ---------------------------------------------------------------------------
// B4-6 — max_pages stops cleanly and results paginate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_pages_stops_cleanly_and_pagination_round_trips() {
 let (fixture, env) = env_with(|_| {}).await;
 let seed = fixture.url("127.0.0.1", "/bulk/");
 let mut input = start_input(vec![seed]);
 input.limits = Some(CrawlLimitsInput {
 max_pages: Some(10),
 per_host_rate: Some(50.0),
 max_duration_s: None,
 });
 let started = env.crawler.start(input).await.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;

 assert_eq!(status.state, "done");
 assert_eq!(status.counters.fetched, 10, "{status:?}");
 assert_eq!(status.limits_hit, vec!["max_pages".to_string()]);
 assert!(
 status.counters.queued > 0,
 "remaining frontier is reported as outstanding: {status:?}"
);

 let (rows, pages) = all_results(&env, &started.job_id, 4).await;
 assert_eq!(rows.len(), 10);
 assert_eq!(pages, 3, "4 + 4 + 2");

 // A cursor from another job is not_found, never a silent restart.
 let other = env
.crawler
.start(start_input(vec![fixture.url("127.0.0.1", "/site/p1")]))
.await
.expect("second job");
 wait_terminal(&env, &other.job_id).await;
 let first_page = env
.crawler
.results(CrawlResultsInput {
 job_id: started.job_id.clone(),
 limit: Some(1),
 cursor: None,
 })
.await
.unwrap();
 let cursor = first_page.next_cursor.expect("a full page yields a cursor");
 let error = env
.crawler
.results(CrawlResultsInput {
 job_id: other.job_id,
 limit: Some(1),
 cursor: Some(cursor),
 })
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// B4-7 — calendar trap stops at the near-duplicate rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn calendar_trap_stops_at_the_near_duplicate_rule() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.log.clear();
 let seed = fixture.url("127.0.0.1", "/trap/0");
 let mut input = start_input(vec![seed]);
 input.limits = Some(CrawlLimitsInput {
 max_pages: Some(1_000),
 per_host_rate: Some(100.0),
 max_duration_s: Some(60),
 });
 let started = env.crawler.start(input).await.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;

 assert_eq!(status.state, "done");
 assert!(
 status.limits_hit.is_empty(),
 "the budget must not be what stopped it: {:?}",
 status.limits_hit
);
 assert!(
 status.counters.fetched <= 3,
 "the trap must collapse, fetched {}",
 status.counters.fetched
);
 assert!(status.counters.skipped >= 1, "{status:?}");
 assert!(
 fixture.log.count_matching("/trap/") <= 3,
 "trap pages requested: {:?}",
 fixture.log.entries()
);
}

// ---------------------------------------------------------------------------
// B4-8 — cooperative cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_stops_leasing_and_keeps_partial_results() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.log.clear();
 let seed = fixture.url("127.0.0.1", "/bulk/");
 let mut input = start_input(vec![seed]);
 input.limits = Some(CrawlLimitsInput {
 max_pages: Some(500),
 per_host_rate: Some(10.0),
 max_duration_s: Some(120),
 });
 let started = env.crawler.start(input).await.expect("crawl starts");

 // Wait until the crawl is actually fetching.
 let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
 while fixture.log.count_matching("/bulk/") < 2 {
 assert!(
 tokio::time::Instant::now() < deadline,
 "crawl never started"
);
 tokio::time::sleep(Duration::from_millis(20)).await;
 }

 let after_cancel = env
.crawler
.cancel(&started.job_id)
.await
.expect("cancel accepted");
 assert!(
 matches!(after_cancel.state.as_str(), "running" | "cancelled"),
 "{after_cancel:?}"
);

 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "cancelled", "{status:?}");
 assert!(status.counters.fetched >= 1, "in-flight page finished");
 let requests = fixture.log.entries().len();
 tokio::time::sleep(Duration::from_millis(400)).await;
 assert_eq!(
 fixture.log.entries().len(),
 requests,
 "no lease may be taken after cancellation"
);

 let (rows, _) = all_results(&env, &started.job_id, 50).await;
 assert_eq!(
 rows.len(),
 status.counters.fetched as usize,
 "partial results stay readable"
);
}

// ---------------------------------------------------------------------------
// B4-9 — schema-driven mining, and fail-fast on an invalid schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_records_are_returned_per_page_and_invalid_schemas_reject_the_job() {
 let (fixture, env) = env_with(|_| {}).await;
 let mut input = start_input(vec![fixture.url("127.0.0.1", "/site/variant")]);
 input.schema = Some(json!({ "fields": { "title": { "selector": "h1" } } }));
 let started = env
.crawler
.start(input)
.await
.expect("crawl starts with a schema");
 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "done", "{status:?}");

 let (rows, _) = all_results(&env, &started.job_id, 10).await;
 assert_eq!(rows.len(), 1);
 assert_eq!(
 rows[0]["extracted"],
 json!([{ "title": "Variant page" }]),
 "records are stored with the crawl result"
);
 assert_eq!(
 rows[0]["url"],
 json!(fixture.url("127.0.0.1", "/site/variant"))
);

 // Invalid schema: rejected before any request.
 fixture.log.clear();
 let mut bad = start_input(vec![fixture.url("127.0.0.1", "/site/variant")]);
 bad.schema = Some(json!({ "fields": { "t": { "selector": "h1", "transform": "regex" } } }));
 let error = env.crawler.start(bad).await.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);
 assert!(
 error
.detail()
.unwrap_or_default()
.contains("/fields/t/transform")
);
 assert!(
 fixture.log.entries().is_empty(),
 "no fetch before schema rejection"
);

 // Invalid scope pattern: same fail-fast rule.
 let mut bad_scope = start_input(vec![fixture.url("127.0.0.1", "/site/variant")]);
 bad_scope.scope = Some(CrawlScopeInput {
 include_paths: Vec::new(),
 exclude_patterns: vec!["(".to_string()],
 max_depth: None,
 allow_offsite: false,
 });
 let error = env.crawler.start(bad_scope).await.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);
 assert!(
 error
.detail()
.unwrap_or_default()
.contains("/scope/exclude_patterns/0")
);
}

#[tokio::test]
async fn start_input_is_validated() {
 let (_fixture, env) = env_with(|_| {}).await;
 let error = env
.crawler
.start(CrawlStartInput {
 start_urls: Vec::new(),
 sitemap_urls: Vec::new(),
 scope: None,
 schema: None,
 render_policy: None,
 limits: None,
 name: None,
 })
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);

 let mut many = start_input(vec!["http://127.0.0.1/".to_string()]);
 many.start_urls = (0..21)
.map(|index| format!("http://127.0.0.1/{index}"))
.collect();
 assert_eq!(many.start_urls.len(), 21);
 let error = env.crawler.start(many).await.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);

 let mut bad_rate = start_input(vec!["http://127.0.0.1/".to_string()]);
 bad_rate.limits = Some(CrawlLimitsInput {
 max_pages: None,
 per_host_rate: Some(0.0),
 max_duration_s: None,
 });
 let error = env.crawler.start(bad_rate).await.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);

 let mut bad_url = start_input(vec!["not-a-url".to_string()]);
 bad_url.start_urls = vec!["not-a-url".to_string()];
 let error = env.crawler.start(bad_url).await.unwrap_err();
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);
 assert!(error.detail().unwrap_or_default().contains("/start_urls/0"));
}

#[tokio::test]
async fn unknown_job_ids_are_not_found() {
 let (_fixture, env) = env_with(|_| {}).await;
 let error = env
.crawler
.status("0192f3aa-0000-7000-8000-000000000000")
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::NotFound);
 let error = env
.crawler
.cancel("0192f3aa-0000-7000-8000-000000000001")
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// Regression (B04 rework): one result row per attempted page even when several
// extractions exist for the same page and schema.
// ---------------------------------------------------------------------------

/// A page can legitimately have more than one `extractions` row for one profile:
/// `extract` without `use_cache`, a second crawl with the same schema, or a
/// resumed lease that stored before a kill. `crawl_results` must still return one
/// row per attempted page, carrying the latest records.
#[tokio::test]
async fn crawl_results_has_one_row_per_page_when_several_extractions_exist() {
 use web_widow::pipeline::ExtractInput;

 let (fixture, env) = env_with(|_| {}).await;
 let url = fixture.url("127.0.0.1", "/site/variant");
 let schema = json!({ "fields": { "title": { "selector": "h1" } } });

 // 1. A standalone `extract` with the same schema stores an extraction for
 // the same page and profile before the crawl exists.
 let extracted = env
.pipeline
.extract(ExtractInput {
 url: url.clone(),
 schema: schema.clone(),
 render: None,
 use_cache: Some(false),
 max_field_chars: None,
 max_chars: None,
 })
.await
.expect("direct extract");
 assert_eq!(extracted.records[0]["title"], json!("Variant page"));

 // 2. The crawl over the same URL with the same schema adds a second row for
 // the same (page, profile).
 let mut input = start_input(vec![url.clone()]);
 input.schema = Some(schema.clone());
 let started = env.crawler.start(input).await.expect("crawl starts");
 let status = wait_terminal(&env, &started.job_id).await;
 assert_eq!(status.state, "done", "{status:?}");
 assert_eq!(status.counters.fetched, 1, "{status:?}");

 let rows = env
.crawler
.results(CrawlResultsInput {
 job_id: started.job_id.clone(),
 limit: Some(50),
 cursor: None,
 })
.await
.expect("results");
 assert_eq!(
 rows.results.len(),
 1,
 "one row per attempted page, got {:?}",
 rows.results
);
 assert_eq!(
 rows.results[0].extracted,
 Some(json!([{ "title": "Variant page" }])),
 "the row carries the job schema's records"
);
 assert!(
 rows.next_cursor.is_none(),
 "a single row must not look like a full page"
);

 // 3. A second crawl with the same schema (another job) sees the same two
 // extraction rows and must also return exactly one row per page.
 let mut second = start_input(vec![url]);
 second.schema = Some(schema);
 let second_job = env.crawler.start(second).await.expect("second crawl");
 let second_status = wait_terminal(&env, &second_job.job_id).await;
 assert_eq!(second_status.state, "done", "{second_status:?}");
 let second_rows = env
.crawler
.results(CrawlResultsInput {
 job_id: second_job.job_id,
 limit: Some(50),
 cursor: None,
 })
.await
.expect("second results");
 assert_eq!(second_rows.results.len(), 1);
 assert_eq!(
 second_rows.results[0].extracted,
 Some(json!([{ "title": "Variant page" }]))
);

 // 4. Pagination still counts rows, not join products.
 let paged = env
.crawler
.results(CrawlResultsInput {
 job_id: started.job_id,
 limit: Some(1),
 cursor: None,
 })
.await
.expect("paged results");
 assert_eq!(paged.results.len(), 1);
}
