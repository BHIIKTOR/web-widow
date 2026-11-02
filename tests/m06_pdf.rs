//! M06 C2 — PDF text-layer extraction on PDFium (`pdf_extract`, C2-1…C2-8).
//!
//! Hermetic: a lane-local axum fixture server on 0.0.0.0 (127.0.0.1 allowed,
//! 127.0.0.2 denies `/blocked`) serving the constructed PDF fixtures; one
//! temporary data directory per test. The PDFium library is provisioned out of
//! tree; set `WEB_WIDOW_PDFIUM_PATH` to its path (the Chrome precedent) — the
//! extraction tests print `SKIP` and return when it is absent.
//!
//! `tests/common/mod.rs` is used unchanged.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use common::{TestEnv, test_config};
use serde_json::json;
use web_widow::ErrorCode;
use web_widow::config::Config;
use web_widow::fts::SearchContentInput;
use web_widow::pdf::PdfExtractInput;
use web_widow::pdf::PdfOutcome;
use web_widow::pdf::engine::ENV_PDFIUM_PATH;

const SIMPLE: &[u8] = include_bytes!("fixtures/m06/pdf/simple.pdf");
const THREE: &[u8] = include_bytes!("fixtures/m06/pdf/three-pages.pdf");
const SCANNED: &[u8] = include_bytes!("fixtures/m06/pdf/scanned.pdf");
const MALFORMED: &[u8] = include_bytes!("fixtures/m06/pdf/malformed.pdf");
const ENCRYPTED_EMPTY: &[u8] = include_bytes!("fixtures/m06/pdf/encrypted-empty-password.pdf");
const ENCRYPTED_USER: &[u8] = include_bytes!("fixtures/m06/pdf/encrypted-user-password.pdf");

#[derive(Clone, Default)]
struct RequestLog(Arc<std::sync::Mutex<Vec<String>>>);

impl RequestLog {
    fn record(&self, entry: String) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    fn count(&self, needle: &str) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|entry| entry.contains(needle))
            .count()
    }
}

#[derive(Clone)]
struct FixtureState {
    log: RequestLog,
    /// `/pdf-once` serves a PDF first and HTML afterwards (the page-text rule).
    pdf_served: Arc<std::sync::atomic::AtomicUsize>,
}

struct Fixture {
    port: u16,
    log: RequestLog,
    _handle: tokio::task::JoinHandle<()>,
}

impl Fixture {
    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }
}

fn body(bytes: Vec<u8>, content_type: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap()
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
    use std::sync::atomic::Ordering;

    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1")
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .to_string();
    let path = request.uri().path().to_string();
    state.log.record(format!("{host}{path}"));
    if path == "/robots.txt" {
        let policy = match host.as_str() {
            "127.0.0.2" => "User-agent: *\nDisallow: /blocked\n",
            _ => "User-agent: *\nAllow: /\n",
        };
        return body(policy.as_bytes().to_vec(), "text/plain");
    }

    match path.as_str() {
        "/simple.pdf" => body(SIMPLE.to_vec(), "application/pdf"),
        "/simple-octet" => body(SIMPLE.to_vec(), "application/octet-stream"),
        "/three.pdf" => body(THREE.to_vec(), "application/pdf"),
        "/scanned.pdf" => body(SCANNED.to_vec(), "application/pdf"),
        "/malformed.pdf" => body(MALFORMED.to_vec(), "application/pdf"),
        "/encrypted-empty.pdf" => body(ENCRYPTED_EMPTY.to_vec(), "application/pdf"),
        "/encrypted-user.pdf" => body(ENCRYPTED_USER.to_vec(), "application/pdf"),
        "/blocked.pdf" => body(SIMPLE.to_vec(), "application/pdf"),
        "/html" => body(
            b"<html><body><p>not a pdf</p></body></html>".to_vec(),
            "text/html; charset=utf-8",
        ),
        "/pdf-once" => {
            let served = state.pdf_served.fetch_add(1, Ordering::SeqCst);
            if served == 0 {
                body(SIMPLE.to_vec(), "application/pdf")
            } else {
                body(
                    b"<html><body><h1>HTML replacement</h1><p>HTML_REPLACEMENT_MARKER is the delivered text after the PDF acquisition.</p></body></html>".to_vec(),
                    "text/html; charset=utf-8",
                )
            }
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("not found"))
            .unwrap(),
    }
}

async fn start_fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = FixtureState {
        log: RequestLog::default(),
        pdf_served: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let app = Router::new()
        .fallback(any(handle))
        .with_state(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    Fixture {
        port,
        log: state.log,
        _handle: handle,
    }
}

/// The provisioned library path, or `None` when the test must skip.
fn pdfium_path(test: &str) -> Option<PathBuf> {
    let path = std::env::var_os(ENV_PDFIUM_PATH)
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    if path.is_none() {
        eprintln!(
            "SKIP {test}: no PDFium library (set {ENV_PDFIUM_PATH} to the provisioned libpdfium.so)"
        );
    }
    path
}

async fn env_with(pdfium: PathBuf, patch: impl FnOnce(&mut Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    config.fetch.connect_timeout_ms = 3_000;
    config.fetch.read_timeout_ms = 3_000;
    config.fetch.total_timeout_ms = 10_000;
    config.pdf.pdfium_path = Some(pdfium);
    config.pdf.pdfium_discovery = false;
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

fn input(url: String) -> PdfExtractInput {
    PdfExtractInput {
        url,
        max_pages: None,
        max_chars: None,
    }
}

async fn search(env: &TestEnv, query: &str) -> usize {
    env.content
        .search(SearchContentInput {
            query: query.to_string(),
            limit: Some(20),
        })
        .await
        .expect("search")
        .result_count
}

#[tokio::test]
async fn a_two_page_pdf_returns_markers_metadata_and_page_headings() {
    let Some(pdfium) = pdfium_path("a_two_page_pdf_returns_markers_metadata_and_page_headings")
    else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let payload = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/simple.pdf")))
        .await
        .expect("extracts");

    assert_eq!(payload.status, 200);
    assert_eq!(payload.content_type, "application/pdf");
    assert_eq!(payload.pages, 2);
    assert_eq!(payload.declared_pages, Some(2));
    assert_eq!(payload.pages_with_text, 2);
    assert_eq!(payload.outcome, PdfOutcome::Ok);
    assert!(payload.markdown.contains("## Page 1"));
    assert!(payload.markdown.contains("## Page 2"));
    assert!(payload.markdown.contains("PAGE_ONE_MARKER"));
    assert!(payload.markdown.contains("PAGE_TWO_MARKER"));
    assert_eq!(payload.chars, payload.markdown.chars().count());
    assert_eq!(
        payload.metadata.title.as_deref(),
        Some("M06 C2 simple fixture")
    );
    assert_eq!(payload.metadata.author.as_deref(), Some("Fixture Author"));
    assert!(!payload.truncated);
    assert_eq!(payload.limit_hit, None);
}

#[tokio::test]
async fn an_octet_stream_body_with_pdf_magic_is_accepted() {
    let Some(pdfium) = pdfium_path("an_octet_stream_body_with_pdf_magic_is_accepted") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let payload = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/simple-octet")))
        .await
        .expect("extracts");
    assert_eq!(payload.outcome, PdfOutcome::Ok);
    assert_eq!(payload.pages, 2);
}

#[tokio::test]
async fn a_scanned_pdf_is_no_text_layer_without_fabricated_text() {
    let Some(pdfium) = pdfium_path("a_scanned_pdf_is_no_text_layer_without_fabricated_text") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let payload = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/scanned.pdf")))
        .await
        .expect("a no-text PDF is a successful call");
    assert_eq!(payload.outcome, PdfOutcome::NoTextLayer);
    assert_eq!(payload.pages, 1);
    assert_eq!(payload.pages_with_text, 0);
    assert_eq!(payload.empty_pages, vec![1]);
    assert!(payload.markdown.contains("## Page 1"));

    // Nothing is stored for a text-less document (no page, no blob).
    let db = env.db();
    for table in ["pages", "fetch_events", "extractions"] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    assert_eq!(env.blob_count(), 0);
}

#[tokio::test]
async fn encryption_and_malformed_documents_are_typed() {
    let Some(pdfium) = pdfium_path("encryption_and_malformed_documents_are_typed") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;

    // An empty user password opens normally.
    let payload = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/encrypted-empty.pdf")))
        .await
        .expect("empty-password PDF opens");
    assert_eq!(payload.outcome, PdfOutcome::Ok);
    assert!(payload.markdown.contains("PAGE_ONE_MARKER"));

    // A real user password is a typed refusal (no guessing).
    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/encrypted-user.pdf")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::PdfEncrypted);

    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/malformed.pdf")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::PdfInvalid);

    // A non-PDF body is refused by the PDF scope, not the parser.
    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/html")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedContentType);

    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/missing.pdf")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::HttpError);
}

#[tokio::test]
async fn caps_truncate_at_a_page_boundary_and_a_body_cap_is_typed() {
    let Some(pdfium) = pdfium_path("caps_truncate_at_a_page_boundary_and_a_body_cap_is_typed")
    else {
        return;
    };
    let (fixture, env) = env_with(pdfium.clone(), |_| {}).await;

    let mut capped = input(fixture.url("127.0.0.1", "/three.pdf"));
    capped.max_pages = Some(2);
    let payload = env.pipeline.pdf_extract(capped).await.expect("extracts");
    assert_eq!(payload.pages, 2);
    assert_eq!(payload.declared_pages, Some(3));
    assert_eq!(payload.outcome, PdfOutcome::Partial);
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_pages"));
    assert!(!payload.markdown.contains("PAGE_THREE_MARKER"));

    let mut char_capped = input(fixture.url("127.0.0.1", "/simple.pdf"));
    char_capped.max_chars = Some(60);
    let payload = env
        .pipeline
        .pdf_extract(char_capped)
        .await
        .expect("extracts");
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_chars"));
    assert!(payload.chars <= 60);

    let (fixture, env) = env_with(pdfium, |config| config.pdf.max_bytes = 512).await;
    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/simple.pdf")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TooLarge);

    let error = env
        .pipeline
        .pdf_extract(PdfExtractInput {
            url: fixture.url("127.0.0.1", "/simple.pdf"),
            max_pages: Some(0),
            max_chars: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
}

#[tokio::test]
async fn a_pdf_is_stored_and_indexed_and_html_replaces_the_text() {
    let Some(pdfium) = pdfium_path("a_pdf_is_stored_and_indexed_and_html_replaces_the_text") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let url = fixture.url("127.0.0.1", "/pdf-once");

    let payload = env
        .pipeline
        .pdf_extract(input(url.clone()))
        .await
        .expect("extracts");
    assert_eq!(payload.outcome, PdfOutcome::Ok);

    let db = env.db();
    let (content_type, mode, outcome): (String, String, String) = db
        .query_row(
            "SELECT content_type, render_mode, outcome FROM pages WHERE url_hash = ?1",
            [web_widow::core::url::PageUrl::parse(&url).unwrap().hash()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(content_type, "application/pdf");
    assert_eq!(mode, "pdf");
    assert_eq!(outcome, "ok");
    let rows: i64 = db
        .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "one index row for the page");
    drop(db);

    assert!(
        search(&env, "PAGE_TWO_MARKER").await >= 1,
        "page 2 text is indexed"
    );

    // A later HTML acquisition of the same URL replaces the delivered text.
    env.fetch(&url).await.expect("html fetch");
    assert_eq!(search(&env, "HTML_REPLACEMENT_MARKER").await, 1);
    assert_eq!(
        search(&env, "PAGE_TWO_MARKER").await,
        0,
        "the PDF text is replaced"
    );
}

#[tokio::test]
async fn fetch_page_still_refuses_pdfs_and_names_pdf_extract() {
    let Some(pdfium) = pdfium_path("fetch_page_still_refuses_pdfs_and_names_pdf_extract") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/simple.pdf"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedContentType);
    let message = error.to_string();
    assert!(message.contains("pdf_extract"), "{message}");
}

#[tokio::test]
async fn robots_and_private_addresses_block_a_pdf_fetch() {
    let Some(pdfium) = pdfium_path("robots_and_private_addresses_block_a_pdf_fetch") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let error = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.2", "/blocked.pdf")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::BlockedByRobots);
    assert_eq!(fixture.log.count("/blocked.pdf"), 0);

    let error = env
        .pipeline
        .pdf_extract(input("http://169.254.169.254/secret.pdf".to_string()))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::BlockedPrivateAddress);
}

#[test]
fn pdf_config_defaults_and_bounds_are_validated() {
    let config = Config::default();
    assert_eq!(config.pdf.max_bytes, 32 * 1024 * 1024);
    assert_eq!(config.pdf.max_pages, 200);
    assert_eq!(config.pdf.max_chars, 1_000_000);
    assert_eq!(config.pdf.pdfium_path, None);
    assert!(config.pdf.pdfium_discovery);

    for (bytes, pages, chars) in [
        (0u64, 200usize, 1_000_000usize),
        (64 * 1024 * 1024 + 1, 200, 1_000_000),
        (32 * 1024 * 1024, 0, 1_000_000),
        (32 * 1024 * 1024, 2_001, 1_000_000),
        (32 * 1024 * 1024, 200, 0),
        (32 * 1024 * 1024, 200, 5_000_001),
    ] {
        let mut bad = Config::default();
        bad.pdf.max_bytes = bytes;
        bad.pdf.max_pages = pages;
        bad.pdf.max_chars = chars;
        assert!(
            bad.validate().is_err(),
            "pdf {bytes}/{pages}/{chars} must be refused"
        );
    }

    let mut empty_path = Config::default();
    empty_path.pdf.pdfium_path = Some(PathBuf::new());
    assert!(
        empty_path.validate().is_err(),
        "an empty strict path is refused"
    );

    let mut edge = Config::default();
    edge.pdf.max_bytes = 1;
    edge.pdf.max_pages = 1;
    edge.pdf.max_chars = 1;
    edge.validate().expect("edge values are accepted");
}

#[tokio::test]
async fn the_tool_payload_shape_is_the_documented_contract() {
    let Some(pdfium) = pdfium_path("the_tool_payload_shape_is_the_documented_contract") else {
        return;
    };
    let (fixture, env) = env_with(pdfium, |_| {}).await;
    let payload = env
        .pipeline
        .pdf_extract(input(fixture.url("127.0.0.1", "/simple.pdf")))
        .await
        .expect("extracts");
    let value = serde_json::to_value(&payload).unwrap();
    for key in [
        "url",
        "final_url",
        "status",
        "content_type",
        "pages",
        "declared_pages",
        "pages_with_text",
        "empty_pages",
        "empty_pages_truncated",
        "chars",
        "markdown",
        "metadata",
        "outcome",
        "truncated",
        "fetched_at",
    ] {
        assert!(value.get(key).is_some(), "missing {key}: {value}");
    }
    assert_eq!(value["outcome"], json!("ok"));
    assert_eq!(value["metadata"]["title"], json!("M06 C2 simple fixture"));
}
