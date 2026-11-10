//! HTTP adapter: one client for the process, manual redirects with the gate run
//! per hop, streamed body cap, header cap, bounded retries and the error
//! taxonomy mapping.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{
 ACCEPT, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH,
 LAST_MODIFIED, LOCATION, RETRY_AFTER,
};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, ClientBuilder, Response};
use url::Url;

use crate::auth::{AuthHosts, CredentialPlan};
use crate::config::FetchConfig;
use crate::core::error::{Result, WidowError};
use crate::fetch::content;
use crate::fetch::gate::HostGate;
use crate::fetch::ssrf::SsrfGate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusHandling {
 /// Non-2xx is an error (page fetches).
 SuccessOnly,
 /// Any final status is returned (robots.txt).
 Any,
}

#[derive(Debug, Clone, Default)]
pub struct Conditional {
 pub etag: Option<String>,
 pub last_modified: Option<String>,
 /// Schema v11: the `pages.validators_write_seq` these headers came from.
 /// Not sent on the wire; the 304 record compares it to the stored page inside
 /// one transaction to decide whether the response is bound to a write.
 pub sent_seq: Option<i64>,
}

/// Per-hop policy consulted before each request, including redirect targets.
/// robots.txt lives here so a redirect to a disallowed path is not followed
/// (). The guard also returns the hop's `Crawl-delay`, if any.
pub type HopCheckFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<Duration>>> + Send + 'a>>;

pub trait HopGuard: Send + Sync + std::fmt::Debug {
 fn check<'a>(&'a self, url: &'a Url) -> HopCheckFuture<'a>;
}

/// A sealed-jar session credential (): a `Cookie` header that may be
/// attached **only** to the exact bound origin. The value is a secret and is
/// never logged; `Debug` prints the site and origin only.
#[derive(Clone)]
pub struct SessionCredential {
 pub site: String,
 pub host: String,
 pub port: u16,
 pub cookie: String,
 /// The site's own login page: a redirect to it means the session is dead.
 pub login: Option<(String, String)>,
}

impl std::fmt::Debug for SessionCredential {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("SessionCredential")
.field("site", &self.site)
.field("host", &self.host)
.field("port", &self.port)
.field("cookie", &"<redacted>")
.finish()
 }
}

impl SessionCredential {
 /// The exact origin this credential belongs to.
 pub fn matches(&self, url: &Url) -> bool {
 crate::auth::canonical_url_host(url) == self.host
 && url.port_or_known_default() == Some(self.port)
 }
}

#[derive(Debug, Clone)]
pub struct FetchRequest {
 pub url: Url,
 pub conditional: Conditional,
 pub accept: &'static str,
 pub max_body_bytes: u64,
 /// `None` for the robots.txt fetch itself (which must never recurse).
 pub hop_guard: Option<Arc<dyn HopGuard>>,
 /// Additional spacing for this request (a crawl job's per-host rate). The
 /// gate takes the larger of this and the robots `Crawl-delay`.
 pub min_interval: Option<Duration>,
 /// The content-type allowlist this request is entitled to. Only the
 /// `extract_rows` acquisition uses `Tabular`; every page path uses `Page`.
 pub scope: content::ContentScope,
 /// `Some` for an authenticated fetch ( A1): every hop is checked against
 /// the plan's binding before a secret is resolved, and the credential is
 /// attached per request, never to the client.
 pub auth: Option<Arc<CredentialPlan>>,
 /// Receives the canonical host of every hop that actually carried the
 /// credential, so the audit row can name all of them (F-2).
 pub auth_hosts: Option<Arc<AuthHosts>>,
 /// `Some` for a sealed-session fetch (): the `Cookie` header is
 /// attached only to the exact origin, and every other hop is refused.
 pub session: Option<Arc<SessionCredential>>,
}

#[derive(Debug, Clone)]
pub struct RawResponse {
 pub requested_url: Url,
 pub final_url: Url,
 pub status: u16,
 pub content_type: Option<String>,
 pub etag: Option<String>,
 pub last_modified: Option<String>,
 pub cache_control_max_age: Option<Duration>,
 pub body: Vec<u8>,
 pub bytes: u64,
 pub redirects: usize,
 pub duration: Duration,
}

#[derive(Debug, Clone)]
struct Hop {
 status: u16,
 content_type: Option<String>,
 etag: Option<String>,
 last_modified: Option<String>,
 cache_control_max_age: Option<Duration>,
 retry_after: Option<Duration>,
 location: Option<String>,
 body: Vec<u8>,
 bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
 pub total: Duration,
 pub max_redirects: usize,
 pub max_body_bytes: u64,
 pub max_header_bytes: u64,
 pub retries_retry_after: u32,
 pub retries_5xx: u32,
 pub max_retry_after: Duration,
}

impl Limits {
 pub fn from_config(config: &FetchConfig) -> Self {
 Self {
 total: Duration::from_millis(config.total_timeout_ms),
 max_redirects: config.max_redirects,
 max_body_bytes: config.max_body_bytes,
 max_header_bytes: config.max_header_bytes,
 retries_retry_after: config.retries_retry_after,
 retries_5xx: config.retries_5xx,
 max_retry_after: Duration::from_secs(config.max_retry_after_s),
 }
 }
}

/// Public suffix of the accept header: HTML first, everything else cheaply.
pub const ACCEPT_PAGE: &str = "text/html,application/xhtml+xml,application/json,application/xml;q=0.9,text/plain;q=0.9,*/*;q=0.1";
/// The tabular family (`extract_rows`): CSV/TSV are named first so an origin
/// that content-negotiates serves the table rather than an HTML wrapper, while
/// the page types stay acceptable behind them.
pub const ACCEPT_TABULAR: &str = "text/csv,text/tab-separated-values,application/json,text/html;q=0.9,application/xhtml+xml;q=0.9,text/plain;q=0.8,application/xml;q=0.8,*/*;q=0.1";
pub const ACCEPT_ROBOTS: &str = "text/plain,*/*;q=0.1";

#[derive(Debug)]
pub struct HttpClient {
 ssrf: std::sync::Arc<SsrfGate>,
 gate: std::sync::Arc<HostGate>,
 limits: Limits,
 user_agent: String,
 connect_timeout: Duration,
 read_timeout: Duration,
 seed: std::collections::hash_map::RandomState,
}

impl HttpClient {
 pub fn new(
 config: &FetchConfig,
 ssrf: std::sync::Arc<SsrfGate>,
 gate: std::sync::Arc<HostGate>,
 user_agent: String) -> Result<Self> {
 let connect_timeout = Duration::from_millis(config.connect_timeout_ms);
 let read_timeout = Duration::from_millis(config.read_timeout_ms);
 // Validate the builder once at construction; every hop rebuilds it with
 // that hop's vetted addresses pinned ().
 Self::base_builder(&user_agent, connect_timeout, read_timeout)
.build()
.map_err(|error| WidowError::Internal(format!("cannot build http client: {error}")))?;
 Ok(Self {
 ssrf,
 gate,
 limits: Limits::from_config(config),
 user_agent,
 connect_timeout,
 read_timeout,
 seed: std::collections::hash_map::RandomState::new(),
 })
 }

 pub fn user_agent(&self) -> &str {
 &self.user_agent
 }

 fn base_builder(
 user_agent: &str,
 connect_timeout: Duration,
 read_timeout: Duration) -> ClientBuilder {
 Client::builder()
.user_agent(user_agent)
.redirect(reqwest::redirect::Policy::none())
.connect_timeout(connect_timeout)
.read_timeout(read_timeout)
.http1_max_headers(100)
.pool_max_idle_per_host(8)
 }

 /// The per-hop client: the URL host is unchanged (so TLS SNI and hostname
 /// verification stay host-based) while the name is resolved to exactly the
 /// addresses the gate vetted (). An empty vetted set fails closed — there
 /// is no second DNS lookup at connect time.
 fn hop_client(&self, peer: &crate::fetch::ssrf::VettedPeer) -> Result<Client> {
 if peer.addresses.is_empty() {
 return Err(WidowError::Dns {
 host: peer.host.clone(),
 message: "the gate vetted no addresses for this hop".to_string(),
 });
 }
 let addrs: Vec<SocketAddr> = peer
.addresses
.iter()
.map(|address| SocketAddr::new(*address, peer.port))
.collect();
 Self::base_builder(&self.user_agent, self.connect_timeout, self.read_timeout)
.resolve_to_addrs(&peer.host, &addrs)
.build()
.map_err(|error| {
 WidowError::Internal(format!("cannot build the pinned hop client: {error}"))
 })
 }

 /// Fetch with the content scope enforced (`unsupported_content_type` before
 /// the body is read). Non-2xx statuses are errors.
 pub async fn fetch(&self, request: FetchRequest) -> Result<RawResponse> {
 self.send(request, true, StatusHandling::SuccessOnly).await
 }

 /// Fetch without the content-type scope and without turning 4xx/5xx into an
 /// error, used for robots.txt (whose status *is* the policy input).
 pub async fn fetch_untyped(&self, request: FetchRequest) -> Result<RawResponse> {
 self.send(request, false, StatusHandling::Any).await
 }

 async fn send(
 &self,
 request: FetchRequest,
 enforce_scope: bool,
 status_handling: StatusHandling) -> Result<RawResponse> {
 let started = Instant::now();
 let deadline = tokio::time::Instant::now() + self.limits.total;
 let mut url = request.url.clone();
 let mut conditional = request.conditional.clone();
 let mut redirects = 0usize;
 // Resolved at most once per fetch (the manager is never re-invoked for a
 // redirect or a retry); it lives only for this call.
 let mut auth_header: Option<(HeaderName, HeaderValue)> = None;

 loop {
 //: a session cookie is exact-origin only, and the check runs
 // *before* any hop work (SSRF, robots, lease), so a cross-origin
 // redirect never touches the other origin at all.
 if let Some(session) = &request.session
 && !session.matches(&url)
 {
 return Err(WidowError::EgressBlocked {
 host: crate::auth::canonical_url_host(&url),
 reason: "credential".to_string(),
 });
 }
 let peer = self.ssrf.vet(&url).await?;
 let hop_client = self.hop_client(&peer)?;
 let host = if peer.host.is_empty() {
 url.host_str().unwrap_or_default().to_string()
 } else {
 peer.host.clone()
 };
 // Policy for this hop: robots.txt, the interval it implies, and the
 // caller's own floor (crawl rate).
 let robots_delay = match &request.hop_guard {
 Some(guard) => guard.check(&url).await?,
 None => None,
 };
 let robots_delay = match (robots_delay, request.min_interval) {
 (Some(robots), Some(policy)) => Some(robots.max(policy)),
 (robots, policy) => robots.or(policy),
 };

 let mut retries_retry_after = 0u32;
 let mut retries_5xx = 0u32;

 let hop = loop {
 let lease = self.gate.acquire(&host, robots_delay).await;
 // A1 — the binding check runs on every hop, after redirect
 // resolution and before any secret is resolved or attached.
 if let Some(plan) = &request.auth {
 plan.check_url(&url, redirects > 0)?;
 if auth_header.is_none() {
 // One resolution per fetch: the manager is never retried.
 auth_header = Some(plan.resolve_header().await?);
 }
 }
 let outcome = tokio::time::timeout_at(
 deadline,
 self.hop(
 &hop_client,
 &url,
 &conditional,
 request.accept,
 enforce_scope.then_some(request.scope),
 request.max_body_bytes,
 auth_header.as_ref(),
 request.session.as_deref()))
.await;
 drop(lease);

 let hop = match outcome {
 Ok(Ok(hop)) => hop,
 Ok(Err(error)) => return Err(error),
 Err(_) => {
 return Err(WidowError::Timeout(format!(
 "fetch exceeded the {} ms total timeout",
 self.limits.total.as_millis()
)));
 }
 };

 let retry_after_eligible = matches!(hop.status, 429 | 503);
 if retry_after_eligible && retries_retry_after < self.limits.retries_retry_after {
 retries_retry_after += 1;
 let wait = hop
.retry_after
.unwrap_or(Duration::from_millis(250))
.min(self.limits.max_retry_after)
 + self.jitter(250);
 self.sleep_before(deadline, wait).await?;
 continue;
 }
 if (500..=599).contains(&hop.status)
 && !retry_after_eligible
 && retries_5xx < self.limits.retries_5xx
 {
 retries_5xx += 1;
 self.sleep_before(deadline, Duration::from_millis(250) + self.jitter(250))
.await?;
 continue;
 }
 break hop;
 };

 // A hop that answered received the credential: record it before the
 // redirect/binding handling so no host can be missing from the row.
 if (request.auth.is_some() && auth_header.is_some() || request.session.is_some())
 && let Some(hosts) = &request.auth_hosts
 {
 hosts.record(&crate::auth::canonical_url_host(&url));
 }

 if is_redirect(hop.status) {
 redirects += 1;
 if redirects > self.limits.max_redirects {
 return Err(WidowError::LimitExceeded(format!(
 "more than {} redirects",
 self.limits.max_redirects
)));
 }
 // A redirect back to the site's own login page is an expired
 // session, not a page to render.
 if let Some(session) = &request.session
 && let Some((login_host, login_path)) = &session.login
 {
 let target = url.join(&hop.location.clone().unwrap_or_default()).ok();
 if let Some(target) = target {
 let host = crate::auth::canonical_url_host(&target);
 if host == *login_host && target.path().starts_with(login_path.as_str()) {
 return Err(WidowError::AuthSessionExpired {
 site: session.site.clone(),
 });
 }
 }
 }
 let location = hop.location.clone().ok_or_else(|| WidowError::Http {
 status: Some(hop.status),
 detail: "redirect response without a Location header".to_string(),
 })?;
 url = url.join(&location).map_err(|error| {
 WidowError::InvalidUrl(format!("invalid redirect target `{location}`: {error}"))
 })?;
 // Conditional headers are origin-specific; drop them across hops.
 conditional = Conditional::default();
 continue;
 }

 if hop.status == 304 {
 return Ok(RawResponse {
 requested_url: request.url,
 final_url: url,
 status: 304,
 content_type: hop.content_type,
 etag: hop.etag,
 last_modified: hop.last_modified,
 cache_control_max_age: hop.cache_control_max_age,
 body: Vec::new(),
 bytes: 0,
 redirects,
 duration: started.elapsed(),
 });
 }

 if request.session.is_some() && matches!(hop.status, 401 | 403) {
 // The session is dead: typed, and the caller invalidates the jar.
 return Err(WidowError::AuthSessionExpired {
 site: request
.session
.as_ref()
.map(|session| session.site.clone())
.unwrap_or_default(),
 });
 }
 if request.auth.is_some() && matches!(hop.status, 401 | 403) {
 return Err(WidowError::AuthRejected { status: hop.status });
 }
 if !(200..300).contains(&hop.status)
 && matches!(status_handling, StatusHandling::SuccessOnly)
 {
 return Err(WidowError::Http {
 status: Some(hop.status),
 detail: format!("origin returned HTTP {}", hop.status),
 });
 }

 return Ok(RawResponse {
 requested_url: request.url,
 final_url: url,
 status: hop.status,
 content_type: hop.content_type,
 etag: hop.etag,
 last_modified: hop.last_modified,
 cache_control_max_age: hop.cache_control_max_age,
 body: hop.body,
 bytes: hop.bytes,
 redirects,
 duration: started.elapsed(),
 });
 }
 }

 #[allow(clippy::too_many_arguments)]
 async fn hop(
 &self,
 client: &Client,
 url: &Url,
 conditional: &Conditional,
 accept: &'static str,
 scope: Option<content::ContentScope>,
 max_body_bytes: u64,
 auth: Option<&(HeaderName, HeaderValue)>,
 session: Option<&SessionCredential>) -> Result<Hop> {
 let mut builder = client.get(url.clone()).header(ACCEPT, accept);
 if let Some((name, value)) = auth {
 builder = builder.header(name, value);
 }
 if let Some(session) = session {
 builder = builder.header(reqwest::header::COOKIE, session.cookie.as_str());
 }
 if let Some(etag) = &conditional.etag {
 builder = builder.header(IF_NONE_MATCH, etag);
 }
 if let Some(last_modified) = &conditional.last_modified {
 builder = builder.header(IF_MODIFIED_SINCE, last_modified);
 }
 let response = builder
.send()
.await
.map_err(|error| map_request_error(error, url))?;

 let status = response.status().as_u16();
 let headers = summarize_headers(response.headers(), self.limits.max_header_bytes, status)?;
 let mut hop = Hop {
 status,
 content_type: headers.content_type,
 etag: headers.etag,
 last_modified: headers.last_modified,
 cache_control_max_age: headers.cache_control_max_age,
 retry_after: headers.retry_after,
 location: headers.location,
 body: Vec::new(),
 bytes: 0,
 };

 if status == 304 || is_redirect(status) || !(200..300).contains(&status) {
 return Ok(hop);
 }
 if status == 204 || status == 205 {
 return Ok(hop);
 }
 if let Some(scope) = scope {
 scope.require(hop.content_type.as_deref())?;
 }
 let (body, bytes) = self.read_body(response, max_body_bytes).await?;
 hop.body = body;
 hop.bytes = bytes;
 Ok(hop)
 }

 async fn read_body(
 &self,
 mut response: Response,
 max_body_bytes: u64) -> Result<(Vec<u8>, u64)> {
 let mut body = Vec::new();
 let mut total = 0u64;
 loop {
 match response.chunk().await {
 Ok(Some(chunk)) => {
 total += chunk.len() as u64;
 if total > max_body_bytes {
 return Err(WidowError::TooLarge {
 limit: max_body_bytes,
 });
 }
 body.extend_from_slice(&chunk);
 }
 Ok(None) => break,
 Err(error) => {
 return Err(map_body_error(error));
 }
 }
 }
 Ok((body, total))
 }

 async fn sleep_before(&self, deadline: tokio::time::Instant, wait: Duration) -> Result<()> {
 let now = tokio::time::Instant::now();
 if now + wait >= deadline {
 return Err(WidowError::Timeout(
 "retry would exceed the total fetch timeout".to_string()));
 }
 tokio::time::sleep(wait).await;
 Ok(())
 }

 fn jitter(&self, span_ms: u64) -> Duration {
 use std::hash::{BuildHasher, Hasher};
 if span_ms == 0 {
 return Duration::ZERO;
 }
 let mut hasher = self.seed.build_hasher();
 hasher.write_u128(
 std::time::SystemTime::now()
.duration_since(std::time::UNIX_EPOCH)
.map(|value| value.as_nanos())
.unwrap_or_default());
 Duration::from_millis(hasher.finish() % (span_ms + 1))
 }
}

#[derive(Debug)]
struct HeaderSummary {
 content_type: Option<String>,
 etag: Option<String>,
 last_modified: Option<String>,
 cache_control_max_age: Option<Duration>,
 retry_after: Option<Duration>,
 location: Option<String>,
}

fn summarize_headers(headers: &HeaderMap, max_bytes: u64, status: u16) -> Result<HeaderSummary> {
 let mut total = 0u64;
 for (name, value) in headers {
 total += name.as_str().len() as u64 + value.as_bytes().len() as u64;
 }
 if total > max_bytes {
 return Err(WidowError::Http {
 status: Some(status),
 detail: format!("response headers exceeded {max_bytes} bytes"),
 });
 }

 let text = |name: reqwest::header::HeaderName| {
 headers
.get(name)
.and_then(|value| value.to_str().ok())
.map(str::to_string)
 };

 Ok(HeaderSummary {
 content_type: text(CONTENT_TYPE),
 etag: text(ETAG),
 last_modified: text(LAST_MODIFIED),
 cache_control_max_age: headers
.get(CACHE_CONTROL)
.and_then(|value| value.to_str().ok())
.and_then(parse_max_age),
 retry_after: headers
.get(RETRY_AFTER)
.and_then(|value| value.to_str().ok())
.and_then(parse_retry_after),
 location: text(LOCATION),
 })
}

pub fn parse_max_age(value: &str) -> Option<Duration> {
 for directive in value.split(',') {
 let directive = directive.trim();
 if let Some((name, argument)) = directive.split_once('=')
 && name.trim().eq_ignore_ascii_case("max-age")
 {
 let seconds: u64 = argument.trim().trim_matches('"').parse().ok()?;
 return Some(Duration::from_secs(seconds));
 }
 }
 None
}

/// `Retry-After` as delay-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
 let value = value.trim();
 if let Ok(seconds) = value.parse::<u64>() {
 return Some(Duration::from_secs(seconds));
 }
 let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
 let now = chrono::Utc::now();
 let delta = when.with_timezone(&chrono::Utc) - now;
 Some(Duration::from_secs(delta.num_seconds().max(0) as u64))
}

fn is_redirect(status: u16) -> bool {
 matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn map_request_error(error: reqwest::Error, url: &Url) -> WidowError {
 if error.is_timeout() {
 return WidowError::Timeout(error.to_string());
 }
 if error.is_connect() {
 let mut chain = String::new();
 let mut source: Option<&(dyn std::error::Error + 'static)> =
 std::error::Error::source(&error);
 while let Some(link) = source {
 chain.push_str(&link.to_string());
 chain.push(' ');
 source = link.source();
 }
 let lower = chain.to_ascii_lowercase();
 if lower.contains("dns") || lower.contains("lookup") || lower.contains("resolve") {
 return WidowError::Dns {
 host: url.host_str().unwrap_or_default().to_string(),
 message: chain.trim().to_string(),
 };
 }
 if lower.contains("tls")
 || lower.contains("certificate")
 || lower.contains("handshake")
 || lower.contains("invalid dnsname")
 {
 return WidowError::Tls(chain.trim().to_string());
 }
 return WidowError::Http {
 status: None,
 detail: format!("connection failed: {}", chain.trim()),
 };
 }
 if error.is_body() || error.is_decode() {
 return map_body_error(error);
 }
 WidowError::Http {
 status: None,
 detail: error.to_string(),
 }
}

fn map_body_error(error: reqwest::Error) -> WidowError {
 if error.is_timeout() {
 WidowError::Timeout(error.to_string())
 } else {
 WidowError::Http {
 status: error.status().map(|status| status.as_u16()),
 detail: format!("while reading the body: {error}"),
 }
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn retry_after_parses_seconds_and_dates() {
 assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
 assert_eq!(parse_retry_after(" 0 "), Some(Duration::ZERO));
 assert_eq!(parse_retry_after("soon"), None);
 let date = (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc2822();
 let parsed = parse_retry_after(&date).unwrap();
 assert!(parsed <= Duration::from_secs(10), "{parsed:?}");
 }

 #[test]
 fn max_age_is_read_from_cache_control() {
 assert_eq!(
 parse_max_age("public, max-age=600"),
 Some(Duration::from_secs(600))
);
 assert_eq!(
 parse_max_age("max-age=\"60\""),
 Some(Duration::from_secs(60))
);
 assert_eq!(parse_max_age("no-store"), None);
 assert_eq!(parse_max_age("s-maxage=60"), None);
 }

 #[test]
 fn redirect_statuses_are_the_expected_set() {
 for status in [301, 302, 303, 307, 308] {
 assert!(is_redirect(status), "{status}");
 }
 for status in [300, 304, 305, 306, 309, 200] {
 assert!(!is_redirect(status), "{status}");
 }
 }
}
