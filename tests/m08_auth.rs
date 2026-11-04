//! v0.8 — the credential engine end to end, over the real binary.
//!
//! Every test spawns the server with a fake manager CLI (mode 0700) and a
//! dedicated fixture that requires a credential on some paths and records the
//! headers it sees. The canary strings are test constants; nothing here is
//! written to an artifact or a log.

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use serde_json::{Value, json};

const CANARY_BEARER: &str = "canary-bearer-value";
/// A unique phrase from the authenticated fixture body (F-1 body-marker scan).
const BODY_MARKER: &str = "The credential was accepted";
const CANARY_HEADER: &str = "canary-header-value";
const BASIC: &str = "user:pass";

#[derive(Clone, Debug)]
struct Seen {
    host: String,
    path: String,
    headers: Vec<(String, String)>,
}

#[derive(Clone)]
struct AuthState {
    seen: Arc<Mutex<Vec<Seen>>>,
    port: u16,
}

impl AuthState {
    fn headers_for(&self, path: &str) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path == path)
            .flat_map(|seen| seen.headers.clone())
            .collect()
    }

    fn saw_path(&self, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path == path)
            .count()
    }

    fn paths_for_host(&self, host: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.host == host)
            .map(|seen| seen.path.clone())
            .collect()
    }

    fn headers_for_host(&self, host: &str) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.host == host)
            .flat_map(|seen| seen.headers.clone())
            .collect()
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| seen.path.clone())
            .collect()
    }
}

struct AuthFixture {
    port: u16,
    state: AuthState,
    _handle: tokio::task::JoinHandle<()>,
}

impl AuthFixture {
    async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let mut state = AuthState { seen, port: 0 };
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        state.port = port;
        let app = Router::new()
            .fallback(any(handle))
            .with_state(state.clone());
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Self {
            port,
            state,
            _handle,
        }
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }

    /// Stop accepting connections (the port starts refusing).
    fn shutdown(self) {
        self._handle.abort();
    }
}

const PAGE: &str = "<html><head><title>Auth page</title></head><body><main><article>\
<h1>Authenticated</h1><p>The credential was accepted and this article body is long enough \
to be extracted as the main content of the page by the readability pass, with several \
words in two sentences so the render heuristic never escalates it.</p>\
</article></main></body></html>";

const THIN_PAGE: &str = "<html><head><title>Thin</title></head><body>\
<div id=\"app\"></div><script>window.__boot()</script></body></html>";

async fn handle(State(state): State<AuthState>, request: Request<Body>) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1")
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .to_string();
    let path = request.uri().path().to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect();
    state.seen.lock().unwrap().push(Seen {
        host: host.clone(),
        path: path.clone(),
        headers: headers.clone(),
    });

    if path == "/robots.txt" {
        return respond(
            StatusCode::OK,
            "text/plain",
            "User-agent: *\nAllow: /\nDisallow: /private\n",
        );
    }
    if path == "/redirect" {
        // Cross-host hop: 127.0.0.2 is outside the profile's binding.
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(
                header::LOCATION,
                format!("http://127.0.0.2:{}/bearer", state.port),
            )
            .body(Body::from(""))
            .unwrap();
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    let expected_basic = format!("Basic {}", {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(BASIC)
    });
    let authorized = match path.as_str() {
        "/bearer" => get("authorization").as_deref() == Some(&format!("Bearer {CANARY_BEARER}")),
        "/basic" => get("authorization").as_deref() == Some(expected_basic.as_str()),
        "/header" => get("x-api-key").as_deref() == Some(CANARY_HEADER),
        "/thin" => true,
        "/open" => true,
        _ => false,
    };
    if !authorized {
        return respond(
            StatusCode::UNAUTHORIZED,
            "text/plain",
            "credential required",
        );
    }
    if path == "/thin" {
        return respond(StatusCode::OK, "text/html; charset=utf-8", THIN_PAGE);
    }
    respond(StatusCode::OK, "text/html; charset=utf-8", PAGE)
}

fn respond(status: StatusCode, content_type: &str, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A fake provider CLI. No value is ever in the config: the script holds them.
fn write_manager(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("op");
    let log = dir.to_string_lossy().to_string();
    let script = format!(
        r#"#!/bin/sh
printf 'argv=%s\n' "$*" >> "{log}/argv.log"
if read -r line; then printf 'stdin=data:%s\n' "$line" >> "{log}/proc.log"; else printf 'stdin=eof\n' >> "{log}/proc.log"; fi
{{ printf 'home=%s\n' "${{HOME-}}"
if [ -n "${{OP_TOKEN+x}}" ]; then echo 'OP_TOKEN=set'; else echo 'OP_TOKEN=unset'; fi
if [ -n "${{PATH+x}}" ]; then printf 'PATH=%s\n' "$PATH"; else echo 'PATH=unset'; fi
if [ -n "${{WEB_WIDOW_HTTP_TOKEN+x}}" ]; then echo 'WEB_WIDOW_HTTP_TOKEN=set'; else echo 'WEB_WIDOW_HTTP_TOKEN=unset'; fi
}} >> "{log}/proc.log"
case "$*" in
  "read Private/vault/token") printf '{CANARY_BEARER}\n' ;;
  "read Private/vault/header") printf '{CANARY_HEADER}\n' ;;
  "read Private/vault/user") printf 'user\n' ;;
  "read Private/vault/pass") printf 'pass\n' ;;
  "read Private/vault/slow") exec /bin/sleep 30 ;;
  "read Private/vault/fail") printf 'provider failure\n' >&2; exit 1 ;;
  "read Private/vault/malformed") printf '\377\376\n' ;;
  *) exit 1 ;;
esac
"#
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn auth_config(
    dir: &std::path::Path,
    manager: &std::path::Path,
    loopback_http: bool,
    timeout_ms: u64,
    port: u16,
) -> std::path::PathBuf {
    let extra = format!(
        r#"[auth]
audit_days = 90
manager_timeout_ms = {timeout_ms}
allow_loopback_http = {loopback_http}

[auth.managers.op]
path = "{}"
env_allow = ["OP_TOKEN"]

[auth.profiles.bearer]
kind = "bearer"
host = "127.0.0.1"
reference = "op://Private/vault/token"

[auth.profiles.basic]
kind = "basic"
host = "127.0.0.1"
username_reference = "op://Private/vault/user"
password_reference = "op://Private/vault/pass"

[auth.profiles.header]
kind = "header"
host = "127.0.0.1"
header = "X-Api-Key"
reference = "op://Private/vault/header"

[auth.profiles.slow]
kind = "bearer"
host = "127.0.0.1"
reference = "op://Private/vault/slow"

[auth.profiles.broken]
kind = "bearer"
host = "127.0.0.1"
reference = "op://Private/vault/fail"

[auth.profiles.malformed]
kind = "bearer"
host = "127.0.0.1"
reference = "op://Private/vault/malformed"
"#,
        manager.display()
    );
    //: the credential origin is `host:port` and the port is configured,
    // so every profile declares the fixture's dynamic origin port.
    let extra = extra.replace(
        "host = \"127.0.0.1\"",
        &format!("host = \"127.0.0.1\"\nport = {port}"),
    );
    common::write_binary_config(dir, &extra)
}

fn env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("OP_TOKEN", "operator-token-value"),
        ("WEB_WIDOW_HTTP_TOKEN", "server-only-token"),
        // A distinctive entry: if the child inherited PATH, this marker shows up.
        ("PATH", "/canary-path-marker:/usr/bin:/bin"),
    ]
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// F-1: the table set comes from `sqlite_master`, not a hand-written list, so a
/// new table cannot silently escape the non-persist assertion.
fn counts(db: &rusqlite::Connection) -> Vec<(String, i64)> {
    let tables: Vec<String> = db
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .expect("read sqlite_master")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query table names")
        .flatten()
        .collect();
    assert!(tables.len() > 10, "the schema is populated: {tables:?}");
    tables
        .into_iter()
        .map(|table| {
            let count: i64 = db
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .unwrap_or_else(|error| panic!("count {table}: {error}"));
            (table, count)
        })
        .collect()
}

/// The tool-call result inside a JSON-RPC frame.
fn tool(frame: &Value) -> &Value {
    frame.get("result").unwrap_or(frame)
}

fn is_error(frame: &Value) -> bool {
    tool(frame)["isError"] == json!(true)
}

fn error_code(frame: &Value) -> String {
    tool(frame)["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {frame}"))
        .to_string()
}

fn result_payload(frame: &Value) -> &Value {
    tool(frame)
        .get("structuredContent")
        .unwrap_or_else(|| panic!("no structuredContent in {frame}"))
}

async fn setup(
    _label: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    AuthFixture,
    common::McpChild,
) {
    let dir = tempfile::tempdir().unwrap();
    let manager = write_manager(dir.path());
    let data = dir.path().join("data");
    let fixture = AuthFixture::start().await;
    let config = auth_config(dir.path(), &manager, true, 3_000, fixture.port);
    let stderr = dir.path().join("server.log");
    let child =
        common::McpChild::spawn_with_env_and_stderr(&config, &data, &env(), Some(&stderr)).await;
    (dir, data, fixture, child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bearer_profile_authenticates_and_persists_nothing() {
    let (_dir, data, fixture, mut child) = setup("bearer").await;

    // Without the profile the fixture refuses: the credential is the difference.
    let refused = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer") }),
        )
        .await;
    assert!(is_error(&refused), "{refused}");
    assert_eq!(error_code(&refused), "http_error");
    // Snapshot after that (recorded) failure: the authenticated call must add
    // nothing beyond its audit row.
    let before = counts(&common::db_at(&data));

    let ok = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(!is_error(&ok), "{ok}");
    let payload = result_payload(&ok);
    assert_eq!(payload["status"], json!(200), "{payload}");
    assert_eq!(payload["authenticated"], json!(true), "{payload}");
    assert_eq!(payload["profile"], json!("bearer"), "{payload}");
    assert_eq!(payload["cached"], json!(false), "{payload}");
    assert_eq!(payload["render_mode"], json!("static"), "{payload}");
    assert_eq!(
        payload["render_skipped"],
        json!("auth_static_only"),
        "{payload}"
    );
    assert!(
        payload["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("Authenticated"),
        "{payload}"
    );

    // Non-persist: every content table is unchanged, and the audit row exists.
    let after = counts(&common::db_at(&data));
    for ((table, before), (_, after)) in before.iter().zip(after.iter()) {
        if table == "auth_audit" {
            assert_eq!(*after, *before + 1, "exactly one audit row per attempt");
        } else {
            assert_eq!(after, before, "{table} must not change");
        }
    }
    let row: (String, String, String, Option<i64>, String, i64) = common::db_at(&data)
        .query_row(
            "SELECT site, auth_id, method, status, outcome, resolution_ms FROM auth_audit",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, "bearer");
    assert_eq!(row.1, "127.0.0.1");
    assert_eq!(row.2, "GET");
    assert_eq!(row.3, Some(200));
    assert_eq!(row.4, "ok");
    assert!(row.5 < 3_000, "resolution_ms is a duration: {row:?}");
    // F-2: the row names every host that received the credential (one hop here).
    let host: String = common::db_at(&data)
        .query_row("SELECT host FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(host, "127.0.0.1", "the credential-bearing hop is named");

    // Secret hygiene: no canary in the result, the database or any blob file.
    let serialized = serde_json::to_string(&ok).unwrap();
    assert!(!serialized.contains(CANARY_BEARER), "canary in the result");
    // Every file under the data directory (database, WAL, blobs, exports) is
    // scanned for the credential canary **and** the fixture's unique body
    // marker: a regression that stores the authenticated body anywhere fails.
    for entry in walk(&data) {
        let content = std::fs::read(&entry).unwrap_or_default();
        let text = String::from_utf8_lossy(&content);
        assert!(
            !text.contains(CANARY_BEARER),
            "canary in {}",
            entry.display()
        );
        assert!(
            !text.contains(BODY_MARKER),
            "authenticated body in {}",
            entry.display()
        );
    }
    // Nothing authenticated reached the corpus: the export stays clean.
    let export = child.call("corpus_export", json!({ "limit": 50 })).await;
    let export = serde_json::to_string(&export).unwrap();
    assert!(!export.contains("The credential was accepted"), "{export}");

    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_and_custom_header_profiles_send_the_right_shape() {
    let (_dir, _data, fixture, mut child) = setup("shapes").await;

    let basic = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/basic"), "profile": "basic" }),
        )
        .await;
    assert!(!is_error(&basic), "{basic}");
    assert_eq!(result_payload(&basic)["status"], json!(200));

    let header = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/header"), "profile": "header" }),
        )
        .await;
    assert!(!is_error(&header), "{header}");
    assert_eq!(result_payload(&header)["status"], json!(200));

    // The fixture recorded exactly the shapes the config asked for.
    let basic_headers = fixture.state.headers_for("/basic");
    let expected = {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(BASIC)
        )
    };
    assert!(
        basic_headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("authorization") && value == &expected),
        "{basic_headers:?}"
    );
    let header_headers = fixture.state.headers_for("/header");
    assert!(
        header_headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-api-key") && value == CANARY_HEADER),
        "{header_headers:?}"
    );
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_host_redirect_is_refused_and_the_hop_carries_no_credential() {
    let (_dir, _data, fixture, mut child) = setup("redirect").await;

    let result = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/redirect"), "profile": "bearer" }),
        )
        .await;
    assert!(is_error(&result), "{result}");
    assert_eq!(error_code(&result), "auth_redirect_blocked");

    // The hop's `robots.txt` IS fetched unauthenticated before the binding
    // refusal (robots always is); no other request reaches the redirect target,
    // and no credential header is ever sent to it.
    let cross_host = fixture.state.paths_for_host("127.0.0.2");
    assert!(
        !cross_host.is_empty() && cross_host.iter().all(|path| path == "/robots.txt"),
        "only the unauthenticated robots fetch reaches the redirect target: {cross_host:?}"
    );
    let cross_headers = fixture.state.headers_for_host("127.0.0.2");
    assert!(
        cross_headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")),
        "no credential header on the hop: {cross_headers:?}"
    );
    // The credential-bearing request itself is never made.
    assert_eq!(
        fixture.state.saw_path("/bearer"),
        0,
        "{:?}",
        fixture.state.paths()
    );
    // F-2: no credential flew, so the row names the requested host only.
    let host: String = common::db_at(&_data)
        .query_row("SELECT host FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(host, "127.0.0.1", "{host}");
    // The probe itself is sensitive: a direct request with the header is recorded.
    let direct = reqwest::Client::new()
        .get(fixture.url("127.0.0.1", "/bearer"))
        .header("Authorization", format!("Bearer {CANARY_BEARER}"))
        .send()
        .await
        .unwrap();
    assert_eq!(direct.status(), 200);
    assert!(
        fixture
            .state
            .headers_for("/bearer")
            .iter()
            .any(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    && value == &format!("Bearer {CANARY_BEARER}")
            }),
        "the fixture records an Authorization header when one is sent"
    );

    // The same redirect with no profile is followed (the guard is auth-only)
    // and ends at the fixture's 401, which is an ordinary http_error.
    let plain = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/redirect") }),
        )
        .await;
    assert!(is_error(&plain), "{plain}");
    assert_eq!(error_code(&plain), "http_error", "{plain}");
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_outside_the_binding_gets_no_credential() {
    let (_dir, _data, fixture, mut child) = setup("binding").await;

    let result = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.2", "/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(is_error(&result), "{result}");
    assert_eq!(error_code(&result), "auth_host_not_allowed");
    assert_eq!(
        fixture.state.saw_path("/bearer"),
        0,
        "{:?}",
        fixture.state.paths()
    );
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_http_requires_the_loopback_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let manager = write_manager(dir.path());
    let data = dir.path().join("data");
    let fixture = AuthFixture::start().await;
    let config = auth_config(dir.path(), &manager, false, 3_000, fixture.port);
    let mut child = common::McpChild::spawn_with_env(&config, &data, &env()).await;

    let result = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(is_error(&result), "{result}");
    assert_eq!(error_code(&result), "auth_insecure_url");
    assert_eq!(
        fixture.state.saw_path("/bearer"),
        0,
        "{:?}",
        fixture.state.paths()
    );

    // An https target is not refused by the scheme rule (it fails later, on the
    // TLS/host path, never as auth_insecure_url).
    let https = child
        .call(
            "fetch_page",
            json!({ "url": "https://127.0.0.1:1/bearer", "profile": "bearer" }),
        )
        .await;
    assert_ne!(error_code(&https), "auth_insecure_url", "{https}");
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_unknown_profile_and_the_failed_manager_are_typed_and_audited() {
    let (_dir, data, fixture, mut child) = setup("failures").await;

    let unknown = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "nope" }),
        )
        .await;
    assert_eq!(error_code(&unknown), "auth_profile_unknown");

    let broken = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "broken" }),
        )
        .await;
    assert_eq!(error_code(&broken), "auth_secret_unavailable");

    let slow = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "slow" }),
        )
        .await;
    assert_eq!(error_code(&slow), "auth_manager_timeout");

    // A 401 with a credential attached is a typed rejection, not an http_error.
    let rejected = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/basic"), "profile": "bearer" }),
        )
        .await;
    assert_eq!(error_code(&rejected), "auth_rejected");

    let outcomes: Vec<String> = {
        let db = common::db_at(&data);
        let mut statement = db
            .prepare("SELECT outcome FROM auth_audit ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(
        outcomes,
        vec![
            "auth_profile_unknown",
            "auth_secret_unavailable",
            "auth_manager_timeout",
            "auth_rejected",
        ],
        "one value-free row per attempt"
    );
    // No provider stderr or value reached the row.
    let rows: String = {
        let db = common::db_at(&data);
        db.query_row(
            "SELECT group_concat(site || host || outcome || coalesce(auth_id,'')) FROM auth_audit",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(!rows.contains("provider failure"), "{rows}");
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_subprocess_contract_is_visible_in_the_childs_own_log() {
    let (dir, _data, fixture, mut child) = setup("subprocess").await;

    let result = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(!is_error(&result), "{result}");

    let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
    assert!(argv.contains("argv=read Private/vault/token"), "{argv}");
    let proc = std::fs::read_to_string(dir.path().join("proc.log")).unwrap();
    assert!(
        proc.contains("stdin=eof"),
        "stdin is null, not the MCP stream: {proc}"
    );
    assert!(proc.contains("home="), "HOME is passed: {proc}");
    assert!(!proc.contains("home=\n"), "HOME is non-empty: {proc}");
    assert!(
        proc.contains("OP_TOKEN=set"),
        "the allowlisted var is passed: {proc}"
    );
    // `sh` may install a default PATH, but the service PATH (with its marker)
    // must never reach the child.
    assert!(
        !proc.contains("canary-path-marker"),
        "the service PATH is not inherited: {proc}"
    );
    assert!(
        proc.contains("WEB_WIDOW_HTTP_TOKEN=unset"),
        "server variables never reach the child: {proc}"
    );
    child.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_always_is_refused_and_auto_is_static_only() {
    let (_dir, _data, fixture, mut child) = setup("render").await;

    let always = child
        .call(
            "fetch_page",
            json!({
                "url": fixture.url("127.0.0.1", "/thin"),
                "profile": "bearer",
                "render": "always"
            }),
        )
        .await;
    assert_eq!(error_code(&always), "auth_render_unsupported");

    let auto = child
        .call(
            "fetch_page",
            json!({
                "url": fixture.url("127.0.0.1", "/thin"),
                "profile": "bearer",
                "render": "auto"
            }),
        )
        .await;
    assert!(!is_error(&auto), "{auto}");
    let payload = result_payload(&auto);
    assert_eq!(payload["render_mode"], json!("static"), "{payload}");
    assert_eq!(
        payload["render_skipped"],
        json!("auth_static_only"),
        "{payload}"
    );
    // One request: no browser navigation was attempted (the fixture saw exactly
    // the page fetch, and no second, rendered request).
    assert_eq!(
        fixture.state.saw_path("/thin"),
        1,
        "{:?}",
        fixture.state.paths()
    );
    child.shutdown().await;
}

/// F-2 — a robots refusal and a network failure are distinguishable outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_robots_refusal_is_not_a_transport_failure() {
    let (_dir, data, fixture, mut child) = setup("robots").await;

    // One success caches the host's robots policy (robots is fetched without
    // credentials), so the next two attempts reach their own decision points.
    let ok = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(!is_error(&ok), "{ok}");

    // 1. robots refuses the destination before any credential is resolved.
    let blocked = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/private"), "profile": "bearer" }),
        )
        .await;
    assert_eq!(error_code(&blocked), "blocked_by_robots", "{blocked}");

    // 2. The origin disappears: the cached robots policy allows, the hop fails.
    let port = fixture.port;
    fixture.shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let down = child
        .call(
            "fetch_page",
            json!({ "url": format!("http://127.0.0.1:{port}/bearer"), "profile": "bearer" }),
        )
        .await;
    assert!(
        !is_error(&down) || error_code(&down) != "blocked_by_robots",
        "{down}"
    );

    let rows: Vec<(String, i64)> = {
        let db = common::db_at(&data);
        let mut statement = db
            .prepare("SELECT outcome, resolution_ms FROM auth_audit ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(
        rows.iter()
            .map(|(outcome, _)| outcome.as_str())
            .collect::<Vec<_>>(),
        vec!["ok", "robots_blocked", "transport_error"],
        "{rows:?}"
    );
    // The robots refusal happens before resolution (0 ms); the transport failure
    // resolved a credential first but the connection was refused, so the origin
    // never observed a request — the distinction the outcome names.
    assert_eq!(rows[1].1, 0, "robots refusal resolves nothing: {rows:?}");
    child.shutdown().await;
}

/// F-3 — the caller's profile string is bounded and grammar-checked before it
/// can reach the audit trail, and every stored `site` satisfies the grammar.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callers_profile_name_is_bounded_and_grammar_checked() {
    let (_dir, data, fixture, mut child) = setup("profile-bound").await;
    let url = fixture.url("127.0.0.1", "/bearer");

    let long = "A".repeat(300);
    let refused = child
        .call("fetch_page", json!({ "url": url, "profile": long }))
        .await;
    assert_eq!(error_code(&refused), "schema_invalid", "{refused}");
    assert!(
        tool(&refused)["structuredContent"]["error"]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("/profile"),
        "{refused}"
    );

    let bad_chars = child
        .call("fetch_page", json!({ "url": url, "profile": "bad name" }))
        .await;
    assert_eq!(error_code(&bad_chars), "schema_invalid", "{bad_chars}");

    // A grammar-valid but unknown name is audited (and only that one).
    let unknown = child
        .call(
            "fetch_page",
            json!({ "url": url, "profile": "not-configured" }),
        )
        .await;
    assert_eq!(error_code(&unknown), "auth_profile_unknown", "{unknown}");

    let sites: Vec<String> = {
        let db = common::db_at(&data);
        let mut statement = db
            .prepare("SELECT site FROM auth_audit ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(sites, vec!["not-configured".to_string()], "{sites:?}");
    for site in &sites {
        assert!(
            !site.is_empty()
                && site.len() <= 64
                && site
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            "auth_audit.site obeys the profile-name grammar: {site:?}"
        );
    }
    child.shutdown().await;
}

/// F-4 — the guard and the authenticated branch over the streamable-HTTP
/// transport, measured with a real `serve --http` process.
#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guard_and_the_authenticated_branch_hold_over_streamable_http() {
    use std::process::Stdio;
    use std::time::Duration;

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    async fn rpc(
        client: &reqwest::Client,
        url: &str,
        token: &str,
        id: u64,
        method: &str,
        params: Value,
    ) -> Value {
        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .send()
            .await
            .expect("http request");
        assert_eq!(response.status(), 200, "{method}");
        let body = response.text().await.expect("response body");
        if body.trim_start().starts_with('{') {
            return serde_json::from_str(&body).expect("json frame");
        }
        for line in body.lines().rev() {
            if let Some(data) = line.strip_prefix("data:")
                && let Ok(frame) = serde_json::from_str::<Value>(data.trim())
            {
                return frame;
            }
        }
        panic!("no JSON-RPC frame in the SSE body: {body}");
    }

    let dir = tempfile::tempdir().unwrap();
    let manager = write_manager(dir.path());
    let data = dir.path().join("data");
    let fixture = AuthFixture::start().await;
    let config = auth_config(dir.path(), &manager, true, 3_000, fixture.port);
    let port = free_port();
    let token = "m08-a1-http-token";
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .env("WEB_WIDOW_HTTP_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("http server starts");
    let url = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    rpc(
        &client,
        &url,
        token,
        1,
        "initialize",
        json!({
            "protocolVersion": "2026-07-28",
            "capabilities": {},
            "clientInfo": { "name": "a5-http", "version": "1" }
        }),
    )
    .await;

    // (a) a credential-bearing key on a non-declaring tool over HTTP
    let guarded = rpc(
        &client,
        &url,
        token,
        2,
        "tools/call",
        json!({ "name": "fetch_feed", "arguments": { "url": fixture.url("127.0.0.1", "/open"), "profile": "bearer" } }),
    )
    .await;
    assert!(is_error(&guarded), "{guarded}");
    assert_eq!(error_code(&guarded), "schema_invalid", "{guarded}");
    assert_eq!(fixture.state.saw_path("/open"), 0, "no fetch happened");
    let rows: i64 = common::db_at(&data)
        .query_row("SELECT COUNT(*) FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no audit row for a refused argument");

    // Prime the robots cache with an unauthenticated fetch of the same host, so
    // the authenticated call below can be asserted table-for-table: the only
    // store write F-1 records on that path is the (unauthenticated) `hosts` row,
    // and it must already exist.
    let plain = rpc(
        &client,
        &url,
        token,
        3,
        "tools/call",
        json!({ "name": "fetch_page", "arguments": { "url": fixture.url("127.0.0.1", "/open") } }),
    )
    .await;
    assert!(!is_error(&plain), "{plain}");
    let rows: i64 = common::db_at(&data)
        .query_row("SELECT COUNT(*) FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "an unauthenticated fetch over HTTP writes no audit row"
    );

    // (b) an authenticated fetch over the same transport
    let before = counts(&common::db_at(&data));
    let ok = rpc(
        &client,
        &url,
        token,
        4,
        "tools/call",
        json!({ "name": "fetch_page", "arguments": { "url": fixture.url("127.0.0.1", "/bearer"), "profile": "bearer" } }),
    )
    .await;
    assert!(!is_error(&ok), "{ok}");
    assert_eq!(result_payload(&ok)["authenticated"], json!(true), "{ok}");
    let after = counts(&common::db_at(&data));
    for ((table, before), (_, after)) in before.iter().zip(after.iter()) {
        if table == "auth_audit" {
            assert_eq!(*after, *before + 1, "exactly one audit row over HTTP");
        } else {
            assert_eq!(after, before, "{table} must not change");
        }
    }
    let _ = child.start_kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unauthenticated_fetch_writes_no_audit_row() {
    let (_dir, data, fixture, mut child) = setup("plain").await;
    let result = child
        .call(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/open") }),
        )
        .await;
    assert!(!is_error(&result), "{result}");
    assert!(result_payload(&result).get("authenticated").is_none());
    let rows: i64 = common::db_at(&data)
        .query_row("SELECT COUNT(*) FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no audit row without a profile");
    child.shutdown().await;
}

/// The credential-scanning check, finally measured: the server's own stderr
/// is captured and scanned. No credential value, no reference and no provider stderr text
/// may appear, including on the forced-failure paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_log_carries_no_credential_or_provider_output() {
    let (dir, data, fixture, mut child) = setup("logscan").await;

    // Successes attach the credential; the failures are the paths that could
    // echo a value or a provider's own output.
    for (path, profile, expect_error) in [
        ("/bearer", "bearer", false),
        ("/header", "header", false),
        ("/bearer", "broken", true),
        ("/bearer", "slow", true),
        ("/bearer", "malformed", true),
    ] {
        let result = child
            .call(
                "fetch_page",
                json!({ "url": fixture.url("127.0.0.1", path), "profile": profile }),
            )
            .await;
        assert_eq!(is_error(&result), expect_error, "{profile}: {result}");
    }

    // The malformed provider value is a typed refusal with its audit row.
    let outcomes: Vec<String> = {
        let db = common::db_at(&data);
        let mut statement = db
            .prepare("SELECT outcome FROM auth_audit ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(
        outcomes,
        vec![
            "ok",
            "ok",
            "auth_secret_unavailable",
            "auth_manager_timeout",
            "auth_secret_unavailable",
        ],
        "{outcomes:?}"
    );

    child.shutdown().await;

    let log = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    // The scan must cover real server output, not an empty file.
    assert!(log.len() > 64, "the server wrote a log: {log:?}");
    assert!(
        log.contains("serving MCP over stdio") || log.contains("tool call failed"),
        "the captured log is the server's own stderr: {log:?}"
    );
    // The auth path demonstrably ran and logged its codes in the same file.
    for code in ["auth_secret_unavailable", "auth_manager_timeout"] {
        assert!(
            log.contains(code),
            "the auth path logged `{code}` while never logging the value: {log:?}"
        );
    }
    for needle in [
        CANARY_BEARER,
        CANARY_HEADER,
        "provider failure",
        "op://Private",
        "Authorization",
        "X-Api-Key",
    ] {
        assert!(
            !log.contains(needle),
            "the server log leaked `{needle}`:\n{log}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_profile_on_another_tool_is_refused_before_dispatch() {
    let (_dir, data, fixture, mut child) = setup("guard").await;
    let result = child
        .call(
            "fetch_feed",
            json!({ "url": fixture.url("127.0.0.1", "/open"), "profile": "bearer" }),
        )
        .await;
    assert_eq!(error_code(&result), "schema_invalid");
    assert_eq!(fixture.state.saw_path("/open"), 0, "no fetch happened");
    let rows: i64 = common::db_at(&data)
        .query_row("SELECT COUNT(*) FROM auth_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no audit row for a refused argument");
    child.shutdown().await;
}
