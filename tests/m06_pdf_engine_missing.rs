//! C2-9 — discovery failure names both settings and the searched locations.
//!
//! Own process: the env variable is removed before the first call so the
//! discovery chain has no candidate. The host has no system-wide libpdfium
//! (recorded in the C2 evidence); a system library would make this test fail
//! loudly, which is the correct signal to update the evidence.

mod common;

use common::{TestEnv, test_config};
use web_widow::ErrorCode;
use web_widow::pdf::PdfExtractInput;
use web_widow::pdf::engine::ENV_PDFIUM_PATH;

#[tokio::test]
async fn discovery_with_no_candidate_is_typed_and_lists_the_search_path() {
    // SAFETY: this test binary runs its tests serially; no other thread reads
    // the environment here.
    unsafe {
        std::env::remove_var(ENV_PDFIUM_PATH);
    }
    let mut config = test_config();
    config.pdf.pdfium_path = None;
    config.pdf.pdfium_discovery = true;
    let env = TestEnv::new(config).await;

    let error = env
        .pipeline
        .pdf_extract(PdfExtractInput {
            url: "http://127.0.0.1:9/simple.pdf".to_string(),
            max_pages: None,
            max_chars: None,
        })
        .await
        .unwrap_err();

    assert_eq!(error.code(), ErrorCode::RenderUnavailable);
    let message = error.to_string();
    assert!(message.contains("[pdf].pdfium_discovery"), "{message}");
    assert!(message.contains("[pdf].pdfium_path"), "{message}");
    assert!(message.contains("system library"), "{message}");
    assert!(!message.contains("connect"), "{message}");
}
