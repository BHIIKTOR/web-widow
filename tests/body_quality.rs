//! Structured body and extract quality.
//!
//! Every fixture is served by the loopback fixture server; the red baselines
//! were captured separately against the previous binary.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestEnv, start_fixture, test_config};
use serde_json::{Value, json};
use web_widow::WidowError;
use web_widow::crawl::{
    CrawlLimitsInput, CrawlRenderPolicy, CrawlResultsInput, CrawlStartInput, CrawlStatusPayload,
};
use web_widow::monitor::{MonitorCheckInput, MonitorCreateInput};
use web_widow::pipeline::{
    ExtractInput, FetchPageInput, RenderPolicy, render_extract_text, render_metadata_summary,
};
use web_widow::render::{BlockedCounts, RenderFuture, RenderedPage, Renderer};

fn fetch_input(
    url: &str,
    include_content: Option<bool>,
    freshness_s: Option<u64>,
) -> FetchPageInput {
    FetchPageInput {
        url: url.to_string(),
        mode: None,
        render: None,
        freshness_s,
        max_chars: None,
        include_links: None,
        include_content,
    }
}

fn extract_input(
    url: &str,
    schema: Value,
    render: Option<RenderPolicy>,
    use_cache: Option<bool>,
    max_field_chars: Option<usize>,
    max_chars: Option<usize>,
) -> ExtractInput {
    ExtractInput {
        url: url.to_string(),
        schema,
        render,
        use_cache,
        max_field_chars,
        max_chars,
    }
}

#[derive(Debug)]
struct StubRenderer {
    html: String,
    fail: bool,
}

impl Renderer for StubRenderer {
    fn render<'a>(&'a self, url: &'a url::Url) -> RenderFuture<'a> {
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

async fn env_with_renderer(html: &str, fail: bool) -> (common::Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let env = TestEnv::with_renderer(
        test_config(),
        Arc::new(StubRenderer {
            html: html.to_string(),
            fail,
        }),
    )
    .await;
    (fixture, env)
}

fn latest_extraction_json(env: &TestEnv) -> Value {
    let stored: String = env
        .db()
        .query_row(
            "SELECT result_json FROM extractions ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&stored).unwrap()
}

// -----------------------------------------------------------------------
#[tokio::test]
async fn structured_body_crosses_and_the_cache_stores_it_once() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/article");

    let first = env
        .pipeline
        .fetch_page(fetch_input(&url, None, None))
        .await
        .unwrap();
    let body = first.markdown.clone();
    assert_eq!(first.payload.markdown.as_deref(), Some(body.as_str()));
    assert_eq!(first.payload.markdown_chars, body.chars().count());
    assert!(body.contains("lighthouse keeper"));

    // The cached payload keeps the body only once (top-level `markdown`); the
    // payload object has no duplicate `markdown` key.
    let stored = latest_extraction_json(&env);
    assert!(stored["payload"].get("markdown").is_none(), "{stored}");
    assert_eq!(stored["markdown"], json!(body));

    let cached = env
        .pipeline
        .fetch_page(fetch_input(&url, None, Some(3_600)))
        .await
        .unwrap();
    assert!(cached.payload.cached);
    assert_eq!(
        cached.payload.markdown.as_deref(),
        Some(cached.markdown.as_str())
    );

    let metadata = env
        .pipeline
        .fetch_page(fetch_input(&url, Some(false), Some(3_600)))
        .await
        .unwrap();
    assert!(metadata.payload.markdown.is_none());
    let summary = render_metadata_summary(&metadata);
    assert!(summary.contains("status: 200"), "{summary}");
    assert!(summary.contains("render_mode: static"), "{summary}");
    assert!(
        !summary.contains("lighthouse"),
        "no body in metadata-only: {summary}"
    );
}

#[tokio::test]
async fn a_pre_m03_cached_payload_delivers_the_body() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/article");
    env.pipeline
        .fetch_page(fetch_input(&url, None, None))
        .await
        .unwrap();

    // Simulate the pre-v0.3 shape: no payload.markdown, no links_requested and no
    // render_quality. The top-level body is still there.
    let db = env.db();
    let (id, stored): (i64, String) = db
        .query_row(
            "SELECT id, result_json FROM extractions ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: Value = serde_json::from_str(&stored).unwrap();
    let payload = value["payload"].as_object_mut().unwrap();
    payload.remove("markdown");
    payload.remove("links_requested");
    payload.remove("render_quality");
    db.execute(
        "UPDATE extractions SET result_json = ?1 WHERE id = ?2",
        rusqlite::params![serde_json::to_string(&value).unwrap(), id],
    )
    .unwrap();

    let cached = env
        .pipeline
        .fetch_page(fetch_input(&url, None, Some(3_600)))
        .await
        .unwrap();
    assert!(cached.payload.cached);
    assert_eq!(
        cached.payload.markdown.as_deref(),
        Some(cached.markdown.as_str())
    );
    assert!(
        cached
            .payload
            .markdown
            .as_deref()
            .unwrap()
            .contains("lighthouse")
    );
}

// -----------------------------------------------------------------------
#[tokio::test]
async fn visible_text_transforms_skip_script_style_and_template() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let payload = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/inline-script"),
            json!({
                "container": "body",
                "fields": {
                    "text": { "selector": ".body", "transform": "text" },
                    "nested": { "selector": ".nested", "transform": "text" },
                    "trimmed": { "selector": ".nested", "transform": "trim" },
                    "html": { "selector": ".nested", "transform": "html" }
                }
            }),
            None,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    let record = &payload.records[0];
    let text = record["text"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        text,
        "Visible sentence with prose only. Nested visible text"
    );
    assert_eq!(record["nested"], json!("Nested visible text"));
    assert_eq!(record["trimmed"], json!("Nested visible text"));
    assert!(
        record["html"]
            .as_str()
            .unwrap()
            .contains("NESTED_SCRIPT_TOKEN")
    );
    let serialized = serde_json::to_string(&payload.records).unwrap();
    assert!(!serialized.contains("SCRIPT_LEAK_TOKEN_178K"));
    assert!(!serialized.contains("color: red"));
    assert!(!serialized.contains("NESTED_SCRIPT_TOKEN\""));

    // The 178k CNBC-class body: the script payload never reaches `text`.
    let cnbc = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/cnbc"),
            json!({
                "container": "body",
                "fields": { "body": { "selector": ".article-body", "transform": "text" } }
            }),
            None,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    let body = cnbc.records[0]["body"].as_str().unwrap();
    assert_eq!(body, "CNBC visible sentence about markets.");
    assert!(!body.contains("AAAA"));
}

// ----------------------------------------------------------------------
#[tokio::test]
async fn extract_caps_are_presentation_only_and_cache_full_values() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/long-field?case=caps");
    let schema = json!({
        "container": ".item",
        "fields": {
            "big": { "selector": ".big" },
            "small": { "selector": ".small" }
        }
    });

    let first = env
        .pipeline
        .extract(extract_input(
            &url,
            schema.clone(),
            None,
            Some(true),
            Some(100),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(first.records.len(), 2);
    assert_eq!(
        first.records[0]["big"].as_str().unwrap().chars().count(),
        100
    );
    assert_eq!(first.truncated_fields, 2);
    assert!(first.truncated);
    assert_eq!(first.limit_hit.as_deref(), Some("max_field_chars"));

    // The stored evaluation keeps the full 10k values.
    let stored = latest_extraction_json(&env);
    assert_eq!(
        stored["records"][0]["big"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        10_000
    );

    // A later larger-cap cache hit returns the full values and re-derives flags.
    let second = env
        .pipeline
        .extract(extract_input(
            &url,
            schema.clone(),
            None,
            Some(true),
            None,
            None,
        ))
        .await
        .unwrap();
    assert!(second.cached);
    assert_eq!(
        second.records[0]["big"].as_str().unwrap().chars().count(),
        10_000
    );
    assert_eq!(second.truncated_fields, 0);
    assert!(!second.truncated);
    assert_eq!(second.limit_hit, None);

    // Total budget: the first record fits, the second does not.
    let budget = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/long-field?case=budget"),
            schema,
            None,
            None,
            Some(100),
            Some(200),
        ))
        .await
        .unwrap();
    assert_eq!(budget.records.len(), 1);
    assert_eq!(budget.record_count, 2);
    assert!(budget.truncated);
    assert_eq!(budget.limit_hit.as_deref(), Some("max_chars"));
    assert!(serde_json::to_value(&budget).is_ok());
}

#[tokio::test]
async fn max_records_takes_precedence_over_the_new_caps() {
    let fixture = start_fixture().await;
    let mut config = test_config();
    config.extract.max_records = 1;
    let env = TestEnv::new(config).await;
    let payload = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/long-field?case=precedence"),
            json!({
                "container": ".item",
                "fields": { "big": { "selector": ".big" } }
            }),
            None,
            None,
            Some(100),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(payload.records.len(), 1);
    assert_eq!(payload.record_count, 2);
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_records"));
    assert_eq!(payload.truncated_fields, 1);
}

// ----------------------------------------------------------------------
#[tokio::test]
async fn rendered_extract_carries_the_thin_signal() {
    let (fixture, env) = env_with_renderer(common::M03_THIN_SHELL, false).await;
    let payload = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/thin-shell"),
            json!({ "fields": { "app": { "selector": "#app" } } }),
            Some(RenderPolicy::Always),
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(payload.render_mode, "rendered");
    assert_eq!(payload.render_quality.as_deref(), Some("thin"));
    let text = render_extract_text(&payload);
    assert!(text.contains("render_quality: thin"), "{text}");

    let words = format!(
        "<html><body><div id=\"app\">{}</div></body></html>",
        "word ".repeat(250)
    );
    let (fixture, env) = env_with_renderer(&words, false).await;
    let payload = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/thin-shell"),
            json!({ "fields": { "app": { "selector": "#app" } } }),
            Some(RenderPolicy::Always),
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(payload.render_quality.as_deref(), Some("ok"));

    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let static_payload = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/thin-shell"),
            json!({ "fields": { "app": { "selector": "#app" } } }),
            Some(RenderPolicy::Never),
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(static_payload.render_mode, "static");
    assert!(static_payload.render_quality.is_none());

    let (fixture, env) = env_with_renderer(common::M03_THIN_SHELL, true).await;
    let fallback = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/thin-shell"),
            json!({ "fields": { "app": { "selector": "#app" } } }),
            None,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(fallback.render_mode, "static_fallback");
    assert!(fallback.render_quality.is_none());
}

// ----------------------------------------------------------------------
#[tokio::test]
async fn main_container_excludes_nav_and_aside() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/nav");

    let scoped = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({
                "container": ":main",
                "fields": {
                    "body": { "selector": "p", "all": true }
                }
            }),
            None,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&scoped.records).unwrap();
    assert!(text.contains("Article sentence"));
    assert!(!text.contains("NAV_DECOY"));
    assert!(!text.contains("ASIDE_DECOY"));
    assert!(!text.contains("FOOTER_DECOY"));

    let whole_body = env
        .pipeline
        .extract(extract_input(
            &url,
            json!({
                "container": "body",
                "fields": {
                    "body": { "selector": "p", "all": true }
                }
            }),
            None,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&whole_body.records).unwrap();
    assert!(text.contains("NAV_DECOY"));
    assert!(text.contains("ASIDE_DECOY"));
}

#[tokio::test]
async fn main_container_works_through_crawl_and_monitor() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let url = fixture.url("127.0.0.1", "/fixtures/nav");
    let schema = json!({
        "container": ":main",
        "fields": { "body": { "selector": "p", "all": true } }
    });

    let started = env
        .crawler
        .start(CrawlStartInput {
            start_urls: vec![url.clone()],
            sitemap_urls: Vec::new(),
            scope: None,
            schema: Some(schema.clone()),
            render_policy: Some(CrawlRenderPolicy::Never),
            limits: Some(CrawlLimitsInput {
                max_pages: Some(1),
                per_host_rate: Some(50.0),
                max_duration_s: Some(30),
            }),
            name: None,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let status: CrawlStatusPayload = loop {
        let status = env.crawler.status(&started.job_id).await.unwrap();
        if !matches!(status.state.as_str(), "queued" | "running") {
            break status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "crawl did not finish"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(status.state, "done", "{status:?}");
    let results = env
        .crawler
        .results(CrawlResultsInput {
            job_id: started.job_id,
            limit: Some(10),
            cursor: None,
        })
        .await
        .unwrap();
    let extracted = serde_json::to_string(&results.results[0].extracted).unwrap();
    assert!(extracted.contains("Article sentence"), "{extracted}");
    assert!(!extracted.contains("NAV_DECOY"));
    assert!(!extracted.contains("ASIDE_DECOY"));

    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: url.clone(),
            mode: None,
            schema: Some(schema),
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor.monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .unwrap();
    let stored = latest_extraction_json(&env);
    let text = stored.to_string();
    assert!(text.contains("Article sentence"), "{text}");
    assert!(!text.contains("NAV_DECOY"));
    assert!(!text.contains("ASIDE_DECOY"));
}

#[tokio::test]
async fn main_as_a_field_selector_is_still_schema_invalid() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    let error = env
        .pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/fixtures/nav"),
            json!({ "fields": { "main": { "selector": ":main" } } }),
            None,
            None,
            None,
            None,
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
