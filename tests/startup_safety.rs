//! Startup refusals and operator warnings, exercised through the real
//! binary (the unit tests cover the mechanism; this covers the wiring).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

async fn run_to_completion(
    config: &std::path::Path,
    data: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .await
        .expect("binary should run")
}

#[tokio::test]
async fn world_readable_data_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = run_to_completion(&config, &data).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "startup must fail: {stderr}");
    assert!(
        stderr.contains("accessible by group/other"),
        "stderr should name the permission problem: {stderr}"
    );
}

#[tokio::test]
async fn second_process_on_the_same_data_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");

    let mut first = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("first process should start");
    let stderr = first.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();

    // Wait until the first process has taken the lock and is serving.
    let mut seen_serving = false;
    for _ in 0..40 {
        let line = tokio::time::timeout(Duration::from_millis(500), lines.next_line())
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .unwrap_or_default();
        if line.contains("serving MCP over stdio") {
            seen_serving = true;
            break;
        }
    }
    assert!(
        seen_serving,
        "first process never reached the serving state"
    );

    let second = run_to_completion(&config, &data).await;
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "second process must fail");
    assert!(
        second_stderr.contains("already in use by pid"),
        "stderr should name the holding pid: {second_stderr}"
    );
    assert!(
        second_stderr.contains(&first.id().unwrap().to_string()),
        "stderr should contain the first process' pid"
    );

    drop(first.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(10), first.wait())
        .await
        .expect("first process should exit after EOF")
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn disabled_robots_and_missing_contact_warn_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[fetch]\nallow_hosts = [\"127.0.0.0/8\"]\nmin_interval_ms = 10\n\
         [robots]\nenabled = false\n[logging]\nlevel = \"info\"\n",
    )
    .unwrap();
    let data = dir.path().join("data");

    let mut child = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let mut collected = String::new();
    for _ in 0..60 {
        let line = tokio::time::timeout(Duration::from_millis(500), lines.next_line())
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .unwrap_or_default();
        if line.is_empty() {
            continue;
        }
        collected.push_str(&line);
        collected.push('\n');
        if collected.contains("NOT enforced") && collected.contains("contact is not set") {
            break;
        }
    }
    assert!(
        collected.contains("robots.enabled = false"),
        "robots override must be announced: {collected}"
    );
    assert!(
        collected.contains("contact is not set"),
        "missing contact must warn: {collected}"
    );
    assert!(
        !collected.contains("password") && !collected.contains("token"),
        "startup logs must not mention secrets"
    );
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}

#[tokio::test]
async fn an_unreadable_config_path_fails_before_serving() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.toml");
    let data = dir.path().join("data");
    let output = run_to_completion(&missing, &data).await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot read config"), "{stderr}");
    assert!(
        !data.exists(),
        "a configuration failure must not create the data directory"
    );
}
