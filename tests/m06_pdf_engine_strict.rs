//! C2-9 — `[pdf].pdfium_path` is strict and fails before any request.
//!
//! This file is its own process, so the process-wide engine cache is empty and
//! the failure is the real load error, not an "already loaded" state.

mod common;

use std::path::PathBuf;

use common::{TestEnv, test_config};
use web_widow::ErrorCode;
use web_widow::pdf::PdfExtractInput;

#[tokio::test]
async fn a_bad_strict_path_is_render_unavailable_before_any_fetch() {
 let mut config = test_config();
 config.pdf.pdfium_path = Some(PathBuf::from("/nonexistent/libpdfium.so"));
 // Discovery is on: a strict path must still not fall back.
 config.pdf.pdfium_discovery = true;
 let env = TestEnv::new(config).await;

 let error = env
.pipeline
.pdf_extract(PdfExtractInput {
 // Port 9 is not served: if the engine check ran after the fetch, the
 // error would be a connection failure instead.
 url: "http://127.0.0.1:9/simple.pdf".to_string(),
 max_pages: None,
 max_chars: None,
 })
.await
.unwrap_err();

 assert_eq!(error.code(), ErrorCode::RenderUnavailable);
 let message = error.to_string();
 assert!(message.contains("[pdf].pdfium_path"), "{message}");
 assert!(message.contains("/nonexistent/libpdfium.so"), "{message}");
 assert!(!message.contains("connect"), "{message}");
 assert!(!message.contains("timeout"), "{message}");
}
