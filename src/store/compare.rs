//! Reads for `snapshot_compare`: one page fetch version.
//!
//! A page version is a `fetch_events.id` joined to its page and to the
//! extraction that event produced (if any). The pair identity for a page compare
//! is `pages.canonical_url`; a version without an extraction cannot be diffed
//! and is reported as `not_found` with the constraint named.

use rusqlite::{OptionalExtension, params};

use crate::core::error::Result;

use super::db::Database;

/// One page fetch version as the comparer sees it.
#[derive(Debug, Clone)]
pub struct PageVersionRow {
    /// `fetch_events.id`.
    pub id: i64,
    pub page_id: i64,
    pub url: String,
    pub canonical_url: String,
    pub started_at: String,
    pub http_status: Option<i64>,
    pub mode: String,
    /// The extraction this event produced, when one was stored.
    pub result_json: Option<String>,
}

impl Database {
    pub async fn page_version(&self, fetch_event_id: i64) -> Result<Option<PageVersionRow>> {
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT e.id, e.page_id, p.url, p.canonical_url, e.started_at,
                            e.http_status, e.mode,
                            (SELECT x.result_json FROM extractions x
                              WHERE x.fetch_event_id = e.id
                              ORDER BY x.id DESC LIMIT 1)
                     FROM fetch_events e
                     JOIN pages p ON p.id = e.page_id
                     WHERE e.id = ?1",
                    params![fetch_event_id],
                    |row| {
                        Ok(PageVersionRow {
                            id: row.get(0)?,
                            page_id: row.get(1)?,
                            url: row.get(2)?,
                            canonical_url: row.get(3)?,
                            started_at: row.get(4)?,
                            http_status: row.get(5)?,
                            mode: row.get(6)?,
                            result_json: row.get(7)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }
}
