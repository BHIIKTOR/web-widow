//! C2-9 — the `<data_dir>/lib/libpdfium.so` candidate loads the engine.
//!
//! Own process: the env variable is removed and the exe directory is not given
//! a library, so the data-dir candidate is the first that exists.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use common::{TestEnv, test_config};
use web_widow::pdf::PdfExtractInput;
use web_widow::pdf::PdfOutcome;
use web_widow::pdf::engine::{self, ENV_PDFIUM_PATH};

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

async fn start_fixture() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().fallback(any(handler));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    port
}

#[tokio::test]
async fn the_data_dir_lib_candidate_loads_the_engine() {
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
        std::env::remove_var(ENV_PDFIUM_PATH);
    }

    let port = start_fixture().await;
    let mut config = test_config();
    config.fetch.total_timeout_ms = 10_000;
    config.pdf.pdfium_path = None;
    config.pdf.pdfium_discovery = true;
    let env = TestEnv::new(config).await;

    // Place the library at <data_dir>/lib/<platform name>.
    let lib_dir = env.data_dir().join("lib");
    std::fs::create_dir_all(&lib_dir).unwrap();
    let target = lib_dir.join(engine::platform_library_name());
    std::fs::copy(&library, &target).unwrap();
    assert!(target.is_file());

    let payload = env
        .pipeline
        .pdf_extract(PdfExtractInput {
            url: format!("http://127.0.0.1:{port}/simple.pdf"),
            max_pages: None,
            max_chars: None,
        })
        .await
        .expect("extracts through the data-dir candidate");
    assert_eq!(payload.outcome, PdfOutcome::Ok);
    assert_eq!(payload.pages, 2);
}
