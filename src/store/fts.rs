//! Full-text index over stored page content (IMP-DEC-029).
//!
//! One `pages_fts` row per `pages` row. The index is maintained by an explicit
//! upsert inside the same transaction as the page write; there are no triggers,
//! so every write path that stores content must supply the text (see
//! [`super::db::FetchRecord::index_text`]). Two write policies keep the row
//! honest (M05 B01b): delivered page text replaces it, a projection such as
//! `extract`'s record text may only fill a page that has none. The backfill
//! fills the rows that predate schema v6 once, guarded by a marker, preferring
//! the most recent stored Markdown over a later record-text extraction.
//!
//! Nothing here talks to FTS5 for reading a query; that lives with the
//! `search_content` contract in [`crate::fts`], and the SQL is in
//! [`super::db::Database::search_content`].

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::truncate::truncate_chars;

/// `meta` key written once the v6 backfill has finished.
pub const BACKFILL_MARKER: &str = "fts_backfill_v6";

/// Pages per backfill transaction: bounded so a large store does not hold one
/// long write transaction during startup.
pub const BACKFILL_BATCH: i64 = 200;

/// Upsert one page's index row. `title` is read from `pages` inside the same
/// transaction, so a caller that has no title (an `extract` acquisition) still
/// indexes the stored page title rather than an empty one.
pub(super) fn upsert(
    transaction: &rusqlite::Transaction<'_>,
    page_id: i64,
    body: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT OR REPLACE INTO pages_fts(rowid, title, body)
         VALUES (?1, COALESCE((SELECT title FROM pages WHERE id = ?1), ''), ?2)",
        params![page_id, body],
    )?;
    Ok(())
}

/// Write one page's index row only if the page has no indexed body yet.
///
/// `extract` is a projection of a page, not the page: when the delivered
/// Markdown is already indexed, record text must not replace it (M05 B01b).
/// A page with no row, or a row whose body is empty (title-only), is filled.
pub(super) fn fill_empty(
    transaction: &rusqlite::Transaction<'_>,
    page_id: i64,
    body: &str,
) -> rusqlite::Result<()> {
    let existing_chars: Option<i64> = transaction
        .query_row(
            "SELECT length(body) FROM pages_fts WHERE rowid = ?1",
            params![page_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing_chars.unwrap_or(0) > 0 {
        return Ok(());
    }
    upsert(transaction, page_id, body)
}

/// Concatenated string values of an extraction record set, one value per line.
/// Nested arrays (`all: true`) are flattened; numbers and booleans are not text
/// and are skipped. This is the extract/crawl index text, shared with the
/// backfill so a stored row and a fresh acquisition produce the same shape.
pub fn index_text_from_records(records: &[serde_json::Map<String, serde_json::Value>]) -> String {
    let mut values = Vec::new();
    for record in records {
        for value in record.values() {
            collect_strings(value, &mut values);
        }
    }
    values.join("\n")
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

/// Index text for a stored extraction payload, as the backfill sees it.
///
/// - a `fetch_page` cache record (`{"payload": …, "markdown": "…"}`) → the
///   delivered Markdown;
/// - an `extract`/crawl payload (`{"records": […]}`) → the concatenated string
///   values;
/// - anything else (a monitor's bare `records` array, an older shape) → `None`,
///   which indexes the title only.
pub fn index_text_from_stored(result_json: Option<&str>) -> Option<String> {
    let object = serde_json::from_str::<serde_json::Value>(result_json?).ok()?;
    let object = object.as_object()?;
    if let Some(markdown) = object.get("markdown").and_then(serde_json::Value::as_str) {
        return Some(markdown.to_string());
    }
    let records = object.get("records")?.as_array()?;
    let mut values = Vec::new();
    for record in records {
        collect_strings(record, &mut values);
    }
    Some(values.join("\n"))
}

/// Fill `pages_fts` for the pages that predate schema v6, once.
///
/// Runs inside `Database::open` after a successful migration: bounded
/// transactions of [`BACKFILL_BATCH`] pages, resumable (upserts are idempotent,
/// so a crash before the marker simply re-runs), and a no-op once the marker is
/// set. Pages acquired after v6 are indexed on write and are never revisited.
pub(super) fn backfill(
    connection: &mut Connection,
    max_index_chars: usize,
) -> rusqlite::Result<()> {
    let done: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![BACKFILL_MARKER],
            |row| row.get(0),
        )
        .optional()?;
    if done.is_some() {
        return Ok(());
    }

    let mut scanned = 0usize;
    let mut with_text = 0usize;
    let mut title_only = 0usize;
    let mut after: i64 = 0;
    loop {
        let transaction = connection.transaction()?;
        // Two candidates per page, in the same bounded window as the page rows:
        // the most recent extraction that carries delivered Markdown, and the
        // most recent extraction of any shape. Markdown wins (M05 B01b): a later
        // `extract` of the same page must not erase the page's own text from the
        // index, which is exactly the live-write defect this revision fixed.
        let rows: Vec<(i64, Option<String>, Option<String>)> = {
            let mut statement = transaction.prepare(
                "SELECT p.id,
                        (SELECT e.result_json FROM extractions e
                         WHERE e.page_id = p.id
                           AND CASE WHEN json_valid(e.result_json)
                                    THEN json_type(e.result_json, '$.markdown') = 'text'
                                    ELSE 0 END
                         ORDER BY e.created_at DESC, e.id DESC LIMIT 1),
                        (SELECT e.result_json FROM extractions e
                         WHERE e.page_id = p.id
                         ORDER BY e.created_at DESC, e.id DESC LIMIT 1)
                 FROM pages p
                 WHERE p.id > ?1
                 ORDER BY p.id
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after, BACKFILL_BATCH], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            transaction.commit()?;
            break;
        }
        for (page_id, markdown_json, latest_json) in &rows {
            let stored = markdown_json.as_deref().or(latest_json.as_deref());
            let text = index_text_from_stored(stored).unwrap_or_default();
            let (body, _) = truncate_chars(&text, max_index_chars);
            if body.is_empty() {
                title_only += 1;
            } else {
                with_text += 1;
            }
            upsert(&transaction, *page_id, &body)?;
            scanned += 1;
        }
        after = rows.last().map(|(id, _, _)| *id).unwrap_or(after);
        transaction.commit()?;
    }

    connection.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![BACKFILL_MARKER, scanned.to_string()],
    )?;
    tracing::info!(
        scanned,
        with_text,
        title_only,
        max_index_chars,
        "fts backfill complete (schema v6)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_fetch_page_row_indexes_its_markdown() {
        let stored = json!({
            "payload": { "url": "https://example.com/a" },
            "markdown": "the delivered body"
        })
        .to_string();
        assert_eq!(
            index_text_from_stored(Some(&stored)).as_deref(),
            Some("the delivered body")
        );
    }

    #[test]
    fn stored_extract_row_indexes_its_record_strings() {
        let stored = json!({
            "url": "https://example.com/a",
            "records": [
                { "title": "First item", "price": 10.0, "tags": ["alpha", "beta"] },
                { "title": "Second item", "flag": true }
            ],
            "record_count": 2
        })
        .to_string();
        // `serde_json::Map` is key-sorted, so the body order is field name
        // order: deterministic, and identical for the live and backfill paths.
        assert_eq!(
            index_text_from_stored(Some(&stored)).as_deref(),
            Some("alpha\nbeta\nFirst item\nSecond item")
        );
    }

    #[test]
    fn a_bare_records_array_or_garbage_indexes_the_title_only() {
        assert_eq!(index_text_from_stored(Some("[{\"title\":\"x\"}]")), None);
        assert_eq!(index_text_from_stored(Some("not json")), None);
        assert_eq!(index_text_from_stored(None), None);
        assert_eq!(index_text_from_stored(Some("{\"records\": 3}")), None);
    }
}
