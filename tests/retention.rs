//! retention/GC drill: budget eviction order, change-point survival
//! and no dangling content hash in any row.
//!
//! The drill deliberately mixes the reference sources the store has: a page raw
//! body, monitor snapshots (raw + normalized + schema result) and old fetch
//! events. It then forces a blob budget smaller than the stored set.

mod common;

use chrono::Utc;
use common::{TestEnv, start_fixture, test_config};
use serde_json::json;
use web_widow::config::RetentionConfig;
use web_widow::core::url::content_hash;
use web_widow::monitor::{MonitorChangesInput, MonitorCheckInput, MonitorCreateInput};
use web_widow::pipeline::{FetchPageInput, RenderPolicy};
use web_widow::store::{BlobRow, FetchRecord};

fn page(body: &str) -> String {
 format!(
 "<!doctype html><html><head><title>Retention</title></head><body>\
 <main><article><h1>Retention</h1>{body}</article></main></body></html>"
)
}

async fn check(env: &TestEnv, monitor_id: i64) {
 env.monitors
.check(MonitorCheckInput {
 monitor_id: Some(monitor_id),
 all: None,
 force: Some(true),
 wait_ms: None,
 })
.await
.expect("check ran");
}

#[tokio::test]
async fn gc_evicts_only_unreferenced_blobs_in_age_order_and_never_dangles() {
 let fixture = start_fixture().await;
 let env = TestEnv::new(test_config()).await;

 // A referenced raw body (~100 KiB) that must survive even over budget.
 env.pipeline
.fetch_page(FetchPageInput {
 url: fixture.url("127.0.0.1", "/large"),
 mode: None,
 render: Some(RenderPolicy::Never),
 freshness_s: None,
 max_chars: None,
 include_links: None,
 include_content: None,
 profile: None,
 })
.await
.expect("large page");

 // A monitor with a schema: one change point (raw + normalized + result
 // blobs) followed by enough unchanged checks to exceed the snapshot cap.
 fixture.set_page(
 "/monitor/retention-drill",
 &page("<p>version one of the retention drill page</p>"));
 let monitor = env
.monitors
.create(MonitorCreateInput {
 url: fixture.url("127.0.0.1", "/monitor/retention-drill"),
 mode: Some(web_widow::extract::Mode::Main),
 schema: Some(json!({ "fields": { "title": { "selector": "h1" } } })),
 interval_s: Some(3600),
 ignore_selectors: Vec::new(),
 label: None,
 })
.await
.expect("monitor created")
.monitor_id;
 check(&env, monitor).await;
 fixture.set_page(
 "/monitor/retention-drill",
 &page("<p>version two of the retention drill page, a real change</p>"));
 check(&env, monitor).await;
 for _ in 0..23 {
 check(&env, monitor).await;
 }

 // Three unreferenced blobs, oldest first. With a 50 KiB budget and the
 // referenced set below that, all three must be evicted oldest-first.
 let mut doomed: Vec<String> = Vec::new();
 for (index, age_days) in [3_i64, 2, 1].into_iter().enumerate() {
 let bytes = vec![b'a' + index as u8; 40 * 1024];
 let hash = env.store.put_blob(&bytes).await.expect("blob written");
 let now = Utc::now() - chrono::Duration::days(age_days);
 let url = format!("https://unreferenced.example/{index}");
 let mut record = FetchRecord::new(&url, &url, &content_hash(url.as_bytes()));
 record.site = "unreferenced.example".to_string();
 record.status = Some(200);
 record.outcome = "ok".to_string();
 record.fetched_at = now;
 record.duration_ms = 1;
 record.blob = Some(BlobRow {
 hash: hash.clone(),
 size: bytes.len() as i64,
 content_type: Some("text/html".to_string()),
 created_at: now,
 });
 env.store.db.record_fetch(record).await.expect("record");
 doomed.push(hash);
 }

 let retention = RetentionConfig {
 max_blob_bytes: 50 * 1024,
 snapshots_per_monitor: 20,
..RetentionConfig::default()
 };
 let report = env
.store
.gc(retention, env.config.auth.audit_days, 7)
.await
.expect("retention pass");

 // Budget eviction is oldest-first and leaves the referenced body alone.
 assert_eq!(
 report.removed_blobs, doomed,
 "unreferenced blobs must be evicted oldest first"
);
 assert!(report.snapshots_pruned >= 3, "{report:?}");

 let db = env.db();
 let snapshots: i64 = db
.query_row(
 "SELECT COUNT(*) FROM snapshots WHERE monitor_id = ?1",
 [monitor],
 |row| row.get(0))
.unwrap();
 assert_eq!(
 snapshots, 22,
 "20 newest + the change point and its from-snapshot"
);
 let change_points: i64 = db
.query_row(
 "SELECT COUNT(*) FROM snapshots WHERE monitor_id = ?1 AND is_change_point = 1",
 [monitor],
 |row| row.get(0))
.unwrap();
 assert_eq!(change_points, 1);
 let oldest_change_ref: i64 = db
.query_row(
 "SELECT COUNT(*) FROM snapshots WHERE monitor_id = ?1 AND id IN (1, 2)",
 [monitor],
 |row| row.get(0))
.unwrap();
 assert_eq!(
 oldest_change_ref, 2,
 "the change and its from-snapshot survive"
);

 // No dangling content hash in any row that names one. `extractions.result_hash`
 // hashes the inline result JSON, not a CAS blob, so it is not in this set.
 let mut hashes: Vec<String> = Vec::new();
 let mut statement = db
.prepare(
 "SELECT raw_hash FROM pages WHERE raw_hash IS NOT NULL
 UNION SELECT raw_hash FROM fetch_events WHERE raw_hash IS NOT NULL
 UNION SELECT raw_hash FROM snapshots
 UNION SELECT normalized_hash FROM snapshots
 UNION SELECT result_hash FROM snapshots WHERE result_hash IS NOT NULL")
.unwrap();
 for row in statement
.query_map([], |row| row.get::<_, String>(0))
.unwrap()
 {
 hashes.push(row.unwrap());
 }
 assert!(!hashes.is_empty(), "the drill produced referenced content");
 for hash in &hashes {
 let rows: i64 = db
.query_row(
 "SELECT COUNT(*) FROM blobs WHERE hash = ?1",
 [hash],
 |row| row.get(0))
.unwrap();
 assert_eq!(
 rows, 1,
 "row references {hash} but the blobs table has no row"
);
 let path = env
.data_dir()
.join("blobs")
.join(&hash[0..2])
.join(&hash[2..4])
.join(hash);
 assert!(
 path.is_file(),
 "row references {hash} but the file is missing"
);
 }

 // Every surviving blob row has bytes, and the change is still readable.
 let mut statement = db.prepare("SELECT hash FROM blobs").unwrap();
 let blobs: Vec<String> = statement
.query_map([], |row| row.get(0))
.unwrap()
.flatten()
.collect();
 for hash in &blobs {
 let path = env
.data_dir()
.join("blobs")
.join(&hash[0..2])
.join(&hash[2..4])
.join(hash);
 assert!(path.is_file(), "blob row {hash} has no file");
 }
 drop(statement);

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
 "the surviving change still renders its diff"
);
}
