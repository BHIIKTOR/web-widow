//! Browser interaction primitives (040).
//!
//! One held page per process, one action per call, observe → act → observe.
//! The manager owns validation, TTL/action bounds, wait timing and screenshot
//! persistence; the page work lives behind [`HeldSession`]. Page text and
//! console lines are untrusted data and are never executed or followed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::error::{Result, WidowError};
use crate::core::url::PageUrl;
use crate::pipeline::Pipeline;
pub use crate::render::SessionCounters;
use crate::render::{ConsoleEntry, HeldSession, ScrollTarget};

/// Hard session lifetime, independent of activity.
pub const SESSION_LIFETIME_S: u64 = 900;
/// TTL reaper tick.
pub const REAPER_TICK: Duration = Duration::from_secs(10);

const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);
const VIEWPORT_MIN: u32 = 320;
const VIEWPORT_MAX: u32 = 3840;
const DEFAULT_CONSOLE_LIMIT: usize = 50;
const MAX_CONSOLE_LIMIT: usize = 200;
const WAIT_POLL: Duration = Duration::from_millis(100);
const MAX_WAIT_MS: u64 = 10_000;
const MAX_NETWORK_QUIET_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TYPED_CHARS: usize = 2_000;
/// Screenshot budget per session ( residual, review F10).
const SESSION_SCREENSHOT_CAP: u32 = 10;
const MAX_TITLE_CHARS: usize = 300;
const MAX_FINAL_URL_CHARS: usize = 4_096;

/// `interact_open.render`: both values render in v1 (a session needs the
/// browser); `always` is the explicit guarantee form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InteractRenderPolicy {
 Auto,
 Always,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractOpenInput {
 /// Absolute http(s) URL to open in the held page.
 pub url: String,
 /// `auto` (default) or `always`; both require the renderer in v1.
 #[serde(default)]
 pub render: Option<InteractRenderPolicy>,
 /// Viewport width in CSS px, 320..=3840 (default 1280).
 #[serde(default)]
 pub viewport_width: Option<u32>,
 /// Viewport height in CSS px, 320..=3840 (default 800).
 #[serde(default)]
 pub viewport_height: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractActInput {
 pub session_id: String,
 /// Closed action object: `click`, `type`, `wait` or `scroll`. There is no
 /// `script`/`eval` field and no text-based selector.
 pub action: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractStateInput {
 pub session_id: String,
 /// Capture the held page (viewport) into the CAS; optional.
 #[serde(default)]
 pub screenshot: Option<bool>,
 /// Console lines to return, 1..=200 (default 50).
 #[serde(default)]
 pub console_limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractCloseInput {
 pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractOpenPayload {
 pub session_id: String,
 pub final_url: String,
 pub status: u16,
 pub title: Option<String>,
 pub expires_at: String,
 pub actions_remaining: u32,
 pub console: Vec<ConsoleEntry>,
}

/// A request the session refused, surfaced on the action that caused it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractBlocked {
 pub reason: String,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractActPayload {
 pub session_id: String,
 /// The executed action, with any typed text omitted.
 pub action: serde_json::Value,
 pub ok: bool,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub blocked: Option<InteractBlocked>,
 pub final_url: String,
 pub title: Option<String>,
 pub console_tail: Vec<ConsoleEntry>,
 pub actions_remaining: u32,
 pub expires_at: String,
 /// True when the page navigated to a different origin since the last call.
 pub origin_changed: bool,
 /// The previous origin, only when `origin_changed`.
 #[serde(skip_serializing_if = "Option::is_none")]
 pub previous_origin: Option<String>,
 /// `title` or `final_url` when a returned field was capped.
 #[serde(skip_serializing_if = "Option::is_none")]
 pub limit_hit: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub typed_chars: Option<usize>,
 /// `"not_followed"` only when a popup attempt was observed.
 #[serde(skip_serializing_if = "Option::is_none")]
 pub popup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScreenshotRef {
 pub hash: String,
 pub bytes: u64,
 pub width: Option<u32>,
 pub height: Option<u32>,
 pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractStatePayload {
 pub session_id: String,
 pub final_url: String,
 pub title: Option<String>,
 pub status: u16,
 pub console: Vec<ConsoleEntry>,
 pub actions_remaining: u32,
 pub expires_at: String,
 pub origin_changed: bool,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub previous_origin: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub limit_hit: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub screenshot: Option<ScreenshotRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractClosePayload {
 pub session_id: String,
 pub closed: bool,
 pub already_closed: bool,
}

/// One closed action, validated from the caller's JSON.
#[derive(Debug)]
enum ActionSpec {
 Click {
 selector: String,
 index: usize,
 },
 Type {
 selector: String,
 text: String,
 index: usize,
 clear: bool,
 },
 Wait {
 selector: Option<String>,
 ms: Option<u64>,
 network_quiet_ms: Option<u64>,
 timeout_ms: u64,
 },
 Scroll {
 to: ScrollTo,
 selector: Option<String>,
 },
}

#[derive(Debug, Clone, Copy)]
enum ScrollTo {
 Element,
 Top,
 Bottom,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawAction {
 Click {
 selector: String,
 #[serde(default)]
 index: Option<usize>,
 },
 Type {
 selector: String,
 text: String,
 #[serde(default)]
 index: Option<usize>,
 #[serde(default)]
 clear: Option<bool>,
 },
 Wait {
 #[serde(default)]
 selector: Option<String>,
 #[serde(default)]
 ms: Option<u64>,
 #[serde(default)]
 network_quiet_ms: Option<u64>,
 #[serde(default)]
 timeout_ms: Option<u64>,
 },
 Scroll {
 to: RawScrollTo,
 #[serde(default)]
 selector: Option<String>,
 },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawScrollTo {
 Element,
 Top,
 Bottom,
}

impl ActionSpec {
 fn parse(value: &serde_json::Value) -> Result<Self> {
 let raw: RawAction =
 serde_json::from_value(value.clone()).map_err(|error| WidowError::SchemaInvalid {
 pointer: "/action".to_string(),
 message: error.to_string(),
 })?;
 match raw {
 RawAction::Click { selector, index } => {
 require_selector(&selector, "/action/selector")?;
 Ok(Self::Click {
 selector,
 index: index.unwrap_or(0),
 })
 }
 RawAction::Type {
 selector,
 text,
 index,
 clear,
 } => {
 require_selector(&selector, "/action/selector")?;
 let chars = text.chars().count();
 if !(1..=MAX_TYPED_CHARS).contains(&chars) {
 return Err(WidowError::SchemaInvalid {
 pointer: "/action/text".to_string(),
 message: format!(
 "text must be 1..={MAX_TYPED_CHARS} characters (got {chars})"
),
 });
 }
 Ok(Self::Type {
 selector,
 text,
 index: index.unwrap_or(0),
 clear: clear.unwrap_or(true),
 })
 }
 RawAction::Wait {
 selector,
 ms,
 network_quiet_ms,
 timeout_ms,
 } => {
 let modes = usize::from(selector.is_some())
 + usize::from(ms.is_some())
 + usize::from(network_quiet_ms.is_some());
 if modes != 1 {
 return Err(WidowError::SchemaInvalid {
 pointer: "/action".to_string(),
 message: "wait needs exactly one of selector, ms or network_quiet_ms"
.to_string(),
 });
 }
 if let Some(selector) = &selector {
 require_selector(selector, "/action/selector")?;
 }
 if let Some(ms) = ms
 && ms > MAX_WAIT_MS
 {
 return Err(WidowError::SchemaInvalid {
 pointer: "/action/ms".to_string(),
 message: format!("ms must be 0..={MAX_WAIT_MS}"),
 });
 }
 if let Some(quiet) = network_quiet_ms
 && !(1..=MAX_NETWORK_QUIET_MS).contains(&quiet)
 {
 return Err(WidowError::SchemaInvalid {
 pointer: "/action/network_quiet_ms".to_string(),
 message: format!("network_quiet_ms must be 1..={MAX_NETWORK_QUIET_MS}"),
 });
 }
 let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
 if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
 return Err(WidowError::SchemaInvalid {
 pointer: "/action/timeout_ms".to_string(),
 message: format!("timeout_ms must be 1..={MAX_TIMEOUT_MS}"),
 });
 }
 Ok(Self::Wait {
 selector,
 ms,
 network_quiet_ms,
 timeout_ms,
 })
 }
 RawAction::Scroll { to, selector } => {
 let to = match to {
 RawScrollTo::Element => {
 let selector =
 selector.clone().ok_or_else(|| WidowError::SchemaInvalid {
 pointer: "/action/selector".to_string(),
 message: "scroll to element needs a selector".to_string(),
 })?;
 require_selector(&selector, "/action/selector")?;
 ScrollTo::Element
 }
 RawScrollTo::Top => ScrollTo::Top,
 RawScrollTo::Bottom => ScrollTo::Bottom,
 };
 Ok(Self::Scroll { to, selector })
 }
 }
 }

 /// The result echo. Typed text is deliberately absent ().
 fn describe(&self) -> serde_json::Value {
 match self {
 Self::Click { selector, index } => {
 serde_json::json!({"kind": "click", "selector": selector, "index": index})
 }
 Self::Type {
 selector,
 index,
 clear,
..
 } => serde_json::json!({
 "kind": "type",
 "selector": selector,
 "index": index,
 "clear": clear,
 }),
 Self::Wait {
 selector,
 ms,
 network_quiet_ms,
 timeout_ms,
 } => serde_json::json!({
 "kind": "wait",
 "selector": selector,
 "ms": ms,
 "network_quiet_ms": network_quiet_ms,
 "timeout_ms": timeout_ms,
 }),
 Self::Scroll { to, selector } => {
 let to = match to {
 ScrollTo::Element => "element",
 ScrollTo::Top => "top",
 ScrollTo::Bottom => "bottom",
 };
 serde_json::json!({"kind": "scroll", "to": to, "selector": selector})
 }
 }
 }
}

fn require_selector(selector: &str, pointer: &str) -> Result<()> {
 if selector.trim().is_empty() {
 return Err(WidowError::SchemaInvalid {
 pointer: pointer.to_string(),
 message: "selector must not be empty".to_string(),
 });
 }
 Ok(())
}

#[derive(Debug)]
struct SessionEntry {
 id: String,
 page: Box<dyn HeldSession>,
 url: String,
 origin: String,
 created: Instant,
 last_used: Instant,
 actions: u32,
 screenshots: u32,
}

#[derive(Debug)]
struct Inner {
 config: Arc<Config>,
 pipeline: Arc<Pipeline>,
 slot: Mutex<Option<SessionEntry>>,
 current: std::sync::Mutex<Option<String>>,
 last_closed: std::sync::Mutex<Option<String>>,
 reaper: AtomicBool,
 shutdown: CancellationToken,
}

/// The process-wide interaction manager. One live session at a time.
#[derive(Debug, Clone)]
pub struct InteractionManager {
 inner: Arc<Inner>,
}

impl InteractionManager {
 pub fn new(config: Arc<Config>, pipeline: Arc<Pipeline>) -> Self {
 Self {
 inner: Arc::new(Inner {
 config,
 pipeline,
 slot: Mutex::new(None),
 current: std::sync::Mutex::new(None),
 last_closed: std::sync::Mutex::new(None),
 reaper: AtomicBool::new(false),
 shutdown: CancellationToken::new(),
 }),
 }
 }

 pub async fn open(&self, input: InteractOpenInput) -> Result<InteractOpenPayload> {
 if cfg!(not(feature = "render")) {
 return Err(static_build_error());
 }
 let page = PageUrl::parse(&input.url)?;
 let width = input.viewport_width.unwrap_or(DEFAULT_VIEWPORT.0);
 let height = input.viewport_height.unwrap_or(DEFAULT_VIEWPORT.1);
 for (name, value) in [("viewport_width", width), ("viewport_height", height)] {
 if !(VIEWPORT_MIN..=VIEWPORT_MAX).contains(&value) {
 return Err(WidowError::SchemaInvalid {
 pointer: format!("/{name}"),
 message: format!("{name} must be {VIEWPORT_MIN}..={VIEWPORT_MAX}"),
 });
 }
 }

 // Holding the slot across the open serializes concurrent opens; any
 // other call sees `session_busy` instead of a second page.
 let mut slot = match self.inner.slot.try_lock() {
 Ok(slot) => slot,
 Err(_) => {
 return Err(WidowError::SessionBusy {
 session_id: self
.current_id()
.map(|id| session_prefix(&id).to_string())
.unwrap_or_else(|| "opening".to_string()),
 });
 }
 };
 if let Some(entry) = slot.as_ref()
 && !self.expired(entry)
 {
 return Err(WidowError::SessionBusy {
 session_id: session_prefix(&entry.id).to_string(),
 });
 }
 if let Some(entry) = slot.take() {
 close_entry(entry).await;
 }
 self.ensure_reaper();

 let session = self
.inner
.pipeline
.open_interaction_session(&page, (width, height))
.await?;
 let status = session.status();
 if !(200..300).contains(&status) {
 let _ = session.close().await;
 return Err(WidowError::Http {
 status: Some(status),
 detail: format!("origin returned HTTP {status} when opening the session"),
 });
 }
 let final_url = session
.url()
.await?
.unwrap_or_else(|| page.url().to_string());
 let title = session.title().await?;
 let console = session.console(DEFAULT_CONSOLE_LIMIT).await?;
 let now = Instant::now();
 let id = uuid::Uuid::now_v7().to_string();
 let entry = SessionEntry {
 id: id.clone(),
 page: session,
 url: final_url.clone(),
 origin: origin_of(&final_url).unwrap_or_default(),
 created: now,
 last_used: now,
 actions: 0,
 screenshots: 0,
 };
 let actions_remaining = self.max_actions();
 let expires_at = expires_at(&entry, self.ttl());
 *slot = Some(entry);
 self.set_current(Some(id.clone()));
 tracing::info!(
 session = %session_prefix(&id),
 url = %crate::pipeline::log_url(&final_url, self.inner.config.logging.redact_query),
 "interaction session opened"
);
 Ok(InteractOpenPayload {
 session_id: id,
 final_url,
 status,
 title,
 expires_at,
 actions_remaining,
 console,
 })
 }

 pub async fn act(&self, input: InteractActInput) -> Result<InteractActPayload> {
 if cfg!(not(feature = "render")) {
 return Err(static_build_error());
 }
 let spec = ActionSpec::parse(&input.action)?;
 let mut slot = match self.inner.slot.try_lock() {
 Ok(slot) => slot,
 Err(_) => {
 return Err(WidowError::SessionBusy {
 session_id: session_prefix(&input.session_id).to_string(),
 });
 }
 };
 let Some(entry) = slot.as_mut() else {
 return Err(WidowError::SessionNotFound {
 session_id: input.session_id.clone(),
 });
 };
 if entry.id != input.session_id {
 return Err(WidowError::SessionNotFound {
 session_id: input.session_id.clone(),
 });
 }
 if self.expired(entry) {
 let entry = slot.take().expect("entry was just observed");
 close_entry(entry).await;
 self.set_current(None);
 return Err(WidowError::SessionExpired {
 session_id: input.session_id,
 });
 }
 let max_actions = self.max_actions();
 if entry.actions >= max_actions {
 return Err(WidowError::LimitExceeded(format!(
 "session action cap ({max_actions}) reached; open a new session"
)));
 }

 // One coherent window per action: a refusal from before the action is
 // not attributed to it.
 let _ = entry.page.take_blocked().await?;
 let descriptor = spec.describe();
 let typed_chars = match &spec {
 ActionSpec::Click { selector, index } => {
 entry.page.click(selector, *index).await?;
 None
 }
 ActionSpec::Type {
 selector,
 text,
 index,
 clear,
 } => Some(entry.page.type_text(selector, *index, text, *clear).await?),
 ActionSpec::Wait {.. } => {
 self.perform_wait(entry.page.as_ref(), &spec).await?;
 None
 }
 ActionSpec::Scroll { to, selector } => {
 let target = match to {
 ScrollTo::Element => {
 ScrollTarget::Element(selector.as_deref().unwrap_or_default())
 }
 ScrollTo::Top => ScrollTarget::Top,
 ScrollTo::Bottom => ScrollTarget::Bottom,
 };
 entry.page.scroll(target).await?;
 None
 }
 };
 let blocked = entry
.page
.take_blocked()
.await?
.map(|blocked| InteractBlocked {
 reason: blocked.reason,
 detail: blocked.detail,
 });
 entry.actions += 1;
 entry.last_used = Instant::now();
 let raw_final_url = entry.page.url().await?.unwrap_or_else(|| entry.url.clone());
 entry.url = raw_final_url.clone();
 let (final_url, url_truncated) = truncate_field(&raw_final_url, MAX_FINAL_URL_CHARS);
 let (title, title_truncated) = match entry.page.title().await? {
 Some(title) => {
 let (title, truncated) = truncate_field(&title, MAX_TITLE_CHARS);
 (Some(title), truncated)
 }
 None => (None, false),
 };
 let (origin_changed, previous_origin) = track_origin(entry, &raw_final_url);
 let limit_hit = if title_truncated {
 Some("title".to_string())
 } else if url_truncated {
 Some("final_url".to_string())
 } else {
 None
 };
 let console_tail = entry.page.console(DEFAULT_CONSOLE_LIMIT).await?;
 let popup = entry
.page
.popup_opened()
.then(|| "not_followed".to_string());
 let actions_remaining = max_actions.saturating_sub(entry.actions);
 let expires_at = expires_at(entry, self.ttl());
 tracing::info!(
 session = %session_prefix(&entry.id),
 action = %descriptor["kind"].as_str().unwrap_or("unknown"),
 actions_remaining,
 blocked = blocked.as_ref().map(|b| b.reason.as_str()).unwrap_or("-"),
 "interaction action completed"
);
 Ok(InteractActPayload {
 session_id: entry.id.clone(),
 action: descriptor,
 ok: true,
 blocked,
 final_url,
 title,
 console_tail,
 actions_remaining,
 expires_at,
 origin_changed,
 previous_origin,
 limit_hit,
 typed_chars,
 popup,
 })
 }

 pub async fn state(&self, input: InteractStateInput) -> Result<InteractStatePayload> {
 if cfg!(not(feature = "render")) {
 return Err(static_build_error());
 }
 let limit = input.console_limit.unwrap_or(DEFAULT_CONSOLE_LIMIT);
 if !(1..=MAX_CONSOLE_LIMIT).contains(&limit) {
 return Err(WidowError::SchemaInvalid {
 pointer: "/console_limit".to_string(),
 message: format!("console_limit must be 1..={MAX_CONSOLE_LIMIT}"),
 });
 }
 let mut slot = match self.inner.slot.try_lock() {
 Ok(slot) => slot,
 Err(_) => {
 return Err(WidowError::SessionBusy {
 session_id: session_prefix(&input.session_id).to_string(),
 });
 }
 };
 let Some(entry) = slot.as_mut() else {
 return Err(WidowError::SessionNotFound {
 session_id: input.session_id.clone(),
 });
 };
 if entry.id != input.session_id {
 return Err(WidowError::SessionNotFound {
 session_id: input.session_id.clone(),
 });
 }
 if self.expired(entry) {
 let entry = slot.take().expect("entry was just observed");
 close_entry(entry).await;
 self.set_current(None);
 return Err(WidowError::SessionExpired {
 session_id: input.session_id,
 });
 }
 let console = entry.page.console(limit).await?;
 let raw_final_url = entry.page.url().await?.unwrap_or_else(|| entry.url.clone());
 let (final_url, url_truncated) = truncate_field(&raw_final_url, MAX_FINAL_URL_CHARS);
 let (title, title_truncated) = match entry.page.title().await? {
 Some(title) => {
 let (title, truncated) = truncate_field(&title, MAX_TITLE_CHARS);
 (Some(title), truncated)
 }
 None => (None, false),
 };
 let (origin_changed, previous_origin) = track_origin(entry, &raw_final_url);
 let limit_hit = if title_truncated {
 Some("title".to_string())
 } else if url_truncated {
 Some("final_url".to_string())
 } else {
 None
 };
 let status = entry.page.status();
 let screenshot = if input.screenshot.unwrap_or(false) {
 if entry.screenshots >= SESSION_SCREENSHOT_CAP {
 return Err(WidowError::LimitExceeded(format!(
 "session screenshot budget ({SESSION_SCREENSHOT_CAP}) reached; open a new session"
)));
 }
 let shot = entry.page.screenshot(false).await?;
 let page = PageUrl::parse(&final_url).ok();
 let stored = self
.inner
.pipeline
.store_capture(
 page.as_ref(),
 &shot.png,
 shot.width,
 shot.height,
 "interaction")
.await?;
 entry.screenshots += 1;
 Some(ScreenshotRef {
 hash: stored.hash,
 bytes: stored.bytes,
 width: shot.width,
 height: shot.height,
 mime: "image/png".to_string(),
 })
 } else {
 None
 };
 entry.last_used = Instant::now();
 entry.url = raw_final_url;
 Ok(InteractStatePayload {
 session_id: entry.id.clone(),
 final_url,
 title,
 status,
 console,
 actions_remaining: self.max_actions().saturating_sub(entry.actions),
 expires_at: expires_at(entry, self.ttl()),
 origin_changed,
 previous_origin,
 limit_hit,
 screenshot,
 })
 }

 pub async fn close(&self, input: InteractCloseInput) -> Result<InteractClosePayload> {
 if cfg!(not(feature = "render")) {
 return Err(static_build_error());
 }
 let mut slot = self.inner.slot.lock().await;
 match slot.as_ref() {
 Some(entry) if entry.id == input.session_id => {
 let expired = self.expired(entry);
 let entry = slot.take().expect("entry was just observed");
 close_entry(entry).await;
 self.set_current(None);
 self.set_last_closed(Some(input.session_id.clone()));
 Ok(InteractClosePayload {
 session_id: input.session_id,
 closed: true,
 already_closed: expired,
 })
 }
 Some(_) => Err(WidowError::SessionNotFound {
 session_id: input.session_id,
 }),
 None if self.last_closed_id().as_deref() == Some(input.session_id.as_str()) => {
 Ok(InteractClosePayload {
 session_id: input.session_id,
 closed: true,
 already_closed: true,
 })
 }
 None => Err(WidowError::SessionNotFound {
 session_id: input.session_id,
 }),
 }
 }

 /// Live counters for one session (tests and diagnostics; never a tool).
 pub async fn counters(&self, session_id: &str) -> Result<SessionCounters> {
 let slot = match self.inner.slot.try_lock() {
 Ok(slot) => slot,
 Err(_) => {
 return Err(WidowError::SessionBusy {
 session_id: session_prefix(session_id).to_string(),
 });
 }
 };
 let Some(entry) = slot.as_ref() else {
 return Err(WidowError::SessionNotFound {
 session_id: session_id.to_string(),
 });
 };
 if entry.id != session_id {
 return Err(WidowError::SessionNotFound {
 session_id: session_id.to_string(),
 });
 }
 Ok(entry.page.counters())
 }

 /// Close the live session and stop the reaper (server shutdown).
 ///
 /// A long action holds the slot; shutdown must not wait for it (the target
 /// is a ≤10 s exit), so an in-flight action leaves the page to the browser
 /// shutdown that follows.
 pub async fn shutdown(&self) {
 self.inner.shutdown.cancel();
 self.set_current(None);
 let Ok(mut slot) = self.inner.slot.try_lock() else {
 tracing::warn!(
 "interaction shutdown while an action is in flight; the browser close ends the page"
);
 return;
 };
 if let Some(entry) = slot.take() {
 close_entry(entry).await;
 }
 }

 async fn perform_wait(&self, page: &dyn HeldSession, spec: &ActionSpec) -> Result<()> {
 let ActionSpec::Wait {
 selector,
 ms,
 network_quiet_ms,
 timeout_ms,
 } = spec
 else {
 return Err(WidowError::internal(
 "perform_wait called for a non-wait action"));
 };
 let deadline = Instant::now() + Duration::from_millis(*timeout_ms);
 if let Some(ms) = ms {
 let sleep = tokio::time::sleep(Duration::from_millis(*ms));
 tokio::pin!(sleep);
 tokio::select! {
 () = &mut sleep => Ok(()),
 () = tokio::time::sleep_until(deadline) => Err(wait_timeout("ms")),
 }
 } else if let Some(selector) = selector {
 loop {
 if page.has_selector(selector).await? {
 return Ok(());
 }
 if Instant::now() >= deadline {
 return Err(wait_timeout("selector"));
 }
 tokio::time::sleep(WAIT_POLL).await;
 }
 } else if let Some(quiet_ms) = network_quiet_ms {
 let quiet = Duration::from_millis(*quiet_ms);
 let mut quiet_since = Instant::now();
 loop {
 if page.pending_requests() == 0 {
 if quiet_since.elapsed() >= quiet {
 return Ok(());
 }
 } else {
 quiet_since = Instant::now();
 }
 if Instant::now() >= deadline {
 return Err(wait_timeout("network_quiet_ms"));
 }
 tokio::time::sleep(Duration::from_millis(50)).await;
 }
 } else {
 Err(WidowError::internal("wait has no mode"))
 }
 }

 fn ensure_reaper(&self) {
 if self.inner.reaper.swap(true, Ordering::SeqCst) {
 return;
 }
 let inner = Arc::clone(&self.inner);
 tokio::spawn(async move {
 loop {
 tokio::select! {
 () = inner.shutdown.cancelled() => break,
 () = tokio::time::sleep(REAPER_TICK) => Inner::reap(&inner).await,
 }
 }
 });
 }

 fn ttl(&self) -> Duration {
 Duration::from_secs(self.inner.config.interaction.ttl_s)
 }

 fn max_actions(&self) -> u32 {
 self.inner.config.interaction.max_actions
 }

 fn expired(&self, entry: &SessionEntry) -> bool {
 let ttl = self.ttl();
 entry.last_used.elapsed() >= ttl
 || entry.created.elapsed() >= Duration::from_secs(SESSION_LIFETIME_S)
 }

 fn current_id(&self) -> Option<String> {
 self.inner
.current
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner())
.clone()
 }

 fn set_current(&self, id: Option<String>) {
 *self
.inner
.current
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
 }

 fn last_closed_id(&self) -> Option<String> {
 self.inner
.last_closed
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner())
.clone()
 }

 fn set_last_closed(&self, id: Option<String>) {
 *self
.inner
.last_closed
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
 }
}

impl Inner {
 async fn reap(inner: &Inner) {
 let Ok(mut slot) = inner.slot.try_lock() else {
 return;
 };
 let expired = match slot.as_ref() {
 Some(entry) => {
 let ttl = Duration::from_secs(inner.config.interaction.ttl_s);
 entry.last_used.elapsed() >= ttl
 || entry.created.elapsed() >= Duration::from_secs(SESSION_LIFETIME_S)
 }
 None => false,
 };
 if !expired {
 return;
 }
 if let Some(entry) = slot.take() {
 tracing::info!(session = %session_prefix(&entry.id), "interaction session expired");
 close_entry(entry).await;
 *inner
.current
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
 }
 }
}

fn session_prefix(id: &str) -> &str {
 let end = id
.char_indices()
.nth(8)
.map(|(index, _)| index)
.unwrap_or(id.len());
 &id[..end]
}

fn origin_of(url: &str) -> Option<String> {
 let url = url::Url::parse(url).ok()?;
 let host = url.host_str()?;
 let port = url.port_or_known_default()?;
 Some(format!("{}://{host}:{port}", url.scheme()))
}

fn track_origin(entry: &mut SessionEntry, url: &str) -> (bool, Option<String>) {
 let Some(origin) = origin_of(url) else {
 return (false, None);
 };
 if origin == entry.origin {
 (false, None)
 } else {
 let previous = std::mem::replace(&mut entry.origin, origin);
 (true, Some(previous))
 }
}

fn truncate_field(value: &str, max_chars: usize) -> (String, bool) {
 let cleaned = strip_control_chars(value);
 crate::core::truncate::truncate_chars(&cleaned, max_chars)
}

/// Page-controlled strings must not carry terminal/control escapes into the
/// tool output. Newlines and tabs are kept; every other control character is
/// dropped.
pub(crate) fn strip_control_chars(value: &str) -> String {
 value
.chars()
.filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
.collect()
}

fn static_build_error() -> WidowError {
 WidowError::RenderUnavailable(
 "this build has no `render` feature; browser interaction needs Chrome".to_string())
}

fn wait_timeout(mode: &str) -> WidowError {
 WidowError::ActionFailed {
 reason: "timeout".to_string(),
 message: format!("wait ({mode}) did not finish before timeout_ms"),
 }
}

async fn close_entry(entry: SessionEntry) {
 if let Err(error) = entry.page.close().await {
 tracing::warn!(session = %session_prefix(&entry.id), error = %error, "session close failed");
 }
}

fn expires_at(entry: &SessionEntry, ttl: Duration) -> String {
 let lifetime = Duration::from_secs(SESSION_LIFETIME_S);
 let deadline = (entry.last_used + ttl).min(entry.created + lifetime);
 let remaining = deadline.saturating_duration_since(Instant::now());
 let at = Utc::now()
 + chrono::Duration::from_std(remaining).unwrap_or_else(|_| chrono::Duration::zero());
 at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
 use super::*;

 fn action(value: serde_json::Value) -> Result<ActionSpec> {
 ActionSpec::parse(&value)
 }

 #[test]
 fn log_helpers_redact_secrets_and_never_emit_a_full_session_id() {
 let url = crate::pipeline::log_url("https://example.com/cb?token=super-secret", true);
 assert!(url.contains("<redacted>"), "{url}");
 assert!(!url.contains("super-secret"), "{url}");
 let plain = crate::pipeline::log_url("https://example.com/plain", true);
 assert_eq!(plain, "https://example.com/plain");

 let id = "0192abcd-1234-7abc-8def-0123456789ab";
 assert_eq!(session_prefix(id), "0192abcd");
 let error = WidowError::SessionBusy {
 session_id: session_prefix(id).to_string(),
 };
 let tool = error.to_tool_error();
 assert!(!tool.message.contains(id), "{}", tool.message);
 assert_eq!(tool.detail.as_deref(), Some("session_id=0192abcd"));
 }

 #[test]
 fn page_controlled_fields_drop_control_characters() {
 let (cleaned, _) = truncate_field("A\u{1b}B\u{7}C\u{0}D", 100);
 assert_eq!(cleaned, "ABCD");
 let (kept, _) = truncate_field("line one\nline two\tend", 100);
 assert_eq!(kept, "line one\nline two\tend");
 }

 #[test]
 fn typed_text_never_appears_in_the_result_echo() {
 let spec = action(serde_json::json!({
 "kind": "type",
 "selector": "input[name=q]",
 "text": "super-secret-value",
 }))
.unwrap();
 let echo = spec.describe().to_string();
 assert!(!echo.contains("super-secret-value"), "{echo}");
 assert!(echo.contains("input[name=q]"), "{echo}");
 }

 #[test]
 fn the_action_set_is_closed_and_has_no_caller_js() {
 for bad in [
 serde_json::json!({"kind": "eval", "code": "alert(1)"}),
 serde_json::json!({"kind": "click", "selector": "#x", "script": "alert(1)"}),
 serde_json::json!({"kind": "click", "selector": "#x", "text": "click me"}),
 serde_json::json!({"kind": "scroll", "to": "sideways"}),
 ] {
 let error = action(bad.clone()).expect_err("must be rejected");
 assert_eq!(
 error.code(),
 crate::core::error::ErrorCode::SchemaInvalid,
 "{bad}"
);
 }
 assert!(action(serde_json::json!({"kind": "click", "selector": "#ok"})).is_ok());
 assert!(action(serde_json::json!({"kind": "scroll", "to": "top"})).is_ok());
 assert!(action(serde_json::json!({"kind": "wait", "ms": 100})).is_ok());
 }

 #[test]
 fn wait_needs_exactly_one_mode_and_respects_bounds() {
 assert!(action(serde_json::json!({"kind": "wait"})).is_err());
 assert!(action(serde_json::json!({"kind": "wait", "ms": 10, "selector": "#x"})).is_err());
 assert!(action(serde_json::json!({"kind": "wait", "ms": 10_001})).is_err());
 assert!(action(serde_json::json!({"kind": "wait", "network_quiet_ms": 5_001})).is_err());
 assert!(
 action(serde_json::json!({"kind": "wait", "ms": 10, "timeout_ms": 30_001})).is_err()
);
 }

 #[test]
 fn type_text_is_bounded() {
 let long = "x".repeat(MAX_TYPED_CHARS + 1);
 assert!(
 action(serde_json::json!({"kind": "type", "selector": "#q", "text": long})).is_err()
);
 assert!(action(serde_json::json!({"kind": "type", "selector": "#q", "text": ""})).is_err());
 }
}
