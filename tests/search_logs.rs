//! Log privacy. A separate test binary so the global tracing subscriber
//! is not raced by other tests in the same process.

mod common;

use std::sync::{Arc, Mutex};

use common::{TestEnv, start_fixture, test_config};
use web_widow::search::SearchInput;

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
async fn the_query_never_reaches_the_logs() {
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
    let mut config = test_config();
    config.search.html_url = fixture.url("127.0.0.1", "/serp/html");
    config.search.lite_url = fixture.url("127.0.0.1", "/serp/lite");
    let env = TestEnv::new(config).await;

    let sentinel = "SENTINEL_QUERY_9f3c7a";
    let payload = env
        .pipeline
        .search(SearchInput {
            query: sentinel.to_string(),
            site: None,
            freshness: None,
            limit: None,
        })
        .await
        .expect("search succeeds");
    assert_eq!(payload.result_count, 10);

    let logs = String::from_utf8(
        capture
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .unwrap();
    assert!(
        logs.contains("search completed"),
        "the capture is live: {logs}"
    );
    assert!(!logs.contains(sentinel), "the query leaked: {logs}");
    assert!(!logs.contains("q="), "the SERP URL leaked: {logs}");
}
