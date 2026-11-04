//! JavaScript rendering acceptance checks.
//!
//! These tests need a real Chrome/Chromium. When none is available they print an
//! explicit `SKIP` marker and return instead of failing, so a static-only host
//! can still run the suite. The evidence run sets `render.chrome_path` to the
//! Playwright Chrome for Testing binary.

#![cfg(feature = "render")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use url::Url;
use web_widow::WidowError;
use web_widow::pipeline::{ExtractInput, FetchPageInput, FetchPageResult, RenderPolicy};

fn input(url: String, policy: RenderPolicy) -> FetchPageInput {
    FetchPageInput {
        url,
        mode: None,
        render: Some(policy),
        freshness_s: None,
        max_chars: None,
        include_links: None,
        include_content: None,
    }
}

async fn fetch(
    env: &common::TestEnv,
    url: String,
    policy: RenderPolicy,
) -> Result<FetchPageResult, WidowError> {
    env.pipeline.fetch_page(input(url, policy)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noscript_fixture_escalates_across_render_policies() {
    let Some(chrome) =
        common::skip_without_chrome("noscript_fixture_escalates_across_render_policies")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = fixture.url("127.0.0.1", "/js/noscript");

    let never = fetch(&env, url.clone(), RenderPolicy::Never)
        .await
        .expect("static fetch");
    assert_eq!(never.payload.render_mode, "static");
    assert!(
        never.payload.word_count < 50,
        "noscript shell should be thin, got {} words",
        never.payload.word_count
    );
    assert!(
        !never.markdown.to_ascii_lowercase().contains("injectedword"),
        "static DOM must not contain the injected article"
    );

    let always = fetch(&env, url.clone(), RenderPolicy::Always)
        .await
        .expect("rendered fetch");
    assert_eq!(always.payload.render_mode, "rendered");
    assert!(always.markdown.contains("Rendered Article"));
    assert!(always.markdown.contains("injectedword"));
    assert!(always.payload.word_count >= 55);

    let auto = fetch(&env, url, RenderPolicy::Auto)
        .await
        .expect("auto fetch");
    assert_eq!(auto.payload.render_mode, "rendered");
    assert!(auto.markdown.contains("injectedword"));
    println!(
        "RENDER_MODES fixture=noscript never={} never_words={} always={} auto={}",
        never.payload.render_mode,
        never.payload.word_count,
        always.payload.render_mode,
        auto.payload.render_mode
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_injected_content_waits_for_network_quiet() {
    let Some(chrome) =
        common::skip_without_chrome("script_injected_content_waits_for_network_quiet")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = fixture.url("127.0.0.1", "/js/async");

    let rendered = fetch(&env, url, RenderPolicy::Always)
        .await
        .expect("rendered fetch");
    assert_eq!(rendered.payload.render_mode, "rendered");
    assert!(
        rendered.markdown.contains("ASYNC") && rendered.markdown.contains("FINAL"),
        "settle policy stopped before the final async DOM: {}",
        rendered.markdown
    );
    assert!(
        fixture.log.count_matching("/api/data") >= 4,
        "expected the four sequential XHRs, log: {:?}",
        fixture.log.entries()
    );
    println!(
        "ASYNC_TRANSCRIPT final_marker={} xhr_requests={} render_mode={}",
        rendered.markdown.contains("FINAL"),
        fixture.log.count_matching("/api/data"),
        rendered.payload.render_mode
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_chrome_is_render_unavailable_and_static_still_works() {
    let fixture = common::start_fixture().await;
    let missing = std::path::PathBuf::from("/nonexistent/web-widow-chrome");
    let mut config = common::test_config();
    config.render.discovery = false;
    config.render.chrome_path = Some(missing.clone());
    let env = common::TestEnv::new_rendering(config).await;

    let error = fetch(
        &env,
        fixture.url("127.0.0.1", "/js/noscript"),
        RenderPolicy::Always,
    )
    .await
    .expect_err("always must not downgrade silently");
    assert!(matches!(error, WidowError::RenderUnavailable(_)));
    let message = error.to_string();
    assert!(message.contains("render.chrome_path"), "{message}");
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );

    let static_result = fetch(
        &env,
        fixture.url("127.0.0.1", "/article"),
        RenderPolicy::Never,
    )
    .await
    .expect("static calls keep working without Chrome");
    assert_eq!(static_result.payload.render_mode, "static");
    assert!(
        static_result
            .markdown
            .contains("lighthouse keeper counted forty-one gulls")
    );

    // `auto` degrades visibly rather than failing the request.
    let auto = fetch(
        &env,
        fixture.url("127.0.0.1", "/js/noscript"),
        RenderPolicy::Auto,
    )
    .await
    .expect("auto falls back when Chrome is absent");
    assert_eq!(auto.payload.render_mode, "static_fallback");
    assert!(
        auto.payload
            .render_warning
            .as_deref()
            .unwrap_or_default()
            .contains("render.chrome_path")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_crash_restarts_and_next_render_succeeds() {
    let Some(chrome) =
        common::skip_without_chrome("browser_crash_restarts_and_next_render_succeeds")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.ceiling_ms = 3_000;
    config.render.settle_ms = 300;
    let env = Arc::new(common::TestEnv::new_rendering(config).await);
    let slow_url = fixture.url("127.0.0.1", "/js/slow");

    let task = tokio::spawn({
        let env = Arc::clone(&env);
        async move { fetch(&env, slow_url, RenderPolicy::Always).await }
    });

    let mut saw_request = false;
    for _ in 0..100 {
        if fixture.log.any_matching("/api/tick") {
            saw_request = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saw_request, "the slow render never started");
    let pid = env
        .renderer
        .browser_pid()
        .expect("browser pid while rendering");
    let status = tokio::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill command");
    assert!(status.success(), "kill -9 {pid} failed: {status:?}");

    let killed = tokio::time::timeout(Duration::from_secs(25), task)
        .await
        .expect("render task should finish after the crash");
    let crash_outcome;
    match killed.expect("render task should not panic") {
        Ok(result) => {
            crash_outcome = format!("ok:{}", result.payload.render_mode);
            assert!(
                matches!(
                    result.payload.render_mode.as_str(),
                    "rendered" | "rendered_partial"
                ),
                "unexpected retry result: {}",
                result.payload.render_mode
            );
        }
        Err(error) => {
            crash_outcome = format!("err:{}", error.code().as_str());
            assert!(
                matches!(
                    error,
                    WidowError::RenderFailed(_) | WidowError::RenderUnavailable(_)
                ),
                "unexpected crash error: {error}"
            );
        }
    }
    assert!(
        env.renderer.restart_count() >= 1,
        "restart count did not increment"
    );
    println!(
        "CRASH_TRANSCRIPT restarts={} killed_outcome={} next_started=true",
        env.renderer.restart_count(),
        crash_outcome
    );

    let next = fetch(
        &env,
        fixture.url("127.0.0.1", "/js/noscript"),
        RenderPolicy::Always,
    )
    .await
    .expect("next render after crash");
    assert_eq!(next.payload.render_mode, "rendered");
    assert!(next.markdown.contains("injectedword"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_terminates_the_browser_process() {
    let Some(chrome) = common::skip_without_chrome("shutdown_terminates_the_browser_process")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    env.renderer
        .render(&Url::parse(&fixture.url("127.0.0.1", "/js/noscript")).unwrap())
        .await
        .expect("warm render");
    let pid = env.renderer.browser_pid().expect("browser pid");
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    env.renderer.shutdown().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "browser process {pid} survived shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resource_blocking_preserves_scripts_and_xhr() {
    let Some(chrome) = common::skip_without_chrome("resource_blocking_preserves_scripts_and_xhr")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = Url::parse(&fixture.url("127.0.0.1", "/js/resources")).unwrap();

    let rendered = env
        .renderer
        .render(&url)
        .await
        .expect("resource fixture render");
    assert!(rendered.html.contains("API_VALUE"), "{}", rendered.html);
    assert!(rendered.blocked.image >= 1, "{:?}", rendered.blocked);
    assert!(rendered.blocked.font >= 1, "{:?}", rendered.blocked);
    assert!(rendered.blocked.stylesheet >= 1, "{:?}", rendered.blocked);

    let entries = fixture.log.entries();
    assert!(
        !entries
            .iter()
            .any(|entry| entry.contains("/js/asset/image.png")),
        "image was fetched: {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.contains("/js/asset/font.woff2")),
        "font was fetched: {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.contains("/js/asset/style.css")),
        "stylesheet was fetched: {entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry.contains("/api/data")),
        "XHR was not fetched: {entries:?}"
    );
    println!(
        "RESOURCE_TRANSCRIPT blocked={:?} xhr={} server_log={entries:?}",
        rendered.blocked,
        entries.iter().any(|entry| entry.contains("/api/data"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caps_stop_big_pages_and_request_floods() {
    let Some(chrome) = common::skip_without_chrome("caps_stop_big_pages_and_request_floods") else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;

    let big = env
        .renderer
        .render(&Url::parse(&fixture.url("127.0.0.1", "/js/big")).unwrap())
        .await
        .expect("big page render");
    assert!(big.partial, "big page should be partial");
    assert_eq!(big.stop_reason.as_deref(), Some("max_bytes"));
    assert!(
        !big.html.contains("data-complete"),
        "the late marker should not have run after the byte cap"
    );

    let flood = env
        .renderer
        .render(&Url::parse(&fixture.url("127.0.0.1", "/js/many-requests")).unwrap())
        .await
        .expect("request-flood render");
    assert!(flood.partial, "request flood should be partial: {flood:?}");
    assert_eq!(flood.stop_reason.as_deref(), Some("max_requests"));
    assert!(
        !flood.html.contains("data-complete"),
        "the late marker should not have run after the request cap"
    );
    assert!(flood.requests <= 201, "request count: {}", flood.requests);
    println!(
        "CAP_TRANSCRIPT big_bytes={} big_requests={} big_reason={:?} flood_bytes={} flood_requests={} flood_reason={:?}",
        big.bytes, big.requests, big.stop_reason, flood.bytes, flood.requests, flood.stop_reason
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_context_isolates_cookies_and_profile_is_removed() {
    let Some(chrome) =
        common::skip_without_chrome("ephemeral_context_isolates_cookies_and_profile_is_removed")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;

    let first = env
        .renderer
        .render(&Url::parse(&fixture.url("127.0.0.1", "/js/cookie/set")).unwrap())
        .await
        .expect("cookie set render");
    assert!(first.html.contains("cookie-set"));

    let second = env
        .renderer
        .render(&Url::parse(&fixture.url("127.0.0.1", "/js/cookie/read")).unwrap())
        .await
        .expect("cookie read render");
    assert!(
        !second.html.contains("leak=1"),
        "cookie leaked between ephemeral contexts: {}",
        second.html
    );
    assert!(second.html.contains("COOKIE["), "{}", second.html);

    let profile = env.data_dir().join("render/chrome-profile");
    assert!(
        profile.exists(),
        "profile should exist while the browser runs"
    );
    env.renderer.shutdown().await;
    assert!(
        !profile.exists(),
        "throwaway profile was not removed on shutdown"
    );
    println!(
        "CONTEXT_TRANSCRIPT cookie_leaked={} profile_present_after_shutdown={}",
        second.html.contains("leak=1"),
        profile.exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_reuses_rendering_through_the_shared_pipeline() {
    let Some(chrome) =
        common::skip_without_chrome("extract_reuses_rendering_through_the_shared_pipeline")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let schema = serde_json::json!({
        "container": "article",
        "fields": {
            "title": { "selector": "h1", "transform": "text" },
            "body": { "selector": "p", "transform": "text" }
        }
    });

    let static_result = env
        .pipeline
        .extract(ExtractInput {
            url: fixture.url("127.0.0.1", "/js/noscript"),
            schema: schema.clone(),
            render: Some(RenderPolicy::Never),
            use_cache: None,
            max_field_chars: None,
            max_chars: None,
        })
        .await
        .expect("static extraction");
    assert_eq!(static_result.record_count, 0);
    assert_eq!(static_result.render_mode, "static");

    let rendered = env
        .pipeline
        .extract(ExtractInput {
            url: fixture.url("127.0.0.1", "/js/noscript"),
            schema,
            render: Some(RenderPolicy::Always),
            use_cache: None,
            max_field_chars: None,
            max_chars: None,
        })
        .await
        .expect("rendered extraction");
    assert_eq!(rendered.record_count, 1);
    assert_eq!(rendered.render_mode, "rendered");
    assert_eq!(rendered.records[0]["title"], "Rendered Article");
    assert!(
        rendered.records[0]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("injectedword")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_heuristic_renders_three_of_five_fixtures() {
    let Some(chrome) = common::skip_without_chrome("auto_heuristic_renders_three_of_five_fixtures")
    else {
        return;
    };
    common::init_test_tracing();
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let urls = [
        fixture.url("127.0.0.1", "/js/noscript"),
        fixture.url("127.0.0.1", "/js/async"),
        fixture.url("127.0.0.1", "/js/four-scripts"),
        fixture.url("127.0.0.1", "/article"),
        fixture.url("127.0.0.1", "/extract/items"),
    ];
    let mut modes = Vec::new();
    for url in urls {
        modes.push(
            fetch(&env, url.clone(), RenderPolicy::Auto)
                .await
                .expect("auto fetch")
                .payload
                .render_mode,
        );
    }
    assert_eq!(
        modes.iter().filter(|mode| *mode == "rendered").count(),
        3,
        "modes: {modes:?}"
    );
    assert_eq!(
        modes.iter().filter(|mode| *mode == "static").count(),
        2,
        "modes: {modes:?}"
    );
    assert_eq!(modes[0], "rendered");
    assert_eq!(modes[1], "rendered");
    assert_eq!(modes[2], "rendered");
    assert_eq!(modes[3], "static");
    assert_eq!(modes[4], "static");
    println!("AUTO_TRANSCRIPT fixtures=5 modes={modes:?}");
}

/// rider (from the disposition): `restart_max = 0` means "no
/// relaunches", not "rendering unavailable". Before the fix the budget was
/// consulted before the first launch, so this configuration failed every render
/// with `render_unavailable`. The relaunch half of the rule is unit-tested on
/// `LaunchGate` (`the_first_launch_is_allowed_even_with_a_zero_budget`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_restart_budget_still_renders_the_first_page() {
    let Some(chrome) =
        common::skip_without_chrome("zero_restart_budget_still_renders_the_first_page")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.render.restart_max = 0;
    let env = common::TestEnv::new_rendering(config).await;

    let rendered = fetch(
        &env,
        fixture.url("127.0.0.1", "/js/noscript"),
        RenderPolicy::Always,
    )
    .await
    .expect("the first launch must work with restart_max = 0");
    assert!(
        rendered.payload.render_mode.starts_with("rendered"),
        "expected a rendered result, got {}",
        rendered.payload.render_mode
    );
    assert!(
        rendered
            .markdown
            .to_ascii_lowercase()
            .contains("injectedword"),
        "the rendered page must contain the script-injected content"
    );
}
