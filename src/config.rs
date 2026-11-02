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
    /// Safety ceiling for records returned by `extract` (IMP-SAFE-001: 5 000).
    pub max_records: usize,
    /// Per-string-value cap for `extract` (IMP-DEC-016). A per-call value can
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

/// Bulk-crawl ceilings (IMP-B04 / IMP-DATA-001). Per-job `limits` may lower
/// these but never raise them.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlConfig {
    /// Hard page ceiling per job (IMP-SAFE-001: 5 000).
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

/// Change monitoring (IMP-B05 / IMP-DATA-001). Thresholds are our semantics;
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

/// Discovery endpoints (IMP-B01 / IMP-CONTRACT-002 §1.2). Endpoints are server
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

/// `[feed]` — `fetch_feed` limits (IMP-DEC-019 §4).
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
/// (IMP-DEC-030). Bounds the fetched body, the decompressed size at 4×, the
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

/// `[pdf]` — `pdf_extract` ceilings and the PDFium engine (IMP-DEC-041).
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

/// `[events]` — the change feed (`events` tool + the loopback GET route,
/// IMP-DEC-038 / IMP-ITEM-004).
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

/// `[fts]` — the local full-text index and `search_content` (IMP-DEC-027/029).
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

/// Browser interaction (`interact_*`, IMP-DEC-039/040). One held page per
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

/// `[embeddings]` — the local semantic index contract (IMP-ITEM-005 / IMP-DEC-049).
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

/// `[webhooks]` — deliberate egress contract (IMP-ITEM-007 / IMP-DEC-049).
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

/// `[scheduler]` — the opt-in in-process timer (IMP-ITEM-006 / IMP-DEC-049).
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

/// `^[a-z0-9][a-z0-9_-]{0,31}$` without a regex compile per validation.
fn valid_target_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    matches!(bytes.first(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.len() <= 32
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
        })
}

/// JavaScript rendering (IMP-DEC-002 / ITEM-004). The browser is optional: with
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
        data_dir_override: Option<PathBuf>,
    ) -> Result<LoadedConfig> {
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
                "fetch.max_redirects must be <= 10".to_string(),
            ));
        }
        if fetch.max_concurrent_per_host == 0 {
            return Err(WidowError::InvalidUrl(
                "fetch.max_concurrent_per_host must be >= 1".to_string(),
            ));
        }
        if self.extract.max_links == 0 {
            return Err(WidowError::InvalidUrl(
                "extract.max_links must be >= 1".to_string(),
            ));
        }
        if self.extract.max_records == 0 {
            return Err(WidowError::InvalidUrl(
                "extract.max_records must be >= 1".to_string(),
            ));
        }
        if self.extract.max_field_chars == 0 {
            return Err(WidowError::InvalidUrl(
                "extract.max_field_chars must be >= 1".to_string(),
            ));
        }
        if self.feed.max_limit == 0 || self.feed.max_limit > 100 {
            return Err(WidowError::InvalidUrl(
                "feed.max_limit must be 1..=100".to_string(),
            ));
        }
        if self.feed.default_limit == 0 || self.feed.default_limit > self.feed.max_limit {
            return Err(WidowError::InvalidUrl(
                "feed.default_limit must be >= 1 and <= feed.max_limit".to_string(),
            ));
        }
        if self.feed.max_summary_chars == 0 {
            return Err(WidowError::InvalidUrl(
                "feed.max_summary_chars must be >= 1".to_string(),
            ));
        }
        let sitemap = &self.sitemap;
        if sitemap.max_bytes == 0 || sitemap.max_bytes > 64 * 1024 * 1024 {
            return Err(WidowError::InvalidUrl(
                "sitemap.max_bytes must be 1..=67108864 (64 MiB)".to_string(),
            ));
        }
        if sitemap.max_urls == 0 || sitemap.max_urls > 50_000 {
            return Err(WidowError::InvalidUrl(
                "sitemap.max_urls must be 1..=50000".to_string(),
            ));
        }
        if sitemap.max_children == 0 || sitemap.max_children > 10 {
            return Err(WidowError::InvalidUrl(
                "sitemap.max_children must be 1..=10".to_string(),
            ));
        }
        if self.pdf.max_bytes == 0 || self.pdf.max_bytes > 64 * 1024 * 1024 {
            return Err(WidowError::InvalidUrl(
                "pdf.max_bytes must be 1..=67108864 (64 MiB)".to_string(),
            ));
        }
        if self.pdf.max_pages == 0 || self.pdf.max_pages > 2_000 {
            return Err(WidowError::InvalidUrl(
                "pdf.max_pages must be 1..=2000".to_string(),
            ));
        }
        if self.pdf.max_chars == 0 || self.pdf.max_chars > 5_000_000 {
            return Err(WidowError::InvalidUrl(
                "pdf.max_chars must be 1..=5000000".to_string(),
            ));
        }
        if self.events.feed_items == 0 || self.events.feed_items > 200 {
            return Err(WidowError::InvalidUrl(
                "events.feed_items must be 1..=200 (the RSS document caps at 100)".to_string(),
            ));
        }
        if self
            .pdf
            .pdfium_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(WidowError::InvalidUrl(
                "pdf.pdfium_path must not be empty (unset it to use discovery)".to_string(),
            ));
        }
        if self.fts.max_limit == 0 || self.fts.max_limit > 100 {
            return Err(WidowError::InvalidUrl(
                "fts.max_limit must be 1..=100".to_string(),
            ));
        }
        if self.fts.default_limit == 0 || self.fts.default_limit > self.fts.max_limit {
            return Err(WidowError::InvalidUrl(
                "fts.default_limit must be >= 1 and <= fts.max_limit".to_string(),
            ));
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
                "crawl.max_pages, crawl.max_duration_s and crawl.lease_s must be > 0".to_string(),
            ));
        }
        if crawl.max_attempts == 0 {
            return Err(WidowError::InvalidUrl(
                "crawl.max_attempts must be >= 1".to_string(),
            ));
        }
        if crawl.max_error_samples == 0 || crawl.max_error_samples > 5 {
            return Err(WidowError::InvalidUrl(
                "crawl.max_error_samples must be 1..=5 (the contract caps samples at 5)"
                    .to_string(),
            ));
        }
        if !crawl.default_per_host_rate.is_finite() || crawl.default_per_host_rate <= 0.0 {
            return Err(WidowError::InvalidUrl(
                "crawl.default_per_host_rate must be a positive number".to_string(),
            ));
        }
        if crawl.result_limit_default == 0
            || crawl.result_limit_max == 0
            || crawl.result_limit_default > crawl.result_limit_max
            || crawl.result_limit_max > 500
        {
            return Err(WidowError::InvalidUrl(
                "crawl.result_limit_default must be 1..=crawl.result_limit_max, and the max <= 500"
                    .to_string(),
            ));
        }
        let monitor = &self.monitor;
        if monitor.min_interval_s == 0
            || monitor.default_interval_s < monitor.min_interval_s
            || monitor.default_interval_s > monitor.max_interval_s
        {
            return Err(WidowError::InvalidUrl(
                "monitor.min_interval_s must be > 0 and monitor.default_interval_s must be within min..=max"
                    .to_string(),
            ));
        }
        if monitor.max_diff_bytes < 512 {
            return Err(WidowError::InvalidUrl(
                "monitor.max_diff_bytes must be >= 512 (a smaller cap is not a usable diff)"
                    .to_string(),
            ));
        }
        if monitor.cosmetic_max_changed_lines == 0 {
            return Err(WidowError::InvalidUrl(
                "monitor.cosmetic_max_changed_lines must be >= 1".to_string(),
            ));
        }
        if monitor.max_ignore_selectors == 0 || monitor.max_checks_per_call == 0 {
            return Err(WidowError::InvalidUrl(
                "monitor.max_ignore_selectors and monitor.max_checks_per_call must be > 0"
                    .to_string(),
            ));
        }
        if monitor.changes_limit_default == 0
            || monitor.changes_limit_max == 0
            || monitor.changes_limit_default > monitor.changes_limit_max
            || monitor.changes_limit_max > 500
        {
            return Err(WidowError::InvalidUrl(
                "monitor.changes_limit_default must be 1..=monitor.changes_limit_max, and the max <= 500"
                    .to_string(),
            ));
        }
        if monitor.max_wait_ms == 0 {
            return Err(WidowError::InvalidUrl(
                "monitor.max_wait_ms must be > 0".to_string(),
            ));
        }
        if self.retention.screenshots_days == 0 || self.retention.screenshots_days > 3650 {
            return Err(WidowError::InvalidUrl(
                "retention.screenshots_days must be 1..=3650".to_string(),
            ));
        }
        for (name, value) in [
            (
                "retention.webhook_deliveries_days",
                self.retention.webhook_deliveries_days,
            ),
            (
                "retention.webhook_dead_days",
                self.retention.webhook_dead_days,
            ),
        ] {
            if !(1..=3650).contains(&value) {
                return Err(WidowError::InvalidUrl(format!("{name} must be 1..=3650")));
            }
        }
        if self.retention.snapshots_per_monitor == 0 {
            return Err(WidowError::InvalidUrl(
                "retention.snapshots_per_monitor must be >= 1".to_string(),
            ));
        }
        if !(30..=600).contains(&self.interaction.ttl_s) {
            return Err(WidowError::InvalidUrl(
                "interaction.ttl_s must be 30..=600".to_string(),
            ));
        }
        if !(1..=60).contains(&self.interaction.max_actions) {
            return Err(WidowError::InvalidUrl(
                "interaction.max_actions must be 1..=60".to_string(),
            ));
        }
        if crawl.idle_poll_ms == 0 {
            return Err(WidowError::InvalidUrl(
                "crawl.idle_poll_ms must be > 0".to_string(),
            ));
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
                render.max_concurrent_pages as u64,
            ),
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
                "render.settle_ms must be <= render.ceiling_ms".to_string(),
            ));
        }
        if render.visual_max_bytes == 0 || render.visual_max_bytes > 64 * 1024 * 1024 {
            return Err(WidowError::InvalidUrl(
                "render.visual_max_bytes must be 1..=64 MiB".to_string(),
            ));
        }
        if !(1..=2_000).contains(&render.visual_max_requests) {
            return Err(WidowError::InvalidUrl(
                "render.visual_max_requests must be 1..=2000".to_string(),
            ));
        }
        if render.max_screenshot_bytes == 0 || render.max_screenshot_bytes > 256 * 1024 * 1024 {
            return Err(WidowError::InvalidUrl(
                "render.max_screenshot_bytes must be 1..=256 MiB".to_string(),
            ));
        }
        if render.restart_max > 0 && render.restart_window_s == 0 {
            return Err(WidowError::InvalidUrl(
                "render.restart_window_s must be > 0 when render.restart_max > 0".to_string(),
            ));
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
                "search.default_limit must be 1..=10".to_string(),
            ));
        }
        if self.search.max_query_chars == 0 {
            return Err(WidowError::InvalidUrl(
                "search.max_query_chars must be > 0".to_string(),
            ));
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
                "embeddings.chunk_chars must be 200..=2000".to_string(),
            ));
        }
        if embeddings.chunk_overlap > embeddings.chunk_chars / 2 {
            return Err(WidowError::InvalidUrl(
                "embeddings.chunk_overlap must be <= embeddings.chunk_chars / 2".to_string(),
            ));
        }
        if !(1..=200).contains(&embeddings.max_chunks_per_page) {
            return Err(WidowError::InvalidUrl(
                "embeddings.max_chunks_per_page must be 1..=200".to_string(),
            ));
        }
        if !(1_000..=200_000).contains(&embeddings.max_source_chars) {
            return Err(WidowError::InvalidUrl(
                "embeddings.max_source_chars must be 1000..=200000".to_string(),
            ));
        }
        if !(1..=200).contains(&embeddings.backfill_pages_per_pass) {
            return Err(WidowError::InvalidUrl(
                "embeddings.backfill_pages_per_pass must be 1..=200".to_string(),
            ));
        }
        if !(1_000..=600_000).contains(&embeddings.backfill_interval_ms) {
            return Err(WidowError::InvalidUrl(
                "embeddings.backfill_interval_ms must be 1000..=600000".to_string(),
            ));
        }
        if !(1..=4_096).contains(&embeddings.query_max_chars) {
            return Err(WidowError::InvalidUrl(
                "embeddings.query_max_chars must be 1..=4096".to_string(),
            ));
        }
        if embeddings
            .model_dir
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(WidowError::InvalidUrl(
                "embeddings.model_dir must not be empty (unset it for the default)".to_string(),
            ));
        }
        if embeddings
            .ort_dylib_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(WidowError::InvalidUrl(
                "embeddings.ort_dylib_path must not be empty (unset it for discovery)".to_string(),
            ));
        }
        let webhooks = &self.webhooks;
        if !(500..=30_000).contains(&webhooks.timeout_ms) {
            return Err(WidowError::InvalidUrl(
                "webhooks.timeout_ms must be 500..=30000".to_string(),
            ));
        }
        if !(1..=10).contains(&webhooks.max_attempts) {
            return Err(WidowError::InvalidUrl(
                "webhooks.max_attempts must be 1..=10".to_string(),
            ));
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
                "scheduler.tick_ms must be 1000..=600000".to_string(),
            ));
        }
        if !(1..=500).contains(&self.scheduler.max_checks_per_tick) {
            return Err(WidowError::InvalidUrl(
                "scheduler.max_checks_per_tick must be 1..=500".to_string(),
            ));
        }
        Ok(())
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
                    "no data directory: set storage.data_dir or WEB_WIDOW_DATA".to_string(),
                )
            })
    }
}

/// Resolved paths inside the data directory (IMP-DATA-001 layout).
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
    // M07 P0 — [embeddings]/[webhooks]/[scheduler] contracts
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
            webhook_target("https://example.com/hook"),
        );
        config.validate().unwrap();

        // `http://` needs the explicit operator opt-in.
        let mut target = Config::default();
        target.webhooks.targets.insert(
            "ops".to_string(),
            webhook_target("http://127.0.0.1:9999/hook"),
        );
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
            "[webhooks.targets.ops]\nurl = \"https://example.com/hook\"\nkind = \"teams\"\n",
        )
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
