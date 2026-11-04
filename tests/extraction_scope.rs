//! v0.4 — one shared main-article source for `fetch_page mode=main` and
//! `:main`, and `selector:"*"` as the record scope itself.
//!
//! The captured pre-fix baselines live outside this repository; this file is
//! the permanent hermetic consistency check.

mod common;

use common::{TestEnv, start_fixture, test_config};
use serde_json::{Value, json};
use web_widow::extract::{ExtractOptions, Mode, extract_page, spec, spec_eval};
use web_widow::fetch::charset;
use web_widow::pipeline::{ExtractInput, FetchPageInput, RenderPolicy};

/// A sentence that only the article body contains; every decoy lives in the
/// page chrome (nav/aside/footer/head).
const PROSE: &str = "The harbourmaster logged forty-two identical crates";

fn fetch_input(url: &str, mode: Mode) -> FetchPageInput {
    FetchPageInput {
        url: url.to_string(),
        mode: Some(mode),
        render: Some(RenderPolicy::Never),
        freshness_s: None,
        max_chars: None,
        include_links: None,
        include_content: None,
        profile: None,
    }
}

fn extract_input(url: &str, schema: Value) -> ExtractInput {
    ExtractInput {
        url: url.to_string(),
        schema,
        render: Some(RenderPolicy::Never),
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

/// The scope the spec evaluates is the article the delivered body comes from:
/// the same distinctive sentence appears in both.
#[tokio::test]
async fn main_scope_carries_the_delivered_body_sentence() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/m04-scope");

    let page = env
        .pipeline
        .fetch_page(fetch_input(&url, Mode::Main))
        .await
        .unwrap();
    // The delivered body (`structuredContent.markdown`) and the text block must
    // both carry the article sentence.
    let markdown = page.payload.markdown.as_deref().unwrap_or_default();
    assert!(markdown.contains(PROSE), "delivered body: {markdown}");
    assert!(
        page.markdown.contains(PROSE),
        "text block: {}",
        page.markdown
    );

    let scoped = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({
                "container": ":main",
                "fields": {
                    "body": { "selector": "*" },
                    "body_html": { "selector": "*", "transform": "html" },
                    "body_list": { "selector": "*", "all": true }
                }
            }),
        ))
        .await
        .unwrap();

    assert_eq!(scoped.record_count, 1);
    assert!(
        scoped.field_errors.is_empty(),
        "unexpected field errors: {:?}",
        scoped.field_errors
    );
    let record = &scoped.records[0];
    let body = record["body"].as_str().unwrap_or_default();
    assert!(
        body.contains(PROSE),
        "scope text must match the body: {body}"
    );
    assert!(!body.contains("NAV_DECOY"), "nav reached the scope: {body}");
    assert!(
        !body.contains("ASIDE_DECOY"),
        "aside reached the scope: {body}"
    );
    assert!(
        !body.contains("FOOTER_DECOY"),
        "footer reached the scope: {body}"
    );
    assert!(
        !body.contains("SCRIPT_DECOY"),
        "script reached the scope: {body}"
    );

    //  — `*` is the scope element: inner HTML for `html`, one element for
    // `all: true`, and never the wrapper `<html>`/`<head>`.
    let html = record["body_html"].as_str().unwrap_or_default();
    assert!(html.contains("<p>The harbourmaster logged"), "{html}");
    assert!(html.contains("<article"), "{html}");
    assert!(!html.contains("<body"), "{html}");
    assert!(!html.contains("<head"), "{html}");
    let list = record["body_list"].as_array().expect("one-element array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].as_str().unwrap_or_default(), body);
}

/// A real selector keeps ordinary descendant CSS semantics next to `*`.
#[tokio::test]
async fn real_selectors_are_unchanged_next_to_star() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/m04-scope");

    let payload = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({
                "container": ":main",
                "fields": {
                    "first": { "selector": "p" },
                    "all": { "selector": "p", "all": true }
                }
            }),
        ))
        .await
        .unwrap();

    assert!(
        payload.field_errors.is_empty(),
        "{:?}",
        payload.field_errors
    );
    let record = &payload.records[0];
    assert_eq!(
        record["first"].as_str().unwrap_or_default(),
        "The harbourmaster logged forty-two identical crates on the pier before dawn."
    );
    let paragraphs = record["all"].as_array().expect("paragraphs");
    assert_eq!(paragraphs.len(), 5);
    assert!(paragraphs[0].as_str().unwrap_or_default().contains(PROSE));
}

/// Without a container the scope stays the document root, so `*` is the
/// page's visible text (head still skipped) and nothing else changes.
#[tokio::test]
async fn star_without_a_container_keeps_the_document_root_scope() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/m04-scope");

    let payload = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({ "fields": { "text": { "selector": "*" } } }),
        ))
        .await
        .unwrap();

    let text = payload.records[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains(PROSE), "{text}");
    assert!(
        text.contains("NAV_DECOY"),
        "root scope includes nav: {text}"
    );
    assert!(
        !text.contains("SCRIPT_DECOY"),
        "head/script skipped: {text}"
    );
}

/// A `:main` field selector is still invalid CSS, and the profile
/// identity of a spec is unchanged by this batch (the container text is already
/// canonical).
#[tokio::test]
async fn main_field_selector_is_still_schema_invalid() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/m04-scope");

    let error = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({ "fields": { "main": { "selector": ":main" } } }),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), web_widow::ErrorCode::SchemaInvalid);
    assert!(
        error
            .detail()
            .unwrap_or_default()
            .contains("/fields/main/selector"),
        "{error}"
    );
}

/// on a repaired page the `:main` records see the restored section, because
/// records and Markdown share the repaired article.
///
/// The source capture is operator data, not part of the committed fixtures:
/// without it the test has nothing to assert and returns.
#[test]
fn main_scope_on_a_repaired_page_includes_the_restored_section() {
    use std::path::PathBuf;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let corpus = std::env::var("WEB_WIDOW_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("corpus"));
    let Ok(bytes) = std::fs::read(
        corpus
            .join("real-site/source")
            .join("sqlite-whentouse.html"),
    ) else {
        eprintln!("SKIP: no operator capture for sqlite-whentouse");
        return;
    };
    let html = charset::decode(&bytes, Some("text/html")).text;
    let base = url::Url::parse("https://www.sqlite.org/whentouse.html").unwrap();

    let validated = spec::validate(&json!({
        "container": ":main",
        "fields": { "body": { "selector": "*" } }
    }))
    .unwrap();
    let compiled = spec::compile(&validated.spec).unwrap();
    let options = ExtractOptions {
        mode: Mode::Main,
        max_chars: 10_000_000,
        max_links: 0,
        include_links: false,
        ignore_selectors: Vec::new(),
    };
    let page = extract_page(
        &html,
        &base,
        &options,
        Some((&compiled, &spec_eval::EvalOptions { max_records: 10 })),
    )
    .unwrap();

    // The heading itself is a Markdown-space restoration (as for every restored
    // heading); the repaired *content* must be present in both views.
    assert!(
        page.main
            .markdown
            .contains("2. Situations Where A Client/Server RDBMS May Work Better"),
        "delivered body missing section 2"
    );
    let restored = "A good rule of thumb is to avoid using SQLite";
    assert!(
        page.main.markdown.contains(restored),
        "body missing restored prose"
    );
    let records = page.records.expect("records");
    let body = records.records[0]["body"].as_str().unwrap_or_default();
    assert!(
        body.contains(restored),
        "scope missing the restored prose: {body}"
    );
}
