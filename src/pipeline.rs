//! The single-URL service: fetch, extract, persist and shape the tool payload
//! (IMP-B01 data flow for `fetch_page`).
//!
//! Order is fixed: validate -> cache (freshness) -> robots -> gate -> fetch ->
//! CAS -> decode -> extract -> persist. The safety gate is consulted before the
//! first byte leaves the process on every path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use scraper::Html;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::compare::{self, ComparePlan, SnapshotCompareInput, SnapshotComparePayload};
use crate::config::Config;
use crate::core::error::{Result, WidowError};
use crate::core::truncate::truncate_chars;

use crate::core::url::{PageUrl, content_hash};
use crate::export::{self, CorpusExportInput, CorpusExportPayload, ExportPlan};
use crate::extract::{self, ExtractOptions, Link, Mode, spec, spec_eval};
use crate::feed::{self, FeedInput, FeedPayload};
use crate::fetch::charset;
use crate::fetch::content;
use crate::fetch::content::ContentScope;
use crate::fetch::gate::{HostGate, HostLease};
use crate::fetch::http::HopGuard;
use crate::fetch::http::{
    ACCEPT_PAGE, ACCEPT_TABULAR, Conditional, FetchRequest, HttpClient, RawResponse,
};
use crate::fetch::robots::{RobotsHopGuard, RobotsService};
use crate::fetch::ssrf::SsrfGate;
use crate::pdf::{self, PdfExtractInput, PdfExtractPayload};
use crate::recipes::{
    self, RecipeApplyInput, RecipeListInput, RecipeListPayload, RecipeSaveInput, RecipeSavePayload,
    RecipeSummary,
};
use crate::render::{
    self, CaptureOptions, CapturedPage, HeldSession, RenderMode, RenderedPage, Renderer,
    SessionGate, SessionOptions,
};
use crate::rows::{self, RowFieldError, RowsSpec};
use crate::search::parse::{Engine, ParseOutcome};
use crate::search::{self, SearchInput, SearchPayload, SearchResult};
use crate::sitemap::{SitemapInput, SitemapPayload};
use crate::store::Store;
use crate::store::db::{BlobRow, ExtractionRow, FetchRecord, IndexText, ProfileRow};
use crate::store::recipes::NewRecipe;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderPolicy {
    /// Render only when the static result is thin (B03); static-only in B01.
    Auto,
    /// Never render.
    Never,
    /// Always render; unavailable in the static-only build.
    Always,
}

/// `fetch_page` input (IMP-CONTRACT-001). Every field except `url` has a
/// documented default; there is deliberately no capability-widening argument.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct FetchPageInput {
    /// Absolute http(s) URL to fetch.
    pub url: String,
    /// `main` (readability, default), `full`, `text` or `raw`.
    #[serde(default)]
    #[schemars(
        description = "main = readability (default), full = whole body, text = plain text, raw = untrusted HTML"
    )]
    pub mode: Option<Mode>,
    /// `auto` (default) renders only when the thin-content heuristic fires;
    /// `never` stays static; `always` requires rendering and fails rather than
    /// silently returning a static result.
    #[serde(default)]
    pub render: Option<RenderPolicy>,
    /// `0` (default) revalidates with the origin; `>0` may serve a cached
    /// extraction younger than this many seconds without touching the network.
    #[serde(default)]
    pub freshness_s: Option<u64>,
    /// Character cap for the returned Markdown; cannot exceed the server cap.
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// Include discovered links (URLs only, never their content).
    #[serde(default)]
    pub include_links: Option<bool>,
    /// Include the delivered body in `structuredContent.markdown` (default true).
    /// `false` returns metadata only on both channels.
    #[serde(default)]
    pub include_content: Option<bool>,
}

/// `fetch_page` machine payload (IMP-CONTRACT-001 output).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FetchPagePayload {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub title: Option<String>,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub published: Option<String>,
    pub lang: Option<String>,
    pub markdown_chars: usize,
    /// The delivered body (the Markdown the text block carries, without the
    /// front matter) when `include_content` was true; absent otherwise. The
    /// cache stores this as `None` and reconstructs it from the stored body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    pub truncated: bool,
    /// The limit that fired, when `truncated` is true.
    pub limit_hit: Option<String>,
    pub render_mode: String,
    /// Present only when `render = auto` asked for rendering but it could not be
    /// delivered; the static result is then explicitly `static_fallback`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_warning: Option<String>,
    pub cached: bool,
    pub fetched_at: String,
    pub duration_ms: u64,
    /// Always a list (IMP-CONTRACT-002 §4.1): `[]` when links were not requested.
    /// A pre-M02 cached payload stored `null`; the custom deserializer maps it to
    /// an empty list.
    #[serde(default, deserialize_with = "deserialize_nullable_links")]
    pub links: Vec<Link>,
    /// Whether *this* call asked for links. Re-derived on every call, including
    /// the cached path, so a cached answer never claims links nobody asked for.
    #[serde(default)]
    pub links_requested: bool,
    pub links_truncated: bool,
    /// `ok | thin` on rendered results only (IMP-CONTRACT-002 §4.3); absent for
    /// static and static_fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_quality: Option<String>,
    /// `interstitial_suspect` when a 200 response redirected into a
    /// verification/login path segment and delivered fewer than 50 words
    /// (IMP-DEC-025). A warning, never an error; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_warning: Option<String>,
    pub word_count: usize,
}

/// What the MCP layer needs: the payload plus the Markdown that goes in the text
/// content block. Both are stored, so a cached answer is identical in shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchPageResult {
    pub payload: FetchPagePayload,
    pub markdown: String,
}

/// `screenshot` input (IMP-DEC-032). The visual capture profile is
/// server-fixed; no argument selects it.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ScreenshotInput {
    /// Absolute http(s) URL to render and capture.
    pub url: String,
    /// Capture the scrollable content size (default `false`: the viewport).
    #[serde(default)]
    pub full_page: Option<bool>,
    /// Extra quiet time after the normal settle, 0..=10 000 ms. Never a
    /// navigation: the page has already loaded and settled.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// `screenshot` output. The PNG stays in the content-addressed store: the
/// payload carries its hash, size and dimensions, never base64 bytes.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScreenshotPayload {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    /// BLAKE3-256 of the PNG bytes; also the CAS path key.
    pub hash: String,
    pub bytes: u64,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `content` (`full_page`), `viewport`, or `unknown` when metrics failed.
    pub dimensions_source: String,
    pub full_page: bool,
    pub captured_at: String,
    pub render_mode: String,
    pub render_quality: String,
    pub truncated: bool,
    /// `visual_max_bytes`, `visual_max_requests`, `ceiling`, or null.
    pub limit_hit: Option<String>,
    /// Requests the capture profile refused (`media`, `file:`); informational.
    pub failed_resources: u32,
}

/// One stored screenshot reference: the `screenshots.id`, its CAS hash, the
/// PNG size and the timestamp both rows carry.
#[derive(Debug, Clone)]
pub(crate) struct StoredCapture {
    pub id: i64,
    pub hash: String,
    pub bytes: u64,
    pub captured_at: chrono::DateTime<Utc>,
}

/// `blob_get` input: a content-addressed hash, never a caller path.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct BlobGetInput {
    /// 64 lowercase/uppercase hex characters (the `screenshot.hash`).
    pub hash: String,
}

/// `blob_get` output. The file path is the server's CAS path; the caller reads
/// the bytes with its own tools, so no payload grows by a media blob.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobGetPayload {
    pub hash: String,
    pub size: i64,
    pub mime: Option<String>,
    /// Absolute CAS path (never derived from caller input).
    pub path: String,
    pub created_at: String,
    /// Bounded context list: `screenshot:<id>`, `page:<id>`, ...
    pub referenced_by: Vec<String>,
}

/// `extract` input (IMP-CONTRACT-001 §2). The spec is data: it is validated
/// server-side and can never execute code.
#[derive(Debug, Clone, schemars::JsonSchema)]
pub struct ExtractInput {
    /// Absolute http(s) URL to fetch and extract from.
    pub url: String,
    /// Declarative spec: optional `container` plus `fields` (selector, attr,
    /// all, transform). Unknown keys and transforms are rejected.
    pub schema: serde_json::Value,
    /// `auto` (default) escalates thin, script-driven pages; `never` stays
    /// static; `always` requires rendering.
    #[serde(default)]
    pub render: Option<RenderPolicy>,
    /// Prefer a stored extraction for this page+spec (default `false`).
    #[serde(default)]
    pub use_cache: Option<bool>,
    /// Per-string-value cap for the returned records (clamped to `[extract]
    /// max_field_chars`). Presentation only: the cache keeps full values.
    #[serde(default)]
    pub max_field_chars: Option<usize>,
    /// Total character budget over the returned records (clamped to
    /// `[extract] max_chars`). Presentation only.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Wire shape used only by `ExtractInput::deserialize`; the `flatten` map lets
/// the custom deserializer recognise a spec passed one level too high.
#[derive(Deserialize)]
struct ExtractInputFields {
    url: String,
    #[serde(default)]
    schema: Option<serde_json::Value>,
    #[serde(default)]
    render: Option<RenderPolicy>,
    #[serde(default)]
    use_cache: Option<bool>,
    #[serde(default)]
    max_field_chars: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(flatten)]
    top_level: serde_json::Map<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for ExtractInput {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = ExtractInputFields::deserialize(deserializer)?;
        let schema = match fields.schema {
            Some(schema) => schema,
            None => {
                // ISS-007: a caller who passes `{url, container, fields}` gets a
                // message naming the envelope instead of bare serde wording. The
                // shape is still rejected: there is one documented contract.
                if fields.top_level.contains_key("container")
                    || fields.top_level.contains_key("fields")
                {
                    return Err(serde::de::Error::custom(
                        "extract expects the extraction spec under `schema`: \
                         {url, schema: {container?, fields}}",
                    ));
                }
                return Err(serde::de::Error::missing_field("schema"));
            }
        };
        Ok(Self {
            url: fields.url,
            schema,
            render: fields.render,
            use_cache: fields.use_cache,
            max_field_chars: fields.max_field_chars,
            max_chars: fields.max_chars,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtractPayload {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub records: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Total records the spec matched, before the `max_records` cap.
    pub record_count: usize,
    pub field_errors: Vec<spec_eval::FieldError>,
    pub truncated: bool,
    /// The limit that fired, when `truncated` is true.
    pub limit_hit: Option<String>,
    /// String values cut by `max_field_chars` in the returned records.
    pub truncated_fields: usize,
    pub render_mode: String,
    /// `ok | thin` on rendered results only (ISS-005); absent for static and
    /// static_fallback. An approximation from the delivered page's visible text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_quality: Option<String>,
    /// Present only when `render = auto` fell back to the static result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_warning: Option<String>,
    pub cached: bool,
    pub fetched_at: String,
    pub duration_ms: u64,
}

/// `extract_rows` input (IMP-DEC-033). A sibling of `extract`, never a mode of
/// it: tabular input is a different data family with its own spec identity.
/// `format: auto` resolves from the response media type, then from a bounded
/// sniff of the body; `application/octet-stream` is refused, not guessed.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExtractRowsInput {
    /// Absolute http(s) URL to fetch.
    pub url: String,
    /// `auto` (default), `csv`, `tsv`, `json` or `html_table`.
    #[serde(default)]
    #[schemars(
        description = "auto = response media type then a bounded sniff; csv, tsv, json, html_table"
    )]
    pub format: Option<String>,
    /// CSS selector picking the table (`html_table` only; first `<table>` when omitted).
    #[serde(default)]
    pub selector: Option<String>,
    /// CSV separator: one of `,` `;` `\\t` `|`; omit to sniff the header line.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// Treat the first row as the header (default true).
    #[serde(default)]
    pub header_row: Option<bool>,
    /// RFC6901 pointer to the JSON record set; default "" (document root).
    #[serde(default)]
    pub json_pointer: Option<String>,
    /// Records returned, 1..=1000 (default 200).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Per-string-value cap, 1..=20000 (default 8000).
    #[serde(default)]
    pub max_field_chars: Option<usize>,
    /// Render policy for HTML tables: auto (default), never, always.
    #[serde(default)]
    pub render: Option<RenderPolicy>,
    /// Prefer a stored extraction for this URL and spec (default false).
    #[serde(default)]
    pub use_cache: Option<bool>,
}

/// `extract_rows` machine payload. Same envelope conventions as `extract`; the
/// record family, the resolved `format` and `row_count`/`returned` are the
/// tabular additions.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtractRowsPayload {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    /// The resolved format (`csv`, `tsv`, `json` or `html_table`), never `auto`.
    pub format: String,
    pub records: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Rows parsed, stopping at `limit + 1` when more existed.
    pub row_count: usize,
    pub returned: usize,
    pub truncated: bool,
    /// The limit that fired, when `truncated` is true (`limit` or
    /// `max_field_chars`).
    pub limit_hit: Option<String>,
    /// Bounded (20) row anomalies: ragged rows, a missing table, ignored spans.
    pub field_errors: Vec<RowFieldError>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub field_errors_truncated: bool,
    /// String values cut by `max_field_chars` in the returned records.
    pub truncated_fields: usize,
    pub fetched_at: String,
    pub render_mode: String,
    pub cached: bool,
    pub duration_ms: u64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A stored recipe version as the wire `u32`; the column is `i64` with a
/// `>= 1` CHECK, so anything else is a corrupt row, not caller input.
fn recipe_version(value: i64) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| WidowError::internal(format!("stored recipe version {value} is out of range")))
}

/// `recipe_apply` payload: the ordinary extraction envelope plus the recipe
/// identity that produced it (IMP-DEC-034). Flattened, so the JSON shape is
/// exactly `extract`'s fields with `recipe`, `version` and `profile_hash`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecipeApplyPayload {
    #[serde(flatten)]
    pub extraction: ExtractPayload,
    pub recipe: String,
    pub version: u32,
    pub profile_hash: String,
}

enum StaticSource {
    NotModified(RawResponse),
    Document(FetchedDocument),
}

/// Outcome of one SERP attempt. A route refusal is final; a transport or parse
/// failure may try the fallback endpoint once.
enum SearchAttempt {
    Parsed {
        response: Box<RawResponse>,
        outcome: ParseOutcome,
    },
    Retryable(WidowError),
    Fatal(WidowError),
}

/// A schema pinned to a crawl job: the validated spec (hash + canonical JSON for
/// the `profiles` row) and its compiled selectors (so a page never re-parses CSS).
#[derive(Debug, Clone, Copy)]
pub struct CrawlSpec<'a> {
    pub validated: &'a spec::ValidatedSpec,
    pub compiled: &'a spec::CompiledSpec,
}

/// One page as the crawl worker sees it.
#[derive(Debug, Clone)]
pub struct CrawlPage {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub title: Option<String>,
    pub raw_hash: String,
    /// BLAKE3 of the delivered Markdown (`pages.text_hash`).
    pub text_hash: String,
    /// Near-duplicate identity (link targets removed); the trap-collapse key.
    pub content_key: String,
    pub links: Vec<Link>,
    pub links_truncated: bool,
    pub render_mode: String,
    pub records: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    pub record_count: usize,
    pub field_errors: usize,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// One fetched sitemap body (internal to [`Pipeline::sitemap`]).
#[derive(Debug, Clone)]
struct FetchedSitemap {
    final_url: String,
    status: u16,
    parsed: crate::sitemap::ParsedSitemap,
}

/// A schema pinned to a monitor check: the validated profile (its versioned
/// hash is stored on the monitor) and the compiled selectors.
#[derive(Debug, Clone, Copy)]
pub struct MonitorSpec<'a> {
    pub validated: &'a spec::ValidatedSpec,
    pub compiled: &'a spec::CompiledSpec,
}

/// One monitor check as the pipeline saw it. `not_modified` means the origin
/// answered 304 and the caller may reuse the previous snapshot's hashes.
#[derive(Debug, Clone)]
pub struct MonitorPage {
    pub final_url: String,
    pub status: u16,
    /// The fetch event for this check (request-level truth for the snapshot).
    pub fetch_event_id: i64,
    pub raw_hash: String,
    pub render_mode: String,
    pub not_modified: bool,
    pub markdown: String,
    pub records: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    pub records_json: Option<String>,
    pub field_errors: usize,
}

/// A fetched body after scope, decode and the content-addressed write.
struct FetchedDocument {
    response: RawResponse,
    media: content::MediaKind,
    decoded: charset::Decoded,
    raw_hash: String,
    render_mode: RenderMode,
    render_warning: Option<String>,
}

#[derive(Debug)]
pub struct Pipeline {
    config: Arc<Config>,
    http: Arc<HttpClient>,
    store: Arc<Store>,
    /// robots.txt enforcement for page fetches, applied to every hop.
    hop_guard: Arc<dyn HopGuard>,
    /// The top-level URL a render is given must pass the same SSRF gate as a
    /// static fetch. Subresources a rendered page loads are outside this gate
    /// (IMP-RISK-004, disclosed residual risk).
    ssrf: Arc<SsrfGate>,
    /// Politeness lease for the top-level render navigation.
    gate: Arc<HostGate>,
    renderer: Arc<dyn Renderer>,
}

impl Pipeline {
    pub fn new(
        config: Arc<Config>,
        http: Arc<HttpClient>,
        ssrf: Arc<SsrfGate>,
        gate: Arc<HostGate>,
        robots: Arc<RobotsService>,
        store: Arc<Store>,
        renderer: Arc<dyn Renderer>,
    ) -> Self {
        let hop_guard: Arc<dyn HopGuard> = Arc::new(RobotsHopGuard::new(robots));
        Self {
            config,
            http,
            store,
            hop_guard,
            ssrf,
            gate,
            renderer,
        }
    }

    /// Conditional state from the last successful fetch of this URL.
    async fn conditional_for(&self, page: &PageUrl) -> Result<Conditional> {
        let existing = self.store.db.page(page.hash()).await?;
        Ok(Conditional {
            etag: existing.as_ref().and_then(|page| page.etag.clone()),
            last_modified: existing
                .as_ref()
                .and_then(|page| page.last_modified.clone()),
        })
    }

    /// One request through the shared adapter. The SSRF gate and the robots
    /// guard run per hop inside it, so every tool gets the same safeguards.
    async fn fetch_raw(
        &self,
        page: &PageUrl,
        conditional: Conditional,
        label: &str,
        started: Instant,
        min_interval: Option<Duration>,
        scope: ContentScope,
    ) -> Result<RawResponse> {
        // The tabular family names CSV/TSV before `*/*`; every page path keeps
        // the HTML-first page header.
        let accept = match scope {
            ContentScope::Page => ACCEPT_PAGE,
            ContentScope::Tabular => ACCEPT_TABULAR,
        };
        match self
            .http
            .fetch(FetchRequest {
                url: page.url().clone(),
                conditional,
                accept,
                max_body_bytes: self.config.fetch.max_body_bytes,
                hop_guard: Some(Arc::clone(&self.hop_guard)),
                min_interval,
                scope,
            })
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => {
                self.record_failure(page, label, started.elapsed(), &error)
                    .await;
                Err(error)
            }
        }
    }

    /// Content scope, decode and the content-addressed write for a fetched body.
    async fn finish_document(
        &self,
        page: &PageUrl,
        label: &str,
        started: Instant,
        response: RawResponse,
        scope: ContentScope,
    ) -> Result<FetchedDocument> {
        let media = match scope.classify(response.content_type.as_deref()) {
            Some(media) => media,
            // A bodiless success carries no content to scope.
            None if response.body.is_empty() && matches!(response.status, 204 | 205) => {
                content::MediaKind::Text
            }
            None => {
                let error = WidowError::UnsupportedContentType {
                    content_type: response
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "(none)".to_string()),
                    processed: match scope {
                        ContentScope::Page => {
                            content::processed_hint(response.content_type.as_deref()).to_string()
                        }
                        ContentScope::Tabular => content::tabular_processed_types(),
                    },
                };
                self.record_failure(page, label, started.elapsed(), &error)
                    .await;
                return Err(error);
            }
        };
        let decoded = charset::decode(&response.body, response.content_type.as_deref());
        let raw_hash = match self.store.put_blob(&response.body).await {
            Ok(hash) => hash,
            Err(error) => {
                self.record_failure(page, label, started.elapsed(), &error)
                    .await;
                return Err(error);
            }
        };
        Ok(FetchedDocument {
            response,
            media,
            decoded,
            raw_hash,
            render_mode: RenderMode::Static,
            render_warning: None,
        })
    }

    /// Feed rendered HTML through the same decode/blob path as a fetched body.
    /// The original response media type is recorded for logs, but the DOM is
    /// HTML by construction, so downstream extraction stays single-path.
    async fn finish_rendered(
        &self,
        page: &PageUrl,
        label: &str,
        started: Instant,
        rendered: RenderedPage,
    ) -> Result<FetchedDocument> {
        // Static fetch parity: the same content scope and HTTP error taxonomy
        // apply before a rendered DOM can enter the extraction pipeline.
        if let Some(content_type) = rendered.content_type.as_deref()
            && content::classify(Some(content_type)).is_none()
        {
            let error = WidowError::UnsupportedContentType {
                content_type: content_type.to_string(),
                processed: content::PROCESSED_TYPES.to_string(),
            };
            self.record_failure(page, label, started.elapsed(), &error)
                .await;
            return Err(error);
        }
        if !(200..300).contains(&rendered.status) {
            let error = WidowError::Http {
                status: Some(rendered.status),
                detail: format!(
                    "origin returned HTTP {} for the rendered navigation",
                    rendered.status
                ),
            };
            self.record_failure(page, label, started.elapsed(), &error)
                .await;
            return Err(error);
        }

        let body = rendered.html.into_bytes();
        let bytes = body.len() as u64;
        tracing::debug!(
            target = %page.url(),
            delegated_content_type = rendered.content_type.as_deref().unwrap_or("(none)"),
            rendered_status = rendered.status,
            "treating the serialized DOM as text/html for extraction"
        );
        let response = RawResponse {
            requested_url: page.url().clone(),
            final_url: rendered.final_url,
            status: rendered.status,
            content_type: Some("text/html".to_string()),
            etag: None,
            last_modified: None,
            cache_control_max_age: None,
            body,
            bytes,
            redirects: 0,
            duration: rendered.duration,
        };
        let media = content::classify(Some("text/html")).unwrap_or(content::MediaKind::Text);
        let decoded = charset::decode(&response.body, Some("text/html"));
        let raw_hash = match self.store.put_blob(&response.body).await {
            Ok(hash) => hash,
            Err(error) => {
                self.record_failure(page, label, started.elapsed(), &error)
                    .await;
                return Err(error);
            }
        };
        Ok(FetchedDocument {
            response,
            media,
            decoded,
            raw_hash,
            render_mode: if rendered.partial {
                RenderMode::RenderedPartial
            } else {
                RenderMode::Rendered
            },
            render_warning: None,
        })
    }

    /// The top-level URL is vetted and robots-checked before the browser is
    /// asked to navigate. A rendered page's own subresources are a disclosed
    /// gap (IMP-RISK-004); this does not claim they pass the gate.
    async fn render_page(
        &self,
        page: &PageUrl,
        min_interval: Option<Duration>,
    ) -> Result<RenderedPage> {
        let _lease = self.vet_render_target(page, min_interval).await?;
        self.renderer.render(page.url()).await
    }

    /// The one top-level render gate: SSRF vet, render preflight (before any
    /// robots request), robots check and the politeness lease. Both the text
    /// and the visual render paths use it so they cannot drift (IMP-DEC-032).
    async fn vet_render_target(
        &self,
        page: &PageUrl,
        min_interval: Option<Duration>,
    ) -> Result<HostLease> {
        self.ssrf.vet(page.url()).await?;
        // Fail before a robots.txt request when the browser is absent.
        self.renderer.preflight()?;
        let delay = self.hop_guard.check(page.url()).await?;
        let delay = match (delay, min_interval) {
            (Some(robots), Some(policy)) => Some(robots.max(policy)),
            (delay, policy) => delay.or(policy),
        };
        let host = page.url().host_str().unwrap_or_default().to_string();
        Ok(self.gate.acquire(&host, delay).await)
    }

    /// Capture through the same gate/robots/caps/settle path as a render.
    /// Visual subresources are a disclosed residual (IMP-RISK-004): the
    /// top-level document is vetted; the browser's own asset requests are not.
    async fn capture_page(
        &self,
        page: &PageUrl,
        options: CaptureOptions,
        min_interval: Option<Duration>,
    ) -> Result<CapturedPage> {
        let _lease = self.vet_render_target(page, min_interval).await?;
        self.renderer.capture(page.url(), options).await
    }

    /// The gates a held session checks every request against (SSRF for every
    /// request; robots + a host lease for documents). Clones of the pipeline's
    /// own services, so a session cannot drift from the render path.
    pub(crate) fn interaction_gate(&self) -> SessionGate {
        SessionGate {
            ssrf: Arc::clone(&self.ssrf),
            hop_guard: Arc::clone(&self.hop_guard),
            host_gate: Arc::clone(&self.gate),
        }
    }

    /// Open one held page after the same top-level vetting as a render
    /// (SSRF, preflight, robots, host lease). The session then installs its own
    /// CDP Fetch handler as defense in depth for later navigations.
    pub(crate) async fn open_interaction_session(
        &self,
        page: &PageUrl,
        viewport: (u32, u32),
    ) -> Result<Box<dyn HeldSession>> {
        {
            // The pre-vet lease covers only the check; the session's own
            // document request acquires a separate lease inside the browser.
            let _lease = self.vet_render_target(page, None).await?;
        }
        self.renderer
            .open_session(
                page.url(),
                SessionOptions {
                    viewport_width: viewport.0,
                    viewport_height: viewport.1,
                    gate: self.interaction_gate(),
                    settle_ms: self.config.render.settle_ms,
                },
            )
            .await
    }

    /// `screenshot`: render, capture PNG, store it in the CAS and reference it
    /// from one `screenshots` row (IMP-DEC-032). The blob row is written before
    /// the screenshot row in the same writer closure: the `screenshots.hash`
    /// foreign key requires that order.
    pub async fn screenshot(&self, input: ScreenshotInput) -> Result<ScreenshotPayload> {
        let page = PageUrl::parse(&input.url)?;
        let full_page = input.full_page.unwrap_or(false);
        let wait_ms = input.wait_ms.unwrap_or(0);
        if wait_ms > 10_000 {
            return Err(WidowError::SchemaInvalid {
                pointer: "/wait_ms".to_string(),
                message: "wait_ms must be 0..=10000".to_string(),
            });
        }

        let captured = self
            .capture_page(&page, CaptureOptions { full_page, wait_ms }, None)
            .await?;
        if !(200..300).contains(&captured.page.status) {
            return Err(WidowError::Http {
                status: Some(captured.page.status),
                detail: format!(
                    "origin returned HTTP {} for the screenshot navigation",
                    captured.page.status
                ),
            });
        }

        let stored = self
            .store_capture(
                Some(&page),
                &captured.png,
                captured.width,
                captured.height,
                "screenshot",
            )
            .await?;
        let captured_at = stored.captured_at;
        let hash = stored.hash.clone();
        let size = stored.bytes;
        let screenshot_id = stored.id;

        let mode = if captured.page.partial {
            RenderMode::RenderedPartial
        } else {
            RenderMode::Rendered
        };
        let limit_hit = if captured.page.partial {
            Some(
                match captured.page.stop_reason.as_deref() {
                    Some("visual_max_bytes") => "visual_max_bytes",
                    Some("visual_max_requests") => "visual_max_requests",
                    _ => "ceiling",
                }
                .to_string(),
            )
        } else {
            None
        };
        let render_quality = render_quality_for(
            mode,
            render::auto::rough_visible_word_count(&captured.page.html),
        )
        .unwrap_or_else(|| "ok".to_string());
        tracing::info!(
            screenshot_id,
            hash = %hash,
            bytes = size,
            width = ?captured.width,
            height = ?captured.height,
            dimensions_source = captured.dimensions_source.as_str(),
            full_page,
            wait_ms,
            failed_resources = captured.failed_resources,
            "screenshot captured"
        );

        Ok(ScreenshotPayload {
            url: page.raw().to_string(),
            final_url: captured.page.final_url.to_string(),
            status: captured.page.status,
            hash,
            bytes: size,
            mime: "image/png".to_string(),
            width: captured.width,
            height: captured.height,
            dimensions_source: captured.dimensions_source.as_str().to_string(),
            full_page,
            captured_at: captured_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            render_mode: mode.as_str().to_string(),
            render_quality,
            truncated: captured.page.partial,
            limit_hit,
            failed_resources: captured.failed_resources,
        })
    }

    /// Store one captured PNG and its reference row: the `blobs` row first,
    /// then the `screenshots` row in the same writer closure (the FK requires
    /// that order). Shared by `screenshot` (`source` `screenshot`) and the
    /// interaction session (`source` `interaction`). The size cap is checked
    /// **before** the CAS write, so an over-size capture leaves nothing behind.
    pub(crate) async fn store_capture(
        &self,
        page: Option<&PageUrl>,
        png: &[u8],
        width: Option<u32>,
        height: Option<u32>,
        source: &str,
    ) -> Result<StoredCapture> {
        let bytes = png.len() as u64;
        let limit = self.config.render.max_screenshot_bytes;
        if bytes > limit {
            return Err(WidowError::TooLarge { limit });
        }
        let captured_at = Utc::now();
        let hash = self.store.put_blob(png).await?;
        let size = png.len().min(i64::MAX as usize) as i64;
        let page_id = match page {
            Some(page) => self.store.db.page(page.hash()).await?.map(|row| row.id),
            None => None,
        };
        let width = width.map(i64::from);
        let height = height.map(i64::from);
        let source = source.to_string();
        let hash_for_write = hash.clone();
        let id = self
            .store
            .db
            .write_with(move |connection| {
                crate::store::db::save_blob(
                    connection,
                    &hash_for_write,
                    size,
                    Some("image/png"),
                    captured_at,
                )?;
                crate::store::db::save_screenshot(
                    connection,
                    page_id,
                    &hash_for_write,
                    "png",
                    width,
                    height,
                    &source,
                    captured_at,
                )
            })
            .await?;
        Ok(StoredCapture {
            id,
            hash,
            bytes,
            captured_at,
        })
    }

    /// `blob_get`: metadata and the server CAS path for one stored blob. The
    /// hash is validated and can only address the CAS; unknown or missing
    /// content is `not_found`.
    pub async fn blob_get(&self, input: BlobGetInput) -> Result<BlobGetPayload> {
        // Hashes are stored lowercase; accept an uppercase spelling by
        // normalizing first, so one blob has one addressable spelling.
        let hash = input.hash.trim().to_ascii_lowercase();
        if !crate::store::blob::is_valid_hash(&hash) {
            return Err(WidowError::NotFound(
                "no blob with that hash (64 hex characters required)".to_string(),
            ));
        }
        let row = self
            .store
            .db
            .blob(&hash)
            .await?
            .ok_or_else(|| WidowError::NotFound("no blob row with that hash".to_string()))?;
        let path = self
            .store
            .blobs
            .path_for(&hash)
            .ok_or_else(|| WidowError::NotFound("no blob path for that hash".to_string()))?;
        let absolute = match tokio::fs::canonicalize(&path).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WidowError::NotFound(
                    "blob bytes are missing for that hash".to_string(),
                ));
            }
            Err(error) => {
                return Err(WidowError::Internal(format!(
                    "cannot resolve the blob path: {error}"
                )));
            }
        };
        let referenced_by = self.store.db.blob_references(&hash, 20).await?;
        Ok(BlobGetPayload {
            hash,
            size: row.size,
            mime: row.content_type,
            path: absolute.display().to_string(),
            created_at: row
                .created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            referenced_by,
        })
    }

    /// Static acquisition: conditional GET, the 304 short-circuit and the
    /// decoded+CAS-stored document. Shared by `fetch_page`, `extract`, the crawl
    /// worker and `extract_rows` so the gate/robots/caps path cannot drift; only
    /// the media `scope` differs.
    async fn acquire_static(
        &self,
        page: &PageUrl,
        label: &str,
        started: Instant,
        conditional: Conditional,
        min_interval: Option<Duration>,
        scope: ContentScope,
    ) -> Result<StaticSource> {
        let response = self
            .fetch_raw(page, conditional, label, started, min_interval, scope)
            .await?;
        if response.status == 304 {
            return Ok(StaticSource::NotModified(response));
        }
        Ok(StaticSource::Document(
            self.finish_document(page, label, started, response, scope)
                .await?,
        ))
    }

    /// Render the page and store the serialized DOM through the same scope and
    /// CAS path (`finish_rendered`).
    async fn acquire_rendered(
        &self,
        page: &PageUrl,
        label: &str,
        started: Instant,
        min_interval: Option<Duration>,
    ) -> Result<FetchedDocument> {
        let rendered = self.render_page(page, min_interval).await?;
        self.finish_rendered(page, label, started, rendered).await
    }

    /// Index text for one page: the delivered body capped at
    /// `[fts].max_index_chars` on a character boundary. The single place every
    /// acquisition path applies that cap, so the store never sees an unbounded
    /// body (IMP-RISK-040).
    fn index_text(&self, body: &str) -> String {
        truncate_chars(body, self.config.fts.max_index_chars).0
    }

    /// The `render=auto` heuristic over a static document. One implementation so
    /// `fetch_page`, `extract` and the crawl worker cannot disagree about what
    /// counts as thin.
    fn auto_wants_render(
        &self,
        page: &PageUrl,
        document: &FetchedDocument,
        main_words: usize,
    ) -> bool {
        let signals = render::auto::signals_from_html(&document.decoded.text, main_words);
        let decision = render::auto::decide(&signals);
        tracing::info!(
            rule = decision.rule,
            main_words,
            script_count = signals.script_count,
            app_root = signals.app_root,
            noscript = signals.noscript,
            javascript_required = signals.javascript_required,
            render = decision.render,
            url = %log_target(page, self.config.logging.redact_query),
            "render=auto decision"
        );
        decision.render
    }

    pub async fn fetch_page(&self, input: FetchPageInput) -> Result<FetchPageResult> {
        let started = Instant::now();
        let mode = match input.mode {
            Some(mode) => mode,
            None => Mode::parse(&self.config.extract.default_mode)
                .map_err(|_| WidowError::internal("invalid extract.default_mode"))?,
        };
        let policy = input.render.unwrap_or(RenderPolicy::Auto);

        let max_chars = input
            .max_chars
            .unwrap_or(self.config.extract.max_chars)
            .min(self.config.extract.max_chars);
        let include_links = input.include_links.unwrap_or(false);
        let include_content = input.include_content.unwrap_or(true);
        let freshness_s = input.freshness_s.unwrap_or(0);
        let page = PageUrl::parse(&input.url)?;
        let profile_hash = profile_hash_for(mode, max_chars, include_links);

        // 1. Freshness window: no network at all. `always` may satisfy the
        // freshness contract only from a stored *rendered* result.
        if freshness_s > 0
            && let Some(cached) = self
                .store
                .db
                .cached_extraction(page.hash(), &profile_hash)
                .await?
        {
            let threshold =
                Utc::now() - chrono::Duration::seconds(freshness_s.min(i64::MAX as u64) as i64);
            if cached.created_at >= threshold
                && let Ok(stored) = serde_json::from_str::<FetchPageResult>(&cached.result_json)
                && cached_mode_acceptable(policy, &stored.payload.render_mode)
            {
                let mut result = stored;
                result.payload.cached = true;
                result.payload.content_warning = cached_content_warning(&result.payload);
                result.payload.links_requested = include_links;
                result.payload.markdown = if include_content {
                    Some(result.markdown.clone())
                } else {
                    None
                };
                if !include_links {
                    result.payload.links.clear();
                }
                result.payload.duration_ms =
                    started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(result);
            }
        }

        let mut source = if policy == RenderPolicy::Always {
            // `always` is a guarantee: no static attempt, no silent downgrade.
            self.acquire_rendered(&page, mode.as_str(), started, None)
                .await?
        } else {
            let conditional = self.conditional_for(&page).await?;
            match self
                .acquire_static(
                    &page,
                    mode.as_str(),
                    started,
                    conditional,
                    None,
                    ContentScope::Page,
                )
                .await?
            {
                StaticSource::Document(document) => document,
                StaticSource::NotModified(response) => {
                    let duration = started.elapsed();
                    self.record_not_modified(&page, mode.as_str(), &response, duration)
                        .await?;
                    if let Some(cached) = self
                        .store
                        .db
                        .cached_extraction(page.hash(), &profile_hash)
                        .await?
                        && let Ok(mut result) =
                            serde_json::from_str::<FetchPageResult>(&cached.result_json)
                        && cached_mode_acceptable(policy, &result.payload.render_mode)
                    {
                        // Re-derive against the stored 200 before the 304 marker
                        // replaces it, so a pre-upgrade cache still carries the
                        // warning on a revalidated answer.
                        result.payload.content_warning = cached_content_warning(&result.payload);
                        result.payload.status = 304;
                        result.payload.cached = true;
                        result.payload.links_requested = include_links;
                        result.payload.markdown = if include_content {
                            Some(result.markdown.clone())
                        } else {
                            None
                        };
                        if !include_links {
                            result.payload.links.clear();
                        }
                        result.payload.duration_ms =
                            duration.as_millis().min(u64::MAX as u128) as u64;
                        return Ok(result);
                    }
                    // Nothing cached for this shape: fetch the body we know is unchanged.
                    match self
                        .acquire_static(
                            &page,
                            mode.as_str(),
                            started,
                            Conditional::default(),
                            None,
                            ContentScope::Page,
                        )
                        .await?
                    {
                        StaticSource::Document(document) => document,
                        StaticSource::NotModified(_) => {
                            return Err(WidowError::internal(
                                "origin answered 304 to an unconditional request",
                            ));
                        }
                    }
                }
            }
        };

        let options = ExtractOptions {
            mode,
            max_chars,
            max_links: self.config.extract.max_links,
            include_links,
            ignore_selectors: Vec::new(),
        };
        let mut extraction = extraction_for(&source, &options)?;
        let mut render_warning: Option<String> = None;

        // 7. `auto`: decide after the static result exists, and fall back
        // visibly when rendering cannot be delivered.
        if policy == RenderPolicy::Auto
            && source.render_mode == RenderMode::Static
            && self.auto_wants_render(&page, &source, extraction.word_count)
        {
            match self
                .acquire_rendered(&page, mode.as_str(), started, None)
                .await
            {
                Ok(rendered) => match extraction_for(&rendered, &options) {
                    Ok(rendered_extraction) => {
                        source = rendered;
                        extraction = rendered_extraction;
                    }
                    Err(error) => {
                        render_warning = Some(format!(
                            "rendered DOM could not be extracted ({error}); static result kept"
                        ));
                    }
                },
                Err(error) => {
                    tracing::warn!(error = %error, "render=auto fallback to static result");
                    render_warning = Some(error.to_string());
                }
            }
        }

        if render_warning.is_some() {
            source.render_mode = RenderMode::StaticFallback;
            source.render_warning = render_warning;
        }

        let fetched_at = Utc::now();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let payload = FetchPagePayload {
            url: page.raw().to_string(),
            final_url: source.response.final_url.to_string(),
            status: source.response.status,
            // Contract shape: the media type without parameters. The full
            // observed header (with charset) is kept in `pages.content_type`.
            content_type: source
                .response
                .content_type
                .as_deref()
                .map(content::base_media_type)
                .unwrap_or_else(|| source.media.as_str().to_string()),
            title: extraction.title,
            byline: extraction.byline,
            site_name: extraction.site_name,
            published: extraction.published,
            lang: extraction.lang,
            markdown_chars: extraction.markdown_chars,
            markdown: None,
            truncated: extraction.truncated,
            limit_hit: extraction.limit_hit,
            render_mode: source.render_mode.as_str().to_string(),
            render_warning: source.render_warning.clone(),
            cached: false,
            fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            duration_ms,
            links: extraction.links.unwrap_or_default(),
            links_requested: include_links,
            links_truncated: extraction.links_truncated,
            render_quality: render_quality_for(source.render_mode, extraction.word_count),
            content_warning: interstitial_warning(
                page.url(),
                &source.response.final_url,
                source.response.status,
                extraction.word_count,
            )
            .map(str::to_string),
            word_count: extraction.word_count,
        };
        let body = extraction.markdown;
        // The cache stores the body once (`FetchPageResult.markdown`) and the
        // payload without it; a cache hit reconstructs the structured body.
        let cache_result = FetchPageResult {
            payload: payload.clone(),
            markdown: body.clone(),
        };
        let result_json = serde_json::to_string(&cache_result).map_err(WidowError::internal)?;
        let mut payload = payload;
        payload.markdown = if include_content {
            Some(body.clone())
        } else {
            None
        };
        let result = FetchPageResult {
            payload,
            markdown: body,
        };

        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.fetched_at = fetched_at;
        record.status = Some(result.payload.status);
        record.content_type = Some(result.payload.content_type.clone());
        record.charset = Some(source.decoded.charset);
        record.etag = source.response.etag.clone();
        record.last_modified = source.response.last_modified.clone();
        record.raw_hash = Some(source.raw_hash.clone());
        record.text_hash = Some(content_hash(result.markdown.as_bytes()));
        record.title = result.payload.title.clone();
        record.lang = result.payload.lang.clone();
        record.render_mode = source.render_mode.as_str().to_string();
        record.outcome = "ok".to_string();
        record.duration_ms = duration_ms;
        record.mode = mode.as_str().to_string();
        record.bytes = Some(source.response.bytes);
        record.index_text = Some(IndexText::replace(self.index_text(&result.markdown)));
        record.blob = Some(BlobRow {
            hash: source.raw_hash,
            size: source.response.body.len() as i64,
            content_type: Some(result.payload.content_type.clone()),
            created_at: fetched_at,
        });
        record.extraction = Some(ExtractionRow {
            profile_hash,
            result_hash: content_hash(result_json.as_bytes()),
            result_json,
            record_count: 1,
            created_at: fetched_at,
        });

        if let Err(error) = self.store.db.record_fetch(record).await {
            tracing::warn!(error = %error, "could not persist fetch result");
        }
        tracing::debug!(
            url = %log_target(&page, self.config.logging.redact_query),
            status = result.payload.status,
            bytes = source.response.bytes,
            render_mode = source.render_mode.as_str(),
            duration_ms,
            "fetch_page completed"
        );
        Ok(result)
    }

    /// `search`: one SERP page through the shared gate (IMP-CONTRACT-002 §1.2).
    ///
    /// Stateless: no page/extraction/profile/blob/job row is written, and a
    /// failure is never recorded. The query is caller input, so logs carry the
    /// engine, result count and duration only — never the query or the SERP URL.
    /// Read one RSS/Atom/JSON feed: discovery only. No render, no store write,
    /// no conditional GET — mirrors `search_fetch` for acquisition.
    pub async fn fetch_feed(&self, input: FeedInput) -> Result<FeedPayload> {
        let started = Instant::now();
        let page = PageUrl::parse(&input.url)?;
        let limit = match input.limit {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/limit".to_string(),
                    message: "limit must be >= 1".to_string(),
                });
            }
            Some(limit) => limit.min(self.config.feed.max_limit),
            None => self
                .config
                .feed
                .default_limit
                .min(self.config.feed.max_limit),
        };
        let response = self
            .http
            .fetch(FetchRequest {
                url: page.url().clone(),
                conditional: Conditional::default(),
                accept: feed::ACCEPT_FEED,
                max_body_bytes: self.config.fetch.max_body_bytes,
                hop_guard: Some(Arc::clone(&self.hop_guard)),
                min_interval: None,
                scope: ContentScope::Page,
            })
            .await?;
        let content_type = response
            .content_type
            .as_deref()
            .map(content::base_media_type)
            .unwrap_or_else(|| "(none)".to_string());
        if let Some(kind) = content::classify(response.content_type.as_deref())
            && matches!(
                kind,
                content::MediaKind::Html | content::MediaKind::Xhtml | content::MediaKind::Text
            )
        {
            return Err(WidowError::UnsupportedContentType {
                content_type,
                processed: content::PROCESSED_TYPES.to_string(),
            });
        }
        let decoded = charset::decode(&response.body, response.content_type.as_deref());
        let parsed = feed::parse::parse(
            decoded.text.as_bytes(),
            &response.final_url,
            self.config.feed.max_summary_chars,
        )?;
        let item_count = parsed.item_count;
        let mut items = parsed.items;
        items.truncate(limit);
        let truncated = item_count > limit || parsed.truncated_summaries > 0;
        let limit_hit = if item_count > limit {
            Some("limit".to_string())
        } else if parsed.truncated_summaries > 0 {
            Some("max_summary_chars".to_string())
        } else {
            None
        };
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        tracing::info!(
            target = %log_target(&page, self.config.logging.redact_query),
            feed_type = parsed.feed_type,
            item_count,
            returned = items.len(),
            duration_ms,
            truncated,
            "fetch_feed completed"
        );
        Ok(FeedPayload {
            url: page.raw().to_string(),
            final_url: response.final_url.to_string(),
            content_type,
            feed_type: parsed.feed_type.to_string(),
            title: parsed.title,
            site_url: parsed.site_url,
            language: parsed.language,
            description: parsed.description,
            fetched_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            duration_ms,
            items,
            item_count,
            truncated,
            limit_hit,
            truncated_summaries: parsed.truncated_summaries,
        })
    }

    /// One sitemap document through the shared fetch adapter: same SSRF gate,
    /// robots hop guard, redirect guard, politeness lease and body cap as
    /// `fetch_page`. `fetch_untyped` is used because a `.xml.gz` may arrive as
    /// `application/octet-stream`; the scope is enforced by the parser instead.
    async fn sitemap_document(
        &self,
        page: &PageUrl,
        limits: crate::sitemap::SitemapLimits,
    ) -> Result<FetchedSitemap> {
        let response = self
            .http
            .fetch_untyped(FetchRequest {
                url: page.url().clone(),
                conditional: Conditional::default(),
                accept: crate::sitemap::ACCEPT_SITEMAP,
                max_body_bytes: limits.max_bytes,
                hop_guard: Some(Arc::clone(&self.hop_guard)),
                min_interval: None,
                scope: ContentScope::Page,
            })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(WidowError::Http {
                status: Some(response.status),
                detail: format!("origin returned HTTP {} for the sitemap", response.status),
            });
        }
        let parsed =
            crate::sitemap::parse_body(&response.body, response.content_type.as_deref(), limits)?;
        Ok(FetchedSitemap {
            final_url: response.final_url.to_string(),
            status: response.status,
            parsed,
        })
    }

    /// `sitemap`: discovery-only fetch and streaming parse (IMP-DEC-030).
    ///
    /// Nothing is stored: no page, extraction, blob, job or fetch-event row is
    /// written. An index is followed up to `[sitemap].max_children`; each child
    /// request passes the same gate, and a child failure is reported in
    /// `sitemaps[].error` instead of being dropped.
    pub async fn sitemap(&self, input: SitemapInput) -> Result<SitemapPayload> {
        let started = Instant::now();
        let limits = self.config.sitemap.limits();
        let limit = match input.limit {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/limit".to_string(),
                    message: "limit must be >= 1".to_string(),
                });
            }
            Some(limit) => limit.min(limits.max_urls),
            None => limits.max_urls,
        };
        let include_lastmod = input.include_lastmod.unwrap_or(true);
        let follow_index = input.follow_index.unwrap_or(true);
        let page = PageUrl::parse(&input.url)?;

        let root = self.sitemap_document(&page, limits).await?;
        // A parsed document always has a kind; a missing one is an internal
        // invariant breach, never a fabricated "unknown" value on the wire.
        let sitemap_type = root
            .parsed
            .kind
            .ok_or_else(|| WidowError::internal("parsed sitemap has no document kind"))?;
        let mut entries: Vec<crate::sitemap::SitemapUrl> = Vec::new();
        let mut reports: Vec<crate::sitemap::SitemapChildReport> = Vec::new();
        let mut skipped_scheme = root.parsed.skipped_scheme;
        let mut invalid_lastmod = root.parsed.invalid_lastmod;
        let mut cap_hit: Option<String> = root.parsed.limit_hit.map(str::to_string);
        let mut child_cut = false;

        match root.parsed.kind {
            Some(crate::sitemap::SitemapKind::Index) if follow_index => {
                let total_children = root.parsed.children.len();
                for (index, child) in root.parsed.children.iter().enumerate() {
                    let child_page = match PageUrl::parse(&child.loc) {
                        Ok(page) => page,
                        Err(error) => {
                            reports.push(crate::sitemap::SitemapChildReport {
                                loc: child.loc.clone(),
                                url_count: 0,
                                error: Some(error.code().as_str().to_string()),
                            });
                            continue;
                        }
                    };
                    match self.sitemap_document(&child_page, limits).await {
                        Ok(document) => {
                            if document.parsed.kind != Some(crate::sitemap::SitemapKind::Urlset) {
                                reports.push(crate::sitemap::SitemapChildReport {
                                    loc: child.loc.clone(),
                                    url_count: 0,
                                    error: Some("extraction_failed".to_string()),
                                });
                                continue;
                            }
                            skipped_scheme += document.parsed.skipped_scheme;
                            invalid_lastmod += document.parsed.invalid_lastmod;
                            if document.parsed.truncated && cap_hit.is_none() {
                                cap_hit = document.parsed.limit_hit.map(str::to_string);
                            }
                            let mut count = 0usize;
                            for entry in document.parsed.urls {
                                entries.push(entry);
                                count += 1;
                            }
                            reports.push(crate::sitemap::SitemapChildReport {
                                loc: child.loc.clone(),
                                url_count: count,
                                error: None,
                            });
                        }
                        Err(error) => reports.push(crate::sitemap::SitemapChildReport {
                            loc: child.loc.clone(),
                            url_count: 0,
                            error: Some(error.code().as_str().to_string()),
                        }),
                    }
                    if entries.len() >= limit {
                        // Further children would only add entries past the cap.
                        child_cut = index + 1 < total_children;
                        break;
                    }
                }
            }
            Some(crate::sitemap::SitemapKind::Urlset) | Some(crate::sitemap::SitemapKind::Text) => {
                entries = root.parsed.urls.clone()
            }
            _ => {}
        }

        let limit_cut = entries.len() > limit || child_cut;
        if limit_cut {
            cap_hit = Some("limit".to_string());
        }
        entries.truncate(limit);
        if !include_lastmod {
            for entry in &mut entries {
                entry.lastmod = None;
            }
        }
        let url_count = entries.len();
        let truncated = limit_cut || cap_hit.is_some();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        tracing::info!(
            target = %log_target(&page, self.config.logging.redact_query),
            sitemap_type = sitemap_type.as_str(),
            url_count,
            children = reports.len(),
            duration_ms,
            truncated,
            "sitemap completed"
        );
        Ok(SitemapPayload {
            url: page.raw().to_string(),
            final_url: root.final_url,
            status: root.status,
            sitemap_type,
            urls: entries,
            url_count,
            skipped_scheme,
            invalid_lastmod,
            sitemaps: reports,
            truncated,
            limit_hit: cap_hit,
            fetched_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        })
    }

    /// `pdf_extract`: text-layer PDF → `## Page N` markdown (IMP-DEC-041).
    ///
    /// The engine is ensured **before** the request, so a broken deployment
    /// spends no fetch. The body then comes through the shared adapter
    /// (`fetch_untyped`: SSRF gate, per-hop robots guard, redirect vetting,
    /// politeness lease and the `[pdf].max_bytes` streaming cap); the PDF scope
    /// is applied here because the shared media scope deliberately excludes
    /// PDFs. Parsing runs on a blocking thread behind the engine mutex and
    /// `catch_unwind` (Rust panics only; a native PDFium crash is not catchable,
    /// RISK-046). `fetch_page`/`extract` keep refusing PDFs.
    pub async fn pdf_extract(&self, input: PdfExtractInput) -> Result<PdfExtractPayload> {
        let started = Instant::now();
        let limits = self.config.pdf.limits();
        let max_pages = match input.max_pages {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/max_pages".to_string(),
                    message: "max_pages must be >= 1".to_string(),
                });
            }
            Some(value) => value.min(limits.max_pages),
            None => limits.max_pages,
        };
        let max_chars = match input.max_chars {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/max_chars".to_string(),
                    message: "max_chars must be >= 1".to_string(),
                });
            }
            Some(value) => value.min(limits.max_chars),
            None => limits.max_chars,
        };
        let page = PageUrl::parse(&input.url)?;

        // Fail before the request when the engine cannot load; the source is
        // process-wide and cannot change without a restart.
        let engine_spec = pdf::engine::EngineSpec::from_config(
            &self.config.pdf,
            self.store
                .blobs
                .root()
                .parent()
                .map(std::path::Path::to_path_buf),
        );
        pdf::engine::ensure(&engine_spec)?;

        let response = self
            .http
            .fetch_untyped(FetchRequest {
                url: page.url().clone(),
                conditional: Conditional::default(),
                accept: pdf::ACCEPT_PDF,
                max_body_bytes: limits.max_bytes,
                hop_guard: Some(Arc::clone(&self.hop_guard)),
                min_interval: None,
                scope: ContentScope::Page,
            })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(WidowError::Http {
                status: Some(response.status),
                detail: format!("origin returned HTTP {} for the PDF", response.status),
            });
        }

        // The PDF scope: a PDF media type or the `%PDF-` magic. `fetch_page`
        // and `extract` still refuse the type before reading the body.
        let observed_type = response
            .content_type
            .as_deref()
            .map(content::base_media_type)
            .unwrap_or_else(|| "(none)".to_string());
        let is_pdf_type = observed_type == "application/pdf";
        let is_pdf_magic = response.body.starts_with(b"%PDF-");
        if !is_pdf_type && !is_pdf_magic {
            return Err(WidowError::UnsupportedContentType {
                content_type: observed_type,
                processed: "application/pdf".to_string(),
            });
        }
        if !is_pdf_magic {
            return Err(WidowError::PdfInvalid(
                "response is not a PDF (`%PDF-` header missing)".to_string(),
            ));
        }

        let fetched_at = Utc::now();
        let body_len = response.body.len() as i64;
        let bytes: Arc<[u8]> = Arc::from(response.body);
        let parse_limits = pdf::PdfLimits {
            max_bytes: limits.max_bytes,
            max_pages,
            max_chars,
        };
        let for_parse = Arc::clone(&bytes);
        let joined = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(|| pdf::extract(&for_parse, parse_limits))
        })
        .await;
        let parsed = match joined {
            Ok(Ok(result)) => result?,
            Ok(Err(_panic)) => {
                return Err(WidowError::PdfInvalid(
                    "PDF parser panicked; the document is rejected".to_string(),
                ));
            }
            Err(join) => return Err(WidowError::internal(join)),
        };

        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let payload = PdfExtractPayload {
            url: page.raw().to_string(),
            final_url: response.final_url.to_string(),
            status: response.status,
            content_type: "application/pdf".to_string(),
            pages: parsed.pages,
            declared_pages: parsed.declared_pages,
            pages_with_text: parsed.pages_with_text,
            empty_pages: parsed.empty_pages.clone(),
            empty_pages_truncated: parsed.empty_pages_truncated,
            chars: parsed.chars,
            markdown: parsed.markdown.clone(),
            metadata: parsed.metadata.clone(),
            outcome: parsed.outcome,
            truncated: parsed.truncated,
            limit_hit: parsed.limit_hit.map(str::to_string),
            fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        tracing::info!(
            target = %log_target(&page, self.config.logging.redact_query),
            pages = parsed.pages,
            declared_pages = parsed.declared_pages,
            pages_with_text = parsed.pages_with_text,
            outcome = parsed.outcome.as_str(),
            duration_ms,
            "pdf_extract completed"
        );

        // Only a complete text layer becomes a stored page: a partial or
        // no-text result would otherwise claim a page the tool did not deliver.
        if parsed.outcome == pdf::PdfOutcome::Ok {
            let raw_hash = self.store.put_blob(&bytes).await?;
            let result_json = serde_json::to_string(&payload).map_err(WidowError::internal)?;
            let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
            record.site = page.site();
            record.fetched_at = fetched_at;
            record.status = Some(response.status);
            record.content_type = Some("application/pdf".to_string());
            record.raw_hash = Some(raw_hash.clone());
            record.text_hash = Some(content_hash(parsed.markdown.as_bytes()));
            record.title = parsed.metadata.title.clone();
            record.render_mode = "pdf".to_string();
            record.outcome = "ok".to_string();
            record.duration_ms = duration_ms;
            record.mode = "pdf".to_string();
            record.bytes = Some(body_len as u64);
            record.index_text = Some(IndexText::replace(self.index_text(&parsed.markdown)));
            record.blob = Some(BlobRow {
                hash: raw_hash,
                size: body_len,
                content_type: Some("application/pdf".to_string()),
                created_at: fetched_at,
            });
            record.extraction = Some(ExtractionRow {
                profile_hash: "pdfium/v1".to_string(),
                result_hash: content_hash(result_json.as_bytes()),
                result_json,
                record_count: parsed.pages as i64,
                created_at: fetched_at,
            });
            if let Err(error) = self.store.db.record_fetch(record).await {
                tracing::warn!(error = %error, "could not persist PDF extraction");
            }
        }
        Ok(payload)
    }

    pub async fn search(&self, input: SearchInput) -> Result<SearchPayload> {
        let started = Instant::now();
        let query = search::validate_query(&input.query, self.config.search.max_query_chars)?;
        let limit = input.limit.unwrap_or(self.config.search.default_limit);
        search::validate_limit(limit)?;
        let freshness = input.freshness.unwrap_or_default();
        let site = match input.site.as_deref() {
            Some(raw) => Some(search::parse::normalize_site(raw).map_err(|message| {
                WidowError::SchemaInvalid {
                    pointer: "/site".to_string(),
                    message,
                }
            })?),
            None => None,
        };

        let primary_url = search::request_url(
            &self.config.search.html_url,
            &query,
            site.as_deref(),
            freshness,
        )?;
        let lite_url = search::request_url(
            &self.config.search.lite_url,
            &query,
            site.as_deref(),
            freshness,
        )?;

        // Primary is DDG html. A route refusal is final; only a transport or
        // parse failure may try the lite endpoint once.
        let (engine, response, outcome) =
            match self.search_attempt(&primary_url, Engine::Html).await {
                SearchAttempt::Parsed { response, outcome } => (Engine::Html, response, outcome),
                SearchAttempt::Fatal(error) => {
                    tracing::info!(
                        code = error.code().as_str(),
                        engine = Engine::Html.as_str(),
                        duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        "search failed"
                    );
                    return Err(error);
                }
                SearchAttempt::Retryable(primary_error) => {
                    match self.search_attempt(&lite_url, Engine::Lite).await {
                        SearchAttempt::Parsed { response, outcome } => {
                            (Engine::Lite, response, outcome)
                        }
                        SearchAttempt::Fatal(error) => {
                            tracing::info!(
                                code = error.code().as_str(),
                                engine = Engine::Lite.as_str(),
                                duration_ms =
                                    started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                                "search failed"
                            );
                            return Err(error);
                        }
                        SearchAttempt::Retryable(fallback_error) => {
                            tracing::info!(
                                primary = primary_error.code().as_str(),
                                fallback = fallback_error.code().as_str(),
                                duration_ms =
                                    started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                                "search failed after fallback"
                            );
                            return Err(primary_error);
                        }
                    }
                }
            };

        let parsed = match outcome {
            ParseOutcome::Empty => Vec::new(),
            ParseOutcome::Results(results) => results,
        };
        let mut results = Vec::with_capacity(parsed.len());
        for record in parsed {
            if let Some(site) = site.as_deref() {
                let on_site = Url::parse(&record.url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                    .is_some_and(|host| search::parse::site_matches(&host, site));
                if !on_site {
                    continue;
                }
            }
            results.push(SearchResult {
                title: record.title,
                url: record.url,
                snippet: record.snippet,
                engine: engine.as_str().to_string(),
            });
        }
        let result_count = results.len();
        let truncated = result_count > limit;
        if truncated {
            results.truncate(limit);
        }
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        tracing::info!(
            engine = engine.as_str(),
            result_count,
            returned = results.len(),
            duration_ms,
            truncated,
            "search completed"
        );
        Ok(SearchPayload {
            query,
            engine: engine.as_str().to_string(),
            final_url: response.final_url.to_string(),
            site,
            freshness: freshness.as_str().to_string(),
            fetched_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            duration_ms,
            results,
            result_count,
            truncated,
            limit_hit: if truncated {
                Some("limit".to_string())
            } else {
                None
            },
        })
    }

    /// One SERP GET through the same client/robots/host-gate path as a page
    /// fetch. Deliberately no `record_failure`: a search is not a page fetch.
    async fn search_fetch(&self, url: &Url) -> Result<RawResponse> {
        self.http
            .fetch(FetchRequest {
                url: url.clone(),
                conditional: Conditional::default(),
                accept: ACCEPT_PAGE,
                max_body_bytes: self.config.fetch.max_body_bytes,
                hop_guard: Some(Arc::clone(&self.hop_guard)),
                min_interval: None,
                scope: ContentScope::Page,
            })
            .await
    }

    async fn search_attempt(&self, url: &Url, engine: Engine) -> SearchAttempt {
        let response = match self.search_fetch(url).await {
            Ok(response) => response,
            Err(error) if search_retryable(&error) => return SearchAttempt::Retryable(error),
            Err(error) => return SearchAttempt::Fatal(error),
        };
        let text = charset::decode(&response.body, response.content_type.as_deref()).text;
        match search::parse::parse(&text, engine) {
            Ok(outcome) => SearchAttempt::Parsed {
                response: Box::new(response),
                outcome,
            },
            Err(error) => SearchAttempt::Retryable(WidowError::Extraction(error.to_string())),
        }
    }

    pub async fn extract(&self, input: ExtractInput) -> Result<ExtractPayload> {
        let started = Instant::now();
        let policy = input.render.unwrap_or(RenderPolicy::Auto);
        let max_field_chars = input
            .max_field_chars
            .unwrap_or(self.config.extract.max_field_chars)
            .min(self.config.extract.max_field_chars);
        let max_chars = input
            .max_chars
            .unwrap_or(self.config.extract.max_chars)
            .min(self.config.extract.max_chars);

        // Validation and selector compilation are CPU work: keep them off the
        // reactor. The error carries the JSON pointer of the offending node.
        let raw_schema = input.schema;
        let (validated, compiled) = tokio::task::spawn_blocking(move || {
            let validated = spec::validate(&raw_schema)?;
            let compiled = spec::compile(&validated.spec)?;
            Ok::<_, WidowError>((validated, compiled))
        })
        .await
        .map_err(|error| WidowError::internal(format!("spec validation task failed: {error}")))??;

        let page = PageUrl::parse(&input.url)?;
        let use_cache = input.use_cache.unwrap_or(false);
        let label = "extract";

        // 1. Cache window: no network at all when the caller allows it. A cached
        // result from a different render policy is not a substitute.
        if use_cache
            && policy != RenderPolicy::Always
            && let Some(cached) = self
                .store
                .db
                .cached_extraction(page.hash(), &validated.hash)
                .await?
        {
            let threshold = Utc::now()
                - chrono::Duration::seconds(
                    self.config.extract.cache_freshness_s.min(i64::MAX as u64) as i64,
                );
            if cached.created_at >= threshold
                && let Ok(mut payload) = serde_json::from_str::<ExtractPayload>(&cached.result_json)
                && cached_mode_acceptable(policy, &payload.render_mode)
            {
                apply_extract_caps(&mut payload, max_field_chars, max_chars);
                payload.cached = true;
                payload.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(payload);
            }
        }

        let mut source = if policy == RenderPolicy::Always {
            self.acquire_rendered(&page, label, started, None).await?
        } else {
            // `use_cache: false` means "always fetch a fresh body", so no
            // conditional headers are sent and a 304 cannot occur.
            let conditional = if use_cache {
                self.conditional_for(&page).await?
            } else {
                Conditional::default()
            };
            match self
                .acquire_static(&page, label, started, conditional, None, ContentScope::Page)
                .await?
            {
                StaticSource::Document(document) => document,
                StaticSource::NotModified(response) => {
                    let duration = started.elapsed();
                    self.record_not_modified(&page, label, &response, duration)
                        .await?;
                    if use_cache
                        && let Some(cached) = self
                            .store
                            .db
                            .cached_extraction(page.hash(), &validated.hash)
                            .await?
                        && let Ok(mut payload) =
                            serde_json::from_str::<ExtractPayload>(&cached.result_json)
                        && cached_mode_acceptable(policy, &payload.render_mode)
                    {
                        apply_extract_caps(&mut payload, max_field_chars, max_chars);
                        payload.status = 304;
                        payload.cached = true;
                        payload.duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;
                        return Ok(payload);
                    }
                    match self
                        .acquire_static(
                            &page,
                            label,
                            started,
                            Conditional::default(),
                            None,
                            ContentScope::Page,
                        )
                        .await?
                    {
                        StaticSource::Document(document) => document,
                        StaticSource::NotModified(_) => {
                            return Err(WidowError::internal(
                                "origin answered 304 to an unconditional request",
                            ));
                        }
                    }
                }
            }
        };

        let eval_options = spec_eval::EvalOptions {
            max_records: self.config.extract.max_records,
        };
        // One readability pass per acquisition: `:main` evaluates the same
        // article the `main` body is built from, never a second selection.
        let main_scoped = matches!(&compiled.container, Some(spec::Container::Main));
        // The gap repair needs the source block tree, so `:main` parses the page
        // once here (the article pass remains the only readability pass). The
        // tree is dropped before the next await: scraper's tree is not `Send`.
        let mut article = if main_scoped {
            let document = Html::parse_document(&source.decoded.text);
            extract::main_article(&document, &source.decoded.text, &source.response.final_url)
        } else {
            None
        };
        let mut outcome = extract::evaluate_main_scoped(
            &source.decoded.text,
            &source.response.final_url,
            article.as_ref().map(|(content, _)| content.as_str()),
            &compiled,
            &eval_options,
        );
        let mut render_warning: Option<String> = None;

        // 5. `auto`: the heuristic needs a word count without a second DOM
        // parse for `extract`; the approximation only feeds the decision.
        if policy == RenderPolicy::Auto && source.render_mode == RenderMode::Static {
            let main_words = render::auto::rough_visible_word_count(&source.decoded.text);
            if self.auto_wants_render(&page, &source, main_words) {
                match self.acquire_rendered(&page, label, started, None).await {
                    Ok(rendered) => {
                        source = rendered;
                        article = if main_scoped {
                            let document = Html::parse_document(&source.decoded.text);
                            extract::main_article(
                                &document,
                                &source.decoded.text,
                                &source.response.final_url,
                            )
                        } else {
                            None
                        };
                        outcome = extract::evaluate_main_scoped(
                            &source.decoded.text,
                            &source.response.final_url,
                            article.as_ref().map(|(content, _)| content.as_str()),
                            &compiled,
                            &eval_options,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "render=auto fallback to static extraction");
                        render_warning = Some(error.to_string());
                    }
                }
            }
        }

        if render_warning.is_some() {
            source.render_mode = RenderMode::StaticFallback;
            source.render_warning = render_warning;
        }

        let fetched_at = Utc::now();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let max_records_truncated = outcome.truncated;
        let mut payload = ExtractPayload {
            url: page.raw().to_string(),
            final_url: source.response.final_url.to_string(),
            status: source.response.status,
            content_type: source
                .response
                .content_type
                .as_deref()
                .map(content::base_media_type)
                .unwrap_or_else(|| source.media.as_str().to_string()),
            records: outcome.records,
            record_count: outcome.record_count,
            field_errors: outcome.field_errors,
            truncated: max_records_truncated,
            limit_hit: if max_records_truncated {
                Some("max_records".to_string())
            } else {
                None
            },
            truncated_fields: 0,
            render_mode: source.render_mode.as_str().to_string(),
            render_quality: extract_render_quality(source.render_mode, &source.decoded.text),
            render_warning: source.render_warning.clone(),
            cached: false,
            fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            duration_ms,
        };
        // Cache the full evaluation (max_records-limited, never the presentation
        // caps), then truncate the returned copy.
        let result_json = serde_json::to_string(&payload).map_err(WidowError::internal)?;
        apply_extract_caps(&mut payload, max_field_chars, max_chars);

        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.fetched_at = fetched_at;
        record.status = Some(payload.status);
        record.content_type = Some(payload.content_type.clone());
        record.charset = Some(source.decoded.charset);
        record.etag = source.response.etag.clone();
        record.last_modified = source.response.last_modified.clone();
        record.raw_hash = Some(source.raw_hash.clone());
        record.render_mode = source.render_mode.as_str().to_string();
        record.outcome = "ok".to_string();
        record.duration_ms = duration_ms;
        record.mode = label.to_string();
        record.bytes = Some(source.response.bytes);
        // The index stores what the caller received: the record string values
        // after the presentation caps, so a hit never quotes text the tool
        // refused to return as a field. `fill_empty` because records project a
        // page: they may give a page its first indexed text, never replace the
        // page text a `fetch_page`/crawl/monitor already indexed (M05 B01b).
        record.index_text = Some(IndexText::fill_empty(self.index_text(
            &crate::store::fts::index_text_from_records(&payload.records),
        )));
        record.blob = Some(BlobRow {
            hash: source.raw_hash,
            size: source.response.body.len() as i64,
            content_type: Some(payload.content_type.clone()),
            created_at: fetched_at,
        });
        record.extraction = Some(ExtractionRow {
            profile_hash: validated.hash.clone(),
            result_hash: content_hash(result_json.as_bytes()),
            result_json,
            record_count: payload.record_count as i64,
            created_at: fetched_at,
        });
        record.profile = Some(ProfileRow {
            hash: validated.hash.clone(),
            name: None,
            spec_json: validated.canonical_json,
            created_at: fetched_at,
        });

        if let Err(error) = self.store.db.record_fetch(record).await {
            tracing::warn!(error = %error, "could not persist extraction");
        }
        tracing::debug!(
            url = %log_target(&page, self.config.logging.redact_query),
            status = payload.status,
            records = payload.record_count,
            field_errors = payload.field_errors.len(),
            render_mode = source.render_mode.as_str(),
            duration_ms,
            "extract completed"
        );
        Ok(payload)
    }

    /// `extract_rows` (IMP-DEC-033): fetch one URL through the shared raw path
    /// with a content scope that adds CSV/TSV, then parse it as delimited text,
    /// JSON or an HTML table. Numbers/booleans/nulls from JSON keep their type;
    /// delimited and table cells are strings by nature.
    pub async fn extract_rows(&self, input: ExtractRowsInput) -> Result<ExtractRowsPayload> {
        let started = Instant::now();
        let policy = input.render.unwrap_or(RenderPolicy::Auto);
        let use_cache = input.use_cache.unwrap_or(false);
        let label = "extract_rows";

        // 1. Validate before any network request: every error carries the JSON
        // pointer of the offending field (no new error code).
        let requested = rows::RowFormat::parse(input.format.as_deref().unwrap_or("auto"))?;
        let header_row = input.header_row.unwrap_or(true);
        let limit = rows::check_limit(input.limit.unwrap_or(rows::DEFAULT_LIMIT))?;
        let max_field_chars = rows::check_max_field_chars(
            input
                .max_field_chars
                .unwrap_or(rows::DEFAULT_MAX_FIELD_CHARS),
        )?;
        // CSS treats surrounding whitespace as insignificant, so a padded
        // selector is the same selector. A JSON pointer and a delimiter are
        // data: they are validated exactly as given (a `\t` delimiter is not
        // "empty", and a pointer's key may legitimately end in a space).
        let selector = input
            .selector
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let pointer = input.json_pointer.as_deref().unwrap_or("");
        let delimiter_raw = input.delimiter.as_deref();
        let delimiter = delimiter_raw.map(rows::parse_delimiter).transpose()?;

        if requested
            .resolved()
            .is_some_and(|format| format != rows::RowFormat::HtmlTable)
            && selector.is_some()
        {
            return Err(WidowError::SchemaInvalid {
                pointer: "/selector".to_string(),
                message: "`selector` picks an HTML table; it is only valid with format html_table"
                    .to_string(),
            });
        }
        if requested
            .resolved()
            .is_some_and(|format| format != rows::RowFormat::Csv)
            && delimiter_raw.is_some()
        {
            return Err(WidowError::SchemaInvalid {
                pointer: "/delimiter".to_string(),
                message: "`delimiter` is only valid with format csv".to_string(),
            });
        }
        if requested
            .resolved()
            .is_some_and(|format| format != rows::RowFormat::Json)
            && !pointer.is_empty()
        {
            return Err(WidowError::SchemaInvalid {
                pointer: "/json_pointer".to_string(),
                message: "`json_pointer` is only valid with format json".to_string(),
            });
        }
        rows::json::validate_pointer(pointer)?;
        if let Some(selector) = selector {
            rows::table::validate_selector(selector)?;
        }

        // 2. The tabular spec is the *request* (`format: auto` included), so the
        // profile hash is known before the fetch and an auto call can hit cache.
        let spec = RowsSpec {
            format: requested.as_str().to_string(),
            selector: selector.map(str::to_string),
            delimiter: delimiter.map(|character| character.to_string()),
            header_row,
            json_pointer: (!pointer.is_empty()).then(|| pointer.to_string()),
        };
        let spec_json = serde_json::to_string(&spec).map_err(WidowError::internal)?;
        let spec_hash = content_hash(spec_json.as_bytes());
        let page = PageUrl::parse(&input.url)?;

        // 3. Cache window: no network at all when the caller allows it.
        if use_cache
            && policy != RenderPolicy::Always
            && let Some((mut payload, created_at)) =
                self.stored_rows(&page, &spec_hash, policy, limit).await?
        {
            let threshold = Utc::now()
                - chrono::Duration::seconds(
                    self.config.extract.cache_freshness_s.min(i64::MAX as u64) as i64,
                );
            if created_at >= threshold {
                finish_cached_rows(&mut payload, max_field_chars, limit);
                payload.cached = true;
                payload.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(payload);
            }
        }

        // 4. Acquisition. `render` applies to HTML tables only; an explicit
        // html_table + always goes straight to the renderer, and `auto` renders
        // only when the resolved format is a table and the shared thin-content
        // rule fires.
        let mut source =
            if policy == RenderPolicy::Always && requested == rows::RowFormat::HtmlTable {
                self.acquire_rendered(&page, label, started, None).await?
            } else {
                let conditional = if use_cache {
                    self.conditional_for(&page).await?
                } else {
                    Conditional::default()
                };
                match self
                    .acquire_static(
                        &page,
                        label,
                        started,
                        conditional,
                        None,
                        ContentScope::Tabular,
                    )
                    .await?
                {
                    StaticSource::Document(document) => document,
                    StaticSource::NotModified(response) => {
                        let duration = started.elapsed();
                        self.record_not_modified(&page, label, &response, duration)
                            .await?;
                        if use_cache
                            && let Some((mut payload, _)) =
                                self.stored_rows(&page, &spec_hash, policy, limit).await?
                        {
                            finish_cached_rows(&mut payload, max_field_chars, limit);
                            payload.status = 304;
                            payload.cached = true;
                            payload.duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;
                            return Ok(payload);
                        }
                        match self
                            .acquire_static(
                                &page,
                                label,
                                started,
                                Conditional::default(),
                                None,
                                ContentScope::Tabular,
                            )
                            .await?
                        {
                            StaticSource::Document(document) => document,
                            StaticSource::NotModified(_) => {
                                return Err(WidowError::internal(
                                    "origin answered 304 to an unconditional request",
                                ));
                            }
                        }
                    }
                }
            };

        // 5. Resolve `auto` from the media type, then from a bounded sniff.
        let mut resolved = match requested.resolved() {
            Some(format) => format,
            None => resolve_row_format(&source)?,
        };
        let mut render_warning: Option<String> = None;
        if resolved == rows::RowFormat::HtmlTable && source.render_mode == RenderMode::Static {
            let wants = match policy {
                RenderPolicy::Always => true,
                RenderPolicy::Never => false,
                RenderPolicy::Auto => self.auto_wants_render(
                    &page,
                    &source,
                    render::auto::rough_visible_word_count(&source.decoded.text),
                ),
            };
            if wants {
                match self.acquire_rendered(&page, label, started, None).await {
                    Ok(rendered) => {
                        source = rendered;
                        resolved = rows::RowFormat::HtmlTable;
                    }
                    Err(error) if policy == RenderPolicy::Always => return Err(error),
                    Err(error) => {
                        tracing::warn!(error = %error, "render=auto fallback to the static table");
                        render_warning = Some(error.to_string());
                    }
                }
            }
        }
        if render_warning.is_some() {
            source.render_mode = RenderMode::StaticFallback;
        }

        // 6. Parse. The parsers are pure and each owns one format's rules.
        let mut parsed = match resolved {
            rows::RowFormat::Csv => {
                rows::csv::parse(&source.decoded.text, delimiter, header_row, limit)?
            }
            rows::RowFormat::Tsv => {
                rows::csv::parse(&source.decoded.text, Some('\t'), header_row, limit)?
            }
            rows::RowFormat::Json => rows::json::parse(&source.decoded.text, pointer, limit)?,
            rows::RowFormat::HtmlTable => {
                rows::table::parse(&source.decoded.text, selector, header_row, limit)?
            }
            rows::RowFormat::Auto => {
                return Err(WidowError::internal(
                    "unresolved row format after acquisition",
                ));
            }
        };
        parsed.apply_limit(limit);

        let fetched_at = Utc::now();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let limit_truncated = parsed.truncated;
        let mut payload = ExtractRowsPayload {
            url: page.raw().to_string(),
            final_url: source.response.final_url.to_string(),
            status: source.response.status,
            content_type: source
                .response
                .content_type
                .as_deref()
                .map(content::base_media_type)
                .unwrap_or_else(|| source.media.as_str().to_string()),
            format: resolved.as_str().to_string(),
            records: parsed.records,
            row_count: parsed.row_count,
            returned: 0,
            truncated: false,
            limit_hit: None,
            field_errors: parsed.field_errors,
            field_errors_truncated: parsed.field_errors_truncated,
            truncated_fields: 0,
            fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            render_mode: source.render_mode.as_str().to_string(),
            cached: false,
            duration_ms,
        };
        // Cache the uncapped records, then cut only the returned copy (the same
        // rule as `extract`). The stored copy still records *its own* row-level
        // truncation, so a later call with a larger `limit` can tell that the
        // cached table is short and refetch instead of reporting it as whole.
        payload.returned = payload.records.len();
        payload.truncated = limit_truncated;
        payload.limit_hit = limit_truncated.then(|| "limit".to_string());
        let result_json = serde_json::to_string(&payload).map_err(WidowError::internal)?;
        apply_rows_caps(&mut payload, max_field_chars);
        set_rows_flags(&mut payload, limit_truncated);

        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.fetched_at = fetched_at;
        record.status = Some(payload.status);
        record.content_type = Some(payload.content_type.clone());
        record.charset = Some(source.decoded.charset);
        record.etag = source.response.etag.clone();
        record.last_modified = source.response.last_modified.clone();
        record.raw_hash = Some(source.raw_hash.clone());
        record.render_mode = source.render_mode.as_str().to_string();
        record.outcome = "ok".to_string();
        record.duration_ms = duration_ms;
        record.mode = label.to_string();
        record.bytes = Some(source.response.bytes);
        // A row set is a projection of a page: it may give a page its first
        // indexed text, never replace delivered page text (M05 B01b).
        record.index_text = Some(IndexText::fill_empty(self.index_text(
            &crate::store::fts::index_text_from_records(&payload.records),
        )));
        record.blob = Some(BlobRow {
            hash: source.raw_hash,
            size: source.response.body.len() as i64,
            content_type: Some(payload.content_type.clone()),
            created_at: fetched_at,
        });
        record.extraction = Some(ExtractionRow {
            profile_hash: spec_hash.clone(),
            result_hash: content_hash(result_json.as_bytes()),
            result_json,
            record_count: payload.returned as i64,
            created_at: fetched_at,
        });
        record.profile = Some(ProfileRow {
            hash: spec_hash,
            name: None,
            spec_json,
            created_at: fetched_at,
        });

        if let Err(error) = self.store.db.record_fetch(record).await {
            tracing::warn!(error = %error, "could not persist tabular extraction");
        }
        tracing::debug!(
            url = %log_target(&page, self.config.logging.redact_query),
            status = payload.status,
            format = %payload.format,
            rows = payload.row_count,
            returned = payload.returned,
            field_errors = payload.field_errors.len(),
            render_mode = source.render_mode.as_str(),
            duration_ms,
            "extract_rows completed"
        );
        Ok(payload)
    }

    /// Store a named, versioned recipe (IMP-DEC-034). The spec is validated by
    /// the same `extract` validator and stored in `profiles`; the recipe tables
    /// hold only the name/version pointer.
    pub async fn recipe_save(&self, input: RecipeSaveInput) -> Result<RecipeSavePayload> {
        recipes::validate_name(&input.name)?;
        if let Some(description) = input.description.as_deref() {
            recipes::validate_description(description)?;
        }
        let raw_schema = input.schema;
        let validated = tokio::task::spawn_blocking(move || spec::validate(&raw_schema))
            .await
            .map_err(|error| {
                WidowError::internal(format!("spec validation task failed: {error}"))
            })??;
        let saved = self
            .store
            .db
            .save_recipe(NewRecipe {
                name: input.name.clone(),
                profile_hash: validated.hash.clone(),
                spec_json: validated.canonical_json,
                description: input.description,
                now: Utc::now(),
            })
            .await?;
        Ok(RecipeSavePayload {
            name: input.name,
            version: recipe_version(saved.version)?,
            profile_hash: validated.hash,
            changed: saved.changed,
            created_at: saved
                .version_created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        })
    }

    /// List recipes by name with an optional literal prefix (IMP-DEC-034).
    /// Read-only: no fetch, no job, nothing stored.
    pub async fn recipe_list(&self, input: RecipeListInput) -> Result<RecipeListPayload> {
        // A prefix is a query, not an identity: any string is accepted and a
        // typo simply matches nothing.
        let prefix = input.prefix.as_deref().filter(|value| !value.is_empty());
        let limit = recipes::check_list_limit(input.limit.unwrap_or(recipes::DEFAULT_LIST_LIMIT))?;
        let include_schema = input.include_schema.unwrap_or(false);
        let (rows, total) = self.store.db.recipe_page(prefix, limit).await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let schema = if include_schema {
                Some(serde_json::from_str(&row.spec_json).map_err(|error| {
                    WidowError::internal(format!("stored recipe spec is not JSON: {error}"))
                })?)
            } else {
                None
            };
            summaries.push(RecipeSummary {
                name: row.name,
                version: recipe_version(row.current_version)?,
                profile_hash: row.profile_hash,
                description: row.description,
                updated_at: row
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                schema,
            });
        }
        let count = summaries.len();
        Ok(RecipeListPayload {
            recipes: summaries,
            count,
            total: usize::try_from(total).unwrap_or(usize::MAX),
            truncated: count < usize::try_from(total).unwrap_or(usize::MAX),
        })
    }

    /// Apply a recipe version (default: current) through the ordinary `extract`
    /// pipeline. This resolves the stored spec and nothing else: every gate, cap
    /// and cache rule of `extract` still applies.
    pub async fn recipe_apply(&self, input: RecipeApplyInput) -> Result<RecipeApplyPayload> {
        recipes::validate_name(&input.name)?;
        if input.version == Some(0) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/version".to_string(),
                message: "version must be >= 1; omit it for the current version".to_string(),
            });
        }
        let (profile_hash, spec_json, version) = match input.version {
            Some(version) => {
                let row = self
                    .store
                    .db
                    .recipe_version(&input.name, i64::from(version))
                    .await?
                    .ok_or_else(|| {
                        WidowError::NotFound(format!(
                            "recipe `{}` version {version} does not exist",
                            input.name
                        ))
                    })?;
                (
                    row.profile_hash,
                    row.spec_json,
                    recipe_version(row.version)?,
                )
            }
            None => {
                let row = self.store.db.recipe(&input.name).await?.ok_or_else(|| {
                    WidowError::NotFound(format!("recipe `{}` does not exist", input.name))
                })?;
                (
                    row.profile_hash,
                    row.spec_json,
                    recipe_version(row.current_version)?,
                )
            }
        };
        let schema: serde_json::Value = serde_json::from_str(&spec_json).map_err(|error| {
            WidowError::internal(format!("stored recipe spec is not JSON: {error}"))
        })?;
        let extraction = self
            .extract(ExtractInput {
                url: input.url,
                schema,
                render: input.render,
                use_cache: input.use_cache,
                max_field_chars: input.max_field_chars,
                max_chars: input.max_chars,
            })
            .await?;
        Ok(RecipeApplyPayload {
            extraction,
            recipe: input.name,
            version,
            profile_hash,
        })
    }

    /// `corpus_export` (IMP-DEC-035): a JSONL file or a markdown directory under
    /// `<data_dir>/exports/`. Streamed, bounded and inline; the response is the
    /// path and counts, never the corpus.
    pub async fn corpus_export(&self, input: CorpusExportInput) -> Result<CorpusExportPayload> {
        let plan = ExportPlan::validate(input)?;
        export::write(&self.store, self.store.paths(), &plan).await
    }

    /// `snapshot_compare` (IMP-DEC-035): an explicit pair of monitor snapshots
    /// or page fetch versions. Read-only: it never writes a row.
    pub async fn snapshot_compare(
        &self,
        input: SnapshotCompareInput,
    ) -> Result<SnapshotComparePayload> {
        let plan = ComparePlan::validate(input)?;
        compare::run(&self.store, &plan).await
    }

    /// A stored tabular payload for (page, spec hash) that the render policy
    /// accepts. Freshness is the caller's decision: a 304 has already
    /// revalidated the page.
    async fn stored_rows(
        &self,
        page: &PageUrl,
        profile_hash: &str,
        policy: RenderPolicy,
        limit: usize,
    ) -> Result<Option<(ExtractRowsPayload, chrono::DateTime<Utc>)>> {
        let Some(cached) = self
            .store
            .db
            .cached_extraction(page.hash(), profile_hash)
            .await?
        else {
            return Ok(None);
        };
        let Ok(payload) = serde_json::from_str::<ExtractRowsPayload>(&cached.result_json) else {
            return Ok(None);
        };
        if !cached_mode_acceptable(policy, &payload.render_mode) {
            return Ok(None);
        }
        // The spec hash does not include `limit`, so a cache that stopped at an
        // earlier limit cannot answer a larger one: refetch instead of claiming a
        // shorter page is the whole table. A request at or below the cached
        // limit is answered (and still reported as truncated).
        if payload.limit_hit.as_deref() == Some("limit") && limit > payload.records.len() {
            return Ok(None);
        }
        Ok(Some((payload, cached.created_at)))
    }

    /// One crawl fetch: the same acquisition, render escalation, scope, decode,
    /// CAS write and failure recording as `fetch_page`, but returning the link
    /// set and (optionally) records from a **single** DOM parse.
    pub async fn crawl_page(
        &self,
        page: &PageUrl,
        schema: Option<CrawlSpec<'_>>,
        policy: RenderPolicy,
        min_interval: Option<Duration>,
        label: &str,
    ) -> Result<CrawlPage> {
        let started = Instant::now();
        let mut source = if policy == RenderPolicy::Always {
            self.acquire_rendered(page, label, started, min_interval)
                .await?
        } else {
            match self
                .acquire_static(
                    page,
                    label,
                    started,
                    Conditional::default(),
                    min_interval,
                    ContentScope::Page,
                )
                .await?
            {
                StaticSource::Document(document) => document,
                StaticSource::NotModified(_) => {
                    match self
                        .acquire_static(
                            page,
                            label,
                            started,
                            Conditional::default(),
                            min_interval,
                            ContentScope::Page,
                        )
                        .await?
                    {
                        StaticSource::Document(document) => document,
                        StaticSource::NotModified(_) => {
                            return Err(WidowError::internal(
                                "origin answered 304 to an unconditional request",
                            ));
                        }
                    }
                }
            }
        };

        let options = ExtractOptions {
            mode: Mode::Main,
            max_chars: self.config.extract.max_chars,
            max_links: self.config.extract.max_links,
            include_links: true,
            ignore_selectors: Vec::new(),
        };
        let eval_options = spec_eval::EvalOptions {
            max_records: self.config.extract.max_records,
        };
        let spec_pair = schema.map(|schema| (schema.compiled, &eval_options));
        let mut extraction = extract::extract_page(
            &source.decoded.text,
            &source.response.final_url,
            &options,
            spec_pair,
        )?;
        let mut render_warning: Option<String> = None;

        if policy == RenderPolicy::Auto
            && source.render_mode == RenderMode::Static
            && self.auto_wants_render(page, &source, extraction.main.word_count)
        {
            match self
                .acquire_rendered(page, label, started, min_interval)
                .await
            {
                Ok(rendered) => {
                    match extract::extract_page(
                        &rendered.decoded.text,
                        &rendered.response.final_url,
                        &options,
                        spec_pair,
                    ) {
                        Ok(rendered_extraction) => {
                            source = rendered;
                            extraction = rendered_extraction;
                        }
                        Err(error) => {
                            render_warning = Some(format!(
                                "rendered DOM could not be extracted ({error}); static result kept"
                            ));
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "render=auto fallback to static result");
                    render_warning = Some(error.to_string());
                }
            }
        }
        if render_warning.is_some() {
            source.render_mode = RenderMode::StaticFallback;
            source.render_warning = render_warning;
        }

        let fetched_at = Utc::now();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let content_type = source
            .response
            .content_type
            .as_deref()
            .map(content::base_media_type)
            .unwrap_or_else(|| source.media.as_str().to_string());
        let text_hash = content_hash(extraction.main.markdown.as_bytes());
        let content_key = extract::near_duplicate_key(&extraction.main.markdown);
        let render_mode = source.render_mode.as_str().to_string();
        let records = extraction
            .records
            .as_ref()
            .map(|outcome| outcome.records.clone());
        let record_count = extraction
            .records
            .as_ref()
            .map(|outcome| outcome.record_count)
            .unwrap_or(0);
        let field_errors = extraction
            .records
            .as_ref()
            .map(|outcome| outcome.field_errors.len())
            .unwrap_or(0);
        let links = extraction.main.links.clone().unwrap_or_default();

        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.fetched_at = fetched_at;
        record.status = Some(source.response.status);
        record.content_type = Some(content_type.clone());
        record.charset = Some(source.decoded.charset.clone());
        record.etag = source.response.etag.clone();
        record.last_modified = source.response.last_modified.clone();
        record.raw_hash = Some(source.raw_hash.clone());
        record.text_hash = Some(text_hash.clone());
        record.title = extraction.main.title.clone();
        record.lang = extraction.main.lang.clone();
        record.render_mode = render_mode.clone();
        record.outcome = "ok".to_string();
        record.duration_ms = duration_ms;
        record.mode = label.to_string();
        record.bytes = Some(source.response.bytes);
        record.index_text = Some(IndexText::replace(
            self.index_text(&extraction.main.markdown),
        ));
        record.blob = Some(BlobRow {
            hash: source.raw_hash.clone(),
            size: source.response.body.len() as i64,
            content_type: Some(content_type.clone()),
            created_at: fetched_at,
        });

        if let (Some(schema), Some(outcome)) = (schema, extraction.records.as_ref()) {
            // One shape for `extract` and crawl results: `crawl_results` reads
            // `records` out of the stored payload.
            let payload = ExtractPayload {
                url: page.raw().to_string(),
                final_url: source.response.final_url.to_string(),
                status: source.response.status,
                content_type: content_type.clone(),
                records: outcome.records.clone(),
                record_count: outcome.record_count,
                field_errors: outcome.field_errors.clone(),
                truncated: outcome.truncated,
                limit_hit: if outcome.truncated {
                    Some("max_records".to_string())
                } else {
                    None
                },
                truncated_fields: 0,
                render_mode: render_mode.clone(),
                render_quality: None,
                render_warning: None,
                cached: false,
                fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                duration_ms,
            };
            let result_json = serde_json::to_string(&payload).map_err(WidowError::internal)?;
            record.extraction = Some(ExtractionRow {
                profile_hash: schema.validated.hash.clone(),
                result_hash: content_hash(result_json.as_bytes()),
                result_json,
                record_count: outcome.record_count as i64,
                created_at: fetched_at,
            });
            record.profile = Some(ProfileRow {
                hash: schema.validated.hash.clone(),
                name: None,
                spec_json: schema.validated.canonical_json.clone(),
                created_at: fetched_at,
            });
        }

        self.store.db.record_fetch(record).await?;

        Ok(CrawlPage {
            url: page.canonical().to_string(),
            final_url: source.response.final_url.to_string(),
            status: source.response.status,
            title: extraction.main.title,
            raw_hash: source.raw_hash,
            text_hash,
            content_key,
            links,
            links_truncated: extraction.main.links_truncated,
            render_mode,
            records,
            record_count,
            field_errors,
            bytes: source.response.bytes,
            duration_ms,
        })
    }

    /// One monitor check: conditional GET, the 304 path, extraction in the
    /// monitor's mode with `ignore_selectors` removed before conversion, and an
    /// optional schema evaluated against the same parsed tree.
    ///
    /// Deliberately **not** `crawl_page`: a crawl fetches unconditionally by
    /// design, and B5-1 requires the conditional path.
    ///
    /// A 304 only means "unchanged for this monitor" when **both** hold: the
    /// previous check was static (a rendered check leaves the origin's static
    /// validator on `pages` while its snapshot is the serialized DOM), and the
    /// validator's representation is the one the previous snapshot saw.
    /// `previous_raw_hash` is the second guard; `allow_not_modified` is the
    /// first. When either fails, the body is fetched unconditionally.
    pub async fn monitor_fetch(
        &self,
        url: &str,
        mode: Mode,
        ignore_selectors: &[scraper::Selector],
        schema: Option<MonitorSpec<'_>>,
        previous_raw_hash: Option<&str>,
        allow_not_modified: bool,
    ) -> Result<MonitorPage> {
        let started = Instant::now();
        let page = PageUrl::parse(url)?;
        let label = "monitor";
        let conditional = if allow_not_modified {
            self.conditional_for(&page).await?
        } else {
            // No validator at all: an origin must not answer 304 to this.
            Conditional::default()
        };

        let mut source = match self
            .acquire_static(&page, label, started, conditional, None, ContentScope::Page)
            .await?
        {
            StaticSource::Document(document) => document,
            StaticSource::NotModified(response) => {
                let duration = started.elapsed();
                let recorded = self
                    .record_not_modified(&page, label, &response, duration)
                    .await?;
                let page_now = self.store.db.page(page.hash()).await?;
                let validator_matches_history = match (
                    previous_raw_hash,
                    page_now.as_ref().and_then(|row| row.raw_hash.as_deref()),
                ) {
                    (Some(previous), Some(current)) => previous == current,
                    _ => false,
                };
                if allow_not_modified && validator_matches_history {
                    return Ok(MonitorPage {
                        final_url: response.final_url.to_string(),
                        status: 304,
                        fetch_event_id: recorded.fetch_event_id,
                        raw_hash: previous_raw_hash.unwrap_or_default().to_string(),
                        render_mode: "static".to_string(),
                        not_modified: true,
                        markdown: String::new(),
                        records: None,
                        records_json: None,
                        field_errors: 0,
                    });
                }
                // The monitor's history predates the validator (another tool or
                // an earlier check stored a newer representation). Fetch the body
                // so "no change" is never inferred from the wrong baseline.
                match self
                    .acquire_static(
                        &page,
                        label,
                        started,
                        Conditional::default(),
                        None,
                        ContentScope::Page,
                    )
                    .await?
                {
                    StaticSource::Document(document) => document,
                    StaticSource::NotModified(_) => {
                        return Err(WidowError::internal(
                            "origin answered 304 to an unconditional request",
                        ));
                    }
                }
            }
        };

        let options = ExtractOptions {
            mode,
            max_chars: self.config.extract.max_chars,
            // A snapshot does not want links: they are markup identity, not
            // content, and would make every check differ by absolute URLs.
            max_links: 0,
            include_links: false,
            ignore_selectors: ignore_selectors.to_vec(),
        };
        let eval_options = spec_eval::EvalOptions {
            max_records: self.config.extract.max_records,
        };
        let spec_pair = schema.map(|schema| (schema.compiled, &eval_options));
        let mut extraction = if source.media.is_markup() {
            extract::extract_page(
                &source.decoded.text,
                &source.response.final_url,
                &options,
                spec_pair,
            )?
        } else {
            if schema.is_some() {
                tracing::warn!(
                    url = %log_target(&page, self.config.logging.redact_query),
                    "monitor has a schema but the response is not markup; records are not evaluated"
                );
            }
            extract::PageExtraction {
                main: extract::extract_plain(&source.decoded.text, &options, false),
                records: None,
                dom_parses: 0,
            }
        };

        // A monitor uses `auto`: render only when the static result is thin, so
        // a script-driven page is monitored on its rendered content (B5-10).
        if source.render_mode == RenderMode::Static
            && self.auto_wants_render(&page, &source, extraction.main.word_count)
        {
            // Unlike `fetch_page`, a monitor cannot show a visible fallback: a
            // static snapshot of a page that needed rendering would silently
            // monitor the wrong content. Fail the check instead.
            let rendered = self
                .acquire_rendered(&page, label, started, None)
                .await
                .map_err(|error| {
                    WidowError::RenderUnavailable(format!(
                        "monitor check needs rendering for {url} but it failed: {error}"
                    ))
                })?;
            extraction = extract::extract_page(
                &rendered.decoded.text,
                &rendered.response.final_url,
                &options,
                spec_pair,
            )?;
            source = rendered;
        }

        let fetched_at = Utc::now();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let content_type = source
            .response
            .content_type
            .as_deref()
            .map(content::base_media_type)
            .unwrap_or_else(|| source.media.as_str().to_string());
        let render_mode = source.render_mode.as_str().to_string();
        let records = extraction
            .records
            .as_ref()
            .map(|outcome| outcome.records.clone());
        let field_errors = extraction
            .records
            .as_ref()
            .map(|outcome| outcome.field_errors.len())
            .unwrap_or(0);
        // Deterministic (sorted keys) so an unchanged extraction keeps its hash.
        let records_json = records
            .as_ref()
            .map(|records| serde_json::to_string(records).map_err(WidowError::internal))
            .transpose()?;

        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.fetched_at = fetched_at;
        record.status = Some(source.response.status);
        record.content_type = Some(content_type.clone());
        record.charset = Some(source.decoded.charset.clone());
        record.etag = source.response.etag.clone();
        record.last_modified = source.response.last_modified.clone();
        record.raw_hash = Some(source.raw_hash.clone());
        record.text_hash = Some(content_hash(extraction.main.markdown.as_bytes()));
        record.title = extraction.main.title.clone();
        record.lang = extraction.main.lang.clone();
        record.render_mode = render_mode.clone();
        record.outcome = "ok".to_string();
        record.duration_ms = duration_ms;
        record.mode = label.to_string();
        record.bytes = Some(source.response.bytes);
        record.index_text = Some(IndexText::replace(
            self.index_text(&extraction.main.markdown),
        ));
        record.blob = Some(BlobRow {
            hash: source.raw_hash.clone(),
            size: source.response.body.len() as i64,
            content_type: Some(content_type.clone()),
            created_at: fetched_at,
        });
        if let (Some(schema), Some(records_json)) = (schema, records_json.as_ref()) {
            record.extraction = Some(ExtractionRow {
                profile_hash: schema.validated.hash.clone(),
                result_hash: content_hash(records_json.as_bytes()),
                result_json: records_json.clone(),
                record_count: records.as_ref().map_or(0, Vec::len) as i64,
                created_at: fetched_at,
            });
            record.profile = Some(ProfileRow {
                hash: schema.validated.hash.clone(),
                name: None,
                spec_json: schema.validated.canonical_json.clone(),
                created_at: fetched_at,
            });
        }
        let recorded = self.store.db.record_fetch(record).await?;

        Ok(MonitorPage {
            final_url: source.response.final_url.to_string(),
            status: source.response.status,
            fetch_event_id: recorded.fetch_event_id,
            raw_hash: source.raw_hash,
            render_mode,
            not_modified: false,
            markdown: extraction.main.markdown,
            records,
            records_json,
            field_errors,
        })
    }

    async fn record_failure(
        &self,
        page: &PageUrl,
        label: &str,
        duration: Duration,
        error: &WidowError,
    ) {
        tracing::info!(
            url = %log_target(page, self.config.logging.redact_query),
            code = error.code().as_str(),
            "fetch_page failed"
        );
        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        // Keep the observed status for failures too, so `crawl_results` can show
        // the 404 rather than a null status.
        record.status = error.http_status();
        record.outcome = format!("error:{}", error.code().as_str());
        record.duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;
        record.mode = label.to_string();
        record.error = Some((error.code().as_str().to_string(), error.to_string()));
        if let Err(write_error) = self.store.db.record_fetch(record).await {
            tracing::warn!(error = %write_error, "could not record failed fetch event");
        }
    }

    async fn record_not_modified(
        &self,
        page: &PageUrl,
        label: &str,
        response: &crate::fetch::http::RawResponse,
        duration: Duration,
    ) -> Result<crate::store::db::Recorded> {
        let mut record = FetchRecord::new(page.raw(), page.canonical(), page.hash());
        record.site = page.site();
        record.status = Some(304);
        record.etag = response.etag.clone();
        record.last_modified = response.last_modified.clone();
        record.outcome = "not_modified".to_string();
        record.duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;
        record.mode = label.to_string();
        record.bytes = Some(0);
        record.render_mode = "static".to_string();
        self.store.db.record_fetch(record).await
    }
}

/// Content-addressed key for the extraction shape. Two calls that ask for a
/// different mode, cap or link set are different results and must not share a
/// cache row.
pub fn profile_hash_for(mode: Mode, max_chars: usize, include_links: bool) -> String {
    content_hash(
        format!(
            "fetch_page/v1/{}/{}/{}",
            mode.as_str(),
            max_chars,
            include_links
        )
        .as_bytes(),
    )
}

/// One extraction path for fetched and rendered documents.
fn extraction_for(
    source: &FetchedDocument,
    options: &ExtractOptions,
) -> Result<extract::Extraction> {
    if source.media.is_markup() {
        extract::extract_html(&source.decoded.text, &source.response.final_url, options)
    } else {
        Ok(extract::extract_plain(
            &source.decoded.text,
            options,
            options.include_links,
        ))
    }
}

/// A cached result produced under a different render policy is not a
/// substitute for the current request: `never` must not serve rendered content
/// and `always` must not serve a static result.
fn cached_mode_acceptable(policy: RenderPolicy, mode: &str) -> bool {
    match policy {
        RenderPolicy::Never => matches!(mode, "static" | "static_fallback"),
        RenderPolicy::Always => matches!(mode, "rendered" | "rendered_partial"),
        RenderPolicy::Auto => true,
    }
}

/// Only transport, HTTP and size/type failures are retryable through the lite
/// endpoint. Robots, SSRF, scheme and URL refusals are final for the call.
fn search_retryable(error: &WidowError) -> bool {
    matches!(
        error,
        WidowError::Http { .. }
            | WidowError::Dns { .. }
            | WidowError::Tls(_)
            | WidowError::Timeout(_)
            | WidowError::TooLarge { .. }
            | WidowError::UnsupportedContentType { .. }
    )
}

/// Path segments that mark a verification/login interstitial instead of the
/// requested content. Matched as whole, case-insensitive path segments.
const INTERSTITIAL_SEGMENTS: &[&str] = &[
    "account-verification",
    "verification",
    "verify",
    "challenge",
    "captcha",
    "login",
    "sign-in",
    "signin",
    "security-check",
    "checkpoint",
    "auth",
    "authorize",
    "validate",
];

/// Below this word count a verification-path page is treated as an
/// interstitial (the BUYALFA sample was 28 words).
const INTERSTITIAL_MAX_WORDS: usize = 50;

/// `interstitial_suspect` only for the whole conjunction (IMP-DEC-025):
/// status 200, the final URL drifted from the request in host or path, the
/// final path has a verification/login segment, and the page is very short.
/// A direct login page (no drift) and a short article (no segment) never warn.
fn interstitial_warning(
    requested: &Url,
    final_url: &Url,
    status: u16,
    word_count: usize,
) -> Option<&'static str> {
    if status != 200 || word_count >= INTERSTITIAL_MAX_WORDS {
        return None;
    }
    let drifted =
        requested.host_str() != final_url.host_str() || requested.path() != final_url.path();
    if !drifted {
        return None;
    }
    let verification_path = final_url.path().split('/').any(|segment| {
        INTERSTITIAL_SEGMENTS
            .iter()
            .any(|known| segment.eq_ignore_ascii_case(known))
    });
    verification_path.then_some("interstitial_suspect")
}

/// A cached payload can predate the warning field (or its heuristic): derive it
/// from the stored 200 response shape instead of trusting a missing field.
fn cached_content_warning(payload: &FetchPagePayload) -> Option<String> {
    let requested = Url::parse(&payload.url).ok()?;
    let final_url = Url::parse(&payload.final_url).ok()?;
    interstitial_warning(&requested, &final_url, payload.status, payload.word_count)
        .map(str::to_string)
}

/// `ok | thin` for rendered results only; the boundary is the same constant and
/// the same word count the payload reports.
fn render_quality_for(mode: RenderMode, word_count: usize) -> Option<String> {
    match mode {
        RenderMode::Rendered | RenderMode::RenderedPartial => Some(
            if word_count < render::auto::THIN_CONTENT_WORDS {
                "thin"
            } else {
                "ok"
            }
            .to_string(),
        ),
        RenderMode::Static | RenderMode::StaticFallback => None,
    }
}

/// Extract's thin signal (ISS-005): the rendered page's visible-word count, not
/// the record text. An approximation, as documented.
fn extract_render_quality(mode: RenderMode, html: &str) -> Option<String> {
    match mode {
        RenderMode::Rendered | RenderMode::RenderedPartial => Some(
            if render::auto::rough_visible_word_count(html) < render::auto::THIN_CONTENT_WORDS {
                "thin"
            } else {
                "ok"
            }
            .to_string(),
        ),
        RenderMode::Static | RenderMode::StaticFallback => None,
    }
}

/// `format: auto`: the response media type first, then a bounded sniff of the
/// decoded body. `application/octet-stream` never reaches here (the tabular
/// scope refuses it), so a sniff failure is a real `unsupported_content_type`.
fn resolve_row_format(source: &FetchedDocument) -> Result<rows::RowFormat> {
    use content::MediaKind;

    let named = match source.media {
        MediaKind::Json => Some(rows::RowFormat::Json),
        MediaKind::Csv => Some(rows::RowFormat::Csv),
        MediaKind::Tsv => Some(rows::RowFormat::Tsv),
        MediaKind::Html | MediaKind::Xhtml => Some(rows::RowFormat::HtmlTable),
        MediaKind::Text | MediaKind::Xml => None,
    };
    if let Some(format) = named {
        return Ok(format);
    }
    rows::sniff_format(&source.decoded.text).ok_or_else(|| WidowError::UnsupportedContentType {
        content_type: source
            .response
            .content_type
            .as_deref()
            .map(content::base_media_type)
            .unwrap_or_else(|| source.media.as_str().to_string()),
        processed: content::tabular_processed_types(),
    })
}

/// `max_field_chars` over the returned rows, and `returned` re-derived.
fn apply_rows_caps(payload: &mut ExtractRowsPayload, max_field_chars: usize) {
    payload.truncated_fields = rows::cap_field_values(&mut payload.records, max_field_chars);
    payload.returned = payload.records.len();
}

/// `truncated`/`limit_hit` for a payload whose caps were just applied.
fn set_rows_flags(payload: &mut ExtractRowsPayload, limit_truncated: bool) {
    payload.truncated = limit_truncated || payload.truncated_fields > 0;
    payload.limit_hit = if limit_truncated {
        Some("limit".to_string())
    } else if payload.truncated_fields > 0 {
        Some("max_field_chars".to_string())
    } else {
        None
    };
}

/// Re-derive a cached row set for this call: the stored records are uncapped and
/// the stored `limit` may differ, so both the cap and the truncation flag are
/// recomputed. `stored_rows` has already refused a cache that was cut short of
/// this call's limit.
fn finish_cached_rows(payload: &mut ExtractRowsPayload, max_field_chars: usize, limit: usize) {
    let stored_limit_hit = payload.limit_hit.as_deref() == Some("limit");
    payload.records.truncate(limit);
    apply_rows_caps(payload, max_field_chars);
    set_rows_flags(
        payload,
        stored_limit_hit || payload.row_count > payload.records.len(),
    );
}

/// Re-derive the presentation caps for a payload (fresh or cached). The stored
/// records are the full evaluation; only the returned copy is cut.
fn apply_extract_caps(payload: &mut ExtractPayload, max_field_chars: usize, max_chars: usize) {
    let max_records_truncated = payload.records.len() < payload.record_count;
    let truncated = extract::truncate_records(
        std::mem::take(&mut payload.records),
        max_field_chars,
        max_chars,
    );
    payload.records = truncated.records;
    payload.truncated_fields = truncated.truncated_fields;
    payload.truncated =
        max_records_truncated || truncated.budget_truncated || truncated.truncated_fields > 0;
    payload.limit_hit = if max_records_truncated {
        Some("max_records")
    } else if truncated.budget_truncated {
        Some("max_chars")
    } else if truncated.truncated_fields > 0 {
        Some("max_field_chars")
    } else {
        None
    }
    .map(str::to_string);
}

/// Pre-M02 cached payloads stored `"links": null`; a list that is always a list
/// must still deserialize them.
fn deserialize_nullable_links<'de, D>(deserializer: D) -> std::result::Result<Vec<Link>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<Link>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Metadata-only text block for `fetch_page include_content: false`: one line,
/// no body, no Markdown front matter.
pub fn render_metadata_summary(result: &FetchPageResult) -> String {
    let payload = &result.payload;
    let title = payload
        .title
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "url: {} status: {} title: {} words: {} render_mode: {}\n",
        payload.final_url, payload.status, title, payload.word_count, payload.render_mode
    )
}

/// Render the text content block: YAML-ish front matter plus the Markdown.
pub fn render_text(result: &FetchPageResult) -> String {
    let payload = &result.payload;
    let mut out = String::with_capacity(result.markdown.len() + 256);
    out.push_str("---\n");
    out.push_str(&format!("url: {}\n", yaml_scalar(&payload.final_url)));
    out.push_str(&format!(
        "title: {}\n",
        yaml_scalar(payload.title.as_deref().unwrap_or(""))
    ));
    out.push_str(&format!("fetched_at: {}\n", payload.fetched_at));
    out.push_str(&format!("words: {}\n", payload.word_count));
    out.push_str(&format!("render_mode: {}\n", payload.render_mode));
    if let Some(quality) = &payload.render_quality {
        out.push_str(&format!("render_quality: {quality}\n"));
    }
    if let Some(warning) = &payload.render_warning {
        out.push_str(&format!("render_warning: {}\n", yaml_scalar(warning)));
    }
    if let Some(warning) = &payload.content_warning {
        out.push_str(&format!("content_warning: {}\n", yaml_scalar(warning)));
    }
    out.push_str(&format!("cached: {}\n", payload.cached));
    out.push_str("content_status: untrusted-data\n");
    out.push_str("---\n\n");
    out.push_str(&result.markdown);
    out
}

fn yaml_scalar(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    format!("\"{}\"", cleaned.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render the `search` text block: a numbered list, or a single `no results`
/// line. The snippets are untrusted data; the caller never gets them as prose
/// instructions.
pub fn render_search_text(payload: &SearchPayload) -> String {
    if payload.results.is_empty() {
        return "no results\n".to_string();
    }
    let mut out = String::new();
    for (index, result) in payload.results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {}\n",
            index + 1,
            result.title,
            result.url
        ));
        if !result.snippet.is_empty() {
            out.push_str(&format!("   {}\n", result.snippet));
        }
    }
    out
}

/// Render the `fetch_feed` text block: one line per item, the full data lives in
/// `structuredContent`.
pub fn render_feed_text(payload: &FeedPayload) -> String {
    let mut out = String::new();
    let title = payload.title.as_deref().unwrap_or("(untitled feed)");
    out.push_str(&format!(
        "{title} — {} items ({})\n",
        payload.item_count, payload.feed_type
    ));
    if payload.items.is_empty() {
        out.push_str("no items\n");
        return out;
    }
    for (index, item) in payload.items.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {}\n",
            index + 1,
            item.title.as_deref().unwrap_or("(untitled)"),
            item.url.as_deref().unwrap_or("(no link)")
        ));
        if let Some(published) = &item.published {
            out.push_str(&format!("   {published}\n"));
        }
    }
    if payload.truncated {
        out.push_str(&format!(
            "truncated: {} ({} of {} items shown)\n",
            payload.limit_hit.as_deref().unwrap_or("limit"),
            payload.items.len(),
            payload.item_count
        ));
    }
    out
}

/// Render the `extract` text block: a compact preview of the first rows plus the
/// error summary. The full data lives in `structuredContent`.
pub fn render_extract_text(payload: &ExtractPayload) -> String {
    const MAX_ROWS: usize = 20;
    const MAX_ERRORS: usize = 5;

    let mut out = String::with_capacity(512);
    out.push_str("---\n");
    out.push_str(&format!("url: {}\n", yaml_scalar(&payload.final_url)));
    out.push_str(&format!("records: {}\n", payload.record_count));
    out.push_str(&format!("returned: {}\n", payload.records.len()));
    out.push_str(&format!("cached: {}\n", payload.cached));
    out.push_str(&format!("render_mode: {}\n", payload.render_mode));
    if let Some(warning) = &payload.render_warning {
        out.push_str(&format!("render_warning: {}\n", yaml_scalar(warning)));
    }
    out.push_str(&format!("truncated: {}\n", payload.truncated));
    out.push_str(&format!("truncated_fields: {}\n", payload.truncated_fields));
    if let Some(quality) = &payload.render_quality {
        out.push_str(&format!("render_quality: {quality}\n"));
    }
    out.push_str(&format!("field_errors: {}\n", payload.field_errors.len()));
    out.push_str("content_status: untrusted-data\n");
    out.push_str("---\n\n");

    push_record_preview(&mut out, &payload.records, MAX_ROWS);

    if !payload.field_errors.is_empty() {
        out.push('\n');
        for error in payload.field_errors.iter().take(MAX_ERRORS) {
            let scope = match (&error.record, &error.field) {
                (Some(record), Some(field)) => format!("record {record} field {field}"),
                (Some(record), None) => format!("record {record}"),
                (None, _) => "page".to_string(),
            };
            out.push_str(&format!("error: {scope} {}\n", error.code.as_str()));
        }
        if payload.field_errors.len() > MAX_ERRORS {
            out.push_str(&format!(
                "... ({} more errors in structuredContent)\n",
                payload.field_errors.len() - MAX_ERRORS
            ));
        }
    }
    out
}

/// The text-block view of `extract_rows`: the same front-matter and preview
/// conventions as `extract`, with the resolved `format` and row counters.
pub fn render_extract_rows_text(payload: &ExtractRowsPayload) -> String {
    const MAX_ROWS: usize = 20;
    const MAX_ERRORS: usize = 5;

    let mut out = String::with_capacity(512);
    out.push_str("---\n");
    out.push_str(&format!("url: {}\n", yaml_scalar(&payload.final_url)));
    out.push_str(&format!("format: {}\n", payload.format));
    out.push_str(&format!("rows: {}\n", payload.row_count));
    out.push_str(&format!("returned: {}\n", payload.returned));
    out.push_str(&format!("cached: {}\n", payload.cached));
    out.push_str(&format!("render_mode: {}\n", payload.render_mode));
    out.push_str(&format!("truncated: {}\n", payload.truncated));
    if let Some(hit) = &payload.limit_hit {
        out.push_str(&format!("limit_hit: {hit}\n"));
    }
    out.push_str(&format!("truncated_fields: {}\n", payload.truncated_fields));
    out.push_str(&format!("field_errors: {}\n", payload.field_errors.len()));
    out.push_str("content_status: untrusted-data\n");
    out.push_str("---\n\n");

    push_record_preview(&mut out, &payload.records, MAX_ROWS);

    if !payload.field_errors.is_empty() {
        out.push('\n');
        for error in payload.field_errors.iter().take(MAX_ERRORS) {
            let scope = match (error.row, &error.field) {
                (0, _) => "table".to_string(),
                (row, Some(field)) => format!("row {row} field {field}"),
                (row, None) => format!("row {row}"),
            };
            out.push_str(&format!("error: {scope} {}\n", error.reason));
        }
        let hidden = payload.field_errors.len().saturating_sub(MAX_ERRORS);
        if hidden > 0 || payload.field_errors_truncated {
            out.push_str(&format!(
                "... (more errors in structuredContent{})\n",
                if payload.field_errors_truncated {
                    "; the list itself is capped at 20"
                } else {
                    ""
                }
            ));
        }
    }
    out
}

/// A bounded column/row preview shared by the two record tools.
fn push_record_preview(
    out: &mut String,
    records: &[serde_json::Map<String, serde_json::Value>],
    max_rows: usize,
) {
    let mut columns: Vec<&str> = Vec::new();
    for record in records {
        for key in record.keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    if columns.is_empty() {
        out.push_str("(no records)\n");
        return;
    }
    out.push_str(&columns.join(" | "));
    out.push('\n');
    for record in records.iter().take(max_rows) {
        let row: Vec<String> = columns
            .iter()
            .map(|column| match record.get(*column) {
                Some(value) => preview_value(value),
                None => String::new(),
            })
            .collect();
        out.push_str(&row.join(" | "));
        out.push('\n');
    }
    if records.len() > max_rows {
        out.push_str(&format!(
            "... ({} more rows in structuredContent)\n",
            records.len() - max_rows
        ));
    }
}

fn preview_value(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let text = text.replace(['\n', '\r', '\t'], " ");
    let text = text.replace('|', "\\|");
    if text.chars().count() > 120 {
        let clipped: String = text.chars().take(120).collect();
        format!("{clipped}…")
    } else {
        text
    }
}

/// URL for a log line from an unparsed string. Anything that is not a valid
/// top-level http(s) URL is replaced entirely, never echoed.
pub fn log_url(url: &str, redact_query: bool) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| PageUrl::parse(url.as_str()).ok())
        .map(|page| log_target(&page, redact_query))
        .unwrap_or_else(|| "<unparsable url>".to_string())
}

/// URL for a log line: query strings are replaced when `logging.redact_query`
/// is on (the default), because they can carry tokens or personal data.
pub fn log_target(page: &PageUrl, redact_query: bool) -> String {
    let canonical = page.canonical();
    if !redact_query {
        return canonical.to_string();
    }
    match canonical.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => canonical.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_hash_separates_extraction_shapes() {
        let base = profile_hash_for(Mode::Main, 200_000, false);
        assert_eq!(base, profile_hash_for(Mode::Main, 200_000, false));
        assert_ne!(base, profile_hash_for(Mode::Full, 200_000, false));
        assert_ne!(base, profile_hash_for(Mode::Main, 1_000, false));
        assert_ne!(base, profile_hash_for(Mode::Main, 200_000, true));
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn render_quality_boundary_is_199_thin_200_ok() {
        assert_eq!(
            render_quality_for(RenderMode::Rendered, 199).as_deref(),
            Some("thin")
        );
        assert_eq!(
            render_quality_for(RenderMode::Rendered, 200).as_deref(),
            Some("ok")
        );
        assert_eq!(
            render_quality_for(RenderMode::RenderedPartial, 199).as_deref(),
            Some("thin")
        );
        assert_eq!(render_quality_for(RenderMode::Static, 1), None);
        assert_eq!(render_quality_for(RenderMode::StaticFallback, 1), None);
    }

    fn url(value: &str) -> Url {
        Url::parse(value).expect("test url")
    }

    #[test]
    fn interstitial_warning_requires_the_whole_conjunction() {
        let requested = url("https://shop.example/p/MLM1");
        let verification = url("https://shop.example/gz/account-verification");
        assert_eq!(
            interstitial_warning(&requested, &verification, 200, 28),
            Some("interstitial_suspect")
        );
        // Not a 200: an error status is never the warning's business.
        assert_eq!(
            interstitial_warning(&requested, &verification, 304, 28),
            None
        );
        assert_eq!(
            interstitial_warning(&requested, &verification, 404, 28),
            None
        );
        // No redirect drift: a direct login page is not flagged.
        let login = url("https://shop.example/login");
        assert_eq!(interstitial_warning(&login, &login, 200, 28), None);
        // Drift without a verification segment: a normal redirected article.
        let article = url("https://shop.example/article");
        assert_eq!(interstitial_warning(&requested, &article, 200, 28), None);
        // Drift into the segment but a long body: not an interstitial.
        assert_eq!(
            interstitial_warning(&requested, &verification, 200, 50),
            None
        );
        assert_eq!(
            interstitial_warning(&requested, &verification, 200, 49),
            Some("interstitial_suspect")
        );
        // Whole segments only, case-insensitive; a host drift is drift too.
        let upper = url("https://shop.example/ACCOUNT-VERIFICATION");
        assert_eq!(
            interstitial_warning(&requested, &upper, 200, 3),
            Some("interstitial_suspect")
        );
        let partial = url("https://shop.example/account-verification-extra");
        assert_eq!(interstitial_warning(&requested, &partial, 200, 3), None);
        let other_host = url("https://verify.example/p/MLM1?checkpoint=1");
        assert_eq!(
            interstitial_warning(&requested, &other_host, 200, 3),
            None,
            "a query value is not a path segment"
        );
        let other_host_path = url("https://verify.example/checkpoint");
        assert_eq!(
            interstitial_warning(&requested, &other_host_path, 200, 3),
            Some("interstitial_suspect")
        );
    }

    #[test]
    fn pre_upgrade_cached_payload_regains_the_warning() {
        // A payload cached before this field existed has no `content_warning`;
        // the cache path re-derives it from the stored 200 shape.
        let payload: FetchPagePayload = serde_json::from_value(serde_json::json!({
            "url": "https://www.mercadolibre.com.mx/p/MLM26030984",
            "final_url": "https://www.mercadolibre.com.mx/gz/account-verification",
            "status": 200,
            "content_type": "text/html",
            "title": null,
            "byline": null,
            "site_name": null,
            "published": null,
            "lang": null,
            "markdown_chars": 0,
            "truncated": false,
            "limit_hit": null,
            "render_mode": "static",
            "cached": true,
            "fetched_at": "2026-11-01T00:00:00Z",
            "duration_ms": 1,
            "links_truncated": false,
            "word_count": 28
        }))
        .expect("pre-upgrade payload deserializes");
        assert!(payload.content_warning.is_none());
        assert_eq!(
            cached_content_warning(&payload).as_deref(),
            Some("interstitial_suspect")
        );
    }

    #[test]
    fn extract_top_level_spec_names_the_schema_envelope() {
        let error = serde_json::from_value::<ExtractInput>(serde_json::json!({
            "url": "https://example.com/items",
            "container": ".item",
            "fields": { "title": { "selector": "h2" } }
        }))
        .expect_err("a top-level spec is still rejected");
        let message = error.to_string();
        assert!(message.contains("`schema`"), "{message}");
        assert!(
            message.contains("{url, schema: {container?, fields}}"),
            "{message}"
        );
    }

    #[test]
    fn extract_missing_schema_without_a_top_level_spec_keeps_serde_wording() {
        let error = serde_json::from_value::<ExtractInput>(serde_json::json!({
            "url": "https://example.com/items"
        }))
        .expect_err("schema is required");
        assert!(error.to_string().contains("missing field"), "{error}");
    }

    #[test]
    fn extract_envelope_still_deserializes() {
        let input: ExtractInput = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/items",
            "schema": { "fields": { "title": { "selector": "h1" } } }
        }))
        .expect("the documented envelope");
        assert_eq!(input.url, "https://example.com/items");
        assert!(input.schema.get("fields").is_some());
    }

    #[test]
    fn extract_render_quality_boundary_is_199_thin_200_ok() {
        let page =
            |words: usize| format!("<html><body><p>{}</p></body></html>", "word ".repeat(words));
        assert_eq!(
            extract_render_quality(RenderMode::Rendered, &page(199)).as_deref(),
            Some("thin")
        );
        assert_eq!(
            extract_render_quality(RenderMode::Rendered, &page(200)).as_deref(),
            Some("ok")
        );
        assert_eq!(
            extract_render_quality(RenderMode::RenderedPartial, &page(1)).as_deref(),
            Some("thin")
        );
        assert_eq!(extract_render_quality(RenderMode::Static, &page(1)), None);
        assert_eq!(
            extract_render_quality(RenderMode::StaticFallback, &page(1)),
            None
        );
    }

    #[test]
    fn a_pre_m02_payload_with_null_links_deserializes() {
        let json = r##"{
            "payload": {
                "url": "https://example.com/a",
                "final_url": "https://example.com/a",
                "status": 200,
                "content_type": "text/html",
                "title": null,
                "byline": null,
                "site_name": null,
                "published": null,
                "lang": null,
                "markdown_chars": 5,
                "truncated": false,
                "limit_hit": null,
                "render_mode": "rendered",
                "cached": false,
                "fetched_at": "2026-10-25T00:00:00Z",
                "duration_ms": 3,
                "links": null,
                "links_truncated": false,
                "word_count": 1
            },
            "markdown": "# Hello"
        }"##;
        let result: FetchPageResult = serde_json::from_str(json).unwrap();
        assert!(result.payload.links.is_empty());
        assert!(!result.payload.links_requested);
        assert!(result.payload.render_quality.is_none());
        let serialized = serde_json::to_value(&result.payload).unwrap();
        assert_eq!(serialized["links"], serde_json::json!([]));
    }

    #[test]
    fn log_target_redacts_query_strings_by_default() {
        let page = PageUrl::parse("https://example.com/a/b?token=secret&page=2").unwrap();
        assert_eq!(
            log_target(&page, true),
            "https://example.com/a/b?<redacted>"
        );
        assert_eq!(
            log_target(&page, false),
            "https://example.com/a/b?page=2&token=secret"
        );
        let plain = PageUrl::parse("https://example.com/a").unwrap();
        assert_eq!(log_target(&plain, true), "https://example.com/a");
    }

    #[test]
    fn extract_preview_is_compact_and_marks_untrusted_data() {
        let payload = ExtractPayload {
            url: "https://example.com/items".into(),
            final_url: "https://example.com/items".into(),
            status: 200,
            content_type: "text/html".into(),
            records: vec![
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                    r#"{"name":"Lantern","price":39.9}"#,
                )
                .unwrap(),
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                    r#"{"name":"Stove","price":1299}"#,
                )
                .unwrap(),
            ],
            record_count: 2,
            field_errors: Vec::new(),
            truncated: false,
            limit_hit: None,
            truncated_fields: 0,
            render_mode: "static".into(),
            render_quality: None,
            render_warning: None,
            cached: false,
            fetched_at: "2026-10-26T00:00:00Z".into(),
            duration_ms: 5,
        };
        let text = render_extract_text(&payload);
        assert!(text.starts_with("---\n"));
        assert!(text.contains("records: 2"));
        assert!(text.contains("content_status: untrusted-data"));
        assert!(text.contains("name | price"));
        assert!(text.contains("Lantern | 39.9"));
    }

    #[test]
    fn rendered_text_has_front_matter_and_untrusted_marker() {
        let result = FetchPageResult {
            payload: FetchPagePayload {
                url: "https://example.com/a".into(),
                final_url: "https://example.com/a".into(),
                status: 200,
                content_type: "text/html".into(),
                title: Some("A \"quoted\" title".into()),
                byline: None,
                site_name: None,
                published: None,
                lang: Some("en".into()),
                markdown_chars: 5,
                markdown: None,
                truncated: false,
                limit_hit: None,
                render_mode: "static".into(),
                render_warning: None,
                cached: false,
                fetched_at: "2026-10-25T00:00:00Z".into(),
                duration_ms: 3,
                links: Vec::new(),
                links_requested: false,
                links_truncated: false,
                render_quality: None,
                content_warning: None,
                word_count: 1,
            },
            markdown: "# Hello".into(),
        };
        let text = render_text(&result);
        assert!(text.starts_with("---\n"));
        assert!(text.contains("title: \"A \\\"quoted\\\" title\""));
        assert!(text.contains("content_status: untrusted-data"));
        assert!(text.ends_with("# Hello"));
    }
}
