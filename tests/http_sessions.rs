//! transport experiment as a test: six probes plus a 10-call burst and the
//! access-log safety assertion (…).
//!
//! Only built with the `http` feature. The server is a real child process; the
//! requests use explicit headers (not the SDK), so a stale or absent
//! `Mcp-Session-Id` is genuinely exercised.

#![cfg(feature = "http")]

mod common;

use std::process::Stdio;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ALLOW, AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use serde_json::{Value, json};

const SYNTHETIC: &str = "11111111-1111-4111-8111-111111111111";
const STALE: &str = "22222222-2222-4222-8222-222222222222";

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_config(dir: &std::path::Path, level: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let text = format!(
        "[fetch]\ncontact = \"tests@example.invalid\"\nmin_interval_ms = 10\njitter_ms = 0\n\
         [render]\ndiscovery = false\n[logging]\nlevel = \"{level}\"\n"
    );
    std::fs::write(&path, text).unwrap();
    path
}

fn spawn(
    config: &std::path::Path,
    data: &std::path::Path,
    port: u16,
    token: &str,
) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .env("WEB_WIDOW_HTTP_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server starts")
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(character);
        }
    }
    out
}

async fn stop(child: tokio::process::Child) -> std::process::Output {
    let pid = child.id().expect("child pid");
    let status = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill");
    assert!(status.success(), "SIGTERM failed");
    tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("server exits within 10 s")
        .expect("wait")
}

async fn wait_ready(url: &str) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let response = client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await;
        if response.is_ok_and(|response| response.status() == StatusCode::UNAUTHORIZED) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTP server did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    session: Option<&str>,
    payload: &Value,
) -> (StatusCode, HeaderMap, String) {
    let mut request = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .json(payload);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.expect("request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    (status, headers, body)
}

/// Parse a JSON response, or the last data frame of an SSE response.
fn parse(body: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Some(value);
    }
    let mut last = None;
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(data)
            {
                last = Some(value);
            }
        }
    }
    last
}

fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "iss-001-test", "version": "1" }
        }
    })
}

fn call_body(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": "monitor_list", "arguments": { "limit": 5 } }
    })
}

async fn initialize(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> (StatusCode, HeaderMap, Option<Value>) {
    let (status, headers, body) = post(client, url, token, None, &initialize_body()).await;
    (status, headers, parse(&body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stateless_transport_survives_parallel_calls_and_ignores_session_ids() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "info");
    let data = dir.path().join("data");
    let token = "iss-001-test-token";
    let port = free_port();
    let child = spawn(&config, &data, port, token);
    let url = format!("http://127.0.0.1:{port}/mcp");
    wait_ready(&url).await;
    let client = reqwest::Client::new();

    // initialize, then 7 concurrent calls with the returned id (or a
    // synthetic one when stateless mode issues no id).
    let (status, headers, initialize_payload) = initialize(&client, &url, token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(initialize_payload.is_some(), "initialize response");
    let issued = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let e1_session = issued.clone().unwrap_or_else(|| SYNTHETIC.to_string());
    let mut handles = Vec::new();
    for index in 0..7 {
        let client = client.clone();
        let url = url.clone();
        let token = token.to_string();
        let session = e1_session.clone();
        handles.push(tokio::spawn(async move {
            post(
                &client,
                &url,
                &token,
                Some(&session),
                &call_body(10 + index),
            )
            .await
        }));
    }
    for handle in handles {
        let (status, _, body) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(parse(&body).is_some(), "{body}");
    }

    // 7 concurrent initializes, then one call on each id (synthetic when
    // the stateless server issues none).
    let mut handles = Vec::new();
    for _ in 0..7 {
        let client = client.clone();
        let url = url.clone();
        let token = token.to_string();
        handles.push(tokio::spawn(async move {
            initialize(&client, &url, &token).await
        }));
    }
    let mut sessions = Vec::new();
    for handle in handles {
        let (status, headers, payload) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(payload.is_some());
        sessions.push(
            headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .unwrap_or_else(|| SYNTHETIC.to_string()),
        );
    }
    let mut handles = Vec::new();
    for (index, session) in sessions.into_iter().enumerate() {
        let client = client.clone();
        let url = url.clone();
        let token = token.to_string();
        handles.push(tokio::spawn(async move {
            post(
                &client,
                &url,
                &token,
                Some(&session),
                &call_body(20 + index as u64),
            )
            .await
        }));
    }
    for handle in handles {
        let (status, _, body) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    // a well-formed stale id is ignored, not 404.
    let (status, _, body) = post(&client, &url, token, Some(STALE), &call_body(30)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("Session not found"), "{body}");

    // no session header is a normal call.
    let (status, _, body) = post(&client, &url, token, None, &call_body(31)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // DELETE is not served; a call with the ignored id still works.
    let response = client
        .delete(&url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("mcp-session-id", e1_session.clone())
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers()
            .get(ALLOW)
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    let (status, _, body) = post(&client, &url, token, Some(&e1_session), &call_body(32)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // GET is not served; no server-initiated messages.
    let response = client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "text/event-stream")
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers()
            .get(ALLOW)
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );

    // 10-call concurrent burst.
    let mut handles = Vec::new();
    for index in 0..10 {
        let client = client.clone();
        let url = url.clone();
        let token = token.to_string();
        let session = e1_session.clone();
        handles.push(tokio::spawn(async move {
            post(
                &client,
                &url,
                &token,
                Some(&session),
                &call_body(40 + index),
            )
            .await
        }));
    }
    for handle in handles {
        let (status, _, body) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let output = stop(child).await;
    assert!(output.status.success(), "server did not stop cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_log_is_decisive_and_never_contains_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "debug");
    let data = dir.path().join("data");
    let token = "iss-001-access-token-9f3c";
    let port = free_port();
    let child = spawn(&config, &data, port, token);
    let url = format!("http://127.0.0.1:{port}/mcp");
    wait_ready(&url).await;
    let client = reqwest::Client::new();

    // 401 (no token), then a 200 initialize with no session header.
    let _ = client
        .post(&url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body("{}")
        .send()
        .await
        .expect("401 request");
    let (status, _, _) = post(&client, &url, token, None, &initialize_body()).await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let output = stop(child).await;
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
    assert!(
        stderr.contains("http request"),
        "access log line missing: {stderr}"
    );
    assert!(stderr.contains("status=401"), "{stderr}");
    assert!(stderr.contains("status=200"), "{stderr}");
    assert!(stderr.contains("session=none"), "{stderr}");
    assert!(stderr.contains("method=POST"), "{stderr}");
    assert!(stderr.contains("path=/mcp"), "{stderr}");
    assert!(stderr.contains("duration_ms="), "{stderr}");
    assert!(!stderr.contains(token), "the token leaked: {stderr}");
}
