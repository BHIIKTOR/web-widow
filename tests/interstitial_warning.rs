//! A01 — `content_warning: "interstitial_suspect"` ().
//!
//! The positive fixture is the BUYALFA verbatim shape: a product path that
//! redirects to `/gz/account-verification` and delivers 28 words. The two
//! negatives prove the conjunction is narrow: a direct login page (no drift)
//! and a short article (no verification path) never warn.

mod common;

use common::{TestEnv, start_fixture, test_config};
use web_widow::pipeline::{FetchPageInput, RenderPolicy, render_text};

fn input(url: String) -> FetchPageInput {
 FetchPageInput {
 url,
 mode: None,
 render: Some(RenderPolicy::Never),
 freshness_s: None,
 max_chars: None,
 include_links: None,
 include_content: None,
 profile: None,
 }
}

async fn env() -> (common::Fixture, TestEnv) {
 let fixture = start_fixture().await;
 (fixture, TestEnv::new(test_config()).await)
}

#[tokio::test]
async fn redirect_into_a_verification_path_with_a_short_body_is_warned() {
 let (fixture, env) = env().await;
 let result = env
.pipeline
.fetch_page(input(fixture.url("127.0.0.1", "/p/MLM26030984")))
.await
.expect("the interstitial is a successful fetch, never an error");

 assert_eq!(result.payload.status, 200);
 assert_eq!(
 result.payload.final_url,
 fixture.url("127.0.0.1", "/gz/account-verification")
);
 assert!(
 result.payload.word_count < 50,
 "the verbatim shape is a very short page: {}",
 result.payload.word_count
);
 assert_eq!(
 result.payload.content_warning.as_deref(),
 Some("interstitial_suspect")
);
 assert!(
 !result.markdown.trim().is_empty(),
 "the body is still delivered"
);
 let text = render_text(&result);
 assert!(
 text.contains("content_warning: \"interstitial_suspect\""),
 "{text}"
);
 let value = serde_json::to_value(&result.payload).expect("payload serializes");
 assert_eq!(value["content_warning"], "interstitial_suspect");
}

#[tokio::test]
async fn a_direct_login_page_without_drift_is_not_warned() {
 let (fixture, env) = env().await;
 let result = env
.pipeline
.fetch_page(input(fixture.url("127.0.0.1", "/login")))
.await
.expect("direct login page fetches");

 assert_eq!(result.payload.status, 200);
 assert!(result.payload.word_count < 50);
 assert_eq!(result.payload.final_url, fixture.url("127.0.0.1", "/login"));
 assert_eq!(result.payload.content_warning, None);
 assert!(!render_text(&result).contains("content_warning:"));
}

#[tokio::test]
async fn a_short_article_without_a_verification_segment_is_not_warned() {
 let (fixture, env) = env().await;
 let result = env
.pipeline
.fetch_page(input(fixture.url("127.0.0.1", "/short-article")))
.await
.expect("short article fetches");

 assert_eq!(result.payload.status, 200);
 assert!(result.payload.word_count < 50);
 assert_eq!(
 result.payload.final_url,
 fixture.url("127.0.0.1", "/short-article")
);
 assert_eq!(result.payload.content_warning, None);
}
