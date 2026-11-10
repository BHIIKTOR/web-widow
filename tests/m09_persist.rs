//! B5b — schema v10 labels and opt-in persistence, per egress surface.
//!
//! Every test uses the real session path (sealed jar + the session profile) and
//! a loopback fixture, then inspects the store through the public APIs:
//! `corpus_pages`, `pages_fts`, `embedding_candidates`, `events_page`, and the
//! cache read.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use web_widow::auth::login::CapturedCookie;
use web_widow::auth::store::{SessionMeta, SessionStore};
use web_widow::config::{AuthKind, AuthProfileConfig, Config};
use web_widow::pipeline::FetchPageInput;

struct Server {
 port: u16,
 hits: Arc<Mutex<Vec<String>>>,
}

fn start_server() -> Server {
 let listener = TcpListener::bind("127.0.0.1:0").unwrap();
 let port = listener.local_addr().unwrap().port();
 let hits = Arc::new(Mutex::new(Vec::new()));
 let recorded = Arc::clone(&hits);
 std::thread::spawn(move || {
 for stream in listener.incoming().flatten() {
 handle(stream, &recorded);
 }
 });
 Server { port, hits }
}

fn handle(stream: TcpStream, hits: &Arc<Mutex<Vec<String>>>) {
 let mut reader = BufReader::new(stream);
 let mut line = String::new();
 if reader.read_line(&mut line).unwrap_or(0) == 0 {
 return;
 }
 let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
 loop {
 let mut header = String::new();
 if reader.read_line(&mut header).unwrap_or(0) == 0 {
 break;
 }
 if header.trim_end().is_empty() {
 break;
 }
 }
 hits.lock().unwrap().push(path.clone());
 let body = format!("<html><body><h1>page</h1><p>content for {path}</p></body></html>");
 let mut writer = reader.into_inner();
 let _ = writer.write_all(
 format!(
 "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
 body.len()
)
.as_bytes());
}

fn now() -> i64 {
 SystemTime::now()
.duration_since(UNIX_EPOCH)
.unwrap()
.as_secs() as i64
}

fn session_config(site: &str, port: u16, persist: bool, index_authenticated: bool) -> Config {
 let mut config = common::test_config();
 config.auth.allow_loopback_http = true;
 config.auth.persist = persist;
 config.auth.index_authenticated = index_authenticated;
 config.auth.profiles.insert(
 site.to_string(),
 AuthProfileConfig {
 kind: AuthKind::Session,
 host: "127.0.0.1".to_string(),
 port,
 login_url: Some("https://127.0.0.1/login".to_string()),
 login_origins: vec!["127.0.0.1".to_string()],
 verify_url: Some("https://127.0.0.1/verify".to_string()),
 session_max_age_days: Some(7),
..Default::default()
 });
 config
}

fn cookie() -> CapturedCookie {
 CapturedCookie {
 name: "ww".to_string(),
 value: zeroize::Zeroizing::new("abc".to_string()),
 domain: "127.0.0.1".to_string(),
 path: "/".to_string(),
 secure: false,
 http_only: false,
 same_site: None,
 expires: Some(now() + 3600),
 }
}

fn seal(dir: &std::path::Path, site: &str) {
 let store = SessionStore::new(dir.to_path_buf());
 let payload = serde_json::to_vec(&[cookie()]).unwrap();
 let meta = SessionMeta {
 captured_at: now(),
 expires_at_min: Some(now() + 3600),
 cookie_count: 1,
 hosts: vec!["127.0.0.1".to_string()],
 verify_url: None,
 };
 store.seal(site, &payload, &meta).expect("seal");
}

fn keyring_available(dir: &std::path::Path, site: &str) -> bool {
 match SessionStore::new(dir.to_path_buf()).preflight(site) {
 Ok(()) => true,
 Err(error) => {
 eprintln!("SKIP m09 persist: keyring unavailable: {error}");
 false
 }
 }
}

fn input(url: String, site: &str) -> FetchPageInput {
 FetchPageInput {
 url,
 mode: None,
 render: None,
 freshness_s: None,
 max_chars: None,
 include_links: None,
 include_content: None,
 profile: Some(site.to_string()),
 }
}

fn page_count(env: &common::TestEnv) -> i64 {
 env.db()
.query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
.unwrap()
}

#[tokio::test]
async fn persist_false_keeps_the_authenticated_fetch_return_only() {
 let site = "site-b5b-off";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, false, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 let result = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 assert!(result.payload.authenticated);
 assert_eq!(page_count(&env), 0, "nothing is stored by default");
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn persist_true_labels_the_page_and_the_export_excludes_it() {
 let site = "site-b5b-on";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 let result = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 assert!(result.payload.authenticated);

 let connection = env.db();
 let (authenticated, auth_site): (i64, Option<String>) = connection
.query_row(
 "SELECT authenticated, auth_site FROM pages WHERE url LIKE '%/page'",
 [],
 |row| Ok((row.get(0)?, row.get(1)?)))
.expect("the page is stored with its label");
 assert_eq!(authenticated, 1);
 assert_eq!(auth_site.as_deref(), Some(site));
 // B5b rework: an authenticated page is extraction-only — no blob row, no
 // `raw_hash` pointing at a CAS object this path never wrote.
 let (raw_hash, blobs): (Option<String>, i64) = connection
.query_row(
 "SELECT raw_hash, (SELECT COUNT(*) FROM blobs) FROM pages
 WHERE url LIKE '%/page'",
 [],
 |row| Ok((row.get(0)?, row.get(1)?)))
.expect("the authenticated page row");
 assert!(raw_hash.is_none(), "no dangling raw_hash");
 assert_eq!(blobs, 0, "no blobs row for an extraction-only page");

 // Export: always excluded, no flag.
 let rows = env
.store
.db
.corpus_pages(None, None, 0, 50)
.await
.expect("corpus pages");
 assert!(
 rows.is_empty(),
 "corpus export excludes authenticated pages"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn fts_and_embedding_candidates_exclude_by_default_and_follow_the_flag() {
 for (site, index, expect_present) in [
 ("site-b5b-idx-off", false, false),
 ("site-b5b-idx-on", true, true),
 ] {
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, index)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 env.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 let connection = env.db();
 let fts: i64 = connection
.query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
.unwrap();
 let candidates = env
.store
.db
.embedding_candidates("minilm-l6-v2-q", 10, index)
.await
.unwrap();
 println!(
 "EVIDENCE b5b index_authenticated={index} fts_rows={fts} candidates={}",
 candidates.len()
);
 if expect_present {
 assert!(fts > 0, "index_authenticated=true indexes the page");
 assert!(!candidates.is_empty(), "the page is an embedding candidate");
 } else {
 assert_eq!(fts, 0, "FTS excludes authenticated pages by default");
 assert!(
 candidates.is_empty(),
 "embeddings exclude authenticated pages by default"
);
 }
 let _ = SessionStore::new(auth_dir).logout(site);
 }
}

#[tokio::test]
async fn events_exclude_authenticated_pages_by_default_and_follow_the_flag() {
 let site = "site-b5b-events";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 env.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");

 // A monitor whose URL is the authenticated page's canonical URL: the only
 // way an event can point at an authenticated page.
 {
 let connection = env.db();
 let at = "2026-11-04T00:00:00.000Z";
 // Join on the stored canonical URL, which carries the fixture's port.
 let canonical: String = connection
.query_row(
 "SELECT canonical_url FROM pages WHERE authenticated = 1 LIMIT 1",
 [],
 |row| row.get(0))
.expect("the stored authenticated page");
 connection
.execute(
 "INSERT INTO monitors (id, url, mode, interval_s, created_at, next_due_at)
 VALUES (1, ?1, 'text', 3600, ?2, ?2)",
 rusqlite::params![canonical, at])
.unwrap();
 for id in [1, 2] {
 connection
.execute(
 "INSERT INTO snapshots (id, monitor_id, taken_at, normalized_hash, raw_hash)
 VALUES (?1, 1, ?2, 'n', 'r')",
 rusqlite::params![id, at])
.unwrap();
 }
 connection
.execute(
 "INSERT INTO changes (monitor_id, from_snapshot, to_snapshot, detected_at, kind,
 stats_json, diff_text)
 VALUES (1, 1, 2, ?1, 'changed', '{}', '-a\\n+b')",
 [at])
.unwrap();
 }
 let excluded = env
.store
.db
.events_page(None, None, 10, false)
.await
.unwrap();
 let included = env
.store
.db
.events_page(None, None, 10, true)
.await
.unwrap();
 println!(
 "EVIDENCE b5b events default={} with_flag={}",
 excluded.len(),
 included.len()
);
 assert!(excluded.is_empty(), "events exclude by default");
 assert_eq!(included.len(), 1, "the operator flag includes it");
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn an_unauthenticated_call_cannot_read_an_authenticated_cache_entry() {
 let site = "site-b5b-cache";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 let url = format!("http://127.0.0.1:{}/page", server.port);
 env.pipeline
.fetch_page(input(url.clone(), site))
.await
.expect("the session fetch succeeds");
 // Same URL, no profile, a freshness window that would serve a cached entry.
 let mut plain = input(url, site);
 plain.profile = None;
 plain.freshness_s = Some(3_600);
 let result = env
.pipeline
.fetch_page(plain)
.await
.expect("the unauthenticated fetch succeeds");
 println!(
 "EVIDENCE b5b unauthenticated cached={} hits={}",
 result.payload.cached,
 server.hits.lock().unwrap().len()
);
 assert!(
 !result.payload.cached,
 "an unauthenticated call must not read the authenticated cache entry"
);
 assert!(
 !result.payload.authenticated,
 "and the payload is not labelled authenticated"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[test]
fn the_new_auth_knobs_have_safe_defaults_and_bounds() {
 let config = Config::default();
 assert!(!config.auth.persist);
 assert!(!config.auth.index_authenticated);
 assert!(!config.auth.events_authenticated);
 assert_eq!(config.auth.page_retention_days, 7);

 let mut bad = Config::default();
 bad.auth.page_retention_days = 0;
 assert!(bad.validate().is_err());
 bad.auth.page_retention_days = 3_651;
 assert!(bad.validate().is_err());
 let _ = BTreeMap::<String, String>::new();
}

#[tokio::test]
async fn authenticated_pages_follow_the_shorter_retention_window() {
 let site = "site-b5b-retention";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 env.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 // Age the stored authenticated page past the 7-day window.
 let old = (chrono::Utc::now() - chrono::Duration::days(8))
.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
 env.db()
.execute(
 "UPDATE pages SET last_fetched = ?1 WHERE authenticated = 1",
 [old])
.unwrap();
 let report = env
.store
.gc(web_widow::config::RetentionConfig::default(), 90, 7)
.await
.expect("retention pass");
 println!(
 "EVIDENCE b5b retention auth_pages_pruned={}",
 report.auth_pages_pruned
);
 assert_eq!(report.auth_pages_pruned, 1);
 assert_eq!(page_count(&env), 0);
 let _ = SessionStore::new(auth_dir).logout(site);
}

/// The blob/CAS invariant: **every** `blobs` row has its file, and an
/// authenticated persisted page has none (extraction-only). A mixed run proves
/// both halves at once.
#[tokio::test]
async fn every_blobs_row_has_its_cas_file_after_a_mixed_run() {
 let site = "site-b5b-invariant";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 // Authenticated (extraction-only).
 env.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 // Unauthenticated (writes a real CAS object).
 let mut public = input(format!("http://127.0.0.1:{}/public", server.port), site);
 public.profile = None;
 env.pipeline
.fetch_page(public)
.await
.expect("the public fetch succeeds");

 let connection = env.db();
 let rows: Vec<(String, Option<String>)> = {
 let mut statement = connection
.prepare(
 "SELECT hash, (SELECT raw_hash FROM pages WHERE url LIKE '%/public') FROM blobs")
.unwrap();
 let mapped = statement
.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap();
 mapped.collect::<rusqlite::Result<Vec<_>>>().unwrap()
 };
 println!(
 "EVIDENCE b5b invariant blobs_rows={} public_raw_hash={:?}",
 rows.len(),
 rows.first().and_then(|(_, raw)| raw.clone())
);
 assert!(
 !rows.is_empty(),
 "the unauthenticated path writes a blob row"
);
 for (hash, _) in &rows {
 let path = env.store.blobs.path_for(hash).expect("a valid stored hash");
 assert!(
 path.is_file(),
 "every blobs row has its CAS file: {hash} -> {}",
 path.display()
);
 }
 // The authenticated page has no row and no raw_hash.
 let (authenticated, raw_hash): (i64, Option<String>) = connection
.query_row(
 "SELECT authenticated, raw_hash FROM pages WHERE url LIKE '%/page'",
 [],
 |row| Ok((row.get(0)?, row.get(1)?)))
.unwrap();
 assert_eq!(authenticated, 1);
 assert!(raw_hash.is_none(), "extraction-only: no CAS reference");
 assert_eq!(rows.len(), 1, "only the public page has a blob");
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn retention_prunes_the_authenticated_page_without_touching_the_public_blob() {
 let site = "site-b5b-cas-retention";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);
 env.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("the session fetch succeeds");
 let mut public = input(format!("http://127.0.0.1:{}/public", server.port), site);
 public.profile = None;
 env.pipeline
.fetch_page(public)
.await
.expect("the public fetch succeeds");
 let public_hash: String = env
.db()
.query_row(
 "SELECT raw_hash FROM pages WHERE url LIKE '%/public'",
 [],
 |row| row.get(0))
.unwrap();
 let public_path = env.store.blobs.path_for(&public_hash).unwrap();
 assert!(public_path.is_file());

 // Age only the authenticated page past the 7-day window.
 let old = (chrono::Utc::now() - chrono::Duration::days(8))
.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
 env.db()
.execute(
 "UPDATE pages SET last_fetched = ?1 WHERE authenticated = 1",
 [old])
.unwrap();
 let report = env
.store
.gc(web_widow::config::RetentionConfig::default(), 90, 7)
.await
.expect("retention pass");
 assert_eq!(report.auth_pages_pruned, 1);
 assert!(
 env.db()
.query_row(
 "SELECT COUNT(*) FROM pages WHERE authenticated = 1",
 [],
 |row| { row.get::<_, i64>(0) }
)
.unwrap()
 == 0,
 "the authenticated page is pruned (its extraction cascades)"
);
 // `blob_get` for the authenticated page was never possible: no hash exists.
 assert!(
 env.store.blobs.read(&public_hash).await.unwrap().is_some(),
 "the unrelated public CAS object still reads"
);
 assert!(
 public_path.is_file(),
 "the public CAS file survives the authenticated page's prune"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

/// repair: a requested persistence that cannot commit is a typed error, not
/// a success with a missing labelled row. The synthetic trigger fails the
/// `record_fetch` transaction; the response must not be returned.
#[tokio::test]
async fn a_persist_store_failure_is_a_typed_error_and_writes_nothing() {
 let site = "site-b5b-fail";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, true, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);

 let connection = env.db();
 connection
.execute_batch(
 "CREATE TRIGGER m10_auth_persist_fail BEFORE INSERT ON fetch_events
 BEGIN SELECT RAISE(ABORT, 'synthetic auth persist failure'); END;")
.unwrap();

 let failed = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await;
 let error = match failed {
 Ok(result) => panic!(
 "the authenticated fetch must fail, got status {}",
 result.payload.status
),
 Err(error) => error,
 };
 assert!(
 matches!(error, web_widow::WidowError::Internal(_)),
 "a persist failure is a typed error: {error}"
);

 let count = |table: &str| -> i64 {
 connection
.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
 row.get(0)
 })
.unwrap()
 };
 assert_eq!(count("pages"), 0, "the page write rolled back");
 assert_eq!(count("fetch_events"), 0, "no failure record on this path");
 assert_eq!(count("extractions"), 0);
 assert_eq!(count("blobs"), 0);
 connection
.execute_batch("DROP TRIGGER m10_auth_persist_fail;")
.unwrap();

 // Recovery: the same authenticated call persists once the fault is gone.
 let recovered = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("recovery after the injected failure");
 assert!(recovered.payload.authenticated);
 assert_eq!(count("pages"), 1);
 let _ = SessionStore::new(auth_dir).logout(site);
}

/// The return-only path writes nothing, so an injected store failure cannot
/// affect it: the current-call metadata still comes back and no row appears.
#[tokio::test]
async fn a_store_failure_does_not_fail_the_return_only_path() {
 let site = "site-b5b-off-fail";
 let server = start_server();
 let env = common::TestEnv::new(session_config(site, server.port, false, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(&auth_dir, site);

 let connection = env.db();
 connection
.execute_batch(
 "CREATE TRIGGER m10_off_persist_fail BEFORE INSERT ON fetch_events
 BEGIN SELECT RAISE(ABORT, 'synthetic persist failure'); END;")
.unwrap();
 let result = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site))
.await
.expect("return-only auth writes nothing, so it cannot fail here");
 assert!(result.payload.authenticated);
 assert!(result.payload.fetched_at == *result.payload.validated_at.as_ref().unwrap());
 assert_eq!(page_count(&env), 0);
 connection
.execute_batch("DROP TRIGGER m10_off_persist_fail;")
.unwrap();
 let _ = SessionStore::new(auth_dir).logout(site);
}
