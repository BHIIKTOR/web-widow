//! SQLite repository: WAL, one writer thread, a small read pool, and the v1
//! migration (IMP-DATA-001). Every write goes through the writer actor, so the
//! "single writer" rule is a property of the type system here rather than a
//! convention.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::oneshot;

use crate::config::{FtsConfig, RetentionConfig};
use crate::core::error::{Result, WidowError};

pub const SCHEMA_VERSION: i64 = 8;

const SCHEMA_V1: &str = include_str!("schema_v1.sql");
const SCHEMA_V2: &str = include_str!("schema_v2.sql");
const SCHEMA_V3: &str = include_str!("schema_v3.sql");
const SCHEMA_V4: &str = include_str!("schema_v4.sql");
const SCHEMA_V5: &str = include_str!("schema_v5.sql");
const SCHEMA_V6: &str = include_str!("schema_v6.sql");
const SCHEMA_V7: &str = include_str!("schema_v7.sql");
const SCHEMA_V8: &str = include_str!("schema_v8.sql");

pub(crate) fn fmt_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(super) fn parse_time(text: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn sql_err(error: rusqlite::Error) -> WidowError {
    WidowError::Internal(format!("sqlite: {error}"))
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PageRecord {
    pub id: i64,
    pub url: String,
    pub canonical_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub charset: Option<String>,
    pub last_status: Option<i64>,
    pub last_fetched: Option<String>,
    pub raw_hash: Option<String>,
    pub title: Option<String>,
    pub lang: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BlobRow {
    pub hash: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Content-addressed extraction spec (`profiles` row).
#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub hash: String,
    /// `None` for inline specs; named profiles are out of M01.
    pub name: Option<String>,
    pub spec_json: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ExtractionRow {
    /// Content-addressed key of the extraction shape (mode for `fetch_page`).
    pub profile_hash: String,
    pub result_json: String,
    pub result_hash: String,
    pub record_count: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct FetchRecord {
    pub url: String,
    pub canonical_url: String,
    pub url_hash: String,
    pub site: String,
    pub fetched_at: DateTime<Utc>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub charset: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub raw_hash: Option<String>,
    pub text_hash: Option<String>,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub render_mode: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub mode: String,
    pub bytes: Option<u64>,
    pub error: Option<(String, String)>,
    pub blob: Option<BlobRow>,
    pub extraction: Option<ExtractionRow>,
    /// Text this acquisition offers to `pages_fts`, and its write policy: the
    /// delivered Markdown of `fetch_page`/crawl/monitor replaces the row, while
    /// `extract` may only fill a page that has no indexed body (`IndexWrite`).
    /// `None` on a 304 and on a failure, so the previous row is kept rather than
    /// cleared. Capped by the caller at `[fts].max_index_chars`.
    pub index_text: Option<IndexText>,
    /// Upserted with the extraction so a spec is stored once.
    pub profile: Option<ProfileRow>,
}

#[derive(Debug, Clone, Copy)]
pub struct Recorded {
    pub page_id: i64,
    pub fetch_event_id: i64,
    pub extraction_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CachedExtraction {
    pub result_json: String,
    pub created_at: DateTime<Utc>,
    pub page: PageRecord,
}

/// One acquisition's contribution to the page's full-text row, and how it may
/// change an existing one (M05 B01b, IMP-RISK-039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexWrite {
    /// Replace the indexed body with this acquisition's text. Used by the paths
    /// that deliver the page itself: `fetch_page`, crawl and monitor.
    Replace,
    /// Keep an existing indexed body and write only when the page has none.
    /// Used by `extract`: its text is a projection of the page, so it must never
    /// hide the delivered Markdown from local search.
    FillEmpty,
}

/// The text an acquisition offers to the index, with its write policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexText {
    pub body: String,
    pub write: IndexWrite,
}

impl IndexText {
    /// Delivered page text: it replaces whatever the page indexed before.
    pub fn replace(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            write: IndexWrite::Replace,
        }
    }

    /// Projected text: it is kept only when the page has no indexed body yet.
    pub fn fill_empty(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            write: IndexWrite::FillEmpty,
        }
    }
}

/// One ranked hit from the local full-text index (`search_content`).
#[derive(Debug, Clone)]
pub struct ContentHitRow {
    pub url: String,
    pub canonical_url: String,
    pub title: String,
    pub snippet: String,
    /// `-bm25`: higher is better (SQLite's bm25 is negative, smaller is better).
    pub score: f64,
    pub last_fetched: Option<String>,
    pub render_mode: Option<String>,
}

/// One bounded page of local hits plus the full match count.
#[derive(Debug, Clone)]
pub struct ContentHits {
    pub rows: Vec<ContentHitRow>,
    pub total: i64,
}

#[derive(Debug, Clone)]
pub struct HostRow {
    pub host: String,
    pub robots_fetched_at: Option<DateTime<Utc>>,
    pub robots_status: Option<i64>,
    pub robots_hash: Option<String>,
    pub robots_body: Option<Vec<u8>>,
    pub robots_expires_at: Option<DateTime<Utc>>,
    pub next_allowed_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i64,
}

#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub events_pruned: usize,
    pub extractions_pruned: usize,
    pub snapshots_pruned: usize,
    pub blobs_pruned: usize,
    pub webhook_deliveries_pruned: usize,
    pub bytes_freed: u64,
    /// Blob hashes whose rows were deleted; the caller removes the files.
    pub removed_blobs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Writer actor
// ---------------------------------------------------------------------------

type CustomWrite = dyn FnOnce(&mut Connection) -> rusqlite::Result<()> + Send;

enum WriteOp {
    /// A typed operation owned by another store module (`store::jobs`).
    Custom(
        Box<CustomWrite>,
        oneshot::Sender<std::result::Result<(), String>>,
    ),
    RecordFetch(
        Box<FetchRecord>,
        oneshot::Sender<std::result::Result<Recorded, String>>,
    ),
    SaveHost(
        Box<HostRow>,
        oneshot::Sender<std::result::Result<(), String>>,
    ),
    Gc(
        RetentionConfig,
        DateTime<Utc>,
        oneshot::Sender<std::result::Result<GcReport, String>>,
    ),
}

#[derive(Debug)]
struct Writer {
    // tokio's unbounded sender is Send + Sync, so the store can be shared across
    // tasks; the receiving side is drained by a dedicated blocking thread.
    tx: tokio::sync::mpsc::UnboundedSender<WriteOp>,
}

impl Writer {
    fn start(db_path: PathBuf) -> Result<(Self, std::thread::JoinHandle<()>)> {
        let mut connection = open_writer(&db_path)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteOp>();
        let handle = std::thread::Builder::new()
            .name("sqlite-writer".to_string())
            .spawn(move || {
                while let Some(op) = rx.blocking_recv() {
                    match op {
                        WriteOp::Custom(operation, reply) => {
                            let outcome = operation(&mut connection).map_err(|e| e.to_string());
                            let _ = reply.send(outcome);
                        }
                        WriteOp::RecordFetch(record, reply) => {
                            let _ = reply.send(
                                record_fetch(&mut connection, &record).map_err(|e| e.to_string()),
                            );
                        }
                        WriteOp::SaveHost(row, reply) => {
                            let _ = reply
                                .send(save_host(&mut connection, &row).map_err(|e| e.to_string()));
                        }
                        WriteOp::Gc(retention, now, reply) => {
                            let _ = reply.send(
                                gc(&mut connection, now, &retention).map_err(|e| e.to_string()),
                            );
                        }
                    }
                }
            })
            .map_err(|error| {
                WidowError::Internal(format!("cannot start sqlite writer: {error}"))
            })?;
        Ok((Self { tx }, handle))
    }
}

fn open_writer(db_path: &Path) -> Result<Connection> {
    let connection = Connection::open(db_path)
        .map_err(|error| WidowError::Internal(format!("cannot open database: {error}")))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(sql_err)?;
    // journal_mode returns a row; query it rather than using execute_batch.
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(sql_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(WidowError::Internal(format!(
            "database refused WAL mode (got `{mode}`)"
        )));
    }
    connection
        .execute_batch(
            "PRAGMA synchronous=NORMAL;\n\
             PRAGMA foreign_keys=ON;\n\
             PRAGMA temp_store=MEMORY;",
        )
        .map_err(sql_err)?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(sql_err)?;
    if version > SCHEMA_VERSION {
        return Err(WidowError::Internal(format!(
            "database schema v{version} is newer than this binary (v{SCHEMA_VERSION}); downgrade is not supported"
        )));
    }
    if version == 0 {
        // auto_vacuum only takes effect before the first table exists; on a
        // brand-new file a VACUUM applies it deterministically (the pragma is a
        // no-op once pages exist, hence the re-read).
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(sql_err)?;
        let mut auto_vacuum: i64 = connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .map_err(sql_err)?;
        if auto_vacuum != 2 {
            connection.execute_batch("VACUUM;").map_err(sql_err)?;
            auto_vacuum = connection
                .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
                .map_err(sql_err)?;
        }
        if auto_vacuum != 2 {
            tracing::warn!(
                auto_vacuum,
                "database did not accept incremental auto-vacuum"
            );
        }
    }
    // Forward-only, one step per version so a v1 database upgrades in place.
    let mut version = version;
    while version < SCHEMA_VERSION {
        let next = version + 1;
        let script = match next {
            1 => SCHEMA_V1,
            2 => SCHEMA_V2,
            3 => SCHEMA_V3,
            4 => SCHEMA_V4,
            5 => SCHEMA_V5,
            6 => SCHEMA_V6,
            7 => SCHEMA_V7,
            8 => SCHEMA_V8,
            other => {
                return Err(WidowError::Internal(format!(
                    "no migration script for schema v{other}"
                )));
            }
        };
        let transaction = connection.transaction().map_err(sql_err)?;
        transaction.execute_batch(script).map_err(sql_err)?;
        transaction
            .pragma_update(None, "user_version", next)
            .map_err(sql_err)?;
        transaction.commit().map_err(sql_err)?;
        version = next;
    }
    Ok(())
}

/// Insert or refresh the `blobs` row for bytes already written to the CAS.
/// The `screenshots.hash` foreign key requires this row first; B1/B2 call both
/// helpers inside one writer-actor closure.
pub(crate) fn save_blob(
    connection: &Connection,
    hash: &str,
    size: i64,
    content_type: Option<&str>,
    created_at: DateTime<Utc>,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO blobs (hash, size, content_type, created_at, last_referenced_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(hash) DO UPDATE SET last_referenced_at = excluded.last_referenced_at",
        params![hash, size, content_type, fmt_time(created_at)],
    )?;
    Ok(())
}

/// Insert a screenshot reference row. The caller has already written the blob
/// to the CAS and its `blobs` row; lanes B1/B2 call this through the writer
/// actor.
// The signature mirrors the table columns; grouping it into a struct would be
// a wider API change than the lanes need.
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_screenshot(
    connection: &Connection,
    page_id: Option<i64>,
    hash: &str,
    format: &str,
    width: Option<i64>,
    height: Option<i64>,
    source: &str,
    captured_at: DateTime<Utc>,
) -> rusqlite::Result<i64> {
    connection.execute(
        "INSERT INTO screenshots (page_id, hash, format, width, height, source, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            page_id,
            hash,
            format,
            width,
            height,
            source,
            fmt_time(captured_at)
        ],
    )?;
    Ok(connection.last_insert_rowid())
}

fn record_fetch(connection: &mut Connection, record: &FetchRecord) -> rusqlite::Result<Recorded> {
    let transaction = connection.transaction()?;
    let fetched_at = fmt_time(record.fetched_at);

    let page_id: i64 = transaction.query_row(
        "INSERT INTO pages (
             url, canonical_url, url_hash, site, first_seen, last_fetched, last_status,
             etag, last_modified, content_type, charset, raw_hash, text_hash, title, lang,
             render_mode, outcome
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(url_hash) DO UPDATE SET
             url = excluded.url,
             canonical_url = excluded.canonical_url,
             site = excluded.site,
             last_fetched = excluded.last_fetched,
             last_status = excluded.last_status,
             etag = COALESCE(excluded.etag, pages.etag),
             last_modified = COALESCE(excluded.last_modified, pages.last_modified),
             content_type = COALESCE(excluded.content_type, pages.content_type),
             charset = COALESCE(excluded.charset, pages.charset),
             raw_hash = COALESCE(excluded.raw_hash, pages.raw_hash),
             text_hash = COALESCE(excluded.text_hash, pages.text_hash),
             title = COALESCE(excluded.title, pages.title),
             lang = COALESCE(excluded.lang, pages.lang),
             render_mode = excluded.render_mode,
             outcome = excluded.outcome
         RETURNING id",
        params![
            record.url,
            record.canonical_url,
            record.url_hash,
            record.site,
            fetched_at,
            record.status.map(i64::from),
            record.etag,
            record.last_modified,
            record.content_type,
            record.charset,
            record.raw_hash,
            record.text_hash,
            record.title,
            record.lang,
            record.render_mode,
            record.outcome,
        ],
        |row| row.get(0),
    )?;

    // Local full-text index: one row per page, written with the page write so
    // the index can never lag the stored content (IMP-RISK-039). A `None`
    // `index_text` (304, failure) leaves the previous row alone; the policy on
    // `IndexText` keeps an `extract` from replacing delivered page text.
    if let Some(index_text) = &record.index_text {
        match index_text.write {
            IndexWrite::Replace => super::fts::upsert(&transaction, page_id, &index_text.body)?,
            IndexWrite::FillEmpty => {
                super::fts::fill_empty(&transaction, page_id, &index_text.body)?
            }
        }
    }

    let (error_kind, error_detail) = match &record.error {
        Some((kind, detail)) => (Some(kind.clone()), Some(detail.clone())),
        None => (None, None),
    };
    transaction.execute(
        "INSERT INTO fetch_events (
             page_id, started_at, duration_ms, http_status, mode, raw_hash, text_hash,
             bytes, error_kind, error_detail
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            page_id,
            fmt_time(
                record
                    .fetched_at
                    .checked_sub_signed(chrono::Duration::milliseconds(
                        record.duration_ms.min(i64::MAX as u64) as i64
                    ))
                    .unwrap_or(record.fetched_at)
            ),
            record.duration_ms as i64,
            record.status.map(i64::from),
            record.mode,
            record.raw_hash,
            record.text_hash,
            record.bytes.map(|value| value.min(i64::MAX as u64) as i64),
            error_kind,
            error_detail,
        ],
    )?;
    let fetch_event_id = transaction.last_insert_rowid();

    if let Some(blob) = &record.blob {
        transaction.execute(
            "INSERT INTO blobs (hash, size, content_type, created_at, last_referenced_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(hash) DO UPDATE SET last_referenced_at = excluded.last_referenced_at",
            params![
                blob.hash,
                blob.size,
                blob.content_type,
                fmt_time(blob.created_at)
            ],
        )?;
    }

    if let Some(profile) = &record.profile {
        transaction.execute(
            "INSERT INTO profiles (hash, name, spec_json, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO NOTHING",
            params![
                profile.hash,
                profile.name,
                profile.spec_json,
                fmt_time(profile.created_at)
            ],
        )?;
    }

    let extraction_id = match &record.extraction {
        Some(extraction) => {
            transaction.execute(
                "INSERT INTO extractions (
                     page_id, fetch_event_id, profile_hash, result_json, result_hash,
                     record_count, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    page_id,
                    fetch_event_id,
                    extraction.profile_hash,
                    extraction.result_json,
                    extraction.result_hash,
                    extraction.record_count,
                    fmt_time(extraction.created_at),
                ],
            )?;
            Some(transaction.last_insert_rowid())
        }
        None => None,
    };

    transaction.commit()?;
    Ok(Recorded {
        page_id,
        fetch_event_id,
        extraction_id,
    })
}

impl FetchRecord {
    pub fn new(url: &str, canonical_url: &str, url_hash: &str) -> Self {
        Self {
            url: url.to_string(),
            canonical_url: canonical_url.to_string(),
            url_hash: url_hash.to_string(),
            site: String::new(),
            fetched_at: Utc::now(),
            status: None,
            content_type: None,
            charset: None,
            etag: None,
            last_modified: None,
            raw_hash: None,
            text_hash: None,
            title: None,
            lang: None,
            render_mode: "static".to_string(),
            outcome: String::new(),
            duration_ms: 0,
            mode: "main".to_string(),
            bytes: None,
            error: None,
            blob: None,
            extraction: None,
            index_text: None,
            profile: None,
        }
    }
}

fn row_to_host(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostRow> {
    let fetched_at: Option<String> = row.get(1)?;
    let expires_at: Option<String> = row.get(5)?;
    let next_allowed: Option<String> = row.get(6)?;
    Ok(HostRow {
        host: row.get(0)?,
        robots_fetched_at: match fetched_at {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        robots_status: row.get(2)?,
        robots_hash: row.get(3)?,
        robots_body: row.get(4)?,
        robots_expires_at: match expires_at {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        next_allowed_at: match next_allowed {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        consecutive_failures: row.get(7)?,
    })
}

fn save_host(connection: &mut Connection, row: &HostRow) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO hosts (
             host, robots_fetched_at, robots_status, robots_hash, robots_body,
             robots_expires_at, next_allowed_at, consecutive_failures
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(host) DO UPDATE SET
             robots_fetched_at = excluded.robots_fetched_at,
             robots_status = excluded.robots_status,
             robots_hash = excluded.robots_hash,
             robots_body = excluded.robots_body,
             robots_expires_at = excluded.robots_expires_at,
             next_allowed_at = COALESCE(excluded.next_allowed_at, hosts.next_allowed_at),
             consecutive_failures = excluded.consecutive_failures",
        params![
            row.host,
            row.robots_fetched_at.map(fmt_time),
            row.robots_status,
            row.robots_hash,
            row.robots_body,
            row.robots_expires_at.map(fmt_time),
            row.next_allowed_at.map(fmt_time),
            row.consecutive_failures,
        ],
    )?;
    Ok(())
}

fn gc(
    connection: &mut Connection,
    now: DateTime<Utc>,
    retention: &RetentionConfig,
) -> rusqlite::Result<GcReport> {
    let transaction = connection.transaction()?;
    let events_cutoff =
        fmt_time(now - chrono::Duration::days(i64::from(retention.fetch_events_days)));
    let extractions_cutoff =
        fmt_time(now - chrono::Duration::days(i64::from(retention.extractions_days)));
    // Screenshots are disposable debugging evidence: prune the rows first, so
    // the blob reference set below no longer protects them. A blob that only a
    // pruned screenshot referenced then ages out with the existing cutoff.
    let screenshots_cutoff =
        fmt_time(now - chrono::Duration::days(i64::from(retention.screenshots_days)));
    let screenshots_pruned = transaction.execute(
        "DELETE FROM screenshots WHERE captured_at < ?1",
        params![screenshots_cutoff],
    )?;

    // Webhook history: delivered rows follow a short retention window; dead
    // letters are kept longer for diagnosis. Pending rows are never pruned.
    let delivered_cutoff =
        fmt_time(now - chrono::Duration::days(i64::from(retention.webhook_deliveries_days)));
    let dead_cutoff =
        fmt_time(now - chrono::Duration::days(i64::from(retention.webhook_dead_days)));
    let webhook_deliveries_pruned = transaction.execute(
        "DELETE FROM webhook_deliveries
         WHERE (status = 'delivered' AND delivered_at IS NOT NULL AND delivered_at < ?1)
            OR (status = 'dead' AND created_at < ?2)",
        params![delivered_cutoff, dead_cutoff],
    )?;

    // Snapshots first: the extraction/fetch-event guards below read the rows
    // that survive. Keep the newest N snapshots per monitor plus every change
    // point and every snapshot a change row references (IMP-DATA-001).
    let snapshots_pruned = transaction.execute(
        "DELETE FROM snapshots
         WHERE is_change_point = 0
           AND id NOT IN (SELECT from_snapshot FROM changes)
           AND id NOT IN (SELECT to_snapshot FROM changes)
           AND id NOT IN (
               SELECT id FROM (
                   SELECT id,
                          ROW_NUMBER() OVER (
                              PARTITION BY monitor_id ORDER BY taken_at DESC, id DESC
                          ) AS position
                   FROM snapshots
               ) WHERE position <= ?1
           )",
        params![retention.snapshots_per_monitor.min(i64::MAX as usize) as i64],
    )?;

    // Extractions first: deleting an old fetch event cascades to its
    // extraction, and an explicit statement gives an accurate count instead of
    // relying on the cascade. A snapshot keeps the extraction/event that
    // produced it alive.
    let extractions_pruned = transaction.execute(
        "DELETE FROM extractions WHERE created_at < ?1
           AND fetch_event_id NOT IN (
               SELECT fetch_event_id FROM snapshots WHERE fetch_event_id IS NOT NULL
           )",
        params![extractions_cutoff],
    )?;
    let events_pruned = transaction.execute(
        "DELETE FROM fetch_events
         WHERE started_at < ?1
           AND id NOT IN (
               SELECT fetch_event_id FROM snapshots WHERE fetch_event_id IS NOT NULL
           )
           AND id NOT IN (
               SELECT newer.id FROM fetch_events AS newer
               WHERE newer.page_id = fetch_events.page_id
               ORDER BY newer.started_at DESC, newer.id DESC
               LIMIT 1
           )",
        params![events_cutoff],
    )?;

    // A blob is retained when a page, a snapshot, a fetch event, an extraction
    // or a screenshot still references it. `fetch_events.raw_hash` was a log-only
    // field in B05; B6-4 requires no dangling `raw_hash` in any row, so it is
    // now a reference too (events age out with their blob).
    let blobs_pruned = transaction.execute(
        "DELETE FROM blobs
         WHERE created_at < ?1
           AND hash NOT IN (SELECT raw_hash FROM pages WHERE raw_hash IS NOT NULL)
           AND hash NOT IN (SELECT raw_hash FROM fetch_events WHERE raw_hash IS NOT NULL)
           AND hash NOT IN (SELECT raw_hash FROM snapshots)
           AND hash NOT IN (SELECT normalized_hash FROM snapshots)
           AND hash NOT IN (SELECT result_hash FROM snapshots WHERE result_hash IS NOT NULL)
           AND hash NOT IN (SELECT hash FROM screenshots)",
        params![extractions_cutoff],
    )?;

    // Budget enforcement: evict oldest unreferenced blobs until under budget.
    let mut removed: Vec<String> = Vec::new();
    let mut bytes_freed = 0u64;
    let mut total: u64 =
        transaction.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |row| {
            row.get::<_, i64>(0).map(|value| value.max(0) as u64)
        })?;
    if total > retention.max_blob_bytes {
        let candidates: Vec<(String, i64)> = {
            let mut statement = transaction.prepare(
                "SELECT hash, size FROM blobs
                 WHERE hash NOT IN (SELECT raw_hash FROM pages WHERE raw_hash IS NOT NULL)
                   AND hash NOT IN (SELECT raw_hash FROM fetch_events WHERE raw_hash IS NOT NULL)
                   AND hash NOT IN (SELECT raw_hash FROM snapshots)
                   AND hash NOT IN (SELECT normalized_hash FROM snapshots)
                   AND hash NOT IN (SELECT result_hash FROM snapshots WHERE result_hash IS NOT NULL)
                   AND hash NOT IN (SELECT hash FROM screenshots)
                 ORDER BY created_at ASC, hash ASC",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (hash, size) in candidates {
            if total <= retention.max_blob_bytes {
                break;
            }
            transaction.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])?;
            total = total.saturating_sub(size.max(0) as u64);
            bytes_freed += size.max(0) as u64;
            removed.push(hash);
        }
    }

    transaction.commit()?;
    let _ = connection.execute_batch("PRAGMA incremental_vacuum(64);");
    tracing::debug!(screenshots_pruned, "screenshot rows pruned");
    Ok(GcReport {
        events_pruned,
        extractions_pruned,
        snapshots_pruned,
        blobs_pruned,
        webhook_deliveries_pruned,
        bytes_freed,
        removed_blobs: removed,
    })
}

// ---------------------------------------------------------------------------
// Read pool
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ReadPool {
    connections: Mutex<Vec<Connection>>,
    available: Condvar,
}

impl ReadPool {
    fn new(db_path: &Path, size: usize) -> Result<Self> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            let connection = Connection::open(db_path).map_err(|error| {
                WidowError::Internal(format!("cannot open read connection: {error}"))
            })?;
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(sql_err)?;
            connection
                .execute_batch("PRAGMA query_only=ON;")
                .map_err(sql_err)?;
            connections.push(connection);
        }
        Ok(Self {
            connections: Mutex::new(connections),
            available: Condvar::new(),
        })
    }

    fn with<T>(&self, operation: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut guard = self
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let connection = loop {
            if let Some(connection) = guard.pop() {
                break connection;
            }
            guard = self
                .available
                .wait(guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        };
        drop(guard);
        let result = operation(&connection);
        let mut guard = self
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.push(connection);
        self.available.notify_one();
        result.map_err(sql_err)
    }
}

fn row_to_page(row: &rusqlite::Row<'_>) -> rusqlite::Result<PageRecord> {
    Ok(PageRecord {
        id: row.get(0)?,
        url: row.get(1)?,
        canonical_url: row.get(2)?,
        etag: row.get(3)?,
        last_modified: row.get(4)?,
        content_type: row.get(5)?,
        charset: row.get(6)?,
        last_status: row.get(7)?,
        last_fetched: row.get(8)?,
        raw_hash: row.get(9)?,
        title: row.get(10)?,
        lang: row.get(11)?,
    })
}

const PAGE_COLUMNS: &str = "p.id, p.url, p.canonical_url, p.etag, p.last_modified, p.content_type, \
                            p.charset, p.last_status, p.last_fetched, p.raw_hash, p.title, p.lang";

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Database {
    writer: Writer,
    readers: Arc<ReadPool>,
    _writer_thread: std::thread::JoinHandle<()>,
}

impl Database {
    /// Run a write closure on the single writer connection.
    pub(crate) async fn write<F>(&self, operation: F) -> Result<()>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<()> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        self.writer
            .tx
            .send(WriteOp::Custom(Box::new(operation), reply))
            .map_err(|_| WidowError::Internal("sqlite writer stopped".to_string()))?;
        response
            .await
            .map_err(|_| WidowError::Internal("sqlite writer dropped the request".to_string()))?
            .map_err(WidowError::Internal)
    }

    /// Run a write closure that returns a value. The value is sent back from the
    /// writer thread; a SQLite error is returned as the write error.
    pub(crate) async fn write_with<T, F>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (value_tx, value_rx) = oneshot::channel::<T>();
        let (done_tx, done_rx) = oneshot::channel::<std::result::Result<(), String>>();
        let op = move |connection: &mut Connection| match operation(connection) {
            Ok(value) => {
                let _ = value_tx.send(value);
                Ok(())
            }
            Err(error) => Err(error),
        };
        self.writer
            .tx
            .send(WriteOp::Custom(Box::new(op), done_tx))
            .map_err(|_| WidowError::Internal("sqlite writer stopped".to_string()))?;
        done_rx
            .await
            .map_err(|_| WidowError::Internal("sqlite writer dropped the request".to_string()))?
            .map_err(WidowError::Internal)?;
        value_rx
            .await
            .map_err(|_| WidowError::Internal("writer did not return a value".to_string()))
    }

    /// Run a read closure on a pooled read connection.
    pub(crate) async fn read<T, F>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || readers.with(operation))
            .await
            .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    pub fn open(db_path: &Path, read_connections: usize, fts: &FtsConfig) -> Result<Self> {
        let mut connection = open_writer(db_path)?;
        migrate(&mut connection)?;
        // The v6 backfill runs on the writer connection before any reader or
        // the writer thread exists, so no concurrent write can interleave.
        super::fts::backfill(&mut connection, fts.max_index_chars).map_err(sql_err)?;
        drop(connection);
        let readers = Arc::new(ReadPool::new(db_path, read_connections)?);
        let (writer, handle) = Writer::start(db_path.to_path_buf())?;
        Ok(Self {
            writer,
            readers,
            _writer_thread: handle,
        })
    }

    pub async fn page(&self, url_hash: &str) -> Result<Option<PageRecord>> {
        let readers = Arc::clone(&self.readers);
        let hash = url_hash.to_string();
        tokio::task::spawn_blocking(move || {
            readers.with(|connection| {
                connection
                    .query_row(
                        &format!("SELECT {PAGE_COLUMNS} FROM pages p WHERE p.url_hash = ?1"),
                        params![hash],
                        row_to_page,
                    )
                    .optional()
            })
        })
        .await
        .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    /// One blob row by hash (`blob_get`). Read-only.
    pub async fn blob(&self, hash: &str) -> Result<Option<BlobRow>> {
        let hash = hash.to_string();
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT hash, size, content_type, created_at FROM blobs WHERE hash = ?1",
                    params![hash],
                    |row| {
                        Ok(BlobRow {
                            hash: row.get(0)?,
                            size: row.get(1)?,
                            content_type: row.get(2)?,
                            created_at: parse_time(&row.get::<_, String>(3)?)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// Bounded reference names for one blob hash: `screenshot:<id>`,
    /// `page:<id>`, `fetch_event:<id>`, `extraction:<id>`, `snapshot:<id>`.
    /// The list is context for `blob_get`, never a retention decision.
    pub async fn blob_references(&self, hash: &str, limit: usize) -> Result<Vec<String>> {
        let hash = hash.to_string();
        let limit = limit.min(i64::MAX as usize) as i64;
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT ref FROM (
                     SELECT 'screenshot:' || id AS ref FROM screenshots WHERE hash = ?1
                     UNION ALL
                     SELECT 'page:' || id FROM pages WHERE raw_hash = ?1 OR text_hash = ?1
                     UNION ALL
                     SELECT 'fetch_event:' || id FROM fetch_events WHERE raw_hash = ?1
                     UNION ALL
                     SELECT 'extraction:' || id FROM extractions WHERE result_hash = ?1
                     UNION ALL
                     SELECT 'snapshot:' || id FROM snapshots
                         WHERE normalized_hash = ?1 OR result_hash = ?1
                 )
                 ORDER BY ref
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(params![hash, limit], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
        })
        .await
    }

    pub async fn cached_extraction(
        &self,
        url_hash: &str,
        profile_hash: &str,
    ) -> Result<Option<CachedExtraction>> {
        let readers = Arc::clone(&self.readers);
        let url_hash = url_hash.to_string();
        let profile_hash = profile_hash.to_string();
        tokio::task::spawn_blocking(move || {
            readers.with(|connection| {
                connection
                    .query_row(
                        &format!(
                            "SELECT e.result_json, e.created_at, {PAGE_COLUMNS}
                             FROM extractions e JOIN pages p ON p.id = e.page_id
                             WHERE p.url_hash = ?1 AND e.profile_hash = ?2
                             ORDER BY e.created_at DESC, e.id DESC
                             LIMIT 1"
                        ),
                        params![url_hash, profile_hash],
                        |row| {
                            Ok(CachedExtraction {
                                result_json: row.get(0)?,
                                created_at: parse_time(&row.get::<_, String>(1)?)?,
                                page: row_to_page_at(row, 2)?,
                            })
                        },
                    )
                    .optional()
            })
        })
        .await
        .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    /// Run one local full-text query over `pages_fts` (`search_content`).
    ///
    /// The caller supplies an already-built FTS5 expression; nothing here
    /// interprets caller text, so no query can reach the engine half-escaped.
    /// `limit` is applied to the ranked rows; `total` is the full match count
    /// over the same MATCH, so truncation is reported against the real total.
    pub async fn search_content(&self, match_expr: String, limit: usize) -> Result<ContentHits> {
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || {
            readers.with(|connection| {
                let total: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH ?1",
                    params![match_expr],
                    |row| row.get(0),
                )?;
                let mut statement = connection.prepare(
                    "SELECT p.url,
                            p.canonical_url,
                            COALESCE(p.title, ''),
                            snippet(pages_fts, 1, '', '', ' … ', 24),
                            -bm25(pages_fts, 4.0, 1.0) AS score,
                            p.last_fetched,
                            p.render_mode
                     FROM pages_fts JOIN pages p ON p.id = pages_fts.rowid
                     WHERE pages_fts MATCH ?1
                     ORDER BY score DESC, p.id ASC
                     LIMIT ?2",
                )?;
                let rows = statement
                    .query_map(params![match_expr, limit as i64], |row| {
                        Ok(ContentHitRow {
                            url: row.get(0)?,
                            canonical_url: row.get(1)?,
                            title: row.get(2)?,
                            snippet: row.get(3)?,
                            score: row.get(4)?,
                            last_fetched: row.get(5)?,
                            render_mode: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ContentHits { rows, total })
            })
        })
        .await
        .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    pub async fn host(&self, host: &str) -> Result<Option<HostRow>> {
        let readers = Arc::clone(&self.readers);
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            readers.with(|connection| {
                connection
                    .query_row(
                        "SELECT host, robots_fetched_at, robots_status, robots_hash, robots_body,
                                robots_expires_at, next_allowed_at, consecutive_failures
                         FROM hosts WHERE host = ?1",
                        params![host],
                        row_to_host,
                    )
                    .optional()
            })
        })
        .await
        .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    /// One page of the robots cache in host order (`after` is the last host of
    /// the previous page). Read-only; the caller never fetches from here.
    pub async fn hosts_page(&self, after: Option<String>, limit: usize) -> Result<Vec<HostRow>> {
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || {
            readers.with(|connection| {
                let mut statement = connection.prepare(
                    "SELECT host, robots_fetched_at, robots_status, robots_hash, robots_body,
                            robots_expires_at, next_allowed_at, consecutive_failures
                     FROM hosts
                     WHERE (?1 IS NULL OR host > ?1)
                     ORDER BY host LIMIT ?2",
                )?;
                let rows = statement.query_map(
                    params![after, limit.min(i64::MAX as usize) as i64],
                    row_to_host,
                )?;
                rows.collect()
            })
        })
        .await
        .map_err(|error| WidowError::Internal(format!("read task failed: {error}")))?
    }

    pub async fn record_fetch(&self, record: FetchRecord) -> Result<Recorded> {
        let (reply, response) = oneshot::channel();
        self.writer
            .tx
            .send(WriteOp::RecordFetch(Box::new(record), reply))
            .map_err(|_| WidowError::Internal("sqlite writer stopped".to_string()))?;
        response
            .await
            .map_err(|_| WidowError::Internal("sqlite writer dropped the request".to_string()))?
            .map_err(WidowError::Internal)
    }

    pub async fn save_host(&self, row: HostRow) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.writer
            .tx
            .send(WriteOp::SaveHost(Box::new(row), reply))
            .map_err(|_| WidowError::Internal("sqlite writer stopped".to_string()))?;
        response
            .await
            .map_err(|_| WidowError::Internal("sqlite writer dropped the request".to_string()))?
            .map_err(WidowError::Internal)
    }

    pub async fn gc(&self, retention: RetentionConfig) -> Result<GcReport> {
        let (reply, response) = oneshot::channel();
        self.writer
            .tx
            .send(WriteOp::Gc(retention, Utc::now(), reply))
            .map_err(|_| WidowError::Internal("sqlite writer stopped".to_string()))?;
        response
            .await
            .map_err(|_| WidowError::Internal("sqlite writer dropped the request".to_string()))?
            .map_err(WidowError::Internal)
    }
}

fn row_to_page_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<PageRecord> {
    Ok(PageRecord {
        id: row.get(offset)?,
        url: row.get(offset + 1)?,
        canonical_url: row.get(offset + 2)?,
        etag: row.get(offset + 3)?,
        last_modified: row.get(offset + 4)?,
        content_type: row.get(offset + 5)?,
        charset: row.get(offset + 6)?,
        last_status: row.get(offset + 7)?,
        last_fetched: row.get(offset + 8)?,
        raw_hash: row.get(offset + 9)?,
        title: row.get(offset + 10)?,
        lang: row.get(offset + 11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(url: &str, outcome: &str) -> FetchRecord {
        let mut record =
            FetchRecord::new(url, url, &crate::core::url::content_hash(url.as_bytes()));
        record.site = "example.com".to_string();
        record.status = Some(200);
        record.content_type = Some("text/html".to_string());
        record.charset = Some("UTF-8".to_string());
        record.title = Some("Title".to_string());
        record.lang = Some("en".to_string());
        record.outcome = outcome.to_string();
        record.duration_ms = 12;
        record.bytes = Some(10);
        record
    }

    #[test]
    fn schema_v5_is_additive_only() {
        let mut statements = 0;
        for line in include_str!("schema_v5.sql").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("--") {
                continue;
            }
            assert!(
                line.starts_with("ALTER TABLE monitors ADD COLUMN"),
                "v5 must only add a nullable column: {line}"
            );
            statements += 1;
        }
        assert_eq!(statements, 1);
    }

    #[tokio::test]
    async fn a_v4_database_upgrades_in_place_to_the_current_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v4.db");
        let now = fmt_time(Utc::now());
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(SCHEMA_V1).unwrap();
            connection.execute_batch(SCHEMA_V2).unwrap();
            connection.execute_batch(SCHEMA_V3).unwrap();
            connection.execute_batch(SCHEMA_V4).unwrap();
            connection.pragma_update(None, "user_version", 4).unwrap();
            connection
                .execute(
                    "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                     VALUES ('https://example.com/old', 'https://example.com/old', ?1, ?2, 'ok')",
                    params!["h".repeat(64), now],
                )
                .unwrap();
            let page_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO fetch_events (page_id, started_at, duration_ms, mode)
                     VALUES (?1, ?2, 0, 'monitor')",
                    params![page_id, now],
                )
                .unwrap();
            let event_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO jobs (id, kind, spec_json, state, created_at, counters_json)
                     VALUES ('job-1', 'monitor_check', '{\"force\":false}', 'done', ?1, '{}')",
                    params![now],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO monitors (url, mode, interval_s, created_at, next_due_at)
                     VALUES ('https://example.com/a', 'main', 3600, ?1, ?1)",
                    params![now],
                )
                .unwrap();
            let monitor_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO monitors (url, mode, interval_s, active, created_at, next_due_at)
                     VALUES ('https://example.com/b', 'main', 3600, 0, ?1, ?1)",
                    params![now],
                )
                .unwrap();
            let mut snapshot_ids = Vec::new();
            for index in 0..2 {
                connection
                    .execute(
                        "INSERT INTO snapshots (monitor_id, taken_at, fetch_event_id,
                                                normalized_hash, raw_hash)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            monitor_id,
                            now,
                            event_id,
                            format!("n{index}").repeat(32),
                            format!("r{index}").repeat(32)
                        ],
                    )
                    .unwrap();
                snapshot_ids.push(connection.last_insert_rowid());
            }
            connection
                .execute(
                    "INSERT INTO changes (monitor_id, from_snapshot, to_snapshot, detected_at,
                                          kind, stats_json, diff_text)
                     VALUES (?1, ?2, ?3, ?4, 'substantive', '{}', 'diff')",
                    params![monitor_id, snapshot_ids[0], snapshot_ids[1], now],
                )
                .unwrap();
        }

        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for (table, expected) in [
            ("monitors", 2_i64),
            ("snapshots", 2),
            ("changes", 1),
            ("pages", 1),
            ("fetch_events", 1),
            ("jobs", 1),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, expected, "{table} rows survive v4 -> v5");
        }
        let null_labels: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM monitors WHERE label IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_labels, 2, "every pre-v5 monitor is unlabeled");
        let fk_violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "foreign keys stay intact");
        drop(connection);
        drop(db);

        // A second open of the already-current database is a no-op.
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let monitors: i64 = connection
            .query_row("SELECT COUNT(*) FROM monitors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(monitors, 2, "the second open changes no rows");
        drop(connection);
        drop(db);
    }

    #[tokio::test]
    async fn a_v2_database_upgrades_in_place_to_the_current_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("upgrade.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(SCHEMA_V1).unwrap();
            connection.execute_batch(SCHEMA_V2).unwrap();
            connection.pragma_update(None, "user_version", 2).unwrap();
            connection
                .execute(
                    "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                     VALUES ('https://example.com/old', 'https://example.com/old', ?1, ?2, 'ok')",
                    params!["h".repeat(64), fmt_time(Utc::now())],
                )
                .unwrap();
        }
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let pages: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages, 1, "v1 data survives the v2 -> v3 upgrade");
        let monitors: i64 = connection
            .query_row("SELECT COUNT(*) FROM monitors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(monitors, 0, "the monitoring tables were created");
        let render_mode = connection
            .prepare("PRAGMA table_info(snapshots)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .flatten()
            .any(|column| column == "render_mode");
        assert!(render_mode, "v4 adds snapshots.render_mode");
        let label = connection
            .prepare("PRAGMA table_info(monitors)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .flatten()
            .any(|column| column == "label");
        assert!(label, "v5 adds monitors.label");
        drop(db);
    }

    #[tokio::test]
    async fn records_pages_and_events_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 2, &FtsConfig::default()).unwrap();
        let first = db
            .record_fetch(record("https://example.com/a", "ok"))
            .await
            .unwrap();
        let second = db
            .record_fetch(record("https://example.com/a", "ok"))
            .await
            .unwrap();
        assert_eq!(first.page_id, second.page_id);
        assert_ne!(first.fetch_event_id, second.fetch_event_id);
        let page = db
            .page(&crate::core::url::content_hash(b"https://example.com/a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.url, "https://example.com/a");
        assert_eq!(page.last_status, Some(200));
    }

    #[tokio::test]
    async fn gc_keeps_the_latest_event_per_page_and_evicts_over_budget_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 2, &FtsConfig::default()).unwrap();
        let mut old = record("https://example.com/a", "ok");
        old.fetched_at = Utc::now() - chrono::Duration::days(90);
        old.text_hash = Some("a".repeat(64));
        let old_extraction = ExtractionRow {
            profile_hash: "p".repeat(64),
            result_json: "{}".to_string(),
            result_hash: "r".repeat(64),
            record_count: 1,
            created_at: Utc::now() - chrono::Duration::days(90),
        };
        old.extraction = Some(old_extraction);
        old.blob = Some(BlobRow {
            hash: "b".repeat(64),
            size: 100,
            content_type: Some("text/html".to_string()),
            created_at: Utc::now() - chrono::Duration::days(90),
        });
        db.record_fetch(old).await.unwrap();
        db.record_fetch(record("https://example.com/a", "ok"))
            .await
            .unwrap();

        let retention = RetentionConfig {
            max_blob_bytes: 1,
            ..RetentionConfig::default()
        };
        let report = db.gc(retention).await.unwrap();
        assert_eq!(report.extractions_pruned, 1);
        assert_eq!(report.blobs_pruned, 1);
        // The most recent event per page survives even though it is old.
        assert_eq!(report.events_pruned, 1);
    }

    // -----------------------------------------------------------------------
    // M06 P0 — schema v7: recipes/recipe_versions/screenshots + retention
    // -----------------------------------------------------------------------

    fn write_v6_fixture(path: &Path) {
        let connection = Connection::open(path).unwrap();
        for script in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
        ] {
            connection.execute_batch(script).unwrap();
        }
        connection.pragma_update(None, "user_version", 6).unwrap();
        let now = fmt_time(Utc::now());
        connection
            .execute(
                "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                 VALUES ('https://example.com/v6', 'https://example.com/v6', ?1, ?2, 'ok')",
                params!["a".repeat(64), now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO blobs (hash, size, created_at, last_referenced_at)
                 VALUES (?1, 10, ?2, ?2)",
                params!["b".repeat(64), now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO pages_fts (rowid, title, body) VALUES (1, 'title', 'body text')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn a_v6_database_upgrades_to_the_current_version_additively() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v6.db");
        write_v6_fixture(&path);
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        drop(db);

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for (table, expected) in [
            ("pages", 1_i64),
            ("blobs", 1),
            ("pages_fts", 1),
            ("recipes", 0),
            ("recipe_versions", 0),
            ("screenshots", 0),
            ("embeddings", 0),
            ("embedding_state", 0),
            ("monitor_webhooks", 0),
            ("webhook_deliveries", 0),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, expected, "{table} after v6 -> v7");
        }
        for index in [
            "recipe_versions_profile",
            "screenshots_page",
            "screenshots_hash",
        ] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "index {index} exists");
        }
        let fk_violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "foreign keys stay intact");
        drop(connection);

        // A second open of the already-current database is a no-op.
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        drop(db);
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let pages: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages, 1, "the second open changes no rows");
    }

    #[test]
    fn a_newer_schema_is_refused_by_the_version_guard() {
        // A v6 binary meeting a v7 database hits the same guard (it compares the
        // file's user_version with its own SCHEMA_VERSION); this test pins the
        // guard itself rather than requiring the old binary.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("future.db");
        Database::open(&path, 1, &FtsConfig::default()).unwrap();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let error = Database::open(&path, 1, &FtsConfig::default()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("newer than this binary"),
            "unexpected guard message: {message}"
        );
    }

    // -----------------------------------------------------------------------
    // M07 P0 — schema v8: embeddings, embedding_state, monitor_webhooks,
    // webhook_deliveries
    // -----------------------------------------------------------------------

    fn write_v7_fixture(path: &Path) -> (i64, i64, i64) {
        let connection = Connection::open(path).unwrap();
        for script in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
        ] {
            connection.execute_batch(script).unwrap();
        }
        connection.pragma_update(None, "user_version", 7).unwrap();
        let now = fmt_time(Utc::now());
        connection
            .execute(
                "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                 VALUES ('https://example.com/v7', 'https://example.com/v7', ?1, ?2, 'ok')",
                params!["c".repeat(64), now],
            )
            .unwrap();
        let page_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO monitors (url, mode, interval_s, created_at, next_due_at)
                 VALUES ('https://example.com/m', 'main', 3600, ?1, ?1)",
                params![now],
            )
            .unwrap();
        let monitor_id = connection.last_insert_rowid();
        let mut snapshot_ids = Vec::new();
        for index in 0..2 {
            connection
                .execute(
                    "INSERT INTO snapshots (monitor_id, taken_at, normalized_hash, raw_hash)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        monitor_id,
                        now,
                        format!("n{index}").repeat(32),
                        format!("r{index}").repeat(32)
                    ],
                )
                .unwrap();
            snapshot_ids.push(connection.last_insert_rowid());
        }
        connection
            .execute(
                "INSERT INTO changes (monitor_id, from_snapshot, to_snapshot, detected_at,
                                      kind, stats_json, diff_text)
                 VALUES (?1, ?2, ?3, ?4, 'substantive', '{}', 'diff')",
                params![monitor_id, snapshot_ids[0], snapshot_ids[1], now],
            )
            .unwrap();
        let change_id = connection.last_insert_rowid();
        (page_id, monitor_id, change_id)
    }

    #[test]
    fn a_v7_database_upgrades_to_v8_additively_and_reopens_as_a_no_op() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v7.db");
        write_v7_fixture(&path);
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        drop(db);

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        for (table, expected) in [
            ("pages", 1_i64),
            ("monitors", 1),
            ("snapshots", 2),
            ("changes", 1),
            ("embeddings", 0),
            ("embedding_state", 0),
            ("monitor_webhooks", 0),
            ("webhook_deliveries", 0),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, expected, "{table} after v7 -> v8");
        }
        for index in [
            "embeddings_model_page",
            "embedding_state_model",
            "webhook_deliveries_due",
            "webhook_deliveries_change",
        ] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "index {index} exists");
        }
        let fk_violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "foreign keys stay intact");
        drop(connection);

        // A second open of the already-current database is a no-op.
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        drop(db);
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        let pages: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages, 1, "the second open changes no rows");
    }

    #[test]
    fn v8_rows_cascade_and_reject_malformed_vectors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v8-cascade.db");
        write_v7_fixture(&path);
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        drop(db);

        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let (page_id, monitor_id, change_id) = {
            let page: i64 = connection
                .query_row("SELECT id FROM pages LIMIT 1", [], |row| row.get(0))
                .unwrap();
            let monitor: i64 = connection
                .query_row("SELECT id FROM monitors LIMIT 1", [], |row| row.get(0))
                .unwrap();
            let change: i64 = connection
                .query_row("SELECT id FROM changes LIMIT 1", [], |row| row.get(0))
                .unwrap();
            (page, monitor, change)
        };
        let now = fmt_time(Utc::now());
        let vector = vec![0_u8; 8];
        connection
            .execute(
                "INSERT INTO embeddings (page_id, model, chunk_index, source_hash, chunk_hash,
                                         start_char, end_char, dim, vector, created_at)
                 VALUES (?1, 'minilm-l6-v2-q', 0, ?2, ?3, 0, 10, 2, ?4, ?5)",
                params![page_id, "s".repeat(64), "h".repeat(64), vector, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO embedding_state (page_id, model, source_hash, chunks, embedded_at)
                 VALUES (?1, 'minilm-l6-v2-q', ?2, 1, ?3)",
                params![page_id, "s".repeat(64), now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO monitor_webhooks (monitor_id, target, created_at)
                 VALUES (?1, 'ops', ?2)",
                params![monitor_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO webhook_deliveries (change_id, monitor_id, target, status,
                                                 attempts, next_attempt_at, created_at)
                 VALUES (?1, ?2, 'ops', 'pending', 0, ?3, ?3)",
                params![change_id, monitor_id, now],
            )
            .unwrap();

        // The UNIQUE(change_id, target) and the vector-length CHECK are real.
        let duplicate = connection.execute(
            "INSERT INTO webhook_deliveries (change_id, monitor_id, target, status,
                                             attempts, next_attempt_at, created_at)
             VALUES (?1, ?2, 'ops', 'pending', 0, ?3, ?3)",
            params![change_id, monitor_id, now],
        );
        assert!(duplicate.is_err(), "one delivery per (change, target)");
        let short_vector = connection.execute(
            "INSERT INTO embeddings (page_id, model, chunk_index, source_hash, chunk_hash,
                                     start_char, end_char, dim, vector, created_at)
             VALUES (?1, 'minilm-l6-v2-q', 1, ?2, ?3, 0, 10, 2, ?4, ?5)",
            params![page_id, "s".repeat(64), "h".repeat(64), vec![0_u8; 4], now],
        );
        assert!(short_vector.is_err(), "length(vector) must be dim * 4");

        // A page delete cascades to its vectors and state.
        connection
            .execute("DELETE FROM pages WHERE id = ?1", params![page_id])
            .unwrap();
        for table in ["embeddings", "embedding_state"] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} cascades with the page");
        }
        let deliveries: i64 = connection
            .query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(deliveries, 1, "a page delete leaves deliveries alone");

        // A monitor delete cascades to its attachments, changes and deliveries.
        connection
            .execute("DELETE FROM monitors WHERE id = ?1", params![monitor_id])
            .unwrap();
        for table in ["monitor_webhooks", "webhook_deliveries"] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} cascades with the monitor");
        }
    }

    fn insert_blob(connection: &Connection, hash: &str, size: i64, created_at: &str) {
        connection
            .execute(
                "INSERT INTO blobs (hash, size, created_at, last_referenced_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![hash, size, created_at],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn gc_prunes_aged_screenshots_and_keeps_referenced_ones() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gc-screenshots.db");
        Database::open(&path, 1, &FtsConfig::default()).unwrap();

        let now = Utc::now();
        let old = fmt_time(now - chrono::Duration::days(90));
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                 VALUES ('https://example.com/a', 'https://example.com/a', ?1, ?2, 'ok')",
                params!["p".repeat(64), fmt_time(now)],
            )
            .unwrap();
        let page_id = connection.last_insert_rowid();
        for hash in ["recent", "aged", "orphan"] {
            insert_blob(&connection, &hash.repeat(64)[..64], 100, &old);
        }
        // A recent screenshot row and one older than the screenshot window.
        save_screenshot(
            &connection,
            Some(page_id),
            &"recent".repeat(64)[..64],
            "png",
            Some(10),
            Some(10),
            "screenshot",
            now,
        )
        .unwrap();
        save_screenshot(
            &connection,
            Some(page_id),
            &"aged".repeat(64)[..64],
            "png",
            Some(10),
            Some(10),
            "interaction",
            now - chrono::Duration::days(60),
        )
        .unwrap();

        let retention = RetentionConfig {
            screenshots_days: 30,
            extractions_days: 30,
            max_blob_bytes: 20 * 1024 * 1024 * 1024,
            ..RetentionConfig::default()
        };
        let report = gc(&mut connection, now, &retention).unwrap();
        assert_eq!(report.blobs_pruned, 2, "aged and orphan blobs pruned");
        let screenshots: i64 = connection
            .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(screenshots, 1, "only the recent screenshot row survives");
        for hash in ["recent", "aged", "orphan"] {
            let key = &hash.repeat(64)[..64];
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM blobs WHERE hash = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .unwrap();
            if hash == "recent" {
                assert_eq!(count, 1, "a referenced screenshot blob is retained");
            } else {
                assert_eq!(count, 0, "{hash} blob pruned");
            }
        }
    }

    #[tokio::test]
    async fn gc_budget_does_not_evict_a_referenced_screenshot_blob() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gc-budget.db");
        Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let now = Utc::now();
        let old = fmt_time(now - chrono::Duration::days(90));
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome)
                 VALUES ('https://example.com/a', 'https://example.com/a', ?1, ?2, 'ok')",
                params!["q".repeat(64), fmt_time(now)],
            )
            .unwrap();
        let page_id = connection.last_insert_rowid();
        insert_blob(&connection, &"keep".repeat(16), 100, &old);
        insert_blob(&connection, &"evict".repeat(13)[..64], 100, &old);
        save_screenshot(
            &connection,
            Some(page_id),
            &"keep".repeat(16),
            "png",
            None,
            None,
            "screenshot",
            now,
        )
        .unwrap();
        let retention = RetentionConfig {
            max_blob_bytes: 1,
            ..RetentionConfig::default()
        };
        let report = gc(&mut connection, now, &retention).unwrap();
        assert_eq!(
            report.blobs_pruned, 1,
            "only the unreferenced blob is evicted"
        );
        let kept: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM blobs WHERE hash = ?1",
                params![&"keep".repeat(16)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            kept, 1,
            "the screenshot-referenced blob survives the budget"
        );
    }

    #[tokio::test]
    async fn cached_extraction_is_keyed_by_profile() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 2, &FtsConfig::default()).unwrap();
        let mut record = record("https://example.com/a", "ok");
        record.extraction = Some(ExtractionRow {
            profile_hash: "p".repeat(64),
            result_json: "{\"markdown_chars\":1}".to_string(),
            result_hash: "r".repeat(64),
            record_count: 1,
            created_at: Utc::now(),
        });
        db.record_fetch(record).await.unwrap();
        let hash = crate::core::url::content_hash(b"https://example.com/a");
        let hit = db
            .cached_extraction(&hash, &"p".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.result_json, "{\"markdown_chars\":1}");
        assert!(
            db.cached_extraction(&hash, &"z".repeat(64))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn hosts_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 2, &FtsConfig::default()).unwrap();
        assert!(db.host("example.com").await.unwrap().is_none());
        db.save_host(HostRow {
            host: "example.com".to_string(),
            robots_fetched_at: Some(Utc::now()),
            robots_status: Some(200),
            robots_hash: Some("h".repeat(64)),
            robots_body: Some(b"User-agent: *\nDisallow: /private\n".to_vec()),
            robots_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            next_allowed_at: None,
            consecutive_failures: 1,
        })
        .await
        .unwrap();
        let row = db.host("example.com").await.unwrap().unwrap();
        assert_eq!(row.robots_status, Some(200));
        assert_eq!(row.consecutive_failures, 1);
        assert_eq!(row.robots_body.unwrap().len(), 33);
    }

    // -----------------------------------------------------------------------
    // M05 Lane B — schema v6: the full-text index and its one-time backfill
    // -----------------------------------------------------------------------

    /// A v5 database holding one page per stored shape the backfill understands.
    fn write_v5_fixture(path: &Path) {
        let now = fmt_time(Utc::now());
        let connection = Connection::open(path).unwrap();
        for script in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5] {
            connection.execute_batch(script).unwrap();
        }
        connection.pragma_update(None, "user_version", 5).unwrap();

        fn insert_page(connection: &Connection, url: &str, title: &str, now: &str) -> i64 {
            connection
                .execute(
                    "INSERT INTO pages (url, canonical_url, url_hash, first_seen, last_fetched,
                                        title, outcome)
                     VALUES (?1, ?1, ?2, ?3, ?3, ?4, 'ok')",
                    params![
                        url,
                        crate::core::url::content_hash(url.as_bytes()),
                        now,
                        title
                    ],
                )
                .unwrap();
            connection.last_insert_rowid()
        }
        fn insert_extraction(connection: &Connection, page_id: i64, result_json: &str, now: &str) {
            connection
                .execute(
                    "INSERT INTO fetch_events (page_id, started_at, duration_ms, mode)
                     VALUES (?1, ?2, 0, 'main')",
                    params![page_id, now],
                )
                .unwrap();
            let event_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO extractions (page_id, fetch_event_id, profile_hash, result_json,
                                              result_hash, record_count, created_at)
                     VALUES (?1, ?2, NULL, ?3, 'result-hash', 1, ?4)",
                    params![page_id, event_id, result_json, now],
                )
                .unwrap();
        }

        let markdown = insert_page(
            &connection,
            "https://example.com/markdown",
            "Markdown Page",
            &now,
        );
        insert_extraction(
            &connection,
            markdown,
            r#"{"payload":{"url":"https://example.com/markdown"},"markdown":"the lighthouse keeper counted forty-one gulls"}"#,
            &now,
        );
        let records = insert_page(
            &connection,
            "https://example.com/records",
            "Records Page",
            &now,
        );
        insert_extraction(
            &connection,
            records,
            r#"{"url":"https://example.com/records","records":[{"title":"First item","price":"€10.00"}],"record_count":1}"#,
            &now,
        );
        // A monitor-only page: its extraction is a bare records array, which the
        // backfill deliberately does not parse (title-only until the next check).
        let monitor = insert_page(
            &connection,
            "https://example.com/monitor",
            "Monitor Page",
            &now,
        );
        insert_extraction(&connection, monitor, r#"[{"value":"monitored"}]"#, &now);
    }

    #[tokio::test]
    async fn a_v5_database_migrates_to_v6_and_backfills_every_page_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v5.db");
        write_v5_fixture(&path);

        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let pages: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        let indexed: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages, 3, "the migration preserves the page rows");
        assert_eq!(indexed, pages, "one index row per page");

        // Every page has a row: the markdown shape, the records shape, and the
        // unparsed monitor shape (title only).
        let body_of = |url: &str| -> String {
            connection
                .query_row(
                    "SELECT f.body FROM pages_fts f JOIN pages p ON p.id = f.rowid WHERE p.url = ?1",
                    params![url],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert!(body_of("https://example.com/markdown").contains("lighthouse keeper"));
        assert!(body_of("https://example.com/records").contains("First item"));
        assert_eq!(body_of("https://example.com/monitor"), "");
        let title_hit: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH '\"Monitor\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title_hit, 1, "a title-only page is still findable by title");

        let marker: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'fts_backfill_v6'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "3");
        drop(connection);
        drop(db);

        // A second open is a no-op: the marker short-circuits the backfill.
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let indexed: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, 3);

        // Crash-before-marker recovery: clearing the marker re-runs the backfill,
        // and the upserts are idempotent (no duplicates, same bodies).
        connection
            .execute("DELETE FROM meta WHERE key = 'fts_backfill_v6'", [])
            .unwrap();
        drop(connection);
        drop(db);
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let indexed: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            indexed, 3,
            "the re-run replaces rows, it does not duplicate them"
        );
        let marker: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'fts_backfill_v6'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "3");
        drop(connection);
        drop(db);
    }

    #[tokio::test]
    async fn the_backfill_respects_the_index_cap_and_keeps_utf8_valid() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v5.db");
        write_v5_fixture(&path);

        let fts = FtsConfig {
            max_index_chars: 8,
            ..FtsConfig::default()
        };
        let db = Database::open(&path, 1, &fts).unwrap();
        let connection = Connection::open(&path).unwrap();
        let body: String = connection
            .query_row(
                "SELECT f.body FROM pages_fts f JOIN pages p ON p.id = f.rowid
                 WHERE p.url = 'https://example.com/markdown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            body.chars().count(),
            8,
            "the cap is applied at a char boundary"
        );
        assert!(body.is_char_boundary(body.len()));
        drop(db);
    }

    #[tokio::test]
    async fn record_fetch_replaces_the_index_row_and_keeps_it_on_a_304_or_failure() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let url = "https://example.com/a";

        let mut first = record(url, "ok");
        first.index_text = Some(IndexText::replace("first body text"));
        db.record_fetch(first).await.unwrap();
        let hits = db.search_content("\"body\"".to_string(), 10).await.unwrap();
        assert_eq!(hits.rows.len(), 1);
        assert!(hits.rows[0].snippet.contains("body"));

        // A newer acquisition replaces the row: still one row, new text.
        let mut second = record(url, "ok");
        second.title = Some("Second title".to_string());
        second.index_text = Some(IndexText::replace("replacement words"));
        db.record_fetch(second).await.unwrap();
        let rows: i64 = db
            .read(|connection| {
                connection.query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
            })
            .await
            .unwrap();
        assert_eq!(
            rows, 1,
            "the upsert replaces the page row, it does not duplicate it"
        );
        let hits = db.search_content("\"body\"".to_string(), 10).await.unwrap();
        assert_eq!(hits.total, 0, "the old body is gone");
        let hits = db
            .search_content("\"replacement\"".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(hits.rows.len(), 1);
        assert_eq!(hits.rows[0].title, "Second title");

        // A 304 and a failure carry no `index_text`: the row is kept.
        let mut not_modified = record(url, "not_modified");
        not_modified.status = Some(304);
        db.record_fetch(not_modified).await.unwrap();
        let mut failed = record(url, "error:http_error");
        failed.status = Some(500);
        failed.error = Some(("http_error".to_string(), "boom".to_string()));
        db.record_fetch(failed).await.unwrap();
        let hits = db
            .search_content("\"replacement\"".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(
            hits.rows.len(),
            1,
            "a 304 or a failure never clears the index"
        );
    }

    #[tokio::test]
    async fn a_page_with_an_empty_body_is_still_indexed_by_title() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let mut page = record("https://example.com/title-only", "ok");
        page.title = Some("Title Only Page".to_string());
        page.index_text = Some(IndexText::replace(""));
        db.record_fetch(page).await.unwrap();

        let hits = db.search_content("title".to_string(), 10).await.unwrap();
        assert_eq!(hits.rows.len(), 1);
        assert_eq!(hits.rows[0].title, "Title Only Page");
        assert_eq!(
            hits.rows[0].snippet, "",
            "an empty body yields an empty snippet"
        );
    }

    // -----------------------------------------------------------------------
    // M05 B01b — index write policy: delivered page text vs projected records
    // -----------------------------------------------------------------------

    async fn indexed_body(db: &Database, url_hash: &str) -> Option<String> {
        let hash = url_hash.to_string();
        db.read(move |connection| {
            connection
                .query_row(
                    "SELECT f.body FROM pages_fts f JOIN pages p ON p.id = f.rowid
                     WHERE p.url_hash = ?1",
                    params![hash],
                    |row| row.get(0),
                )
                .optional()
        })
        .await
        .unwrap()
    }

    async fn index_rows(db: &Database) -> i64 {
        db.read(|connection| {
            connection.query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_extract_never_replaces_indexed_page_text() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let url = "https://example.com/page";
        let hash = crate::core::url::content_hash(url.as_bytes());

        // A page acquisition delivers the page text.
        let mut page = record(url, "ok");
        page.index_text = Some(IndexText::replace("the delivered page text"));
        db.record_fetch(page).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("the delivered page text")
        );

        // A later `extract` of the same URL offers only a projection. It must
        // not replace the page text (the B01b defect), and it adds no row.
        let mut extract = record(url, "ok");
        extract.mode = "extract".to_string();
        extract.index_text = Some(IndexText::fill_empty("one projected field"));
        db.record_fetch(extract).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("the delivered page text"),
            "an extract must not hide the delivered text from local search"
        );
        assert_eq!(index_rows(&db).await, 1);
        assert_eq!(
            db.search_content("\"delivered\"".to_string(), 10)
                .await
                .unwrap()
                .total,
            1
        );
        assert_eq!(
            db.search_content("\"projected\"".to_string(), 10)
                .await
                .unwrap()
                .total,
            0,
            "the discarded projection is not in the index"
        );

        // A newer page acquisition still replaces it.
        let mut newer = record(url, "ok");
        newer.index_text = Some(IndexText::replace("the newer page text"));
        db.record_fetch(newer).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("the newer page text")
        );
        assert_eq!(index_rows(&db).await, 1);
    }

    #[tokio::test]
    async fn an_extract_fills_a_page_that_has_no_indexed_body_and_a_page_acquisition_replaces_it() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let url = "https://example.com/extract-first";
        let hash = crate::core::url::content_hash(url.as_bytes());

        // Extract first: the page has no row, so the projection fills it.
        let mut extract = record(url, "ok");
        extract.mode = "extract".to_string();
        extract.index_text = Some(IndexText::fill_empty("first projected text"));
        db.record_fetch(extract).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("first projected text")
        );

        // A second projection cannot overwrite the first.
        let mut again = record(url, "ok");
        again.mode = "extract".to_string();
        again.index_text = Some(IndexText::fill_empty("second projected text"));
        db.record_fetch(again).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("first projected text")
        );

        // The page acquisition replaces it.
        let mut page = record(url, "ok");
        page.index_text = Some(IndexText::replace("the delivered page text"));
        db.record_fetch(page).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("the delivered page text")
        );
        assert_eq!(index_rows(&db).await, 1);
    }

    #[tokio::test]
    async fn an_extract_with_no_text_leaves_a_title_only_row_and_a_later_projection_fills_it() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let url = "https://example.com/records-only";
        let hash = crate::core::url::content_hash(url.as_bytes());

        // Numbers and booleans are not text: the record text is empty.
        let mut extract = record(url, "ok");
        extract.title = Some("Records Only".to_string());
        extract.mode = "extract".to_string();
        extract.index_text = Some(IndexText::fill_empty(""));
        db.record_fetch(extract).await.unwrap();
        assert_eq!(indexed_body(&db, &hash).await.as_deref(), Some(""));
        assert_eq!(
            db.search_content("\"Records\"".to_string(), 10)
                .await
                .unwrap()
                .total,
            1,
            "the title is still indexed"
        );

        // An empty body counts as "no indexed body": a later projection fills it.
        let mut second = record(url, "ok");
        second.mode = "extract".to_string();
        second.index_text = Some(IndexText::fill_empty("later projected text"));
        db.record_fetch(second).await.unwrap();
        assert_eq!(
            indexed_body(&db, &hash).await.as_deref(),
            Some("later projected text")
        );
    }

    #[tokio::test]
    async fn mixed_writes_keep_exactly_one_index_row_per_page() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.db"), 1, &FtsConfig::default()).unwrap();
        let urls = [
            "https://example.com/a",
            "https://example.com/b",
            "https://example.com/c",
        ];
        for (index, url) in urls.iter().enumerate() {
            let mut page = record(url, "ok");
            page.index_text = Some(IndexText::replace(format!("page {index} text")));
            db.record_fetch(page).await.unwrap();
            let mut extract = record(url, "ok");
            extract.mode = "extract".to_string();
            extract.index_text = Some(IndexText::fill_empty(format!("record {index} text")));
            db.record_fetch(extract).await.unwrap();
        }
        let pages: i64 = db
            .read(|connection| {
                connection.query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            })
            .await
            .unwrap();
        assert_eq!(pages, 3);
        assert_eq!(index_rows(&db).await, pages, "one index row per page");
        for url in urls {
            let hash = crate::core::url::content_hash(url.as_bytes());
            assert!(
                indexed_body(&db, &hash).await.unwrap().contains("page"),
                "{url} keeps its delivered text"
            );
        }
    }

    /// Two pages whose newest extraction is not the one that carries Markdown.
    fn write_v5_preference_fixture(path: &Path) {
        let connection = Connection::open(path).unwrap();
        for script in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5] {
            connection.execute_batch(script).unwrap();
        }
        connection.pragma_update(None, "user_version", 5).unwrap();

        let insert_page = |url: &str, title: &str| -> i64 {
            connection
                .execute(
                    "INSERT INTO pages (url, canonical_url, url_hash, first_seen, last_fetched,
                                        title, outcome)
                     VALUES (?1, ?1, ?2, '2026-10-01T00:00:00.000Z', '2026-10-02T00:00:00.000Z',
                             ?3, 'ok')",
                    params![url, crate::core::url::content_hash(url.as_bytes()), title],
                )
                .unwrap();
            connection.last_insert_rowid()
        };
        let insert_extraction = |page_id: i64, result_json: &str, at: &str| {
            connection
                .execute(
                    "INSERT INTO fetch_events (page_id, started_at, duration_ms, mode)
                     VALUES (?1, ?2, 0, 'main')",
                    params![page_id, at],
                )
                .unwrap();
            let event_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO extractions (page_id, fetch_event_id, profile_hash, result_json,
                                              result_hash, record_count, created_at)
                     VALUES (?1, ?2, NULL, ?3, 'result-hash', 1, ?4)",
                    params![page_id, event_id, result_json, at],
                )
                .unwrap();
        };

        // The delivered Markdown is older; a later extract of the same page is
        // newer. The backfill must still index the Markdown.
        let older_markdown = insert_page("https://example.com/older-markdown", "Older Markdown");
        insert_extraction(
            older_markdown,
            r#"{"payload":{"url":"https://example.com/older-markdown"},"markdown":"delivered article body about herons"}"#,
            "2026-10-01T00:00:00.000Z",
        );
        insert_extraction(
            older_markdown,
            r#"{"records":[{"label":"projection only"}],"record_count":1}"#,
            "2026-10-03T00:00:00.000Z",
        );

        // The reverse order: the Markdown is the newest, so both rules agree.
        let newer_markdown = insert_page("https://example.com/newer-markdown", "Newer Markdown");
        insert_extraction(
            newer_markdown,
            r#"{"records":[{"label":"projection only"}],"record_count":1}"#,
            "2026-10-01T00:00:00.000Z",
        );
        insert_extraction(
            newer_markdown,
            r#"{"payload":{"url":"https://example.com/newer-markdown"},"markdown":"delivered article body about otters"}"#,
            "2026-10-03T00:00:00.000Z",
        );
    }

    #[tokio::test]
    async fn the_backfill_prefers_stored_markdown_over_a_later_projection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v5-preference.db");
        write_v5_preference_fixture(&path);

        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        let connection = Connection::open(&path).unwrap();
        let body_of = |url: &str| -> String {
            connection
                .query_row(
                    "SELECT f.body FROM pages_fts f JOIN pages p ON p.id = f.rowid
                     WHERE p.url = ?1",
                    params![url],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            body_of("https://example.com/older-markdown"),
            "delivered article body about herons",
            "the older delivered Markdown wins over a newer record projection"
        );
        assert_eq!(
            body_of("https://example.com/newer-markdown"),
            "delivered article body about otters"
        );
        let indexed: i64 = connection
            .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, 2);
        drop(connection);
        drop(db);
    }
}
