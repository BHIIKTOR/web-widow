//! `corpus_export` (IMP-DEC-035): a machine-readable corpus of the store.
//!
//! Two shapes, one row set: **JSONL** (one page per line) and **markdown** (a
//! directory with `index.md` and `pages/<url_hash>.md`). A page is a row only
//! when its latest fetch succeeded and it has stored text; a page whose text is
//! absent is skipped and counted, never emitted with an invented body. Monitors
//! and recipes are **not** corpus rows: monitor history lives in
//! `monitor_changes`/`changes`, and the recipe library is a configuration
//! surface over `profiles`.
//!
//! Path safety is structural: the name is a validated slug, the filename is
//! generated here, the target lives under `<data_dir>/exports/` (created 0700),
//! creation is `create_new` (never overwrite), and the bytes go to a dot-temp
//! first and are renamed only on success.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Paths;
use crate::core::error::{Result, WidowError};
use crate::core::truncate::truncate_chars;
use crate::store::Store;
use crate::store::corpus::CorpusPageRow;

/// Per-row text cap (`text` and the embedded extraction).
pub const TEXT_CAP_CHARS: usize = 200_000;
/// Rows returned when the caller is silent.
pub const DEFAULT_LIMIT: usize = 1_000;
/// Hard ceiling for `limit`.
pub const MAX_LIMIT: usize = 10_000;
/// Byte floor for `max_bytes` (1 MiB).
pub const MIN_MAX_BYTES: u64 = 1 << 20;
/// Byte ceiling for `max_bytes` (1 GiB).
pub const MAX_MAX_BYTES: u64 = 1 << 30;
/// `max_bytes` default (256 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 256 << 20;
/// Pages fetched per keyset batch.
const BATCH: usize = 200;

fn slug_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").expect("static export-name pattern")
    })
}

/// `jsonl` or `markdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Markdown,
}

impl ExportFormat {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "jsonl" => Ok(Self::Jsonl),
            "markdown" => Ok(Self::Markdown),
            other => Err(WidowError::SchemaInvalid {
                pointer: "/format".to_string(),
                message: format!("unknown export format `{other}`: expected jsonl or markdown"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Markdown => "markdown",
        }
    }
}

/// `corpus_export` input.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CorpusExportInput {
    /// `jsonl` (one page per line) or `markdown` (a directory with `index.md`).
    pub format: String,
    /// File/directory name slug: `[a-z0-9][a-z0-9._-]{0,63}`.
    pub name: String,
    /// Only pages whose canonical site equals this value.
    #[serde(default)]
    pub site: Option<String>,
    /// Only pages last fetched at or after this RFC3339 instant.
    #[serde(default)]
    pub since: Option<String>,
    /// Records written, 1..=10000 (default 1000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include the page text (default true).
    #[serde(default)]
    pub include_text: Option<bool>,
    /// Include the latest extraction payload (default true).
    #[serde(default)]
    pub include_extraction: Option<bool>,
    /// Byte budget, 1 MiB..=1 GiB (default 256 MiB).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// The filters a payload and `index.md` echo back.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CorpusFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    pub limit: usize,
    pub include_text: bool,
    pub include_extraction: bool,
    pub max_bytes: u64,
}

/// `corpus_export` result: the path and the counts, never the corpus itself.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CorpusExportPayload {
    pub format: String,
    pub path: String,
    pub records: usize,
    pub skipped_no_text: usize,
    pub bytes: u64,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_hit: Option<String>,
    pub generated_at: String,
    pub filters: CorpusFilters,
}

/// A validated export request.
#[derive(Debug, Clone)]
pub struct ExportPlan {
    pub format: ExportFormat,
    pub name: String,
    pub site: Option<String>,
    /// The caller's `since`, normalized to RFC3339 UTC seconds for the payload.
    pub since_display: Option<String>,
    /// The same instant in the store's timestamp format (lexicographic compare).
    pub since_stored: Option<String>,
    pub limit: usize,
    pub include_text: bool,
    pub include_extraction: bool,
    pub max_bytes: u64,
}

impl ExportPlan {
    pub fn validate(input: CorpusExportInput) -> Result<Self> {
        let format = ExportFormat::parse(&input.format)?;
        if !slug_pattern().is_match(&input.name) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/name".to_string(),
                message: format!(
                    "`{}` is not an export name: use lowercase `[a-z0-9][a-z0-9._-]{{0,63}}`",
                    input.name
                ),
            });
        }
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/limit".to_string(),
                message: format!("limit must be 1..={MAX_LIMIT}, got {limit}"),
            });
        }
        let max_bytes = input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        if !(MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&max_bytes) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/max_bytes".to_string(),
                message: format!(
                    "max_bytes must be {MIN_MAX_BYTES}..={MAX_MAX_BYTES} (1 MiB..=1 GiB), got {max_bytes}"
                ),
            });
        }
        let since = match input.since.as_deref() {
            Some(raw) => {
                let parsed = DateTime::parse_from_rfc3339(raw).map_err(|error| {
                    WidowError::SchemaInvalid {
                        pointer: "/since".to_string(),
                        message: format!("`{raw}` is not an RFC3339 instant: {error}"),
                    }
                })?;
                Some(parsed.with_timezone(&Utc))
            }
            None => None,
        };
        Ok(Self {
            format,
            name: input.name,
            site: input.site,
            since_display: since.map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            since_stored: since.map(crate::store::db::fmt_time),
            limit,
            include_text: input.include_text.unwrap_or(true),
            include_extraction: input.include_extraction.unwrap_or(true),
            max_bytes,
        })
    }

    /// The filters block a payload and `index.md` echo.
    pub fn filters(&self) -> CorpusFilters {
        CorpusFilters {
            site: self.site.clone(),
            since: self.since_display.clone(),
            limit: self.limit,
            include_text: self.include_text,
            include_extraction: self.include_extraction,
            max_bytes: self.max_bytes,
        }
    }
}

/// The page row's stored text: the delivered Markdown or the record text, as
/// stored (never the FTS projection's own cap).
pub fn stored_text(row: &CorpusPageRow) -> Option<String> {
    let text = crate::store::fts::index_text_from_stored(row.result_json.as_deref())?;
    (!text.trim().is_empty()).then_some(text)
}

/// One JSONL line for a page (no trailing newline).
pub fn jsonl_line(row: &CorpusPageRow, plan: &ExportPlan) -> Option<String> {
    let text = stored_text(row)?;
    let (body, text_truncated) = truncate_chars(&text, TEXT_CAP_CHARS);
    let (extraction, extraction_truncated) = match row.result_json.as_deref() {
        Some(json) if plan.include_extraction => match serde_json::from_str::<Value>(json) {
            Ok(value) => {
                if value.to_string().chars().count() > TEXT_CAP_CHARS {
                    (Value::Null, true)
                } else {
                    (value, false)
                }
            }
            Err(_) => (Value::Null, false),
        },
        _ => (Value::Null, false),
    };
    let line = json!({
        "url": row.url,
        "canonical_url": row.canonical_url,
        "title": row.title,
        "lang": row.lang,
        "site": row.site,
        "first_seen": row.first_seen,
        "last_fetched": row.last_fetched,
        "status": row.last_status,
        "content_type": row.content_type,
        "render_mode": row.render_mode,
        "text_hash": row.text_hash,
        "embedding_model": row.embedding_model,
        "embedding_chunks": row.embedding_chunks.unwrap_or(0),
        "text": if plan.include_text { Value::String(body) } else { Value::Null },
        "text_truncated": text_truncated,
        "extraction": extraction,
        "extraction_truncated": extraction_truncated,
    });
    Some(line.to_string())
}

/// One markdown page (front matter + body).
pub fn markdown_page(row: &CorpusPageRow, plan: &ExportPlan) -> Option<String> {
    let text = stored_text(row)?;
    let (body, _) = truncate_chars(&text, TEXT_CAP_CHARS);
    let body = if plan.include_text {
        body
    } else {
        String::new()
    };
    Some(format!(
        "---\nurl: {}\ntitle: {}\nfetched_at: {}\n---\n\n{}\n",
        one_line(&row.url),
        one_line(row.title.as_deref().unwrap_or("")),
        one_line(row.last_fetched.as_deref().unwrap_or("")),
        body
    ))
}

/// Front matter is one line per field; a newline in a title would break it.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The markdown `index.md` body.
pub fn markdown_index(
    plan: &ExportPlan,
    records: usize,
    skipped: usize,
    generated_at: &str,
) -> String {
    format!(
        "---\ngenerated_at: {generated_at}\nformat: markdown\nrecords: {records}\nskipped_no_text: {skipped}\nsite: {}\nsince: {}\nlimit: {}\ninclude_text: {}\ninclude_extraction: {}\nmax_bytes: {}\n---\n\n# Corpus export\n\n{records} pages, {skipped} skipped (no stored text).\n",
        plan.site.as_deref().unwrap_or(""),
        plan.since_display.as_deref().unwrap_or(""),
        plan.limit,
        plan.include_text,
        plan.include_extraction,
        plan.max_bytes
    )
}

/// Where one page's bytes go.
enum Sink {
    Jsonl(tokio::fs::File),
    Markdown { root: PathBuf },
}

impl Sink {
    /// Write one page. `false` means the byte budget stopped the export before
    /// this page was written.
    async fn write_page(
        &mut self,
        row: &CorpusPageRow,
        plan: &ExportPlan,
        written: &mut u64,
    ) -> Result<bool> {
        match self {
            Self::Jsonl(file) => {
                let Some(mut line) = jsonl_line(row, plan) else {
                    return Ok(true);
                };
                line.push('\n');
                let bytes = line.into_bytes();
                if *written + bytes.len() as u64 > plan.max_bytes {
                    return Ok(false);
                }
                use tokio::io::AsyncWriteExt;
                file.write_all(&bytes).await.map_err(|error| {
                    WidowError::internal(format!("cannot write the export file: {error}"))
                })?;
                *written += bytes.len() as u64;
                Ok(true)
            }
            Self::Markdown { root } => {
                let Some(page) = markdown_page(row, plan) else {
                    return Ok(true);
                };
                let bytes = page.into_bytes();
                if *written + bytes.len() as u64 > plan.max_bytes {
                    return Ok(false);
                }
                let pages = root.join("pages");
                tokio::fs::create_dir_all(&pages).await.map_err(|error| {
                    WidowError::internal(format!("cannot create {}: {error}", pages.display()))
                })?;
                let path = pages.join(format!("{}.md", row.url_hash));
                tokio::fs::write(&path, &bytes).await.map_err(|error| {
                    WidowError::internal(format!("cannot write {}: {error}", path.display()))
                })?;
                *written += bytes.len() as u64;
                Ok(true)
            }
        }
    }

    /// Finish the export: flush JSONL, or write `index.md` for markdown.
    async fn finalize(
        &mut self,
        plan: &ExportPlan,
        records: usize,
        skipped: usize,
        generated_at: &str,
        written: &mut u64,
    ) -> Result<()> {
        match self {
            Self::Jsonl(file) => {
                use tokio::io::AsyncWriteExt;
                file.flush().await.map_err(|error| {
                    WidowError::internal(format!("cannot flush the export file: {error}"))
                })
            }
            Self::Markdown { root } => {
                let pages = root.join("pages");
                if !tokio::fs::try_exists(&pages).await.unwrap_or(false) {
                    tokio::fs::create_dir_all(&pages).await.map_err(|error| {
                        WidowError::internal(format!("cannot create {}: {error}", pages.display()))
                    })?;
                }
                let index = markdown_index(plan, records, skipped, generated_at);
                let path = root.join("index.md");
                tokio::fs::write(&path, index.as_bytes())
                    .await
                    .map_err(|error| {
                        WidowError::internal(format!("cannot write {}: {error}", path.display()))
                    })?;
                *written += index.len() as u64;
                Ok(())
            }
        }
    }
}

/// Run the export. The caller has validated the plan.
pub async fn write(store: &Store, paths: &Paths, plan: &ExportPlan) -> Result<CorpusExportPayload> {
    let exports = paths.exports();
    crate::store::datadir::ensure_private_dir(&exports)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let base = format!("{}-{stamp}", plan.name);
    let final_path = match plan.format {
        ExportFormat::Jsonl => exports.join(format!("{base}.jsonl")),
        ExportFormat::Markdown => exports.join(&base),
    };
    if !final_path.starts_with(&exports) {
        return Err(WidowError::internal(
            "refusing an export path outside the exports directory",
        ));
    }
    let temp_path = exports.join(format!(".{base}.tmp"));
    if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
        return Err(WidowError::internal(format!(
            "export path already exists: {}",
            final_path.display()
        )));
    }

    let result = run(store, plan, &temp_path, &generated_at).await;
    match result {
        Ok(mut payload) => {
            if let Err(error) = tokio::fs::rename(&temp_path, &final_path).await {
                remove_temp(&temp_path).await;
                return Err(WidowError::internal(format!(
                    "cannot move the export into place: {error}"
                )));
            }
            payload.path = final_path.to_string_lossy().to_string();
            payload.format = plan.format.as_str().to_string();
            payload.generated_at = generated_at;
            payload.filters = plan.filters();
            Ok(payload)
        }
        Err(error) => {
            remove_temp(&temp_path).await;
            Err(error)
        }
    }
}

async fn run(
    store: &Store,
    plan: &ExportPlan,
    temp_path: &Path,
    generated_at: &str,
) -> Result<CorpusExportPayload> {
    let mut sink = match plan.format {
        ExportFormat::Jsonl => {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(temp_path)
                .await
                .map_err(|error| {
                    WidowError::internal(format!("cannot create {}: {error}", temp_path.display()))
                })?;
            Sink::Jsonl(file)
        }
        ExportFormat::Markdown => {
            tokio::fs::DirBuilder::new()
                .mode(0o700)
                .create(temp_path)
                .await
                .map_err(|error| {
                    WidowError::internal(format!("cannot create {}: {error}", temp_path.display()))
                })?;
            Sink::Markdown {
                root: temp_path.to_path_buf(),
            }
        }
    };

    let mut after = 0i64;
    let mut records = 0usize;
    let mut skipped = 0usize;
    let mut written = 0u64;
    let mut truncated = false;
    let mut limit_hit: Option<String> = None;

    'paging: loop {
        let rows = store
            .db
            .corpus_pages(plan.site.clone(), plan.since_stored.clone(), after, BATCH)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            after = row.id;
            if stored_text(row).is_none() {
                skipped += 1;
                continue;
            }
            // The limit is exact: keep scanning non-exportable pages so a
            // truncated result always means "another exportable page exists".
            if records == plan.limit {
                truncated = true;
                limit_hit = Some("limit".to_string());
                break 'paging;
            }
            if !sink.write_page(row, plan, &mut written).await? {
                truncated = true;
                limit_hit = Some("max_bytes".to_string());
                break 'paging;
            }
            records += 1;
        }
        if rows.len() < BATCH {
            break;
        }
    }

    sink.finalize(plan, records, skipped, generated_at, &mut written)
        .await?;
    Ok(CorpusExportPayload {
        format: String::new(),
        path: String::new(),
        records,
        skipped_no_text: skipped,
        bytes: written,
        truncated,
        limit_hit,
        generated_at: String::new(),
        filters: CorpusFilters {
            site: None,
            since: None,
            limit: plan.limit,
            include_text: plan.include_text,
            include_extraction: plan.include_extraction,
            max_bytes: plan.max_bytes,
        },
    })
}

async fn remove_temp(path: &Path) {
    if path.is_dir() {
        let _ = tokio::fs::remove_dir_all(path).await;
    } else {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// Ensure the exports directory exists with `0700`.
pub fn exports_root(paths: &Paths) -> Result<PathBuf> {
    let exports = paths.exports();
    crate::store::datadir::ensure_private_dir(&exports)?;
    Ok(exports)
}

/// True when `path` is inside `<data_dir>/exports/` (the containment rule).
pub fn is_contained(paths: &Paths, path: &Path) -> bool {
    path.starts_with(paths.exports())
}
