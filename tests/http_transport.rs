//! The packaged release / — the loopback streamable HTTP transport: bearer auth, a real
//! MCP SDK client, stdout hygiene and a clean SIGTERM.
//!
//! Only built/run when the `http` feature is enabled (`cargo test --features http`),
//! Which is also the feature set packages.

#![cfg(feature = "http")]

mod common;

use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn(
    config: &std::path::Path,
    data: &std::path::Path,
    port: u16,
    token: Option<&str>,
) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"));
    command
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match token {
        Some(token) => {
            command.env("WEB_WIDOW_HTTP_TOKEN", token);
        }
        None => {
            command.env_remove("WEB_WIDOW_HTTP_TOKEN");
        }
    }
    command.spawn().expect("server starts")
}

async fn wait_ready(port: u16) {
    let url = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let response = client
            .post(&url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await;
        if response.is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTP server did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("object arguments").clone()
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    value: Value,
) -> Value {
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args(value)))
        .await
        .unwrap_or_else(|error| panic!("{name} transport failure: {error}"));
    assert_eq!(result.is_error, Some(false), "{name}: {result:?}");
    result.structured_content.unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_transport_requires_the_bearer_token_and_serves_a_real_client() {
    let fixture = common::start_fixture().await;
    fixture.set_page(
        "/monitor/http",
        "<html><head><title>HTTP</title></head><body><main><article>\
         <h1>HTTP monitor</h1><p>stable http monitor content</p>\
         </article></main></body></html>",
    );
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");
    let token = "b06-http-token";
    let port = free_port();
    let child = spawn(&config, &data, port, Some(token));
    wait_ready(port).await;

    let url = format!("http://127.0.0.1:{port}/mcp");
    let http = reqwest::Client::new();
    let no_auth = http
        .post(&url)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(no_auth.headers().get("www-authenticate").is_some());
    let wrong_auth = http
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", "Bearer wrong-token")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The SDK client supplies the same bearer header as any MCP HTTP client.
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(url.clone()).auth_header(token);
    let transport =
        StreamableHttpClientTransport::with_client(reqwest::Client::new(), transport_config);
    let client = ().serve(transport).await.expect("HTTP initialize handshake");

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .expect("tools/list over HTTP")
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names.len(), 34, "{names:?}");
    assert!(names.contains(&"fetch_page".to_string()));
    assert!(names.contains(&"extract_rows".to_string()));
    assert!(names.contains(&"recipe_save".to_string()));
    assert!(names.contains(&"recipe_list".to_string()));
    assert!(names.contains(&"recipe_apply".to_string()));
    assert!(names.contains(&"corpus_export".to_string()));
    assert!(names.contains(&"snapshot_compare".to_string()));
    assert!(names.contains(&"fetch_feed".to_string()));
    assert!(names.contains(&"search".to_string()));
    assert!(names.contains(&"search_content".to_string()));
    assert!(names.contains(&"sitemap".to_string()));
    assert!(names.contains(&"pdf_extract".to_string()));
    assert!(names.contains(&"events".to_string()));
    assert!(names.contains(&"robots_cache".to_string()));
    assert!(names.contains(&"monitor_list".to_string()));
    assert!(names.contains(&"screenshot".to_string()));
    assert!(names.contains(&"blob_get".to_string()));
    assert!(names.contains(&"interact_open".to_string()));
    assert!(names.contains(&"interact_act".to_string()));
    assert!(names.contains(&"interact_state".to_string()));
    assert!(names.contains(&"interact_close".to_string()));

    let fetched = call(
        &client,
        "fetch_page",
        json!({ "url": fixture.url("127.0.0.1", "/article") }),
    )
    .await;
    assert_eq!(fetched["status"], json!(200), "{fetched}");

    // The local index is maintained over the same data directory: the fetch
    // above is findable through the HTTP transport without any network call.
    let local = call(&client, "search_content", json!({ "query": "lighthouse" })).await;
    assert_eq!(local["result_count"], json!(1), "{local}");
    assert_eq!(
        local["results"][0]["url"],
        json!(fixture.url("127.0.0.1", "/article"))
    );

    let created = call(
        &client,
        "monitor_create",
        json!({
            "url": fixture.url("127.0.0.1", "/monitor/http"),
            "interval_s": 3600
        }),
    )
    .await;
    assert!(created["monitor_id"].as_i64().is_some(), "{created}");
    client.cancel().await.expect("HTTP client shutdown");

    // SIGTERM must stop the listener cleanly and leave the lock free.
    let pid = child.id().expect("child pid");
    let status = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("server exits within 10 s")
        .expect("wait");
    assert!(output.status.success(), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "HTTP mode must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
    assert!(
        stderr.contains("serving MCP over streamable HTTP"),
        "{stderr}"
    );
    assert!(stderr.contains("stateless=true"), "{stderr}");
    assert!(!stderr.contains("legacy_sessions"), "{stderr}");

    // A fresh process can take the data-directory lock immediately.
    let after = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("run-due")
        .output()
        .await
        .expect("run-due starts");
    assert!(
        after.status.success(),
        "the data-dir lock was not released: {}",
        String::from_utf8_lossy(&after.stderr)
    );
}

async fn expect_startup_failure(mut command: tokio::process::Command, needle: &str) {
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("startup failure returns promptly")
        .expect("process runs");
    assert!(
        !output.status.success(),
        "expected a startup failure, got success with stderr {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(needle), "expected `{needle}` in: {stderr}");
}

#[tokio::test]
async fn http_startup_refuses_missing_or_unsafe_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let port = free_port();

    let mut missing = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"));
    missing
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(dir.path().join("data-a"))
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("WEB_WIDOW_HTTP_TOKEN");
    expect_startup_failure(missing, "requires a bearer token").await;

    let mut non_loopback = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"));
    non_loopback
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(dir.path().join("data-b"))
        .arg("serve")
        .arg("--http")
        .arg(format!("0.0.0.0:{port}"))
        .env("WEB_WIDOW_HTTP_TOKEN", "token");
    expect_startup_failure(non_loopback, "refusing a non-loopback").await;

    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "token\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let mut loose = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"));
    loose
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(dir.path().join("data-c"))
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--http-token-file")
        .arg(&token_file)
        .env_remove("WEB_WIDOW_HTTP_TOKEN");
    expect_startup_failure(loose, "expected 0600").await;
}
