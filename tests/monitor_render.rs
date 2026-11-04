//! A render-mode monitor sees script-driven content that static
//! monitoring cannot. Needs Chrome; skips with an explicit marker otherwise.

#![cfg(feature = "render")]

mod common;

use web_widow::extract::Mode;
use web_widow::monitor::{MonitorChangesInput, MonitorCheckInput, MonitorCreateInput};
use web_widow::pipeline::{FetchPageInput, RenderPolicy};

fn js_monitor_page(version: usize) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>JS Monitor</title></head>\
         <body><noscript>This application requires JavaScript to display its content.</noscript>\
         <div id=\"app\">loading</div>\
         <script>document.getElementById('app').innerHTML = \
         '<article><h1>Rendered Monitor</h1><p>' + \
         Array(60).fill('injectedword v{version}').join(' ') + '</p></article>';</script>\
         </body></html>"
    )
}

async fn check(env: &common::TestEnv, monitor_id: i64) -> web_widow::monitor::MonitorCheckPayload {
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .expect("check ran")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_mode_monitor_detects_script_driven_change_static_cannot_see() {
    let Some(chrome) =
        common::skip_without_chrome("render_mode_monitor_detects_script_driven_change")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = fixture.url("127.0.0.1", "/js/monitor");

    fixture.set_page("/js/monitor", &js_monitor_page(1));
    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: url.clone(),
            mode: Some(Mode::Main),
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .expect("monitor created")
        .monitor_id;

    let baseline = check(&env, monitor).await;
    assert!(baseline.changed.is_empty(), "{baseline:?}");
    assert_eq!(baseline.failed.len(), 0, "{baseline:?}");

    // The check rendered: the snapshot text carries the injected content.
    let db = env.db();
    let (_, normalized_hash, render_mode): (i64, String, String) = db
        .query_row(
            "SELECT s.id, s.normalized_hash, p.render_mode FROM snapshots s
             JOIN fetch_events fe ON fe.id = s.fetch_event_id
             JOIN pages p ON p.id = fe.page_id
             WHERE s.monitor_id = ?1 ORDER BY s.id DESC LIMIT 1",
            [monitor],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(
        render_mode.starts_with("rendered"),
        "the check must render, got {render_mode}"
    );
    let text = env
        .store
        .blobs
        .read(&normalized_hash)
        .await
        .unwrap()
        .expect("snapshot text");
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains("injectedword v1"), "{text}");

    fixture.set_page("/js/monitor", &js_monitor_page(2));
    let changed = check(&env, monitor).await;
    assert_eq!(changed.changed.len(), 1, "{changed:?}");
    assert_eq!(changed.changed[0].kind, "substantive", "{changed:?}");

    let history = env
        .monitors
        .changes(MonitorChangesInput {
            monitor_id: monitor,
            limit: None,
            cursor: None,
            include_diff: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(history.changes.len(), 1);
    let diff: &str = history.changes[0].diff.as_deref().unwrap_or_default();
    assert!(diff.contains("injectedword v2"), "{diff}");

    // Counter-proof: the static content is identical across versions, so a
    // static-only monitor could not see this change.
    fixture.set_page("/js/monitor", &js_monitor_page(3));
    let static_v3 = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: url.clone(),
            mode: Some(Mode::Main),
            render: Some(RenderPolicy::Never),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect("static fetch v3");
    fixture.set_page("/js/monitor", &js_monitor_page(4));
    let static_v4 = env
        .pipeline
        .fetch_page(FetchPageInput {
            url,
            mode: Some(Mode::Main),
            render: Some(RenderPolicy::Never),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect("static fetch v4");
    assert_eq!(
        static_v3.markdown, static_v4.markdown,
        "static extraction is identical (the change lives only in the script)"
    );
    assert!(!static_v3.markdown.contains("injectedword"));
}

/// A shell whose static body and ETag never change; only the fetched API value
/// changes. Static monitoring cannot see it; an auto-rendered check must.
fn api_page() -> String {
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>API Monitor</title></head>\
     <body><noscript>This application requires JavaScript to display its content.</noscript>\
     <div id=\"app\">loading</div>\
     <script>fetch('/api/monitor-value').then(r => r.json()).then(d => {\
     document.getElementById('app').textContent = 'API_VALUE: ' + d.value;});</script>\
     </body></html>"
        .to_string()
}

fn latest_snapshot(env: &common::TestEnv, monitor: i64) -> (String, i64, String) {
    env.db()
        .query_row(
            "SELECT s.render_mode, fe.http_status, s.normalized_hash FROM snapshots s
             JOIN fetch_events fe ON fe.id = s.fetch_event_id
             WHERE s.monitor_id = ?1 ORDER BY s.id DESC LIMIT 1",
            [monitor],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rendered_monitor_never_trusts_a_static_304() {
    let Some(chrome) = common::skip_without_chrome("a_rendered_monitor_never_trusts_a_static_304")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = fixture.url("127.0.0.1", "/monitor/api");
    fixture.set_page("/monitor/api", &api_page());
    fixture.set_api_value("one");

    // A prior static fetch stores the origin's static ETag on the page row. This
    // is the condition under which a rendered check can leave a stale static
    // validator behind.
    let static_shell = env
        .pipeline
        .fetch_page(FetchPageInput {
            url: url.clone(),
            mode: Some(Mode::Main),
            render: Some(RenderPolicy::Never),
            freshness_s: None,
            max_chars: None,
            include_links: None,
            include_content: None,
        })
        .await
        .expect("static fetch");
    assert!(!static_shell.markdown.contains("API_VALUE"));
    let static_etag: String = env
        .db()
        .query_row(
            "SELECT etag FROM pages WHERE url_hash = ?1",
            [web_widow::core::url::content_hash(url.as_bytes())],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!static_etag.is_empty());

    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: url.clone(),
            mode: Some(Mode::Main),
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .expect("monitor created")
        .monitor_id;

    let baseline = check(&env, monitor).await;
    assert!(baseline.changed.is_empty(), "{baseline:?}");
    assert_eq!(baseline.failed.len(), 0, "{baseline:?}");
    let (mode, status, normalized_hash) = latest_snapshot(&env, monitor);
    assert_eq!(mode, "rendered", "the baseline check must render");
    assert_eq!(status, 200, "the snapshot points at the rendered response");
    let text = env
        .store
        .blobs
        .read(&normalized_hash)
        .await
        .unwrap()
        .expect("snapshot text");
    let text = String::from_utf8_lossy(&text).replace('\\', "");
    assert!(text.contains("API_VALUE: one"), "{text}");

    // A purely static monitor on a stable page still gets its 304.
    let static_url = fixture.url("127.0.0.1", "/monitor/static304");
    fixture.set_page(
        "/monitor/static304",
        "<html><head><title>Static</title></head><body><main><article>\
         <h1>Static</h1><p>stable static monitoring page</p>\
         </article></main></body></html>",
    );
    let static_monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: static_url,
            mode: Some(Mode::Main),
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .expect("static monitor created")
        .monitor_id;
    check(&env, static_monitor).await;
    let static_second = check(&env, static_monitor).await;
    assert!(static_second.changed.is_empty(), "{static_second:?}");
    let (static_mode, static_status, _) = latest_snapshot(&env, static_monitor);
    assert_eq!(static_mode, "static");
    assert_eq!(static_status, 304, "the static monitor keeps its 304 path");

    // Only the API value changes; the page shell and its ETag are untouched.
    fixture.set_api_value("two");
    let changed = check(&env, monitor).await;
    assert_eq!(
        changed.changed.len(),
        1,
        "the rendered change must not be missed: {changed:?}"
    );
    assert_eq!(changed.changed[0].kind, "substantive", "{changed:?}");
    let (mode, status, _) = latest_snapshot(&env, monitor);
    assert_eq!(mode, "rendered", "check 2 must render, not accept a 304");
    assert_eq!(status, 200, "check 2 must fetch the body");

    let etag_after: String = env
        .db()
        .query_row(
            "SELECT etag FROM pages WHERE url_hash = ?1",
            [web_widow::core::url::content_hash(url.as_bytes())],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        etag_after, static_etag,
        "the static validator really did not change; only the API value did"
    );

    let history = env
        .monitors
        .changes(MonitorChangesInput {
            monitor_id: monitor,
            limit: None,
            cursor: None,
            include_diff: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(history.changes.len(), 1);
    let diff = history.changes[0]
        .diff
        .as_deref()
        .unwrap_or_default()
        .replace('\\', "");
    assert!(diff.contains("API_VALUE: two"), "{diff}");
}
