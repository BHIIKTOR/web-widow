//! v0.7 integration: config-named targets, atomic enqueue, HMAC delivery,
//! retry/backoff/dead-letter, isolation, tombstoning and history retention.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use chrono::Utc;
use serde_json::{Value, json};
use web_widow::config::{Config, RetentionConfig, WebhookKind, WebhookTargetConfig};
use web_widow::core::error::ErrorCode;
use web_widow::monitor::{MonitorCheckInput, MonitorCreateInput, MonitorDeleteInput};
use web_widow::webhooks::{WebhookService, WebhookStatusInput, WebhookTargetInput};

use common::{TestEnv, start_fixture, test_config};

#[derive(Clone, Copy)]
enum Mode {
    Ok,
    Status(u16),
    Hang,
}

#[derive(Clone, Default)]
struct ReceiverState {
    log: Arc<Mutex<Vec<Captured>>>,
    mode: Arc<Mutex<Option<Mode>>>,
}

#[derive(Clone)]
struct Captured {
    headers: HashMap<String, String>,
    body: String,
}

struct Receiver {
    port: u16,
    state: ReceiverState,
    _handle: tokio::task::JoinHandle<()>,
}

impl Receiver {
    fn set_mode(&self, mode: Mode) {
        *self.state.mode.lock().unwrap() = Some(mode);
    }

    fn captured(&self) -> Vec<Captured> {
        self.state
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

async fn start_receiver(mode: Mode) -> Receiver {
    let state = ReceiverState {
        log: Arc::new(Mutex::new(Vec::new())),
        mode: Arc::new(Mutex::new(Some(mode))),
    };
    let app = Router::new()
        .fallback(any(handle_hook))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    Receiver {
        port,
        state,
        _handle: handle,
    }
}

async fn handle_hook(State(state): State<ReceiverState>, request: Request<Body>) -> Response {
    let headers = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect::<HashMap<_, _>>();
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    state
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Captured {
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        });
    let mode = *state.mode.lock().unwrap();
    match mode {
        Some(Mode::Status(code)) => StatusCode::from_u16(code).unwrap().into_response(),
        Some(Mode::Hang) => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK.into_response()
        }
        _ => StatusCode::OK.into_response(),
    }
}

fn page(text: &str) -> String {
    format!(
        "<html><head><title>Webhook</title></head><body><main><article>\
         <h1>Webhook</h1><p>{text}</p></article></main></body></html>"
    )
}

fn write_secret(dir: &Path, value: &str) -> PathBuf {
    let path = dir.join("webhook-secret");
    std::fs::write(&path, value).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn config_with_target(
    port: u16,
    secret_path: &Path,
    max_attempts: u32,
    kind: WebhookKind,
    include_diff: bool,
) -> Config {
    let mut config = test_config();
    config.webhooks.enabled = true;
    config.webhooks.allow_http = true;
    config.webhooks.timeout_ms = 500;
    config.webhooks.max_attempts = max_attempts;
    config.webhooks.targets.insert(
        "ops".to_string(),
        WebhookTargetConfig {
            url: format!("http://127.0.0.1:{port}/hook"),
            kind,
            secret_file: Some(secret_path.to_path_buf()),
            include_diff,
            ..WebhookTargetConfig::default()
        },
    );
    config
}

async fn create_monitor(env: &TestEnv, url: &str, label: Option<&str>) -> i64 {
    env.monitors
        .create(MonitorCreateInput {
            url: url.to_string(),
            mode: None,
            schema: None,
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: label.map(str::to_string),
        })
        .await
        .unwrap()
        .monitor_id
}

async fn check(env: &TestEnv, monitor_id: i64) -> web_widow::monitor::MonitorCheckPayload {
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .unwrap()
}

fn count(env: &TestEnv, table: &str) -> i64 {
    env.db()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn delivery_status(env: &TestEnv) -> (String, i64, Option<i64>, Option<String>, String) {
    env.db()
        .query_row(
            "SELECT status, attempts, last_status, last_error, next_attempt_at
             FROM webhook_deliveries ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap()
}

fn verify_hmac(captured: &Captured, secret: &str) {
    let timestamp = captured
        .headers
        .get("x-web-widow-timestamp")
        .expect("timestamp header");
    let signature = captured
        .headers
        .get("x-web-widow-signature")
        .expect("signature header");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let message = format!("{timestamp}.{}", captured.body);
    let expected = ring::hmac::sign(&key, message.as_bytes());
    let mut hex = String::new();
    for byte in expected.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    assert_eq!(signature, &format!("sha256={hex}"));
}

fn service(env: &TestEnv) -> WebhookService {
    WebhookService::new(Arc::clone(&env.config), Arc::clone(&env.store), None).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_check_enqueues_and_delivers_one_signed_post() {
    let receiver = start_receiver(Mode::Ok).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let env = TestEnv::new(config_with_target(
        receiver.port,
        &secret,
        3,
        WebhookKind::Generic,
        false,
    ))
    .await;
    let webhooks = service(&env);
    let id = create_monitor(
        &env,
        &fixture.url("127.0.0.1", "/monitor/webhook"),
        Some("ops"),
    )
    .await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();

    assert!(check(&env, id).await.changed.is_empty(), "baseline");
    fixture.set_page("/monitor/webhook", &page("second body, materially changed"));
    assert_eq!(check(&env, id).await.changed.len(), 1, "one change");
    assert_eq!(count(&env, "webhook_deliveries"), 1, "one outbox row");

    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let captured = receiver.captured();
    assert_eq!(captured.len(), 1, "exactly one POST");
    verify_hmac(&captured[0], "s3cret-token");
    let payload: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(payload["event"], "change");
    assert_eq!(payload["monitor"]["id"], id);
    assert_eq!(
        payload["monitor"]["url"],
        fixture.url("127.0.0.1", "/monitor/webhook")
    );
    assert_eq!(payload["change"]["kind"], "substantive");
    assert!(payload.get("diff").is_none(), "no diff by default");
    assert_eq!(
        captured[0].headers.get("x-web-widow-event").unwrap(),
        "change"
    );

    let status = webhooks
        .status(WebhookStatusInput {
            include_deliveries: Some(true),
            limit: Some(20),
        })
        .await
        .unwrap();
    assert_eq!(status.pending, 0);
    assert_eq!(status.dead, 0);
    assert_eq!(status.deliveries[0].status, "delivered");
    assert_eq!(status.deliveries[0].attempts, 1);
    assert!(status.monitors.iter().any(|m| m.monitor_id == id));
    let json = serde_json::to_string(&status).unwrap();
    assert!(!json.contains("s3cret-token"));
    assert!(!json.contains("/hook"), "only the host is returned: {json}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_target_or_disabled_webhooks_produce_zero_egress() {
    let receiver = start_receiver(Mode::Ok).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));

    // Enabled but no attachment.
    let env = TestEnv::new(config_with_target(
        receiver.port,
        &secret,
        3,
        WebhookKind::Generic,
        false,
    ))
    .await;
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(count(&env, "webhook_deliveries"), 0);
    let webhooks = service(&env);
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 0);

    // Attached but disabled.
    let mut config = config_with_target(receiver.port, &secret, 3, WebhookKind::Generic, false);
    config.webhooks.enabled = false;
    let env = TestEnv::new(config).await;
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    let webhooks = service(&env);
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed again"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(count(&env, "webhook_deliveries"), 0);
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 0);
    assert!(receiver.captured().is_empty(), "no request at all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_progression_and_immediate_dead_on_4xx() {
    let receiver = start_receiver(Mode::Status(500)).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let env = TestEnv::new(config_with_target(
        receiver.port,
        &secret,
        2,
        WebhookKind::Generic,
        false,
    ))
    .await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);

    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let (status, attempts, last_status, error, next) = delivery_status(&env);
    assert_eq!(
        (status.as_str(), attempts, last_status),
        ("pending", 1, Some(500))
    );
    assert_eq!(error.as_deref(), Some("http 500"));
    let next: chrono::DateTime<Utc> = chrono::DateTime::parse_from_rfc3339(&next)
        .unwrap()
        .with_timezone(&Utc);
    let delay = (next - Utc::now()).num_seconds();
    assert!(
        (40..=80).contains(&delay),
        "first backoff near 60 s, got {delay}"
    );

    // Force the row due and fail once more: attempts == max_attempts -> dead.
    env.db()
        .execute(
            "UPDATE webhook_deliveries SET next_attempt_at = ?1",
            rusqlite::params!["2000-01-01T00:00:00.000Z"],
        )
        .unwrap();
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let (status, attempts, last_status, _, _) = delivery_status(&env);
    assert_eq!(
        (status.as_str(), attempts, last_status),
        ("dead", 2, Some(500))
    );

    // A 404 is dead after the first attempt (non-retryable 4xx).
    receiver.set_mode(Mode::Status(404));
    fixture.set_page("/monitor/webhook", &page("changed a third time"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let (status, attempts, last_status, error, _) = delivery_status(&env);
    assert_eq!(
        (status.as_str(), attempts, last_status),
        ("dead", 1, Some(404))
    );
    assert_eq!(error.as_deref(), Some("http 404"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_is_retryable_and_never_blocks_the_check() {
    let receiver = start_receiver(Mode::Hang).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let mut config = config_with_target(receiver.port, &secret, 3, WebhookKind::Generic, false);
    config.webhooks.timeout_ms = 300;
    let env = TestEnv::new(config).await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    let started = Instant::now();
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the check never waits for delivery"
    );
    assert!(
        receiver.captured().is_empty(),
        "no delivery on the check path"
    );

    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let (status, attempts, last_status, error, _) = delivery_status(&env);
    assert_eq!(status.as_str(), "pending");
    assert_eq!(attempts, 1);
    assert_eq!(last_status, None);
    assert_eq!(error.as_deref(), Some("timeout"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstoning_dead_letters_pending_deliveries() {
    let receiver = start_receiver(Mode::Ok).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let env = TestEnv::new(config_with_target(
        receiver.port,
        &secret,
        3,
        WebhookKind::Generic,
        false,
    ))
    .await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(count(&env, "webhook_deliveries"), 1);

    env.monitors
        .delete(MonitorDeleteInput { monitor_id: id })
        .await
        .unwrap();
    let (status, _, _, error, _) = delivery_status(&env);
    assert_eq!(status, "dead");
    assert_eq!(error.as_deref(), Some("monitor is inactive"));
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 0);
    assert!(receiver.captured().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivered_history_is_pruned_and_dead_letters_are_kept_longer() {
    let receiver = start_receiver(Mode::Ok).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let env = TestEnv::new(config_with_target(
        receiver.port,
        &secret,
        3,
        WebhookKind::Generic,
        false,
    ))
    .await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    let change_id: i64 = env
        .db()
        .query_row(
            "SELECT id FROM changes ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let old = |days: i64| {
        (Utc::now() - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let connection = env.db();
    for (target, status, created, delivered) in [
        ("old-delivered", "delivered", old(10), Some(old(10))),
        ("old-dead", "dead", old(5), None),
        ("fresh-dead", "dead", old(1), None),
        ("old-pending", "pending", old(10), None),
    ] {
        connection
            .execute(
                "INSERT INTO webhook_deliveries
                     (change_id, monitor_id, target, status, attempts, next_attempt_at,
                      created_at, delivered_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, ?6)",
                rusqlite::params![change_id, id, target, status, created, delivered],
            )
            .unwrap();
    }
    env.store
        .gc(RetentionConfig {
            webhook_deliveries_days: 2,
            webhook_dead_days: 3,
            ..RetentionConfig::default()
        })
        .await
        .unwrap();
    let remaining: Vec<(String,)> = connection
        .prepare("SELECT target FROM webhook_deliveries WHERE target LIKE 'old-%' OR target = 'fresh-dead'")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?,)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let names: Vec<&str> = remaining.iter().map(|(name,)| name.as_str()).collect();
    assert!(names.contains(&"fresh-dead"), "{names:?}");
    assert!(names.contains(&"old-pending"), "pending is never pruned");
    assert!(!names.contains(&"old-delivered"), "delivered aged out");
    assert!(
        !names.contains(&"old-dead"),
        "dead aged out after the longer window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_target_names_are_rejected_and_host_only_status_is_secret_safe() {
    let receiver = start_receiver(Mode::Status(500)).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let mut config = config_with_target(receiver.port, &secret, 3, WebhookKind::Generic, false);
    // A path token is a secret: it must never appear in status or errors.
    config.webhooks.targets.get_mut("ops").unwrap().url =
        format!("http://127.0.0.1:{}/hook/TOPSECRET", receiver.port);
    let env = TestEnv::new(config).await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    let error = webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["not-configured".to_string()],
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert_eq!(count(&env, "monitor_webhooks"), 0);

    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 1);
    let (status, _, last_status, error, _) = delivery_status(&env);
    assert_eq!((status.as_str(), last_status), ("pending", Some(500)));
    assert!(!error.unwrap().contains("TOPSECRET"));
    let status = webhooks
        .status(WebhookStatusInput {
            include_deliveries: Some(true),
            limit: Some(20),
        })
        .await
        .unwrap();
    let json = serde_json::to_string(&status).unwrap();
    assert!(!json.contains("TOPSECRET"), "{json}");
    assert!(!json.contains("s3cret-token"), "{json}");
    assert!(json.contains("127.0.0.1"), "host is reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slack_and_discord_targets_use_their_shapes() {
    let receiver = start_receiver(Mode::Ok).await;
    let dir = tempfile::tempdir().unwrap();
    let secret = write_secret(dir.path(), "s3cret-token");
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/webhook", &page("first body"));
    let mut config = config_with_target(receiver.port, &secret, 3, WebhookKind::Slack, false);
    config.webhooks.targets.insert(
        "chat".to_string(),
        WebhookTargetConfig {
            url: format!("http://127.0.0.1:{}/hook", receiver.port),
            kind: WebhookKind::Discord,
            secret_file: Some(secret.clone()),
            include_diff: false,
            ..WebhookTargetConfig::default()
        },
    );
    let env = TestEnv::new(config).await;
    let webhooks = service(&env);
    let id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/webhook"), None).await;
    webhooks
        .attach(WebhookTargetInput {
            monitor_id: id,
            targets: vec!["ops".to_string(), "chat".to_string()],
        })
        .await
        .unwrap();
    assert!(check(&env, id).await.changed.is_empty());
    fixture.set_page("/monitor/webhook", &page("changed body"));
    assert_eq!(check(&env, id).await.changed.len(), 1);
    assert_eq!(webhooks.flush_due(20).await.unwrap(), 2);
    let captured = receiver.captured();
    assert_eq!(captured.len(), 2);
    let mut saw_slack = false;
    let mut saw_discord = false;
    for post in captured {
        let payload: Value = serde_json::from_str(&post.body).unwrap();
        if payload.get("text").is_some() {
            saw_slack = true;
            assert!(payload["text"].as_str().unwrap().chars().count() <= 3_000);
        }
        if payload.get("content").is_some() {
            saw_discord = true;
            assert!(payload["content"].as_str().unwrap().chars().count() <= 1_900);
        }
    }
    assert!(saw_slack && saw_discord);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_monitor_create_tool_accepts_configured_target_names() {
    let dir = tempfile::tempdir().unwrap();
    let extra = "[webhooks]\nenabled = true\ntimeout_ms = 500\nmax_attempts = 3\n[webhooks.targets.ops]\nurl = \"https://example.com/hook\"\nkind = \"generic\"\n";
    let config = common::write_binary_config(dir.path(), extra);
    let data = dir.path().join("data");
    let transport = rmcp::transport::TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow")).configure(|command| {
            command
                .arg("--config")
                .arg(&config)
                .arg("--data-dir")
                .arg(&data)
                .arg("serve");
        }),
    )
    .expect("client transport starts");
    use rmcp::ServiceExt;
    use rmcp::transport::ConfigureCommandExt;
    let client = ().serve(transport).await.expect("initialize handshake");
    let created = client
        .peer()
        .call_tool(
            rmcp::model::CallToolRequestParams::new("monitor_create".to_string()).with_arguments(
                json!({
                    "url": "https://example.com/",
                    "webhook_targets": ["ops"]
                })
                .as_object()
                .cloned()
                .unwrap(),
            ),
        )
        .await
        .expect("monitor_create");
    assert_eq!(created.is_error, Some(false), "{created:?}");
    let monitor_id = created.structured_content.unwrap()["monitor_id"]
        .as_i64()
        .unwrap();
    let status = client
        .peer()
        .call_tool(
            rmcp::model::CallToolRequestParams::new("webhook_status".to_string()).with_arguments(
                json!({ "include_deliveries": false })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("webhook_status");
    assert_eq!(status.is_error, Some(false), "{status:?}");
    let payload = status.structured_content.unwrap();
    let monitors = payload["monitors"].as_array().unwrap();
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0]["monitor_id"], monitor_id);
    assert_eq!(monitors[0]["targets"], json!(["ops"]));
    assert_eq!(payload["targets"][0]["host"], "example.com");
    assert!(payload["targets"][0].get("url").is_none());
}
