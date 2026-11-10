//! The authenticated browser path (): a dedicated headless pipe browser,
//! an **ephemeral browser context per fetch**, exact-host cookie injection, and
//! the B1 [`AuthEgressPolicy`] on **every** request before `ContinueRequest`.
//!
//! Boundaries this module owns:
//!
//! - **Egress:** every paused request (document *and* subresource, in every
//! attached target) is checked against the policy; a denial fails the request
//! with a typed reason. A request that carries a credential is additionally
//! subject to `may_bear_credential`, so a cookie a page somehow set can never
//! reach a non-binding origin.
//! - **Credential injection:** only cookies whose domain is the binding host
//! exactly are installed, and they are installed **host-only** (`url`), so the
//! browser itself cannot widen them to a subdomain (finding 2).
//! - **:** `handleAuthRequests` is on; any `authRequired` is answered with a
//! failure and the fetch returns a typed refusal. A challenge is never
//! answered with a credential.
//! - **:** a fresh context per fetch, a temp profile wiped at exit, and no
//! content is persisted here.
//!
//! The result is a [`RenderedPage`]-shaped value the pipeline extracts like any
//! other rendered document.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use url::Url;

use crate::auth::egress::{AuthEgressPolicy, EgressDecision};
use crate::auth::login::{CapturedCookie, TempProfile};
use crate::auth::pipe::{Browser, Control, EventHandler, LaunchOptions};
use crate::config::Config;
use crate::core::error::{Result, WidowError};
use crate::render::RenderedPage;

/// What one authenticated render is allowed to do.
#[derive(Debug, Clone)]
pub struct GateLimits {
 pub ceiling: Duration,
 /// Post-load settle: page scripts may still resolve a fetch after the load
 /// event (the render path uses the same idea).
 pub settle: Duration,
 pub max_requests: u32,
}

impl Default for GateLimits {
 fn default() -> Self {
 Self {
 ceiling: Duration::from_secs(15),
 settle: Duration::from_millis(300),
 max_requests: 300,
 }
 }
}

/// One refused request, for the audit/evidence row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDenial {
 pub host: String,
 pub reason: String,
 pub layer: &'static str,
}

/// The authenticated render result.
#[derive(Debug)]
pub struct AuthPage {
 pub page: RenderedPage,
 pub denials: Vec<GateDenial>,
 pub challenge_refused: bool,
 pub injected_cookies: usize,
 /// Cookie names from the jar that were **not** injected because their domain
 /// is not the binding host (finding 2's negative half).
 pub dropped_cookies: Vec<String>,
}

/// The per-fetch gate.
#[derive(Clone)]
pub struct AuthGate {
 pub site: String,
 pub host: String,
 pub port: u16,
 pub cookies: Vec<CapturedCookie>,
 pub policy: AuthEgressPolicy,
 pub chrome: PathBuf,
 pub data_dir: PathBuf,
 /// Test-only extra browser arguments (never set by the pipeline).
 pub launch_args: Vec<String>,
 /// Test-only resolver map for the filtering proxy, matching the browser's
 /// `--host-resolver-rules` in the hermetic fixture (`host -> address`).
 pub host_map: std::collections::BTreeMap<String, String>,
 /// The configured login page (`host`, `path`): a final document there is an
 /// expired session, never an authenticated page.
 pub login: Option<(String, String)>,
 pub limits: GateLimits,
}

impl std::fmt::Debug for AuthGate {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("AuthGate")
.field("site", &self.site)
.field("host", &self.host)
.field("port", &self.port)
.field("cookies", &format_args!("{} redacted", self.cookies.len()))
.field("policy", &self.policy)
.finish_non_exhaustive()
 }
}

impl AuthGate {
 /// Build a gate from the operator config and one unsealed jar.
 pub fn from_config(
 config: &Config,
 site: &str,
 host: &str,
 port: u16,
 cookies: Vec<CapturedCookie>) -> Result<Self> {
 let entry =
 config
.auth
.profiles
.get(site)
.ok_or_else(|| WidowError::AuthProfileUnknown {
 profile: site.to_string(),
 })?;
 let policy = AuthEgressPolicy::new(
 host,
 port,
 entry.allow_subdomains,
 &entry.assets,
 config.auth.allow_loopback_http)?;
 Ok(Self {
 site: site.to_string(),
 host: host.trim_end_matches('.').to_ascii_lowercase(),
 port,
 cookies,
 policy,
 chrome: PathBuf::new(),
 data_dir: PathBuf::new(),
 launch_args: Vec::new(),
 host_map: std::collections::BTreeMap::new(),
 login: entry
.login_url
.as_deref()
.and_then(|raw| Url::parse(raw).ok())
.map(|login| {
 (
 crate::auth::canonical_url_host(&login),
 login.path().to_string())
 }),
 limits: GateLimits::default(),
 })
 }

 pub fn with_browser(mut self, chrome: PathBuf, data_dir: PathBuf) -> Self {
 self.chrome = chrome;
 self.data_dir = data_dir;
 self
 }

 /// The exact-origin URL a cookie is installed for: host-only, never a domain.
 fn cookie_url(&self, cookie: &CapturedCookie) -> String {
 let path = if cookie.path.starts_with('/') {
 cookie.path.clone()
 } else {
 "/".to_string()
 };
 format!("https://{}:{}{}", self.host, self.port, path)
 }

 /// Render `url` in a fresh ephemeral context with the session cookies.
 pub fn render(&self, url: &Url) -> Result<AuthPage> {
 let started = Instant::now();
 match self.policy.check(url) {
 EgressDecision::Allow => {}
 EgressDecision::Block(denial) => {
 return Err(WidowError::EgressBlocked {
 host: crate::auth::canonical_url_host(url),
 reason: denial.as_str().to_string(),
 });
 }
 }
 // F8: a unique directory per render, so concurrent fetches cannot race
 // on one profile.
 static RENDER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
 let sequence = RENDER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
 let unique = format!("{}-{}", std::process::id(), sequence);
 let profile = TempProfile::new_at(
 self.data_dir
.join("auth")
.join("render")
.join(format!("{}-{unique}", self.site)))?;
 // The enforcement layer below the renderer (B1 §2 step 7): Fetch cannot
 // see ws:// or a preconnect, so the same policy is applied at connect
 // time and loopback is not silently bypassed.
 let proxy =
 crate::auth::proxy::FilteringProxy::start(self.policy.clone(), self.host_map.clone())?;
 let mut launch_args = vec![
 "--headless=new".to_string(),
 "--disable-gpu".to_string(),
 // C3: no non-proxied UDP (WebRTC ICE) may leave the browser. The
 // `force-` spelling is a no-op in headless 156 (measured: 4
 // datagrams); this spelling is the one that works.
 "--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
 format!("--proxy-server=http://127.0.0.1:{}", proxy.port()),
 "--proxy-bypass-list=<-loopback>".to_string(),
 ];
 launch_args.extend(self.launch_args.iter().cloned());
 let options = LaunchOptions {
 chrome: self.chrome.clone(),
 profile: profile.path().to_path_buf(),
 stderr_log: profile.path().join("chrome.stderr.log"),
 extra_args: launch_args,
 };
 let state = Arc::new(GateState {
 policy: self.policy.clone(),
 denials: Mutex::new(Vec::new()),
 requests: AtomicU32::new(0),
 bytes: AtomicU64::new(0),
 challenges: AtomicBool::new(false),
 method_refused: AtomicBool::new(false),
 loaded: AtomicBool::new(false),
 over_limit: AtomicBool::new(false),
 main_target: Mutex::new(None),
 sessions: Mutex::new(HashMap::new()),
 document: Mutex::new(None),
 });
 let browser = Browser::launch(&options, gate_handler(Arc::clone(&state)))?;
 let mut outcome = self.drive(&browser, &state, url);
 browser.shutdown();
 // Proxy refusals are part of the evidence and of the document verdict.
 let proxy_denials = proxy.denials();
 proxy.stop();
 if let Ok(page) = outcome.as_mut() {
 for denial in proxy_denials {
 page.denials.push(GateDenial {
 host: denial.host,
 reason: denial.reason,
 layer: "proxy",
 });
 }
 }
 let _ = profile.wipe_checked();
 let mut page = outcome?;
 page.page.duration = started.elapsed();
 Ok(page)
 }

 fn drive(&self, browser: &Browser, state: &Arc<GateState>, url: &Url) -> Result<AuthPage> {
 let context = browser.call(
 "Target.createBrowserContext",
 json!({"disposeOnDetach": false}),
 None)?;
 let context_id = context
.get("browserContextId")
.and_then(Value::as_str)
.unwrap_or_default()
.to_string();
 let target = browser.call(
 "Target.createTarget",
 json!({"url": "about:blank", "browserContextId": context_id}),
 None)?;
 let target_id = target
.get("targetId")
.and_then(Value::as_str)
.unwrap_or_default()
.to_string();
 if let Ok(mut main) = state.main_target.lock() {
 *main = Some(target_id.clone());
 }
 let session = browser.attach(&target_id)?;
 state.remember(&session, &target_id);
 browser.enable_page(&session)?;
 // Defence in depth, not the control (R-2): `Network.setBlockedURLs`
 // does **not** stop a `ws://` handshake (B1 class 12 measured it) and
 // cannot see a preconnect; it only refuses the listed schemes early.
 // What actually decides is the filtering proxy this render starts (the
 // connect-time policy, class 12/16) plus the Fetch handler for every
 // http(s) request below.
 browser.call("Network.setBlockedURLs", blocked_urls(), Some(&session))?;
 //: any challenge is a typed failure, never answered.
 browser.enable_fetch(&session)?;
 // Popups and SharedWorkers are browser-level targets (B1 sign-off).
 browser.auto_attach_browser()?;
 browser.auto_attach_pages(Some(&session))?;

 // Exact-host injection: only the binding host's cookies, installed
 // host-only so the browser cannot scope them to a subdomain.
 let mut injected = 0usize;
 let mut dropped = Vec::new();
 for cookie in &self.cookies {
 if cookie.domain != self.host {
 dropped.push(cookie.name.clone());
 continue;
 }
 let mut params = json!({
 "name": cookie.name,
 "value": cookie.value.as_str(),
 "url": self.cookie_url(cookie),
 "path": cookie.path,
 "secure": cookie.secure,
 "httpOnly": cookie.http_only,
 });
 if let Some(same_site) = &cookie.same_site {
 params["sameSite"] = Value::String(same_site.clone());
 }
 if let Some(expires) = cookie.expires {
 params["expires"] = json!(expires);
 }
 browser.call("Network.setCookie", params, Some(&session))?;
 injected += 1;
 }

 browser.navigate_async(&session, url.as_str());
 let deadline = Instant::now() + self.limits.ceiling;
 while Instant::now() < deadline {
 if state.loaded.load(Ordering::Relaxed)
 || state.challenges.load(Ordering::Relaxed)
 || state.over_limit.load(Ordering::Relaxed)
 {
 break;
 }
 std::thread::sleep(Duration::from_millis(50));
 }
 // A refused challenge aborts the navigation: return the typed refusal
 // before touching the (possibly absent) document.
 if state.challenges.load(Ordering::Relaxed) {
 return Err(WidowError::AuthChallengeRefused);
 }
 // C2: the read-only refusal is typed and the page never loads.
 if state.method_refused.load(Ordering::Relaxed) {
 return Err(WidowError::AuthMethodRefused);
 }
 // Evaluation is best-effort: a page that never produced a document is a
 // partial render, not a transport failure.
 if state.loaded.load(Ordering::Relaxed) {
 std::thread::sleep(self.limits.settle);
 }
 let located = browser
.evaluate(&session, "location.href")
.unwrap_or(Value::Null);
 let final_url = located
.get("value")
.and_then(Value::as_str)
.and_then(|value| Url::parse(value).ok())
.unwrap_or_else(|| url.clone());
 let html = browser
.evaluate(&session, "document.documentElement.outerHTML")
.unwrap_or(Value::Null);
 let html = html
.get("value")
.and_then(Value::as_str)
.unwrap_or_default()
.to_string();
 let (status, content_type) = state
.document
.lock()
.ok()
.and_then(|document| document.clone())
.unwrap_or((200, None));
 // C6: a 401/403 document, or a 200 page that is the site's login page,
 // is a dead session — never an authenticated page.
 if matches!(status, 401 | 403) {
 return Err(WidowError::AuthSessionExpired {
 site: self.site.clone(),
 });
 }
 if let Some((login_host, login_path)) = &self.login
 && crate::auth::canonical_url_host(&final_url) == *login_host
 && final_url.path().starts_with(login_path.as_str())
 {
 return Err(WidowError::AuthSessionExpired {
 site: self.site.clone(),
 });
 }
 let denials = state
.denials
.lock()
.map(|denials| denials.clone())
.unwrap_or_default();
 let challenges = state.challenges.load(Ordering::Relaxed);
 let requests = state.requests.load(Ordering::Relaxed);
 let bytes = state.bytes.load(Ordering::Relaxed);
 let over_limit = state.over_limit.load(Ordering::Relaxed);

 // Dispose the context before the process exits: no profile persistence.
 let _ = browser.call(
 "Target.disposeBrowserContext",
 json!({"browserContextId": context_id}),
 None);

 if challenges {
 return Err(WidowError::AuthChallengeRefused);
 }
 if over_limit {
 return Err(WidowError::RenderFailed(format!(
 "authenticated render exceeded the {} request cap",
 self.limits.max_requests
)));
 }
 // A denied *document* is a typed egress refusal, not an empty page.
 if let Some(denial) = denials.iter().find(|denial| denial.layer == "document") {
 return Err(WidowError::EgressBlocked {
 host: denial.host.clone(),
 reason: denial.reason.clone(),
 });
 }
 Ok(AuthPage {
 page: RenderedPage {
 html,
 final_url,
 status,
 content_type,
 partial: !state.loaded.load(Ordering::Relaxed),
 stop_reason: None,
 blocked: crate::render::BlockedCounts::default(),
 requests,
 bytes,
 duration: Duration::default(),
 },
 denials,
 challenge_refused: false,
 injected_cookies: injected,
 dropped_cookies: dropped,
 })
 }
}

struct GateState {
 policy: AuthEgressPolicy,
 denials: Mutex<Vec<GateDenial>>,
 requests: AtomicU32,
 bytes: AtomicU64,
 challenges: AtomicBool,
 method_refused: AtomicBool,
 loaded: AtomicBool,
 over_limit: AtomicBool,
 main_target: Mutex<Option<String>>,
 sessions: Mutex<HashMap<String, String>>,
 document: Mutex<Option<(u16, Option<String>)>>,
}

impl GateState {
 fn remember(&self, session: &str, target: &str) {
 if let Ok(mut sessions) = self.sessions.lock() {
 sessions.insert(session.to_string(), target.to_string());
 }
 }

 fn deny(&self, host: &str, reason: &str, layer: &'static str) {
 if let Ok(mut denials) = self.denials.lock()
 && denials.len() < 32
 {
 denials.push(GateDenial {
 host: host.to_string(),
 reason: reason.to_string(),
 layer,
 });
 }
 }
}

/// C2: the authenticated browser is read-only.
fn method_allowed(method: &str) -> bool {
 method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
}

/// The network-layer block every session installs. The Fetch domain cannot see a
/// WebSocket handshake, so the scheme block is what enforces class 12 in the
/// authenticated browser (B1 measured the same block on the render path).
fn blocked_urls() -> Value {
 json!({
 "urlPatterns": [
 {"urlPattern": "file://*/*", "block": true},
 {"urlPattern": "ws://*/*", "block": true},
 {"urlPattern": "wss://*/*", "block": true}
 ]
 })
}

fn gate_handler(state: Arc<GateState>) -> EventHandler {
 Arc::new(move |control: &Control, message: &Value| {
 let method = message.get("method").and_then(Value::as_str).unwrap_or("");
 let session = message.get("sessionId").and_then(Value::as_str);
 let params = message.get("params").cloned().unwrap_or(Value::Null);
 match method {
 "Fetch.requestPaused" => {
 let request_id = params
.get("requestId")
.and_then(Value::as_str)
.unwrap_or("");
 let request = params.get("request").cloned().unwrap_or(Value::Null);
 let raw = request.get("url").and_then(Value::as_str).unwrap_or("");
 let resource = params
.get("resourceType")
.and_then(Value::as_str)
.unwrap_or("");
 let layer = if resource == "Document" {
 "document"
 } else {
 "subresource"
 };
 // C2: the authenticated browser is read-only. A non-GET/HEAD
 // request is refused before it reaches the origin.
 let method = request
.get("method")
.and_then(Value::as_str)
.unwrap_or("GET");
 if !method_allowed(method) {
 let host = Url::parse(raw)
.map(|url| crate::auth::canonical_url_host(&url))
.unwrap_or_default();
 state.method_refused.store(true, Ordering::Relaxed);
 state.deny(&host, "method", "method");
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 return;
 }
 let count = state.requests.fetch_add(1, Ordering::Relaxed) + 1;
 if count > 300 {
 state.over_limit.store(true, Ordering::Relaxed);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 return;
 }
 if raw.starts_with("data:") || raw.starts_with("blob:") {
 control.send(
 "Fetch.continueRequest",
 json!({"requestId": request_id}),
 session);
 return;
 }
 let Ok(url) = Url::parse(raw) else {
 state.deny("", "scheme", layer);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 return;
 };
 let host = crate::auth::canonical_url_host(&url);
 if let EgressDecision::Block(denial) = state.policy.check(&url) {
 state.deny(&host, denial.as_str(), layer);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 return;
 }
 // A request that carries a credential may only target the exact
 // binding origin (defence in depth over host-only injection).
 let carries_credential = request
.get("headers")
.and_then(Value::as_object)
.is_some_and(|headers| {
 headers.keys().any(|name| {
 name.eq_ignore_ascii_case("cookie")
 || name.eq_ignore_ascii_case("authorization")
 })
 });
 if carries_credential
 && let EgressDecision::Block(denial) = state.policy.may_bear_credential(&url)
 {
 state.deny(&host, denial.as_str(), layer);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 return;
 }
 control.send(
 "Fetch.continueRequest",
 json!({"requestId": request_id}),
 session);
 }
 "Fetch.authRequired" => {
 let request_id = params
.get("requestId")
.and_then(Value::as_str)
.unwrap_or("");
 state.challenges.store(true, Ordering::Relaxed);
 state.deny("", "challenge_refused", "challenge");
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 }
 "Network.responseReceived" => {
 let kind = params
.get("type")
.and_then(Value::as_str)
.unwrap_or_default();
 if kind == "Document" {
 let response = params.get("response").cloned().unwrap_or(Value::Null);
 let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
 let mime = response
.get("mimeType")
.and_then(Value::as_str)
.map(str::to_string);
 if let Ok(mut document) = state.document.lock() {
 *document = Some((status as u16, mime));
 }
 }
 }
 "Network.loadingFinished" => {
 if let Some(length) = params.get("encodedDataLength").and_then(Value::as_f64) {
 state
.bytes
.fetch_add(length.max(0.0) as u64, Ordering::Relaxed);
 }
 }
 "Page.loadEventFired" => {
 state.loaded.store(true, Ordering::Relaxed);
 }
 "Target.attachedToTarget" => {
 let child = params
.get("sessionId")
.and_then(Value::as_str)
.unwrap_or("")
.to_string();
 let target = params
.get("targetInfo")
.and_then(|info| info.get("targetId"))
.and_then(Value::as_str)
.unwrap_or("")
.to_string();
 if !child.is_empty() {
 state.remember(&child, &target);
 }
 // The gate must cover every child target, or a worker/iframe
 // request would escape it.
 control.send(
 "Fetch.enable",
 json!({
 "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
 "handleAuthRequests": true
 }),
 Some(&child));
 control.send("Network.enable", json!({}), Some(&child));
 control.send("Network.setBlockedURLs", blocked_urls(), Some(&child));
 control.send(
 "Target.setAutoAttach",
 json!({
 "autoAttach": true,
 "waitForDebuggerOnStart": true,
 "flatten": true
 }),
 Some(&child));
 control.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&child));
 }
 _ => {}
 }
 })
}

#[cfg(test)]
mod tests {
 use super::*;
 use crate::auth::login::CapturedCookie;
 use crate::config::{AuthKind, AuthProfileConfig, Config};
 use zeroize::Zeroizing;

 fn config() -> Config {
 let mut config = Config::default();
 config.auth.profiles.insert(
 "site".to_string(),
 AuthProfileConfig {
 kind: AuthKind::Session,
 host: "bound.test".to_string(),
 port: 443,
 login_url: Some("https://bound.test/login".to_string()),
 login_origins: vec!["bound.test".to_string()],
 verify_url: Some("https://bound.test/verify".to_string()),
..Default::default()
 });
 config
 }

 fn cookie(value: &str) -> CapturedCookie {
 CapturedCookie {
 name: "ww".to_string(),
 value: Zeroizing::new(value.to_string()),
 domain: "bound.test".to_string(),
 path: "/".to_string(),
 secure: true,
 http_only: true,
 same_site: None,
 expires: None,
 }
 }

 #[test]
 fn the_gate_debug_is_redacted() {
 let gate = AuthGate::from_config(
 &config(),
 "site",
 "bound.test",
 443,
 vec![cookie("GATE-SECRET")])
.expect("gate");
 let rendered = format!("{gate:?}");
 assert!(!rendered.contains("GATE-SECRET"), "{rendered}");
 assert!(rendered.contains("1 redacted"), "{rendered}");
 }

 #[test]
 fn only_get_and_head_are_allowed() {
 for allowed in ["GET", "get", "HEAD", "head"] {
 assert!(method_allowed(allowed), "{allowed}");
 }
 for refused in [
 "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT",
 ] {
 assert!(!method_allowed(refused), "{refused}");
 }
 }

 #[test]
 fn the_gate_launch_block_is_documented() {
 // C3: the flag is part of the production launch line even though the
 // canary measured it as insufficient on 156 headless (EV-B5a).
 let source = include_str!("gate.rs");
 assert!(source.contains("--webrtc-ip-handling-policy=disable_non_proxied_udp"));
 assert!(source.contains("--proxy-bypass-list=<-loopback>"));
 }

 #[test]
 fn the_login_page_is_derived_from_the_config() {
 let gate =
 AuthGate::from_config(&config(), "site", "bound.test", 443, Vec::new()).expect("gate");
 assert_eq!(
 gate.login,
 Some(("bound.test".to_string(), "/login".to_string()))
);
 }
}
