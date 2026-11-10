//! / — corpus freshness.
//!
//! Fixtures for the four per-result facts (IMP-DEC-FRESHNESS-001/002/003):
//! capture vs validation vs attempt, row→write binding, 304 binding and
//! staleness, cache reads, retained index text, semantic row selection, crawl
//! item attribution, monotonic gates, persist-failure atomicity, export aliases
//! and success-anchored authenticated retention.
//!
//! Synthetic stores only: every database here is created by the test, and the
//! only origin is the loopback fixture server.

mod common;

use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use common::{TestEnv, start_fixture, test_config};
use serde_json::json;
use web_widow::config::FtsConfig;
use web_widow::core::url::content_hash;
use web_widow::export::CorpusExportInput;
use web_widow::extract::Mode;
use web_widow::pipeline::{ExtractInput, FetchPageInput, RenderPolicy};
use web_widow::store::db::rfc3339_secs;
use web_widow::store::{BlobRow, Database, ExtractionRow, FetchRecord, IndexText, NewJob, Seed};

fn open_db(name: &str) -> (tempfile::TempDir, Database) {
 let dir = tempfile::tempdir().unwrap();
 let db = Database::open(&dir.path().join(name), 1, &FtsConfig::default(), false).unwrap();
 (dir, db)
}

/// One minimal acquisition record for store-level drills.
fn record(url: &str, outcome: &str, at: DateTime<Utc>) -> FetchRecord {
 let mut record = FetchRecord::new(url, url, &content_hash(url.as_bytes()));
 record.site = "example.com".to_string();
 record.status = Some(if outcome == "ok" { 200 } else { 500 });
 record.content_type = Some("text/html".to_string());
 record.charset = Some("UTF-8".to_string());
 record.fetched_at = at;
 record.outcome = outcome.to_string();
 record.duration_ms = 10;
 record.bytes = Some(10);
 record
}

/// Model a page-text acquisition: the delivered text hash is what makes a
/// usable 200 a *content* write (schema v11), independent of FTS indexing.
fn page_text(record: &mut FetchRecord, body: &str) {
 record.text_hash = Some(content_hash(body.as_bytes()));
 record.index_text = Some(IndexText::replace(body));
}

fn extraction(profile: &str, body: &str) -> ExtractionRow {
 ExtractionRow {
 profile_hash: profile.to_string(),
 result_json: format!(r#"{{"records":[{{"body":"{body}"}}]}}"#),
 result_hash: content_hash(body.as_bytes()),
 record_count: 1,
 created_at: Utc::now(),
 }
}

async fn page(db: &Database, url: &str) -> web_widow::store::PageRecord {
 db.page(&content_hash(url.as_bytes()))
.await
.unwrap()
.expect("page row exists")
}

// ---------------------------------------------------------------------------
// P1 — a 200 sets capture = validation; a bound 304 moves validation only.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_200_then_304_advances_validation_only() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 let url = fixture.url("127.0.0.1", "/article");

 let first = env.fetch(&url).await.expect("200");
 assert_eq!(first.payload.status, 200);
 let captured = first.payload.fetched_at.clone();
 assert_eq!(
 first.payload.validated_at.as_deref(),
 Some(captured.as_str())
);
 assert_eq!(first.payload.last_attempt_failed, Some(false));
 assert_eq!(
 first.payload.last_attempt_at.as_deref(),
 Some(captured.as_str())
);

 let before = page(&env.store.db, &url).await;
 assert_eq!(before.write_seq, 1);
 assert_eq!(before.content_write_seq, Some(1));
 assert_eq!(before.index_write_seq, Some(1));
 assert_eq!(before.validators_write_seq, Some(1));
 assert_eq!(before.last_attempt_failed, Some(0));

 // A distinguishable second so "capture stayed, validation moved" is visible.
 tokio::time::sleep(StdDuration::from_millis(1100)).await;

 let second = env.fetch(&url).await.expect("304");
 assert_eq!(second.payload.status, 304);
 assert!(second.payload.cached);
 assert_eq!(
 second.payload.fetched_at, captured,
 "the 304 serves the row captured by the 200"
);
 let validated = second.payload.validated_at.clone().expect("bound 304");
 assert!(
 validated > captured,
 "validation moves to the 304 time: {validated} vs {captured}"
);
 assert_eq!(second.payload.last_attempt_failed, Some(false));

 let after = page(&env.store.db, &url).await;
 assert_eq!(after.write_seq, 1, "a 304 is not a new write");
 assert_eq!(after.content_write_seq, Some(1));
 assert_eq!(
 after.content_fetched_at, before.content_fetched_at,
 "capture never moves on a 304"
);
 let stored_validated = after
.content_validated_at
.as_deref()
.and_then(web_widow::store::db::stored_secs)
.expect("validation recorded");
 assert!(stored_validated > captured);
 assert_eq!(after.last_attempt_failed, Some(0));
}

// ---------------------------------------------------------------------------
// P2 — a failure is an attempt, never a capture or a validation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_attempt_never_renews_the_success_clock() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 // No stored monitor page yet: the fixture answers 404, then serves after
 // `set_page`, so the same URL fails and then succeeds deterministically.
 let url = fixture.url("127.0.0.1", "/monitor/freshness");

 let failed = env.fetch(&url).await;
 assert!(failed.is_err(), "{:?}", failed.map(|r| r.payload.status));
 let after_failure = page(&env.store.db, &url).await;
 assert_eq!(after_failure.last_attempt_failed, Some(1));
 assert!(after_failure.last_fetched.is_none(), "no success clock");
 assert!(after_failure.content_fetched_at.is_none());
 assert_eq!(after_failure.write_seq, 0);

 fixture.set_page(
 "/monitor/freshness",
 "<html><head><title>Fresh</title></head><body><article><h1>Fresh</h1>\
 <p>Enough distinct prose to be a readable fixture article body for the freshness drill.</p>\
 </article></body></html>");
 let ok = env.fetch(&url).await.expect("200 after the failure");
 assert_eq!(ok.payload.status, 200);
 assert_eq!(ok.payload.last_attempt_failed, Some(false));

 let after_success = page(&env.store.db, &url).await;
 assert_eq!(after_success.last_attempt_failed, Some(0));
 assert_eq!(after_success.write_seq, 1);
 assert!(
 after_success.last_fetched.is_some(),
 "only the usable 200 starts the success clock"
);
 assert_ne!(
 after_success.last_attempt_at, after_failure.last_attempt_at,
 "the attempt clock tracks the newest attempt"
);
}

// ---------------------------------------------------------------------------
// P3 — F1: identical Markdown, different payloads, stale sent sequence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_304_that_does_not_match_the_served_rows_write_is_unbound() {
 let (_dir, db) = open_db("f1.db");
 let url = "https://example.com/f1";
 let t1 = Utc::now() - chrono::Duration::seconds(120);
 let t2 = t1 + chrono::Duration::seconds(60);

 // Write 1: profile A, body "same body", validators from this write.
 let mut first = record(url, "ok", t1);
 first.etag = Some("\"v1\"".to_string());
 page_text(&mut first, "same body");
 first.extraction = Some(extraction("profile-a", "same body"));
 db.record_fetch(first).await.unwrap();

 // Write 2: profile B, byte-identical Markdown, its own validators.
 let mut second = record(url, "ok", t2);
 second.etag = Some("\"v2\"".to_string());
 page_text(&mut second, "same body");
 second.extraction = Some(extraction("profile-b", "same body"));
 db.record_fetch(second).await.unwrap();

 let before = page(&db, url).await;
 assert_eq!(before.write_seq, 2);
 assert_eq!(before.validators_write_seq, Some(2));

 // A 304 sent with write 1's validators (the F1 scenario): the origin
 // confirms "same body", but the served row for profile A belongs to write 1,
 // which is no longer the page's current write.
 let mut not_modified = record(url, "not_modified", t2 + chrono::Duration::seconds(30));
 not_modified.status = Some(304);
 not_modified.sent_validators_seq = Some(1);
 not_modified.served_profile_hash = Some("profile-a".to_string());
 let outcome = db.record_fetch(not_modified).await.unwrap();
 let attribution = outcome.not_modified.expect("304 attribution");
 assert!(!attribution.bound, "stale validators are unbound");
 let served = attribution.served.expect("the served row is returned");
 assert_eq!(served.source_write_seq, Some(1));

 let after = page(&db, url).await;
 assert_eq!(
 after.last_attempt_failed, None,
 "an unbound 304 records an attempt with an unknown success class"
);
 assert_eq!(
 after.content_validated_at, before.content_validated_at,
 "an unbound 304 never advances validation"
);
 assert_eq!(after.last_fetched, before.last_fetched);
 assert_eq!(after.write_seq, 2);
}

#[tokio::test]
async fn a_stale_sent_sequence_after_a_newer_page_write_is_unbound() {
 let (_dir, db) = open_db("stale.db");
 let url = "https://example.com/stale";
 let t1 = Utc::now() - chrono::Duration::seconds(120);
 let t2 = t1 + chrono::Duration::seconds(60);

 let mut first = record(url, "ok", t1);
 first.etag = Some("\"v1\"".to_string());
 page_text(&mut first, "first body");
 db.record_fetch(first).await.unwrap();

 let mut second = record(url, "ok", t2);
 second.etag = Some("\"v2\"".to_string());
 page_text(&mut second, "second body");
 db.record_fetch(second).await.unwrap();

 // The conditional headers were read before write 2 landed.
 let mut not_modified = record(url, "not_modified", t2 + chrono::Duration::seconds(5));
 not_modified.status = Some(304);
 not_modified.sent_validators_seq = Some(1);
 let before = page(&db, url).await;
 let outcome = db.record_fetch(not_modified).await.unwrap();
 let attribution = outcome.not_modified.unwrap();
 assert!(!attribution.bound);
 let after = page(&db, url).await;
 assert_eq!(after.last_attempt_failed, None);
 assert_eq!(
 after.content_validated_at, before.content_validated_at,
 "an unbound 304 never advances the content clock"
);
 assert_eq!(after.last_fetched, before.last_fetched);
}

// ---------------------------------------------------------------------------
// P4 — extract 200 → 304 binds to the served extraction row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extract_200_then_304_is_bound_to_the_served_row() {
 let fixture = start_fixture().await;
 // A zero cache window makes the first call miss the local cache and send the
 // conditional request, which is the path this fixture is about.
 let mut config = test_config();
 config.extract.cache_freshness_s = 0;
 let env = TestEnv::new(config).await;
 let url = fixture.url("127.0.0.1", "/article");
 let payload = json!({
 "url": url,
 "schema": { "fields": { "title": { "selector": "h1" } } },
 "use_cache": true
 });
 let input: ExtractInput = serde_json::from_value(payload).expect("valid extract input");

 let first = env.pipeline.extract(input.clone()).await.expect("200");
 assert_eq!(first.status, 200);
 let captured = first.fetched_at.clone();
 assert_eq!(first.validated_at.as_deref(), Some(captured.as_str()));

 let before = page(&env.store.db, &url).await;
 assert_eq!(before.write_seq, 1);
 assert!(
 before.content_write_seq.is_none(),
 "extract does not write the page text"
);
 assert_eq!(before.index_write_seq, Some(1), "the projection filled FTS");

 tokio::time::sleep(StdDuration::from_millis(1100)).await;

 let second = env.pipeline.extract(input).await.expect("304");
 assert_eq!(second.status, 304);
 assert!(second.cached);
 assert_eq!(
 second.fetched_at, captured,
 "the served extraction row's own capture"
);
 let validated = second.validated_at.clone().expect("bound 304");
 assert!(validated > captured);
 assert_eq!(second.last_attempt_failed, Some(false));

 let after = page(&env.store.db, &url).await;
 assert_eq!(
 after.last_fetched, before.last_fetched,
 "an index-bound 304 does not move the page success clock"
);
 let stored = after
.index_validated_at
.as_deref()
.and_then(web_widow::store::db::stored_secs)
.expect("index validation recorded");
 assert!(stored > captured);
}

// ---------------------------------------------------------------------------
// P5 — cache read after a failure: same capture, failure as attempt only.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cache_read_after_a_failure_keeps_capture_and_reports_the_attempt() {
 let (_dir, db) = open_db("cache.db");
 let url = "https://example.com/cache";
 let captured = Utc::now() - chrono::Duration::seconds(300);

 let mut ok = record(url, "ok", captured);
 page_text(&mut ok, "the delivered body");
 let mut row = extraction("shape", "the delivered body");
 row.created_at = captured;
 ok.extraction = Some(row);
 db.record_fetch(ok).await.unwrap();

 let fresh = db
.cached_extraction(&content_hash(url.as_bytes()), "shape")
.await
.unwrap()
.expect("row stored");
 let facts = fresh
.page
.freshness_for(Some(fresh.created_at), fresh.source_write_seq);
 assert_eq!(facts.fetched_at.as_deref(), Some(secs(captured).as_str()));
 assert_eq!(
 facts.validated_at, facts.fetched_at,
 "a 200 validates itself"
);
 assert_eq!(facts.last_attempt_failed, Some(false));

 // A later failure on the same page: the read still serves the old body with
 // the old capture, and the failure appears only as an attempt.
 let failed = record(url, "error:http_error", Utc::now());
 db.record_fetch(failed).await.unwrap();

 let after = db
.cached_extraction(&content_hash(url.as_bytes()), "shape")
.await
.unwrap()
.expect("row still stored");
 let facts = after
.page
.freshness_for(Some(after.created_at), after.source_write_seq);
 assert_eq!(facts.fetched_at.as_deref(), Some(secs(captured).as_str()));
 assert_eq!(facts.validated_at, facts.fetched_at);
 assert_eq!(facts.last_attempt_failed, Some(true));
 assert!(
 facts.last_attempt_at.is_some(),
 "the failure time is reported"
);
}

// ---------------------------------------------------------------------------
// P6 — retained index text keeps its own write's facts after a failure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_fts_hit_after_a_failure_reports_the_index_write_facts() {
 let env = TestEnv::new(test_config()).await;
 let db = &env.store.db;
 let url = "https://example.com/fts";
 let captured = Utc::now() - chrono::Duration::seconds(300);

 let mut ok = record(url, "ok", captured);
 page_text(&mut ok, "lighthouse keeper counted gulls");
 db.record_fetch(ok).await.unwrap();

 let failed = record(url, "error:http_error", Utc::now());
 db.record_fetch(failed).await.unwrap();

 let payload = env
.content
.search(web_widow::fts::SearchContentInput {
 query: "lighthouse".to_string(),
 limit: Some(10),
 })
.await
.unwrap();
 assert_eq!(payload.results.len(), 1, "a failure never clears the index");
 let hit = &payload.results[0];
 assert_eq!(
 hit.fetched_at.as_deref(),
 Some(secs(captured).as_str()),
 "the hit is dated by the index write, not the failed attempt"
);
 assert_eq!(hit.validated_at, hit.fetched_at);
 assert_eq!(
 hit.last_fetched, hit.validated_at,
 "the legacy key aliases validation"
);
 assert_eq!(hit.last_attempt_failed, Some(true));
 assert!(hit.last_attempt_at.is_some());
}

// ---------------------------------------------------------------------------
// P7 — semantic source: one row, its own capture, markdown preferred.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_source_returns_one_row_with_its_own_capture() {
 let (_dir, db) = open_db("semantic.db");
 let url = "https://example.com/semantic";
 let t1 = Utc::now() - chrono::Duration::seconds(600);
 let t2 = t1 + chrono::Duration::seconds(300);

 let mut page_write = record(url, "ok", t1);
 page_text(&mut page_write, "the delivered markdown body");
 let mut markdown_row = ExtractionRow {
 profile_hash: "markdown-shape".to_string(),
 result_json: json!({"markdown": "the delivered markdown body"}).to_string(),
 result_hash: "r1".to_string(),
 record_count: 1,
 created_at: t1,
 };
 markdown_row.record_count = 1;
 page_write.extraction = Some(markdown_row);
 db.record_fetch(page_write).await.unwrap();

 // A newer records-only extraction of the same page: it must not date the
 // markdown row, and the markdown row must still win.
 let mut extract_write = record(url, "ok", t2);
 extract_write.mode = "extract".to_string();
 let mut rows = extraction("records-shape", "records only");
 rows.created_at = t2;
 extract_write.extraction = Some(rows);
 db.record_fetch(extract_write).await.unwrap();

 let page_id = page(&db, url).await.id;
 let sources = db.page_sources(&[page_id]).await.unwrap();
 let source = sources.get(&page_id).expect("source row");
 assert_eq!(
 source.result_json.as_deref(),
 Some(
 json!({"markdown": "the delivered markdown body"})
.to_string()
.as_str()
),
 "the markdown-bearing row wins"
);
 assert_eq!(
 source.created_at.map(rfc3339_secs),
 Some(secs(t1)),
 "the row's own capture, not the newer extraction's"
);
 let facts = source.freshness();
 assert_eq!(facts.fetched_at.as_deref(), Some(secs(t1).as_str()));
 assert_eq!(facts.validated_at.as_deref(), Some(secs(t1).as_str()));
}

// ---------------------------------------------------------------------------
// P8 — crawl item: a later same-spec extract is dated by its own write.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crawl_item_is_dated_by_the_row_it_returns() {
 let (_dir, db) = open_db("crawl.db");
 let url = "https://example.com/crawl-page";
 let hash = content_hash(url.as_bytes());
 let t1 = Utc::now() - chrono::Duration::seconds(900);
 let t2 = t1 + chrono::Duration::seconds(600);
 let profile = "crawl-profile";

 db.create_job(NewJob {
 id: "job-m10".to_string(),
 kind: "crawl".to_string(),
 spec_json: "{}".to_string(),
 created_at: t1,
 seeds: vec![Seed {
 url: url.to_string(),
 url_hash: hash.clone(),
 depth: 0,
 }],
 })
.await
.unwrap();
 db.finish_url("job-m10", &hash, "done", 0, None, None)
.await
.unwrap();

 // E5: the crawl wrote the page text and its extraction.
 let mut crawl = record(url, "ok", t1);
 crawl.mode = "crawl".to_string();
 crawl.etag = Some("\"v1\"".to_string());
 page_text(&mut crawl, "crawl page body");
 let mut e5 = extraction(profile, "records from the crawl");
 e5.created_at = t1;
 crawl.extraction = Some(e5);
 db.record_fetch(crawl).await.unwrap();

 // E6: a later uncached `extract` of the same URL and spec.
 let mut extract = record(url, "ok", t2);
 extract.mode = "extract".to_string();
 let mut e6 = extraction(profile, "records from the later extract");
 e6.created_at = t2;
 extract.extraction = Some(e6);
 db.record_fetch(extract).await.unwrap();

 let rows = db
.crawl_results("job-m10", 0, 10, Some(profile))
.await
.unwrap();
 assert_eq!(rows.len(), 1);
 let row = &rows[0];
 assert_eq!(
 row.extracted,
 Some(json!([{"body": "records from the later extract"}])),
 "the returned records are the later extraction's"
);
 assert_eq!(
 row.fetched_at.as_deref(),
 Some(secs(t2).as_str()),
 "dated by the returned row's own capture, never E5"
);
 assert_eq!(row.validated_at.as_deref(), Some(secs(t2).as_str()));
 assert_ne!(row.fetched_at.as_deref(), Some(secs(t1).as_str()));
}

// ---------------------------------------------------------------------------
// P9 — reversed completions: the older 200 writes nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reversed_commit_of_two_200s_keeps_the_newer_write() {
 let (dir, db) = open_db("reversed.db");
 let url = "https://example.com/reversed";
 let newer = Utc::now() - chrono::Duration::seconds(60);
 let older = newer - chrono::Duration::seconds(120);

 let mut first = record(url, "ok", newer);
 first.render_mode = "rendered".to_string();
 first.etag = Some("\"v2\"".to_string());
 page_text(&mut first, "newer body");
 first.extraction = Some(extraction("shape", "newer"));
 first.blob = Some(BlobRow {
 hash: "a".repeat(64),
 size: 10,
 content_type: Some("text/html".to_string()),
 created_at: newer,
 });
 db.record_fetch(first).await.unwrap();

 // Same URL, older completion, committed second.
 let mut late = record(url, "ok", older);
 late.render_mode = "static".to_string();
 late.etag = Some("\"v1\"".to_string());
 page_text(&mut late, "older body");
 late.extraction = Some(extraction("shape", "older"));
 late.blob = Some(BlobRow {
 hash: "b".repeat(64),
 size: 10,
 content_type: Some("text/html".to_string()),
 created_at: older,
 });
 db.record_fetch(late).await.unwrap();

 let page_row = page(&db, url).await;
 assert_eq!(page_row.write_seq, 1, "a gated-out success adds no write");
 assert_eq!(
 page_row
.content_fetched_at
.as_deref()
.and_then(web_widow::store::db::stored_secs),
 Some(secs(newer))
);
 let conn = rusqlite::Connection::open(dir.path().join("reversed.db")).unwrap();
 let count = |table: &str| -> i64 {
 conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
 row.get(0)
 })
.unwrap()
 };
 assert_eq!(
 count("extractions"),
 1,
 "the older extraction is not stored"
);
 assert_eq!(
 count("fetch_events"),
 2,
 "both attempts stay visible in the event log"
);
 let hits = db
.search_content("\"newer\"".to_string(), 10)
.await
.unwrap();
 assert_eq!(hits.rows.len(), 1, "the index keeps the newer text");
 assert_eq!(
 count("blobs"),
 1,
 "the gated-out success writes no blob row"
);
 let (render_mode, etag, validators_seq): (Option<String>, Option<String>, Option<i64>) = conn
.query_row(
 "SELECT render_mode, etag, validators_write_seq FROM pages",
 [],
 |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
.unwrap();
 assert_eq!(
 render_mode.as_deref(),
 Some("rendered"),
 "a gated-out success does not overwrite the newer render mode"
);
 assert_eq!(
 etag.as_deref(),
 Some("\"v2\""),
 "the stored validators stay owned by the recorded write"
);
 assert_eq!(validators_seq, Some(1));
}

// ---------------------------------------------------------------------------
// P10 — invalid input has no canonical target and writes nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_input_writes_nothing() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 let payload = json!({
 "url": fixture.url("127.0.0.1", "/article"),
 "schema": { "fields": { "title": { "selector": "##not-css" } } }
 });
 let input: ExtractInput = serde_json::from_value(payload).expect("shape is valid JSON");
 let error = env.pipeline.extract(input).await.expect_err("invalid spec");
 assert!(
 matches!(error, web_widow::WidowError::SchemaInvalid {.. }),
 "{error}"
);
 let db = env.db();
 for table in ["pages", "fetch_events", "extractions"] {
 let count: i64 = db
.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
 row.get(0)
 })
.unwrap();
 assert_eq!(count, 0, "{table} must stay empty");
 }
 assert_eq!(
 fixture.log.count_matching("/article"),
 0,
 "no origin request for a rejected spec"
);
}

// ---------------------------------------------------------------------------
// P11 — a persist failure is returned and commits nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_persist_failure_returns_an_error_and_commits_nothing() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 let url = fixture.url("127.0.0.1", "/article");

 let db = env.db();
 db.execute_batch(
 "CREATE TRIGGER m10_fail_events BEFORE INSERT ON fetch_events
 BEGIN SELECT RAISE(ABORT, 'synthetic persist failure'); END;")
.unwrap();

 let result = env.fetch(&url).await;
 assert!(
 matches!(result, Err(web_widow::WidowError::Internal(_))),
 "a persist error is returned, not swallowed: {:?}",
 result.map(|r| r.payload.status)
);

 let pages: i64 = db
.query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
.unwrap();
 let extractions: i64 = db
.query_row("SELECT COUNT(*) FROM extractions", [], |row| row.get(0))
.unwrap();
 let blobs: i64 = db
.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
.unwrap();
 assert_eq!((pages, extractions, blobs), (0, 0, 0), "rolled back");

 db.execute_batch("DROP TRIGGER m10_fail_events;").unwrap();
 let recovered = env.fetch(&url).await.expect("recovery after the fault");
 assert_eq!(recovered.payload.status, 200);
 assert_eq!(page(&env.store.db, &url).await.write_seq, 1);
}

// ---------------------------------------------------------------------------
// P12 — export: capture, validation and the legacy alias.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corpus_export_aliases_last_fetched_to_validation() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 env.fetch(&fixture.url("127.0.0.1", "/article"))
.await
.expect("200");

 let payload = env
.pipeline
.corpus_export(CorpusExportInput {
 format: "jsonl".to_string(),
 name: "m10".to_string(),
 site: None,
 since: None,
 limit: None,
 include_text: Some(false),
 include_extraction: Some(false),
 max_bytes: None,
 })
.await
.expect("export");
 let line = std::fs::read_to_string(&payload.path).expect("export file");
 let row: serde_json::Value =
 serde_json::from_str(line.lines().next().expect("one row")).unwrap();
 assert_eq!(row["fetched_at"], row["validated_at"]);
 assert_eq!(row["last_fetched"], row["validated_at"]);
 assert_eq!(row["last_attempt_failed"], json!(false));
 assert!(row["last_attempt_at"].is_string());
 assert_ne!(row["fetched_at"], json!(null));
}

// ---------------------------------------------------------------------------
// P13 — success-anchored authenticated retention.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authenticated_retention_is_not_renewed_by_failed_attempts() {
 let (dir, db) = open_db("retention.db");
 let now = Utc::now();
 let old = now - chrono::Duration::days(8);
 let recent = now - chrono::Duration::days(2);

 let insert_auth_page = |url: &str, hash_seed: &str, clock: DateTime<Utc>| {
 let mut record = record(url, "ok", clock);
 record.authenticated = true;
 record.auth_site = Some("site".to_string());
 record.index_text = None;
 record.extraction = Some(ExtractionRow {
 profile_hash: "auth:site".to_string(),
 result_json: format!(r#"{{"markdown":"private {hash_seed}"}}"#),
 result_hash: hash_seed.to_string(),
 record_count: 1,
 created_at: clock,
 });
 record
 };
 let old_id = db
.record_fetch(insert_auth_page("https://example.com/old-auth", "r1", old))
.await
.unwrap()
.ids
.page_id;
 db.record_fetch(insert_auth_page(
 "https://example.com/recent-auth",
 "r2",
 recent))
.await
.unwrap();

 // A failure-only attempt today on the old page must not keep it alive.
 let conn = rusqlite::Connection::open(dir.path().join("retention.db")).unwrap();
 conn.execute(
 "INSERT INTO fetch_events (page_id, started_at, duration_ms, mode, error_kind)
 VALUES (?1, ?2, 1, 'auth', 'http_error')",
 rusqlite::params![old_id, now.to_rfc3339()])
.unwrap();
 drop(conn);

 let report = db
.gc(web_widow::config::RetentionConfig::default(), 90, 7)
.await
.unwrap();
 assert_eq!(
 report.auth_pages_pruned, 1,
 "the failed attempt renewed nothing"
);
 let conn = rusqlite::Connection::open(dir.path().join("retention.db")).unwrap();
 let remaining: i64 = conn
.query_row(
 "SELECT COUNT(*) FROM pages WHERE authenticated = 1",
 [],
 |r| r.get(0))
.unwrap();
 assert_eq!(remaining, 1, "the recently validated page survives");
}

// ---------------------------------------------------------------------------
// P14 — a bound page 304 must not validate a row another write produced (F1).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_bound_304_does_not_validate_a_row_from_another_write() {
 let (_dir, db) = open_db("bound-mismatch.db");
 let url = "https://example.com/bound-mismatch";
 let t1 = Utc::now() - chrono::Duration::seconds(300);
 let t2 = t1 + chrono::Duration::seconds(60);
 let t3 = t2 + chrono::Duration::seconds(60);

 let mut w1 = record(url, "ok", t1);
 w1.etag = Some("\"v1\"".to_string());
 page_text(&mut w1, "identical body");
 w1.extraction = Some(extraction("profile-a", "identical body"));
 db.record_fetch(w1).await.unwrap();

 let mut w2 = record(url, "ok", t2);
 w2.etag = Some("\"v2\"".to_string());
 page_text(&mut w2, "identical body");
 w2.extraction = Some(extraction("profile-b", "identical body"));
 db.record_fetch(w2).await.unwrap();

 // The conditional headers came from write 2, but the caller is served row A.
 let mut not_modified = record(url, "not_modified", t3);
 not_modified.status = Some(304);
 not_modified.sent_validators_seq = Some(2);
 not_modified.served_profile_hash = Some("profile-a".to_string());
 let attribution = db
.record_fetch(not_modified)
.await
.unwrap()
.not_modified
.expect("304 attribution");
 assert!(
 attribution.bound,
 "the page-level attempt is a bound success"
);
 assert!(
 !attribution.served_validated,
 "row A belongs to write 1, not to the validators that were sent"
);
 assert_eq!(attribution.served.unwrap().source_write_seq, Some(1));
 assert_eq!(attribution.attempt_failed, Some(false));

 let after = page(&db, url).await;
 assert_eq!(after.last_attempt_failed, Some(0));
}

#[tokio::test]
async fn a_fetch_304_for_another_shapes_row_is_page_bound_but_not_served_validated() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;
 let url = fixture.url("127.0.0.1", "/article");

 fn shape(url: &str, mode: Mode) -> FetchPageInput {
 FetchPageInput {
 url: url.to_string(),
 mode: Some(mode),
 render: Some(RenderPolicy::Never),
 freshness_s: None,
 max_chars: None,
 include_links: None,
 include_content: None,
 profile: None,
 }
 }

 let first = env
.pipeline
.fetch_page(shape(&url, Mode::Full))
.await
.expect("shape A 200");
 assert_eq!(first.payload.status, 200);
 let other = env
.pipeline
.fetch_page(shape(&url, Mode::Main))
.await
.expect("shape B 200");
 assert_eq!(other.payload.status, 200);

 let row = page(&env.store.db, &url).await;
 assert_eq!(row.write_seq, 2);
 assert_eq!(row.validators_write_seq, Some(2));

 let again = env
.pipeline
.fetch_page(shape(&url, Mode::Full))
.await
.expect("shape A 304");
 assert_eq!(again.payload.status, 304);
 assert!(again.payload.cached);
 assert_eq!(again.payload.fetched_at, first.payload.fetched_at);
 assert_eq!(
 again.payload.validated_at, None,
 "row A's write is not the one whose validators were sent"
);
 assert_eq!(
 again.payload.last_attempt_failed,
 Some(false),
 "the page-level 304 is still a bound attempt"
);
 assert!(again.payload.last_attempt_at.is_some());
}

#[tokio::test]
async fn an_extract_304_across_specs_is_page_bound_but_not_served_validated() {
 let fixture = start_fixture().await;
 let mut config = test_config();
 config.extract.cache_freshness_s = 0;
 let env = TestEnv::new(config).await;
 let url = fixture.url("127.0.0.1", "/article");
 let spec_a = json!({
 "url": url.clone(),
 "schema": { "fields": { "title": { "selector": "h1" } } },
 "use_cache": true
 });
 let spec_b = json!({
 "url": url.clone(),
 "schema": {
 "fields": {
 "headline": { "selector": "h1" },
 "link": { "selector": "nav a" }
 }
 },
 "use_cache": true
 });

 let first = env
.pipeline
.extract(serde_json::from_value(spec_a.clone()).expect("spec A"))
.await
.expect("spec A 200");
 assert_eq!(first.status, 200);
 let second = env
.pipeline
.extract(serde_json::from_value(spec_b).expect("spec B"))
.await
.expect("spec B 200");
 assert_eq!(second.status, 200);
 let row = page(&env.store.db, &url).await;
 assert_eq!(row.validators_write_seq, Some(2));

 let again = env
.pipeline
.extract(serde_json::from_value(spec_a).expect("spec A"))
.await
.expect("spec A 304");
 assert_eq!(again.status, 304);
 assert_eq!(again.fetched_at, first.fetched_at);
 assert_eq!(again.validated_at, None);
 assert_eq!(again.last_attempt_failed, Some(false));
}

// ---------------------------------------------------------------------------
// P15 — a legacy markdown row keeps its null metadata beside a newer row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_legacy_markdown_row_keeps_its_null_metadata_beside_a_newer_records_row() {
 let (dir, db) = open_db("legacy-semantic.db");
 let url = "https://example.com/legacy-semantic";
 let t1 = Utc::now() - chrono::Duration::seconds(900);
 let t2 = t1 + chrono::Duration::seconds(300);

 let mut page_write = record(url, "ok", t1);
 page_text(&mut page_write, "legacy markdown body");
 page_write.extraction = Some(ExtractionRow {
 profile_hash: "markdown-shape".to_string(),
 result_json: json!({"markdown": "legacy markdown body"}).to_string(),
 result_hash: "r1".to_string(),
 record_count: 1,
 created_at: t1,
 });
 db.record_fetch(page_write).await.unwrap();

 let mut extract_write = record(url, "ok", t2);
 extract_write.mode = "extract".to_string();
 let mut rows = extraction("records-shape", "records only");
 rows.created_at = t2;
 extract_write.extraction = Some(rows);
 db.record_fetch(extract_write).await.unwrap();

 // Make the markdown row look pre-v11: no write sequence at all.
 let conn = rusqlite::Connection::open(dir.path().join("legacy-semantic.db")).unwrap();
 conn.execute(
 "UPDATE extractions SET source_write_seq = NULL WHERE profile_hash = 'markdown-shape'",
 [])
.unwrap();

 let page_id = page(&db, url).await.id;
 let sources = db.page_sources(&[page_id]).await.unwrap();
 let source = sources.get(&page_id).expect("source row");
 assert_eq!(
 source.result_json.as_deref(),
 Some(
 json!({"markdown": "legacy markdown body"})
.to_string()
.as_str()
),
 "the markdown row still wins"
);
 assert_eq!(
 source.source_write_seq, None,
 "no write sequence is borrowed from the newer records row"
);
 assert_eq!(source.created_at.map(rfc3339_secs), Some(secs(t1)));
 let facts = source.freshness();
 assert_eq!(facts.fetched_at.as_deref(), Some(secs(t1).as_str()));
 assert_eq!(
 facts.validated_at, None,
 "a pre-v11 row proves no validation"
);
}

// ---------------------------------------------------------------------------
// P16 — persisted auth without indexing still has content capture/validation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_persisted_auth_write_without_indexing_still_captures_content() {
 let (dir, db) = open_db("auth-clocks.db");
 let url = "https://example.com/auth-clocks";
 let t1 = Utc::now() - chrono::Duration::seconds(60);

 let mut record = record(url, "ok", t1);
 record.authenticated = true;
 record.auth_site = Some("site".to_string());
 record.text_hash = Some(content_hash(b"private body"));
 record.index_text = None; // index_authenticated = false
 record.extraction = Some(extraction("auth:site", "private body"));
 db.record_fetch(record).await.unwrap();

 struct Stored {
 write_seq: i64,
 content_fetched_at: Option<String>,
 content_validated_at: Option<String>,
 content_write_seq: Option<i64>,
 index_write_seq: Option<i64>,
 last_fetched: Option<String>,
 }
 let conn = rusqlite::Connection::open(dir.path().join("auth-clocks.db")).unwrap();
 let stored = conn
.query_row(
 "SELECT write_seq, content_fetched_at, content_validated_at, content_write_seq,
 index_write_seq, last_fetched
 FROM pages",
 [],
 |row| {
 Ok(Stored {
 write_seq: row.get(0)?,
 content_fetched_at: row.get(1)?,
 content_validated_at: row.get(2)?,
 content_write_seq: row.get(3)?,
 index_write_seq: row.get(4)?,
 last_fetched: row.get(5)?,
 })
 })
.unwrap();
 assert_eq!(stored.write_seq, 1);
 assert_eq!(
 stored.content_write_seq,
 Some(1),
 "the delivered text is a content write"
);
 assert!(stored.content_fetched_at.is_some());
 assert_eq!(stored.content_fetched_at, stored.content_validated_at);
 assert_eq!(stored.index_write_seq, None, "nothing was indexed");
 assert_eq!(stored.last_fetched, stored.content_fetched_at);
}

// ---------------------------------------------------------------------------
// P17 — a rejected (older) 304 attempt reports the committed pair together.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rejected_attempt_304_reports_the_committed_pair() {
 let (_dir, db) = open_db("rejected-304.db");
 let url = "https://example.com/rejected-304";
 let t2 = Utc::now() - chrono::Duration::seconds(60);
 let t1 = t2 - chrono::Duration::seconds(120);

 let mut first = record(url, "ok", t2);
 first.etag = Some("\"v1\"".to_string());
 page_text(&mut first, "rejected body");
 db.record_fetch(first).await.unwrap();

 // An older completion: the attempt gate rejects it, so both halves must come
 // from the pair already committed.
 let mut not_modified = record(url, "not_modified", t1);
 not_modified.status = Some(304);
 not_modified.sent_validators_seq = Some(1);
 let attribution = db
.record_fetch(not_modified)
.await
.unwrap()
.not_modified
.expect("304 attribution");
 assert!(attribution.bound);
 assert_eq!(
 attribution.attempt_at.map(rfc3339_secs),
 Some(secs(t2)),
 "the attempt time is the committed one, not a rejected half"
);
 assert_eq!(attribution.attempt_failed, Some(false));

 let after = page(&db, url).await;
 assert_eq!(
 after
.last_attempt_at
.as_deref()
.and_then(web_widow::store::db::stored_secs),
 Some(secs(t2))
);
 assert_eq!(after.last_attempt_failed, Some(0));
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn secs(at: DateTime<Utc>) -> String {
 rfc3339_secs(at)
}
