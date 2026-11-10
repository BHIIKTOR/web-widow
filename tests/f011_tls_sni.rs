//! TLS/SNI under DNS pinning (decree: normal hostname verification).
//!
//! Only built with the `http` feature (it needs the test TLS material). One test
//! in its own binary so the process-scoped `SSL_CERT_FILE` it sets cannot affect
//! another test: the trust root is the generated test CA, never a relaxed
//! verifier (`danger_accept_invalid_certs` appears nowhere).
//!
//! Asserts: (a) `https://pinned.test:<port>/page` succeeds with normal hostname
//! verification, (b) the same server reached as `wrong.test` FAILS verification,
//! (c) the injected resolver is consulted exactly once, (d) no rebind address is
//! contacted.

#![cfg(feature = "http")]

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use web_widow::fetch::content;
use web_widow::fetch::http::{ACCEPT_PAGE, Conditional, FetchRequest, HttpClient};
use web_widow::fetch::ssrf::{AllowList, Resolver, SsrfGate};

const PINNED: &str = "pinned.test";
const WRONG: &str = "wrong.test";

fn material(dir: &std::path::Path) -> (Arc<ServerConfig>, std::path::PathBuf) {
 let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
 ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
 ca_params
.distinguished_name
.push(DnType::CommonName, "f011 test CA");
 let ca_key = KeyPair::generate().unwrap();
 let ca = ca_params.self_signed(&ca_key).unwrap();

 let mut leaf_params = CertificateParams::new(vec![PINNED.to_string()]).unwrap();
 leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
 let leaf_key = KeyPair::generate().unwrap();
 let leaf = leaf_params
.signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))
.unwrap();

 let ca_path = dir.join("ca.pem");
 std::fs::write(&ca_path, ca.pem()).unwrap();
 let config = ServerConfig::builder()
.with_no_client_auth()
.with_single_cert(
 vec![CertificateDer::from(leaf.der().to_vec())],
 PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())))
.unwrap();
 (Arc::new(config), ca_path)
}

fn serve(config: Arc<ServerConfig>) -> u16 {
 let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
 let port = listener.local_addr().unwrap().port();
 std::thread::spawn(move || {
 for stream in listener.incoming().flatten() {
 let config = Arc::clone(&config);
 std::thread::spawn(move || {
 let Ok(tls) = rustls::ServerConnection::new(config) else {
 return;
 };
 let mut stream = rustls::StreamOwned::new(tls, stream);
 let _ = stream.flush();
 // Handshake + one request.
 let mut buffer = [0u8; 2048];
 if let Ok(n) = stream.read(&mut buffer) {
 if n == 0 {
 return;
 }
 }
 let body = "<html><body><article><h1>tls</h1><p>pinned tls page</p></article></body></html>";
 let response = format!(
 "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
 body.len(),
 body
);
 let _ = stream.write_all(response.as_bytes());
 let _ = stream.flush();
 });
 }
 });
 port
}

/// Resolver pinning both names to 127.0.0.1 (the fixture); counts calls.
fn pinned_resolver() -> (Resolver, Arc<Mutex<BTreeMap<String, usize>>>) {
 let calls: Arc<Mutex<BTreeMap<String, usize>>> = Arc::new(Mutex::new(BTreeMap::new()));
 let counter = Arc::clone(&calls);
 let resolver: Resolver = Arc::new(move |host: String, _port: u16| {
 let call = {
 let mut calls = counter.lock().unwrap();
 let entry = calls.entry(host).or_insert(0);
 *entry += 1;
 *entry
 };
 // A second resolution for the same name would return the rebind target;
 // the pinned client never asks twice.
 let addr: IpAddr = if call == 1 {
 "127.0.0.1".parse().unwrap()
 } else {
 "127.0.0.2".parse().unwrap()
 };
 Box::pin(async move { Ok(vec![addr]) })
 });
 (resolver, calls)
}

fn client(resolver: Resolver) -> HttpClient {
 let config = common::test_config();
 let allow = AllowList::parse(&[
 "127.0.0.0/8".to_string(),
 PINNED.to_string(),
 WRONG.to_string(),
 ])
.unwrap();
 let ssrf = Arc::new(SsrfGate::with_resolver(allow, resolver));
 let gate = Arc::new(web_widow::fetch::gate::HostGate::new(
 web_widow::fetch::gate::PolitenessConfig {
 min_interval: std::time::Duration::from_millis(1),
 jitter: std::time::Duration::from_millis(0),
 max_concurrent: 4,
 }));
 HttpClient::new(&config.fetch, ssrf, gate, config.user_agent()).unwrap()
}

fn request(url: &str) -> FetchRequest {
 FetchRequest {
 url: url.parse().unwrap(),
 conditional: Conditional::default(),
 accept: ACCEPT_PAGE,
 max_body_bytes: 1 << 20,
 hop_guard: None,
 min_interval: None,
 scope: content::ContentScope::Page,
 auth: None,
 auth_hosts: None,
 session: None,
 }
}

#[tokio::test]
async fn https_pinning_keeps_normal_hostname_verification() {
 let temp = tempfile::tempdir().unwrap();
 let (config, ca_path) = material(temp.path());
 let port = serve(config);
 // Process-scoped test trust root: the verifier must use it like any operator
 // CA. No certificate-acceptance relaxation exists in this or any shipped path.
 // SAFETY: this test binary holds exactly one test.
 unsafe {
 std::env::set_var("SSL_CERT_FILE", &ca_path);
 }

 let rebind = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
 let rebind_hits = Arc::new(AtomicUsize::new(0));
 let hits = Arc::clone(&rebind_hits);
 tokio::spawn(async move {
 while let Ok(_stream) = rebind.accept().await {
 hits.fetch_add(1, Ordering::SeqCst);
 }
 });
 let (resolver, calls) = pinned_resolver();
 let http = client(resolver);

 let ok = http
.fetch(request(&format!("https://{PINNED}:{port}/page")))
.await
.expect("the pinned host verifies against the test CA");
 assert_eq!(ok.status, 200);

 let wrong = http
.fetch(request(&format!("https://{WRONG}:{port}/page")))
.await
.expect_err("a certificate for another host must fail verification");
 let code = wrong.code().as_str();
 let text = wrong.to_string();
 assert!(
 code == "tls_error" || code == "dns_error" || code == "transport_error",
 "expected a TLS failure, got {code}: {wrong}"
);
 assert!(
 text.contains("certificate not valid for name") || text.contains("certificate"),
 "the failure must be the hostname check, got: {text}"
);

 let calls = calls.lock().unwrap().clone();
 assert_eq!(
 calls.get(PINNED),
 Some(&1),
 "one resolution for the pinned hop: {calls:?}"
);
 assert_eq!(
 calls.get(WRONG),
 Some(&1),
 "one resolution for the wrong-host hop: {calls:?}"
);
 assert_eq!(
 rebind_hits.load(Ordering::SeqCst),
 0,
 "no rebind address was ever dialled"
);
}
