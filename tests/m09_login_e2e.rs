//! hermetic end-to-end assisted login over a loopback HTTPS fixture.
//!
//! The fixture is deliberately small and fully local: three TLS listeners
//! (binding host, an SSO/IdP host, an undeclared host) with a test CA. Chrome is
//! given the fixture trust anchor and resolver rules as **test-only** launch
//! arguments; the production launch line is asserted in `auth::pipe`'s tests.
//! The real keyring seals the jar, so the capture proof includes an actual
//! decrypt-and-inspect.
//!
//! Each test uses its own site name: the keyring key is per site, not per data
//! directory, so parallel tests must not share one.
//!
//! Run with `--features http`.

#![cfg(feature = "http")]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcgen::{
 BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use web_widow::auth::login::{Decision, Login, Operator};
use web_widow::auth::store::SessionStore;
use web_widow::config::{AuthKind, AuthProfileConfig, Config};
use web_widow::core::error::Result;

const BINDING_HOST: &str = "login.example.test";

struct Scripted(Decision);

impl Operator for Scripted {
 fn confirm(&self, _origin: &str) -> Result<Decision> {
 Ok(self.0)
 }
}

type Handler = Arc<dyn Fn(&str, &str) -> (u16, Vec<String>, String) + Send + Sync>;

struct Servers {
 binding: u16,
 idp: u16,
 ca_path: PathBuf,
 evil_hits: Arc<AtomicUsize>,
 idp_hits: Arc<AtomicUsize>,
 _dir: tempfile::TempDir,
}

fn test_material(dir: &Path) -> (Arc<ServerConfig>, PathBuf) {
 let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
 ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
 ca_params
.distinguished_name
.push(DnType::CommonName, "web-widow login test CA");
 let ca_key = KeyPair::generate().unwrap();
 let ca_cert = ca_params.self_signed(&ca_key).unwrap();

 let mut leaf_params = CertificateParams::new(vec![
 BINDING_HOST.to_string(),
 "idp.test".to_string(),
 "evil.test".to_string(),
 "127.0.0.1".to_string(),
 "127.0.0.2".to_string(),
 "127.0.0.3".to_string(),
 ])
.unwrap();
 leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
 let leaf_key = KeyPair::generate().unwrap();
 let leaf = leaf_params
.signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))
.unwrap();

 let ca_path = dir.join("ca.pem");
 std::fs::write(&ca_path, ca_cert.pem()).unwrap();

 let config = ServerConfig::builder()
.with_no_client_auth()
.with_single_cert(
 vec![CertificateDer::from(leaf.der().to_vec())],
 PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())))
.unwrap();
 (Arc::new(config), ca_path)
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor, handler: Handler) {
 loop {
 let Ok((tcp, _)) = listener.accept().await else {
 return;
 };
 let acceptor = acceptor.clone();
 let handler = Arc::clone(&handler);
 tokio::spawn(async move {
 let Ok(mut tls) = acceptor.accept(tcp).await else {
 return;
 };
 let mut buffer = vec![0u8; 16 * 1024];
 let Ok(read) = tls.read(&mut buffer).await else {
 return;
 };
 let request = String::from_utf8_lossy(&buffer[..read]).to_string();
 let path = request
.lines()
.next()
.and_then(|line| line.split_whitespace().nth(1))
.unwrap_or("/")
.to_string();
 let (status, headers, body) = handler(&path, &request);
 let mut response = format!(
 "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
 body.len()
);
 for header in headers {
 response.push_str(&header);
 response.push_str("\r\n");
 }
 response.push_str("\r\n");
 response.push_str(&body);
 let _ = tls.write_all(response.as_bytes()).await;
 let _ = tls.shutdown().await;
 });
 }
}

async fn start_servers() -> Servers {
 let dir = tempfile::tempdir().unwrap();
 let (config, ca_path) = test_material(dir.path());
 let acceptor = TlsAcceptor::from(config);
 let evil_hits = Arc::new(AtomicUsize::new(0));
 let idp_hits = Arc::new(AtomicUsize::new(0));

 // Bind first so the redirect routes can name the other listeners.
 let binding = TcpListener::bind("127.0.0.1:0").await.unwrap();
 let binding_port = binding.local_addr().unwrap().port();
 let idp = TcpListener::bind("127.0.0.2:0").await.unwrap();
 let idp_port = idp.local_addr().unwrap().port();
 let evil = TcpListener::bind("127.0.0.3:0").await.unwrap();
 let evil_port = evil.local_addr().unwrap().port();

 let binding_handler: Handler = {
 let idp = idp_port;
 let evil = evil_port;
 Arc::new(move |path: &str, request: &str| match path {
 "/login" => (
 200,
 vec![
 "Set-Cookie: ww_session=abc123; Path=/; Secure; HttpOnly; SameSite=Lax"
.to_string(),
 "Set-Cookie: ww_parent=1; Domain=example.test; Path=/; Secure".to_string(),
 "Content-Type: text/html".to_string(),
 ],
 "<html><body>sign in</body></html>".to_string()),
 "/verify" => {
 if request.contains("ww_session=abc123") {
 (
 200,
 vec!["Content-Type: text/plain".to_string()],
 "ok".to_string())
 } else {
 (
 401,
 vec!["Content-Type: text/plain".to_string()],
 "no".to_string())
 }
 }
 "/verify-fail" => (
 401,
 vec!["Content-Type: text/plain".to_string()],
 "no".to_string()),
 "/go-evil" => (
 302,
 vec![format!("Location: https://evil.test:{evil}/")],
 String::new()),
 "/go-idp" => (
 302,
 vec![format!("Location: https://idp.test:{idp}/idp")],
 String::new()),
 _ => (404, vec!["Content-Length: 0".to_string()], String::new()),
 })
 };
 tokio::spawn(serve(binding, acceptor.clone(), binding_handler));

 let idp_handler: Handler = {
 let hits = Arc::clone(&idp_hits);
 let binding = binding_port;
 Arc::new(move |path: &str, _request: &str| {
 hits.fetch_add(1, Ordering::SeqCst);
 if path == "/idp-frame" {
 // F4: a declared top-level page that embeds the binding host.
 (
 200,
 vec!["Content-Type: text/html".to_string()],
 format!(
 "<html><body>idp<iframe src=\"https://{BINDING_HOST}:{binding}/\" width=\"200\" height=\"60\"></iframe></body></html>"
))
 } else if path == "/idp" {
 (
 200,
 vec![
 "Set-Cookie: idp_session=zzz; Path=/; Secure".to_string(),
 "Content-Type: text/html".to_string(),
 ],
 "<html><body>identity provider</body></html>".to_string())
 } else {
 (404, vec!["Content-Length: 0".to_string()], String::new())
 }
 })
 };
 tokio::spawn(serve(idp, acceptor.clone(), idp_handler));

 let evil_handler: Handler = {
 let hits = Arc::clone(&evil_hits);
 Arc::new(move |_path: &str, _request: &str| {
 hits.fetch_add(1, Ordering::SeqCst);
 (
 200,
 vec!["Content-Type: text/plain".to_string()],
 "evil".to_string())
 })
 };
 tokio::spawn(serve(evil, acceptor, evil_handler));

 Servers {
 binding: binding_port,
 idp: idp_port,
 ca_path,
 evil_hits,
 idp_hits,
 _dir: dir,
 }
}

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

fn session_config(site: &str, binding_port: u16, verify_path: &str) -> Config {
 let mut config = Config::default();
 config.auth.profiles.insert(
 site.to_string(),
 AuthProfileConfig {
 kind: AuthKind::Session,
 host: BINDING_HOST.to_string(),
 login_url: Some(format!("https://{BINDING_HOST}:{binding_port}/login")),
 login_origins: vec![BINDING_HOST.to_string(), "idp.test".to_string()],
 verify_url: Some(format!(
 "https://{BINDING_HOST}:{binding_port}{verify_path}"
)),
 assets: Vec::new(),
 session_max_age_days: Some(7),
..Default::default()
 });
 config
}

fn login_for(config: &Config, servers: &Servers, data: &Path, site: &str) -> Login {
 let Some(chrome) = common::chrome_for_tests() else {
 panic!("this evidence test needs a Chrome binary");
 };
 let mut login = Login::from_config(config, site).expect("session profile");
 login = login.with_browser(chrome, data.to_path_buf());
 login.verify_root_ca = Some(servers.ca_path.clone());
 login.verify_resolve = Some((
 BINDING_HOST.to_string(),
 SocketAddr::from(([127, 0, 0, 1], servers.binding))));
 let mut args = platform_args();
 args.push("--ignore-certificate-errors".to_string());
 args.push(format!(
 "--host-resolver-rules=MAP {BINDING_HOST} 127.0.0.1, MAP idp.test 127.0.0.2, MAP evil.test 127.0.0.3"
));
 login.launch_args = args;
 login
}

fn data_dir(root: &Path) -> PathBuf {
 use std::os::unix::fs::PermissionsExt;
 let data = root.join("data");
 std::fs::create_dir_all(&data).unwrap();
 std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
 data
}

fn keyring_available(data: &Path, site: &str) -> bool {
 let store = SessionStore::new(data.join("auth"));
 match store.preflight(site) {
 Ok(()) => true,
 Err(error) => {
 eprintln!("SKIP m09 login e2e: keyring unavailable: {error}");
 false
 }
 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_captures_only_the_binding_host_and_seals_a_verifiable_jar() {
 let servers = start_servers().await;
 let dir = tempfile::tempdir().unwrap();
 let data = data_dir(dir.path());
 let site = "site-capture";
 if !keyring_available(&data, site) {
 return;
 }
 let config = session_config(site, servers.binding, "/verify");
 let login = login_for(&config, &servers, &data, site);
 let summary = tokio::task::spawn_blocking(move || login.run(&Scripted(Decision::Capture)))
.await
.unwrap()
.expect("the login flow should capture and seal");

 println!(
 "EVIDENCE summary {}",
 serde_json::to_string(&summary).unwrap_or_default()
);
 assert_eq!(
 summary.cookie_count, 1,
 "only the binding-host cookie is sealed"
);
 assert_eq!(summary.verify_status, 200);
 assert!(summary.profile_wiped, "the login profile must be wiped");
 assert!(summary.browser.no_tcp_listener());
 assert!(summary.browser.devtools_active_port_absent);
 assert!(summary.refused_navigations.is_empty());
 assert!(
 servers.idp_hits.load(Ordering::SeqCst) == 0,
 "the flow never visits the IdP in this scenario"
);

 // Decrypt-and-inspect: exactly the binding-host cookie with its attributes.
 let store = SessionStore::new(data.join("auth"));
 let unsealed = store
.unseal(site)
.expect("the jar should unseal")
.expect("the jar should exist");
 let cookies: Vec<web_widow::auth::login::CapturedCookie> =
 serde_json::from_slice(&unsealed.payload).expect("jar payload");
 println!(
 "EVIDENCE decrypted meta={:?} names={:?}",
 unsealed.meta,
 cookies.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
);
 assert_eq!(cookies.len(), 1);
 assert_eq!(cookies[0].name, "ww_session");
 assert_eq!(cookies[0].domain, BINDING_HOST);
 assert!(cookies[0].secure && cookies[0].http_only);
 assert_eq!(cookies[0].same_site.as_deref(), Some("Lax"));
 assert_eq!(unsealed.meta.cookie_count, 1);
 assert_eq!(unsealed.meta.hosts, vec![BINDING_HOST.to_string()]);
 // No plaintext of the value anywhere in the sidecar or the jar.
 let raw = std::fs::read(data.join(format!("auth/{site}.sess"))).unwrap();
 assert!(
 !raw.windows(6).any(|window| window == b"abc123"),
 "the jar must not contain the plaintext cookie value"
);
 let meta = std::fs::read_to_string(data.join(format!("auth/{site}.meta"))).unwrap();
 assert!(!meta.contains("abc123"));
 assert!(!meta.contains("ww_session"));
 let _ = store.logout(site);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_navigation_outside_the_login_origins_is_refused_before_the_network() {
 let servers = start_servers().await;
 let dir = tempfile::tempdir().unwrap();
 let data = data_dir(dir.path());
 let site = "site-gate";
 if !keyring_available(&data, site) {
 return;
 }
 let mut config = session_config(site, servers.binding, "/verify");
 if let Some(profile) = config.auth.profiles.get_mut(site) {
 profile.login_url = Some(format!(
 "https://{BINDING_HOST}:{}/go-evil",
 servers.binding
));
 }
 let login = login_for(&config, &servers, &data, site);
 let error = tokio::task::spawn_blocking(move || login.run(&Scripted(Decision::Capture)))
.await
.unwrap()
.expect_err("an undeclared navigation must refuse the login");
 println!("EVIDENCE gate refusal: {error}");
 assert_eq!(error.code().as_str(), "auth_login_origin_refused");
 assert!(
 error.to_string().contains("evil.test"),
 "the refusal must name the refused host: {error}"
);
 assert_eq!(
 servers.evil_hits.load(Ordering::SeqCst),
 0,
 "the gate refuses the document before any request reaches the undeclared host"
);
 assert!(
 !data.join(format!("auth/{site}.sess")).exists(),
 "nothing may be sealed after a refused navigation"
);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_final_origin_off_the_binding_host_refuses_capture() {
 let servers = start_servers().await;
 let dir = tempfile::tempdir().unwrap();
 let data = data_dir(dir.path());
 let site = "site-final";
 if !keyring_available(&data, site) {
 return;
 }
 let mut config = session_config(site, servers.binding, "/verify");
 if let Some(profile) = config.auth.profiles.get_mut(site) {
 // A declared SSO hop: the gate allows it, but capture must not.
 profile.login_url = Some(format!("https://{BINDING_HOST}:{}/go-idp", servers.binding));
 }
 let login = login_for(&config, &servers, &data, site);
 let error = tokio::task::spawn_blocking(move || login.run(&Scripted(Decision::Capture)))
.await
.unwrap()
.expect_err("a final origin on the IdP must refuse capture");
 println!("EVIDENCE final-origin refusal: {error}");
 assert_eq!(error.code().as_str(), "auth_login_final_origin_refused");
 assert!(
 servers.idp_hits.load(Ordering::SeqCst) > 0,
 "the declared hop was allowed to load"
);
 assert!(!data.join(format!("auth/{site}.sess")).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_verify_seals_nothing() {
 let servers = start_servers().await;
 let dir = tempfile::tempdir().unwrap();
 let data = data_dir(dir.path());
 let site = "site-verify";
 if !keyring_available(&data, site) {
 return;
 }
 let config = session_config(site, servers.binding, "/verify-fail");
 let login = login_for(&config, &servers, &data, site);
 let error = tokio::task::spawn_blocking(move || login.run(&Scripted(Decision::Capture)))
.await
.unwrap()
.expect_err("a non-2xx verify must refuse to seal");
 println!("EVIDENCE verify refusal: {error}");
 assert_eq!(error.code().as_str(), "auth_verify_failed");
 assert!(!data.join(format!("auth/{site}.sess")).exists());
 assert!(!data.join(format!("auth/{site}.meta")).exists());
 assert!(
 !data.join(format!("auth/login/{site}")).exists(),
 "the profile is wiped even when the flow refuses"
);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f4_a_binding_iframe_does_not_become_the_final_origin() {
 let servers = start_servers().await;
 let dir = tempfile::tempdir().unwrap();
 let data = data_dir(dir.path());
 let site = "site-f4-frame";
 if !keyring_available(&data, site) {
 return;
 }
 let mut config = session_config(site, servers.binding, "/verify");
 if let Some(profile) = config.auth.profiles.get_mut(site) {
 // The top-level login page is on the IdP; it embeds the binding host.
 profile.login_url = Some(format!("https://idp.test:{}/idp-frame", servers.idp));
 }
 let login = login_for(&config, &servers, &data, site);
 let error = tokio::task::spawn_blocking(move || login.run(&Scripted(Decision::Capture)))
.await
.unwrap()
.expect_err("a binding iframe must not make the IdP page capturable");
 println!("EVIDENCE f4 final-origin refusal: {error}");
 assert_eq!(error.code().as_str(), "auth_login_final_origin_refused");
 assert!(
 error.to_string().contains("idp.test"),
 "the refusal names the main frame's origin: {error}"
);
 assert!(
 !data.join(format!("auth/{site}.sess")).exists(),
 "nothing may be sealed from a subframe origin"
);
}
