//! v0.9 — a unit with **no session bus** is a typed refusal, and nothing is
//! created (fail-closed proof).
//!
//! This binary holds exactly one test on purpose: the credential-store
//! initialization in `keyring` is one-shot and process-wide, so the broken bus
//! address must be in place before the first keyring use. A test that shares a
//! process with a healthy store could not prove the refusal.

use std::fs;
use std::path::Path;
use std::process::Command;

use web_widow::auth::store::{SessionMeta, SessionStore};

fn meta() -> SessionMeta {
    SessionMeta {
        captured_at: 1_800_000_000,
        expires_at_min: None,
        cookie_count: 1,
        hosts: vec!["example.test".to_string()],
        verify_url: None,
    }
}

fn run_auth(data: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env_remove("WEB_WIDOW_CONFIG")
        .env_remove("WEB_WIDOW_DATA")
        .output()
        .expect("the web-widow binary runs")
}

#[test]
fn a_host_without_a_session_bus_is_typed_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("no-such-bus");
    // SAFETY: this binary has one test, and `keyring` initializes its store
    // lazily on first use, which happens after this line.
    unsafe {
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        );
    }

    let data = temp.path().join("data");
    fs::create_dir_all(&data).unwrap();

    let store = SessionStore::new(data.join("auth"));
    let error = store
        .seal("example", b"payload", &meta())
        .expect_err("a missing session bus must refuse to seal");
    assert_eq!(error.code().as_str(), "auth_store_unavailable", "{error}");
    assert!(
        !store.dir().exists(),
        "fail-closed: an unavailable keyring creates no store directory"
    );

    let output = run_auth(&data, &["auth", "logout", "example"]);
    assert!(
        !output.status.success(),
        "logout without a keyring is a failure, not a no-op"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("auth_store_unavailable"),
        "stderr names the typed code: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !data.join("auth").exists(),
        "the CLI creates no store when the keyring is unavailable"
    );

    // A status of an absent site needs no keyring at all: presence is a file.
    let output = run_auth(&data, &["auth", "status"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["sites"],
        serde_json::json!([])
    );

    // A jar on disk cannot be unlocked, and status says so instead of guesswork.
    fs::create_dir_all(store.dir()).unwrap();
    let mut jar = b"WWS1".to_vec();
    jar.extend_from_slice(&[0u8; 40]);
    fs::write(store.dir().join("example.sess"), &jar).unwrap();
    let error = store
        .unseal("example")
        .expect_err("a missing session bus must refuse to unseal");
    assert_eq!(error.code().as_str(), "auth_store_unavailable");
    let error = store
        .status("example")
        .expect_err("status must not pretend a sealed jar is usable");
    assert_eq!(error.code().as_str(), "auth_store_unavailable");
}
