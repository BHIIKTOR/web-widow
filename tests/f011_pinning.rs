//! DNS-rebind pinning at the vet boundary ().
//!
//! Every test injects a deterministic resolver into [`SsrfGate`], so no real DNS
//! is consulted and no test reaches a real site. The loopback fixtures record what
//! they were asked for, so a pass cannot come from the wrong address.

mod common;

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use web_widow::fetch::content::ContentScope;
use web_widow::fetch::http::{ACCEPT_PAGE, Conditional, FetchRequest, HttpClient};
use web_widow::fetch::ssrf::{AllowList, Resolver, SsrfGate};

/// A resolver that returns each queued address list in turn (the last repeats)
/// and counts how many times it was consulted.
fn scripted(addrs: Vec<Vec<IpAddr>>) -> (Resolver, Arc<Mutex<usize>>) {
 let calls = Arc::new(Mutex::new(0usize));
 let counter = Arc::clone(&calls);
 let resolver: Resolver = Arc::new(move |_host: String, _port: u16| {
 let index = {
 let mut calls = counter.lock().unwrap();
 let index = *calls;
 *calls += 1;
 index
 };
 let list = addrs
.get(index)
.or_else(|| addrs.last())
.cloned()
.unwrap_or_default();
 Box::pin(async move { Ok(list) })
 });
 (resolver, calls)
}

fn loopback(addr: &str) -> IpAddr {
 addr.parse().unwrap()
}

async fn client_with(resolver: Resolver) -> HttpClient {
 let config = common::test_config();
 // 127.0.0.0/8 is the fixture's own neighbourhood; the rebind target lives at
 // 127.0.0.2 so a second address is a genuinely different peer.
 let allow = AllowList::parse(&[
 "127.0.0.0/8".to_string(),
 "rebind.test".to_string(),
 "hop1.test".to_string(),
 "hop2.test".to_string(),
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
 scope: ContentScope::Page,
 auth: None,
 auth_hosts: None,
 session: None,
 }
}

/// A minimal server that answers any Host header (the fixtures used elsewhere
/// key on `Host`, which a synthetic name cannot satisfy). `/page` is 200 and
/// `/redirect?to=…` is a 302.
async fn any_host_server() -> (u16, Arc<Mutex<Vec<String>>>) {
 use axum::Router;
 use axum::extract::Query;
 use axum::response::Redirect;
 use axum::routing::get;

 let seen = Arc::new(Mutex::new(Vec::new()));
 let log = Arc::clone(&seen);
 let app = Router::new()
.route(
 "/page",
 get(move |uri: axum::http::Uri| {
 let log = Arc::clone(&log);
 async move {
 log.lock()
.unwrap()
.push(format!("page host={:?}", uri.host()));
 "<html><body><article><h1>f011</h1><p>pinned page</p></article></body></html>"
 }
 }))
.route(
 "/redirect",
 get(
 |Query(params): Query<std::collections::HashMap<String, String>>| async move {
 Redirect::temporary(params.get("to").map(String::as_str).unwrap_or("/page"))
 }));
 let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
 let port = listener.local_addr().unwrap().port();
 tokio::spawn(async move {
 let _ = axum::serve(listener, app).await;
 });
 (port, seen)
}

/// A loopback listener that records whether anything arrived on it.
async fn silent_listener() -> (u16, Arc<AtomicUsize>) {
 let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
 let port = listener.local_addr().unwrap().port();
 let hits = Arc::new(AtomicUsize::new(0));
 let counter = Arc::clone(&hits);
 tokio::spawn(async move {
 while let Ok(_stream) = listener.accept().await {
 counter.fetch_add(1, Ordering::SeqCst);
 }
 });
 (port, hits)
}

#[tokio::test]
async fn a_rebind_address_is_never_contacted_and_the_resolver_runs_once() {
 let (fixture_port, _seen) = any_host_server().await;
 let (rebind_port, rebind_hits) = silent_listener().await;
 let (resolver, calls) = scripted(vec![
 vec![loopback("127.0.0.1")],
 vec![loopback("127.0.0.2")],
 ]);
 let client = client_with(resolver).await;
 let url = format!("http://rebind.test:{}/page", fixture_port);
 // `rebind.test` is allowlisted by name, so the gate resolves and pins it.
 let response = client
.fetch(request(&url))
.await
.expect("the pinned first address answers");
 assert_eq!(response.status, 200);
 assert_eq!(
 *calls.lock().unwrap(),
 1,
 "exactly one resolution for the hop"
);
 assert_eq!(
 rebind_hits.load(Ordering::SeqCst),
 0,
 "the rebind target is never contacted"
);
 let _ = rebind_port;
}

#[tokio::test]
async fn the_pinned_positive_control_answers() {
 let (fixture_port, seen) = any_host_server().await;
 let (resolver, calls) = scripted(vec![vec![loopback("127.0.0.1")]]);
 let client = client_with(resolver).await;
 let response = client
.fetch(request(&format!(
 "http://rebind.test:{}/page",
 fixture_port
)))
.await
.expect("pinned host answers");
 assert_eq!(response.status, 200);
 assert_eq!(*calls.lock().unwrap(), 1);
 assert!(
 !seen.lock().unwrap().is_empty(),
 "the pinned server was asked"
);
}

#[tokio::test]
async fn a_redirect_hop_uses_its_own_pin_and_not_a_rebind_address() {
 let (fixture_port, _seen1) = any_host_server().await;
 let (hop2_port, seen2) = any_host_server().await;
 let (rebind_port, rebind_hits) = silent_listener().await;
 // hop1.test -> fixture 1; hop2.test -> hop2 fixture; a *second* resolution for
 // either name would return the rebind address, which must never be dialled.
 let (resolver, calls) = scripted(vec![
 vec![loopback("127.0.0.1")],
 vec![loopback("127.0.0.1")],
 vec![loopback("127.0.0.2")],
 ]);
 let client = client_with(resolver).await;
 let url = format!(
 "http://hop1.test:{}/redirect?to=http://hop2.test:{}/page",
 fixture_port, hop2_port
);
 let response = client.fetch(request(&url)).await.expect("both hops answer");
 assert_eq!(response.status, 200);
 assert_eq!(response.redirects, 1);
 assert!(!seen2.lock().unwrap().is_empty(), "hop 2 used its own pin");
 assert_eq!(rebind_hits.load(Ordering::SeqCst), 0);
 assert_eq!(
 *calls.lock().unwrap(),
 2,
 "one resolution per hop, none extra"
);
 let _ = rebind_port;
}

#[tokio::test]
async fn an_allowlisted_host_is_pinned_at_the_vet_boundary() {
 //: the host-name allowlist no longer short-circuits; it resolves and
 // pins like any other host. A mid-flight change cannot move the connection.
 let (fixture_port, _seen) = any_host_server().await;
 let (resolver, calls) = scripted(vec![
 vec![loopback("127.0.0.1")],
 vec![loopback("127.0.0.2")],
 ]);
 let allow = AllowList::parse(&["pinned.test".to_string(), "127.0.0.0/8".to_string()]).unwrap();
 let gate = SsrfGate::with_resolver(allow, resolver);
 let url: url::Url = format!("http://pinned.test:{}/page", fixture_port)
.parse()
.unwrap();
 let peer = gate.vet(&url).await.expect("allowlisted host vets");
 assert!(!peer.addresses.is_empty(), "allowlisted hosts are pinned");
 assert_eq!(peer.addresses, vec![loopback("127.0.0.1")]);
 assert_eq!(peer.allowed_by.as_deref(), Some("pinned.test"));
 assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_empty_vetted_set_fails_closed_without_a_second_lookup() {
 let (fixture_port, _seen) = any_host_server().await;
 let (resolver, calls) = scripted(vec![Vec::new()]);
 let client = client_with(resolver).await;
 let error = client
.fetch(request(&format!(
 "http://rebind.test:{}/page",
 fixture_port
)))
.await
.expect_err("no vetted address is a failure, not a fallback");
 assert_eq!(error.code().as_str(), "dns_error", "{error}");
 assert_eq!(
 *calls.lock().unwrap(),
 1,
 "no second lookup at connect time"
);
}
