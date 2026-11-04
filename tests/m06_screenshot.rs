//! v0.6 — `screenshot` + `blob_get` (checks …,
//! …).
//!
//! Lane-local fixture server: `tests/fixtures/m06/browser/` (tall, styled with
//! CSS/font/image/media, plain) plus a robots.txt that disallows `/private`.
//! A real Chrome is required; when none is available the render tests print an
//! explicit `SKIP` marker and return (house style). Nothing here touches
//! `tests/common/mod.rs`.

#![cfg(feature = "render")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use chrono::{SecondsFormat, Utc};
use rusqlite::params;
use web_widow::core::ErrorCode;
use web_widow::pipeline::{
    BlobGetInput, FetchPageInput, RenderPolicy, ScreenshotInput, ScreenshotPayload,
};

const STYLED: &str = include_str!("fixtures/m06/browser/styled.html");
const TALL: &str = include_str!("fixtures/m06/browser/tall.html");
const PLAIN: &str = include_str!("fixtures/m06/browser/plain.html");
const STYLE_CSS: &str = include_str!("fixtures/m06/browser/style.css");

#[derive(Clone, Default)]
struct RequestLog {
    entries: Arc<Mutex<Vec<String>>>,
}

impl RequestLog {
    fn record(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

struct BrowserFixture {
    origin: String,
    log: RequestLog,
    _handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct FixtureState {
    log: RequestLog,
}

fn html(body: &'static [u8]) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body.to_vec()))
        .unwrap()
}

fn asset(content_type: &str, body: &'static [u8]) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    state.log.record(path.clone());
    match path.as_str() {
        "/robots.txt" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("User-agent: *\nAllow: /\nDisallow: /private\n"))
            .unwrap(),
        "/tall.html" => html(TALL.as_bytes()),
        "/styled.html" => html(STYLED.as_bytes()),
        "/plain.html" => html(PLAIN.as_bytes()),
        "/asset/style.css" => asset("text/css", STYLE_CSS.as_bytes()),
        "/asset/font.woff2" => asset(
            "font/woff2",
            include_bytes!("fixtures/m06/browser/font.woff2"),
        ),
        "/asset/image.png" => asset(
            "image/png",
            include_bytes!("fixtures/m06/browser/image.png"),
        ),
        "/asset/movie.mp4" => asset(
            "video/mp4",
            include_bytes!("fixtures/m06/browser/movie.mp4"),
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn start_browser_fixture() -> BrowserFixture {
    let log = RequestLog::default();
    let state = FixtureState { log: log.clone() };
    let app = Router::new().fallback(any(handle)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    BrowserFixture {
        origin: format!("http://127.0.0.1:{port}"),
        log,
        _handle: handle,
    }
}

fn screenshot_input(url: String, full_page: bool, wait_ms: u64) -> ScreenshotInput {
    ScreenshotInput {
        url,
        full_page: Some(full_page),
        wait_ms: Some(wait_ms),
    }
}

async fn capture(
    env: &common::TestEnv,
    url: String,
    full_page: bool,
    wait_ms: u64,
) -> ScreenshotPayload {
    env.pipeline
        .screenshot(screenshot_input(url, full_page, wait_ms))
        .await
        .expect("screenshot")
}

fn blob_file(env: &common::TestEnv, hash: &str) -> Vec<u8> {
    let path = env.store.blobs.path_for(hash).expect("valid hash");
    std::fs::read(path).expect("blob file")
}

fn screenshots_rows(env: &common::TestEnv) -> i64 {
    env.db()
        .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
        .unwrap()
}

fn blobs_rows(env: &common::TestEnv) -> i64 {
    env.db()
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_screenshot_produces_a_png_cas_blob_and_one_screenshot_row() {
    let Some(chrome) =
        common::skip_without_chrome("a_screenshot_produces_a_png_cas_blob_and_one_screenshot_row")
    else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    let env = common::TestEnv::new_rendering(config).await;
    let url = format!("{}/plain.html", fixture.origin);

    let payload = capture(&env, url.clone(), false, 0).await;

    let png = blob_file(&env, &payload.hash);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
    assert!(!png.is_empty());
    assert_eq!(payload.hash, web_widow::core::url::content_hash(&png));
    assert_eq!(payload.bytes, png.len() as u64);
    assert_eq!(payload.mime, "image/png");
    assert_eq!(payload.url, url);
    assert_eq!(payload.status, 200);
    assert!(!payload.full_page);
    assert_eq!(payload.dimensions_source, "viewport");
    assert!(!payload.truncated);
    assert_eq!(payload.limit_hit, None);
    assert_eq!(payload.render_mode, "rendered");
    let width = payload.width.expect("viewport width");
    let height = payload.height.expect("viewport height");
    assert!(width > 0 && height > 0, "{width}x{height}");
    assert!(
        payload.render_quality == "ok" || payload.render_quality == "thin",
        "{}",
        payload.render_quality
    );

    assert_eq!(blobs_rows(&env), 1, "one blobs row");
    assert_eq!(screenshots_rows(&env), 1, "one screenshots row");
    println!(
        "CAPTURE hash={} bytes={} width={} height={} source={} render_mode={} quality={}",
        payload.hash,
        payload.bytes,
        width,
        height,
        payload.dimensions_source,
        payload.render_mode,
        payload.render_quality
    );
    let (row_hash, format, row_width, row_height, source): (
        String,
        String,
        Option<i64>,
        Option<i64>,
        String,
    ) = env
        .db()
        .query_row(
            "SELECT hash, format, width, height, source FROM screenshots",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row_hash, payload.hash);
    assert_eq!(format, "png");
    assert_eq!(row_width, Some(width as i64));
    assert_eq!(row_height, Some(height as i64));
    assert_eq!(source, "screenshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_page_height_is_the_content_size_and_the_default_is_the_viewport() {
    let Some(chrome) = common::skip_without_chrome(
        "full_page_height_is_the_content_size_and_the_default_is_the_viewport",
    ) else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    let env = common::TestEnv::new_rendering(config).await;
    let url = format!("{}/tall.html", fixture.origin);

    let viewport = capture(&env, url.clone(), false, 0).await;
    let full = capture(&env, url, true, 0).await;

    assert_eq!(viewport.dimensions_source, "viewport");
    assert_eq!(full.dimensions_source, "content");
    let viewport_height = viewport.height.expect("viewport height");
    let content_height = full.height.expect("content height");
    assert!(
        content_height > viewport_height,
        "full page {content_height} must exceed the viewport {viewport_height}"
    );
    assert!(
        content_height > 2_000,
        "the tall fixture is ~3000 CSS px, got {content_height}"
    );
    assert!(
        viewport_height <= 1_000,
        "default viewport height {viewport_height}"
    );
    assert!(full.full_page);
    assert_ne!(viewport.hash, full.hash, "different captures differ");
    println!(
        "DIMENSIONS viewport_height={viewport_height} content_height={content_height} viewport_hash={} full_hash={}",
        viewport.hash, full.hash
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_visual_request_cap_sets_truncated_and_limit_hit() {
    let Some(chrome) =
        common::skip_without_chrome("a_visual_request_cap_sets_truncated_and_limit_hit")
    else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    config.render.visual_max_requests = 1;
    let env = common::TestEnv::new_rendering(config).await;

    let payload = capture(&env, format!("{}/styled.html", fixture.origin), false, 0).await;

    assert!(payload.truncated, "cap must mark the capture truncated");
    assert_eq!(payload.limit_hit.as_deref(), Some("visual_max_requests"));
    assert_eq!(payload.render_mode, "rendered_partial");
    assert!(!blob_file(&env, &payload.hash).is_empty());
    println!(
        "CAP limit_hit={:?} truncated={} render_mode={} failed_resources={}",
        payload.limit_hit, payload.truncated, payload.render_mode, payload.failed_resources
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ssrf_and_robots_gates_still_apply_and_write_no_blob() {
    let Some(chrome) =
        common::skip_without_chrome("the_ssrf_and_robots_gates_still_apply_and_write_no_blob")
    else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    let env = common::TestEnv::new_rendering(config).await;

    let private = env
        .pipeline
        .screenshot(screenshot_input(
            "http://169.254.169.254/latest/meta-data/".to_string(),
            false,
            0,
        ))
        .await
        .expect_err("link-local metadata address must be blocked");
    assert_eq!(private.code(), ErrorCode::BlockedPrivateAddress);
    assert_eq!(blobs_rows(&env), 0);
    assert_eq!(screenshots_rows(&env), 0);

    let robots = env
        .pipeline
        .screenshot(screenshot_input(
            format!("{}/private/secret", fixture.origin),
            false,
            0,
        ))
        .await
        .expect_err("robots-disallowed path must be blocked");
    assert_eq!(robots.code(), ErrorCode::BlockedByRobots);
    assert_eq!(blobs_rows(&env), 0);
    assert_eq!(screenshots_rows(&env), 0);
    assert_eq!(env.blob_count(), 0, "no CAS file for a refused capture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_get_returns_the_cas_path_size_mime_and_bounded_references() {
    let Some(chrome) = common::skip_without_chrome(
        "blob_get_returns_the_cas_path_size_mime_and_bounded_references",
    ) else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    let env = common::TestEnv::new_rendering(config).await;
    let url = format!("{}/plain.html", fixture.origin);

    // A fetched page row references its own raw blob by `page:<id>`; the
    // screenshot blob is referenced by its `screenshots` row. Both go through
    // the same bounded list.
    env.pipeline
        .fetch_page(FetchPageInput {
            url: url.clone(),
            mode: None,
            render: Some(RenderPolicy::Never),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
            profile: None,
        })
        .await
        .expect("page fetch");
    let (page_id, raw_hash): (i64, String) = env
        .db()
        .query_row(
            "SELECT id, raw_hash FROM pages ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let page_blob = env
        .pipeline
        .blob_get(BlobGetInput {
            hash: raw_hash.clone(),
        })
        .await
        .expect("page blob_get");
    assert!(
        page_blob
            .referenced_by
            .iter()
            .any(|reference| reference == &format!("page:{page_id}")),
        "{:?}",
        page_blob.referenced_by
    );
    assert_eq!(page_blob.mime.as_deref(), Some("text/html"));

    let payload = capture(&env, url, false, 0).await;

    let found = env
        .pipeline
        .blob_get(BlobGetInput {
            hash: payload.hash.clone(),
        })
        .await
        .expect("blob_get");
    assert_eq!(found.hash, payload.hash);
    assert_eq!(found.size, blob_file(&env, &payload.hash).len() as i64);
    assert_eq!(found.mime.as_deref(), Some("image/png"));
    let path = std::path::Path::new(&found.path);
    assert!(path.is_absolute(), "{}", found.path);
    assert!(path.is_file(), "{}", found.path);
    let uppercase = env
        .pipeline
        .blob_get(BlobGetInput {
            hash: payload.hash.to_ascii_uppercase(),
        })
        .await
        .expect("an uppercase spelling normalizes to the same blob");
    assert_eq!(uppercase.hash, payload.hash);
    assert_eq!(uppercase.path, found.path);

    let screenshot_id: i64 = env
        .db()
        .query_row("SELECT MAX(id) FROM screenshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        found.referenced_by,
        vec![format!("screenshot:{screenshot_id}")],
        "a screenshot blob is referenced by its screenshot row"
    );

    let unknown = "a".repeat(64);
    let error = env
        .pipeline
        .blob_get(BlobGetInput { hash: unknown })
        .await
        .expect_err("unknown hash");
    assert_eq!(error.code(), ErrorCode::NotFound);

    for candidate in ["../../etc/passwd", "..", "/etc/passwd", "not-a-hash"] {
        let error = env
            .pipeline
            .blob_get(BlobGetInput {
                hash: candidate.to_string(),
            })
            .await
            .expect_err("traversal or malformed hash");
        assert_eq!(error.code(), ErrorCode::NotFound, "{candidate}");
    }

    // A row whose file is gone is `not_found`, not an empty payload.
    std::fs::remove_file(path).unwrap();
    let error = env
        .pipeline
        .blob_get(BlobGetInput {
            hash: payload.hash.clone(),
        })
        .await
        .expect_err("missing bytes");
    assert_eq!(error.code(), ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_screenshot_blob_is_retained_then_collectable_after_its_row_is_removed() {
    let Some(chrome) = common::skip_without_chrome(
        "a_screenshot_blob_is_retained_then_collectable_after_its_row_is_removed",
    ) else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    let env = common::TestEnv::new_rendering(config).await;
    let payload = capture(&env, format!("{}/plain.html", fixture.origin), false, 0).await;
    let blob_path = env.store.blobs.path_for(&payload.hash).unwrap();
    assert!(blob_path.is_file());

    // Age the blob past every other retention window while the screenshot row
    // is recent: only the reference set can keep it alive.
    let old =
        (Utc::now() - chrono::Duration::days(40)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let recent =
        (Utc::now() - chrono::Duration::days(10)).to_rfc3339_opts(SecondsFormat::Millis, true);
    env.db()
        .execute("UPDATE blobs SET created_at = ?1", params![old])
        .unwrap();
    env.db()
        .execute("UPDATE screenshots SET captured_at = ?1", params![recent])
        .unwrap();

    let mut retention = env.config.retention.clone();
    retention.screenshots_days = 3_650;
    retention.extractions_days = 3_650;
    let report = env
        .store
        .gc(retention, env.config.auth.audit_days, 7)
        .await
        .expect("gc with the row present");
    assert_eq!(report.removed_blobs, Vec::<String>::new());
    assert!(blob_path.is_file(), "the screenshot row retains the blob");
    assert_eq!(blobs_rows(&env), 1);

    // Removing the screenshot row makes the same blob collectable.
    env.db().execute("DELETE FROM screenshots", []).unwrap();
    let mut retention = env.config.retention.clone();
    retention.screenshots_days = 3_650;
    retention.extractions_days = 3_650;
    retention.max_blob_bytes = 0;
    let report = env
        .store
        .gc(retention, env.config.auth.audit_days, 7)
        .await
        .expect("gc without the row");
    assert_eq!(report.removed_blobs, vec![payload.hash.clone()]);
    assert!(!blob_path.exists(), "the CAS file is removed");
    assert_eq!(blobs_rows(&env), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_visual_profile_loads_assets_and_the_text_profile_keeps_blocking_them() {
    let Some(chrome) = common::skip_without_chrome(
        "the_visual_profile_loads_assets_and_the_text_profile_keeps_blocking_them",
    ) else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 400;
    let env = common::TestEnv::new_rendering(config).await;
    let url = format!("{}/styled.html", fixture.origin);

    let payload = capture(&env, url.clone(), false, 0).await;
    let visual_entries = fixture.log.entries();
    for asset in ["/asset/style.css", "/asset/image.png", "/asset/font.woff2"] {
        assert!(
            visual_entries.iter().any(|entry| entry.contains(asset)),
            "the visual profile must fetch {asset}: {visual_entries:?}"
        );
    }
    assert!(
        !visual_entries
            .iter()
            .any(|entry| entry.contains("/asset/movie.mp4")),
        "media is blocked in the visual profile: {visual_entries:?}"
    );
    assert!(
        payload.failed_resources >= 1,
        "the refused media request is counted: {}",
        payload.failed_resources
    );

    // The text profile is unchanged: the same URL through `fetch_page` blocks
    // stylesheets, fonts, images and media; only scripts/XHR may load.
    fixture.log.clear();
    env.pipeline
        .fetch_page(FetchPageInput {
            url: url.clone(),
            mode: None,
            render: Some(RenderPolicy::Always),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
            profile: None,
        })
        .await
        .expect("text-profile render");
    let text_entries = fixture.log.entries();
    for asset in ["/asset/style.css", "/asset/image.png", "/asset/font.woff2"] {
        assert!(
            !text_entries.iter().any(|entry| entry.contains(asset)),
            "fetch_page must still block {asset}: {text_entries:?}"
        );
    }
    assert!(
        !text_entries
            .iter()
            .any(|entry| entry.contains("/asset/movie.mp4")),
        "media is blocked for fetch_page too: {text_entries:?}"
    );

    // Direct text-profile attribution: the same blocked counters as v0.3.
    // The linked stylesheet is blocked, so the inline `@font-face` is the only
    // reason a font request exists to block in this profile.
    let rendered = env
        .renderer
        .render(&url::Url::parse(&url).unwrap())
        .await
        .expect("text-profile render");
    assert!(rendered.blocked.image >= 1, "{:?}", rendered.blocked);
    assert!(rendered.blocked.font >= 1, "{:?}", rendered.blocked);
    assert!(rendered.blocked.stylesheet >= 1, "{:?}", rendered.blocked);
    assert!(rendered.blocked.media >= 1, "{:?}", rendered.blocked);
    println!(
        "PROFILE visual_requests={visual_entries:?} visual_failed={} text_requests={text_entries:?} text_blocked={:?}",
        payload.failed_resources, rendered.blocked
    );
}

#[tokio::test]
async fn wait_ms_out_of_range_is_schema_invalid() {
    let fixture = start_browser_fixture().await;
    let env = common::TestEnv::new(common::test_config()).await;
    let error = env
        .pipeline
        .screenshot(screenshot_input(
            format!("{}/plain.html", fixture.origin),
            false,
            10_001,
        ))
        .await
        .expect_err("wait_ms above the ceiling");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert!(error.to_string().contains("wait_ms"), "{error}");
    assert_eq!(blobs_rows(&env), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_size_capture_is_too_large_and_stores_nothing() {
    let Some(chrome) =
        common::skip_without_chrome("an_over_size_capture_is_too_large_and_stores_nothing")
    else {
        return;
    };
    let fixture = start_browser_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.settle_ms = 250;
    config.render.max_screenshot_bytes = 1;
    let env = common::TestEnv::new_rendering(config).await;

    let error = env
        .pipeline
        .screenshot(screenshot_input(
            format!("{}/plain.html", fixture.origin),
            false,
            0,
        ))
        .await
        .expect_err("a 1-byte cap cannot accept a PNG");
    assert_eq!(error.code(), ErrorCode::TooLarge);
    assert!(error.to_string().contains("1"), "{error}");
    assert_eq!(blobs_rows(&env), 0, "no blob row");
    assert_eq!(screenshots_rows(&env), 0, "no screenshot row");
    assert_eq!(env.blob_count(), 0, "no CAS file");
}
