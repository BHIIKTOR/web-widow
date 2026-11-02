//! B03 — the `--no-default-features` build must stay fully functional without
//! Chrome, and an `always` request must fail with a typed `render_unavailable`
//! rather than a compile error or a silent static result.

#![cfg(not(feature = "render"))]

mod common;

use web_widow::pipeline::{FetchPageInput, RenderPolicy};

#[tokio::test]
async fn static_only_build_serves_static_and_reports_render_unavailable() {
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new(common::test_config()).await;

    let static_result = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            render: Some(RenderPolicy::Never),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect("static fetch works in the static-only build");
    assert_eq!(static_result.payload.render_mode, "static");
    assert!(
        static_result
            .markdown
            .contains("lighthouse keeper counted forty-one gulls")
    );

    let always = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/js/noscript"),
            mode: None,
            render: Some(RenderPolicy::Always),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect_err("always must not silently downgrade");
    assert!(matches!(
        always,
        web_widow::WidowError::RenderUnavailable(_)
    ));
    assert!(
        always.to_string().contains("render"),
        "the message should name the missing build feature: {always}"
    );

    let auto = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: fixture.url("127.0.0.1", "/js/noscript"),
            mode: None,
            render: Some(RenderPolicy::Auto),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect("auto falls back to the static result");
    assert_eq!(auto.payload.render_mode, "static_fallback");
    assert!(auto.payload.render_warning.is_some());
}
