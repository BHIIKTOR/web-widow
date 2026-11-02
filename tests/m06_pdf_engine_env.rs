//! C2-9 — the env candidate loads the engine (discovery order, first entry).
//!
//! Own process: the env variable is set before the first call, so the engine
//! binds the provisioned library and a PDF extraction succeeds. A second
//! pipeline with a *different* strict path must then fail: the library is
//! process-wide and cannot change without a restart.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use common::{TestEnv, test_config};
use web_widow::ErrorCode;
use web_widow::pdf::PdfExtractInput;
use web_widow::pdf::PdfOutcome;
use web_widow::pdf::engine::ENV_PDFIUM_PATH;

const SIMPLE: &[u8] = include_bytes!("fixtures/m06/pdf/simple.pdf");

async fn handler(request: Request<Body>) -> Response {
    match request.uri().path() {
        "/robots.txt" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("User-agent: *\nAllow: /\n"))
            .unwrap(),
        "/simple.pdf" => Response::builder()
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(Body::from(SIMPLE.to_vec()))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

async fn start_fixture() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().fallback(any(handler));
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (port, handle)
}

#[tokio::test]
async fn the_env_candidate_loads_the_engine_and_the_source_is_process_wide() {
    let library = std::env::var_os(ENV_PDFIUM_PATH)
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let Some(library) = library else {
        eprintln!("SKIP: set {ENV_PDFIUM_PATH} to the provisioned libpdfium.so");
        return;
    };
    // SAFETY: this test binary runs its tests serially; no other thread reads
    // the environment here.
    unsafe {
        std::env::set_var(ENV_PDFIUM_PATH, &library);
    }

    let (port, _server) = start_fixture().await;
    let mut config = test_config();
    config.fetch.total_timeout_ms = 10_000;
    config.pdf.pdfium_path = None;
    config.pdf.pdfium_discovery = true;
    let env = TestEnv::new(config).await;

    let payload = env
        .pipeline
        .pdf_extract(PdfExtractInput {
            url: format!("http://127.0.0.1:{port}/simple.pdf"),
            max_pages: None,
            max_chars: None,
        })
        .await
        .expect("extracts through the env candidate");
    assert_eq!(payload.outcome, PdfOutcome::Ok);
    assert_eq!(payload.pages, 2);

    // A different strict path in the same process is refused, naming the key.
    let mut other = test_config();
    other.pdf.pdfium_path = Some(PathBuf::from("/tmp/other/libpdfium.so"));
    other.pdf.pdfium_discovery = false;
    let other_env = TestEnv::new(other).await;
    let error = other_env
        .pipeline
        .pdf_extract(PdfExtractInput {
            url: format!("http://127.0.0.1:{port}/simple.pdf"),
            max_pages: None,
            max_chars: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::RenderUnavailable);
    assert!(error.to_string().contains("[pdf].pdfium_path"), "{error}");
}
