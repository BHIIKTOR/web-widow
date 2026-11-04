//! v0.9 — the pipe driver and the login boundary's non-interactive proofs.
//!
//! These run on the real pinned Chrome (a pipe-only, headed launch is the point)
//! and skip with an explicit marker when no browser is available. The interactive
//! login flow itself is exercised hermetically by `m09_login_e2e.rs`.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use web_widow::auth::pipe::{Browser, LaunchOptions};
use web_widow::config::Config;

/// This host has no X server, so the headed launch is given Chrome's headless
/// *platform* backend (a full browser, no window). The production launch line
/// carries neither flag and is asserted in `auth::pipe`'s unit tests.
fn platform_args() -> Vec<String> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        vec![
            "--ozone-platform=headless".to_string(),
            "--disable-gpu".to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn launch(chrome: &std::path::Path, dir: &tempfile::TempDir) -> Browser {
    let options = LaunchOptions {
        extra_args: platform_args(),
        ..LaunchOptions::cli(
            chrome.to_path_buf(),
            dir.path().join("profile"),
            dir.path().join("chrome.stderr.log"),
        )
    };
    Browser::launch(&options, Arc::new(|_, _| {})).expect("the login browser should launch")
}

fn data_dir(root: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).expect("mode");
    data
}

#[test]
fn the_pipe_driver_round_trips_over_pipe_without_a_tcp_listener() {
    let Some(chrome) = common::skip_without_chrome("the_pipe_driver_round_trips") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let browser = launch(&chrome, &dir);

    let version = browser.version().expect("Browser.getVersion over the pipe");
    assert!(
        version.contains("Chrome/"),
        "unexpected product string: {version}"
    );

    let facts = browser.facts();
    println!(
        "EVIDENCE pipe version={version} pid={} pids={} tcp_listeners={:?} devtools_active_port_absent={}",
        facts.pid,
        facts.pids.len(),
        facts.tcp_listeners,
        facts.devtools_active_port_absent
    );
    assert!(
        facts.no_tcp_listener(),
        "the login browser must publish no TCP listener: {:?}",
        facts.tcp_listeners
    );
    assert!(
        facts.devtools_active_port_absent,
        "no DevToolsActivePort file may exist in the login profile"
    );

    // A real command round-trip: create a target, attach, evaluate, read cookies.
    let target = browser
        .create_target("data:text/html,<title>ww-pipe</title>")
        .expect("Target.createTarget");
    let session = browser.attach(&target).expect("Target.attachToTarget");
    browser
        .enable_page(&session)
        .expect("Page/Runtime/Network enable");
    std::thread::sleep(Duration::from_millis(300));
    let title = browser
        .evaluate(&session, "document.title")
        .expect("Runtime.evaluate");
    assert_eq!(title.get("value").and_then(Value::as_str), Some("ww-pipe"));
    assert!(browser.cookies(Some(&session)).is_ok());
    browser.shutdown();
}

#[test]
fn a_same_user_attach_finds_no_tcp_devtools_endpoint() {
    let Some(chrome) = common::skip_without_chrome("a_same_user_attach_finds_no_endpoint") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let browser = launch(&chrome, &dir);
    let facts = browser.facts();

    // The classic TCP DevTools endpoint is not there.
    assert!(
        std::net::TcpStream::connect_timeout(
            &"127.0.0.1:9222".parse().unwrap(),
            Duration::from_millis(300)
        )
        .is_err(),
        "a same-user attach to 127.0.0.1:9222 must fail"
    );
    // Nothing in the browser's own pid tree listens at all.
    assert!(facts.pids.contains(&facts.pid));
    assert!(
        facts.tcp_listeners.is_empty(),
        "no process in the browser tree may listen: {:?}",
        facts.tcp_listeners
    );
    assert!(!browser.profile().join("DevToolsActivePort").exists());
    browser.shutdown();
}

#[test]
fn auth_login_refuses_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(
        dir.path(),
        r#"
[auth.profiles.site]
kind = "session"
host = "login.example.test"
login_url = "https://login.example.test/login"
login_origins = ["login.example.test"]
verify_url = "https://login.example.test/verify"
session_max_age_days = 7
"#,
    );
    let data = data_dir(dir.path());
    let output = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("auth")
        .arg("login")
        .arg("site")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the binary should run");
    assert!(
        !output.status.success(),
        "a non-terminal login must fail: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auth_login_not_terminal"),
        "the refusal must be typed, got: {stderr}"
    );
    // The refusal happens before any window: no login profile directory exists.
    assert!(!data.join("auth/login/site").exists());
}

#[test]
fn no_mcp_tool_exposes_the_login_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = data_dir(dir.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("stdio server should start");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut send = |frame: Value| {
        let mut line = serde_json::to_string(&frame).unwrap();
        line.push('\n');
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    };
    let read_until = |stdout: &mut BufReader<std::process::ChildStdout>, id: u64| loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("server closed stdout while waiting for id {id}");
        }
        let value: Value = serde_json::from_str(line.trim()).expect("JSON frame");
        if value["id"] == json!(id) {
            return value;
        }
    };

    send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2026-07-28",
            "capabilities": {},
            "clientInfo": {"name": "m09-login-test", "version": "0.1.0"}
        }
    }));
    read_until(&mut stdout, 1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let response = read_until(&mut stdout, 2);
    let names: Vec<String> = response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(!names.is_empty(), "the server must expose its tools");
    assert!(
        !names
            .iter()
            .any(|name| name.contains("login") || name == "auth" || name.starts_with("auth_")),
        "no MCP tool may reach the login boundary, got: {names:?}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_bearer_profile_cannot_start_a_login() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = common::write_binary_config(
        dir.path(),
        r#"
[auth.managers.pass]
path = "/bin/true"
env_allow = []

[auth.profiles.header]
kind = "bearer"
host = "github.com"
reference = "pass://github/token"
"#,
    );
    let config = Config::load(Some(&config_path), Some(dir.path().join("data")))
        .expect("config should load")
        .config;
    let error = web_widow::auth::login::Login::from_config(&config, "header").unwrap_err();
    assert_eq!(error.code().as_str(), "auth_session_unsupported");
}

#[test]
fn the_login_browser_inherits_only_the_minimal_environment() {
    let Some(chrome) = common::skip_without_chrome("the_login_browser_environment") else {
        return;
    };
    // A forbidden name planted in this process must not reach the browser tree.
    unsafe {
        std::env::set_var("WEB_WIDOW_TEST_SESSION_TOKEN", "must-not-leak");
        std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "must-not-leak");
    }
    let dir = tempfile::tempdir().unwrap();
    let browser = launch(&chrome, &dir);
    let environ = std::fs::read(format!("/proc/{}/environ", browser.pid())).expect("environ");
    let mut names: Vec<String> = environ
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            String::from_utf8_lossy(entry)
                .split_once('=')
                .map(|(name, _)| name.to_string())
                .unwrap_or_default()
        })
        .collect();
    names.sort();
    println!("EVIDENCE browser env names: {names:?}");
    for forbidden in [
        "WEB_WIDOW_TEST_SESSION_TOKEN",
        "OP_SERVICE_ACCOUNT_TOKEN",
        "PATH",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "{forbidden} must not be inherited: {names:?}"
        );
    }
    for name in &names {
        assert!(
            web_widow::auth::pipe::LAUNCH_ENV_ALLOW.contains(&name.as_str())
                || matches!(name.as_str(), "PWD" | "SHLVL" | "_"),
            "unexpected inherited variable {name}"
        );
    }
    browser.shutdown();
}
