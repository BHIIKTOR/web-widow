//! B03 feed log privacy. A separate test binary so the global tracing
//! subscriber is not raced by other tests in the same process.
//!
//! The feed URL can carry a token in its query; `fetch_feed` must log the
//! acquisition target like every other path (`log_target`, honoring
//! `logging.redact_query`), never the raw URL.

mod common;

use std::sync::{Arc, Mutex};

use common::{TestEnv, start_fixture, test_config};
use web_widow::feed::FeedInput;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_feed_url_query_never_reaches_the_logs() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("test owns the global subscriber");
    tracing::callsite::rebuild_interest_cache();

    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let sentinel = "SENTINEL_FEED_TOKEN_9f3c7a";
    let url = format!("{}?token={sentinel}", fixture.url("127.0.0.1", "/feed/rss"));
    let payload = env
        .pipeline
        .fetch_feed(FeedInput { url, limit: None })
        .await
        .expect("feed read succeeds");
    assert_eq!(payload.item_count, 2);

    let logs = String::from_utf8(
        capture
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .unwrap();
    assert!(
        logs.contains("fetch_feed completed"),
        "the capture is live: {logs}"
    );
    assert!(!logs.contains(sentinel), "the feed token leaked: {logs}");
    assert!(!logs.contains("token="), "the raw query leaked: {logs}");
    assert!(
        logs.contains("<redacted>"),
        "the target was not redacted: {logs}"
    );
}
