//! v0.8 — local HTTPS on the loopback listener (generated CA).
//!
//! Only built with the `http` feature. The tests spawn the real binary and use
//! a reqwest client that trusts the generated `<data>/tls/ca.pem`; no live
//! site or operator data directory is touched.

#![cfg(feature = "http")]

mod common;

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_config(path: &Path, data: &Path, tls: bool, feed: bool) {
    let text = format!(
        "[fetch]\ncontact = \"tests@example.invalid\"\nallow_hosts = [\"127.0.0.0/8\"]\n\
         [render]\ndiscovery = false\n[events]\nfeed_enabled = {feed}\n\
         [logging]\nlevel = \"warn\"\n[http]\ntls = {tls}\n"
    );
    std::fs::write(path, text).unwrap();
    let _ = data;
}

fn command(config: &Path, data: &Path, port: u16, token: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_web-widow"));
    command
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .env("WEB_WIDOW_HTTP_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

async fn wait_tls_ready(client: &reqwest::Client, base: &str, _token: &str, child: &mut Child) {
    let url = format!("{base}/mcp");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body("{}")
            .send()
            .await;
        if result.is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let mut text = String::new();
            if let Some(stderr) = child.stderr.as_mut() {
                let _ = stderr.read_to_string(&mut text).await;
            }
            panic!("TLS server did not become ready: {text}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_plain_ready(client: &reqwest::Client, base: &str, _token: &str, child: &mut Child) {
    let url = format!("{base}/mcp");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body("{}")
            .send()
            .await;
        if result.is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let mut text = String::new();
            if let Some(stderr) = child.stderr.as_mut() {
                let _ = stderr.read_to_string(&mut text).await;
            }
            panic!("plain server did not become ready: {text}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

type HookState = Arc<Mutex<Vec<Value>>>;

async fn capture_hook(State(state): State<HookState>, Json(body): Json<Value>) -> StatusCode {
    state.lock().unwrap().push(body);
    StatusCode::OK
}

fn write_hook_config(path: &Path, data: &Path, hook_port: u16) {
    write_config(path, data, true, true);
    let mut text = std::fs::read_to_string(path).unwrap();
    text.push_str(&format!(
        "[webhooks]\nenabled = true\ntimeout_ms = 500\nmax_attempts = 3\nallow_http = true\n\
         [webhooks.targets.ops]\nurl = \"http://127.0.0.1:{hook_port}/hook\"\nkind = \"generic\"\n"
    ));
    std::fs::write(path, text).unwrap();
}

async fn rpc(client: &reqwest::Client, base: &str, token: &str, payload: Value) -> Value {
    let response = client
        .post(format!("{base}/mcp"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&payload)
        .send()
        .await
        .expect("HTTPS request");
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.expect("JSON-RPC response")
}

async fn initialize(client: &reqwest::Client, base: &str, token: &str) {
    let reply = rpc(
        client,
        base,
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "m08-tls", "version": "1"}
            }
        }),
    )
    .await;
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let reply = rpc(
        client,
        base,
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    reply["result"]["structuredContent"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_ca_serves_mcp_and_feed_over_https() {
    let fixture = common::start_fixture().await;
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_config(&config, &data, true, true);
    let token = "m08-tls-bearer";
    let port = free_port();
    let mut child: Child = command(&config, &data, port, token).spawn().unwrap();

    let ca_path = data.join("tls").join("ca.pem");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !ca_path.is_file() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "generated CA did not appear"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ca = std::fs::read(&ca_path).unwrap();
    let tls = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("https://127.0.0.1:{port}");
    wait_tls_ready(&tls, &base, token, &mut child).await;

    initialize(&tls, &base, token).await;
    let tools = rpc(
        &tls,
        &base,
        token,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 34);

    let fetched = call(
        &tls,
        &base,
        token,
        "fetch_page",
        json!({"url": fixture.url("127.0.0.1", "/article")}),
    )
    .await;
    assert_eq!(fetched["status"], 200);

    let events = call(&tls, &base, token, "events", json!({"format": "rss"})).await;
    let feed_url = events["feed_url"].as_str().unwrap_or_default();
    assert!(
        feed_url.starts_with(&format!("https://127.0.0.1:{port}/feeds/changes.xml")),
        "{feed_url}"
    );

    // The feed uses its own capability token, not the bearer.
    let feed_token = std::fs::read_to_string(data.join("feed-token")).unwrap();
    let feed_base = format!("{base}/feeds/changes.xml");
    let first = tls
        .get(format!("{feed_base}?token={}", feed_token.trim()))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert!(
        first.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/rss+xml")
    );
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let not_modified = tls
        .get(format!("{feed_base}?token={}", feed_token.trim()))
        .header("If-None-Match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(not_modified.status(), reqwest::StatusCode::NOT_MODIFIED);
    assert_eq!(
        tls.get(format!("{feed_base}?token=wrong"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        tls.get(&feed_base).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    // Plain HTTP to the TLS port must not be served.
    let plain = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let plain_result = plain
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await;
    assert!(
        plain_result.is_err() || !plain_result.unwrap().status().is_success(),
        "plain HTTP must not succeed on the TLS port"
    );

    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_change_webhook_carries_the_https_feed_url() {
    let fixture = common::start_fixture().await;
    fixture.set_page("/monitor/tls-hook", "<html><head><title>TLS hook</title></head><body><main><article><h1>TLS hook</h1><p>first body</p></article></main></body></html>");
    let captured: HookState = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route("/hook", post(capture_hook))
        .with_state(Arc::clone(&captured));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_hook_config(&config, &data, hook_port);
    let token = "m08-tls-hook-bearer";
    let port = free_port();
    let mut child = command(&config, &data, port, token).spawn().unwrap();
    let ca_path = data.join("tls").join("ca.pem");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !ca_path.is_file() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "generated CA did not appear"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ca = std::fs::read(&ca_path).unwrap();
    let tls = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("https://127.0.0.1:{port}");
    wait_tls_ready(&tls, &base, token, &mut child).await;
    initialize(&tls, &base, token).await;
    let created = call(
        &tls,
        &base,
        token,
        "monitor_create",
        json!({
            "url": fixture.url("127.0.0.1", "/monitor/tls-hook"),
            "interval_s": 3600,
            "webhook_targets": ["ops"]
        }),
    )
    .await;
    let monitor_id = created["monitor_id"].clone();
    let baseline = call(
        &tls,
        &base,
        token,
        "monitor_check",
        json!({"monitor_id": monitor_id, "force": true}),
    )
    .await;
    assert_eq!(baseline["changed"].as_array().unwrap().len(), 0);
    fixture.set_page("/monitor/tls-hook", "<html><head><title>TLS hook</title></head><body><main><article><h1>TLS hook</h1><p>second body, changed</p></article></main></body></html>");
    let changed = call(
        &tls,
        &base,
        token,
        "monitor_check",
        json!({"monitor_id": monitor_id, "force": true}),
    )
    .await;
    assert_eq!(changed["changed"].as_array().unwrap().len(), 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(50);
    loop {
        if !captured.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "webhook was not delivered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let payload = captured.lock().unwrap()[0].clone();
    assert_eq!(
        payload["feed_url"].as_str(),
        Some(format!("https://127.0.0.1:{port}/feeds/changes.xml").as_str())
    );
    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_mcp_sdk_client_handshakes_and_lists_34_tools() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_config(&config, &data, true, true);
    let token = "m08-tls-sdk";
    let port = free_port();
    let mut child = command(&config, &data, port, token).spawn().unwrap();
    let ca_path = data.join("tls").join("ca.pem");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !ca_path.is_file() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "generated CA did not appear"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ca = std::fs::read(&ca_path).unwrap();
    let tls = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("https://127.0.0.1:{port}");
    wait_tls_ready(&tls, &base, token, &mut child).await;
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token);
    let transport = StreamableHttpClientTransport::with_client(tls, transport_config);
    let client = ().serve(transport).await.expect("HTTPS MCP initialize handshake");
    let tools = client
        .list_all_tools()
        .await
        .expect("tools/list over HTTPS");
    assert_eq!(tools.len(), 34);
    assert!(tools.iter().any(|tool| tool.name == "fetch_page"));
    client.cancel().await.expect("HTTPS MCP client shutdown");
    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_to_a_plain_port_fails() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_config(&config, &data, false, true);
    let token = "m08-plain-bearer";
    let port = free_port();
    let mut child = command(&config, &data, port, token).spawn().unwrap();
    let plain = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    wait_plain_ready(
        &plain,
        &format!("http://127.0.0.1:{port}"),
        token,
        &mut child,
    )
    .await;

    let ca = std::fs::read(data.join("tls").join("ca.pem")).unwrap_err();
    assert_eq!(
        ca.kind(),
        std::io::ErrorKind::NotFound,
        "plain mode must not create TLS material"
    );

    let tls = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let result = tls
        .post(format!("https://127.0.0.1:{port}/mcp"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await;
    assert!(result.is_err(), "TLS to a plain port must fail");

    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_listener_stops_on_sigterm_within_the_bound() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_config(&config, &data, true, true);
    let token = "m08-tls-shutdown";
    let port = free_port();
    let mut child = command(&config, &data, port, token).spawn().unwrap();
    let ca_path = data.join("tls").join("ca.pem");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !ca_path.is_file() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "generated CA did not appear"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ca = std::fs::read(&ca_path).unwrap();
    let tls = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("https://127.0.0.1:{port}");
    wait_tls_ready(&tls, &base, token, &mut child).await;
    let pid = child.id().expect("child pid");
    let killed = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(killed.success());
    match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
        Ok(Ok(status)) => assert!(status.success(), "TLS server exited {status}"),
        Ok(Err(error)) => panic!("wait failed: {error}"),
        Err(_) => {
            let _ = child.kill().await;
            panic!("TLS server did not stop within 15 s");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_true_without_http_is_a_typed_startup_error() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("config.toml");
    write_config(&config, &data, true, true);
    let output = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tls_requires_http"), "{stderr}");
    assert!(!data.join("tls").join("ca.pem").exists());
}
