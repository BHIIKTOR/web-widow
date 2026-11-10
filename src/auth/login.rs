//! Assisted login and cookie capture.
//!
//! The operator drives the login window; web-widow never types, clicks or fills.
//! This module owns the four boundaries the batch must prove:
//!
//! - **origin gate** — every document the window navigates to must be a declared
//! `login_origins` host; anything else is refused, recorded and the window is
//! closed ();
//! - **final origin** — capture is refused unless the window ended on the binding
//! host ();
//! - **exact-host capture** — only cookies whose domain is the binding host
//! exactly are sealed; no IdP, no parent-domain, no wildcard, and attributes
//! are preserved;
//! - **verify before seal** — the captured cookies must produce a 2xx from
//! `verify_url` through a plain cookie-bearing GET before anything is sealed.
//!
//! The profile is a dedicated temp directory wiped on exit (), and the
//! browser is the pipe-only, headed process from [`super::pipe`].

use std::collections::{BTreeSet, HashMap};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zeroize::Zeroizing;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::auth::canonical_url_host;
use crate::auth::pipe::{Browser, BrowserFacts, Control, LaunchOptions};
use crate::auth::store::{KeyringKeys, SessionMeta, SessionStore};
use crate::config::Config;
use crate::core::error::{Result, WidowError};

/// What the operator decided at the confirmation prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
 Capture,
 Abort,
}

/// The human at the TTY. The CLI supplies [`TtyOperator`]; the hermetic
/// integration test supplies a scripted one.
pub trait Operator {
 fn confirm(&self, origin: &str) -> Result<Decision>;
}

/// A real terminal. Construction fails without one, so no tool can reach the
/// login flow by accident.
pub struct TtyOperator;

impl TtyOperator {
 pub fn new() -> Result<Self> {
 if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
 Ok(Self)
 } else {
 Err(WidowError::AuthLoginNotTerminal)
 }
 }
}

impl Operator for TtyOperator {
 fn confirm(&self, origin: &str) -> Result<Decision> {
 use std::io::{BufRead, Write};
 eprintln!("final origin: {origin}");
 eprintln!(
 "Finish the login in the window (including 2FA), then press Enter here to capture; \
 type `q` to abort."
);
 eprint!("> ");
 std::io::stderr().flush().ok();
 let mut line = String::new();
 std::io::stdin()
.lock()
.read_line(&mut line)
.map_err(|error| WidowError::AuthLoginBrowserFailed {
 detail: format!("stdin could not be read: {error}"),
 })?;
 if line.trim().eq_ignore_ascii_case("q") {
 Ok(Decision::Abort)
 } else {
 Ok(Decision::Capture)
 }
 }
}

/// One captured cookie. Attributes are preserved verbatim from the browser; the
/// value is a secret: it is a [`Zeroizing`] buffer and never appears in `Debug`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturedCookie {
 pub name: String,
 /// The secret value. Wiped on drop; redacted in every `Debug` rendering.
 #[serde(with = "zeroized_value")]
 pub value: Zeroizing<String>,
 pub domain: String,
 pub path: String,
 pub secure: bool,
 pub http_only: bool,
 pub same_site: Option<String>,
 /// Unix seconds, `-1`/absent for a session cookie.
 pub expires: Option<i64>,
}

impl std::fmt::Debug for CapturedCookie {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("CapturedCookie")
.field("name", &self.name)
.field("value", &"<redacted>")
.field("domain", &self.domain)
.field("path", &self.path)
.field("secure", &self.secure)
.field("http_only", &self.http_only)
.field("same_site", &self.same_site)
.field("expires", &self.expires)
.finish()
 }
}

/// Serde for [`Zeroizing<String>`] without the crate's `serde` feature: the jar
/// payload is the only serialization, and it never leaves the encrypted file.
mod zeroized_value {
 use serde::{Deserialize, Deserializer, Serialize, Serializer};
 use zeroize::Zeroizing;

 pub fn serialize<S: Serializer>(
 value: &Zeroizing<String>,
 serializer: S) -> Result<S::Ok, S::Error> {
 value.as_str().serialize(serializer)
 }

 pub fn deserialize<'de, D: Deserializer<'de>>(
 deserializer: D) -> Result<Zeroizing<String>, D::Error> {
 String::deserialize(deserializer).map(Zeroizing::new)
 }
}

/// The value-free result printed by the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct LoginSummary {
 pub site: String,
 pub host: String,
 pub cookie_count: usize,
 pub expires_at_min: Option<i64>,
 pub verify_status: u16,
 pub refused_navigations: Vec<String>,
 pub profile_wiped: bool,
 pub browser: BrowserFacts,
}

/// The gate's shared state, written by the protocol handler on the reader thread.
struct GateState {
 origins: BTreeSet<String>,
 targets: Mutex<HashMap<String, String>>,
 last_document: Mutex<Option<String>>,
 /// F4: only this frame's documents may set the final origin.
 main_frame: Mutex<Option<String>>,
 loaded: AtomicBool,
 refused: Mutex<Vec<String>>,
 refused_flag: AtomicBool,
 auth_challenges: AtomicBool,
}

impl GateState {
 fn record_target(&self, session: &str, target: &str) {
 if let Ok(mut targets) = self.targets.lock() {
 targets.insert(session.to_string(), target.to_string());
 }
 }

 fn target_for(&self, session: &str) -> Option<String> {
 self.targets
.lock()
.ok()
.and_then(|targets| targets.get(session).cloned())
 }
}

/// The protocol handler: gate documents, refuse auth challenges, gate children.
fn handler(state: Arc<GateState>) -> crate::auth::pipe::EventHandler {
 Arc::new(move |control: &Control, message: &Value| {
 let method = message.get("method").and_then(Value::as_str).unwrap_or("");
 let session = message.get("sessionId").and_then(Value::as_str);
 let params = message.get("params").cloned().unwrap_or(Value::Null);
 match method {
 "Fetch.requestPaused" => {
 let request = params.get("request").cloned().unwrap_or(Value::Null);
 let url = request.get("url").and_then(Value::as_str).unwrap_or("");
 let request_id = params
.get("requestId")
.and_then(Value::as_str)
.unwrap_or("");
 let resource = params
.get("resourceType")
.and_then(Value::as_str)
.unwrap_or("");
 if resource == "Document" {
 let parsed = Url::parse(url).ok();
 let host = parsed.as_ref().map(canonical_url_host).unwrap_or_default();
 if state.origins.contains(&host) {
 // F4: only the main frame's documents move the final
 // origin; a subframe cannot make a non-binding top-level
 // page look capturable.
 let frame = params.get("frameId").and_then(Value::as_str);
 let is_main = state
.main_frame
.lock()
.map(|main| main.as_deref().is_some_and(|id| Some(id) == frame))
.unwrap_or(false);
 if is_main && let Ok(mut last) = state.last_document.lock() {
 *last = Some(url.to_string());
 }
 control.send(
 "Fetch.continueRequest",
 json!({"requestId": request_id}),
 session);
 } else {
 if let Ok(mut refused) = state.refused.lock()
 && refused.len() < 8
 {
 refused.push(host.clone());
 }
 state.refused_flag.store(true, Ordering::Relaxed);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
 // Close the window that tried to leave the boundary.
 if let Some(target) = session.and_then(|value| state.target_for(value)) {
 control.send("Target.closeTarget", json!({"targetId": target}), None);
 }
 }
 } else {
 control.send(
 "Fetch.continueRequest",
 json!({"requestId": request_id}),
 session);
 }
 }
 "Page.loadEventFired" => {
 state.loaded.store(true, Ordering::Relaxed);
 }
 "Fetch.authRequired" => {
 // A login page must not pull an HTTP auth credential from us.
 let request_id = params
.get("requestId")
.and_then(Value::as_str)
.unwrap_or("");
 state.auth_challenges.store(true, Ordering::Relaxed);
 control.send(
 "Fetch.failRequest",
 json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
 session);
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
 state.record_target(&child, &target);
 }
 // A popup must not navigate before the gate is on its session.
 control.send(
 "Fetch.enable",
 json!({
 "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
 "handleAuthRequests": true
 }),
 Some(&child));
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

/// Everything one `auth login` needs.
#[derive(Debug, Clone)]
pub struct Login {
 pub site: String,
 pub host: String,
 pub login_url: Url,
 pub login_origins: BTreeSet<String>,
 pub verify_url: Url,
 /// Assets are recorded for the profile contract; the login window's boundary
 /// is `login_origins`, not this set.
 pub assets: Vec<String>,
 pub max_age_days: u32,
 pub chrome: PathBuf,
 pub data_dir: PathBuf,
 /// Test-only trust anchor for the hermetic fixture; the CLI leaves it unset.
 pub verify_root_ca: Option<PathBuf>,
 /// Test-only extra browser arguments (resolver rules, fixture trust); the
 /// CLI leaves this empty and the production launch line is asserted in
 /// `pipe::tests`.
 pub launch_args: Vec<String>,
 /// Test-only resolver override matching the browser's `--host-resolver-rules`
 /// in the hermetic fixture: `(host, address)`.
 pub verify_resolve: Option<(String, std::net::SocketAddr)>,
}

impl Login {
 /// Resolve a `kind = "session"` profile. Every value was validated at config
 /// load; this re-checks the shape the flow depends on.
 pub fn from_config(config: &Config, profile: &str) -> Result<Self> {
 let entry =
 config
.auth
.profiles
.get(profile)
.ok_or_else(|| WidowError::AuthProfileUnknown {
 profile: profile.to_string(),
 })?;
 if entry.kind != crate::config::AuthKind::Session {
 return Err(WidowError::AuthSessionUnsupported);
 }
 let host = entry
.host
.strip_suffix('.')
.unwrap_or(&entry.host)
.to_string();
 let login_url = Url::parse(entry.login_url.as_deref().unwrap_or_default())
.map_err(|error| WidowError::InvalidUrl(format!("login_url: {error}")))?;
 let verify_url = Url::parse(entry.verify_url.as_deref().unwrap_or_default())
.map_err(|error| WidowError::InvalidUrl(format!("verify_url: {error}")))?;
 let origins: BTreeSet<String> = entry.login_origins.iter().cloned().collect();
 Ok(Self {
 site: profile.to_string(),
 host,
 login_url,
 login_origins: origins,
 verify_url,
 assets: entry.assets.clone(),
 max_age_days: entry.session_max_age_days.unwrap_or(30),
 chrome: PathBuf::new(),
 data_dir: PathBuf::new(),
 verify_root_ca: None,
 launch_args: Vec::new(),
 verify_resolve: None,
 })
 }

 pub fn with_browser(mut self, chrome: PathBuf, data_dir: PathBuf) -> Self {
 self.chrome = chrome;
 self.data_dir = data_dir;
 self
 }

 /// The CLI entry: a real terminal, the keyring preflight, then the window.
 pub fn run_tty(&self) -> Result<LoginSummary> {
 let operator = TtyOperator::new()?;
 self.run(&operator)
 }

 /// The whole flow. Refuses before any window when the keyring is unusable.
 pub fn run(&self, operator: &dyn Operator) -> Result<LoginSummary> {
 let store = SessionStore::with_keys(self.data_dir.join("auth"), Box::new(KeyringKeys));
 //: the operator learns the environment is unusable *before* a window
 // opens. This probes the store only; no file or directory is created.
 store.preflight(&self.site)?;

 let profile = TempProfile::new(&self.data_dir, &self.site)?;
 let state = Arc::new(GateState {
 origins: self.login_origins.clone(),
 targets: Mutex::new(HashMap::new()),
 last_document: Mutex::new(None),
 main_frame: Mutex::new(None),
 loaded: AtomicBool::new(false),
 refused: Mutex::new(Vec::new()),
 refused_flag: AtomicBool::new(false),
 auth_challenges: AtomicBool::new(false),
 });
 let options = LaunchOptions {
 chrome: self.chrome.clone(),
 profile: profile.path().to_path_buf(),
 stderr_log: profile.path().join("chrome.stderr.log"),
 extra_args: Vec::new(),
 };
 let options = LaunchOptions {
 extra_args: self.launch_args.clone(),
..options
 };
 let browser = Browser::launch(&options, handler(Arc::clone(&state)))?;
 // Defense in depth: a TCP DevTools endpoint would make the pipe claim
 // false, so refuse to continue if one appeared.
 let facts = browser.facts();
 if !facts.no_tcp_listener() || !facts.devtools_active_port_absent {
 return Err(WidowError::AuthLoginBrowserFailed {
 detail: "the login browser published a TCP DevTools endpoint; refusing".to_string(),
 });
 }
 let result = self.drive(&browser, &state, operator);
 // The final launch facts (a live page has spawned its renderer tree by
 // now) are the evidence; the early check above was the guard.
 let final_facts = browser.facts();
 // Stop the browser before the wipe in both outcomes: the profile is only
 // removable once its process (and its children) are gone.
 browser.shutdown();
 let summary = match result {
 Ok(summary) => summary,
 Err(error) => {
 // applies to a refused flow too: the profile must not
 // survive. A profile that outlives the retries replaces the
 // original error, because a surviving profile is the graver
 // stop condition.
 if profile.wipe_checked().is_err() {
 return Err(WidowError::AuthLoginBrowserFailed {
 detail: "the login profile survived a refused capture".to_string(),
 });
 }
 return Err(error);
 }
 };
 //: a profile that survives capture is a stop condition, not a note.
 let wiped = profile.wipe_checked()?;
 Ok(LoginSummary {
 profile_wiped: wiped,
 browser: final_facts,
..summary
 })
 }

 fn drive(
 &self,
 browser: &Browser,
 state: &Arc<GateState>,
 operator: &dyn Operator) -> Result<LoginSummary> {
 let version = browser.version()?;
 if !version.contains("Chrome/") {
 return Err(WidowError::AuthLoginBrowserFailed {
 detail: format!("unexpected Browser.getVersion product `{version}`"),
 });
 }
 let login_target = browser.create_target("about:blank")?;
 let session = browser.attach(&login_target)?;
 state.record_target(&session, &login_target);
 browser.enable_page(&session)?;
 // F4: remember the main frame before any navigation, so only its
 // documents can become the final origin.
 if let Ok(tree) = browser.call("Page.getFrameTree", json!({}), Some(&session))
 && let Some(id) = tree
.get("frameTree")
.and_then(|tree| tree.get("frame"))
.and_then(|frame| frame.get("id"))
.and_then(Value::as_str)
 && let Ok(mut main) = state.main_frame.lock()
 {
 *main = Some(id.to_string());
 }
 browser.enable_fetch(&session)?;
 browser.auto_attach_pages(Some(&session))?;
 browser.navigate(&session, self.login_url.as_str())?;
 // Give the first document a bounded moment to load: the operator needs
 // a window to look at, and the test's scripted operator needs the page's
 // cookies to exist before it confirms.
 wait_for_load(state, Duration::from_secs(20));

 // The operator completes the login; the gate records the final document.
 let decision = operator.confirm(&self.current_origin(state))?;
 if decision == Decision::Abort {
 return Err(WidowError::AuthLoginOriginRefused {
 host: self.current_host(state),
 });
 }
 if state.refused_flag.load(Ordering::Relaxed) {
 let refused = state
.refused
.lock()
.ok()
.and_then(|refused| refused.last().cloned())
.unwrap_or_default();
 return Err(WidowError::AuthLoginOriginRefused { host: refused });
 }
 let final_host = self.current_host(state);
 if final_host != self.host {
 return Err(WidowError::AuthLoginFinalOriginRefused {
 host: final_host,
 expected: self.host.clone(),
 });
 }

 let verify_origin = format!(
 "https://{}:{}/",
 self.host,
 self.verify_url.port().unwrap_or(443)
);
 let cookies = browser.cookies_for(Some(&session), &[verify_origin.as_str()])?;
 let captured = capture_cookies(&cookies, &self.host);
 if captured.is_empty() {
 return Err(WidowError::AuthCaptureEmpty);
 }
 let cookie_header = cookie_header(&captured);
 let verify_status = self.verify(&cookie_header)?;
 if !(200..300).contains(&verify_status) {
 return Err(WidowError::AuthVerifyFailed {
 verify_url: origin_of(&self.verify_url),
 status: verify_status,
 reason: "the verify request did not return 2xx".to_string(),
 });
 }

 let now = unix_now();
 let expires_at_min = captured
.iter()
.filter_map(|cookie| cookie.expires.filter(|expiry| *expiry > 0))
.min()
.map(|cookie_expiry| cookie_expiry.min(now + i64::from(self.max_age_days) * 86_400))
.or(Some(now + i64::from(self.max_age_days) * 86_400));
 let payload =
 serde_json::to_vec(&captured).map_err(|error| WidowError::AuthLoginBrowserFailed {
 detail: format!("cookie jar could not be serialized: {error}"),
 })?;
 let meta = SessionMeta {
 captured_at: now,
 expires_at_min,
 cookie_count: captured.len(),
 hosts: vec![self.host.clone()],
 verify_url: Some(self.verify_url.to_string()),
 };
 let store = SessionStore::with_keys(self.data_dir.join("auth"), Box::new(KeyringKeys));
 store.seal(&self.site, &payload, &meta)?;

 Ok(LoginSummary {
 site: self.site.clone(),
 host: self.host.clone(),
 cookie_count: captured.len(),
 expires_at_min,
 verify_status,
 refused_navigations: state
.refused
.lock()
.map(|refused| refused.clone())
.unwrap_or_default(),
 profile_wiped: false,
 browser: browser.facts(),
 })
 }

 fn current_origin(&self, state: &GateState) -> String {
 state
.last_document
.lock()
.ok()
.and_then(|last| last.clone())
.and_then(|url| Url::parse(&url).ok())
.map(|url| origin_of(&url))
.unwrap_or_else(|| "about:blank".to_string())
 }

 fn current_host(&self, state: &GateState) -> String {
 state
.last_document
.lock()
.ok()
.and_then(|last| last.clone())
.and_then(|url| Url::parse(&url).ok())
.map(|url| canonical_url_host(&url))
.unwrap_or_default()
 }

 /// A plain cookie-bearing GET. Redirects are not followed (a redirect to
 /// another host must never carry the jar), and only a 2xx verifies.
 fn verify(&self, cookie_header: &str) -> Result<u16> {
 let runtime = tokio::runtime::Builder::new_current_thread()
.enable_all()
.build()
.map_err(|error| WidowError::AuthVerifyFailed {
 verify_url: origin_of(&self.verify_url),
 status: 0,
 reason: format!("verify runtime could not start: {error}"),
 })?;
 let url = self.verify_url.clone();
 let cookie = cookie_header.to_string();
 let root_ca = self.verify_root_ca.clone();
 let resolve = self.verify_resolve.clone();
 runtime.block_on(async move {
 let mut builder = reqwest::Client::builder()
.redirect(reqwest::redirect::Policy::none())
.timeout(Duration::from_secs(20))
.user_agent("web-widow/session-verify");
 if let Some((host, address)) = resolve {
 builder = builder.resolve(&host, address);
 }
 if let Some(path) = root_ca
 && let Ok(pem) = std::fs::read(&path)
 && let Ok(certificate) = reqwest::Certificate::from_pem(&pem)
 {
 builder = builder.add_root_certificate(certificate);
 }
 let client = builder
.build()
.map_err(|error| WidowError::AuthVerifyFailed {
 verify_url: origin_of(&url),
 status: 0,
 reason: format!("verify client could not be built: {error}"),
 })?;
 let response = client
.get(url.clone())
.header(reqwest::header::COOKIE, cookie)
.send()
.await
.map_err(|error| WidowError::AuthVerifyFailed {
 verify_url: origin_of(&url),
 status: 0,
 reason: format!("verify request failed: {error}"),
 })?;
 Ok(response.status().as_u16())
 })
 }
}

/// Capture filter: a cookie is sealed only when its domain is the
/// binding host **exactly**. A leading dot is the cookie-spec host marker, not a
/// wildcard; anything else — an IdP origin, a parent domain broader than the
/// host, a subdomain — is dropped. Attributes are preserved.
pub fn capture_cookies(cookies: &[Value], host: &str) -> Vec<CapturedCookie> {
 let host = host.trim_end_matches('.').to_ascii_lowercase();
 let mut captured = Vec::new();
 for cookie in cookies {
 let Some(domain) = cookie.get("domain").and_then(Value::as_str) else {
 continue;
 };
 let normalized = domain
.trim_start_matches('.')
.trim_end_matches('.')
.to_ascii_lowercase();
 if normalized != host {
 continue;
 }
 let Some(name) = cookie.get("name").and_then(Value::as_str) else {
 continue;
 };
 let value = cookie
.get("value")
.and_then(Value::as_str)
.unwrap_or_default();
 captured.push(CapturedCookie {
 name: name.to_string(),
 value: Zeroizing::new(value.to_string()),
 domain: normalized,
 path: cookie
.get("path")
.and_then(Value::as_str)
.unwrap_or("/")
.to_string(),
 secure: cookie
.get("secure")
.and_then(Value::as_bool)
.unwrap_or(false),
 http_only: cookie
.get("httpOnly")
.and_then(Value::as_bool)
.unwrap_or(false),
 same_site: cookie
.get("sameSite")
.and_then(Value::as_str)
.map(str::to_string),
 expires: cookie
.get("expires")
.and_then(Value::as_f64)
.filter(|value| *value > 0.0)
.map(|value| value as i64),
 });
 }
 captured
}

/// The `Cookie` header for the verify request.
fn cookie_header(cookies: &[CapturedCookie]) -> String {
 cookies
.iter()
.map(|cookie| format!("{}={}", cookie.name, cookie.value.as_str()))
.collect::<Vec<_>>()
.join("; ")
}

/// A value-free origin (`scheme://host[:port]`): never a path or query, which
/// could carry a token.
pub fn origin_of(url: &Url) -> String {
 match url.port() {
 Some(port) => format!("{}://{}:{port}", url.scheme(), url.host_str().unwrap_or("")),
 None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")),
 }
}

/// Wait (bounded) for the gate to see an allowed document and for that document
/// to finish loading, so a cookie set by the response is in the jar before the
/// capture reads it. The operator's own pace normally covers this; the bound
/// keeps a broken page from hanging the command.
fn wait_for_load(state: &GateState, timeout: Duration) {
 let deadline = std::time::Instant::now() + timeout;
 while std::time::Instant::now() < deadline {
 let document = state
.last_document
.lock()
.map(|last| last.is_some())
.unwrap_or(false);
 if state.refused_flag.load(Ordering::Relaxed)
 || (document && state.loaded.load(Ordering::Relaxed))
 {
 return;
 }
 std::thread::sleep(Duration::from_millis(50));
 }
}

/// The document gate's decision, shared with the unit tests.
pub fn document_allowed(host: &str, origins: &BTreeSet<String>) -> bool {
 origins.contains(&host.trim_end_matches('.').to_ascii_lowercase())
}

fn unix_now() -> i64 {
 SystemTime::now()
.duration_since(UNIX_EPOCH)
.map(|since| since.as_secs() as i64)
.unwrap_or_default()
}

/// A dedicated profile that is removed on drop, whatever the outcome.
pub(crate) struct TempProfile {
 path: PathBuf,
 armed: Mutex<bool>,
}

impl TempProfile {
 fn new(data_dir: &Path, site: &str) -> Result<Self> {
 Self::new_at(data_dir.join("auth").join("login").join(site))
 }

 /// Create a fresh `0700` profile at an explicit path, removing a stale one.
 pub(crate) fn new_at(path: PathBuf) -> Result<Self> {
 // A stale profile from a killed run must never be reused.
 let _ = std::fs::remove_dir_all(&path);
 // `0700` on every created level: the store refuses a readable `auth/`.
 use std::os::unix::fs::DirBuilderExt;
 std::fs::DirBuilder::new()
.recursive(true)
.mode(0o700)
.create(&path)
.map_err(|error| WidowError::AuthLoginBrowserFailed {
 detail: format!("login profile could not be created: {error}"),
 })?;
 Ok(Self {
 path,
 armed: Mutex::new(true),
 })
 }

 pub(crate) fn path(&self) -> &Path {
 &self.path
 }

 /// Remove the profile; a directory that survives the retries is an error.
 /// Chrome's child processes can still be flushing when the browser exits, so
 /// the removal is retried briefly before it is declared a failure.
 pub(crate) fn wipe_checked(&self) -> Result<bool> {
 let armed = self
.armed
.lock()
.map(|mut armed| std::mem::replace(&mut *armed, false))
.unwrap_or(false);
 if !armed {
 return Ok(!self.path.exists());
 }
 for _ in 0..40 {
 if !self.path.exists() {
 return Ok(true);
 }
 let _ = std::fs::remove_dir_all(&self.path);
 if !self.path.exists() {
 return Ok(true);
 }
 std::thread::sleep(Duration::from_millis(100));
 }
 if self.path.exists() {
 return Err(WidowError::AuthLoginBrowserFailed {
 detail: format!(
 "the login profile survived capture at {}",
 self.path.display()
),
 });
 }
 Ok(true)
 }
}

impl Drop for TempProfile {
 fn drop(&mut self) {
 if let Ok(mut armed) = self.armed.lock()
 && *armed
 {
 // Bounded retry: Chrome's children can still be flushing.
 for _ in 0..20 {
 if !self.path.exists() {
 break;
 }
 let _ = std::fs::remove_dir_all(&self.path);
 if !self.path.exists() {
 break;
 }
 std::thread::sleep(Duration::from_millis(50));
 }
 *armed = false;
 }
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 fn origin_set(hosts: &[&str]) -> BTreeSet<String> {
 hosts.iter().map(|host| host.to_string()).collect()
 }

 fn cookie(domain: &str, name: &str) -> Value {
 json!({
 "name": name,
 "value": "v",
 "domain": domain,
 "path": "/",
 "secure": true,
 "httpOnly": true,
 "sameSite": "Lax",
 "expires": 1_900_000_000.0
 })
 }

 #[test]
 fn only_exact_host_cookies_are_captured() {
 let cookies = vec![
 cookie(".example.com", "binding"),
 cookie("example.com", "exact"),
 cookie(".sub.example.com", "sub"),
 cookie(".idp.test", "idp"),
 cookie(".com", "broad"),
 ];
 let captured = capture_cookies(&cookies, "example.com");
 let names: Vec<&str> = captured.iter().map(|c| c.name.as_str()).collect();
 assert_eq!(names, vec!["binding", "exact"]);
 assert!(captured.iter().all(|c| c.domain == "example.com"));
 }

 #[test]
 fn a_binding_subdomain_does_not_capture_the_parent_domain() {
 let cookies = vec![
 cookie(".example.com", "parent"),
 cookie(".app.example.com", "binding"),
 ];
 let captured = capture_cookies(&cookies, "app.example.com");
 let names: Vec<&str> = captured.iter().map(|c| c.name.as_str()).collect();
 assert_eq!(names, vec!["binding"]);
 }

 #[test]
 fn attributes_are_preserved_verbatim() {
 let captured = capture_cookies(&[cookie(".example.com", "s")], "example.com");
 let cookie = &captured[0];
 assert!(cookie.secure);
 assert!(cookie.http_only);
 assert_eq!(cookie.same_site.as_deref(), Some("Lax"));
 assert_eq!(cookie.path, "/");
 assert_eq!(cookie.expires, Some(1_900_000_000));
 }

 #[test]
 fn debug_never_prints_a_cookie_value() {
 let mut value = cookie(".example.com", "s");
 value["value"] = json!("SUPER-SECRET-VALUE");
 let captured = capture_cookies(&[value], "example.com");
 let rendered = format!("{:?}", captured[0]);
 assert!(!rendered.contains("SUPER-SECRET-VALUE"), "{rendered}");
 assert!(rendered.contains("<redacted>"), "{rendered}");
 let list = format!("{captured:?}");
 assert!(!list.contains("SUPER-SECRET-VALUE"), "{list}");
 }

 #[test]
 fn the_jar_payload_still_round_trips_the_secret() {
 let mut value = cookie(".example.com", "s");
 value["value"] = json!("jar-only-secret");
 let captured = capture_cookies(&[value], "example.com");
 let payload = serde_json::to_vec(&captured).expect("serialize");
 let back: Vec<CapturedCookie> = serde_json::from_slice(&payload).expect("deserialize");
 assert_eq!(back[0].value.as_str(), "jar-only-secret");
 // The value is a zeroizing buffer, not a plain String field.
 let debug = format!("{:?}", back[0]);
 assert!(!debug.contains("jar-only-secret"));
 }

 #[test]
 fn a_session_cookie_has_no_expiry() {
 let mut value = cookie(".example.com", "s");
 value["expires"] = json!(1.0);
 let captured = capture_cookies(&[value], "example.com");
 assert_eq!(captured[0].expires, None);
 }

 #[test]
 fn a_malformed_cookie_is_skipped_not_guessed() {
 let cookies = vec![json!({"name": "no-domain", "value": "v"})];
 assert!(capture_cookies(&cookies, "example.com").is_empty());
 }

 #[test]
 fn the_document_gate_allows_only_declared_origins() {
 let origins = origin_set(&["github.com", "accounts.google.com"]);
 assert!(document_allowed("github.com", &origins));
 assert!(document_allowed("GitHub.com.", &origins));
 assert!(document_allowed("accounts.google.com", &origins));
 assert!(!document_allowed("evil.test", &origins));
 assert!(!document_allowed("github.com.evil.test", &origins));
 }

 #[test]
 fn the_origin_never_prints_a_path_or_query() {
 let url = Url::parse("https://example.com/login?token=SECRET#frag").unwrap();
 assert_eq!(origin_of(&url), "https://example.com");
 let with_port = Url::parse("https://example.com:8443/a").unwrap();
 assert_eq!(origin_of(&with_port), "https://example.com:8443");
 }

 #[test]
 fn the_cookie_header_carries_name_and_value_only() {
 let captured = capture_cookies(&[cookie(".example.com", "a")], "example.com");
 assert_eq!(cookie_header(&captured), "a=v");
 }

 #[test]
 fn a_temp_profile_is_wiped_on_drop() {
 let dir = tempfile::tempdir().unwrap();
 let path;
 {
 let profile = TempProfile::new(dir.path(), "example").unwrap();
 path = profile.path().to_path_buf();
 assert!(path.exists());
 std::fs::write(path.join("Cookies"), b"x").unwrap();
 }
 assert!(!path.exists());
 }

 #[test]
 fn a_stale_profile_is_never_reused() {
 let dir = tempfile::tempdir().unwrap();
 let stale = dir.path().join("auth").join("login").join("example");
 std::fs::create_dir_all(&stale).unwrap();
 std::fs::write(stale.join("Cookies"), b"stale").unwrap();
 let profile = TempProfile::new(dir.path(), "example").unwrap();
 assert!(!profile.path().join("Cookies").exists());
 }

 #[test]
 fn a_non_session_profile_cannot_start_a_login() {
 let config = Config::default();
 let error = Login::from_config(&config, "missing").unwrap_err();
 assert_eq!(error.code().as_str(), "auth_profile_unknown");
 }
}
