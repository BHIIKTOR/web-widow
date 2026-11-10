//! the sealed-session paths through the pipeline.
//!
//! Static (`render` absent/never): the exact-host `Cookie` is attached per hop,
//! any other hop is refused, a 401 invalidates the jar, and an expired/missing
//! jar is a typed refusal with no request.
//!
//! Render (`render = always`): the dedicated authenticated browser renders with
//! exact-host injection and the B1 egress policy on every request.
//!
//! The jar is sealed with the **real** keyring; the tests skip cleanly when one
//! is unavailable.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use web_widow::auth::login::CapturedCookie;
use web_widow::auth::store::{SessionMeta, SessionStore};
use web_widow::config::{AuthKind, AuthProfileConfig, Config};
use web_widow::pipeline::{FetchPageInput, RenderPolicy};

#[derive(Clone, Debug)]
struct Hit {
 path: String,
 method: String,
 cookie: Option<String>,
 at: std::time::Instant,
}

struct Server {
 port: u16,
 hits: Arc<Mutex<Vec<Hit>>>,
}

fn start_server(other_port: Option<u16>) -> Server {
 start_on("127.0.0.1:0", other_port)
}

fn start_on(address: &str, other_port: Option<u16>) -> Server {
 let listener = TcpListener::bind(address).unwrap();
 let port = listener.local_addr().unwrap().port();
 let hits = Arc::new(Mutex::new(Vec::new()));
 let recorded = Arc::clone(&hits);
 std::thread::spawn(move || {
 for stream in listener.incoming().flatten() {
 handle(stream, &recorded, other_port);
 }
 });
 Server { port, hits }
}

fn handle(stream: TcpStream, hits: &Arc<Mutex<Vec<Hit>>>, other_port: Option<u16>) {
 let mut reader = BufReader::new(stream);
 let mut line = String::new();
 if reader.read_line(&mut line).unwrap_or(0) == 0 {
 return;
 }
 let method = line.split_whitespace().next().unwrap_or("GET").to_string();
 let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
 let mut headers: BTreeMap<String, String> = BTreeMap::new();
 loop {
 let mut header = String::new();
 if reader.read_line(&mut header).unwrap_or(0) == 0 {
 break;
 }
 let trimmed = header.trim_end();
 if trimmed.is_empty() {
 break;
 }
 if let Some((name, value)) = trimmed.split_once(':') {
 headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
 }
 }
 hits.lock().unwrap().push(Hit {
 path: path.clone(),
 method,
 cookie: headers.get("cookie").cloned(),
 at: std::time::Instant::now(),
 });
 let (status, extra, body) = route(&path, other_port);
 let mut response = format!(
 "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
 body.len()
);
 response.push_str(&body);
 let mut writer = reader.into_inner();
 let _ = writer.write_all(response.as_bytes());
}

fn route(path: &str, other_port: Option<u16>) -> (u16, String, String) {
 match path {
 "/robots.txt" => (
 200,
 "Content-Type: text/plain\r\n".to_string(),
 "User-agent: *\nDisallow: /private\n".to_string()),
 "/private" => (
 200,
 "Content-Type: text/html\r\n".to_string(),
 "<html><body>private</body></html>".to_string()),
 "/post-page" => (
 200,
 "Content-Type: text/html\r\n".to_string(),
 "<html><body><div id=\"x\">seed</div><script>fetch('/api',{method:'POST'})".to_string()
 + ".catch(()=>{})</script></body></html>"),
 "/page" => (
 200,
 "Content-Type: text/html\r\n".to_string(),
 "<html><body><h1>session page</h1></body></html>".to_string()),
 "/render" => (
 200,
 "Content-Type: text/html\r\n".to_string(),
 "<html><body><div id=\"x\">seed</div><script>fetch('/api')".to_string()
 + ".then(r=>r.json()).then(d=>{document.getElementById('x').textContent=d.value})"
 + "</script></body></html>"),
 "/api" => (
 200,
 "Content-Type: application/json\r\n".to_string(),
 "{\"value\":\"from-js\"}".to_string()),
 "/401" => (
 401,
 "Content-Type: text/plain\r\n".to_string(),
 "unauthorized".to_string()),
 "/challenge" => (
 401,
 "WWW-Authenticate: Basic realm=\"ww\"\r\nContent-Type: text/plain\r\n".to_string(),
 "challenge".to_string()),
 "/login" => (
 200,
 "Content-Type: text/html\r\n".to_string(),
 "<html><body>sign in again</body></html>".to_string()),
 "/redirect-login" => (302, "Location: /login\r\n".to_string(), String::new()),
 "/redirect-other" => (
 302,
 format!(
 "Location: http://127.0.0.5:{}/page\r\n",
 other_port.unwrap_or(9)
),
 String::new()),
 other => (
 404,
 "Content-Type: text/plain\r\n".to_string(),
 format!("no route {other}")),
 }
}

fn now() -> i64 {
 SystemTime::now()
.duration_since(UNIX_EPOCH)
.unwrap()
.as_secs() as i64
}

fn session_config(site: &str, port: u16, allow_subdomains: bool) -> Config {
 let mut config = common::test_config();
 config.auth.allow_loopback_http = true;
 config.auth.profiles.insert(
 site.to_string(),
 AuthProfileConfig {
 kind: AuthKind::Session,
 host: "127.0.0.1".to_string(),
 port,
 login_url: Some("https://127.0.0.1/login".to_string()),
 login_origins: vec!["127.0.0.1".to_string()],
 verify_url: Some("https://127.0.0.1/verify".to_string()),
 allow_subdomains,
 session_max_age_days: Some(7),
..Default::default()
 });
 config
}

fn cookie(name: &str, value: &str, domain: &str, expires: Option<i64>) -> CapturedCookie {
 CapturedCookie {
 name: name.to_string(),
 value: zeroize::Zeroizing::new(value.to_string()),
 domain: domain.to_string(),
 path: "/".to_string(),
 secure: false,
 http_only: false,
 same_site: None,
 expires,
 }
}

fn seal(
 dir: &std::path::Path,
 site: &str,
 cookies: &[CapturedCookie],
 expires_at_min: Option<i64>) {
 let store = SessionStore::new(dir.to_path_buf());
 let payload = serde_json::to_vec(cookies).unwrap();
 let meta = SessionMeta {
 captured_at: now(),
 expires_at_min,
 cookie_count: cookies.len(),
 hosts: vec!["127.0.0.1".to_string()],
 verify_url: None,
 };
 store.seal(site, &payload, &meta).expect("seal");
}

fn keyring_available(data: &std::path::Path, site: &str) -> bool {
 match SessionStore::new(data.to_path_buf()).preflight(site) {
 Ok(()) => true,
 Err(error) => {
 eprintln!("SKIP m09 session reuse: keyring unavailable: {error}");
 false
 }
 }
}

fn input(url: String, site: &str, render: Option<RenderPolicy>) -> FetchPageInput {
 FetchPageInput {
 url,
 mode: None,
 render,
 freshness_s: None,
 max_chars: None,
 include_links: None,
 include_content: None,
 profile: Some(site.to_string()),
 }
}

#[tokio::test]
async fn the_static_session_path_attaches_the_exact_host_cookie() {
 let site = "site-static";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let result = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site,
 None))
.await
.expect("the authenticated static fetch should succeed");
 assert!(result.payload.authenticated);
 assert_eq!(result.payload.status, 200);
 let hits = server.hits.lock().unwrap().clone();
 let page = hits
.iter()
.find(|hit| hit.path == "/page")
.expect("page hit");
 assert_eq!(
 page.cookie.as_deref(),
 Some("ww=abc"),
 "the exact-host cookie must be attached"
);
 // Audit: exactly one value-free row with outcome ok.
 assert_eq!(count_audit(&env, "ok"), 1, "one ok audit row");
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn a_missing_jar_is_session_missing_and_makes_no_request() {
 let site = "site-missing";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 if !keyring_available(&env.store.paths().auth_dir(), site) {
 return;
 }
 let _ = SessionStore::new(env.store.paths().auth_dir()).logout(site);
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site,
 None))
.await
.expect_err("no jar must refuse");
 assert_eq!(error.code().as_str(), "auth_session_missing");
 assert!(
 server.hits.lock().unwrap().is_empty(),
 "no request may be made"
);
 assert_eq!(count_audit(&env, "session_missing"), 1);
}

#[tokio::test]
async fn an_expired_jar_is_destroyed_and_reported() {
 let site = "site-expired";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() - 10))],
 Some(now() - 10));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site,
 None))
.await
.expect_err("an expired jar must refuse");
 assert_eq!(error.code().as_str(), "auth_session_expired");
 assert!(server.hits.lock().unwrap().is_empty());
 let status = SessionStore::new(auth_dir).status(site).expect("status");
 assert!(!status.present, "the expired jar must be destroyed");
 assert_eq!(count_audit(&env, "session_expired"), 1);
}

#[tokio::test]
async fn a_401_invalidates_the_jar() {
 let site = "site-401";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/401", server.port),
 site,
 None))
.await
.expect_err("a 401 must end the session");
 assert_eq!(error.code().as_str(), "auth_session_expired");
 assert!(
 !SessionStore::new(auth_dir.clone())
.status(site)
.unwrap()
.present,
 "the jar is invalidated after a 401"
);
 assert_eq!(count_audit(&env, "session_expired"), 1);
}

#[tokio::test]
async fn a_cross_origin_redirect_is_refused_before_the_second_host() {
 let site = "site-redirect";
 let other = start_on("127.0.0.5:0", None);
 let server = start_server(Some(other.port));
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/redirect-other", server.port),
 site,
 None))
.await
.expect_err("a redirect off the origin must refuse");
 assert_eq!(error.code().as_str(), "egress_blocked");
 assert!(
 other.hits.lock().unwrap().is_empty(),
 "the other origin must never be contacted"
);
 assert_eq!(count_audit(&env, "egress_blocked"), 1);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn a_redirect_to_the_login_page_ends_the_session() {
 let site = "site-login-redirect";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/redirect-login", server.port),
 site,
 None))
.await
.expect_err("a redirect to the login page must end the session");
 assert_eq!(error.code().as_str(), "auth_session_expired");
 assert!(
 !SessionStore::new(auth_dir.clone())
.status(site)
.unwrap()
.present,
 "the jar is invalidated when the site asks for a login"
);
 assert_eq!(count_audit(&env, "session_expired"), 1);
}

#[tokio::test]
async fn the_authenticated_render_uses_an_ephemeral_context_and_injects_the_cookie() {
 let Some(chrome) = common::skip_without_chrome("the_authenticated_render") else {
 return;
 };
 let site = "site-render";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = Some(chrome);
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let result = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/render", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect("the authenticated render should succeed");
 assert_eq!(result.payload.render_mode, "rendered");
 assert!(
 result.markdown.contains("from-js"),
 "page JS ran in the authenticated browser: {}",
 result.markdown
);
 let hits = server.hits.lock().unwrap().clone();
 assert!(
 hits.iter()
.any(|hit| hit.path == "/render" && hit.cookie.as_deref() == Some("ww=abc")),
 "the document carried the exact-host cookie: {hits:?}"
);
 assert!(
 hits.iter()
.any(|hit| hit.path == "/api" && hit.cookie.as_deref() == Some("ww=abc")),
 "the subresource carried the cookie through the gate: {hits:?}"
);
 //: the render profile is not left behind.
 assert!(
 !env.store.paths().root().join("auth/render/site").exists(),
 "the authenticated render profile must be wiped"
);
 assert_eq!(count_audit(&env, "ok"), 1);
 let _ = SessionStore::new(auth_dir).logout(site);
}

fn count_audit(env: &common::TestEnv, outcome: &str) -> usize {
 let connection = env.db();
 let count: i64 = connection
.query_row(
 "SELECT COUNT(*) FROM auth_audit WHERE outcome = ?1",
 [outcome],
 |row| row.get(0))
.expect("auth_audit is queryable");
 count as usize
}

/// Seal a jar whose sidecar claims a different host (C4 tamper case).
fn seal_claiming_host(
 dir: &std::path::Path,
 site: &str,
 claimed_host: &str,
 cookies: &[CapturedCookie]) {
 let store = SessionStore::new(dir.to_path_buf());
 let payload = serde_json::to_vec(cookies).unwrap();
 let meta = SessionMeta {
 captured_at: now(),
 expires_at_min: Some(now() + 3600),
 cookie_count: cookies.len(),
 hosts: vec![claimed_host.to_string()],
 verify_url: None,
 };
 store.seal(site, &payload, &meta).expect("seal");
}

#[tokio::test]
async fn c1_a_robots_disallowed_render_target_is_refused_before_any_browser() {
 let Some(_chrome) = common::skip_without_chrome("c1_a_robots") else {
 return;
 };
 let site = "site-c1-robots";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/private", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect_err("robots must refuse the private path");
 // The error code is the shipped `blocked_by_robots`; the audit outcome is
 // `robots_blocked`. Both name the same refusal.
 assert_eq!(error.code().as_str(), "blocked_by_robots");
 assert!(
 !env.store.paths().root().join("auth/render").exists(),
 "no render profile may be created before the vet"
);
 assert!(
 server
.hits
.lock()
.unwrap()
.iter()
.all(|hit| hit.path != "/private"),
 "the origin must not receive the document"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn c1_b_two_renders_respect_the_lease_interval() {
 let Some(_chrome) = common::skip_without_chrome("c1_b_lease") else {
 return;
 };
 let site = "site-c1-lease";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.fetch.min_interval_ms = 1_200;
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 // Monotone property: the *second* render cannot start before the first
 // grant plus the interval, so its document cannot arrive earlier either —
 // robust to browser-startup variance under load.
 let started = std::time::Instant::now();
 for _ in 0..2 {
 let _ = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/render", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect("both renders should succeed");
 }
 let document_hits: Vec<std::time::Instant> = server
.hits
.lock()
.unwrap()
.iter()
.filter(|hit| hit.path == "/render")
.map(|hit| hit.at)
.collect();
 assert_eq!(document_hits.len(), 2, "two document arrivals");
 let second_since_start = document_hits[1].duration_since(started);
 let between = document_hits[1].duration_since(document_hits[0]);
 println!(
 "EVIDENCE c1 lease: second_doc_since_first_call={second_since_start:?} between_documents={between:?}"
);
 assert!(
 second_since_start >= std::time::Duration::from_millis(1_100),
 "the second render must start after the lease interval: {second_since_start:?}"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn c1_c_a_binding_host_resolving_to_loopback_is_refused_by_ssrf() {
 let site = "site-c1-ssrf";
 // No allowlist: a *name* that resolves to loopback must be refused, exactly
 // like a literal address.
 let mut config = common::test_config();
 config.fetch.allow_hosts = Vec::new();
 config.auth.allow_loopback_http = true;
 config.auth.profiles.insert(
 site.to_string(),
 AuthProfileConfig {
 kind: AuthKind::Session,
 host: "localhost".to_string(),
 port: 443,
 login_url: Some("https://localhost/login".to_string()),
 login_origins: vec!["localhost".to_string()],
 verify_url: Some("https://localhost/verify".to_string()),
 session_max_age_days: Some(7),
..Default::default()
 });
 let env = common::TestEnv::new(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 let store = SessionStore::new(auth_dir.clone());
 let payload =
 serde_json::to_vec(&[cookie("ww", "abc", "localhost", Some(now() + 3600))]).unwrap();
 let meta = SessionMeta {
 captured_at: now(),
 expires_at_min: Some(now() + 3600),
 cookie_count: 1,
 hosts: vec!["localhost".to_string()],
 verify_url: None,
 };
 store.seal(site, &payload, &meta).expect("seal");
 let error = env
.pipeline
.fetch_page(input(
 "https://localhost/private".to_string(),
 site,
 Some(RenderPolicy::Always)))
.await
.expect_err("a loopback-resolving host must be refused");
 println!("EVIDENCE c1 ssrf: {}", error.code().as_str());
 assert!(
 matches!(error.code().as_str(), "blocked_private_address" | "dns"),
 "expected an SSRF/DNS refusal, got {}: {error}",
 error.code().as_str()
);
 assert!(
 !env.store.paths().root().join("auth/render").exists(),
 "no render profile before the vet"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn c2_a_non_read_method_is_refused_with_zero_origin_hits() {
 let Some(_chrome) = common::skip_without_chrome("c2_method") else {
 return;
 };
 let site = "site-c2-method";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/post-page", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect_err("a POST from the page must be refused");
 assert_eq!(error.code().as_str(), "auth_method_refused");
 let hits = server.hits.lock().unwrap().clone();
 assert!(
 hits.iter().all(|hit| hit.method != "POST"),
 "no POST may reach the origin: {hits:?}"
);
 assert_eq!(count_audit(&env, "egress_blocked"), 1, "one audit row");
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn c4_a_sidecar_host_mismatch_is_refused_and_no_cookie_leaves() {
 let site = "site-c4-tamper";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 // The jar's sidecar claims another host: the destination stays the config's.
 seal_claiming_host(
 &auth_dir,
 site,
 "evil.test",
 &[cookie("ww", "abc", "evil.test", Some(now() + 3600))]);
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/page", server.port),
 site,
 None))
.await
.expect_err("a tampered sidecar must be refused");
 assert_eq!(error.code().as_str(), "auth_store_invalid");
 assert!(
 server.hits.lock().unwrap().is_empty(),
 "no request may be made and no cookie may leave"
);
 assert_eq!(count_audit(&env, "auth_store_invalid"), 1);
 let _ = SessionStore::new(auth_dir).logout(site);
}

#[tokio::test]
async fn c6_a_401_render_ends_the_session() {
 let Some(_chrome) = common::skip_without_chrome("c6_401") else {
 return;
 };
 let site = "site-c6-401";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/401", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect_err("a 401 render is a dead session");
 assert_eq!(error.code().as_str(), "auth_session_expired");
 assert!(
 !SessionStore::new(auth_dir.clone())
.status(site)
.unwrap()
.present,
 "the jar is destroyed"
);
 assert_eq!(count_audit(&env, "session_expired"), 1);
}

#[tokio::test]
async fn c6_a_login_page_render_is_not_authenticated() {
 let Some(_chrome) = common::skip_without_chrome("c6_login_page") else {
 return;
 };
 let site = "site-c6-login";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/login", server.port),
 site,
 Some(RenderPolicy::Always)))
.await
.expect_err("a 200 login page is a dead session");
 assert_eq!(error.code().as_str(), "auth_session_expired");
 assert!(
 !SessionStore::new(auth_dir.clone())
.status(site)
.unwrap()
.present,
 "the jar is destroyed"
);
}

#[tokio::test]
async fn c6_a_static_login_page_is_not_authenticated() {
 let site = "site-c6-static-login";
 let server = start_server(None);
 let env = common::TestEnv::new(session_config(site, server.port, false)).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let error = env
.pipeline
.fetch_page(input(
 format!("http://127.0.0.1:{}/login", server.port),
 site,
 None))
.await
.expect_err("a 200 login page must not be delivered as authenticated");
 assert_eq!(error.code().as_str(), "auth_session_expired");
}

#[tokio::test]
async fn f8_concurrent_renders_use_distinct_profiles() {
 let Some(_chrome) = common::skip_without_chrome("f8_concurrent") else {
 return;
 };
 let site = "site-f8";
 let server = start_server(None);
 let mut config = session_config(site, server.port, false);
 config.render.chrome_path = common::chrome_for_tests();
 config.render.discovery = false;
 let env = common::TestEnv::new_rendering(config).await;
 let auth_dir = env.store.paths().auth_dir();
 if !keyring_available(&auth_dir, site) {
 return;
 }
 seal(
 &auth_dir,
 site,
 &[cookie("ww", "abc", "127.0.0.1", Some(now() + 3600))],
 Some(now() + 3600));
 let url = format!("http://127.0.0.1:{}/render", server.port);
 let first = {
 let pipeline = std::sync::Arc::clone(&env.pipeline);
 let url = url.clone();
 std::thread::spawn(move || {
 tokio::runtime::Builder::new_current_thread()
.enable_all()
.build()
.unwrap()
.block_on(pipeline.fetch_page(input(url, "site-f8", Some(RenderPolicy::Always))))
 })
 };
 let second = {
 let pipeline = std::sync::Arc::clone(&env.pipeline);
 std::thread::spawn(move || {
 tokio::runtime::Builder::new_current_thread()
.enable_all()
.build()
.unwrap()
.block_on(pipeline.fetch_page(input(url, "site-f8", Some(RenderPolicy::Always))))
 })
 };
 let a = first.join().unwrap().expect("first concurrent render");
 let b = second.join().unwrap().expect("second concurrent render");
 assert!(a.markdown.contains("from-js") && b.markdown.contains("from-js"));
 let render_root = env.store.paths().root().join("auth/render");
 let leftovers: Vec<_> = std::fs::read_dir(&render_root)
.map(|entries| entries.flatten().map(|e| e.file_name()).collect())
.unwrap_or_default();
 assert!(
 leftovers.is_empty(),
 "both per-render profiles are removed: {leftovers:?}"
);
 let _ = SessionStore::new(auth_dir).logout(site);
}
