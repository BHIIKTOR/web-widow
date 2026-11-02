//! Integration behaviour against a local scripted HTTP server (IMP-B01 A4–A9).
//!
//! Every test asserts a server-visible fact (request log, stored rows, blob
//! count) rather than only the tool output, so a check cannot pass by accident
//! of the fixture.

mod common;

use std::time::{Duration, Instant};

use common::{Fixture, TestEnv, start_fixture, start_slow_server, test_config};
use web_widow::ErrorCode;
use web_widow::core::error::WidowError;
use web_widow::extract::Mode;
use web_widow::pipeline::{FetchPageInput, RenderPolicy};

async fn env_with(patch: impl FnOnce(&mut web_widow::config::Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

fn error_code(error: &WidowError) -> ErrorCode {
    error.code()
}

// ---------------------------------------------------------------------------
// A4 — SSRF gate on every hop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redirect_to_private_address_is_blocked_on_the_hop() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/redirect-private");
    let error = env.fetch(&url).await.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedPrivateAddress);
    assert!(error.to_string().contains("169.254.169.254"), "{error}");
    // The first hop was requested; the second never left the process.
    assert!(fixture.log.any_matching("/redirect-private"));
    assert!(!fixture.log.any_matching("meta-data"));
}

#[tokio::test]
async fn redirect_to_a_blocked_scheme_is_refused() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/redirect-file"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedScheme);
}

#[tokio::test]
async fn unsupported_scheme_is_refused_before_any_request() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env.fetch("ftp://127.0.0.1/etc/passwd").await.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedScheme);
    assert!(
        fixture.log.entries().is_empty(),
        "{:?}",
        fixture.log.entries()
    );
}

#[tokio::test]
async fn hostname_resolving_to_a_private_address_is_blocked() {
    // The plan requires the hostname case, not only a literal IP: a name that
    // resolves into loopback/private space must be refused, and the request must
    // never reach the server.
    let fixture = start_fixture().await;

    // Control: the same name and port are reachable when the operator allowlists
    // the loopback address, so the refusal below is the address classification.
    let mut allowed = test_config();
    allowed.fetch.allow_hosts = vec!["127.0.0.0/8".to_string(), "::1/128".to_string()];
    let allowed_env = TestEnv::new(allowed).await;
    let control = allowed_env
        .fetch(&format!("http://localhost:{}/article", fixture.port))
        .await
        .expect("an allowlisted loopback name is fetchable");
    assert_eq!(control.payload.status, 200);
    let requests_after_control = fixture.log.count_matching("/article");
    assert_eq!(requests_after_control, 1);
    let robots_after_control = fixture.log.count_matching("localhost/robots.txt");
    assert_eq!(robots_after_control, 1);

    // No allowlist entry: the name resolves to a blocked address.
    let mut blocked = test_config();
    blocked.fetch.allow_hosts.clear();
    let blocked_env = TestEnv::new(blocked).await;
    let error = blocked_env
        .fetch(&format!("http://localhost:{}/article", fixture.port))
        .await
        .unwrap_err();
    assert_eq!(
        error_code(&error),
        ErrorCode::BlockedPrivateAddress,
        "a hostname resolving to loopback must be blocked: {error}"
    );
    assert!(
        error.detail().unwrap_or_default().contains("127.0.0.0/8")
            || error.detail().unwrap_or_default().contains("::1/128"),
        "the error should name the matched range: {error}"
    );
    assert_eq!(
        fixture.log.count_matching("/article"),
        requests_after_control,
        "a blocked hostname must not reach the server: {:?}",
        fixture.log.entries()
    );
    assert_eq!(
        fixture.log.count_matching("localhost/robots.txt"),
        robots_after_control,
        "the gate runs before the robots fetch, so no new request may appear"
    );
}

#[tokio::test]
async fn a_loopback_site_without_the_allowlist_is_blocked() {
    let fixture = start_fixture().await;
    let mut config = test_config();
    config.fetch.allow_hosts.clear();
    let env = TestEnv::new(config).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/article"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedPrivateAddress);
    assert!(fixture.log.entries().is_empty());
}

// ---------------------------------------------------------------------------
// A5 — caps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_body_is_refused() {
    let (fixture, env) = env_with(|config| {
        config.fetch.min_interval_ms = 1;
    })
    .await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/oversize"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::TooLarge);
    assert_eq!(
        env.blob_count(),
        0,
        "no blob may be written for a rejected body"
    );
}

#[tokio::test]
async fn header_flood_is_refused_by_the_byte_cap() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/many-headers"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::HttpError);
    assert!(error.to_string().contains("headers exceeded"), "{error}");
}

#[tokio::test]
async fn slow_headers_hit_the_timeout() {
    let (address, _handle) = start_slow_server(Duration::from_secs(5)).await;
    let mut config = test_config();
    config.fetch.read_timeout_ms = 200;
    config.fetch.total_timeout_ms = 400;
    let env = TestEnv::new(config).await;
    let started = Instant::now();
    let error = env
        .fetch(&format!("http://127.0.0.1:{}/slow", address.port()))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::Timeout);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn redirect_limit_is_enforced() {
    let (fixture, env) = env_with(|_| {}).await;
    // Five hops are allowed.
    let result = env
        .fetch(&fixture.url("127.0.0.1", "/chain/5"))
        .await
        .expect("five redirects are within the limit");
    assert!(result.markdown.contains("chain end"));
    assert_eq!(
        result.payload.final_url,
        fixture.url("127.0.0.1", "/chain/0")
    );

    // Six hops are not.
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/chain/6"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::LimitExceeded);
}

#[tokio::test]
async fn unsupported_content_type_is_typed_and_bodiless() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/pdf"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::UnsupportedContentType);
    let detail = error.detail().unwrap_or_default();
    assert!(detail.contains("application/pdf"), "{detail}");
    assert_eq!(env.blob_count(), 0);
}

#[tokio::test]
async fn http_errors_keep_the_status() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/missing"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::HttpError);
    assert_eq!(error.detail().as_deref(), Some("status=404"));
}

#[tokio::test]
async fn retries_honour_retry_after_and_recover() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.1", "/retry-after"))
        .await
        .expect("two 429s then a 200");
    assert!(result.markdown.contains("after retry"));
    assert_eq!(fixture.log.count_matching("/retry-after"), 3);
}

#[tokio::test]
async fn other_5xx_is_retried_once() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.1", "/flaky-5xx"))
        .await
        .expect("one 500 then a 200");
    assert!(result.markdown.contains("recovered"));
    assert_eq!(fixture.log.count_matching("/flaky-5xx"), 2);
}

// ---------------------------------------------------------------------------
// A6 — robots enforcement, with server-side assertions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn robots_disallowed_path_is_never_requested() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.2", "/private/page"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    assert!(fixture.log.any_matching("127.0.0.2/robots.txt"));
    assert!(
        !fixture.log.any_matching("/private"),
        "a disallowed path must not be requested: {:?}",
        fixture.log.entries()
    );
}

#[tokio::test]
async fn redirect_to_a_robots_disallowed_path_is_not_followed() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.2", "/redirect-disallowed-path"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    assert!(fixture.log.any_matching("/redirect-disallowed-path"));
    assert!(
        !fixture.log.any_matching("/private/secret"),
        "a redirect must not bypass robots: {:?}",
        fixture.log.entries()
    );
}

#[tokio::test]
async fn redirect_to_another_host_applies_that_hosts_robots() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.1", "/redirect-other-host"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    // The target host was asked for robots.txt but never for the page.
    assert!(fixture.log.any_matching("127.0.0.5/robots.txt"));
    assert!(!fixture.log.any_matching("127.0.0.5/article"));
}

#[tokio::test]
async fn robots_user_agent_group_is_selected_over_the_wildcard() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.3", "/article"))
        .await
        .expect("our UA group allows this host");
    assert_eq!(result.payload.status, 200);
}

#[tokio::test]
async fn robots_5xx_is_a_conservative_disallow() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.4", "/article"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    assert!(!fixture.log.any_matching("/article"));
}

#[tokio::test]
async fn robots_404_allows_the_fetch() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.7", "/article"))
        .await
        .expect("absent robots.txt means allow");
    assert_eq!(result.payload.status, 200);
}

#[tokio::test]
async fn robots_disallow_all_for_our_agent() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .fetch(&fixture.url("127.0.0.5", "/article"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    assert!(!fixture.log.any_matching("/article"));
}

#[tokio::test]
async fn robots_is_cached_per_host() {
    let (fixture, env) = env_with(|_| {}).await;
    env.fetch(&fixture.url("127.0.0.1", "/article"))
        .await
        .unwrap();
    env.fetch(&fixture.url("127.0.0.1", "/article"))
        .await
        .unwrap();
    assert_eq!(
        fixture.log.count_matching("127.0.0.1/robots.txt"),
        1,
        "robots.txt must be cached, not re-fetched per call"
    );
}

#[tokio::test]
async fn crawl_delay_raises_the_request_interval() {
    let (fixture, env) = env_with(|config| {
        config.fetch.min_interval_ms = 10;
        // Generous timeouts: this test measures a deliberate 1 s politeness
        // wait, and a loaded machine must not turn the robots fetch into a
        // transport failure (which would legitimately block the page).
        config.fetch.read_timeout_ms = 5_000;
        config.fetch.total_timeout_ms = 10_000;
    })
    .await;
    // The fixture's robots.txt for 127.0.0.6 says `Crawl-delay: 1`, while the
    // configured minimum interval is 10 ms, so a multi-second total for three
    // requests can only come from the robots policy (also unit-tested in
    // core::robots).
    //
    // Measured as total elapsed for three fetches, not as a per-fetch
    // completion-to-completion gap: the gate reserves the interval from the
    // *start* of the previous request, so a slow request legitimately shrinks a
    // completion gap while the spacing itself is correct. Request time can only
    // add to the total, which keeps this bound monotone under load.
    let url = fixture.url("127.0.0.6", "/article");
    let started = Instant::now();
    for _ in 0..3 {
        env.fetch(&url).await.unwrap();
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1_800),
        "Crawl-delay 1s did not space three requests (two waits): {elapsed:?}"
    );
}

#[tokio::test]
async fn robots_override_is_off_by_default() {
    let (_, env) = env_with(|_| {}).await;
    assert!(env.config.robots.enabled);
    // No per-call capability exists: the declared input schema has exactly the
    // contract fields, so an agent cannot ask to ignore robots or reach LAN.
    let schema = schemars::schema_for!(FetchPageInput);
    let properties = schema
        .as_value()
        .get("properties")
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "freshness_s",
            "include_content",
            "include_links",
            "max_chars",
            "mode",
            "render",
            "url"
        ]
    );
}

// ---------------------------------------------------------------------------
// A7 — CAS dedup and conditional GET
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_page_twice_writes_one_blob_and_a_second_event() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/article");
    env.fetch(&url).await.unwrap();
    assert_eq!(env.blob_count(), 1);
    env.fetch(&url).await.unwrap();
    assert_eq!(
        env.blob_count(),
        1,
        "identical bytes must not be stored twice"
    );

    let db = env.db();
    let events: i64 = db
        .query_row("SELECT COUNT(*) FROM fetch_events", [], |row| row.get(0))
        .unwrap();
    let extractions: i64 = db
        .query_row("SELECT COUNT(*) FROM extractions", [], |row| row.get(0))
        .unwrap();
    let pages: i64 = db
        .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pages, 1, "one row per canonical URL");
    assert_eq!(events, 2, "one event per fetch, including the 304");
    assert_eq!(extractions, 1, "a 304 must not re-extract");
}

#[tokio::test]
async fn not_modified_reports_304_and_serves_the_stored_extraction() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/article");
    let first = env.fetch(&url).await.unwrap();
    assert_eq!(first.payload.status, 200);
    assert!(!first.payload.cached);

    let second = env.fetch(&url).await.unwrap();
    assert_eq!(second.payload.status, 304);
    assert!(second.payload.cached);
    assert_eq!(second.markdown, first.markdown);

    let db = env.db();
    let not_modified: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM fetch_events WHERE http_status = 304 AND raw_hash IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(not_modified, 1);
}

#[tokio::test]
async fn freshness_window_never_touches_the_network() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/article");
    env.fetch(&url).await.unwrap();
    fixture.log.clear();

    let cached = env
        .pipeline
        .fetch_page(FetchPageInput {
            url,
            mode: None,
            render: None,
            freshness_s: Some(3_600),
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .unwrap();
    assert!(cached.payload.cached);
    assert_eq!(cached.payload.status, 200);
    assert!(
        fixture.log.entries().is_empty(),
        "a fresh cache hit must not fetch: {:?}",
        fixture.log.entries()
    );
}

// ---------------------------------------------------------------------------
// A8 — charset
// ---------------------------------------------------------------------------

#[tokio::test]
async fn latin1_page_is_decoded_via_meta_charset() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.1", "/latin1"))
        .await
        .unwrap();
    assert!(result.markdown.contains("café"), "{}", result.markdown);
    assert!(result.markdown.contains("naïve"), "{}", result.markdown);
    assert_eq!(
        result.payload.title.as_deref(),
        Some("Café"),
        "{:?}",
        result.payload.title
    );
}

// ---------------------------------------------------------------------------
// A9 — truncation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn truncation_is_flagged_and_never_exceeds_the_cap() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            render: None,
            freshness_s: None,
            max_chars: Some(40),
            include_links: None,
            include_content: None,
        })
        .await
        .unwrap();
    assert!(result.payload.truncated);
    assert_eq!(result.payload.limit_hit.as_deref(), Some("max_chars"));
    assert!(result.payload.markdown_chars <= 40);
    assert_eq!(
        result.markdown.chars().count(),
        result.payload.markdown_chars
    );
}

#[tokio::test]
async fn per_call_cap_cannot_exceed_the_server_cap() {
    let (fixture, env) = env_with(|config| config.extract.max_chars = 50).await;
    let result = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            render: None,
            freshness_s: None,
            max_chars: Some(10_000_000),
            include_links: None,
            include_content: None,
        })
        .await
        .unwrap();
    assert!(result.payload.markdown_chars <= 50);
}

// ---------------------------------------------------------------------------
// Content scope and modes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_text_json_and_xml_are_delivered_as_text() {
    let (fixture, env) = env_with(|_| {}).await;
    let plain = env
        .fetch(&fixture.url("127.0.0.1", "/plain"))
        .await
        .unwrap();
    assert!(plain.markdown.contains("plain text body"));
    let json = env.fetch(&fixture.url("127.0.0.1", "/json")).await.unwrap();
    assert!(json.markdown.contains("\"answer\":42"));
    assert_eq!(json.payload.content_type, "application/json");
    let xml = env.fetch(&fixture.url("127.0.0.1", "/xml")).await.unwrap();
    assert!(xml.markdown.contains("<item>one</item>"));
}

#[tokio::test]
async fn render_always_is_render_unavailable() {
    let (fixture, env) = env_with(|_| {}).await;
    let error = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            render: Some(RenderPolicy::Always),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::RenderUnavailable);
    assert!(fixture.log.entries().is_empty());
}

#[tokio::test]
async fn scripts_and_styles_never_reach_the_markdown() {
    let (fixture, env) = env_with(|_| {}).await;
    for mode in [Mode::Main, Mode::Full, Mode::Text] {
        let result = env
            .pipeline
            .fetch_page(FetchPageInput {
                url: fixture.url("127.0.0.1", "/article"),
                mode: Some(mode),
                render: None,
                freshness_s: None,
                max_chars: None,
                include_links: None,
                include_content: None,
            })
            .await
            .unwrap();
        assert!(
            !result.markdown.contains("script-should-not-appear"),
            "mode {mode:?} leaked script content"
        );
        assert!(
            !result.markdown.contains("color:red"),
            "mode {mode:?} leaked CSS"
        );
    }
}

#[tokio::test]
async fn metadata_and_links_are_reported() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            render: None,
            freshness_s: None,
            max_chars: None,
            include_links: Some(true),
            include_content: None,
        })
        .await
        .unwrap();
    assert_eq!(result.payload.title.as_deref(), Some("Fixture Article"));
    assert_eq!(result.payload.byline.as_deref(), Some("Test Author"));
    assert_eq!(result.payload.site_name.as_deref(), Some("Fixture Site"));
    assert_eq!(
        result.payload.published.as_deref(),
        Some("2026-02-03T04:05:06Z")
    );
    assert!(result.payload.links_requested);
    let links = result.payload.links;
    assert!(links.iter().any(|link| link.url.ends_with("/relative")));
    assert!(links.iter().all(|link| link.url.starts_with("http")));
}

#[tokio::test]
async fn no_content_status_is_not_an_error() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .fetch(&fixture.url("127.0.0.1", "/empty"))
        .await
        .unwrap();
    assert_eq!(result.payload.status, 204);
    assert_eq!(result.payload.markdown_chars, 0);
}
