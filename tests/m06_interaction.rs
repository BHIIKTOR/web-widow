//! M06 B2 — browser interaction primitives (`interact_*`; checks B2-1…B2-10).
//!
//! Lane-local fixture server plus a private probe listener on `127.0.0.2` that
//! is deliberately **outside** the test SSRF allowlist. A real Chrome is
//! required; without one the render tests print `SKIP` and return.

#![cfg(feature = "render")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::io::AsyncReadExt;
use web_widow::core::ErrorCode;
use web_widow::interaction::{
    InteractActInput, InteractCloseInput, InteractOpenInput, InteractOpenPayload,
    InteractStateInput, InteractionManager,
};

const APP: &str = include_str!("fixtures/m06/browser/app.html");
const NEXT: &str = include_str!("fixtures/m06/browser/next.html");
const ASYNC: &str = include_str!("fixtures/m06/browser/async.html");
const FORM: &str = include_str!("fixtures/m06/browser/form.html");
const PRIVATE_LINK: &str = include_str!("fixtures/m06/browser/private-link.html");
const POPUP: &str = include_str!("fixtures/m06/browser/popup.html");
const CONSOLE: &str = include_str!("fixtures/m06/browser/console.html");
const RING: &str = include_str!("fixtures/m06/browser/ring.html");
const TALL: &str = include_str!("fixtures/m06/browser/tall.html");
const STYLE_CSS: &str = include_str!("fixtures/m06/browser/style.css");

#[derive(Clone, Default)]
struct RequestLog {
    entries: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RequestLog {
    fn record(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn count_matching(&self, needle: &str) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.contains(needle))
            .count()
    }
}

#[derive(Clone)]
struct FixtureState {
    log: RequestLog,
    private_port: u16,
}

struct InteractionFixture {
    origin: String,
    log: RequestLog,
    private: PrivateProbe,
    _handle: tokio::task::JoinHandle<()>,
}

struct PrivateProbe {
    port: u16,
    hits: Arc<AtomicUsize>,
    _handle: tokio::task::JoinHandle<()>,
}

impl PrivateProbe {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.hits.store(0, Ordering::SeqCst);
    }
}

async fn start_private_probe() -> PrivateProbe {
    let listener = tokio::net::TcpListener::bind("127.0.0.2:0")
        .await
        .expect("bind 127.0.0.2 probe");
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                // Count an actual HTTP request, not a speculative preconnect
                // (Chrome may open a socket without sending anything).
                let mut buffer = [0u8; 1024];
                if let Ok(Ok(read)) =
                    tokio::time::timeout(Duration::from_millis(500), socket.read(&mut buffer)).await
                    && read > 0
                    && String::from_utf8_lossy(&buffer[..read]).starts_with("GET ")
                {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    PrivateProbe {
        port,
        hits,
        _handle: handle,
    }
}

fn html(body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    state.log.record(match &query {
        Some(query) => format!("{path}?{query}"),
        None => path.clone(),
    });
    match path.as_str() {
        "/robots.txt" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("User-agent: *\nAllow: /\nDisallow: /private\n"))
            .unwrap(),
        "/app.html" => html(APP.to_string()),
        "/next.html" => html(NEXT.to_string()),
        "/async.html" => html(ASYNC.to_string()),
        "/form.html" => html(FORM.to_string()),
        "/gate.html" => {
            html(PRIVATE_LINK.replace("{{private_port}}", &state.private_port.to_string()))
        }
        "/popup.html" => html(POPUP.to_string()),
        "/console.html" => html(CONSOLE.to_string()),
        "/ring.html" => html(RING.to_string()),
        "/tall.html" => html(TALL.to_string()),
        "/echo" => html("<html><body><h1>Echo</h1></body></html>".to_string()),
        "/scroll" => StatusCode::NO_CONTENT.into_response(),
        "/api/slow" => {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ok":true}"#))
                .unwrap()
        }
        "/api/data" => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"value":"one"}"#))
            .unwrap(),
        "/asset/style.css" => Response::builder()
            .header(header::CONTENT_TYPE, "text/css")
            .body(Body::from(STYLE_CSS.to_string()))
            .unwrap(),
        "/private/denied" => {
            html("<html><body><p>must not be reached</p></body></html>".to_string())
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn start_fixture() -> InteractionFixture {
    let private = start_private_probe().await;
    let log = RequestLog::default();
    let state = FixtureState {
        log: log.clone(),
        private_port: private.port,
    };
    let app = Router::new().fallback(any(handle)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    InteractionFixture {
        origin: format!("http://127.0.0.1:{port}"),
        log,
        private,
        _handle: handle,
    }
}

struct SessionEnv {
    env: common::TestEnv,
    manager: Arc<InteractionManager>,
    fixture: InteractionFixture,
}

async fn setup(chrome: &std::path::Path, ttl_s: u64, max_actions: u32) -> SessionEnv {
    let fixture = start_fixture().await;
    let mut config = common::rendering_test_config(chrome);
    // Only 127.0.0.1 is allowlisted: the 127.0.0.2 probe is a blocked private
    // address, exactly as B2-7 needs.
    config.fetch.allow_hosts = vec!["127.0.0.1/32".to_string()];
    config.fetch.min_interval_ms = 5;
    config.fetch.jitter_ms = 0;
    config.render.settle_ms = 250;
    config.interaction.ttl_s = ttl_s;
    config.interaction.max_actions = max_actions;
    let env = common::TestEnv::new_rendering(config).await;
    let manager = Arc::new(InteractionManager::new(
        Arc::clone(&env.config),
        Arc::clone(&env.pipeline),
    ));
    SessionEnv {
        env,
        manager,
        fixture,
    }
}

impl SessionEnv {
    async fn open(&self, path: &str) -> InteractOpenPayload {
        self.manager
            .open(InteractOpenInput {
                url: format!("{}{path}", self.fixture.origin),
                render: None,
                viewport_width: None,
                viewport_height: None,
            })
            .await
            .expect("session open")
    }

    async fn act(
        &self,
        session: &str,
        action: serde_json::Value,
    ) -> Result<web_widow::interaction::InteractActPayload, web_widow::WidowError> {
        self.manager
            .act(InteractActInput {
                session_id: session.to_string(),
                action,
            })
            .await
    }

    async fn state(
        &self,
        session: &str,
        screenshot: bool,
        console_limit: Option<usize>,
    ) -> Result<web_widow::interaction::InteractStatePayload, web_widow::WidowError> {
        self.manager
            .state(InteractStateInput {
                session_id: session.to_string(),
                screenshot: Some(screenshot),
                console_limit,
            })
            .await
    }

    async fn close(&self, session: &str) -> web_widow::interaction::InteractClosePayload {
        self.manager
            .close(InteractCloseInput {
                session_id: session.to_string(),
            })
            .await
            .expect("session close")
    }

    fn db(&self) -> rusqlite::Connection {
        self.env.db()
    }
}

async fn wait_for_scroll(
    fixture: &InteractionFixture,
    predicate: impl Fn(&[(i64, i64)]) -> bool,
) -> Vec<(i64, i64)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let samples: Vec<(i64, i64)> = fixture
            .log
            .entries()
            .iter()
            .filter_map(|entry| {
                let rest = entry.strip_prefix("/scroll?")?;
                let mut y = None;
                let mut top = None;
                for pair in rest.split('&') {
                    let (name, value) = pair.split_once('=')?;
                    match name {
                        "y" => y = value.parse().ok(),
                        "top" => top = value.parse().ok(),
                        _ => {}
                    }
                }
                Some((y?, top?))
            })
            .collect();
        if predicate(&samples) {
            return samples;
        }
        if Instant::now() >= deadline {
            return samples;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn blob_bytes(env: &common::TestEnv, hash: &str) -> Vec<u8> {
    let path = env.store.blobs.path_for(hash).expect("valid hash");
    std::fs::read(path).expect("blob bytes")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_navigates_and_a_css_only_control_is_clickable() {
    let Some(chrome) =
        common::skip_without_chrome("click_navigates_and_a_css_only_control_is_clickable")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    assert_eq!(opened.status, 200);
    assert_eq!(opened.actions_remaining, 60);

    // `#styled` is hidden by the inline style and shown only by the linked
    // stylesheet the visual profile loads: a clickable point proves it applied.
    let styled = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#styled"}),
        )
        .await
        .expect("the CSS-only control is clickable");
    assert!(styled.ok);

    let toggle = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#toggle"}),
        )
        .await
        .expect("toggle click");
    assert!(toggle.ok);
    assert!(
        toggle
            .console_tail
            .iter()
            .any(|entry| entry.text.contains("panel toggled")),
        "{:?}",
        toggle.console_tail
    );

    let nav = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#next"}),
        )
        .await
        .expect("navigation click");
    assert!(nav.final_url.ends_with("/next.html"), "{}", nav.final_url);
    assert_eq!(nav.title.as_deref(), Some("M06 next fixture"));
    assert!(nav.console_tail.iter().all(|entry| entry.text.len() <= 500));
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn type_submits_and_returns_only_the_character_count() {
    let Some(chrome) =
        common::skip_without_chrome("type_submits_and_returns_only_the_character_count")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/form.html").await;
    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "type", "selector": "#q", "text": "first-value"}),
    )
    .await
    .expect("type the value to clear");
    let secret = "m06-typed-secret-42";
    let typed = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "type", "selector": "#q", "text": secret}),
        )
        .await
        .expect("type");
    assert_eq!(typed.typed_chars, Some(secret.chars().count()));
    let json = serde_json::to_string(&typed).unwrap();
    assert!(
        !json.contains(secret),
        "typed text leaked into the payload: {json}"
    );
    assert!(!json.to_ascii_lowercase().contains("script"));

    let submit = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#send"}),
        )
        .await
        .expect("submit click");
    assert!(
        submit.final_url.contains("/echo"),
        "form submit did not navigate: {}",
        submit.final_url
    );
    let echo = case
        .fixture
        .log
        .entries()
        .into_iter()
        .find(|entry| entry.starts_with("/echo?"))
        .expect("the fixture received the submitted value");
    assert!(echo.contains(secret), "{echo}");
    assert!(
        !echo.contains("first-value"),
        "clear must replace the old value: {echo}"
    );
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_selector_ms_and_network_quiet_and_times_out() {
    let Some(chrome) =
        common::skip_without_chrome("waits_for_selector_ms_and_network_quiet_and_times_out")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/async.html").await;

    let selector = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "wait", "selector": "#late", "timeout_ms": 3000}),
        )
        .await
        .expect("selector presence wait");
    assert!(selector.ok);

    let ms = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "wait", "ms": 200, "timeout_ms": 2000}),
        )
        .await
        .expect("ms wait");
    assert!(ms.ok);

    // Typing triggers an input handler that starts a 400 ms XHR; `type` does
    // not settle, so the quiet wait must see the in-flight request.
    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "type", "selector": "#q", "text": "a"}),
    )
    .await
    .expect("type starts the XHR");
    let started = Instant::now();
    let quiet = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "wait", "network_quiet_ms": 500, "timeout_ms": 5000}),
        )
        .await
        .expect("network-quiet wait");
    assert!(quiet.ok);
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "network quiet returned too early: {:?}",
        started.elapsed()
    );

    let timeout = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "wait", "selector": "#never", "timeout_ms": 300}),
        )
        .await
        .expect_err("a missing selector must time out");
    assert_eq!(timeout.code(), ErrorCode::ActionFailed);
    assert!(timeout.to_string().contains("timeout"), "{timeout}");
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrolls_to_an_element_top_and_bottom() {
    let Some(chrome) = common::skip_without_chrome("scrolls_to_an_element_top_and_bottom") else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/tall.html").await;

    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "scroll", "to": "bottom"}),
    )
    .await
    .expect("scroll bottom");
    let samples = wait_for_scroll(&case.fixture, |samples| {
        samples.last().is_some_and(|(y, _)| *y > 1_000)
    })
    .await;
    assert!(
        samples.last().is_some_and(|(y, _)| *y > 1_000),
        "bottom scroll: {samples:?}"
    );

    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "scroll", "to": "top"}),
    )
    .await
    .expect("scroll top");
    let samples = wait_for_scroll(&case.fixture, |samples| {
        samples.last().is_some_and(|(y, _)| *y == 0)
    })
    .await;
    assert_eq!(samples.last().map(|(y, _)| *y), Some(0), "{samples:?}");

    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "scroll", "to": "element", "selector": "#middle"}),
    )
    .await
    .expect("scroll element");
    let samples = wait_for_scroll(&case.fixture, |samples| {
        samples
            .last()
            .is_some_and(|(y, top)| *y > 500 && (-80..=900).contains(top))
    })
    .await;
    assert!(
        samples
            .last()
            .is_some_and(|(y, top)| *y > 500 && (-80..=900).contains(top)),
        "element scroll: {samples:?}"
    );
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captures_console_levels_the_ring_and_long_text() {
    let Some(chrome) =
        common::skip_without_chrome("captures_console_levels_the_ring_and_long_text")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/console.html").await;
    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "wait", "ms": 700}),
    )
    .await
    .expect("let late console events land");
    let state = case
        .state(&opened.session_id, false, Some(200))
        .await
        .expect("state");
    let text = |needle: &str| {
        state
            .console
            .iter()
            .find(|entry| entry.text.contains(needle))
            .unwrap_or_else(|| panic!("missing console entry {needle}: {:?}", state.console))
    };
    assert_eq!(text("APP_CONSOLE_ERROR").level, "error");
    assert!(
        text("APP_CONSOLE_ERROR").at.starts_with("20"),
        "event timestamps must be real dates: {}",
        text("APP_CONSOLE_ERROR").at
    );
    assert_eq!(text("APP_UNCAUGHT").level, "error");
    assert!(
        state
            .console
            .iter()
            .any(|entry| entry.source == "log" && entry.level == "error"),
        "a failed network request should appear as a log error: {:?}",
        state.console
    );
    let long = text("LLLL");
    assert_eq!(
        long.text.chars().count(),
        500,
        "long args are capped at 500"
    );
    let ctrl = text("CTRL:");
    assert!(
        ctrl.text.chars().all(|character| !character.is_control()),
        "control characters survived console capture: {:?}",
        ctrl.text
    );
    println!("N8 console_ctrl_text={:?}", ctrl.text);
    assert!(
        state
            .console
            .iter()
            .all(|entry| entry.text.chars().count() <= 500)
    );
    case.close(&opened.session_id).await;

    let opened = case.open("/ring.html").await;
    let state = case
        .state(&opened.session_id, false, Some(200))
        .await
        .expect("state");
    assert_eq!(state.console.len(), 200, "ring cap");
    assert!(
        !state
            .console
            .iter()
            .any(|entry| entry.text.contains("RING_0"))
    );
    assert!(
        state
            .console
            .iter()
            .any(|entry| entry.text.contains("RING_209"))
    );
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttl_cap_unknown_id_and_busy_are_typed() {
    let Some(chrome) = common::skip_without_chrome("ttl_cap_unknown_id_and_busy_are_typed") else {
        return;
    };
    let case = setup(&chrome, 1, 2).await;

    let opened = case.open("/app.html").await;
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let expired = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "scroll", "to": "top"}),
        )
        .await
        .expect_err("idle TTL expired");
    assert_eq!(expired.code(), ErrorCode::SessionExpired);

    let opened = case.open("/app.html").await;
    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "scroll", "to": "top"}),
    )
    .await
    .expect("first action");
    case.act(
        &opened.session_id,
        serde_json::json!({"kind": "scroll", "to": "top"}),
    )
    .await
    .expect("second action");
    let capped = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "scroll", "to": "top"}),
        )
        .await
        .expect_err("action cap");
    assert_eq!(capped.code(), ErrorCode::LimitExceeded);
    case.close(&opened.session_id).await;

    let unknown = case
        .act(
            "00000000-0000-7000-8000-000000000000",
            serde_json::json!({"kind": "scroll", "to": "top"}),
        )
        .await
        .expect_err("unknown id");
    assert_eq!(unknown.code(), ErrorCode::SessionNotFound);

    // Concurrent actions: one long wait holds the session; the second action
    // must be refused with session_busy instead of queueing.
    let opened = case.open("/app.html").await;
    let manager = Arc::clone(&case.manager);
    let id = opened.session_id.clone();
    let first = tokio::spawn(async move {
        manager
            .act(InteractActInput {
                session_id: id,
                action: serde_json::json!({"kind": "wait", "ms": 1500}),
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "scroll", "to": "top"}),
        )
        .await
        .expect_err("concurrent action");
    assert_eq!(busy.code(), ErrorCode::SessionBusy);
    first.await.expect("join").expect("the long wait finishes");
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_gate_blocks_private_robots_and_file_requests() {
    let Some(chrome) =
        common::skip_without_chrome("the_session_gate_blocks_private_robots_and_file_requests")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let url = format!("{}/gate.html", case.fixture.origin);

    // RED: the plain render path does not SSRF-vet subresources (RISK-004), so
    // the page's own fetch reaches the private listener.
    let parsed = url::Url::parse(&url).unwrap();
    let _ = case
        .env
        .renderer
        .render(&parsed)
        .await
        .expect("render red probe");
    let deadline = Instant::now() + Duration::from_secs(3);
    while case.fixture.private.hits() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let red_hits = case.fixture.private.hits();
    assert!(
        red_hits > 0,
        "the red probe must reach the private listener; it did not"
    );
    println!("SAFETY red_render_path_private_requests={red_hits}");

    // GREEN: the same page inside a session; every request is vetted first.
    case.fixture.private.reset();
    let opened = case.open("/gate.html").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        case.fixture.private.hits(),
        0,
        "the session gate must fail the private subresource before it leaves the browser"
    );
    println!(
        "SAFETY green_session_private_requests={} after_open_gate_held",
        case.fixture.private.hits()
    );

    // A private document navigation is refused. Chrome commits its error page
    // for a failed navigation, so the safety claim is the refusal (and zero
    // private HTTP requests); the next case reopens a fresh session.
    let private = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#private"}),
        )
        .await
        .expect("private click returns a typed result");
    let blocked = private
        .blocked
        .as_ref()
        .expect("private navigation is blocked");
    assert_eq!(blocked.reason, "blocked_private_address");
    assert_eq!(case.fixture.private.hits(), 0, "no private HTTP request");
    println!(
        "SAFETY private_click blocked={} detail={:?} private_requests={}",
        blocked.reason,
        blocked.detail,
        case.fixture.private.hits()
    );
    case.close(&opened.session_id).await;

    // A robots-disallowed document is refused mid-session.
    let opened = case.open("/gate.html").await;
    let robots = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#robots"}),
        )
        .await
        .expect("robots click returns a typed result");
    let blocked = robots.blocked.as_ref().expect("robots document is blocked");
    assert_eq!(blocked.reason, "blocked_by_robots");
    assert_eq!(case.fixture.log.count_matching("/private/denied"), 0);
    println!(
        "SAFETY robots_click blocked={} detail={:?} denied_requests_on_server={}",
        blocked.reason,
        blocked.detail,
        case.fixture.log.count_matching("/private/denied")
    );
    case.close(&opened.session_id).await;

    // `file:` never leaves the browser and the page stays on the same document.
    let opened = case.open("/gate.html").await;
    let before = case
        .state(&opened.session_id, false, None)
        .await
        .expect("state");
    let file = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#file"}),
        )
        .await
        .expect("file click returns a typed result");
    if let Some(blocked) = file.blocked.as_ref() {
        assert_eq!(blocked.reason, "blocked_scheme");
        println!(
            "SAFETY file_click blocked={} detail={:?}",
            blocked.reason, blocked.detail
        );
    } else {
        println!("SAFETY file_click refused_before_fetch (page unchanged)");
    }
    let after = case
        .state(&opened.session_id, false, None)
        .await
        .expect("state");
    assert_eq!(before.final_url, after.final_url);
    assert_eq!(case.fixture.private.hits(), 0);
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_popup_click_does_not_change_the_session_page() {
    let Some(chrome) =
        common::skip_without_chrome("a_popup_click_does_not_change_the_session_page")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/popup.html").await;
    assert!(opened.final_url.ends_with("/popup.html"));
    let clicked = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "click", "selector": "#popup"}),
        )
        .await
        .expect("popup click");
    assert!(
        clicked.final_url.ends_with("/popup.html"),
        "{}",
        clicked.final_url
    );
    if let Some(popup) = clicked.popup.as_deref() {
        assert_eq!(popup, "not_followed");
    }
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_action_set_rejects_caller_js_and_text_targeting() {
    let Some(chrome) =
        common::skip_without_chrome("the_action_set_rejects_caller_js_and_text_targeting")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    for bad in [
        serde_json::json!({"kind": "eval", "code": "alert(1)"}),
        serde_json::json!({"kind": "click", "selector": "#toggle", "script": "alert(1)"}),
        serde_json::json!({"kind": "click", "text": "Toggle"}),
        serde_json::json!({"kind": "wait", "ms": 10, "selector": "#toggle"}),
    ] {
        let error = case
            .act(&opened.session_id, bad.clone())
            .await
            .expect_err("closed action set");
        assert_eq!(error.code(), ErrorCode::SchemaInvalid, "{bad}");
    }
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_screenshot_reuses_the_b1_capture_path() {
    let Some(chrome) = common::skip_without_chrome("a_state_screenshot_reuses_the_b1_capture_path")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    let state = case
        .state(&opened.session_id, true, Some(10))
        .await
        .expect("state");
    let shot = state.screenshot.clone().expect("screenshot requested");
    assert_eq!(shot.mime, "image/png");
    assert!(shot.bytes > 0);
    let png = blob_bytes(&case.env, &shot.hash);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(shot.hash, web_widow::core::url::content_hash(&png));
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("base64"), "no base64 in the payload");

    let (source, hash): (String, String) = case
        .db()
        .query_row(
            "SELECT source, hash FROM screenshots ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(source, "interaction");
    assert_eq!(hash, shot.hash);

    let blob = case
        .env
        .pipeline
        .blob_get(web_widow::pipeline::BlobGetInput {
            hash: shot.hash.clone(),
        })
        .await
        .expect("blob_get for the interaction screenshot");
    assert!(
        blob.referenced_by
            .iter()
            .any(|r| r.starts_with("screenshot:"))
    );
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_visual_caps_refuse_further_requests() {
    let Some(chrome) = common::skip_without_chrome("session_visual_caps_refuse_further_requests")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.fetch.allow_hosts = vec!["127.0.0.1/32".to_string()];
    config.fetch.min_interval_ms = 5;
    config.fetch.jitter_ms = 0;
    config.render.settle_ms = 200;
    config.render.visual_max_requests = 1;
    let env = common::TestEnv::new_rendering(config).await;
    let manager = Arc::new(InteractionManager::new(
        Arc::clone(&env.config),
        Arc::clone(&env.pipeline),
    ));
    let opened = manager
        .open(InteractOpenInput {
            url: format!("{}/async.html", fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect("open with a one-request cap");
    manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "type", "selector": "#q", "text": "a"}),
        })
        .await
        .expect("type");
    let wait = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "wait", "ms": 300}),
        })
        .await
        .expect("wait");
    let blocked = wait.blocked.as_ref().expect("the cap refusal is reported");
    assert_eq!(blocked.reason, "visual_max_requests");
    manager
        .close(InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_is_idempotent_and_an_unknown_id_is_not_found() {
    let Some(chrome) =
        common::skip_without_chrome("close_is_idempotent_and_an_unknown_id_is_not_found")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    let closed = case.close(&opened.session_id).await;
    assert!(closed.closed);
    assert!(!closed.already_closed);
    let again = case.close(&opened.session_id).await;
    assert!(again.closed);
    assert!(again.already_closed);
    let unknown = case
        .manager
        .close(InteractCloseInput {
            session_id: "00000000-0000-7000-8000-000000000000".to_string(),
        })
        .await
        .expect_err("unknown close id");
    assert_eq!(unknown.code(), ErrorCode::SessionNotFound);
    let missing = case
        .act(
            &opened.session_id,
            serde_json::json!({"kind": "scroll", "to": "top"}),
        )
        .await
        .expect_err("act after close");
    assert_eq!(missing.code(), ErrorCode::SessionNotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_busy_never_hands_out_the_live_session_id() {
    let Some(chrome) =
        common::skip_without_chrome("session_busy_never_hands_out_the_live_session_id")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    let error = case
        .manager
        .open(InteractOpenInput {
            url: format!("{}/app.html", case.fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect_err("second open while live");
    assert_eq!(error.code(), ErrorCode::SessionBusy);
    let tool = error.to_tool_error();
    let detail = tool.detail.unwrap_or_default();
    let value = detail.strip_prefix("session_id=").unwrap_or_default();
    assert_eq!(value.len(), 8, "a redacted prefix only: {detail}");
    assert!(
        !tool.message.contains(&opened.session_id),
        "{}",
        tool.message
    );
    assert!(!tool.message.contains('-'), "{}", tool.message);
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_state_screenshots_count_against_a_session_budget() {
    let Some(chrome) =
        common::skip_without_chrome("interact_state_screenshots_count_against_a_session_budget")
    else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    for index in 0..10 {
        case.state(&opened.session_id, true, Some(10))
            .await
            .unwrap_or_else(|error| panic!("screenshot {index}: {error}"));
    }
    let error = case
        .state(&opened.session_id, true, Some(10))
        .await
        .expect_err("the session screenshot budget is spent");
    assert_eq!(error.code(), ErrorCode::LimitExceeded);
    let rows: i64 = case
        .db()
        .query_row(
            "SELECT COUNT(*) FROM screenshots WHERE source = 'interaction'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 10);
    case.close(&opened.session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n4_concurrent_open_busy_is_prefix_only() {
    let Some(chrome) = common::skip_without_chrome("n4_concurrent_open_busy_is_prefix_only") else {
        return;
    };
    let case = setup(&chrome, 120, 60).await;
    let opened = case.open("/app.html").await;
    let manager = Arc::clone(&case.manager);
    let id = opened.session_id.clone();
    let long_action = tokio::spawn(async move {
        manager
            .act(InteractActInput {
                session_id: id,
                action: serde_json::json!({"kind": "wait", "ms": 1_500}),
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let error = case
        .manager
        .open(InteractOpenInput {
            url: format!("{}/app.html", case.fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect_err("open while the slot is held");
    assert_eq!(error.code(), ErrorCode::SessionBusy);
    let detail = error.to_tool_error().detail.unwrap_or_default();
    let value = detail.strip_prefix("session_id=").unwrap_or_default();
    assert_eq!(value.len(), 8, "prefix only: {detail}");
    assert!(value.chars().all(|c| c.is_ascii_hexdigit()), "{detail}");
    assert!(!error.to_string().contains(&opened.session_id));
    long_action.await.expect("join").expect("long action");
    case.close(&opened.session_id).await;
}
