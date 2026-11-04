//! v0.9 — the sealed session store, end to end.
//!
//! Two halves:
//!
//! - `auth status`/`auth logout` take **no data-dir lock** and create nothing:
//!   they are exercised with another process (this test) holding the lock.
//! - a sealed session is reported value-free and destroyed by the CLI. That half
//!   needs a Secret Service on the session bus; without one the test prints
//!   `SKIP` and returns (house style, like the render and PDF suites).
//!
//! It writes a real keyring item under the operator's own session
//! (`service = "web-widow"`, `account = "session/example"`) and deletes it again
//! through `auth logout`; a failure before the logout can leave that one item
//! behind, which `web-widow auth logout example` removes.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use web_widow::auth::store::{SessionMeta, SessionStore};
use web_widow::core::error::WidowError;

const CANARY: &str = "session-canary-cookie-value-9f3a";

fn run_auth(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env_remove("WEB_WIDOW_CONFIG")
        .env_remove("WEB_WIDOW_DATA")
        .output()
        .expect("the web-widow binary runs")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object ({error}): {:?} / stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn meta() -> SessionMeta {
    SessionMeta {
        captured_at: 1_800_000_000,
        expires_at_min: Some(1_800_003_600),
        cookie_count: 2,
        hosts: vec!["example.test".to_string()],
        verify_url: Some("https://example.test/account".to_string()),
    }
}

#[test]
fn auth_status_takes_no_data_dir_lock_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir_all(&data).unwrap();

    // Hold the one-process lock exactly as a running `serve` would.
    let lock_path = data.join("server.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().expect("the test holds the data-dir lock");

    let output = run_auth(&data, &["auth", "status"]);
    assert!(
        output.status.success(),
        "auth status must not compete for the lock: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["sites"], serde_json::json!([]));

    // The lock is still ours: a second exclusive `flock` on the same path is
    // refused with `WouldBlock`, exactly as `DataDir::open` would see it.
    let second = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    match second.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {}
        other => panic!("auth status must leave the data-dir lock untouched: {other:?}"),
    }

    let output = run_auth(&data, &["auth", "status", "example"]);
    assert!(output.status.success());
    let row = &json(&output)["sites"][0];
    assert_eq!(row["site"], "example");
    assert_eq!(row["present"], false);
    assert_eq!(row["key_present"], false);
    assert!(
        !data.join("auth").exists(),
        "a read-only status creates no store"
    );
}

#[test]
fn a_sealed_session_is_reported_value_free_and_destroyed_by_the_cli() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let store = SessionStore::new(data.join("auth"));

    if let Err(error) = store.seal("example", CANARY.as_bytes(), &meta()) {
        let detail = match &error {
            WidowError::AuthStoreUnavailable { detail }
            | WidowError::AuthStoreLocked { detail } => detail.clone(),
            other => panic!("unexpected seal failure: {other}"),
        };
        eprintln!("SKIP: no reachable Secret Service session ({detail})");
        return;
    }

    // The store is private, and only the envelope is on disk.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(data.join("auth"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for suffix in ["example.sess", "example.meta"] {
        let path = data.join("auth").join(suffix);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{suffix} is 0600"
        );
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(CANARY.len())
                .any(|window| window == CANARY.as_bytes()),
            "{suffix} must not carry the payload"
        );
    }

    let output = run_auth(&data, &["auth", "status"]);
    assert!(
        output.status.success(),
        "stderr {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(CANARY),
        "status is value-free"
    );
    let row = &json(&output)["sites"][0];
    assert_eq!(row["site"], "example");
    assert_eq!(row["present"], true);
    assert_eq!(row["key_present"], true);
    assert_eq!(row["cookie_count"], 2);
    assert_eq!(row["captured_at"], 1_800_000_000);
    assert_eq!(row["expires_at_min"], 1_800_003_600);
    assert_eq!(row["expired"], false);
    assert_eq!(row["hosts"], serde_json::json!(["example.test"]));
    assert_eq!(row["verify_url"], "https://example.test/account");

    let unsealed = store.unseal("example").unwrap().unwrap();
    assert_eq!(&unsealed.payload[..], CANARY.as_bytes());
    assert_eq!(unsealed.meta, meta());

    let output = run_auth(&data, &["auth", "logout", "example"]);
    assert!(
        output.status.success(),
        "stderr {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["removed"], true);
    assert!(!data.join("auth").join("example.sess").exists());
    assert!(!data.join("auth").join("example.meta").exists());
    assert!(
        store.unseal("example").unwrap().is_none(),
        "logout removes the sealed session"
    );

    let output = run_auth(&data, &["auth", "status", "example"]);
    assert_eq!(json(&output)["sites"][0]["present"], false);

    // A second logout is a no-op, not an error.
    let output = run_auth(&data, &["auth", "logout", "example"]);
    assert!(output.status.success());
    assert_eq!(json(&output)["removed"], false);
}
