//! B05 — change monitoring (IMP-…B5-9 and B5-11).
//!
//! Hermetic: every monitor watches the loopback fixture server. B5-10
//! (render-mode monitoring) lives in `monitor_render.rs` because it needs Chrome.
//! B5-11 runs the real `run-due` binary so the cron path is exercised, not
//! simulated.

mod common;

use std::time::Duration;

use common::{Fixture, TestEnv, start_fixture, test_config};
use serde_json::{Value, json};
use web_widow::config::Config;
use web_widow::extract::Mode;
use web_widow::monitor::{
 MonitorChangesInput, MonitorCheckInput, MonitorCheckPayload, MonitorCreateInput,
 MonitorCreatePayload, MonitorListInput,
};

fn page(body: &str) -> String {
 format!(
 "<!doctype html><html><head><title>Monitor page</title></head><body>\
 <main><article><h1>Monitor page</h1>{body}</article></main></body></html>"
)
}

async fn env_with(patch: impl FnOnce(&mut Config)) -> (Fixture, TestEnv) {
 let fixture = start_fixture().await;
 let mut config = test_config();
 patch(&mut config);
 (fixture, TestEnv::new(config).await)
}

async fn create(
 env: &TestEnv,
 url: &str,
 mode: Mode,
 schema: Option<Value>,
 ignore_selectors: Vec<&str>) -> i64 {
 env.monitors
.create(MonitorCreateInput {
 url: url.to_string(),
 mode: Some(mode),
 schema,
 interval_s: Some(3600),
 ignore_selectors: ignore_selectors.into_iter().map(str::to_string).collect(),
 label: None,
 })
.await
.expect("monitor created")
.monitor_id
}

async fn check(env: &TestEnv, monitor_id: i64) -> MonitorCheckPayload {
 env.monitors
.check(MonitorCheckInput {
 monitor_id: Some(monitor_id),
 all: None,
 force: Some(true),
 wait_ms: None,
 })
.await
.expect("check ran")
}

fn snapshot_count(env: &TestEnv, monitor_id: i64) -> i64 {
 env.db()
.query_row(
 "SELECT COUNT(*) FROM snapshots WHERE monitor_id = ?1",
 [monitor_id],
 |row| row.get(0))
.unwrap()
}

fn change_count(env: &TestEnv, monitor_id: i64) -> i64 {
 env.db()
.query_row(
 "SELECT COUNT(*) FROM changes WHERE monitor_id = ?1",
 [monitor_id],
 |row| row.get(0))
.unwrap()
}

// ---------------------------------------------------------------------------
// B5-1 — unchanged page: second check is a 304, one snapshot, no blob
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unchanged_check_is_a_304_with_no_new_blob_and_one_snapshot() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page(
 "/monitor/page",
 &page("<p>The keeper counted forty-one gulls at the lighthouse.</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/page"),
 Mode::Main,
 None,
 vec![])
.await;

 let first = check(&env, monitor).await;
 assert_eq!(first.checked, 1, "{first:?}");
 assert!(
 first.changed.is_empty(),
 "baseline is not a change: {first:?}"
);
 assert_eq!(snapshot_count(&env, monitor), 1);
 let blobs_after_first = env.blob_count();
 assert!(
 blobs_after_first >= 2,
 "raw body + normalized text are stored"
);

 let second = check(&env, monitor).await;
 assert_eq!(second.checked, 1, "{second:?}");
 assert!(second.changed.is_empty(), "no change expected: {second:?}");
 assert_eq!(snapshot_count(&env, monitor), 2, "one snapshot per check");
 assert_eq!(
 env.blob_count(),
 blobs_after_first,
 "a 304 must not create a blob"
);
 assert_eq!(change_count(&env, monitor), 0);

 let db = env.db();
 let (status, event_id): (i64, i64) = db
.query_row(
 "SELECT fe.http_status, s.fetch_event_id FROM snapshots s
 JOIN fetch_events fe ON fe.id = s.fetch_event_id
 WHERE s.monitor_id = ?1 ORDER BY s.id DESC LIMIT 1",
 [monitor],
 |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap();
 assert_eq!(status, 304, "the second check must be a conditional 304");
 let mode: String = db
.query_row(
 "SELECT mode FROM fetch_events WHERE id = ?1",
 [event_id],
 |row| row.get(0))
.unwrap();
 assert_eq!(mode, "monitor");
 let changes = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: None,
 })
.await
.unwrap();
 assert!(changes.changes.is_empty());
}

// ---------------------------------------------------------------------------
// B5-2 — whitespace is cosmetic, a changed word is substantive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn whitespace_is_cosmetic_and_a_changed_word_is_substantive() {
 let (fixture, env) = env_with(|_| {}).await;
 // `full` + `pre` keeps the whitespace verbatim: readability/plain-text
 // conversion would collapse an internal double space and hide the case.
 fixture.set_page(
 "/monitor/text",
 &page("<pre>alpha beta gamma delta</pre><p>second stable paragraph here</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/text"),
 Mode::Full,
 None,
 vec![])
.await;
 check(&env, monitor).await;

 // A whitespace change that survives normalization (an internal double space).
 fixture.set_page(
 "/monitor/text",
 &page("<pre>alpha beta gamma delta</pre><p>second stable paragraph here</p>"));
 let cosmetic = check(&env, monitor).await;
 assert_eq!(cosmetic.changed.len(), 1, "{cosmetic:?}");
 assert_eq!(cosmetic.changed[0].kind, "cosmetic", "{cosmetic:?}");

 // A line-ending-only change is normalized away: no change row at all.
 fixture.set_page(
 "/monitor/text",
 "<!doctype html>\r\n<html><head><title>Monitor page</title></head><body><main><article>\
 <h1>Monitor page</h1><pre>alpha beta gamma delta</pre>\
 <p>second stable paragraph here</p></article></main></body></html>\r\n");
 let normalized_away = check(&env, monitor).await;
 assert!(
 normalized_away.changed.is_empty(),
 "CRLF/trailing-space churn must not be a change: {normalized_away:?}"
);

 // One word inside a paragraph is substantive and the diff shows it.
 fixture.set_page(
 "/monitor/text",
 &page("<pre>alpha beta gamma zeta</pre><p>second stable paragraph here</p>"));
 let substantive = check(&env, monitor).await;
 assert_eq!(substantive.changed.len(), 1, "{substantive:?}");
 assert_eq!(
 substantive.changed[0].kind, "substantive",
 "{substantive:?}"
);
 assert!(
 substantive.changed[0].summary.contains("lines changed"),
 "{substantive:?}"
);

 let history = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(true),
 })
.await
.unwrap();
 assert_eq!(history.changes.len(), 2, "cosmetic + substantive");
 assert_eq!(history.changes[0].kind, "substantive");
 assert_eq!(history.changes[1].kind, "cosmetic");
 assert!(
 history.changes[0]
.diff
.as_deref()
.unwrap_or_default()
.contains("zeta"),
 "the diff must contain the changed word"
);
 assert_eq!(history.changes[0].stats.changed_lines, 2);

 // Pagination is newest-first and the cursor is bound to the monitor.
 let first_page = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: Some(1),
 cursor: None,
 include_diff: None,
 })
.await
.unwrap();
 assert_eq!(first_page.changes.len(), 1);
 let cursor = first_page
.next_cursor
.clone()
.expect("a full page has a cursor");
 let second_page = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: Some(1),
 cursor: Some(cursor.clone()),
 include_diff: None,
 })
.await
.unwrap();
 assert_eq!(second_page.changes.len(), 1);
 assert!(second_page.next_cursor.is_none(), "two changes, two pages");
 assert_eq!(second_page.changes[0].kind, "cosmetic");
 assert_ne!(
 first_page.changes[0].change_id,
 second_page.changes[0].change_id
);

 let other = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/other"),
 Mode::Main,
 None,
 vec![])
.await;
 let wrong_monitor = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: other,
 limit: Some(1),
 cursor: Some(cursor),
 include_diff: None,
 })
.await
.unwrap_err();
 assert_eq!(wrong_monitor.code(), web_widow::ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// B5-3 — ignore selectors are effective and load-bearing
// ---------------------------------------------------------------------------

fn chrome_page(ad: &str, footer: &str) -> String {
 format!(
 "<!doctype html><html><head><title>Monitor page</title></head><body>\
 <div class=\"ad-slot\">{ad}</div>\
 <main><article><h1>Stable title</h1><p>Stable body text that never changes.</p></article></main>\
 <footer>{footer}</footer></body></html>"
)
}

#[tokio::test]
async fn ignore_selectors_suppress_nav_churn_and_are_load_bearing() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page("/monitor/chrome", &chrome_page("Ad v1", "Footer v1"));
 let url = fixture.url("127.0.0.1", "/monitor/chrome");
 // `full` keeps the ad/footer in the extraction; `main` readability would
 // already drop most of it for reasons unrelated to the selectors.
 let ignored = create(&env, &url, Mode::Full, None, vec![".ad-slot", "footer"]).await;
 let kept = create(&env, &url, Mode::Full, None, vec![]).await;
 check(&env, ignored).await;
 check(&env, kept).await;

 fixture.set_page("/monitor/chrome", &chrome_page("Ad v2", "Footer v2"));
 let without_noise = check(&env, ignored).await;
 assert!(
 without_noise.changed.is_empty(),
 "ignored subtrees must not produce a change: {without_noise:?}"
);
 let with_noise = check(&env, kept).await;
 assert_eq!(with_noise.changed.len(), 1, "{with_noise:?}");
 assert_eq!(with_noise.changed[0].kind, "substantive");
}

// ---------------------------------------------------------------------------
// B5-4 — markup churn with identical text is no change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn script_nonce_and_class_churn_are_not_content_changes() {
 let (fixture, env) = env_with(|_| {}).await;
 let body = |nonce: &str, class: &str| {
 format!(
 "<!doctype html><html><head><title>Monitor page</title>\
 <script nonce=\"{nonce}\">window.x = 1;</script></head>\
 <body class=\"{class}\"><main><article><h1>Stable</h1>\
 <p>This paragraph is the content and it does not change.</p>\
 </article></main><script nonce=\"{nonce}\">window.y = 2;</script></body></html>"
)
 };
 fixture.set_page("/monitor/churn", &body("n1", "theme-a"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/churn"),
 Mode::Main,
 None,
 vec![])
.await;
 check(&env, monitor).await;

 fixture.set_page("/monitor/churn", &body("n2", "theme-b"));
 let result = check(&env, monitor).await;
 assert!(result.changed.is_empty(), "{result:?}");
 assert_eq!(change_count(&env, monitor), 0);

 let db = env.db();
 let mut statement = db
.prepare(
 "SELECT raw_hash, normalized_hash FROM snapshots WHERE monitor_id = ?1 ORDER BY id")
.unwrap();
 let rows: Vec<(String, String)> = statement
.query_map([monitor], |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap()
.flatten()
.collect();
 assert_eq!(rows.len(), 2);
 assert_ne!(rows[0].0, rows[1].0, "the raw body really changed");
 assert_eq!(
 rows[0].1, rows[1].1,
 "the normalized content is identical, so no change"
);
}

// ---------------------------------------------------------------------------
// B5-5 — field-level monitoring names the changed field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_schema_monitor_names_the_changed_field_without_page_diff_noise() {
 let (fixture, env) = env_with(|_| {}).await;
 let body = |price: &str, visible: &str, prose: &str| {
 format!(
 "<!doctype html><html><head><title>Widget</title></head><body>\
 <h1>Acme Widget</h1>\
 <span id=\"price\" data-price=\"{price}\">{visible}</span>\
 <p id=\"body\">{prose}</p></body></html>"
)
 };
 fixture.set_page(
 "/monitor/field",
 &body("10", "€10.00", "Stable description."));
 let schema = json!({
 "fields": {
 "title": { "selector": "h1" },
 "price": { "selector": "#price", "attr": "data-price" },
 "body": { "selector": "#body" }
 }
 });
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/field"),
 Mode::Main,
 Some(schema),
 vec![])
.await;
 check(&env, monitor).await;

 // Only the attribute changes; the visible Markdown is identical.
 fixture.set_page(
 "/monitor/field",
 &body("11", "€10.00", "Stable description."));
 let changed = check(&env, monitor).await;
 assert_eq!(changed.changed.len(), 1, "{changed:?}");
 assert_eq!(changed.changed[0].kind, "substantive", "{changed:?}");
 assert!(
 changed.changed[0].summary.contains("price"),
 "the field change is named: {changed:?}"
);

 let history = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(true),
 })
.await
.unwrap();
 assert_eq!(history.changes.len(), 1);
 assert_eq!(history.changes[0].stats.changed_fields, vec!["price"]);
 assert_eq!(history.changes[0].stats.changed_lines, 0);
 assert!(
 history.changes[0]
.diff
.as_deref()
.unwrap_or_default()
.is_empty(),
 "an attribute-only change has no text diff"
);

 // The B02 rejection still applies through monitor_create.
 let error = env
.monitors
.create(MonitorCreateInput {
 url: fixture.url("127.0.0.1", "/monitor/field"),
 mode: None,
 schema: Some(json!({
 "fields": { "t": { "selector": "h1", "attr": "data-x", "transform": "text" } }
 })),
 interval_s: None,
 ignore_selectors: Vec::new(),
 label: None,
 })
.await
.unwrap_err();
 assert_eq!(error.code(), web_widow::ErrorCode::SchemaInvalid);
 assert!(
 error
.detail()
.unwrap_or_default()
.contains("/fields/t/attr")
);
}

// ---------------------------------------------------------------------------
// B02 rules still hold through a monitor: `all: true` with no match is `[]`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_true_with_no_matches_is_an_empty_array_not_a_failure() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page("/monitor/all", &page("<p>no tags on this page</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/all"),
 Mode::Main,
 Some(json!({ "fields": { "tags": { "selector": ".tag", "all": true } } })),
 vec![])
.await;
 check(&env, monitor).await;

 let db = env.db();
 let result_hash: String = db
.query_row(
 "SELECT result_hash FROM snapshots WHERE monitor_id = ?1 ORDER BY id DESC LIMIT 1",
 [monitor],
 |row| row.get(0))
.unwrap();
 let bytes = env
.store
.blobs
.read(&result_hash)
.await
.unwrap()
.expect("records blob");
 let records: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
 assert_eq!(records.len(), 1);
 assert_eq!(records[0]["tags"], json!([]));
}

// ---------------------------------------------------------------------------
// B5-6 — newest-first, capped diff with full stats
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_large_change_is_capped_but_its_stats_are_complete() {
 let (fixture, env) = env_with(|_| {}).await;
 let body = |version: usize| {
 let mut body =
 String::from("<!doctype html><html><head><title>Big</title></head><body><h1>Big</h1>");
 for index in 0..600 {
 body.push_str(&format!(
 "<p>Line {index} version {version}: the monitor diff fixture changes every paragraph.</p>"
));
 }
 body.push_str("</body></html>");
 body
 };
 fixture.set_page("/monitor/big", &body(1));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/big"),
 Mode::Full,
 None,
 vec![])
.await;
 check(&env, monitor).await;

 fixture.set_page("/monitor/big", &body(2));
 let changed = check(&env, monitor).await;
 assert_eq!(changed.changed.len(), 1, "{changed:?}");

 let history = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(true),
 })
.await
.unwrap();
 let change = &history.changes[0];
 assert!(
 change.truncated,
 "a 600-line change must exceed the diff cap"
);
 let diff = change.diff.as_deref().unwrap_or_default();
 assert!(
 diff.len() <= env.config.monitor.max_diff_bytes,
 "diff {} bytes exceeds the cap",
 diff.len()
);
 assert_eq!(
 change.stats.changed_lines, 1200,
 "600 deleted + 600 inserted"
);
 assert_eq!(change.stats.lines_added, 600);
 assert_eq!(change.stats.lines_removed, 600);

 // include_diff: false drops the text but keeps the stats and the flag.
 let stats_only = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(false),
 })
.await
.unwrap();
 assert!(stats_only.changes[0].diff.is_none());
 assert!(stats_only.changes[0].truncated);
 assert_eq!(stats_only.changes[0].stats.changed_lines, 1200);
}

// ---------------------------------------------------------------------------
// B5-7 — retention keeps change points and their referenced snapshots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_retention_keeps_change_points_and_their_diffs() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page(
 "/monitor/retention",
 &page("<p>version one of the retained page</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/retention"),
 Mode::Main,
 None,
 vec![])
.await;
 check(&env, monitor).await; // id 1, baseline

 fixture.set_page(
 "/monitor/retention",
 &page("<p>version two of the retained page, a real change</p>"));
 check(&env, monitor).await; // id 2, change point (from id 1)

 // 23 unchanged checks: 304s, one snapshot each, id 3..25.
 for _ in 0..23 {
 let outcome = check(&env, monitor).await;
 assert!(outcome.changed.is_empty(), "unexpected change: {outcome:?}");
 }
 assert_eq!(snapshot_count(&env, monitor), 25);
 assert_eq!(change_count(&env, monitor), 1);

 env.store
.gc(env.config.retention.clone(), env.config.auth.audit_days, 7)
.await
.expect("retention pass");

 let db = env.db();
 let mut statement = db
.prepare("SELECT id FROM snapshots WHERE monitor_id = ?1 ORDER BY id")
.unwrap();
 let ids: Vec<i64> = statement
.query_map([monitor], |row| row.get(0))
.unwrap()
.flatten()
.collect();
 assert_eq!(
 ids.len(),
 22,
 "20 newest plus the change point and its from-snapshot: {ids:?}"
);
 assert!(ids.contains(&1), "the change's from-snapshot survives");
 assert!(ids.contains(&2), "the change point survives");
 assert!(!ids.contains(&3) && !ids.contains(&4) && !ids.contains(&5));

 // The change row and its diff are still readable after pruning.
 let history = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(true),
 })
.await
.unwrap();
 assert_eq!(history.changes.len(), 1);
 assert!(
 history.changes[0]
.diff
.as_deref()
.unwrap_or_default()
.contains("version two"),
 "the surviving diff is still renderable"
);
}

// ---------------------------------------------------------------------------
// B5-8 — bulk check is polite and returns a job id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_check_hits_each_host_at_the_configured_rate_and_returns_a_job() {
 let (fixture, env) = env_with(|config| {
 config.fetch.min_interval_ms = 50;
 config.fetch.jitter_ms = 0;
 })
.await;
 let mut ids = Vec::new();
 for index in 0..5 {
 let path = format!("/monitor/rate{index}");
 fixture.set_page(&path, &page(&format!("<p>rate page {index} content</p>")));
 ids.push(
 create(
 &env,
 &fixture.url("127.0.0.1", &path),
 Mode::Main,
 None,
 vec![])
.await);
 }
 fixture.log.clear();

 let payload = env
.monitors
.check(MonitorCheckInput {
 monitor_id: None,
 all: Some(true),
 force: Some(true),
 wait_ms: None,
 })
.await
.expect("bulk check enqueued");
 assert!(payload.pending, "a queued bulk check is pending");
 assert_eq!(payload.checked, 0);
 let job_id = payload.job_id.expect("bulk check returns a job id");

 let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
 let status = loop {
 let status = env.crawler.status(&job_id).await.expect("job status");
 if !matches!(status.state.as_str(), "queued" | "running") {
 break status;
 }
 assert!(
 tokio::time::Instant::now() < deadline,
 "monitor check job did not finish: {status:?}"
);
 tokio::time::sleep(Duration::from_millis(25)).await;
 };
 assert_eq!(status.state, "done", "{status:?}");
 assert_eq!(status.counters.fetched, 5, "{status:?}");
 assert_eq!(status.counters.failed, 0, "{status:?}");

 let stamps = fixture.log.stamps_matching("/monitor/rate");
 assert_eq!(stamps.len(), 5, "one request per monitor");
 for pair in stamps.windows(2) {
 let gap = pair[1].duration_since(pair[0]);
 assert!(
 gap >= Duration::from_millis(45),
 "per-host interval violated: {gap:?}"
);
 }
 for id in ids {
 assert_eq!(snapshot_count(&env, id), 1, "each monitor was checked once");
 }
}

// ---------------------------------------------------------------------------
// B5-9 — delete is a tombstone
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_deactivates_but_keeps_history_and_checks_skip_it() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page("/monitor/tombstone", &page("<p>first version</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/tombstone"),
 Mode::Main,
 None,
 vec![])
.await;
 check(&env, monitor).await;
 fixture.set_page("/monitor/tombstone", &page("<p>second version changed</p>"));
 check(&env, monitor).await;
 assert_eq!(change_count(&env, monitor), 1);

 let deleted = env
.monitors
.delete(web_widow::monitor::MonitorDeleteInput {
 monitor_id: monitor,
 })
.await
.expect("delete");
 assert_eq!(deleted.monitor_id, monitor);
 assert!(!deleted.active);

 let requests_before = fixture.log.count_matching("/monitor/tombstone");
 let skipped = check(&env, monitor).await;
 assert_eq!(
 skipped.checked, 0,
 "an inactive monitor is skipped: {skipped:?}"
);
 assert_eq!(
 fixture.log.count_matching("/monitor/tombstone"),
 requests_before,
 "a skipped check makes no request"
);

 let history = env
.monitors
.changes(MonitorChangesInput {
 monitor_id: monitor,
 limit: None,
 cursor: None,
 include_diff: Some(true),
 })
.await
.unwrap();
 assert_eq!(history.changes.len(), 1, "history remains queryable");
 assert_eq!(snapshot_count(&env, monitor), 2);

 let missing = env
.monitors
.delete(web_widow::monitor::MonitorDeleteInput {
 monitor_id: 999_999,
 })
.await
.unwrap_err();
 assert_eq!(missing.code(), web_widow::ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// B5-11 — the real run-due binary checks only due monitors and exits
// ---------------------------------------------------------------------------

fn received(response: &serde_json::Value) -> &serde_json::Value {
 &response["result"]["structuredContent"]
}

async fn mcp_create(child: &mut common::McpChild, url: &str) -> i64 {
 let response = child
.call(
 "monitor_create",
 json!({ "url": url, "interval_s": 3600, "ignore_selectors": [] }))
.await;
 assert_eq!(response["result"]["isError"], json!(false), "{response}");
 received(&response)["monitor_id"]
.as_i64()
.expect("monitor id")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_due_checks_only_due_monitors_advances_schedule_and_exits() {
 let fixture = start_fixture().await;
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "");
 let data = dir.path().join("data");
 for index in 0..3 {
 fixture.set_page(
 &format!("/monitor/due{index}"),
 &page(&format!("<p>due page {index}</p>")));
 }

 let mut child = common::McpChild::spawn(&config, &data).await;
 let _first = mcp_create(&mut child, &fixture.url("127.0.0.1", "/monitor/due0")).await;
 let second = mcp_create(&mut child, &fixture.url("127.0.0.1", "/monitor/due1")).await;
 let third = mcp_create(&mut child, &fixture.url("127.0.0.1", "/monitor/due2")).await;
 for monitor_id in [second, third] {
 let response = child
.call(
 "monitor_check",
 json!({ "monitor_id": monitor_id, "force": true }))
.await;
 assert_eq!(response["result"]["isError"], json!(false), "{response}");
 assert_eq!(
 received(&response)["checked"],
 json!(1),
 "explicit check: {response}"
);
 }
 child.shutdown().await;

 // run 1: only `first` is due (the other two just advanced by 3600 s).
 let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
.arg("--config")
.arg(&config)
.arg("--data-dir")
.arg(&data)
.arg("run-due")
.output()
.await
.expect("run-due starts");
 assert!(
 output.status.success(),
 "run-due failed: {}",
 String::from_utf8_lossy(&output.stderr)
);
 let stdout = String::from_utf8_lossy(&output.stdout);
 let line = stdout
.lines()
.find(|line| line.trim_start().starts_with('{'))
.expect("a JSON summary on stdout");
 let summary: Value = serde_json::from_str(line).expect("valid JSON summary");
 assert_eq!(summary["checked"], json!(1), "{summary}");
 assert_eq!(summary["skipped"], json!(2), "{summary}");
 assert_eq!(summary["failed"], json!([]), "{summary}");
 assert!(
 fixture.log.count_matching("/monitor/due0") >= 1,
 "the due monitor was fetched"
);

 // run 2: nothing is due now, and the lock from run 1 did not survive.
 let second_run = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
.arg("--config")
.arg(&config)
.arg("--data-dir")
.arg(&data)
.arg("run-due")
.output()
.await
.expect("second run-due starts");
 assert!(
 second_run.status.success(),
 "second run-due failed: {}",
 String::from_utf8_lossy(&second_run.stderr)
);
 let stdout = String::from_utf8_lossy(&second_run.stdout);
 let line = stdout
.lines()
.find(|line| line.trim_start().starts_with('{'))
.expect("a JSON summary on stdout");
 let summary: Value = serde_json::from_str(line).expect("valid JSON summary");
 assert_eq!(summary["checked"], json!(0), "{summary}");
 assert_eq!(summary["skipped"], json!(3), "{summary}");
}

// ---------------------------------------------------------------------------
// Repair (B5-10 second pass): a pre-v4 NULL render mode is unknown, not static
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pre_v4_unknown_snapshot_mode_never_produces_a_304() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page(
 "/monitor/unknown-mode",
 &page("<p>stable pre-v4 monitoring page</p>"));
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/unknown-mode"),
 Mode::Main,
 None,
 vec![])
.await;

 let baseline = check(&env, monitor).await;
 assert_eq!(baseline.checked, 1, "{baseline:?}");
 assert!(baseline.changed.is_empty(), "{baseline:?}");

 // Simulate a pre-v4 row: the snapshot exists but its render mode is unknown.
 // Under the old predicate NULL was treated like "no previous" and allowed,
 // so a retained static ETag would 304 and skip the check's body fetch.
 env.db()
.execute(
 "UPDATE snapshots SET render_mode = NULL WHERE monitor_id = ?1",
 [monitor])
.unwrap();

 let second = check(&env, monitor).await;
 assert_eq!(second.checked, 1, "{second:?}");
 assert!(second.changed.is_empty(), "{second:?}");

 let (status, mode): (i64, Option<String>) = env
.db()
.query_row(
 "SELECT fe.http_status, s.render_mode FROM snapshots s
 JOIN fetch_events fe ON fe.id = s.fetch_event_id
 WHERE s.monitor_id = ?1 ORDER BY s.id DESC LIMIT 1",
 [monitor],
 |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap();
 assert_eq!(
 status, 200,
 "an unknown/null mode must not be treated as static: no 304 may be accepted"
);
 assert_eq!(
 mode.as_deref(),
 Some("static"),
 "the new check records its actual mode, so the next one may revalidate"
);
 assert_eq!(snapshot_count(&env, monitor), 2);
}

// ---------------------------------------------------------------------------
// Error policy: a permanent 404 is one terminal check result, not three
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permanent_404_is_one_terminal_check_and_advances_the_schedule() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.log.clear();
 let monitor = create(
 &env,
 &fixture.url("127.0.0.1", "/monitor/missing"),
 Mode::Main,
 None,
 vec![])
.await;
 let first = check(&env, monitor).await;
 assert_eq!(first.checked, 1);
 assert!(first.changed.is_empty());
 assert_eq!(first.failed.len(), 1, "{first:?}");
 assert_eq!(first.failed[0].code, "http_error");
 assert_eq!(
 fixture.log.count_matching("/monitor/missing"),
 1,
 "a permanent 404 must be requested exactly once: {:?}",
 fixture.log.entries()
);
 assert_eq!(snapshot_count(&env, monitor), 0, "no fabricated snapshot");
 assert_eq!(change_count(&env, monitor), 0);

 let db = env.db();
 let (last_checked, next_due): (String, String) = db
.query_row(
 "SELECT last_checked_at, next_due_at FROM monitors WHERE id = ?1",
 [monitor],
 |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap();
 assert!(!last_checked.is_empty());
 assert!(
 next_due > last_checked,
 "the one result advances the schedule"
);

 // The failed check is a stored fetch event, not a lost result.
 let events: i64 = db
.query_row(
 "SELECT COUNT(*) FROM fetch_events WHERE mode = 'monitor' AND http_status = 404",
 [],
 |row| row.get(0))
.unwrap();
 assert_eq!(events, 1);
}

// ---------------------------------------------------------------------------
// Input validation and not-found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn monitor_inputs_are_validated() {
 let (fixture, env) = env_with(|_| {}).await;
 let url = fixture.url("127.0.0.1", "/monitor/page");
 let raw = env
.monitors
.create(MonitorCreateInput {
 url: url.clone(),
 mode: Some(Mode::Raw),
 schema: None,
 interval_s: None,
 ignore_selectors: Vec::new(),
 label: None,
 })
.await
.unwrap_err();
 assert_eq!(raw.code(), web_widow::ErrorCode::SchemaInvalid);

 let short = env
.monitors
.create(MonitorCreateInput {
 url: url.clone(),
 mode: None,
 schema: None,
 interval_s: Some(1),
 ignore_selectors: Vec::new(),
 label: None,
 })
.await
.unwrap_err();
 assert_eq!(short.code(), web_widow::ErrorCode::SchemaInvalid);

 let selector = env
.monitors
.create(MonitorCreateInput {
 url: url.clone(),
 mode: None,
 schema: None,
 interval_s: None,
 ignore_selectors: vec![":::not-a-selector".to_string()],
 label: None,
 })
.await
.unwrap_err();
 assert_eq!(selector.code(), web_widow::ErrorCode::SchemaInvalid);
 assert!(
 selector
.detail()
.unwrap_or_default()
.contains("/ignore_selectors/0")
);

 let both = env
.monitors
.check(MonitorCheckInput {
 monitor_id: Some(1),
 all: Some(true),
 force: None,
 wait_ms: None,
 })
.await
.unwrap_err();
 assert_eq!(both.code(), web_widow::ErrorCode::SchemaInvalid);

 let missing = env
.monitors
.check(MonitorCheckInput {
 monitor_id: Some(999_999),
 all: None,
 force: None,
 wait_ms: None,
 })
.await
.unwrap_err();
 assert_eq!(missing.code(), web_widow::ErrorCode::NotFound);

 let created: MonitorCreatePayload = env
.monitors
.create(MonitorCreateInput {
 url: format!("{url}?utm_source=news"),
 mode: None,
 schema: None,
 interval_s: None,
 ignore_selectors: Vec::new(),
 label: None,
 })
.await
.unwrap();
 let db = env.db();
 let stored: String = db
.query_row(
 "SELECT url FROM monitors WHERE id = ?1",
 [created.monitor_id],
 |row| row.get(0))
.unwrap();
 assert_eq!(stored, url, "the monitor stores the canonical URL");
}

// ---------------------------------------------------------------------------
// B02 — monitor_list, label and monitor_check(wait_ms)
// ---------------------------------------------------------------------------

async fn create_labeled(
 env: &TestEnv,
 url: &str,
 schema: Option<Value>,
 label: Option<&str>) -> i64 {
 env.monitors
.create(MonitorCreateInput {
 url: url.to_string(),
 mode: None,
 schema,
 interval_s: Some(3600),
 ignore_selectors: Vec::new(),
 label: label.map(str::to_string),
 })
.await
.expect("monitor created")
.monitor_id
}

fn monitor_check_job_count(env: &TestEnv) -> i64 {
 env.db()
.query_row(
 "SELECT COUNT(*) FROM jobs WHERE kind = 'monitor_check'",
 [],
 |row| row.get(0))
.unwrap()
}

async fn wait_for_job(env: &TestEnv, job_id: &str) -> web_widow::crawl::CrawlStatusPayload {
 let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
 loop {
 let status = env.crawler.status(job_id).await.expect("job status");
 if !matches!(status.state.as_str(), "queued" | "running") {
 return status;
 }
 assert!(
 tokio::time::Instant::now() < deadline,
 "monitor check job did not finish: {status:?}"
);
 tokio::time::sleep(Duration::from_millis(25)).await;
 }
}

#[tokio::test]
async fn monitor_list_orders_labels_and_inactive_rows() {
 let (fixture, env) = env_with(|_| {}).await;
 for path in ["/monitor/list-a", "/monitor/list-b", "/monitor/list-c"] {
 fixture.set_page(path, &page("<p>list content</p>"));
 }
 let labeled = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/list-a"),
 None,
 Some("project:alpha"))
.await;
 let unlabeled = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/list-b"),
 None,
 None)
.await;
 let with_schema = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/list-c"),
 Some(json!({ "fields": { "title": { "selector": "h1" } } })),
 None)
.await;
 let tombstone = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/list-a"),
 None,
 None)
.await;
 env.monitors
.delete(web_widow::monitor::MonitorDeleteInput {
 monitor_id: tombstone,
 })
.await
.unwrap();

 let payload = env
.monitors
.list(MonitorListInput {
 include_inactive: None,
 limit: None,
 })
.await
.unwrap();
 assert_eq!(payload.count, 3);
 assert_eq!(payload.total, 3);
 assert!(!payload.truncated);
 assert_eq!(payload.limit_hit, None);
 let ids: Vec<i64> = payload.monitors.iter().map(|row| row.monitor_id).collect();
 assert_eq!(ids, vec![labeled, unlabeled, with_schema], "id order");
 assert_eq!(payload.monitors[0].label.as_deref(), Some("project:alpha"));
 assert!(payload.monitors[1].label.is_none());
 assert!(!payload.monitors[1].has_schema);
 assert!(payload.monitors[2].has_schema);
 assert!(payload.monitors.iter().all(|row| row.active));
 assert_eq!(payload.monitors[0].interval_s, 3600);
 assert!(payload.monitors[0].created_at.ends_with('Z'));
 assert!(payload.monitors[0].next_due_at.ends_with('Z'));

 let all = env
.monitors
.list(MonitorListInput {
 include_inactive: Some(true),
 limit: None,
 })
.await
.unwrap();
 assert_eq!(all.count, 4);
 assert_eq!(all.total, 4);
 assert!(
 all.monitors
.iter()
.any(|row| row.monitor_id == tombstone && !row.active && row.label.is_none())
);

 let page = env
.monitors
.list(MonitorListInput {
 include_inactive: None,
 limit: Some(2),
 })
.await
.unwrap();
 assert_eq!(page.count, 2);
 assert_eq!(page.total, 3);
 assert!(page.truncated);
 assert_eq!(page.limit_hit.as_deref(), Some("limit"));
}

#[tokio::test]
async fn label_is_trimmed_duplicates_allowed_and_invalid_values_rejected() {
 let (fixture, env) = env_with(|_| {}).await;
 let url = fixture.url("127.0.0.1", "/monitor/label");
 fixture.set_page("/monitor/label", &page("<p>label content</p>"));

 let first = create_labeled(&env, &url, None, Some(" project:alpha ")).await;
 let second = create_labeled(&env, &url, None, Some("project:alpha")).await;
 let listed = env
.monitors
.list(MonitorListInput {
 include_inactive: None,
 limit: None,
 })
.await
.unwrap();
 let labels: Vec<Option<&str>> = listed
.monitors
.iter()
.filter(|row| row.monitor_id == first || row.monitor_id == second)
.map(|row| row.label.as_deref())
.collect();
 assert_eq!(labels, vec![Some("project:alpha"), Some("project:alpha")]);

 for bad in [
 String::new(),
 " ".to_string(),
 "x".repeat(65),
 "bad\nlabel".to_string(),
 ] {
 let error = env
.monitors
.create(MonitorCreateInput {
 url: url.clone(),
 mode: None,
 schema: None,
 interval_s: Some(3600),
 ignore_selectors: Vec::new(),
 label: Some(bad.clone()),
 })
.await
.unwrap_err();
 assert_eq!(error.code(), web_widow::ErrorCode::SchemaInvalid, "{bad:?}");
 assert!(
 error.detail().unwrap_or_default().contains("/label"),
 "{bad:?}: {error}"
);
 }
}

#[tokio::test]
async fn wait_ms_zero_keeps_the_async_path_with_pending() {
 let (fixture, env) = env_with(|config| {
 config.fetch.min_interval_ms = 10;
 config.fetch.jitter_ms = 0;
 })
.await;
 let mut ids = Vec::new();
 for index in 0..2 {
 let path = format!("/monitor/wait-zero{index}");
 fixture.set_page(&path, &page(&format!("<p>wait zero {index}</p>")));
 ids.push(create_labeled(&env, &fixture.url("127.0.0.1", &path), None, None).await);
 }
 let payload = env
.monitors
.check(MonitorCheckInput {
 monitor_id: None,
 all: Some(true),
 force: Some(true),
 wait_ms: Some(0),
 })
.await
.unwrap();
 assert!(payload.pending);
 assert_eq!(payload.checked, 0);
 let job_id = payload.job_id.expect("a queued check has a job id");
 let status = wait_for_job(&env, &job_id).await;
 assert_eq!(status.state, "done", "{status:?}");
 assert_eq!(status.counters.fetched, 2, "{status:?}");
 for id in ids {
 assert_eq!(snapshot_count(&env, id), 1);
 }
}

#[tokio::test]
async fn wait_ms_completes_inline_without_a_job() {
 let (fixture, env) = env_with(|_| {}).await;
 let mut ids = Vec::new();
 for index in 0..2 {
 let path = format!("/monitor/wait-fast{index}");
 fixture.set_page(&path, &page(&format!("<p>wait fast {index}</p>")));
 ids.push(create_labeled(&env, &fixture.url("127.0.0.1", &path), None, None).await);
 }
 let before = monitor_check_job_count(&env);
 let payload = env
.monitors
.check(MonitorCheckInput {
 monitor_id: None,
 all: Some(true),
 force: Some(true),
 wait_ms: Some(10_000),
 })
.await
.unwrap();
 assert!(!payload.pending);
 assert!(payload.job_id.is_none());
 assert_eq!(payload.checked, 2);
 assert_eq!(monitor_check_job_count(&env), before, "no job row");
 for id in ids {
 assert_eq!(snapshot_count(&env, id), 1);
 }
}

#[tokio::test]
async fn wait_ms_deadline_enqueues_the_remainder_exactly_once() {
 let (fixture, env) = env_with(|config| {
 config.fetch.min_interval_ms = 10;
 config.fetch.jitter_ms = 0;
 })
.await;
 let mut ids = Vec::new();
 for index in 0..3 {
 let path = format!("/crawl/slow?i={index}");
 ids.push(create_labeled(&env, &fixture.url("127.0.0.1", &path), None, None).await);
 }
 fixture.log.clear();

 let payload = env
.monitors
.check(MonitorCheckInput {
 monitor_id: None,
 all: Some(true),
 force: Some(true),
 wait_ms: Some(50),
 })
.await
.unwrap();
 assert!(payload.pending, "the remainder is still queued");
 assert_eq!(payload.checked, 1, "the in-flight check completed");
 let job_id = payload.job_id.expect("the remainder has a job id");
 let status = wait_for_job(&env, &job_id).await;
 assert_eq!(status.state, "done", "{status:?}");
 assert_eq!(status.counters.fetched, 2, "{status:?}");

 for (index, id) in ids.iter().enumerate() {
 assert_eq!(snapshot_count(&env, *id), 1, "monitor {id} checked once");
 assert_eq!(
 fixture
.log
.count_matching(&format!("/crawl/slow?i={index}")),
 1,
 "exactly one request for monitor {index}"
);
 }
 let spec: String = env
.db()
.query_row(
 "SELECT spec_json FROM jobs WHERE id = ?1",
 [&job_id],
 |row| row.get(0))
.unwrap();
 let spec: Value = serde_json::from_str(&spec).unwrap();
 assert_eq!(
 spec["monitor_ids"].as_array().map(Vec::len),
 Some(2),
 "the job carries the remaining ids: {spec}"
);
}

#[tokio::test]
async fn wait_ms_bounds_and_single_check_rejection() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page("/monitor/wait-bounds", &page("<p>bounds</p>"));
 let id = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/wait-bounds"),
 None,
 None)
.await;

 let with_id = env
.monitors
.check(MonitorCheckInput {
 monitor_id: Some(id),
 all: None,
 force: None,
 wait_ms: Some(100),
 })
.await
.unwrap_err();
 assert_eq!(with_id.code(), web_widow::ErrorCode::SchemaInvalid);
 assert!(with_id.detail().unwrap_or_default().contains("/wait_ms"));

 let over = env
.monitors
.check(MonitorCheckInput {
 monitor_id: None,
 all: Some(true),
 force: None,
 wait_ms: Some(env.config.monitor.max_wait_ms + 1),
 })
.await
.unwrap_err();
 assert_eq!(over.code(), web_widow::ErrorCode::SchemaInvalid);
 assert!(over.detail().unwrap_or_default().contains("/wait_ms"));
}

#[tokio::test]
async fn a_single_check_is_not_pending() {
 let (fixture, env) = env_with(|_| {}).await;
 fixture.set_page("/monitor/single-pending", &page("<p>single</p>"));
 let id = create_labeled(
 &env,
 &fixture.url("127.0.0.1", "/monitor/single-pending"),
 None,
 None)
.await;
 let payload = check(&env, id).await;
 assert!(!payload.pending);
 assert!(payload.job_id.is_none());
 assert_eq!(payload.checked, 1);
}
