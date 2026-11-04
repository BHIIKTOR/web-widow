#![cfg(feature = "http")]

//! v0.6 — the loopback feed route (C3-5, C3-6, C3-7).
//!
//! Spawns the real binary (`--features http`) over 127.0.0.1 with a seeded
//! database and the capability token from the environment or the `0600`
//! `<data_dir>/feed-token` file. Proves the bearer is scoped to `/mcp`, the feed
//! has its own token, ETag/304 works, a disabled feed is 404, and the access log
//! never records the token.

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::test_config;
use web_widow::config::Paths;
use web_widow::store::Store;

struct Server {
    child: Child,
    addr: String,
    data: tempfile::TempDir,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn seed(data_dir: &Path) {
    std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = Paths::new(data_dir.to_path_buf());
    std::fs::create_dir_all(paths.blobs()).unwrap();
    let config = test_config();
    let store = Store::open(&paths, &config.fts).expect("store opens");
    drop(store);
    let connection = rusqlite::Connection::open(paths.database()).unwrap();
    connection
        .execute_batch(
            "INSERT INTO monitors
                 (id, url, mode, profile_hash, interval_s, ignore_selectors_json, active,
                  created_at, next_due_at, label)
             VALUES (1, 'http://example.com/a?x=1&y=2', 'main', NULL, 86400, '[]', 1,
                     '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z', 'project:slug');
             INSERT INTO snapshots (id, monitor_id, taken_at, normalized_hash, raw_hash, is_change_point)
             VALUES (1, 1, '2026-01-01T00:00:00.000Z', 'h1', 'r1', 1);
             INSERT INTO snapshots (id, monitor_id, taken_at, normalized_hash, raw_hash, is_change_point)
             VALUES (2, 1, '2026-01-02T00:00:00.000Z', 'h2', 'r2', 1);
             INSERT INTO changes
                 (id, monitor_id, from_snapshot, to_snapshot, detected_at, kind, stats_json, diff_text, truncated)
             VALUES (1, 1, 1, 2, '2026-01-02T00:00:00.000Z', 'substantive',
                     '{\"lines_added\":1,\"lines_removed\":0,\"changed_lines\":1,\"words_added\":2,\"words_removed\":0}',
                     '--- a\\n+++ b', 0);
             INSERT INTO pages
                 (id, url, canonical_url, url_hash, site, first_seen, last_fetched, title, outcome)
             VALUES (1, 'http://example.com/a?x=1&y=2', 'http://example.com/a?x=1&y=2',
                     'hash-a', 'example.com', '2026-01-01T00:00:00.000Z',
                     '2026-01-01T00:00:00.000Z', 'Example A', 'ok');",
        )
        .unwrap();
}

fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let path = dir.join("config.toml");
    let text = format!(
        "[fetch]\nallow_hosts = [\"127.0.0.0/8\"]\nmin_interval_ms = 10\njitter_ms = 0\n\
         contact = \"tests@example.invalid\"\n[render]\ndiscovery = false\n\
         [logging]\nlevel = \"debug\"\n{extra}\n"
    );
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

fn wait_ready(addr: &str, log: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = std::fs::read_to_string(log).unwrap_or_default();
    panic!("server did not listen on {addr}; log:\n{output}");
}

fn spawn(extra_config: &str, feed_token: Option<&str>, bearer: &str) -> Server {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let config = write_config(data.path(), extra_config);
    let addr = free_addr();
    let log = data.path().join("server.log");
    let stderr = std::fs::File::create(&log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_web-widow"));
    command
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(data.path())
        .arg("serve")
        .arg("--http")
        .arg(&addr)
        .env("WEB_WIDOW_HTTP_TOKEN", bearer)
        .stdout(Stdio::null())
        .stderr(stderr);
    if let Some(token) = feed_token {
        command.env("WEB_WIDOW_FEED_TOKEN", token);
    } else {
        command.env_remove("WEB_WIDOW_FEED_TOKEN");
    }
    let child = command.spawn().expect("binary starts");
    let server = Server {
        child,
        addr,
        data,
        log,
    };
    wait_ready(&server.addr, &server.log);
    std::thread::sleep(Duration::from_millis(100));
    server
}

async fn get(url: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.expect("request")
}

async fn feed_ready(server: &Server, token: &str) -> reqwest::Response {
    let url = format!("http://{}/feeds/changes.xml?token={token}", server.addr);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match reqwest::Client::new().get(&url).send().await {
            Ok(response) => return response,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("feed never became reachable: {error}"),
        }
    }
}

fn counts(server: &Server) -> (i64, i64) {
    let connection = rusqlite::Connection::open(server.data.path().join("web-widow.db")).unwrap();
    let changes: i64 = connection
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .unwrap();
    let fetches: i64 = connection
        .query_row("SELECT COUNT(*) FROM fetch_events", [], |row| row.get(0))
        .unwrap();
    (changes, fetches)
}

#[tokio::test]
async fn the_feed_route_has_its_own_token_etag_and_the_bearer_stays_on_mcp() {
    let bearer = "b".repeat(64);
    let feed = "f".repeat(64);
    let mut server = spawn("", Some(&feed), &bearer);
    let url = format!("http://{}/feeds/changes.xml?token={feed}", server.addr);

    let response = feed_ready(&server, &feed).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/rss+xml; charset=utf-8")
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("max-age=60")
    );
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag")
        .to_string();
    let xml = response.text().await.unwrap();
    let parsed = feed_rs::parser::parse(xml.as_bytes()).expect("feed-rs parses the served feed");
    assert_eq!(parsed.entries.len(), 1, "the one seeded change");
    assert_eq!(
        parsed.entries[0]
            .links
            .first()
            .map(|link| link.href.as_str()),
        Some("http://example.com/a?x=1&y=2")
    );

    // ETag / If-None-Match → 304 with no body.
    let not_modified = get(&url, &[("if-none-match", etag.as_str())]).await;
    assert_eq!(not_modified.status(), reqwest::StatusCode::NOT_MODIFIED);
    assert_eq!(
        not_modified
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some(etag.as_str())
    );
    assert!(not_modified.text().await.unwrap().is_empty());

    // Missing/wrong capability token → 401.
    let missing = get(&format!("http://{}/feeds/changes.xml", server.addr), &[]).await;
    assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong = get(
        &format!("http://{}/feeds/changes.xml?token=nope", server.addr),
        &[],
    )
    .await;
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The bearer is scoped to /mcp: the feed token does not authorize /mcp...
    let mcp_url = format!("http://{}/mcp", server.addr);
    let no_bearer = reqwest::Client::new()
        .post(&mcp_url)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(no_bearer.status(), reqwest::StatusCode::UNAUTHORIZED);
    let feed_as_bearer = reqwest::Client::new()
        .post(&mcp_url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {feed}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(feed_as_bearer.status(), reqwest::StatusCode::UNAUTHORIZED);
    // ...and the MCP bearer is not needed for the feed, but does reach /mcp.
    let with_bearer = reqwest::Client::new()
        .post(&mcp_url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_ne!(with_bearer.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The feed is read-only: no change is fabricated and no outbound fetch ran.
    assert_eq!(counts(&server), (1, 0));
    let after = feed_ready(&server, &feed).await;
    assert_eq!(after.status(), reqwest::StatusCode::OK);
    assert_eq!(counts(&server), (1, 0));

    // The access log records the path only, never the query/token or bearer.
    let _ = server.child.kill();
    let _ = server.child.wait();
    let log = std::fs::read_to_string(&server.log).unwrap_or_default();
    assert!(log.contains("/feeds/changes.xml"), "{log}");
    assert!(
        !log.contains(&feed),
        "the feed token must not be logged: {log}"
    );
    assert!(
        !log.contains(&bearer),
        "the bearer must not be logged: {log}"
    );
}

#[tokio::test]
async fn a_disabled_feed_is_404_and_needs_no_token_file() {
    let bearer = "b".repeat(64);
    let server = spawn("[events]\nfeed_enabled = false\n", None, &bearer);
    let response = get(
        &format!("http://{}/feeds/changes.xml?token=anything", server.addr),
        &[],
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(
        !server.data.path().join("feed-token").exists(),
        "a disabled feed provisions no token"
    );
}

#[tokio::test]
async fn the_token_file_is_created_0600_and_authorizes_the_feed() {
    let bearer = "b".repeat(64);
    let server = spawn("", None, &bearer);
    let path = server.data.path().join("feed-token");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let token = std::fs::read_to_string(&path)
        .expect("token file")
        .trim()
        .to_string();
    assert_eq!(token.len(), 64, "32 random bytes as hex");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let response = feed_ready(&server, &token).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}
