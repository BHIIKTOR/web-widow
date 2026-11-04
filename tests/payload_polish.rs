//! payload-polish tests.
//!
//! The rendered cases use a stub renderer: no browser is needed to prove the
//! `render_quality` boundary or that the field is absent on static results.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestEnv, start_fixture, test_config};
use url::Url;
use web_widow::WidowError;
use web_widow::config::Config;
use web_widow::extract::Mode;
use web_widow::pipeline::{FetchPageInput, RenderPolicy, profile_hash_for};
use web_widow::render::{BlockedCounts, RenderFuture, RenderedPage, Renderer};

fn fetch_input(
    url: &str,
    include_links: Option<bool>,
    render: Option<RenderPolicy>,
) -> FetchPageInput {
    FetchPageInput {
        url: url.to_string(),
        mode: None,
        render,
        freshness_s: None,
        max_chars: None,
        include_links,
        include_content: None,
        profile: None,
    }
}

#[derive(Debug)]
struct StubRenderer {
    html: String,
    fail: bool,
}

impl Renderer for StubRenderer {
    fn render<'a>(&'a self, url: &'a Url) -> RenderFuture<'a> {
        let html = self.html.clone();
        let url = url.clone();
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err(WidowError::RenderUnavailable("stub renderer".to_string()));
            }
            Ok(RenderedPage {
                html,
                final_url: url,
                status: 200,
                content_type: Some("text/html".to_string()),
                partial: false,
                stop_reason: None,
                blocked: BlockedCounts::default(),
                requests: 1,
                bytes: 0,
                duration: Duration::from_millis(1),
            })
        })
    }

    fn preflight(&self) -> Result<(), WidowError> {
        if self.fail {
            Err(WidowError::RenderUnavailable("stub renderer".to_string()))
        } else {
            Ok(())
        }
    }
}

async fn env_with_renderer(config: Config, renderer: StubRenderer) -> TestEnv {
    TestEnv::with_renderer(config, Arc::new(renderer)).await
}

fn word_page(words: usize) -> String {
    format!(
        "<html><body><article><h1>Rendered shell</h1><p>{}</p></article></body></html>",
        "word ".repeat(words)
    )
}

//  ----------------------------------------------------------------------
#[tokio::test]
async fn links_are_an_empty_list_when_not_requested() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let result = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/article"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert!(result.payload.links.is_empty());
    assert!(!result.payload.links_requested);
    // The serialized payload is total, not null.
    let value = serde_json::to_value(&result.payload).unwrap();
    assert_eq!(value["links"], serde_json::json!([]));
    assert_eq!(value["links_requested"], serde_json::json!(false));
}

#[tokio::test]
async fn links_are_present_when_requested() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let result = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/article"),
            Some(true),
            None,
        ))
        .await
        .unwrap();
    assert!(result.payload.links_requested);
    assert!(!result.payload.links.is_empty());
    assert!(
        result
            .payload
            .links
            .iter()
            .any(|link| link.url.ends_with("/relative"))
    );
}

/// Rewrite the stored extraction into the pre-v0.2 shape: `links: null` and no
/// `links_requested` / `render_quality`.
fn seed_legacy_cache(env: &TestEnv, include_links: bool) {
    let profile_hash = profile_hash_for(Mode::Main, env.config.extract.max_chars, include_links);
    let db = env.db();
    let (id, result_json): (i64, String) = db
        .query_row(
            "SELECT id, result_json FROM extractions WHERE profile_hash = ?1",
            [&profile_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&result_json).unwrap();
    let payload = value
        .get_mut("payload")
        .and_then(|payload| payload.as_object_mut())
        .expect("stored FetchPageResult payload");
    payload.insert("links".to_string(), serde_json::Value::Null);
    payload.remove("links_requested");
    payload.remove("render_quality");
    let legacy = serde_json::to_string(&value).unwrap();
    db.execute(
        "UPDATE extractions SET result_json = ?1 WHERE id = ?2",
        rusqlite::params![legacy, id],
    )
    .unwrap();
}

#[tokio::test]
async fn a_pre_m02_cached_payload_loads_without_inventing_links() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/article");

    // include_links: true stores links under its own profile, then the row is
    // rewritten to the pre-v0.2 shape.
    env.pipeline
        .fetch_page(fetch_input(&url, Some(true), None))
        .await
        .unwrap();
    seed_legacy_cache(&env, true);
    fixture.log.clear();

    let cached = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: url.clone(),
            mode: None,
            render: None,
            freshness_s: Some(3_600),
            max_chars: None,
            include_links: Some(true),
            include_content: None,
            profile: None,
        })
        .await
        .unwrap();
    assert!(cached.payload.cached);
    assert!(
        fixture.log.entries().is_empty(),
        "a fresh cache hit must not fetch"
    );
    assert!(cached.payload.links.is_empty(), "links are not invented");
    assert!(
        cached.payload.links_requested,
        "current request is reported"
    );
    assert!(
        cached.payload.render_quality.is_none(),
        "a pre-v0.2 payload does not gain render_quality"
    );

    // The false profile also loads, with links_requested false.
    env.fetch(&url).await.unwrap();
    seed_legacy_cache(&env, false);
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
            profile: None,
        })
        .await
        .unwrap();
    assert!(cached.payload.links.is_empty());
    assert!(!cached.payload.links_requested);
}

//  ----------------------------------------------------------------------
#[tokio::test]
async fn render_quality_is_thin_for_a_rendered_shell() {
    let fixture = start_fixture().await;
    let env = env_with_renderer(
        test_config(),
        StubRenderer {
            html: word_page(5),
            fail: false,
        },
    )
    .await;
    let result = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/article"),
            None,
            Some(RenderPolicy::Always),
        ))
        .await
        .unwrap();
    assert_eq!(result.payload.render_mode, "rendered");
    assert_eq!(result.payload.render_quality.as_deref(), Some("thin"));
    assert!(result.payload.word_count < 200);
}

#[tokio::test]
async fn render_quality_is_ok_for_a_rendered_article() {
    let fixture = start_fixture().await;
    let env = env_with_renderer(
        test_config(),
        StubRenderer {
            html: word_page(250),
            fail: false,
        },
    )
    .await;
    let result = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/article"),
            None,
            Some(RenderPolicy::Always),
        ))
        .await
        .unwrap();
    assert_eq!(result.payload.render_mode, "rendered");
    assert_eq!(result.payload.render_quality.as_deref(), Some("ok"));
    assert!(result.payload.word_count >= 200);
}

#[tokio::test]
async fn render_quality_is_absent_on_static_and_static_fallback() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let static_result = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/article"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(static_result.payload.render_mode, "static");
    assert!(static_result.payload.render_quality.is_none());

    let env = env_with_renderer(
        test_config(),
        StubRenderer {
            html: word_page(5),
            fail: true,
        },
    )
    .await;
    let fallback = env
        .pipeline
        .fetch_page(fetch_input(
            &fixture.url("127.0.0.1", "/js/four-scripts"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(fallback.payload.render_mode, "static_fallback");
    assert!(fallback.payload.render_quality.is_none());
    assert!(fallback.payload.render_warning.is_some());
}

#[test]
fn crawl_start_accepts_the_auto_alias_in_its_input_schema() {
    let input: web_widow::crawl::CrawlStartInput = serde_json::from_value(serde_json::json!({
        "start_urls": ["https://example.com/"],
        "render_policy": "auto"
    }))
    .unwrap();
    assert_eq!(
        input.render_policy,
        Some(web_widow::crawl::CrawlRenderPolicy::ThinOnly)
    );
}
