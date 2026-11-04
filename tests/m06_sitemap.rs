//! Sitemap discovery and crawl seeding.
//!
//! Hermetic: one lane-local axum fixture server on 0.0.0.0 (so 127.0.0.1,
//! 127.0.0.2 and 127.0.0.9 reach different robots policies), one temporary data
//! directory per test, no real site. `tests/common/mod.rs` is used unchanged.

mod common;

use std::collections::BTreeSet;
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
use web_widow::crawl::{CrawlLimitsInput, CrawlRenderPolicy, CrawlStartInput, CrawlStatusPayload};
use web_widow::sitemap::{self, SitemapInput, SitemapKind};

const URLSET: &str = include_str!("fixtures/m06/discovery/urlset.xml");
const URLSET_GZ: &[u8] = include_bytes!("fixtures/m06/discovery/urlset.xml.gz");
const BOMB_GZ: &[u8] = include_bytes!("fixtures/m06/discovery/bomb.xml.gz");
const INDEX: &str = include_str!("fixtures/m06/discovery/index.xml");
const CHILD_ONE: &str = include_str!("fixtures/m06/discovery/child-one.xml");
const CHILD_TWO: &str = include_str!("fixtures/m06/discovery/child-two.xml");
const LEGACY: &str = include_str!("fixtures/m06/discovery/sitemap-legacy-08.xml");
const URLS_TXT: &str = include_str!("fixtures/m06/discovery/urls.txt");
const MALFORMED: &str = include_str!("fixtures/m06/discovery/malformed.xml");
const HUGE: &str = include_str!("fixtures/m06/discovery/huge.xml");
const OVERSIZE: &str = include_str!("fixtures/m06/discovery/oversize.xml");
const LASTMOD: &str = include_str!("fixtures/m06/discovery/lastmod-variants.xml");
const SITE_SITEMAP: &str = include_str!("fixtures/m06/discovery/site-sitemap.xml");
const SITE_SITEMAP_ONLY: &str = include_str!("fixtures/m06/discovery/site-sitemap-only.xml");

#[derive(Clone, Default)]
struct RequestLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl RequestLog {
    fn record(&self, entry: String) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn count(&self, needle: &str) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.contains(needle))
            .count()
    }
}

#[derive(Clone)]
struct FixtureState {
    log: RequestLog,
    port: u16,
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

fn with_port(text: &str, port: u16) -> String {
    text.replace("{{PORT}}", &port.to_string())
}

fn body(text: &str, content_type: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(text.to_string()))
        .unwrap()
}

fn page(title: &str, text: &str) -> String {
    format!(
        "<html><body><h1>{title}</h1><p>{text} This paragraph carries enough words for the readability pass to keep it.</p></body></html>"
    )
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
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
    let logged = match request.uri().query() {
        Some(query) => format!("{host}{path}?{query}"),
        None => format!("{host}{path}"),
    };
    state.log.record(logged);

    if path == "/robots.txt" {
        return robots_for(&host);
    }

    let text =
        |source: &str, content_type: &str| body(&with_port(source, state.port), content_type);
    match path.as_str() {
        "/sitemap.xml" => text(URLSET, "application/xml"),
        "/sitemap-legacy.xml" => text(LEGACY, "application/xml"),
        "/index.xml" => text(INDEX, "application/xml"),
        "/child-one.xml" => text(CHILD_ONE, "application/xml"),
        "/child-two.xml" => text(CHILD_TWO, "application/xml"),
        "/urls.txt" => text(URLS_TXT, "text/plain; charset=utf-8"),
        "/malformed.xml" => text(MALFORMED, "application/xml"),
        "/huge.xml" => text(HUGE, "application/xml"),
        "/oversize.xml" => text(OVERSIZE, "application/xml"),
        "/lastmod.xml" => text(LASTMOD, "application/xml"),
        "/site-sitemap.xml" => text(SITE_SITEMAP, "application/xml"),
        "/site-sitemap-only.xml" => text(SITE_SITEMAP_ONLY, "application/xml"),
        // A `.xml.gz` served as a generic binary: the parser must use magic
        // bytes, not the file name or the Content-Type.
        "/sitemap.xml.gz" => {
            let gz = gzip(&with_port(URLSET, state.port));
            Response::builder()
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(gz))
                .unwrap()
        }
        "/bomb.xml.gz" => Response::builder()
            .header(header::CONTENT_TYPE, "application/gzip")
            .body(Body::from(BOMB_GZ.to_vec()))
            .unwrap(),
        // Candidate page of the sitemap-index child test.
        "/site/one-a" | "/site/one-b" | "/site/two-a" | "/site/two-b" => {
            body(&page("Sitemap child page", "child page text"), "text/html")
        }
        // Crawl fixture: the shell links only to /site/other; /site/sitemap-only
        // is reachable exclusively through the sitemap.
        "/site/" => body(
            "<html><body><h1>Crawl root</h1><p>The root page of the crawl fixture carries enough words for the readability pass, and it links only to the other page. <a href=\"/site/other\">other</a></p></body></html>",
            "text/html; charset=utf-8",
        ),
        "/site/other" => body(
            &page("Other page", "Other body, reached by the seed link."),
            "text/html; charset=utf-8",
        ),
        "/site/sitemap-only" => body(
            &page(
                "Sitemap only",
                "This page is absent from the seed page and is listed only in the sitemap.",
            ),
            "text/html; charset=utf-8",
        ),
        "/site/excluded" => body(
            &page("Excluded page", "This page matches the exclusion pattern."),
            "text/html; charset=utf-8",
        ),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("not found"))
            .unwrap(),
    }
}

fn robots_for(host: &str) -> Response {
    let policy = match host {
        // 127.0.0.2 disallows the blocked sitemap path.
        "127.0.0.2" => "User-agent: *\nDisallow: /blocked\n",
        _ => "User-agent: *\nAllow: /\n",
    };
    body(policy, "text/plain")
}

fn gzip(text: &str) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

async fn start_fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = FixtureState {
        log: RequestLog::default(),
        port,
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

async fn env_with(patch: impl FnOnce(&mut Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    config.crawl.lease_s = 2;
    config.crawl.idle_poll_ms = 25;
    // The suite runs serially on a shared host; a 700 ms read timeout is the
    // known flake source (v0.5), not a behaviour under test here.
    config.fetch.connect_timeout_ms = 3_000;
    config.fetch.read_timeout_ms = 3_000;
    config.fetch.total_timeout_ms = 10_000;
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

fn sitemap_input(url: String) -> SitemapInput {
    SitemapInput {
        url,
        limit: None,
        include_lastmod: None,
        follow_index: None,
    }
}

fn start_input(
    start_urls: Vec<String>,
    sitemap_urls: Vec<String>,
    scope: Option<web_widow::crawl::CrawlScopeInput>,
    max_pages: Option<usize>,
) -> CrawlStartInput {
    CrawlStartInput {
        start_urls,
        sitemap_urls,
        scope,
        schema: None,
        render_policy: Some(CrawlRenderPolicy::Never),
        limits: Some(CrawlLimitsInput {
            max_pages,
            per_host_rate: Some(50.0),
            max_duration_s: None,
        }),
        name: Some("sitemap-fixture".to_string()),
    }
}

async fn wait_terminal(env: &TestEnv, job_id: &str) -> CrawlStatusPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let status = env.crawler.status(job_id).await.expect("status");
        if !matches!(status.state.as_str(), "queued" | "running") {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "crawl job did not finish: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn fetched_urls(env: &TestEnv, job_id: &str) -> BTreeSet<String> {
    let page = env
        .crawler
        .results(web_widow::crawl::CrawlResultsInput {
            job_id: job_id.to_string(),
            limit: Some(100),
            cursor: None,
        })
        .await
        .expect("results");
    page.results.into_iter().map(|row| row.url).collect()
}

// ---------------------------------------------------------------------------
// Parser and tool surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn urlset_parses_loc_lastmod_entities_and_skips_non_http() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/sitemap.xml")))
        .await
        .expect("urlset parses");

    assert_eq!(payload.status, 200);
    assert_eq!(payload.sitemap_type, SitemapKind::Urlset);
    assert_eq!(payload.url_count, 4);
    assert!(
        payload
            .urls
            .iter()
            .any(|url| url.loc.ends_with("/site/b?x=1&y=2"))
    );
    assert!(payload.urls.iter().any(|url| url.loc.ends_with("/site/c")));
    assert_eq!(payload.skipped_scheme, 1);
    assert_eq!(payload.invalid_lastmod, 1);
    assert!(!payload.truncated);
    assert!(payload.sitemaps.is_empty());

    // include_lastmod:false omits the field but the entry stays.
    let mut input = sitemap_input(fixture.url("127.0.0.1", "/sitemap.xml"));
    input.include_lastmod = Some(false);
    let payload = env.pipeline.sitemap(input).await.expect("urlset parses");
    assert_eq!(payload.url_count, 4);
    assert!(payload.urls.iter().all(|url| url.lastmod.is_none()));
}

#[tokio::test]
async fn legacy_namespace_and_text_format_parse() {
    let (fixture, env) = env_with(|_| {}).await;
    let legacy = env
        .pipeline
        .sitemap(sitemap_input(
            fixture.url("127.0.0.1", "/sitemap-legacy.xml"),
        ))
        .await
        .expect("legacy namespace parses");
    assert_eq!(legacy.sitemap_type, SitemapKind::Urlset);
    assert_eq!(legacy.url_count, 1);

    let text = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/urls.txt")))
        .await
        .expect("text sitemap parses");
    assert_eq!(text.sitemap_type, SitemapKind::Text);
    assert_eq!(text.url_count, 2);
    assert_eq!(text.skipped_scheme, 1);
    assert!(text.urls.iter().all(|url| url.lastmod.is_none()));
}

#[tokio::test]
async fn gzipped_body_parses_from_magic_bytes() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/sitemap.xml.gz")))
        .await
        .expect("gzip sitemap parses");
    assert_eq!(payload.sitemap_type, SitemapKind::Urlset);
    assert_eq!(payload.url_count, 4);

    // The retained on-disk gzip fixture parses through the same entry point.
    let parsed = sitemap::parse_body(
        URLSET_GZ,
        Some("application/octet-stream"),
        sitemap::SitemapLimits {
            max_bytes: 1024 * 1024,
            max_urls: 10,
            max_children: 2,
        },
    )
    .expect("fixture gzip parses");
    assert_eq!(parsed.urls.len(), 4);
}

#[tokio::test]
async fn an_index_follows_children_and_reports_child_errors() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/index.xml")))
        .await
        .expect("index parses");

    assert_eq!(payload.sitemap_type, SitemapKind::Index);
    assert_eq!(payload.sitemaps.len(), 3);
    assert_eq!(payload.sitemaps[0].url_count, 2);
    assert_eq!(payload.sitemaps[1].url_count, 2);
    assert_eq!(payload.sitemaps[2].error.as_deref(), Some("http_error"));
    assert_eq!(payload.url_count, 4);
    assert!(!payload.truncated);
    // Every child request passed through the gate.
    assert_eq!(fixture.log.count("/child-one.xml"), 1);
    assert_eq!(fixture.log.count("/child-two.xml"), 1);
    assert_eq!(fixture.log.count("/missing-child.xml"), 1);

    // follow_index:false reads the index document only.
    let mut input = sitemap_input(fixture.url("127.0.0.1", "/index.xml"));
    input.follow_index = Some(false);
    let payload = env.pipeline.sitemap(input).await.expect("index parses");
    assert!(payload.sitemaps.is_empty());
    assert_eq!(payload.url_count, 0);
    assert_eq!(
        fixture.log.count("/child-one.xml"),
        1,
        "follow_index:false must not fetch a child"
    );
}

#[tokio::test]
async fn caps_and_errors_are_typed_and_bounded() {
    let (fixture, env) = env_with(|config| {
        config.sitemap.max_urls = 5;
        config.sitemap.max_children = 1;
        config.sitemap.max_bytes = 64 * 1024;
    })
    .await;

    // max_urls cap on a 12-entry document.
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/huge.xml")))
        .await
        .expect("huge parses");
    assert_eq!(payload.url_count, 5);
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_urls"));

    // A caller limit cuts further and says so.
    let mut input = sitemap_input(fixture.url("127.0.0.1", "/huge.xml"));
    input.limit = Some(2);
    let payload = env.pipeline.sitemap(input).await.expect("huge parses");
    assert_eq!(payload.url_count, 2);
    assert_eq!(payload.limit_hit.as_deref(), Some("limit"));

    // max_children cap: one child fetched, one truncation report.
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/index.xml")))
        .await
        .expect("index parses");
    assert_eq!(payload.sitemaps.len(), 1);
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_children"));

    // A gzip bomb is capped at 4 × max_bytes.
    let error = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/bomb.xml.gz")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TooLarge);

    // An oversized fetched body hits the fetch cap.
    let error = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/oversize.xml")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TooLarge);

    // Malformed XML carries the parser line/column.
    let error = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/malformed.xml")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ExtractionFailed);
    let message = error.to_string();
    assert!(message.contains("line"), "{message}");
    assert!(message.contains("column"), "{message}");
}

#[tokio::test]
async fn lastmod_is_data_only_and_invalid_values_are_counted() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/lastmod.xml")))
        .await
        .expect("lastmod variants parse");
    assert_eq!(payload.url_count, 4);
    assert_eq!(payload.invalid_lastmod, 2);
    let lastmods: Vec<Option<&str>> = payload
        .urls
        .iter()
        .map(|url| url.lastmod.as_deref())
        .collect();
    assert_eq!(
        lastmods,
        vec![
            Some("2026-01-02"),
            Some("2026-01-02T03:04:05+02:00"),
            None,
            None
        ]
    );
}

#[tokio::test]
async fn robots_and_private_addresses_block_a_sitemap_fetch() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .pipeline
        .sitemap(sitemap_input(
            fixture.url("127.0.0.2", "/blocked/sitemap.xml"),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::BlockedByRobots);
    assert_eq!(fixture.log.count("/blocked/sitemap.xml"), 0);
    assert!(fixture.log.count("/robots.txt") >= 1);

    let error = env
        .pipeline
        .sitemap(sitemap_input(
            "http://169.254.169.254/sitemap.xml".to_string(),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::BlockedPrivateAddress);
}

#[tokio::test]
async fn the_sitemap_tool_stores_nothing() {
    let (fixture, env) = env_with(|_| {}).await;
    env.pipeline
        .sitemap(sitemap_input(fixture.url("127.0.0.1", "/index.xml")))
        .await
        .expect("index parses");
    assert_eq!(env.blob_count(), 0, "no blob is written");
    let db = env.db();
    for table in ["pages", "extractions", "jobs", "fetch_events"] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} must stay empty");
    }
}

// ---------------------------------------------------------------------------
// crawl_start.sitemap_urls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crawl_seeded_from_a_sitemap_fetches_the_unlinked_page() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            vec![fixture.url("127.0.0.1", "/site-sitemap.xml")],
            Some(web_widow::crawl::CrawlScopeInput {
                exclude_patterns: vec![r"\?p=2".to_string()],
                ..Default::default()
            }),
            None,
        ))
        .await
        .expect("crawl starts");

    let summary = started.sitemap.as_ref().expect("sitemap summary");
    assert_eq!(summary.urls_found, 6);
    // enqueued: /site/sitemap-only and /site/other. Skipped: excluded query,
    // off-site host, the seed duplicate and the in-sitemap duplicate.
    assert_eq!(summary.urls_enqueued, 2);
    assert_eq!(summary.urls_skipped, 4);
    assert!(summary.errors.is_empty());

    let status = wait_terminal(&env, &started.job_id).await;
    assert_eq!(status.state, "done", "{status:?}");
    assert!(status.limits_hit.is_empty(), "{:?}", status.limits_hit);

    let urls = fetched_urls(&env, &started.job_id).await;
    assert!(
        urls.contains(&fixture.url("127.0.0.1", "/site/sitemap-only")),
        "the sitemap-only page must enter the frontier: {urls:?}"
    );
    assert!(urls.contains(&fixture.url("127.0.0.1", "/site/other")));
    assert!(
        !urls.iter().any(|url| url.contains("/site/excluded")),
        "the exclusion pattern must apply to sitemap entries: {urls:?}"
    );
    assert!(
        !urls.iter().any(|url| url.contains("127.0.0.9")),
        "an off-site sitemap entry must be skipped: {urls:?}"
    );
    assert_eq!(fixture.log.count("/site/sitemap-only"), 1);
}

#[tokio::test]
async fn an_empty_start_urls_derives_the_site_set_from_the_sitemap_url() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            Vec::new(),
            vec![fixture.url("127.0.0.1", "/site-sitemap-only.xml")],
            None,
            None,
        ))
        .await
        .expect("crawl starts");

    let summary = started.sitemap.as_ref().expect("sitemap summary");
    assert_eq!(summary.urls_found, 3);
    assert_eq!(summary.urls_enqueued, 2);
    assert_eq!(summary.urls_skipped, 1, "the off-site entry is skipped");

    let status = wait_terminal(&env, &started.job_id).await;
    assert_eq!(status.state, "done", "{status:?}");
    let urls = fetched_urls(&env, &started.job_id).await;
    assert_eq!(
        urls,
        BTreeSet::from([
            fixture.url("127.0.0.1", "/site/other"),
            fixture.url("127.0.0.1", "/site/sitemap-only"),
        ]),
        "the sitemap URL supplies the site, not a seed page"
    );
}

#[tokio::test]
async fn explicit_seeds_still_bypass_scope_while_sitemap_entries_do_not() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            vec![fixture.url("127.0.0.1", "/site-sitemap.xml")],
            Some(web_widow::crawl::CrawlScopeInput {
                include_paths: vec!["/site/sitemap-only".to_string()],
                ..Default::default()
            }),
            None,
        ))
        .await
        .expect("crawl starts");

    let summary = started.sitemap.as_ref().expect("sitemap summary");
    assert_eq!(summary.urls_found, 6);
    assert_eq!(summary.urls_enqueued, 1, "only /site/sitemap-only passes");

    let status = wait_terminal(&env, &started.job_id).await;
    assert_eq!(status.state, "done", "{status:?}");
    let urls = fetched_urls(&env, &started.job_id).await;
    assert!(
        urls.contains(&fixture.url("127.0.0.1", "/site/")),
        "{urls:?}"
    );
    assert!(urls.contains(&fixture.url("127.0.0.1", "/site/sitemap-only")));
    assert!(
        !urls.contains(&fixture.url("127.0.0.1", "/site/other")),
        "a discovered entry outside include_paths is skipped: {urls:?}"
    );
}

#[tokio::test]
async fn max_pages_bounds_sitemap_seeding() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            vec![fixture.url("127.0.0.1", "/site-sitemap.xml")],
            None,
            Some(1),
        ))
        .await
        .expect("crawl starts");
    let summary = started.sitemap.as_ref().expect("sitemap summary");
    assert_eq!(
        summary.urls_enqueued, 0,
        "the explicit seed fills max_pages"
    );
    assert_eq!(summary.urls_skipped, 6);

    let status = wait_terminal(&env, &started.job_id).await;
    assert_eq!(status.counters.fetched, 1, "{status:?}");
    assert!(status.limits_hit.iter().any(|hit| hit == "max_pages"));
}

#[tokio::test]
async fn a_sitemap_error_is_recorded_and_the_other_seeds_start() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            vec![
                fixture.url("127.0.0.1", "/malformed.xml"),
                fixture.url("127.0.0.1", "/site-sitemap-only.xml"),
            ],
            None,
            None,
        ))
        .await
        .expect("crawl starts");

    let summary = started.sitemap.as_ref().expect("sitemap summary");
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert_eq!(summary.errors[0].code, "extraction_failed");
    assert_eq!(summary.urls_enqueued, 2);

    let status = wait_terminal(&env, &started.job_id).await;
    assert_eq!(status.state, "done", "{status:?}");
    let urls = fetched_urls(&env, &started.job_id).await;
    assert!(
        urls.contains(&fixture.url("127.0.0.1", "/site/other")),
        "{urls:?}"
    );
    assert!(urls.contains(&fixture.url("127.0.0.1", "/site/sitemap-only")));
}

#[tokio::test]
async fn crawl_start_without_sitemap_urls_keeps_the_pre_sitemap_payload() {
    let (fixture, env) = env_with(|_| {}).await;
    let started = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            Vec::new(),
            None,
            None,
        ))
        .await
        .expect("crawl starts");
    let value = serde_json::to_value(&started).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(
        object.keys().cloned().collect::<Vec<_>>(),
        vec!["job_id", "limits", "state"],
        "no sitemap key when the input has none"
    );
    assert_eq!(object["limits"]["max_depth"], json!(5));
    wait_terminal(&env, &started.job_id).await;
}

#[tokio::test]
async fn sitemap_inputs_are_validated_before_any_fetch() {
    let (fixture, env) = env_with(|_| {}).await;

    let error = env
        .crawler
        .start(start_input(Vec::new(), Vec::new(), None, None))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert!(error.detail().unwrap_or_default().contains("/start_urls"));

    let error = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            vec!["not-a-url".to_string()],
            None,
            None,
        ))
        .await
        .unwrap_err();
    assert!(
        error
            .detail()
            .unwrap_or_default()
            .contains("/sitemap_urls/0")
    );

    let many: Vec<String> = (0..6)
        .map(|index| fixture.url("127.0.0.1", &format!("/sitemap-{index}.xml")))
        .collect();
    let error = env
        .crawler
        .start(start_input(
            vec![fixture.url("127.0.0.1", "/site/")],
            many,
            None,
            None,
        ))
        .await
        .unwrap_err();
    assert!(error.detail().unwrap_or_default().contains("/sitemap_urls"));
    assert_eq!(fixture.log.count("/sitemap-0.xml"), 0);
}

#[test]
fn sitemap_config_defaults_and_bounds_are_validated() {
    let config = Config::default();
    assert_eq!(config.sitemap.max_bytes, 20 * 1024 * 1024);
    assert_eq!(config.sitemap.max_urls, 5_000);
    assert_eq!(config.sitemap.max_children, 5);

    for (bytes, urls, children) in [
        (0u64, 5_000usize, 5usize),
        (64 * 1024 * 1024 + 1, 5_000, 5),
        (20 * 1024 * 1024, 0, 5),
        (20 * 1024 * 1024, 50_001, 5),
        (20 * 1024 * 1024, 5_000, 0),
        (20 * 1024 * 1024, 5_000, 11),
    ] {
        let mut bad = Config::default();
        bad.sitemap.max_bytes = bytes;
        bad.sitemap.max_urls = urls;
        bad.sitemap.max_children = children;
        assert!(
            bad.validate().is_err(),
            "sitemap {bytes}/{urls}/{children} must be refused"
        );
    }

    let mut edge = Config::default();
    edge.sitemap.max_bytes = 1;
    edge.sitemap.max_urls = 1;
    edge.sitemap.max_children = 1;
    edge.validate().expect("edge values are accepted");
}
