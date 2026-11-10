//! the static-only build has no browser, so `screenshot` is a typed
//! `render_unavailable` and writes nothing. Compiled only with
//! `--no-default-features`.

#![cfg(not(feature = "render"))]

mod common;

use web_widow::core::ErrorCode;
use web_widow::pipeline::ScreenshotInput;

#[tokio::test]
async fn screenshot_is_render_unavailable_and_writes_nothing() {
 let env = common::TestEnv::new(common::test_config()).await;
 let error = env
.pipeline
.screenshot(ScreenshotInput {
 url: "http://127.0.0.1:1/static-only".to_string(),
 full_page: Some(false),
 wait_ms: Some(0),
 })
.await
.expect_err("the static build must not fabricate a capture");
 assert_eq!(error.code(), ErrorCode::RenderUnavailable);
 assert!(error.to_string().contains("render"), "{error}");

 let db = env.db();
 let blobs: i64 = db
.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
.unwrap();
 let screenshots: i64 = db
.query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
.unwrap();
 assert_eq!(blobs, 0);
 assert_eq!(screenshots, 0);
 assert_eq!(env.blob_count(), 0);
}
