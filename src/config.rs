//! Configuration: one TOML file, environment overrides for the two locations
//! that must be relocatable, and CLI flags for the same.
//!
//! Policy that widens reach (address allowlist, robots override) is readable
//! only from the config file — never from the environment, CLI or a tool
//! argument — so an agent cannot widen its own scope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::error::{Result, WidowError};

pub const ENV_CONFIG: &str = "WEB_WIDOW_CONFIG";
pub const ENV_DATA: &str = "WEB_WIDOW_DATA";
pub const ENV_CONTACT: &str = "WEB_WIDOW_CONTACT";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
 pub fetch: FetchConfig,
 pub robots: RobotsConfig,
 pub extract: ExtractConfig,
 pub storage: StorageConfig,
 pub logging: LoggingConfig,
 pub retention: RetentionConfig,
 pub render: RenderConfig,
 pub crawl: CrawlConfig,
 pub monitor: MonitorConfig,
 pub search: SearchConfig,
 pub feed: FeedConfig,
 pub sitemap: SitemapConfig,
 pub pdf: PdfConfig,
 pub events: EventsConfig,
 pub fts: FtsConfig,
 pub interaction: InteractionConfig,
 pub embeddings: EmbeddingsConfig,
 pub webhooks: WebhooksConfig,
 pub scheduler: SchedulerConfig,
 pub http: HttpConfig,
 pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchConfig {
 /// Exact hostnames or CIDRs allowed despite the SSRF gate. Config file only.
 pub allow_hosts: Vec<String>,
 /// Contact for the User-Agent; a missing contact warns at startup.
 pub contact: Option<String>,
 pub connect_timeout_ms: u64,
 pub read_timeout_ms: u64,
 pub total_timeout_ms: u64,
 pub max_redirects: usize,
 pub max_body_bytes: u64,
 pub max_header_bytes: u64,
 pub min_interval_ms: u64,
 pub jitter_ms: u64,
 pub max_concurrent_per_host: usize,
 pub retries_retry_after: u32,
 pub retries_5xx: u32,
 pub max_retry_after_s: u64,
}

impl Default for FetchConfig {
 fn default() -> Self {
 Self {
 allow_hosts: Vec::new(),
 contact: None,
 connect_timeout_ms: 10_000,
 read_timeout_ms: 15_000,
 total_timeout_ms: 30_000,
 max_redirects: 5,
 max_body_bytes: 10 * 1024 * 1024,
 max_header_bytes: 64 * 1024,
 min_interval_ms: 1_000,
 jitter_ms: 500,
 max_concurrent_per_host: 1,
 retries_retry_after: 2,
 retries_5xx: 1,
 max_retry_after_s: 60,
 }
 }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RobotsConfig {
 /// The only override of robots enforcement; logs loudly at startup.
 pub enabled: bool,
 pub user_agent: String,
 pub max_body_bytes: u64,
 pub success_ttl_s: u64,
 pub failure_ttl_s: u64,
}

impl Default for RobotsConfig {
 fn default() -> Self {
 Self {
 enabled: true,
 user_agent: "web-widow".to_string(),
 max_body_bytes: 512 * 1024,
 success_ttl_s: 24 * 60 * 60,
 failure_ttl_s: 60 * 60,
 }
 }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
 pub default_mode: String,
 /// Safety ceiling for returned Markdown; a per-call `max_chars` cannot raise it.
 pub max_chars: usize,
 pub max_links: usize,
 /// Safety ceiling for records returned by `extract` (5 000).
 pub max_records: usize,
 /// Per-string-value cap for `extract` (). A per-call value can
 /// lower it, never raise it.
 pub max_field_chars: usize,
 /// How long `extract` with `use_cache: true` may serve a stored result without
 /// revalidating the page. `0` always revalidates.
 pub cache_freshness_s: u64,
}

impl Default for ExtractConfig {
 fn default() -> Self {
 Self {
 default_mode: "main".to_string(),
 max_chars: 200_000,
 max_links: 2_000,
 max_records: 5_000,
 max_field_chars: 16_384,
 cache_freshness_s: 300,
 }
 }
}

/// Bulk-crawl ceilings (IMP-B04). Per-job `limits` may lower
/// these but never raise them.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlConfig {
 /// Hard page ceiling per job (5 000).
 pub max_pages: usize,
 /// Hard depth ceiling per job.
 pub max_depth: u8,
 /// Hard duration ceiling per job.
 pub max_duration_s: u64,
 /// Default per-host request rate for a job when it does not set one.
 pub default_per_host_rate: f64,
 /// A leased URL is requeued when its lease expires (crash recovery window).
 pub lease_s: u64,
 /// Attempts before a URL becomes `failed` with its last error.
 pub max_attempts: u32,
 /// Error samples kept in the job counters (contract: max 5).
 pub max_error_samples: usize,
 /// `crawl_results` page size default and cap.
 pub result_limit_default: usize,
 pub result_limit_max: usize,
 /// How long a worker waits before re-checking a frontier whose only rows are
 /// leased (e.g. by a previous process whose leases have not expired yet).
 pub idle_poll_ms: u64,
}

impl Default for CrawlConfig {
 fn default() -> Self {
 Self {
 max_pages: 5_000,
 max_depth: 5,
 max_duration_s: 1_800,
 default_per_host_rate: 1.0,
 lease_s: 30,
 max_attempts: 3,
 max_error_samples: 5,
 result_limit_default: 50,
 result_limit_max: 500,
 idle_poll_ms: 250,
 }
 }
}

/// Change monitoring (IMP-B05). Thresholds are our semantics;
/// `similar` supplies only the diff.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
 /// Interval used when `monitor_create` does not set one.
 pub default_interval_s: u64,
 /// Shortest allowed check interval; creates a bounded request rate.
 pub min_interval_s: u64,
 /// Longest allowed check interval.
 pub max_interval_s: u64,
 /// Unified diff cap. Stats always carry the full counts.
 pub max_diff_bytes: usize,
 /// A whitespace-only change touching at most this many lines is `cosmetic`.
 /// A changed word or a watched field is `substantive` regardless.
 pub cosmetic_max_changed_lines: usize,
 /// `ignore_selectors` per monitor (parse cost is bounded).
 pub max_ignore_selectors: usize,
 /// Bound on one bulk check (`monitor_check(all=true)` / `run-due`).
 pub max_checks_per_call: usize,
 /// `monitor_changes` page size default and cap.
 pub changes_limit_default: usize,
 pub changes_limit_max: usize,
 /// Upper bound for `monitor_check.wait_ms`; the inline wait never exceeds it.
 pub max_wait_ms: u64,
}

impl Default for MonitorConfig {
 fn default() -> Self {
 Self {
 default_interval_s: 86_400,
 min_interval_s: 60,
 max_interval_s: 2_592_000,
 max_diff_bytes: 20 * 1024,
 cosmetic_max_changed_lines: 2,
 max_ignore_selectors: 32,
 max_checks_per_call: 500,
 changes_limit_default: 50,
 changes_limit_max: 500,
 max_wait_ms: 30_000,
 }
 }
}

/// Discovery endpoints (IMP-B01 / §1.2). Endpoints are server
/// config, never tool arguments: an agent cannot repoint the parser at another
/// site. Both URLs are `Url`-validated at config load.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
 /// Primary DuckDuckGo HTML endpoint.
 pub html_url: String,
 /// Fallback DuckDuckGo Lite endpoint; used only on transport/parse failure.
 pub lite_url: String,
 /// Results returned when the caller omits `limit` (1..=10).
 pub default_limit: usize,
 /// Hard query-length ceiling; over-length is `schema_invalid`.
 pub max_query_chars: usize,
}

/// `[feed]` — `fetch_feed` limits ( §4).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedConfig {
 /// Items returned when the caller omits `limit` (1..=max_limit).
 pub default_limit: usize,
 /// Hard per-call ceiling; larger requests are clamped and reported.
 pub max_limit: usize,
 /// Summary/description character cap; cuts are counted.
 pub max_summary_chars: usize,
}

impl Default for FeedConfig {
 fn default() -> Self {
 Self {
 default_limit: 20,
 max_limit: 100,
 max_summary_chars: 2_000,
 }
 }
}

/// `[sitemap]` — `sitemap` / `crawl_start.sitemap_urls` ceilings
/// (). Bounds the fetched body, the decompressed size at 4×, the
/// URL entries and the index children.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SitemapConfig {
 /// Fetched body ceiling in bytes (1..=64 MiB). Decompression is capped at 4×.
 pub max_bytes: u64,
 /// URL entries kept per `sitemap` call (1..=50 000).
 pub max_urls: usize,
 /// Child sitemaps fetched from one index (1..=10).
 pub max_children: usize,
}

impl Default for SitemapConfig {
 fn default() -> Self {
 Self {
 max_bytes: 20 * 1024 * 1024,
 max_urls: 5_000,
 max_children: 5,
 }
 }
}

impl SitemapConfig {
 pub fn limits(&self) -> crate::sitemap::SitemapLimits {
 crate::sitemap::SitemapLimits {
 max_bytes: self.max_bytes,
 max_urls: self.max_urls,
 max_children: self.max_children,
 }
 }
}

/// `[pdf]` — `pdf_extract` ceilings and the PDFium engine ().
/// `pdfium_path`/`pdfium_discovery` are config-file only; no tool argument can
/// point the engine at another library.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PdfConfig {
 /// Strict path to `libpdfium.so`. When set, exactly this file is loaded and
 /// a failure is `render_unavailable` naming the key; no fallback.
 pub pdfium_path: Option<PathBuf>,
 /// When `pdfium_path` is unset, search env → exe dir → data dir → system.
 pub pdfium_discovery: bool,
 /// Fetched PDF body ceiling in bytes (1..=64 MiB).
 pub max_bytes: u64,
 /// Pages extracted per call (1..=2 000).
 pub max_pages: usize,
 /// Delivered markdown ceiling in characters (1..=5 000 000).
 pub max_chars: usize,
}

impl Default for PdfConfig {
 fn default() -> Self {
 Self {
 pdfium_path: None,
 pdfium_discovery: true,
 max_bytes: 32 * 1024 * 1024,
 max_pages: 200,
 max_chars: 1_000_000,
 }
 }
}

impl PdfConfig {
 pub fn limits(&self) -> crate::pdf::PdfLimits {
 crate::pdf::PdfLimits {
 max_bytes: self.max_bytes,
 max_pages: self.max_pages,
 max_chars: self.max_chars,
 }
 }
}

/// `[events]` — the change feed (`events` tool + the loopback GET route).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
 /// Serve `GET /feeds/changes.xml` on the loopback listener. `false` → 404.
 pub feed_enabled: bool,
 /// Feed item cap (1..=200); the RSS document itself never exceeds 100.
 pub feed_items: usize,
}

impl Default for EventsConfig {
 fn default() -> Self {
 Self {
 feed_enabled: true,
 feed_items: 100,
 }
 }
}

/// `[fts]` — the local full-text index and `search_content` (029).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FtsConfig {
 /// Indexed body cap per page (chars, char-boundary safe). The title is always
 /// indexed. Lowering it shrinks the database at the cost of recall.
 pub max_index_chars: usize,
 /// Query cap; longer input is `schema_invalid` at `/query`.
 pub max_query_chars: usize,
 /// Results returned when the caller omits `limit`.
 pub default_limit: usize,
 /// Hard per-call ceiling (1..=100).
 pub max_limit: usize,
 /// Snippet cap (chars) applied to the plain-text context.
 pub max_snippet_chars: usize,
}

impl Default for FtsConfig {
 fn default() -> Self {
 Self {
 max_index_chars: 50_000,
 max_query_chars: 512,
 default_limit: 20,
 max_limit: 100,
 max_snippet_chars: 320,
 }
 }
}

impl Default for SearchConfig {
 fn default() -> Self {
 Self {
 html_url: "https://html.duckduckgo.com/html/".to_string(),
 lite_url: "https://lite.duckduckgo.com/lite/".to_string(),
 default_limit: 10,
 max_query_chars: 512,
 }
 }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
 /// Overrides the XDG default ($XDG_DATA_HOME/web-widow).
 pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
 pub level: String,
 pub json: bool,
 /// Redact query strings from log lines (default on).
 pub redact_query: bool,
}

impl Default for LoggingConfig {
 fn default() -> Self {
 Self {
 level: "info".to_string(),
 json: false,
 redact_query: true,
 }
 }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
 pub fetch_events_days: u32,
 pub extractions_days: u32,
 /// Screenshots are disposable debugging evidence; rows older than this are
 /// pruned before the blob pass (default 30, 1..=3650).
 pub screenshots_days: u32,
 /// Delivered webhook rows are pruned after this many days (default 30,
 /// 1..=3650). Dead letters use `webhook_dead_days` instead.
 pub webhook_deliveries_days: u32,
 /// Dead-letter rows are kept longer than delivered history (default 90,
 /// 1..=3650); pending rows are never pruned.
 pub webhook_dead_days: u32,
 /// Non-change-point snapshots kept per monitor; change points always stay.
 pub snapshots_per_monitor: usize,
 pub max_blob_bytes: u64,
}

impl Default for RetentionConfig {
 fn default() -> Self {
 Self {
 fetch_events_days: 30,
 extractions_days: 30,
 screenshots_days: 30,
 webhook_deliveries_days: 30,
 webhook_dead_days: 90,
 snapshots_per_monitor: 20,
 max_blob_bytes: 20 * 1024 * 1024 * 1024,
 }
 }
}

/// Browser interaction (`interact_*`,/040). One held page per
/// process; the idle TTL is configurable, the hard lifetime and the action
/// ceiling are server constants.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InteractionConfig {
 /// Idle TTL in seconds, 30..=600.
 pub ttl_s: u64,
 /// Actions allowed per session, 1..=60 (60 is the server ceiling).
 pub max_actions: u32,
}

impl Default for InteractionConfig {
 fn default() -> Self {
 Self {
 ttl_s: 120,
 max_actions: 60,
 }
 }
}

/// `[embeddings]` — the local semantic index contract.
/// P0 adds the section and its validation only; E1 reads it for behaviour and no
/// process in P0 loads a model or a native library.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
 /// Off by default: the deployment provisions the library + model, then enables.
 pub enabled: bool,
 /// Built-in model id; the only id in v1.
 pub model: String,
 /// Optional override; defaults to `<data_dir>/models/<model>`.
 pub model_dir: Option<PathBuf>,
 /// Optional strict path to `libonnxruntime.so`; unset means discovery (E1).
 pub ort_dylib_path: Option<PathBuf>,
 /// Target chunk size in characters (200..=2000).
 pub chunk_chars: usize,
 /// Chunk overlap in characters (0..=chunk_chars / 2).
 pub chunk_overlap: usize,
 /// Chunks kept per page (1..=200).
 pub max_chunks_per_page: usize,
 /// Source text cap per page in characters (1000..=200000).
 pub max_source_chars: usize,
 /// Pages embedded per backfill pass (1..=200).
 pub backfill_pages_per_pass: usize,
 /// Delay between backfill passes in milliseconds (1000..=600000).
 pub backfill_interval_ms: u64,
 /// Query length ceiling in characters (1..=4096).
 pub query_max_chars: usize,
}

impl Default for EmbeddingsConfig {
 fn default() -> Self {
 Self {
 enabled: false,
 model: "minilm-l6-v2-q".to_string(),
 model_dir: None,
 ort_dylib_path: None,
 chunk_chars: 900,
 chunk_overlap: 120,
 max_chunks_per_page: 60,
 max_source_chars: 60_000,
 backfill_pages_per_pass: 20,
 backfill_interval_ms: 30_000,
 query_max_chars: 512,
 }
 }
}

/// `[webhooks]` — deliberate egress contract.
/// Targets exist only in the operator's config file; a tool or a monitor can
/// attach only a configured name. P0 validates the shape; W1 delivers.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhooksConfig {
 pub enabled: bool,
 /// Per-request timeout in milliseconds (500..=30000).
 pub timeout_ms: u64,
 /// Attempts before a delivery becomes `dead` (1..=10).
 pub max_attempts: u32,
 /// Permit `http://` targets; intended only for loopback/test receivers.
 pub allow_http: bool,
 /// Operator-named targets. Empty is valid: nothing fires until one is added.
 pub targets: BTreeMap<String, WebhookTargetConfig>,
}

impl Default for WebhooksConfig {
 fn default() -> Self {
 Self {
 enabled: false,
 timeout_ms: 5_000,
 max_attempts: 5,
 allow_http: false,
 targets: BTreeMap::new(),
 }
 }
}

/// Payload shape for one named target. The URL is treated as a secret (Slack and
/// Discord embed their token in it) and is never logged or returned by a tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookTargetConfig {
 pub url: String,
 pub kind: WebhookKind,
 /// Exactly one of `secret_env`/`secret_file` may be set; absent is unsigned.
 /// The reference is not resolved at config load.
 pub secret_env: Option<String>,
 pub secret_file: Option<PathBuf>,
 /// Exactly one of `auth_env`/`auth_file` may be set; sent as `Authorization`.
 pub auth_env: Option<String>,
 pub auth_file: Option<PathBuf>,
 /// Operator opt-in; the diff is omitted by default.
 pub include_diff: bool,
}

/// Closed payload shape set (W1). Unknown values are refused at config parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
 #[default]
 Generic,
 Slack,
 Discord,
}

/// `[scheduler]` — the opt-in in-process timer.
/// Default off; S1 reads it. `run-due` semantics are unchanged.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
 pub enabled: bool,
 /// Tick interval in milliseconds (1000..=600000).
 pub tick_ms: u64,
 /// Checks per tick (1..=500), also clamped by `[monitor].max_checks_per_call`.
 pub max_checks_per_tick: usize,
}

impl Default for SchedulerConfig {
 fn default() -> Self {
 Self {
 enabled: false,
 tick_ms: 30_000,
 max_checks_per_tick: 25,
 }
 }
}

/// `[http]` — the loopback listener's transport policy ().
/// `tls = false` is the historical plain-HTTP behavior. An operator PEM pair
/// is the override; generated material is used only when both paths are absent.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
 pub tls: bool,
 pub tls_cert_file: Option<PathBuf>,
 pub tls_key_file: Option<PathBuf>,
}

/// `^[a-z0-9][a-z0-9_-]{0,31}$` without a regex compile per validation.
fn valid_target_name(name: &str) -> bool {
 let bytes = name.as_bytes();
 matches!(bytes.first(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
 && bytes.len() <= 32
 && bytes[1..].iter().all(|byte| {
 byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
 })
}

/// JavaScript rendering. The browser is optional: with
/// `render.chrome_path` unset and discovery enabled, the resolver checks
/// `$CHROME_PATH` and well-known Chrome/Chromium locations lazily at the first
/// render call; a missing browser degrades to `render_unavailable`, never a
/// startup failure.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderConfig {
 /// Explicit Chrome/Chromium binary. Config file only: an agent cannot point
 /// the browser at an arbitrary executable through a tool argument.
 pub chrome_path: Option<PathBuf>,
 /// Also check `$CHROME_PATH` and well-known paths when `chrome_path` is unset.
 pub discovery: bool,
 /// Concurrent render pages (the shared browser gets one context per page).
 pub max_concurrent_pages: usize,
 /// Network-quiet window after the load event.
 pub settle_ms: u64,
 /// Hard ceiling for one navigation; the current DOM is returned as
 /// `rendered_partial` when it fires.
 pub ceiling_ms: u64,
 /// Transferred-byte cap per page (5 MiB).
 pub max_bytes: u64,
 /// Request cap per page (documents, scripts, XHR and all subresources).
 pub max_requests: u32,
 /// Browser restarts allowed inside one `restart_window_s` window. The first
 /// launch is not a restart: `0` means "never relaunch after a failure", not
 /// "rendering unavailable".
 pub restart_max: usize,
 pub restart_window_s: u64,
 /// Timeout for launching Chrome and connecting over CDP.
 pub launch_timeout_ms: u64,
 /// Transferred-byte cap for the visual capture profile (screenshots), 20 MiB.
 /// The text profile keeps `max_bytes`; this one only bounds a capture.
 pub visual_max_bytes: u64,
 /// Request cap for the visual capture profile, 500.
 pub visual_max_requests: u32,
 /// Largest PNG a screenshot may store (32 MiB). Checked **before** the CAS
 /// write: an over-size capture is a typed `too_large` and leaves no row.
 pub max_screenshot_bytes: u64,
}

impl Default for RenderConfig {
 fn default() -> Self {
 Self {
 chrome_path: None,
 discovery: true,
 max_concurrent_pages: 2,
 settle_ms: 500,
 ceiling_ms: 10_000,
 max_bytes: 5 * 1024 * 1024,
 max_requests: 200,
 restart_max: 3,
 restart_window_s: 10 * 60,
 launch_timeout_ms: 20_000,
 visual_max_bytes: 20 * 1024 * 1024,
 visual_max_requests: 500,
 max_screenshot_bytes: 32 * 1024 * 1024,
 }
 }
}

/// Where the effective configuration came from, for startup warnings.
#[derive(Debug, Clone)]
pub enum ConfigOrigin {
 File(PathBuf),
 Defaults,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
 pub config: Config,
 pub origin: ConfigOrigin,
}

impl Config {
 /// Load `/explicit` if given, else `$WEB_WIDOW_CONFIG`, else defaults.
 /// `data_dir_override` wins over env and file. Environment may set the data
 /// dir and the contact; nothing else.
 pub fn load(
 explicit: Option<&Path>,
 data_dir_override: Option<PathBuf>) -> Result<LoadedConfig> {
 let from_env = std::env::var_os(ENV_CONFIG).map(PathBuf::from);
 let path = explicit.map(Path::to_path_buf).or(from_env);

 let (mut config, origin) = match path {
 Some(path) => {
 let text = std::fs::read_to_string(&path).map_err(|error| {
 WidowError::InvalidUrl(format!(
 "cannot read config {}: {error}",
 path.display()
))
 })?;
 let config: Config = toml::from_str(&text).map_err(|error| {
 WidowError::InvalidUrl(format!("invalid config {}: {error}", path.display()))
 })?;
 (config, ConfigOrigin::File(path))
 }
 None => (Config::default(), ConfigOrigin::Defaults),
 };

 if let Some(dir) = data_dir_override {
 config.storage.data_dir = Some(dir);
 } else if let Ok(dir) = std::env::var(ENV_DATA) {
 config.storage.data_dir = Some(PathBuf::from(dir));
 }
 if let Ok(contact) = std::env::var(ENV_CONTACT) {
 config.fetch.contact = Some(contact);
 }

 config.validate()?;
 Ok(LoadedConfig { config, origin })
 }

 pub fn validate(&self) -> Result<()> {
 let fetch = &self.fetch;
 for (name, value) in [
 ("fetch.connect_timeout_ms", fetch.connect_timeout_ms),
 ("fetch.read_timeout_ms", fetch.read_timeout_ms),
 ("fetch.total_timeout_ms", fetch.total_timeout_ms),
 ("fetch.max_body_bytes", fetch.max_body_bytes),
 ("fetch.max_header_bytes", fetch.max_header_bytes),
 ("fetch.min_interval_ms", fetch.min_interval_ms),
 ("extract.max_chars", self.extract.max_chars as u64),
 ] {
 if value == 0 {
 return Err(WidowError::InvalidUrl(format!("{name} must be > 0")));
 }
 }
 if fetch.max_redirects > 10 {
 return Err(WidowError::InvalidUrl(
 "fetch.max_redirects must be <= 10".to_string()));
 }
 if fetch.max_concurrent_per_host == 0 {
 return Err(WidowError::InvalidUrl(
 "fetch.max_concurrent_per_host must be >= 1".to_string()));
 }
 if self.extract.max_links == 0 {
 return Err(WidowError::InvalidUrl(
 "extract.max_links must be >= 1".to_string()));
 }
 if self.extract.max_records == 0 {
 return Err(WidowError::InvalidUrl(
 "extract.max_records must be >= 1".to_string()));
 }
 if self.extract.max_field_chars == 0 {
 return Err(WidowError::InvalidUrl(
 "extract.max_field_chars must be >= 1".to_string()));
 }
 if self.feed.max_limit == 0 || self.feed.max_limit > 100 {
 return Err(WidowError::InvalidUrl(
 "feed.max_limit must be 1..=100".to_string()));
 }
 if self.feed.default_limit == 0 || self.feed.default_limit > self.feed.max_limit {
 return Err(WidowError::InvalidUrl(
 "feed.default_limit must be >= 1 and <= feed.max_limit".to_string()));
 }
 if self.feed.max_summary_chars == 0 {
 return Err(WidowError::InvalidUrl(
 "feed.max_summary_chars must be >= 1".to_string()));
 }
 let sitemap = &self.sitemap;
 if sitemap.max_bytes == 0 || sitemap.max_bytes > 64 * 1024 * 1024 {
 return Err(WidowError::InvalidUrl(
 "sitemap.max_bytes must be 1..=67108864 (64 MiB)".to_string()));
 }
 if sitemap.max_urls == 0 || sitemap.max_urls > 50_000 {
 return Err(WidowError::InvalidUrl(
 "sitemap.max_urls must be 1..=50000".to_string()));
 }
 if sitemap.max_children == 0 || sitemap.max_children > 10 {
 return Err(WidowError::InvalidUrl(
 "sitemap.max_children must be 1..=10".to_string()));
 }
 if self.pdf.max_bytes == 0 || self.pdf.max_bytes > 64 * 1024 * 1024 {
 return Err(WidowError::InvalidUrl(
 "pdf.max_bytes must be 1..=67108864 (64 MiB)".to_string()));
 }
 if self.pdf.max_pages == 0 || self.pdf.max_pages > 2_000 {
 return Err(WidowError::InvalidUrl(
 "pdf.max_pages must be 1..=2000".to_string()));
 }
 if self.pdf.max_chars == 0 || self.pdf.max_chars > 5_000_000 {
 return Err(WidowError::InvalidUrl(
 "pdf.max_chars must be 1..=5000000".to_string()));
 }
 if self.events.feed_items == 0 || self.events.feed_items > 200 {
 return Err(WidowError::InvalidUrl(
 "events.feed_items must be 1..=200 (the RSS document caps at 100)".to_string()));
 }
 if self
.pdf
.pdfium_path
.as_ref()
.is_some_and(|path| path.as_os_str().is_empty())
 {
 return Err(WidowError::InvalidUrl(
 "pdf.pdfium_path must not be empty (unset it to use discovery)".to_string()));
 }
 if self.fts.max_limit == 0 || self.fts.max_limit > 100 {
 return Err(WidowError::InvalidUrl(
 "fts.max_limit must be 1..=100".to_string()));
 }
 if self.fts.default_limit == 0 || self.fts.default_limit > self.fts.max_limit {
 return Err(WidowError::InvalidUrl(
 "fts.default_limit must be >= 1 and <= fts.max_limit".to_string()));
 }
 for (name, value) in [
 ("fts.max_index_chars", self.fts.max_index_chars),
 ("fts.max_query_chars", self.fts.max_query_chars),
 ("fts.max_snippet_chars", self.fts.max_snippet_chars),
 ] {
 if value == 0 {
 return Err(WidowError::InvalidUrl(format!("{name} must be > 0")));
 }
 }
 let crawl = &self.crawl;
 if crawl.max_pages == 0 || crawl.max_duration_s == 0 || crawl.lease_s == 0 {
 return Err(WidowError::InvalidUrl(
 "crawl.max_pages, crawl.max_duration_s and crawl.lease_s must be > 0".to_string()));
 }
 if crawl.max_attempts == 0 {
 return Err(WidowError::InvalidUrl(
 "crawl.max_attempts must be >= 1".to_string()));
 }
 if crawl.max_error_samples == 0 || crawl.max_error_samples > 5 {
 return Err(WidowError::InvalidUrl(
 "crawl.max_error_samples must be 1..=5 (the contract caps samples at 5)"
.to_string()));
 }
 if !crawl.default_per_host_rate.is_finite() || crawl.default_per_host_rate <= 0.0 {
 return Err(WidowError::InvalidUrl(
 "crawl.default_per_host_rate must be a positive number".to_string()));
 }
 if crawl.result_limit_default == 0
 || crawl.result_limit_max == 0
 || crawl.result_limit_default > crawl.result_limit_max
 || crawl.result_limit_max > 500
 {
 return Err(WidowError::InvalidUrl(
 "crawl.result_limit_default must be 1..=crawl.result_limit_max, and the max <= 500"
.to_string()));
 }
 let monitor = &self.monitor;
 if monitor.min_interval_s == 0
 || monitor.default_interval_s < monitor.min_interval_s
 || monitor.default_interval_s > monitor.max_interval_s
 {
 return Err(WidowError::InvalidUrl(
 "monitor.min_interval_s must be > 0 and monitor.default_interval_s must be within min..=max"
.to_string()));
 }
 if monitor.max_diff_bytes < 512 {
 return Err(WidowError::InvalidUrl(
 "monitor.max_diff_bytes must be >= 512 (a smaller cap is not a usable diff)"
.to_string()));
 }
 if monitor.cosmetic_max_changed_lines == 0 {
 return Err(WidowError::InvalidUrl(
 "monitor.cosmetic_max_changed_lines must be >= 1".to_string()));
 }
 if monitor.max_ignore_selectors == 0 || monitor.max_checks_per_call == 0 {
 return Err(WidowError::InvalidUrl(
 "monitor.max_ignore_selectors and monitor.max_checks_per_call must be > 0"
.to_string()));
 }
 if monitor.changes_limit_default == 0
 || monitor.changes_limit_max == 0
 || monitor.changes_limit_default > monitor.changes_limit_max
 || monitor.changes_limit_max > 500
 {
 return Err(WidowError::InvalidUrl(
 "monitor.changes_limit_default must be 1..=monitor.changes_limit_max, and the max <= 500"
.to_string()));
 }
 if monitor.max_wait_ms == 0 {
 return Err(WidowError::InvalidUrl(
 "monitor.max_wait_ms must be > 0".to_string()));
 }
 if self.retention.screenshots_days == 0 || self.retention.screenshots_days > 3650 {
 return Err(WidowError::InvalidUrl(
 "retention.screenshots_days must be 1..=3650".to_string()));
 }
 for (name, value) in [
 (
 "retention.webhook_deliveries_days",
 self.retention.webhook_deliveries_days),
 (
 "retention.webhook_dead_days",
 self.retention.webhook_dead_days),
 ] {
 if !(1..=3650).contains(&value) {
 return Err(WidowError::InvalidUrl(format!("{name} must be 1..=3650")));
 }
 }
 if self.retention.snapshots_per_monitor == 0 {
 return Err(WidowError::InvalidUrl(
 "retention.snapshots_per_monitor must be >= 1".to_string()));
 }
 if !(30..=600).contains(&self.interaction.ttl_s) {
 return Err(WidowError::InvalidUrl(
 "interaction.ttl_s must be 30..=600".to_string()));
 }
 if !(1..=60).contains(&self.interaction.max_actions) {
 return Err(WidowError::InvalidUrl(
 "interaction.max_actions must be 1..=60".to_string()));
 }
 if crawl.idle_poll_ms == 0 {
 return Err(WidowError::InvalidUrl(
 "crawl.idle_poll_ms must be > 0".to_string()));
 }
 if !matches!(
 self.extract.default_mode.as_str(),
 "main" | "full" | "text" | "raw"
) {
 return Err(WidowError::InvalidUrl(format!(
 "extract.default_mode `{}` is not one of main|full|text|raw",
 self.extract.default_mode
)));
 }
 let render = &self.render;
 for (name, value) in [
 (
 "render.max_concurrent_pages",
 render.max_concurrent_pages as u64),
 ("render.ceiling_ms", render.ceiling_ms),
 ("render.max_bytes", render.max_bytes),
 ("render.max_requests", render.max_requests as u64),
 ("render.launch_timeout_ms", render.launch_timeout_ms),
 ] {
 if value == 0 {
 return Err(WidowError::InvalidUrl(format!("{name} must be > 0")));
 }
 }
 if render.settle_ms > render.ceiling_ms {
 return Err(WidowError::InvalidUrl(
 "render.settle_ms must be <= render.ceiling_ms".to_string()));
 }
 if render.visual_max_bytes == 0 || render.visual_max_bytes > 64 * 1024 * 1024 {
 return Err(WidowError::InvalidUrl(
 "render.visual_max_bytes must be 1..=64 MiB".to_string()));
 }
 if !(1..=2_000).contains(&render.visual_max_requests) {
 return Err(WidowError::InvalidUrl(
 "render.visual_max_requests must be 1..=2000".to_string()));
 }
 if render.max_screenshot_bytes == 0 || render.max_screenshot_bytes > 256 * 1024 * 1024 {
 return Err(WidowError::InvalidUrl(
 "render.max_screenshot_bytes must be 1..=256 MiB".to_string()));
 }
 if render.restart_max > 0 && render.restart_window_s == 0 {
 return Err(WidowError::InvalidUrl(
 "render.restart_window_s must be > 0 when render.restart_max > 0".to_string()));
 }
 for (name, endpoint) in [
 ("search.html_url", self.search.html_url.as_str()),
 ("search.lite_url", self.search.lite_url.as_str()),
 ] {
 let url = Url::parse(endpoint)
.map_err(|error| WidowError::InvalidUrl(format!("{name} is not a URL: {error}")))?;
 if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
 return Err(WidowError::InvalidUrl(format!(
 "{name} must be an absolute http(s) URL"
)));
 }
 }
 if !(1..=10).contains(&self.search.default_limit) {
 return Err(WidowError::InvalidUrl(
 "search.default_limit must be 1..=10".to_string()));
 }
 if self.search.max_query_chars == 0 {
 return Err(WidowError::InvalidUrl(
 "search.max_query_chars must be > 0".to_string()));
 }
 let embeddings = &self.embeddings;
 if !matches!(embeddings.model.as_str(), "minilm-l6-v2-q") {
 return Err(WidowError::InvalidUrl(format!(
 "embeddings.model `{}` is not a known model id (v1: minilm-l6-v2-q)",
 embeddings.model
)));
 }
 if !(200..=2_000).contains(&embeddings.chunk_chars) {
 return Err(WidowError::InvalidUrl(
 "embeddings.chunk_chars must be 200..=2000".to_string()));
 }
 if embeddings.chunk_overlap > embeddings.chunk_chars / 2 {
 return Err(WidowError::InvalidUrl(
 "embeddings.chunk_overlap must be <= embeddings.chunk_chars / 2".to_string()));
 }
 if !(1..=200).contains(&embeddings.max_chunks_per_page) {
 return Err(WidowError::InvalidUrl(
 "embeddings.max_chunks_per_page must be 1..=200".to_string()));
 }
 if !(1_000..=200_000).contains(&embeddings.max_source_chars) {
 return Err(WidowError::InvalidUrl(
 "embeddings.max_source_chars must be 1000..=200000".to_string()));
 }
 if !(1..=200).contains(&embeddings.backfill_pages_per_pass) {
 return Err(WidowError::InvalidUrl(
 "embeddings.backfill_pages_per_pass must be 1..=200".to_string()));
 }
 if !(1_000..=600_000).contains(&embeddings.backfill_interval_ms) {
 return Err(WidowError::InvalidUrl(
 "embeddings.backfill_interval_ms must be 1000..=600000".to_string()));
 }
 if !(1..=4_096).contains(&embeddings.query_max_chars) {
 return Err(WidowError::InvalidUrl(
 "embeddings.query_max_chars must be 1..=4096".to_string()));
 }
 if embeddings
.model_dir
.as_ref()
.is_some_and(|path| path.as_os_str().is_empty())
 {
 return Err(WidowError::InvalidUrl(
 "embeddings.model_dir must not be empty (unset it for the default)".to_string()));
 }
 if embeddings
.ort_dylib_path
.as_ref()
.is_some_and(|path| path.as_os_str().is_empty())
 {
 return Err(WidowError::InvalidUrl(
 "embeddings.ort_dylib_path must not be empty (unset it for discovery)".to_string()));
 }
 let webhooks = &self.webhooks;
 if !(500..=30_000).contains(&webhooks.timeout_ms) {
 return Err(WidowError::InvalidUrl(
 "webhooks.timeout_ms must be 500..=30000".to_string()));
 }
 if !(1..=10).contains(&webhooks.max_attempts) {
 return Err(WidowError::InvalidUrl(
 "webhooks.max_attempts must be 1..=10".to_string()));
 }
 for (name, target) in &webhooks.targets {
 if !valid_target_name(name) {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}: name must match ^[a-z0-9][a-z0-9_-]{{0,31}}$"
)));
 }
 let url = Url::parse(&target.url).map_err(|error| {
 WidowError::InvalidUrl(format!("webhooks.targets.{name}.url is not a URL: {error}"))
 })?;
 if url.host_str().is_none() {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}.url must have a host"
)));
 }
 if !url.username().is_empty() || url.password().is_some() {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}.url must not carry userinfo"
)));
 }
 let scheme_allowed =
 url.scheme() == "https" || (webhooks.allow_http && url.scheme() == "http");
 if !scheme_allowed {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}.url must be https (http only with webhooks.allow_http = true)"
)));
 }
 if target.secret_env.is_some() && target.secret_file.is_some() {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}: set at most one of secret_env/secret_file"
)));
 }
 if target.auth_env.is_some() && target.auth_file.is_some() {
 return Err(WidowError::InvalidUrl(format!(
 "webhooks.targets.{name}: set at most one of auth_env/auth_file"
)));
 }
 }
 if !(1_000..=600_000).contains(&self.scheduler.tick_ms) {
 return Err(WidowError::InvalidUrl(
 "scheduler.tick_ms must be 1000..=600000".to_string()));
 }
 if !(1..=500).contains(&self.scheduler.max_checks_per_tick) {
 return Err(WidowError::InvalidUrl(
 "scheduler.max_checks_per_tick must be 1..=500".to_string()));
 }
 match (&self.http.tls_cert_file, &self.http.tls_key_file) {
 (None, None) => {}
 (Some(_), Some(_)) if self.http.tls => {}
 (Some(_), Some(_)) => {
 return Err(WidowError::SchemaInvalid {
 pointer: "/http/tls".to_string(),
 message: "tls_cert_file/tls_key_file require [http].tls = true".to_string(),
 });
 }
 _ => {
 return Err(WidowError::SchemaInvalid {
 pointer: "/http/tls_cert_file".to_string(),
 message: "tls_cert_file and tls_key_file must be set together".to_string(),
 });
 }
 }
 self.validate_auth()?;
 Ok(())
 }

 /// Validate the `[auth]` contract ( A0). Inert at runtime: A1 consumes
 /// these values, A0 only refuses a config that could not be honoured.
 fn validate_auth(&self) -> Result<()> {
 validate_auth(&self.auth)
 }

 /// `web-widow/<version> (+<contact>)`, or without the comment when absent.
 pub fn user_agent(&self) -> String {
 let base = format!("{}/{}", self.robots.user_agent, env!("CARGO_PKG_VERSION"));
 match self.fetch.contact.as_deref().map(str::trim) {
 Some(contact) if !contact.is_empty() => format!("{base} (+{contact})"),
 _ => base,
 }
 }

 pub fn data_dir(&self) -> Result<PathBuf> {
 if let Some(dir) = &self.storage.data_dir {
 return Ok(dir.clone());
 }
 dirs::data_dir()
.map(|base| base.join("web-widow"))
.ok_or_else(|| {
 WidowError::Internal(
 "no data directory: set storage.data_dir or WEB_WIDOW_DATA".to_string())
 })
 }
}

// ---------------------------------------------------------------------------
// [auth] — config-named credentials ( A0, item 1)
// ---------------------------------------------------------------------------

/// Closed credential-shape set (). Unknown values are refused at parse.
/// `session` is the assisted-login kind: no reference or
/// header field, a login boundary and a sealed cookie jar instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
 #[default]
 Bearer,
 Basic,
 Header,
 Session,
}

/// `[auth]` — the config-only credential contract. References live in this
/// 0600 file; the agent names a configured profile and never a reference or a
/// value. A0 validates the contract and is otherwise **inert**: no manager is
/// spawned, no header is attached, and no tool accepts a `profile` argument
/// until A1.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
 /// `auth_audit` pruning window in days (1..=3650, default 90).
 pub audit_days: u32,
 /// Hard per-resolution manager timeout in milliseconds (100..=60000).
 pub manager_timeout_ms: u64,
 /// Permit plain HTTP only for the literal loopback addresses `127.0.0.1`
 /// and `::1`; never widens a non-loopback binding, and never a host name
 /// that merely resolves to loopback ().
 pub allow_loopback_http: bool,
 /// Provider binaries keyed by reference scheme. Each entry must be an
 /// absolute, existing, executable path (no `PATH` lookup, D4).
 pub managers: BTreeMap<String, AuthManagerConfig>,
 /// Operator-named credentials. Empty is valid: nothing is authenticated.
 pub profiles: BTreeMap<String, AuthProfileConfig>,
 /// B5b: store an authenticated body ( stays return-only by default).
 pub persist: bool,
 /// Include authenticated pages in the local full-text index and the
 /// embedding candidates (default: excluded).
 pub index_authenticated: bool,
 /// Include events derived from authenticated pages in the feed (default:
 /// excluded).
 pub events_authenticated: bool,
 /// Retention for stored authenticated pages, 1..=3650 days (default 7).
 pub page_retention_days: u32,
}

impl Default for AuthConfig {
 fn default() -> Self {
 Self {
 audit_days: 90,
 manager_timeout_ms: 5_000,
 allow_loopback_http: false,
 managers: BTreeMap::new(),
 profiles: BTreeMap::new(),
 persist: false,
 index_authenticated: false,
 events_authenticated: false,
 page_retention_days: 7,
 }
 }
}

/// One provider CLI (`op`, `bw`, `pass`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthManagerConfig {
 /// Absolute path to the provider binary; the server never searches `PATH`.
 pub path: String,
 /// The only variable names passed to the child, in addition to `HOME`.
 /// Values are read from the service environment at invocation and are
 /// never logged. `PATH` and the server's own `WEB_WIDOW_*` names are
 /// refused: the child must not inherit either.
 pub env_allow: Vec<String>,
}

/// One operator-named binding. The kind decides which reference fields exist.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthProfileConfig {
 pub kind: AuthKind,
 /// Bound host: lowercase, no scheme, port, path or userinfo. Trailing dot
 /// allowed; compared exact. `allow_subdomains` widens **reachability** only
 /// (): credential delivery stays exact-origin.
 pub host: String,
 /// `op://`/`bw://`/`pass://` secret reference (bearer and header kinds).
 pub reference: Option<String>,
 /// `basic` username reference; also a reference, never a literal.
 pub username_reference: Option<String>,
 /// `basic` password reference; also a reference, never a literal.
 pub password_reference: Option<String>,
 /// Exact header name for `kind = "header"` ( allowlist).
 pub header: Option<String>,
 pub allow_subdomains: bool,
 /// Exact origin port for credential delivery (default 443). The credential
 /// origin is `host:port`; a redirect to another port is `egress_blocked`.
 pub port: u16,
 // ---- session profiles (). Refused on every other kind. ----
 /// SSO entry point; `https` only. The login window opens here.
 pub login_url: Option<String>,
 /// Exact hosts the login window's **documents** may navigate to. Must
 /// contain the binding host; SSO hops must be declared.
 pub login_origins: Vec<String>,
 /// `https` URL on the binding host. A 2xx answer through the captured
 /// cookies verifies the session before it is sealed.
 pub verify_url: Option<String>,
 /// Exact egress hosts for the authenticated browser (never the login
 /// window's boundary).
 pub assets: Vec<String>,
 /// Sealed-jar lifetime bound, 1..=365 days (default 30).
 pub session_max_age_days: Option<u32>,
}

impl Default for AuthProfileConfig {
 fn default() -> Self {
 Self {
 kind: AuthKind::default(),
 host: String::new(),
 reference: None,
 username_reference: None,
 password_reference: None,
 header: None,
 allow_subdomains: false,
 port: 443,
 login_url: None,
 login_origins: Vec::new(),
 verify_url: None,
 assets: Vec::new(),
 session_max_age_days: None,
 }
 }
}

const AUTH_PROVIDERS: [&str; 3] = ["op", "bw", "pass"];
const AUTH_REFERENCE_MAX: usize = 256;
const AUTH_SEGMENT_MAX: usize = 64;
/// Transport-managed request headers a profile may never set.
const AUTH_FORBIDDEN_HEADERS: [&str; 5] = [
 "host",
 "cookie",
 "content-length",
 "transfer-encoding",
 "connection",
];

fn validate_auth(auth: &AuthConfig) -> Result<()> {
 use std::os::unix::fs::PermissionsExt;

 if !(1..=3650).contains(&auth.audit_days) {
 return Err(WidowError::InvalidUrl(
 "auth.audit_days must be 1..=3650".to_string()));
 }
 if !(1..=3650).contains(&auth.page_retention_days) {
 return Err(WidowError::InvalidUrl(
 "auth.page_retention_days must be 1..=3650".to_string()));
 }
 if !(100..=60_000).contains(&auth.manager_timeout_ms) {
 return Err(WidowError::InvalidUrl(
 "auth.manager_timeout_ms must be 100..=60000".to_string()));
 }

 for (name, manager) in &auth.managers {
 let key = format!("auth.managers.{name}");
 if !AUTH_PROVIDERS.contains(&name.as_str()) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}: unknown provider (expected one of op, bw, pass)"
)));
 }
 let path = Path::new(&manager.path);
 if manager.path.is_empty() || !path.is_absolute() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.path must be an absolute path to the provider binary (PATH is not searched)"
)));
 }
 let metadata = std::fs::metadata(path).map_err(|error| {
 WidowError::InvalidUrl(format!(
 "{key}.path {} is not usable: {error}",
 path.display()
))
 })?;
 if !metadata.is_file() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.path {} is not a regular file",
 path.display()
)));
 }
 if metadata.permissions().mode() & 0o111 == 0 {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.path {} is not executable",
 path.display()
)));
 }
 let mut seen: Vec<&str> = Vec::new();
 for variable in &manager.env_allow {
 if !valid_env_name(variable) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.env_allow: `{variable}` is not an environment variable name"
)));
 }
 if variable == "PATH" {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.env_allow must not name PATH: the child runs with a cleared environment"
)));
 }
 if variable.starts_with("WEB_WIDOW_") {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.env_allow must not name the server's own variables (`{variable}`)"
)));
 }
 if seen.contains(&variable.as_str()) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.env_allow names `{variable}` twice"
)));
 }
 seen.push(variable);
 }
 }

 for (name, profile) in &auth.profiles {
 let key = format!("auth.profiles.{name}");
 if !valid_profile_name(name) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}: profile name must match ^[A-Za-z0-9._-]{{1,64}}$ and must not start with '-' or be '.'/'..'"
)));
 }
 if profile.port == 0 {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.port must be 1..=65535"
)));
 }
 let host = profile.host.strip_suffix('.').unwrap_or(&profile.host);
 canonical_auth_host(host)
.map_err(|detail| WidowError::InvalidUrl(format!("{key}.host: {detail}")))?;
 match profile.kind {
 AuthKind::Bearer => {
 require_reference(profile.reference.as_deref(), &key, "reference")?;
 forbid_field(
 profile.username_reference.as_deref(),
 &key,
 "username_reference")?;
 forbid_field(
 profile.password_reference.as_deref(),
 &key,
 "password_reference")?;
 forbid_field(profile.header.as_deref(), &key, "header")?;
 }
 AuthKind::Basic => {
 forbid_field(profile.reference.as_deref(), &key, "reference")?;
 require_reference(
 profile.username_reference.as_deref(),
 &key,
 "username_reference")?;
 require_reference(
 profile.password_reference.as_deref(),
 &key,
 "password_reference")?;
 forbid_field(profile.header.as_deref(), &key, "header")?;
 }
 AuthKind::Header => {
 require_reference(profile.reference.as_deref(), &key, "reference")?;
 let header = match profile.header.as_deref() {
 Some(header) if !header.is_empty() => header,
 _ => {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.header is required for kind = \"header\""
)));
 }
 };
 valid_auth_header(header)
.map_err(|detail| WidowError::InvalidUrl(format!("{key}.header: {detail}")))?;
 forbid_field(
 profile.username_reference.as_deref(),
 &key,
 "username_reference")?;
 forbid_field(
 profile.password_reference.as_deref(),
 &key,
 "password_reference")?;
 }
 AuthKind::Session => {
 // The credential is a captured jar, never a reference or header.
 forbid_field(profile.reference.as_deref(), &key, "reference")?;
 forbid_field(
 profile.username_reference.as_deref(),
 &key,
 "username_reference")?;
 forbid_field(
 profile.password_reference.as_deref(),
 &key,
 "password_reference")?;
 forbid_field(profile.header.as_deref(), &key, "header")?;
 validate_session_profile(profile, &key, host)?;
 }
 }
 for reference in [
 profile.reference.as_deref(),
 profile.username_reference.as_deref(),
 profile.password_reference.as_deref(),
 ]
.into_iter()
.flatten()
 {
 let provider = parse_auth_reference(reference)
.map_err(|detail| WidowError::InvalidUrl(format!("{key}: {detail}")))?;
 if !auth.managers.contains_key(provider) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}: reference provider `{provider}` has no auth.managers.{provider} entry"
)));
 }
 }
 if profile.kind != AuthKind::Session {
 for (field, present) in [
 ("login_url", profile.login_url.is_some()),
 ("login_origins", !profile.login_origins.is_empty()),
 ("verify_url", profile.verify_url.is_some()),
 ("assets", !profile.assets.is_empty()),
 (
 "session_max_age_days",
 profile.session_max_age_days.is_some()),
 ] {
 if present {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.{field} is only valid for kind = \"session\""
)));
 }
 }
 }
 }
 Ok(())
}

/// The session-only shape: an https login boundary, an https verify URL on the
/// binding host, exact asset hosts and a bounded jar lifetime.
fn validate_session_profile(profile: &AuthProfileConfig, key: &str, host: &str) -> Result<()> {
 let login_url = profile
.login_url
.as_deref()
.ok_or_else(|| WidowError::InvalidUrl(format!("{key}.login_url is required")))?;
 https_url(login_url, key, "login_url")?;

 if profile.login_origins.is_empty() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.login_origins must declare the hosts the login window may visit"
)));
 }
 for origin in &profile.login_origins {
 canonical_auth_host(origin)
.map_err(|detail| WidowError::InvalidUrl(format!("{key}.login_origins: {detail}")))?;
 }
 // The final origin must be the binding host, so the binding host must be a
 // declared login origin; otherwise the flow could never reach a capturable
 // state.
 if !profile.login_origins.iter().any(|origin| origin == host) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.login_origins must contain the binding host `{host}` (the final origin)"
)));
 }

 let verify_url = profile
.verify_url
.as_deref()
.ok_or_else(|| WidowError::InvalidUrl(format!("{key}.verify_url is required")))?;
 let verify = https_url(verify_url, key, "verify_url")?;
 let verify_host = verify
.host_str()
.map(|value| value.trim_end_matches('.').to_ascii_lowercase())
.unwrap_or_default();
 if verify_host != host {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.verify_url must be on the binding host `{host}` (found `{verify_host}`)"
)));
 }

 for asset in &profile.assets {
 canonical_auth_host(asset)
.map_err(|detail| WidowError::InvalidUrl(format!("{key}.assets: {detail}")))?;
 }

 let days = profile.session_max_age_days.unwrap_or(30);
 if !(1..=365).contains(&days) {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.session_max_age_days must be 1..=365"
)));
 }
 Ok(())
}

/// Parse a config URL that must be `https`, host-bearing, without userinfo, and
/// without a fragment. The URL is returned for the caller's host check.
fn https_url(raw: &str, key: &str, field: &str) -> Result<url::Url> {
 let parsed = url::Url::parse(raw)
.map_err(|error| WidowError::InvalidUrl(format!("{key}.{field}: {error}")))?;
 if parsed.scheme() != "https" {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.{field} must be https"
)));
 }
 if parsed.host_str().is_none() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.{field} must name a host"
)));
 }
 if !parsed.username().is_empty() || parsed.password().is_some() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.{field} must not carry userinfo"
)));
 }
 if parsed.fragment().is_some() {
 return Err(WidowError::InvalidUrl(format!(
 "{key}.{field} must not carry a fragment"
)));
 }
 Ok(parsed)
}

pub(crate) fn valid_profile_name(name: &str) -> bool {
 !name.is_empty()
 && name.len() <= 64
 && !name.starts_with('-')
 && name != "."
 && name != ".."
 && name
.bytes()
.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_env_name(name: &str) -> bool {
 let mut bytes = name.bytes();
 matches!(bytes.next(), Some(first) if first.is_ascii_alphabetic() || first == b'_')
 && name.len() <= 128
 && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Canonical form of a binding host, or the reason it is not one.
fn canonical_auth_host(host: &str) -> std::result::Result<(), String> {
 if host.is_empty() {
 return Err("must not be empty".to_string());
 }
 if host.contains('*') || host.contains('_') || host.starts_with('.') || host.contains("..") {
 return Err(format!(
 "`{host}` must be a bare exact host (no wildcard, leading dot, empty label or underscore)"
));
 }
 if host.contains('/') || host.contains('@') || host.contains('?') || host.contains('#') {
 return Err(format!(
 "`{host}` must be a bare host (no scheme, path or userinfo)"
));
 }
 if !host.is_ascii() {
 return Err("must be ASCII (use the punycode form)".to_string());
 }
 if host != host.to_ascii_lowercase() {
 return Err("must be lowercase (host comparison is case-insensitive)".to_string());
 }
 let candidate = if host.contains(':') {
 format!("[{host}]")
 } else {
 host.to_string()
 };
 let parsed =
 url::Host::parse(&candidate).map_err(|error| format!("`{host}` is not a host: {error}"))?;
 let rendered = match &parsed {
 url::Host::Domain(domain) => domain.clone(),
 url::Host::Ipv4(address) => address.to_string(),
 url::Host::Ipv6(address) => address.to_string(),
 };
 if rendered != host {
 return Err("must be a bare host (no scheme, port, path or userinfo)".to_string());
 }
 Ok(())
}

/// Validate the grammar and return the provider the scheme selects.
fn parse_auth_reference(reference: &str) -> std::result::Result<&'static str, String> {
 if reference.len() > AUTH_REFERENCE_MAX {
 return Err(format!(
 "reference is longer than {AUTH_REFERENCE_MAX} bytes"
));
 }
 let (provider, rest) = AUTH_PROVIDERS
.iter()
.find_map(|provider| {
 reference
.strip_prefix(&format!("{provider}://"))
.map(|rest| (*provider, rest))
 })
.ok_or_else(|| {
 "reference must start with op://, bw:// or pass:// (never an inline value)".to_string()
 })?;
 if rest.is_empty() {
 return Err(format!("{provider}:// reference is missing its path"));
 }
 if rest
.chars()
.any(|character| character.is_whitespace() || character.is_control())
 {
 return Err("reference must not contain whitespace or control characters".to_string());
 }
 let segments: Vec<&str> = rest.split('/').collect();
 for segment in &segments {
 if segment.is_empty() {
 return Err("reference must not carry an absolute or empty path segment".to_string());
 }
 if segment.len() > AUTH_SEGMENT_MAX {
 return Err(format!(
 "reference segment `{segment}` is longer than {AUTH_SEGMENT_MAX} bytes"
));
 }
 if segment.starts_with('-') {
 return Err(format!(
 "reference segment `{segment}` must not start with '-'"
));
 }
 if matches!(*segment, "." | "..") {
 return Err("reference must not contain `.` or `..` segments".to_string());
 }
 if !segment
.bytes()
.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
 {
 return Err(format!(
 "reference segment `{segment}` must match [A-Za-z0-9._-]"
));
 }
 }
 match provider {
 "op" if segments.len() != 3 => Err(format!(
 "op:// needs <vault>/<item>/<field> (got {} segments)",
 segments.len()
)),
 "bw" if !(1..=2).contains(&segments.len()) => Err(format!(
 "bw:// needs <item> or <item>/<field> (got {} segments)",
 segments.len()
)),
 "pass" if segments.len() < 2 => Err(format!(
 "pass:// needs <store>/<relative/path> (got {} segments)",
 segments.len()
)),
 _ => Ok(provider),
 }
}

fn valid_auth_header(name: &str) -> std::result::Result<(), String> {
 if name.is_empty() {
 return Err("must not be empty".to_string());
 }
 if name.len() > 64 {
 return Err("is longer than 64 bytes".to_string());
 }
 let token = name
.bytes()
.all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
 if !token {
 return Err("must be an HTTP token (no space, colon, CR, LF or NUL)".to_string());
 }
 let lower = name.to_ascii_lowercase();
 if AUTH_FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
 return Err(format!(
 "`{name}` is transport-managed and cannot be set by a profile"
));
 }
 Ok(())
}

fn require_reference(value: Option<&str>, key: &str, field: &str) -> Result<()> {
 if value.is_some_and(|value| !value.is_empty()) {
 Ok(())
 } else {
 Err(WidowError::InvalidUrl(format!(
 "{key}.{field} is required for this kind"
)))
 }
}

fn forbid_field(value: Option<&str>, key: &str, field: &str) -> Result<()> {
 if value.is_some() {
 Err(WidowError::InvalidUrl(format!(
 "{key}.{field} is not used by this kind"
)))
 } else {
 Ok(())
 }
}

/// Resolved paths inside the data directory ( layout).
#[derive(Debug, Clone)]
pub struct Paths {
 root: PathBuf,
}

impl Paths {
 pub fn new(root: PathBuf) -> Self {
 Self { root }
 }

 pub fn root(&self) -> &Path {
 &self.root
 }

 pub fn database(&self) -> PathBuf {
 self.root.join("web-widow.db")
 }

 pub fn blobs(&self) -> PathBuf {
 self.root.join("blobs")
 }

 /// Corpus exports (`corpus_export`): server-generated files only.
 pub fn exports(&self) -> PathBuf {
 self.root.join("exports")
 }

 pub fn lock(&self) -> PathBuf {
 self.root.join("server.lock")
 }

 /// Throwaway browser profile root. The renderer wipes it at start and exit.
 pub fn render(&self) -> PathBuf {
 self.root.join("render")
 }

 /// The feed capability token (0600; created on first start when absent).
 pub fn feed_token(&self) -> PathBuf {
 self.root.join("feed-token")
 }

 /// The sealed browser-session store (`<site>.sess`, `<site>.meta`): 0700,
 /// created on the first seal ().
 pub fn auth_dir(&self) -> PathBuf {
 self.root.join("auth")
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 /// `config.example.toml` claims every value it shows is the built-in default.
 /// Pin that: the example is what an operator copies, so a drifted example is
 /// a documented behaviour that does not exist.
 #[test]
 fn the_example_config_is_the_documented_default_set() {
 let example: Config = toml::from_str(include_str!("../config.example.toml"))
.expect("config.example.toml parses");
 example.validate().expect("config.example.toml validates");
 let mut expected = Config::default();
 // The one documented non-default: the example shows a placeholder contact.
 expected.fetch.contact = Some("crawler@example.com".to_string());
 assert_eq!(
 toml::to_string(&example).unwrap(),
 toml::to_string(&expected).unwrap(),
 "config.example.toml differs from the built-in defaults"
);
 }

 #[test]
 fn fts_defaults_and_bounds_are_validated() {
 let config = Config::default();
 assert_eq!(config.fts.max_index_chars, 50_000);
 assert_eq!(config.fts.max_query_chars, 512);
 assert_eq!(config.fts.default_limit, 20);
 assert_eq!(config.fts.max_limit, 100);
 assert_eq!(config.fts.max_snippet_chars, 320);
 config.validate().unwrap();

 for mutate in [
 (|config: &mut Config| config.fts.default_limit = 0) as fn(&mut Config),
 |config: &mut Config| config.fts.default_limit = 101,
 |config: &mut Config| config.fts.max_limit = 0,
 |config: &mut Config| config.fts.max_limit = 101,
 |config: &mut Config| config.fts.max_index_chars = 0,
 |config: &mut Config| config.fts.max_query_chars = 0,
 |config: &mut Config| config.fts.max_snippet_chars = 0,
 ] {
 let mut bad = Config::default();
 mutate(&mut bad);
 assert!(
 bad.validate().is_err(),
 "an invalid [fts] value must be refused"
);
 }
 }

 #[test]
 fn defaults_match_the_pinned_safety_limits() {
 let config = Config::default();
 assert_eq!(config.fetch.connect_timeout_ms, 10_000);
 assert_eq!(config.fetch.read_timeout_ms, 15_000);
 assert_eq!(config.fetch.total_timeout_ms, 30_000);
 assert_eq!(config.fetch.max_redirects, 5);
 assert_eq!(config.fetch.max_body_bytes, 10 * 1024 * 1024);
 assert_eq!(config.fetch.max_header_bytes, 64 * 1024);
 assert_eq!(config.fetch.min_interval_ms, 1_000);
 assert_eq!(config.fetch.max_concurrent_per_host, 1);
 assert_eq!(config.extract.max_chars, 200_000);
 assert_eq!(config.extract.max_records, 5_000);
 assert_eq!(config.extract.max_field_chars, 16_384);
 assert_eq!(config.extract.max_links, 2_000);
 assert_eq!(config.extract.cache_freshness_s, 300);
 assert_eq!(config.crawl.max_pages, 5_000);
 assert_eq!(config.crawl.max_depth, 5);
 assert_eq!(config.crawl.max_duration_s, 1_800);
 assert_eq!(config.crawl.lease_s, 30);
 assert_eq!(config.crawl.max_attempts, 3);
 assert_eq!(config.crawl.default_per_host_rate, 1.0);
 assert_eq!(config.crawl.result_limit_default, 50);
 assert_eq!(config.crawl.result_limit_max, 500);
 assert_eq!(config.monitor.default_interval_s, 86_400);
 assert_eq!(config.monitor.max_diff_bytes, 20 * 1024);
 assert_eq!(config.monitor.cosmetic_max_changed_lines, 2);
 assert_eq!(config.monitor.changes_limit_max, 500);
 assert_eq!(config.monitor.max_wait_ms, 30_000);
 assert!(config.robots.enabled);
 assert!(config.fetch.allow_hosts.is_empty());
 assert_eq!(config.retention.fetch_events_days, 30);
 assert_eq!(config.retention.screenshots_days, 30);
 assert_eq!(config.retention.snapshots_per_monitor, 20);
 assert_eq!(config.retention.max_blob_bytes, 20 * 1024 * 1024 * 1024);
 assert_eq!(config.render.max_concurrent_pages, 2);
 assert_eq!(config.render.settle_ms, 500);
 assert_eq!(config.render.ceiling_ms, 10_000);
 assert_eq!(config.render.max_bytes, 5 * 1024 * 1024);
 assert_eq!(config.render.max_requests, 200);
 assert_eq!(config.render.restart_max, 3);
 assert_eq!(config.render.restart_window_s, 600);
 assert_eq!(config.render.visual_max_bytes, 20 * 1024 * 1024);
 assert_eq!(config.render.visual_max_requests, 500);
 assert_eq!(config.render.max_screenshot_bytes, 32 * 1024 * 1024);
 assert!(config.render.chrome_path.is_none());
 assert!(config.render.discovery);
 assert_eq!(config.search.html_url, "https://html.duckduckgo.com/html/");
 assert_eq!(config.search.lite_url, "https://lite.duckduckgo.com/lite/");
 assert_eq!(config.search.default_limit, 10);
 assert_eq!(config.search.max_query_chars, 512);
 }

 #[test]
 fn webhook_retention_defaults_and_bounds_are_validated() {
 let config = Config::default();
 assert_eq!(config.retention.webhook_deliveries_days, 30);
 assert_eq!(config.retention.webhook_dead_days, 90);
 config.validate().unwrap();
 for mutate in [
 (|config: &mut Config| config.retention.webhook_deliveries_days = 0) as fn(&mut Config),
 |config: &mut Config| config.retention.webhook_deliveries_days = 3_651,
 |config: &mut Config| config.retention.webhook_dead_days = 0,
 |config: &mut Config| config.retention.webhook_dead_days = 3_651,
 ] {
 let mut bad = Config::default();
 mutate(&mut bad);
 assert!(bad.validate().is_err());
 }
 let mut edge = Config::default();
 edge.retention.webhook_deliveries_days = 1;
 edge.retention.webhook_dead_days = 3_650;
 edge.validate().unwrap();
 }

 #[test]
 fn screenshot_retention_defaults_and_bounds_are_validated() {
 let config = Config::default();
 assert_eq!(config.retention.screenshots_days, 30);
 config.validate().unwrap();

 for days in [0, 3651] {
 let mut bad = Config::default();
 bad.retention.screenshots_days = days;
 assert!(
 bad.validate().is_err(),
 "retention.screenshots_days = {days} must be refused"
);
 }
 let mut edge = Config::default();
 edge.retention.screenshots_days = 1;
 edge.validate().unwrap();
 edge.retention.screenshots_days = 3650;
 edge.validate().unwrap();
 }

 #[test]
 fn search_endpoints_and_bounds_are_validated() {
 let mut config = Config::default();
 config.search.html_url = "not a url".to_string();
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.search.lite_url = "ftp://lite.example/".to_string();
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.search.default_limit = 11;
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.search.default_limit = 0;
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.search.max_query_chars = 0;
 assert!(config.validate().is_err());
 }

 #[test]
 fn monitor_wait_bounds_are_validated() {
 let mut config = Config::default();
 config.monitor.max_wait_ms = 0;
 assert!(config.validate().is_err());
 }

 #[test]
 fn render_limits_are_validated() {
 let mut config = Config::default();
 config.render.max_requests = 0;
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.render.settle_ms = config.render.ceiling_ms + 1;
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.render.restart_max = 1;
 config.render.restart_window_s = 0;
 assert!(config.validate().is_err());
 }

 #[test]
 fn interaction_bounds_are_validated() {
 let config = Config::default();
 assert_eq!(config.interaction.ttl_s, 120);
 assert_eq!(config.interaction.max_actions, 60);
 config.validate().unwrap();
 for ttl in [0, 29, 601] {
 let mut bad = Config::default();
 bad.interaction.ttl_s = ttl;
 assert!(
 bad.validate().is_err(),
 "interaction.ttl_s = {ttl} must be refused"
);
 }
 for actions in [0, 61] {
 let mut bad = Config::default();
 bad.interaction.max_actions = actions;
 assert!(
 bad.validate().is_err(),
 "interaction.max_actions = {actions} must be refused"
);
 }
 let mut edge = Config::default();
 edge.interaction.ttl_s = 30;
 edge.interaction.max_actions = 1;
 edge.validate().unwrap();
 edge.interaction.ttl_s = 600;
 edge.interaction.max_actions = 60;
 edge.validate().unwrap();
 }

 #[test]
 fn visual_capture_caps_are_bounded() {
 let config = Config::default();
 config.validate().unwrap();
 for bytes in [0, 64 * 1024 * 1024 + 1] {
 let mut bad = Config::default();
 bad.render.visual_max_bytes = bytes;
 assert!(
 bad.validate().is_err(),
 "render.visual_max_bytes = {bytes} must be refused"
);
 }
 let mut edge = Config::default();
 edge.render.visual_max_bytes = 1;
 edge.validate().unwrap();
 edge.render.visual_max_bytes = 64 * 1024 * 1024;
 edge.validate().unwrap();
 for requests in [0, 2_001] {
 let mut bad = Config::default();
 bad.render.visual_max_requests = requests;
 assert!(
 bad.validate().is_err(),
 "render.visual_max_requests = {requests} must be refused"
);
 }
 let mut edge = Config::default();
 edge.render.visual_max_requests = 1;
 edge.validate().unwrap();
 edge.render.visual_max_requests = 2_000;
 edge.validate().unwrap();
 for bytes in [0, 256 * 1024 * 1024 + 1] {
 let mut bad = Config::default();
 bad.render.max_screenshot_bytes = bytes;
 assert!(
 bad.validate().is_err(),
 "render.max_screenshot_bytes = {bytes} must be refused"
);
 }
 let mut edge = Config::default();
 edge.render.max_screenshot_bytes = 1;
 edge.validate().unwrap();
 edge.render.max_screenshot_bytes = 256 * 1024 * 1024;
 edge.validate().unwrap();
 }

 #[test]
 fn unknown_keys_are_rejected_rather_than_ignored() {
 let error = toml::from_str::<Config>("[fetch]\nallow_private = true\n").unwrap_err();
 assert!(error.to_string().contains("unknown field"));
 }

 // -----------------------------------------------------------------------
 // P0 — [embeddings]/[webhooks]/[scheduler] contracts
 // -----------------------------------------------------------------------

 fn webhook_target(url: &str) -> WebhookTargetConfig {
 WebhookTargetConfig {
 url: url.to_string(),
..WebhookTargetConfig::default()
 }
 }

 #[test]
 fn m07_sections_default_to_inert_and_the_pinned_limits() {
 let config = Config::default();
 assert!(!config.embeddings.enabled);
 assert_eq!(config.embeddings.model, "minilm-l6-v2-q");
 assert!(config.embeddings.model_dir.is_none());
 assert!(config.embeddings.ort_dylib_path.is_none());
 assert_eq!(config.embeddings.chunk_chars, 900);
 assert_eq!(config.embeddings.chunk_overlap, 120);
 assert_eq!(config.embeddings.max_chunks_per_page, 60);
 assert_eq!(config.embeddings.max_source_chars, 60_000);
 assert_eq!(config.embeddings.backfill_pages_per_pass, 20);
 assert_eq!(config.embeddings.backfill_interval_ms, 30_000);
 assert_eq!(config.embeddings.query_max_chars, 512);
 assert!(!config.webhooks.enabled);
 assert_eq!(config.webhooks.timeout_ms, 5_000);
 assert_eq!(config.webhooks.max_attempts, 5);
 assert!(!config.webhooks.allow_http);
 assert!(config.webhooks.targets.is_empty());
 assert!(!config.scheduler.enabled);
 assert_eq!(config.scheduler.tick_ms, 30_000);
 assert_eq!(config.scheduler.max_checks_per_tick, 25);
 config.validate().unwrap();
 }

 #[test]
 fn embeddings_bounds_and_the_overlap_rule_are_validated() {
 for mutate in [
 (|config: &mut Config| config.embeddings.chunk_chars = 199) as fn(&mut Config),
 |config: &mut Config| config.embeddings.chunk_chars = 2_001,
 |config: &mut Config| config.embeddings.chunk_overlap = 451,
 |config: &mut Config| config.embeddings.max_chunks_per_page = 0,
 |config: &mut Config| config.embeddings.max_chunks_per_page = 201,
 |config: &mut Config| config.embeddings.max_source_chars = 999,
 |config: &mut Config| config.embeddings.max_source_chars = 200_001,
 |config: &mut Config| config.embeddings.backfill_pages_per_pass = 0,
 |config: &mut Config| config.embeddings.backfill_pages_per_pass = 201,
 |config: &mut Config| config.embeddings.backfill_interval_ms = 999,
 |config: &mut Config| config.embeddings.backfill_interval_ms = 600_001,
 |config: &mut Config| config.embeddings.query_max_chars = 0,
 |config: &mut Config| config.embeddings.query_max_chars = 4_097,
 |config: &mut Config| config.embeddings.model = "other".to_string(),
 |config: &mut Config| config.embeddings.model_dir = Some(PathBuf::new()),
 |config: &mut Config| config.embeddings.ort_dylib_path = Some(PathBuf::new()),
 ] {
 let mut bad = Config::default();
 mutate(&mut bad);
 assert!(
 bad.validate().is_err(),
 "an invalid [embeddings] value must be refused"
);
 }
 let mut edge = Config::default();
 edge.embeddings.chunk_chars = 200;
 edge.embeddings.chunk_overlap = 100;
 edge.validate().unwrap();
 edge.embeddings.chunk_chars = 2_000;
 edge.embeddings.chunk_overlap = 1_000;
 edge.validate().unwrap();
 }

 #[test]
 fn webhook_targets_are_validated() {
 let mut config = Config::default();
 config.webhooks.targets.insert(
 "ops".to_string(),
 webhook_target("https://example.com/hook"));
 config.validate().unwrap();

 // `http://` needs the explicit operator opt-in.
 let mut target = Config::default();
 target.webhooks.targets.insert(
 "ops".to_string(),
 webhook_target("http://127.0.0.1:9999/hook"));
 assert!(target.validate().is_err());
 target.webhooks.allow_http = true;
 target.validate().unwrap();

 let names = vec![
 "Ops".to_string(),
 "-ops".to_string(),
 String::new(),
 "with space".to_string(),
 "a".repeat(33),
 ];
 for name in names {
 let mut bad = Config::default();
 bad.webhooks
.targets
.insert(name.clone(), webhook_target("https://example.com/hook"));
 assert!(
 bad.validate().is_err(),
 "target name `{name}` must be refused"
);
 }
 for url in [
 "not a url",
 "ftp://example.com/hook",
 "https://user:pass@example.com/hook",
 "mailto:ops@example.com",
 ] {
 let mut bad = Config::default();
 bad.webhooks
.targets
.insert("ops".to_string(), webhook_target(url));
 assert!(
 bad.validate().is_err(),
 "target url `{url}` must be refused"
);
 }
 let mut both = Config::default();
 let mut target = webhook_target("https://example.com/hook");
 target.secret_env = Some("HOOK_SECRET".to_string());
 target.secret_file = Some(PathBuf::from("/run/secrets/hook"));
 both.webhooks.targets.insert("ops".to_string(), target);
 assert!(both.validate().is_err());
 let mut both = Config::default();
 let mut target = webhook_target("https://example.com/hook");
 target.auth_env = Some("HOOK_AUTH".to_string());
 target.auth_file = Some(PathBuf::from("/run/secrets/auth"));
 both.webhooks.targets.insert("ops".to_string(), target);
 assert!(both.validate().is_err());

 // `kind` is a closed set, refused at parse time.
 let error = toml::from_str::<Config>(
 "[webhooks.targets.ops]\nurl = \"https://example.com/hook\"\nkind = \"teams\"\n")
.unwrap_err();
 assert!(
 error.to_string().contains("unknown variant"),
 "unexpected error: {error}"
);
 }

 #[test]
 fn scheduler_bounds_are_validated() {
 for mutate in [
 (|config: &mut Config| config.scheduler.tick_ms = 999) as fn(&mut Config),
 |config: &mut Config| config.scheduler.tick_ms = 600_001,
 |config: &mut Config| config.scheduler.max_checks_per_tick = 0,
 |config: &mut Config| config.scheduler.max_checks_per_tick = 501,
 ] {
 let mut bad = Config::default();
 mutate(&mut bad);
 assert!(
 bad.validate().is_err(),
 "an invalid [scheduler] value must be refused"
);
 }
 }

 #[test]
 fn m07_sections_load_the_same_values_from_a_file() {
 let text = r#"
[embeddings]
enabled = true
model = "minilm-l6-v2-q"
chunk_chars = 400
chunk_overlap = 50
max_chunks_per_page = 10
max_source_chars = 10_000
backfill_pages_per_pass = 5
backfill_interval_ms = 2_000
query_max_chars = 256

[webhooks]
enabled = true
timeout_ms = 1_000
max_attempts = 3
allow_http = true

[webhooks.targets.ops]
url = "http://127.0.0.1:9999/hook"
kind = "slack"
secret_env = "HOOK_SECRET"
auth_env = "HOOK_AUTH"
include_diff = true

[scheduler]
enabled = true
tick_ms = 5_000
max_checks_per_tick = 10
"#;
 let config: Config = toml::from_str(text).unwrap();
 config.validate().unwrap();
 assert!(config.embeddings.enabled);
 assert_eq!(config.embeddings.chunk_chars, 400);
 assert_eq!(config.embeddings.chunk_overlap, 50);
 assert!(config.webhooks.enabled);
 assert!(config.webhooks.allow_http);
 let target = &config.webhooks.targets["ops"];
 assert_eq!(target.kind, WebhookKind::Slack);
 assert_eq!(target.secret_env.as_deref(), Some("HOOK_SECRET"));
 assert!(target.include_diff);
 assert!(config.scheduler.enabled);
 assert_eq!(config.scheduler.max_checks_per_tick, 10);

 let reparsed: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
 assert_eq!(
 toml::to_string(&config).unwrap(),
 toml::to_string(&reparsed).unwrap(),
 "the same config loads to the same values"
);
 }

 #[test]
 fn user_agent_includes_contact_when_present() {
 let mut config = Config::default();
 assert_eq!(
 config.user_agent(),
 format!("web-widow/{}", env!("CARGO_PKG_VERSION"))
);
 config.fetch.contact = Some("ops@example.com".to_string());
 assert_eq!(
 config.user_agent(),
 format!("web-widow/{} (+ops@example.com)", env!("CARGO_PKG_VERSION"))
);
 }

 #[test]
 fn zero_limits_are_rejected() {
 let mut config = Config::default();
 config.fetch.max_body_bytes = 0;
 assert!(config.validate().is_err());
 let mut config = Config::default();
 config.extract.max_chars = 0;
 assert!(config.validate().is_err());
 }
}

#[cfg(test)]
mod http_tls_config_tests {
 use super::*;

 #[test]
 fn tls_paths_are_both_or_neither_and_require_tls() {
 let mut config = Config::default();
 config.http.tls = true;
 assert!(config.validate().is_ok(), "generated mode needs no paths");

 config.http.tls_cert_file = Some(PathBuf::from("/tmp/cert.pem"));
 config.http.tls_key_file = None;
 let error = config.validate().unwrap_err();
 assert!(
 matches!(error, WidowError::SchemaInvalid { ref pointer,.. } if pointer == "/http/tls_cert_file"),
 "{error:?}"
);

 config.http.tls_key_file = Some(PathBuf::from("/tmp/key.pem"));
 config.http.tls = false;
 let error = config.validate().unwrap_err();
 assert!(
 matches!(error, WidowError::SchemaInvalid { ref pointer,.. } if pointer == "/http/tls"),
 "{error:?}"
);

 config.http.tls = true;
 assert!(config.validate().is_ok(), "a complete override is valid");
 }
}
/// A0 — the `[auth]` contract is validated at load and inert at runtime
/// (`IMP-LANE-A` §1, checks A0-2/A0-3).
#[cfg(test)]
mod auth_config_tests {
 use super::*;

 fn config_with(body: &str) -> Result<Config> {
 let text = format!("[auth]\n{body}");
 let config: Config = toml::from_str(&text).expect("the TOML itself parses");
 config.validate()?;
 Ok(config)
 }

 fn assert_refused(body: &str, field: &str) {
 let error = match config_with(body) {
 Ok(_) => panic!("`{body}` must be refused"),
 Err(error) => error.to_string(),
 };
 assert!(error.contains(field), "expected `{field}` in `{error}`");
 }

 const MANAGERS: &str = r#"
[auth.managers.op]
path = "/bin/sh"
env_allow = ["OP_SERVICE_ACCOUNT_TOKEN"]

[auth.managers.bw]
path = "/bin/sh"
env_allow = ["BW_SESSION"]

[auth.managers.pass]
path = "/bin/sh"
env_allow = []
"#;

 #[test]
 fn the_default_auth_contract_is_empty_valid_and_inert() {
 let config = Config::default();
 config.validate().unwrap();
 assert_eq!(config.auth.audit_days, 90);
 assert_eq!(config.auth.manager_timeout_ms, 5_000);
 assert!(!config.auth.allow_loopback_http);
 assert!(config.auth.managers.is_empty());
 assert!(config.auth.profiles.is_empty());
 }

 #[test]
 fn the_documented_contract_validates() {
 let body = format!(
 r#"
audit_days = 30
manager_timeout_ms = 2500
allow_loopback_http = true
{MANAGERS}
[auth.profiles.github]
kind = "bearer"
host = "github.com"
reference = "op://Private/github/token"
allow_subdomains = true

[auth.profiles.legacy]
kind = "basic"
host = "legacy.example.com"
username_reference = "op://Private/legacy/username"
password_reference = "op://Private/legacy/password"

[auth.profiles.internal]
kind = "header"
host = "internal.example.com"
header = "X-Api-Key"
reference = "op://Private/internal/key"

[auth.profiles.loopback]
kind = "bearer"
host = "::1"
reference = "pass://store/service/token"
"#
);
 let config = config_with(&body).expect("the documented contract validates");
 assert_eq!(config.auth.audit_days, 30);
 assert_eq!(config.auth.manager_timeout_ms, 2_500);
 assert!(config.auth.allow_loopback_http);
 assert_eq!(config.auth.profiles.len(), 4);
 assert_eq!(config.auth.profiles["github"].kind, AuthKind::Bearer);
 assert!(config.auth.profiles["github"].allow_subdomains);
 assert_eq!(config.auth.profiles["legacy"].kind, AuthKind::Basic);
 assert_eq!(config.auth.profiles["internal"].kind, AuthKind::Header);
 }

 #[test]
 fn auth_bounds_are_enforced() {
 for days in [0_u32, 3_651] {
 assert_refused(&format!("audit_days = {days}"), "auth.audit_days");
 }
 for timeout in [0_u64, 99, 60_001] {
 assert_refused(
 &format!("manager_timeout_ms = {timeout}"),
 "auth.manager_timeout_ms");
 }
 }

 #[test]
 fn manager_entries_must_name_a_real_absolute_binary() {
 assert_refused(
 "[auth.managers.vault]\npath = \"/bin/sh\"",
 "auth.managers.vault: unknown provider");
 assert_refused(
 "[auth.managers.op]\npath = \"op\"",
 "auth.managers.op.path must be an absolute path");
 assert_refused(
 "[auth.managers.op]\npath = \"/nonexistent/op\"",
 "auth.managers.op.path");

 let temp = tempfile::tempdir().unwrap();
 let plain = temp.path().join("op");
 std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
 assert_refused(
 &format!("[auth.managers.op]\npath = \"{}\"", plain.display()),
 "is not executable");
 }

 #[test]
 fn env_allow_is_the_only_environment_the_child_gets() {
 assert_refused(
 "[auth.managers.op]\npath = \"/bin/sh\"\nenv_allow = [\"PATH\"]",
 "must not name PATH");
 assert_refused(
 "[auth.managers.op]\npath = \"/bin/sh\"\nenv_allow = [\"WEB_WIDOW_HTTP_TOKEN\"]",
 "server's own variables");
 assert_refused(
 "[auth.managers.op]\npath = \"/bin/sh\"\nenv_allow = [\"1BAD\"]",
 "auth.managers.op.env_allow");
 assert_refused(
 "[auth.managers.op]\npath = \"/bin/sh\"\nenv_allow = [\"OP_TOKEN\", \"OP_TOKEN\"]",
 "names `OP_TOKEN` twice");
 }

 #[test]
 fn profile_names_and_hosts_are_canonical() {
 for name in ["\"..\"", "\".\"", "\"-x\"", "\"bad name\""] {
 let body = format!(
 "[auth.managers.op]\npath = \"/bin/sh\"\n[auth.profiles.{name}]\nhost = \"example.com\"\nreference = \"op://V/i/f\""
);
 assert_refused(&body, "profile name must match");
 }
 for host in [
 "Example.com",
 "https://example.com",
 "example.com:443",
 "example.com/path",
 "",
 ] {
 let body = format!(
 "[auth.managers.op]\npath = \"/bin/sh\"\n[auth.profiles.site]\nhost = \"{host}\"\nreference = \"op://V/i/f\""
);
 assert_refused(&body, "auth.profiles.site.host");
 }
 }

 #[test]
 fn the_profile_origin_port_defaults_to_443_and_is_bounded() {
 let body = |extra: &str| {
 format!(
 "[auth.managers.op]\npath = \"/bin/sh\"\n[auth.profiles.site]\nhost = \"example.com\"\nreference = \"op://V/i/f\"\n{extra}"
)
 };

 let config = config_with(&body("")).expect("the port is optional");
 assert_eq!(
 config.auth.profiles["site"].port, 443,
 "default origin port"
);

 let config = config_with(&body("port = 8443\n")).expect("a non-default port is valid");
 assert_eq!(config.auth.profiles["site"].port, 8443);

 assert_refused(&body("port = 0\n"), "auth.profiles.site.port");
 }

 #[test]
 fn reference_grammar_is_enforced() {
 let cases = [
 ("op://vault/item", "op:// needs"),
 ("op://vault/item/field/extra", "op:// needs"),
 ("bw://item/field/extra", "bw:// needs"),
 ("pass://store", "pass:// needs"),
 ("pass:///absolute/path", "absolute or empty path segment"),
 ("op://vault/../item/field", "`.` or `..`"),
 ("op://-vault/item/field", "must not start with '-'"),
 ("op://vault/it em/field", "whitespace or control"),
 (
 "https://vault/item/field",
 "must start with op://, bw:// or pass://"),
 ("ghp_inlinevalue", "must start with op://, bw:// or pass://"),
 ];
 for (reference, expected) in cases {
 let body = format!(
 "[auth.managers.op]\npath = \"/bin/sh\"\n[auth.managers.bw]\npath = \"/bin/sh\"\n[auth.managers.pass]\npath = \"/bin/sh\"\n[auth.profiles.site]\nhost = \"example.com\"\nreference = \"{reference}\""
);
 assert_refused(&body, expected);
 }
 // A reference may not silently cross providers.
 let body = "[auth.managers.op]\npath = \"/bin/sh\"\n[auth.profiles.site]\nhost = \"example.com\"\nreference = \"bw://item\"";
 assert_refused(body, "auth.managers.bw");
 }

 #[test]
 fn kind_decides_which_reference_fields_exist() {
 let managers = "[auth.managers.op]\npath = \"/bin/sh\"\n";
 for (body, field) in [
 (
 "kind = \"bearer\"\nhost = \"e.com\"",
 "auth.profiles.site.reference is required"),
 (
 "kind = \"bearer\"\nhost = \"e.com\"\nreference = \"op://V/i/f\"\nusername_reference = \"op://V/i/u\"",
 "username_reference is not used"),
 (
 "kind = \"basic\"\nhost = \"e.com\"\nreference = \"op://V/i/f\"\nusername_reference = \"op://V/i/u\"",
 "reference is not used"),
 (
 "kind = \"basic\"\nhost = \"e.com\"\nusername_reference = \"op://V/i/u\"",
 "password_reference is required"),
 (
 "kind = \"header\"\nhost = \"e.com\"\nreference = \"op://V/i/f\"",
 "header is required"),
 ] {
 assert_refused(&format!("{managers}[auth.profiles.site]\n{body}"), field);
 }
 }

 #[test]
 fn transport_headers_cannot_be_set_by_a_profile() {
 let managers = "[auth.managers.op]\npath = \"/bin/sh\"\n";
 for header in [
 "Host",
 "cookie",
 "Content-Length",
 "Transfer-Encoding",
 "Connection",
 "Proxy-Authorization",
 "X Bad",
 "X-Bad\\r\\nInjected",
 ] {
 let body = format!(
 "{managers}[auth.profiles.site]\nkind = \"header\"\nhost = \"e.com\"\nheader = \"{header}\"\nreference = \"op://V/i/f\""
);
 assert_refused(&body, "auth.profiles.site.header");
 }
 let body = format!(
 "{managers}[auth.profiles.site]\nkind = \"header\"\nhost = \"e.com\"\nheader = \"Authorization\"\nreference = \"op://V/i/f\""
);
 assert!(config_with(&body).is_ok(), "Authorization stays available");
 }

 #[test]
 fn malformed_auth_toml_is_refused_at_parse() {
 for (body, expected) in [
 (
 "[auth.profiles.site]\nkind = \"oauth\"\nhost = \"e.com\"",
 "unknown variant"),
 (
 "[auth.profiles.site]\nallow_subdomains = \"yes\"",
 "invalid type"),
 ("bogus = 1", "unknown field"),
 (
 "[auth.profiles.site]\nhost = \"e.com\"\nreference = 7",
 "invalid type"),
 ] {
 let text = format!("[auth]\n{body}");
 let error = toml::from_str::<Config>(&text).expect_err("must not parse");
 assert!(
 error.to_string().contains(expected),
 "expected `{expected}` in `{error}`"
);
 }
 }
}

/// the `kind = "session"` contract.
#[cfg(test)]
mod session_config_tests {
 use super::*;

 fn config_with(profile: &str) -> Result<Config> {
 let text = format!("[auth.profiles.site]\n{profile}");
 let config: Config = toml::from_str(&text).expect("the TOML itself parses");
 config.validate()?;
 Ok(config)
 }

 const VALID: &str = r#"
kind = "session"
host = "login.example.test"
login_url = "https://login.example.test/login"
login_origins = ["login.example.test", "idp.test"]
verify_url = "https://login.example.test/verify"
assets = ["cdn.example.test"]
session_max_age_days = 30
"#;

 #[test]
 fn a_session_profile_parses_and_round_trips() {
 let config = config_with(VALID).expect("a valid session profile");
 let profile = &config.auth.profiles["site"];
 assert_eq!(profile.kind, AuthKind::Session);
 assert_eq!(profile.login_origins.len(), 2);
 assert_eq!(profile.session_max_age_days, Some(30));
 let reparsed: Config =
 toml::from_str(&toml::to_string(&config).unwrap()).expect("re-parse");
 assert_eq!(
 toml::to_string(&config).unwrap(),
 toml::to_string(&reparsed).unwrap()
);
 }

 #[test]
 fn a_session_profile_defaults_its_lifetime_bound() {
 let without = VALID.replace("session_max_age_days = 30", "");
 let config = config_with(&without).expect("the default lifetime is valid");
 assert_eq!(config.auth.profiles["site"].session_max_age_days, None);
 }

 #[test]
 fn a_session_profile_forbids_reference_and_header_fields() {
 for (field, line) in [
 ("reference", "reference = \"op://vault/item/token\""),
 (
 "username_reference",
 "username_reference = \"op://vault/item/user\""),
 (
 "password_reference",
 "password_reference = \"op://vault/item/pass\""),
 ("header", "header = \"X-Api-Key\""),
 ] {
 let body = format!("{VALID}\n{line}");
 let error = config_with(&body).expect_err("must be refused").to_string();
 assert!(error.contains(field), "expected `{field}` in `{error}`");
 }
 }

 #[test]
 fn a_non_session_profile_forbids_session_fields() {
 for field in [
 "login_url = \"https://github.com/login\"",
 "login_origins = [\"github.com\"]",
 "verify_url = \"https://github.com/\"",
 "assets = [\"cdn.github.com\"]",
 "session_max_age_days = 7",
 ] {
 let text = format!(
 "[auth.managers.op]\npath = \"/bin/true\"\nenv_allow = []\n\n\
 [auth.profiles.site]\nkind = \"bearer\"\nhost = \"github.com\"\n\
 reference = \"op://vault/item/token\"\n{field}\n"
);
 let config: Config = toml::from_str(&text).expect("the TOML itself parses");
 let error = config.validate().expect_err("must be refused").to_string();
 assert!(
 error.contains("only valid for kind = \"session\""),
 "expected a session-only refusal for `{field}`, got `{error}`"
);
 }
 }

 #[test]
 fn a_session_login_and_verify_url_must_be_https() {
 let body = VALID.replace(
 "login_url = \"https://login.example.test/login\"",
 "login_url = \"http://login.example.test/login\"");
 let error = config_with(&body).expect_err("http login_url").to_string();
 assert!(error.contains("login_url must be https"), "{error}");

 let body = VALID.replace(
 "verify_url = \"https://login.example.test/verify\"",
 "verify_url = \"http://login.example.test/verify\"");
 let error = config_with(&body).expect_err("http verify_url").to_string();
 assert!(error.contains("verify_url must be https"), "{error}");
 }

 #[test]
 fn a_session_verify_url_must_be_on_the_binding_host() {
 let body = VALID.replace(
 "verify_url = \"https://login.example.test/verify\"",
 "verify_url = \"https://other.example.test/verify\"");
 let error = config_with(&body).expect_err("off-host verify").to_string();
 assert!(
 error.contains("verify_url must be on the binding host"),
 "{error}"
);
 }

 #[test]
 fn a_session_requires_declared_origins_including_the_binding_host() {
 let body = VALID.replace(
 "login_origins = [\"login.example.test\", \"idp.test\"]",
 "login_origins = []");
 let error = config_with(&body).expect_err("empty origins").to_string();
 assert!(error.contains("login_origins must declare"), "{error}");

 let body = VALID.replace(
 "login_origins = [\"login.example.test\", \"idp.test\"]",
 "login_origins = [\"idp.test\"]");
 let error = config_with(&body)
.expect_err("binding host absent")
.to_string();
 assert!(error.contains("must contain the binding host"), "{error}");
 }

 #[test]
 fn a_session_origin_and_asset_must_be_a_bare_host() {
 for bad in ["*.example.test", "https://example.test", "example.test:443"] {
 let body = VALID.replace(
 "login_origins = [\"login.example.test\", \"idp.test\"]",
 &format!("login_origins = [\"login.example.test\", \"{bad}\"]"));
 let error = config_with(&body).expect_err("bad origin").to_string();
 assert!(error.contains("login_origins"), "{bad}: {error}");

 let body = VALID.replace(
 "assets = [\"cdn.example.test\"]",
 &format!("assets = [\"{bad}\"]"));
 let error = config_with(&body).expect_err("bad asset").to_string();
 assert!(error.contains("assets"), "{bad}: {error}");
 }
 }

 #[test]
 fn a_session_lifetime_is_bounded() {
 for days in ["0", "366"] {
 let body = VALID.replace(
 "session_max_age_days = 30",
 &format!("session_max_age_days = {days}"));
 let error = config_with(&body).expect_err("out of bounds").to_string();
 assert!(
 error.contains("session_max_age_days must be 1..=365"),
 "{error}"
);
 }
 }

 #[test]
 fn a_session_login_url_must_not_carry_userinfo_or_a_fragment() {
 let body = VALID.replace(
 "login_url = \"https://login.example.test/login\"",
 "login_url = \"https://user:pass@login.example.test/login\"");
 let error = config_with(&body).expect_err("userinfo").to_string();
 assert!(error.contains("userinfo"), "{error}");

 let body = VALID.replace(
 "login_url = \"https://login.example.test/login\"",
 "login_url = \"https://login.example.test/login#x\"");
 let error = config_with(&body).expect_err("fragment").to_string();
 assert!(error.contains("fragment"), "{error}");
 }
}
