//! B01 `search` integration tests (IMP-B01 B1-1…B1-11).
//!
//! Every test uses the loopback fixture server; the real-DDG transcript is a
//! separate, bounded operator run recorded under `plan/artifacts/`.

mod common;

use common::{Fixture, TestEnv, start_fixture, test_config};
use web_widow::config::Config;
use web_widow::search::{SearchInput, SearchPayload};
use web_widow::{ErrorCode, WidowError};

fn input(query: &str) -> SearchInput {
    SearchInput {
        query: query.to_string(),
        site: None,
        freshness: None,
        limit: None,
    }
}

async fn env_with_serp(
    html_path: &str,
    lite_path: &str,
    patch: impl FnOnce(&mut Config),
) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    config.search.html_url = fixture.url("127.0.0.1", html_path);
    config.search.lite_url = fixture.url("127.0.0.1", lite_path);
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

async fn search(env: &TestEnv, input: SearchInput) -> SearchPayload {
    env.pipeline.search(input).await.expect("search succeeds")
}

fn error_code(error: &WidowError) -> ErrorCode {
    error.code()
}

fn row_counts(env: &TestEnv) -> (i64, i64, i64, i64, i64) {
    let db = env.db();
    let count = |table: &str| {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    (
        count("pages"),
        count("extractions"),
        count("profiles"),
        count("blobs"),
        count("jobs"),
    )
}

// B1-1 -----------------------------------------------------------------------
#[tokio::test]
async fn search_returns_the_captured_organic_results() {
    let (fixture, env) = env_with_serp("/serp/html", "/serp/lite", |_| {}).await;
    let payload = search(&env, input("rust async runtime comparison")).await;

    assert_eq!(payload.engine, "duckduckgo_html");
    assert_eq!(payload.result_count, 10);
    assert_eq!(payload.results.len(), 10);
    assert!(!payload.truncated);
    assert_eq!(payload.limit_hit, None);
    assert_eq!(payload.query, "rust async runtime comparison");
    assert_eq!(payload.freshness, "any");
    assert_eq!(payload.site, None);
    assert!(payload.final_url.starts_with("http://127.0.0.1:"));
    assert_eq!(
        payload.results[0].title,
        "Rust Async Runtimes: Tokio vs async-std vs smol 2026"
    );
    assert_eq!(
        payload.results[0].url,
        "https://rustify.rs/articles/rust-async-runtimes-tokio-vs-async-std-2026"
    );
    assert!(
        payload.results[0]
            .snippet
            .starts_with("Tokio dominates async Rust")
    );
    assert!(
        payload.results[1]
            .snippet
            .starts_with("Compare Rust async runtimes: Tokio, async-std, smol, and glommio.")
    );
    assert_eq!(
        payload.results[2].url,
        "https://www.nxrates.com/blog/rust-async-runtimes/"
    );
    assert!(
        payload
            .results
            .iter()
            .all(|result| result.engine == "duckduckgo_html")
    );
    assert_eq!(fixture.log.count_matching("/serp/lite"), 0);
}

// B1-2 -----------------------------------------------------------------------
#[tokio::test]
async fn lite_fallback_parses_when_the_primary_page_is_unparseable() {
    let (fixture, env) = env_with_serp("/serp/anomaly", "/serp/lite", |_| {}).await;
    let payload = search(&env, input("rust tokio async runtime")).await;

    assert_eq!(payload.engine, "duckduckgo_lite");
    assert_eq!(payload.result_count, 10);
    assert_eq!(
        payload.results[0].title,
        "Async in depth | Tokio - An asynchronous Rust runtime"
    );
    assert!(
        payload
            .results
            .iter()
            .all(|result| result.engine == "duckduckgo_lite")
    );
    assert_eq!(fixture.log.count_matching("/serp/anomaly"), 1);
    assert_eq!(fixture.log.count_matching("/serp/lite"), 1);
}

// B1-3 -----------------------------------------------------------------------
#[tokio::test]
async fn robots_refusal_is_final_and_never_tries_the_fallback_host() {
    let fixture = start_fixture().await;
    let mut config = test_config();
    // 127.0.0.2 disallows /private; 127.0.0.3 allows everything for our UA.
    config.search.html_url = fixture.url("127.0.0.2", "/private/serp");
    config.search.lite_url = fixture.url("127.0.0.3", "/serp/lite");
    let env = TestEnv::new(config).await;

    let error = env
        .pipeline
        .search(input("rust async runtime comparison"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedByRobots);
    assert_eq!(
        fixture.log.count_matching("127.0.0.2/private/serp"),
        0,
        "the disallowed SERP path must never be requested"
    );
    let lite_requests = fixture
        .log
        .entries()
        .iter()
        .filter(|entry| entry.starts_with("127.0.0.3"))
        .count();
    assert_eq!(lite_requests, 0, "a route refusal must not fall back");
    assert!(
        fixture.log.any_matching("127.0.0.2/robots.txt"),
        "robots.txt is fetched to make the decision"
    );
}

// B1-4 -----------------------------------------------------------------------
#[tokio::test]
async fn site_freshness_and_limit_reach_the_engine_as_specified() {
    let (fixture, env) = env_with_serp("/serp/html", "/serp/lite", |_| {}).await;
    let mut with_site = input("rust async");
    with_site.site = Some("rust-lang.github.io".to_string());
    with_site.freshness = Some(web_widow::search::Freshness::Month);
    with_site.limit = Some(10);
    search(&env, with_site).await;

    let entry = fixture
        .log
        .entries()
        .into_iter()
        .find(|entry| entry.contains("/serp/html?"))
        .expect("the html endpoint was requested");
    assert!(
        entry.contains("q=rust+async+site%3Arust-lang.github.io"),
        "query carries the site qualifier: {entry}"
    );
    assert!(entry.contains("df=m"), "freshness maps to df=m: {entry}");
    assert!(!entry.contains("&s="), "no pagination parameter: {entry}");
    assert!(!entry.contains("nextParams"), "no pagination: {entry}");
    assert!(!entry.contains("api="), "no API mode: {entry}");

    fixture.log.clear();
    let payload = search(
        &env,
        SearchInput {
            query: "rust async runtime comparison".to_string(),
            site: None,
            freshness: None,
            limit: Some(3),
        },
    )
    .await;
    assert_eq!(payload.results.len(), 3);
    assert_eq!(payload.result_count, 10);
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("limit"));
}

// B1-5 -----------------------------------------------------------------------
#[tokio::test]
async fn site_is_enforced_on_results_not_only_requested() {
    let (_fixture, env) = env_with_serp("/serp/html", "/serp/lite", |_| {}).await;
    let payload = search(
        &env,
        SearchInput {
            query: "rust async".to_string(),
            site: Some("rust-lang.github.io".to_string()),
            freshness: None,
            limit: None,
        },
    )
    .await;
    assert_eq!(payload.result_count, 2);
    assert_eq!(payload.results.len(), 2);
    assert!(payload.results.iter().all(|result| {
        let host = result
            .url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        host == "rust-lang.github.io"
    }));
    assert!(
        !payload
            .results
            .iter()
            .any(|result| result.url.contains("rustify.rs")),
        "the off-site first result is filtered"
    );
}

// B1-8 -----------------------------------------------------------------------
#[tokio::test]
async fn a_zero_result_page_is_a_success() {
    let (fixture, env) = env_with_serp("/serp/empty", "/serp/lite", |_| {}).await;
    let payload = search(&env, input("site:zzqx-nonexistent-4f9c.invalid")).await;
    assert_eq!(payload.engine, "duckduckgo_html");
    assert!(payload.results.is_empty());
    assert_eq!(payload.result_count, 0);
    assert!(!payload.truncated);
    assert_eq!(payload.limit_hit, None);
    assert_eq!(
        fixture.log.count_matching("/serp/lite"),
        0,
        "an empty page is a success, not a fallback trigger"
    );
}

#[tokio::test]
async fn an_anomaly_page_is_a_typed_failure() {
    let (fixture, env) = env_with_serp("/serp/anomaly", "/serp/anomaly", |_| {}).await;
    let error = env
        .pipeline
        .search(input("rust async runtime comparison"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::ExtractionFailed);
    assert_eq!(fixture.log.count_matching("/serp/anomaly"), 2);
}

// B1-9 -----------------------------------------------------------------------
#[tokio::test]
async fn search_writes_no_content_rows() {
    let (_fixture, env) = env_with_serp("/serp/html", "/serp/lite", |_| {}).await;
    let before = row_counts(&env);
    search(&env, input("rust async runtime comparison")).await;
    assert_eq!(row_counts(&env), before, "success writes no content rows");

    let (_fixture, failing) = env_with_serp("/serp/anomaly", "/serp/anomaly", |_| {}).await;
    let before = row_counts(&failing);
    let error = failing
        .pipeline
        .search(input("rust async runtime comparison"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::ExtractionFailed);
    assert_eq!(
        row_counts(&failing),
        before,
        "failure writes no content rows either"
    );
}

// B1-10 ----------------------------------------------------------------------
#[tokio::test]
async fn two_searches_respect_the_configured_interval() {
    let (fixture, env) = env_with_serp("/serp/html", "/serp/lite", |config| {
        config.fetch.min_interval_ms = 300;
    })
    .await;
    search(&env, input("rust async runtime comparison")).await;
    search(&env, input("rust async runtime comparison")).await;

    let stamps = fixture.log.stamps_matching("/serp/html");
    assert_eq!(stamps.len(), 2, "one SERP request per call");
    let delta = stamps[1].duration_since(stamps[0]);
    // Timer granularity can land a fraction under the configured floor; a
    // missing gate would be ~0-5 ms, so 290 ms still distinguishes the cases.
    assert!(
        delta >= std::time::Duration::from_millis(290),
        "host gate spaced the requests: {delta:?}"
    );
}

#[tokio::test]
async fn a_serp_redirect_to_a_private_address_is_blocked_before_connecting() {
    let (fixture, env) = env_with_serp("/serp/redirect-private", "/serp/lite", |_| {}).await;
    let error = env
        .pipeline
        .search(input("rust async runtime comparison"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::BlockedPrivateAddress);
    assert_eq!(
        fixture.log.count_matching("/serp/lite"),
        0,
        "a private-address refusal is final"
    );
}
