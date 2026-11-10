//! B03 — `run-due --http` (): a cron client that checks due
//! monitors beside the running server, prints one JSON line, and never opens
//! the data directory.
//!
//! Only built with the `http` feature (the deployed feature set).

#![cfg(feature = "http")]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

fn free_port() -> u16 {
 let listener = TcpListener::bind("127.0.0.1:0").unwrap();
 listener.local_addr().unwrap().port()
}

fn spawn_server(
 config: &std::path::Path,
 data: &std::path::Path,
 port: u16,
 token: &str) -> std::process::Child {
 Command::new(env!("CARGO_BIN_EXE_web-widow"))
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

fn runtime() -> tokio::runtime::Runtime {
 tokio::runtime::Builder::new_current_thread()
.enable_all()
.build()
.unwrap()
}

fn wait_ready(port: u16) {
 let url = format!("http://127.0.0.1:{port}/mcp");
 let deadline = std::time::Instant::now() + Duration::from_secs(10);
 loop {
 let ready = runtime().block_on(async {
 reqwest::Client::new()
.post(&url)
.header("content-type", "application/json")
.body("{}")
.send()
.await
.is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED)
 });
 if ready {
 return;
 }
 assert!(
 std::time::Instant::now() < deadline,
 "HTTP server did not become ready"
);
 std::thread::sleep(Duration::from_millis(25));
 }
}

fn run_due(
 config: &std::path::Path,
 data: &std::path::Path,
 args: &[&str],
 token: Option<&str>) -> Output {
 let mut command = Command::new(env!("CARGO_BIN_EXE_web-widow"));
 command
.arg("--config")
.arg(config)
.arg("--data-dir")
.arg(data)
.args(args)
.stdin(Stdio::null());
 match token {
 Some(token) => {
 command.env("WEB_WIDOW_HTTP_TOKEN", token);
 }
 None => {
 command.env_remove("WEB_WIDOW_HTTP_TOKEN");
 }
 }
 command.output().expect("run-due runs")
}

fn one_json_line(output: &Output) -> Value {
 let stdout = String::from_utf8_lossy(&output.stdout);
 assert_eq!(
 stdout.trim().lines().count(),
 1,
 "stdout must be one JSON line: {stdout:?}"
);
 serde_json::from_str(stdout.trim()).expect("stdout is JSON")
}

/// Minimal JSON-RPC POST helper for seeding a monitor through the server.
fn mcp_post(url: &str, token: &str, body: Value) -> Value {
 let text = runtime().block_on(async {
 let response = reqwest::Client::new()
.post(url)
.header("content-type", "application/json")
.header("accept", "application/json, text/event-stream")
.bearer_auth(token)
.json(&body)
.send()
.await
.unwrap();
 response.text().await.unwrap()
 });
 for line in text.lines().rev() {
 if let Some(data) = line.strip_prefix("data:")
 && let Ok(value) = serde_json::from_str(data.trim())
 {
 return value;
 }
 }
 serde_json::from_str(&text).unwrap_or(Value::Null)
}

#[test]
fn the_http_mode_checks_beside_the_lock_and_prints_one_line() {
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "[monitor]\nmax_wait_ms = 1000\n");
 let data = dir.path().join("data");
 let token = "b03-run-due-token";
 let port = free_port();
 let mut child = spawn_server(&config, &data, port, token);
 wait_ready(port);
 let url = format!("http://127.0.0.1:{port}/mcp");

 // With no monitors the pass completes inline and prints one line.
 let output = run_due(
 &config,
 &data,
 &["run-due", "--http", &url, "--wait-ms", "1000"],
 Some(token));
 assert!(
 output.status.success(),
 "stderr: {}",
 String::from_utf8_lossy(&output.stderr)
);
 let payload = one_json_line(&output);
 assert_eq!(payload["via"], json!("http"));
 assert_eq!(payload["url"], json!(url));
 assert_eq!(payload["pending"], json!(false));
 assert_eq!(payload["checked"], json!(0));

 // The local mode in the same data directory is still refused by the lock.
 let local = run_due(&config, &data, &["run-due"], Some(token));
 assert!(
 !local.status.success(),
 "local run-due must not take the held lock"
);

 // Seed one monitor through the server, then ask for an immediate pass: the
 // honest answer is pending + job_id, still exit 0.
 let initialized = mcp_post(
 &url,
 token,
 json!({
 "jsonrpc": "2.0",
 "id": 1,
 "method": "initialize",
 "params": {"protocolVersion": "2025-11-25", "capabilities": {},
 "clientInfo": {"name": "b03-seed", "version": "1"}}
 }));
 assert!(
 initialized["result"]["serverInfo"].is_object(),
 "{initialized}"
);
 let created = mcp_post(
 &url,
 token,
 json!({
 "jsonrpc": "2.0",
 "id": 2,
 "method": "tools/call",
 "params": {"name": "monitor_create", "arguments": {
 "url": format!("http://127.0.0.1:{}/article", port),
 "interval_s": 3600
 }}
 }));
 assert!(
 created["result"]["structuredContent"]["monitor_id"]
.as_i64()
.is_some(),
 "{created}"
);

 let queued = run_due(
 &config,
 &data,
 &["run-due", "--http", &url, "--wait-ms", "0"],
 Some(token));
 assert!(
 queued.status.success(),
 "stderr: {}",
 String::from_utf8_lossy(&queued.stderr)
);
 let payload = one_json_line(&queued);
 assert_eq!(payload["pending"], json!(true), "{payload}");
 assert!(payload["job_id"].as_str().is_some(), "{payload}");

 let _ = child.kill();
 let _ = child.wait();
}

#[test]
fn token_failures_exit_non_zero_before_any_request() {
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "");
 let data = dir.path().join("data");

 // No token at all: the error names the token, not the transport, so no
 // request could have been attempted.
 let missing = run_due(
 &config,
 &data,
 &["run-due", "--http", "http://127.0.0.1:9/mcp"],
 None);
 assert!(!missing.status.success());
 let stderr = String::from_utf8_lossy(&missing.stderr);
 assert!(stderr.to_lowercase().contains("token"), "{stderr}");

 // An empty 0600 token file is rejected the same way.
 let token_file = dir.path().join("http-token");
 std::fs::write(&token_file, "").unwrap();
 use std::os::unix::fs::PermissionsExt;
 std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
 let empty = run_due(
 &config,
 &data,
 &[
 "run-due",
 "--http",
 "http://127.0.0.1:9/mcp",
 "--http-token-file",
 token_file.to_str().unwrap(),
 ],
 None);
 assert!(!empty.status.success());
 assert!(
 String::from_utf8_lossy(&empty.stderr)
.to_lowercase()
.contains("empty")
);
}

#[test]
fn limit_cannot_be_combined_with_the_http_mode() {
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "");
 let data = dir.path().join("data");
 let output = run_due(
 &config,
 &data,
 &[
 "run-due",
 "--http",
 "http://127.0.0.1:9/mcp",
 "--limit",
 "5",
 ],
 Some("any-token"));
 assert!(!output.status.success());
 let stderr = String::from_utf8_lossy(&output.stderr);
 assert!(stderr.contains("--limit"), "{stderr}");
 assert!(stderr.contains("--http"), "{stderr}");
 assert!(stderr.contains("local"), "{stderr}");
}

#[test]
fn http_failures_are_non_zero_with_the_status() {
 // A live server rejects a wrong bearer token with 401.
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "");
 let data = dir.path().join("data");
 let port = free_port();
 let mut child = spawn_server(&config, &data, port, "right-token");
 wait_ready(port);
 let url = format!("http://127.0.0.1:{port}/mcp");
 let unauthorized = run_due(
 &config,
 &data,
 &["run-due", "--http", &url],
 Some("wrong-token"));
 assert!(!unauthorized.status.success());
 assert!(String::from_utf8_lossy(&unauthorized.stderr).contains("401"));

 // A 3xx is refused (redirects are disabled so the bearer cannot follow it).
 let listener = TcpListener::bind("127.0.0.1:0").unwrap();
 let redirect_port = listener.local_addr().unwrap().port();
 let redirector = std::thread::spawn(move || {
 if let Ok((mut stream, _)) = listener.accept() {
 let mut buffer = [0u8; 2048];
 let _ = stream.read(&mut buffer);
 let _ = stream.write_all(
 b"HTTP/1.1 302 Found\r\nLocation: http://example.com/\r\nContent-Length: 0\r\n\r\n");
 }
 });
 let redirected = run_due(
 &config,
 &data,
 &[
 "run-due",
 "--http",
 &format!("http://127.0.0.1:{redirect_port}/mcp"),
 ],
 Some("right-token"));
 let stderr = String::from_utf8_lossy(&redirected.stderr);
 assert!(!redirected.status.success(), "{stderr}");
 assert!(stderr.contains("302"), "{stderr}");
 redirector.join().unwrap();

 // An unreachable listener is a transport error, not a hang.
 let closed = free_port();
 let unreachable = run_due(
 &config,
 &data,
 &[
 "run-due",
 "--http",
 &format!("http://127.0.0.1:{closed}/mcp"),
 ],
 Some("right-token"));
 assert!(!unreachable.status.success());

 let _ = child.kill();
 let _ = child.wait();
}

#[test]
fn a_wait_over_the_server_cap_is_a_typed_schema_error() {
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "[monitor]\nmax_wait_ms = 1000\n");
 let data = dir.path().join("data");
 let port = free_port();
 let mut child = spawn_server(&config, &data, port, "cap-token");
 wait_ready(port);
 let url = format!("http://127.0.0.1:{port}/mcp");
 let output = run_due(
 &config,
 &data,
 &["run-due", "--http", &url, "--wait-ms", "5000"],
 Some("cap-token"));
 assert!(!output.status.success());
 let stderr = String::from_utf8_lossy(&output.stderr);
 assert!(stderr.contains("schema_invalid"), "{stderr}");
 let _ = child.kill();
 let _ = child.wait();
}
