//! The `extract` service end to end: final-URL resolution, profile and
//! extraction dedup, and the safeguards and error taxonomy applied to the
//! second tool.

mod common;

use common::{Fixture, TestEnv, start_fixture, test_config};
use serde_json::json;
use web_widow::ErrorCode;
use web_widow::pipeline::{ExtractInput, RenderPolicy};

fn list_spec() -> serde_json::Value {
    json!({
        "container": ".item",
        "fields": {
            "name": { "selector": "h2" },
            "price": { "selector": ".price", "transform": "number" },
            "href": { "selector": "a", "transform": "absolute_url" }
        }
    })
}

async fn env_with(patch: impl FnOnce(&mut web_widow::config::Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

fn input(url: String, spec: serde_json::Value, use_cache: Option<bool>) -> ExtractInput {
    ExtractInput {
        url,
        schema: spec,
        render: None,
        use_cache,
        max_field_chars: None,
        max_chars: None,
    }
}

// ---------------------------------------------------------------------------
//  — absolute URLs use the response's final URL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relative_urls_resolve_against_the_final_url_after_a_redirect() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .pipeline
        .extract(input(
            fixture.url("127.0.0.1", "/extract/go"),
            list_spec(),
            None,
        ))
        .await
        .expect("redirecting page extracts");

    assert_eq!(result.status, 200);
    assert_eq!(
        result.final_url,
        fixture.url("127.0.0.1", "/extract/items/")
    );
    assert_eq!(result.record_count, 2);
    // `../next` against the final URL `/extract/items/` is `/extract/next`; if
    // the requested URL `/extract/go` had been used as the base it would be
    // `/next`. The assertion therefore distinguishes the two bases.
    let resolved = result.records[0]["href"].as_str().unwrap_or_default();
    assert_eq!(resolved, fixture.url("127.0.0.1", "/extract/next"));
    assert_ne!(resolved, fixture.url("127.0.0.1", "/next"));
    assert_eq!(
        result.records[1]["href"],
        json!(fixture.url("127.0.0.1", "/absolute"))
    );
    assert_eq!(result.records[0]["price"].as_f64(), Some(10.0));
    assert_eq!(result.records[1]["price"].as_f64(), Some(20.5));
}

// ---------------------------------------------------------------------------
//  — profile dedup and use_cache semantics, counted in the database
// ---------------------------------------------------------------------------

fn counts(env: &TestEnv) -> (i64, i64, i64) {
    let db = env.db();
    let extractions: i64 = db
        .query_row("SELECT COUNT(*) FROM extractions", [], |row| row.get(0))
        .unwrap();
    let profiles: i64 = db
        .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
        .unwrap();
    let events: i64 = db
        .query_row("SELECT COUNT(*) FROM fetch_events", [], |row| row.get(0))
        .unwrap();
    (extractions, profiles, events)
}

#[tokio::test]
async fn use_cache_reuses_one_extraction_and_one_profile() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    let first = env
        .pipeline
        .extract(input(url.clone(), list_spec(), Some(true)))
        .await
        .unwrap();
    assert!(!first.cached);
    assert_eq!(counts(&env), (1, 1, 1));

    fixture.log.clear();
    let second = env
        .pipeline
        .extract(input(url, list_spec(), Some(true)))
        .await
        .unwrap();
    assert!(second.cached, "second call served from the cache window");
    assert_eq!(second.records, first.records);
    assert_eq!(counts(&env), (1, 1, 1), "no new rows, no new fetch event");
    assert!(
        fixture.log.entries().is_empty(),
        "a cache hit must not touch the network: {:?}",
        fixture.log.entries()
    );
}

#[tokio::test]
async fn without_use_cache_every_call_stores_a_new_extraction() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    env.pipeline
        .extract(input(url.clone(), list_spec(), None))
        .await
        .unwrap();
    env.pipeline
        .extract(input(url, list_spec(), None))
        .await
        .unwrap();
    assert_eq!(counts(&env), (2, 1, 2), "two extractions, one profile");
}

#[tokio::test]
async fn a_revalidated_page_reuses_the_cached_extraction() {
    let (fixture, env) = env_with(|config| config.extract.cache_freshness_s = 0).await;
    let url = fixture.url("127.0.0.1", "/article");
    let spec = json!({ "fields": { "title": { "selector": "h1" } } });

    let first = env
        .pipeline
        .extract(input(url.clone(), spec.clone(), Some(true)))
        .await
        .unwrap();
    assert_eq!(first.status, 200);
    assert!(!first.cached);
    assert_eq!(counts(&env), (1, 1, 1));

    // Window is disabled, so the second call revalidates; the fixture answers
    // 304 and the extraction is reused without a new row.
    let second = env
        .pipeline
        .extract(input(url, spec, Some(true)))
        .await
        .unwrap();
    assert_eq!(second.status, 304);
    assert!(second.cached);
    assert_eq!(
        counts(&env),
        (1, 1, 2),
        "one extraction, a second 304 event"
    );
}

#[tokio::test]
async fn a_different_spec_is_a_different_profile() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    env.pipeline
        .extract(input(url.clone(), list_spec(), None))
        .await
        .unwrap();
    env.pipeline
        .extract(input(
            url,
            json!({ "container": ".item", "fields": { "n": { "selector": "h2" } } }),
            None,
        ))
        .await
        .unwrap();
    let (extractions, profiles, events) = counts(&env);
    assert_eq!((extractions, profiles, events), (2, 2, 2));

    // Field order in the agent's JSON must not create a second profile.
    let db = env.db();
    // Profile hashes are versioned (`spec/v1/…`), so select by content rather
    // than by hash order.
    let mut statement = db.prepare("SELECT spec_json FROM profiles").unwrap();
    let stored: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(stored.len(), 2, "one profile per distinct spec");
    for canonical in &stored {
        assert!(
            !canonical.contains(' '),
            "canonical JSON has no whitespace: {canonical}"
        );
    }
    let list = stored
        .iter()
        .find(|canonical| canonical.contains("\"price\""))
        .expect("the list spec profile");
    // Canonical form: no whitespace, field names in sorted order.
    assert!(
        list.starts_with("{\"container\":\".item\",\"fields\":{"),
        "profiles store the canonical spec: {list}"
    );
    let name_at = list.find("\"name\"").unwrap_or(usize::MAX);
    let price_at = list.find("\"price\"").unwrap_or(usize::MAX);
    assert!(name_at < price_at, "fields are sorted: {list}");
}

// ---------------------------------------------------------------------------
// Safeguards and taxonomy carried over from
// ---------------------------------------------------------------------------

#[tokio::test]
async fn robots_and_private_addresses_still_apply_to_extract() {
    let (fixture, env) = env_with(|_| {}).await;
    let spec = json!({ "fields": { "title": { "selector": "h1" } } });

    let robots = env
        .pipeline
        .extract(input(
            fixture.url("127.0.0.5", "/article"),
            spec.clone(),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(robots.code(), ErrorCode::BlockedByRobots);
    assert!(!fixture.log.any_matching("127.0.0.5/article"));

    let private = env
        .pipeline
        .extract(input(
            "http://169.254.169.254/latest".to_string(),
            spec,
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(private.code(), ErrorCode::BlockedPrivateAddress);
}

#[tokio::test]
async fn unsupported_content_types_and_http_errors_are_typed() {
    let (fixture, env) = env_with(|_| {}).await;
    let spec = json!({ "fields": { "x": { "selector": "p" } } });

    let pdf = env
        .pipeline
        .extract(input(fixture.url("127.0.0.1", "/pdf"), spec.clone(), None))
        .await
        .unwrap_err();
    assert_eq!(pdf.code(), ErrorCode::UnsupportedContentType);

    let missing = env
        .pipeline
        .extract(input(fixture.url("127.0.0.1", "/missing"), spec, None))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), ErrorCode::HttpError);
    assert_eq!(missing.detail().as_deref(), Some("status=404"));
}

#[tokio::test]
async fn render_always_is_render_unavailable_and_invalid_specs_are_schema_invalid() {
    let (fixture, env) = env_with(|_| {}).await;

    let render = env
        .pipeline
        .extract(ExtractInput {
            url: fixture.url("127.0.0.1", "/article"),
            schema: list_spec(),
            render: Some(RenderPolicy::Always),
            use_cache: None,
            max_field_chars: None,
            max_chars: None,
        })
        .await
        .unwrap_err();
    assert_eq!(render.code(), ErrorCode::RenderUnavailable);

    let invalid = env
        .pipeline
        .extract(input(
            fixture.url("127.0.0.1", "/article"),
            json!({ "fields": { "p": { "selector": "p", "transform": "regex" } } }),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(invalid.code(), ErrorCode::SchemaInvalid);
    assert!(
        invalid
            .detail()
            .unwrap_or_default()
            .contains("/fields/p/transform"),
        "{invalid}"
    );
    assert!(
        fixture.log.entries().is_empty(),
        "rejected before any request"
    );
}

#[tokio::test]
async fn non_markup_pages_report_a_page_level_selector_error() {
    let (fixture, env) = env_with(|_| {}).await;
    let result = env
        .pipeline
        .extract(input(
            fixture.url("127.0.0.1", "/json"),
            json!({ "container": ".item", "fields": { "n": { "selector": "h2" } } }),
            None,
        ))
        .await
        .expect("a JSON body is scoped content, not an error");
    assert_eq!(result.record_count, 0);
    assert!(result.records.is_empty());
    assert_eq!(result.field_errors.len(), 1);
    assert_eq!(result.field_errors[0].record, None);
    assert_eq!(result.field_errors[0].code.as_str(), "selector_no_match");
}
