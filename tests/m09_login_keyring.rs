//! the keyring preflight happens **before** any window.
//!
//! This is its own test binary so the deliberately broken D-Bus address cannot
//! affect any other test's keyring access. The Chrome path is a non-existent
//! file: reaching the launch would produce `auth_login_browser_failed`, so an
//! `auth_store_unavailable`/`auth_store_locked` answer proves the preflight
//! refused first and no window was ever considered.

use web_widow::auth::login::{Decision, Login, Operator};
use web_widow::config::{AuthKind, AuthProfileConfig, Config};
use web_widow::core::error::Result;

struct Scripted(Decision);

impl Operator for Scripted {
    fn confirm(&self, _origin: &str) -> Result<Decision> {
        Ok(self.0)
    }
}

#[test]
fn an_unusable_keyring_refuses_before_any_window() {
    // A session bus that cannot exist: Secret Service is unreachable.
    unsafe {
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/web-widow-test-bus",
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut config = Config::default();
    config.auth.profiles.insert(
        "site".to_string(),
        AuthProfileConfig {
            kind: AuthKind::Session,
            host: "login.example.test".to_string(),
            login_url: Some("https://login.example.test/login".to_string()),
            login_origins: vec!["login.example.test".to_string()],
            verify_url: Some("https://login.example.test/verify".to_string()),
            session_max_age_days: Some(7),
            ..Default::default()
        },
    );
    let login = Login::from_config(&config, "site")
        .expect("session profile")
        .with_browser(
            // Deliberately absent: the launch must never be attempted.
            dir.path().join("no-such-chrome"),
            data.clone(),
        );
    let error = login
        .run(&Scripted(Decision::Capture))
        .expect_err("an unreachable keyring must refuse");
    assert!(
        matches!(
            error.code().as_str(),
            "auth_store_unavailable" | "auth_store_locked"
        ),
        "expected a keyring refusal, got {}: {error}",
        error.code().as_str()
    );
    // No window, no profile directory, no jar.
    assert!(
        !data.join("auth/login/site").exists(),
        "the preflight must refuse before any login profile exists"
    );
    assert!(!data.join("auth/site.sess").exists());
}
