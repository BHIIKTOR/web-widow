//! SIGTERM during a real crawl: the process exits within 10 s, the
//! data-directory lock is released, the throwaway Chrome profile is wiped and
//! the persisted job is in a clean, recoverable state.
//!
//! Uses a rendering crawl so the browser profile lifecycle is exercised, not
//! just the SQLite writer.

#![cfg(feature = "render")]

mod common;

use std::time::Duration;

use common::{Fixture, McpChild, start_fixture};
use serde_json::{Value, json};

fn received(response: &Value) -> &Value {
 &response["result"]["structuredContent"]
}

async fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
 let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
 while !predicate() {
 assert!(
 tokio::time::Instant::now() < deadline,
 "timed out waiting for {what}"
);
 tokio::time::sleep(Duration::from_millis(25)).await;
 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_during_a_rendering_crawl_exits_cleanly() {
 let Some(chrome) =
 common::skip_without_chrome("sigterm_during_a_rendering_crawl_exits_cleanly")
 else {
 return;
 };
 let fixture: Fixture = start_fixture().await;
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(
 dir.path(),
 &format!(
 "chrome_path = \"{}\"\n[crawl]\nlease_s = 2\nidle_poll_ms = 50\n",
 chrome.display()
));
 let data = dir.path().join("data");
 let profile = data.join("render").join("chrome-profile");

 let mut child = McpChild::spawn(&config, &data).await;
 let started = child
.call(
 "crawl_start",
 json!({
 "start_urls": [
 fixture.url("127.0.0.1", "/js/noscript"),
 fixture.url("127.0.0.1", "/bulk/")
 ],
 "render_policy": "thin_only",
 "limits": { "max_pages": 50, "per_host_rate": 5.0, "max_duration_s": 120 }
 }))
.await;
 assert_eq!(started["result"]["isError"], json!(false), "{started}");
 let job_id = received(&started)["job_id"]
.as_str()
.expect("job id")
.to_string();

 // The browser profile exists once the noscript page rendered, and at least
 // one bulk page proves the crawl is still working when the signal arrives.
 wait_until("the Chrome profile and crawl progress", || {
 profile.exists() && fixture.log.count_matching("/bulk/") >= 2
 })
.await;

 let pid = child.child.id().expect("child pid");
 let status = tokio::process::Command::new("kill")
.arg("-TERM")
.arg(pid.to_string())
.status()
.await
.unwrap();
 assert!(status.success());
 let exit = tokio::time::timeout(Duration::from_secs(10), child.child.wait())
.await
.expect("SIGTERM must end the process within 10 s")
.expect("wait");
 assert!(exit.success(), "{exit:?}");
 assert!(
 !profile.exists(),
 "the throwaway Chrome profile must be wiped on clean shutdown"
);

 // The advisory lock is released: a fresh one-shot process can open the data dir.
 let after = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
.arg("--config")
.arg(&config)
.arg("--data-dir")
.arg(&data)
.arg("run-due")
.output()
.await
.expect("run-due starts");
 assert!(
 after.status.success(),
 "lock was not released: {}",
 String::from_utf8_lossy(&after.stderr)
);

 // The job is persisted in a clean state (running/queued for lease recovery,
 // or cooperatively cancelled) — never a half-written row.
 let db = common::db_at(&data);
 let state: String = db
.query_row("SELECT state FROM jobs WHERE id = ?1", [&job_id], |row| {
 row.get(0)
 })
.unwrap();
 assert!(
 matches!(state.as_str(), "running" | "queued" | "cancelled" | "done"),
 "unexpected job state after SIGTERM: {state}"
);
 drop(db);

 // A restart can open the same data dir and report the job without a failure.
 let mut second = McpChild::spawn(&config, &data).await;
 let status = second
.call("crawl_status", json!({ "job_id": job_id }))
.await;
 assert_eq!(status["result"]["isError"], json!(false), "{status}");
 let resumed_state = received(&status)["state"]
.as_str()
.unwrap_or_default()
.to_string();
 assert_ne!(
 resumed_state, "failed",
 "restart must not fail the job: {status}"
);
 let _ = second
.call("crawl_cancel", json!({ "job_id": job_id }))
.await;
 second.shutdown().await;
}
