//! Corpus reads for `corpus_export`.
//!
//! One row per page with a successful latest fetch, keyset-paged by `id` so the
//! exporter can stream without holding the corpus. The page's text and
//! extraction come from its latest extraction row; a page with none is returned
//! with `result_json = None` and the exporter counts it as `skipped_no_text`.

use rusqlite::params;

use crate::core::error::Result;

use super::db::Database;

/// One page as the exporter sees it.
#[derive(Debug, Clone)]
pub struct CorpusPageRow {
    pub id: i64,
    pub url: String,
    pub canonical_url: String,
    /// `pages.url_hash`: the filename a markdown export uses.
    pub url_hash: String,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub site: Option<String>,
    /// Stored RFC3339 timestamps; passed through to the export line unchanged.
    pub first_seen: String,
    pub last_fetched: Option<String>,
    pub last_status: Option<i64>,
    pub content_type: Option<String>,
    pub render_mode: Option<String>,
    pub text_hash: Option<String>,
    /// The latest extraction's stored payload, if any.
    pub result_json: Option<String>,
    /// `embedding_state` for the active model, when the page has been embedded.
    pub embedding_model: Option<String>,
    pub embedding_chunks: Option<i64>,
}

impl Database {
    /// `after` is the last `pages.id` returned (0 for the first batch). `site`
    /// and `since` are exact/lexicographic filters; `since` is a stored-format
    /// timestamp.
    pub async fn corpus_pages(
        &self,
        site: Option<String>,
        since: Option<String>,
        after: i64,
        batch: usize,
    ) -> Result<Vec<CorpusPageRow>> {
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT p.id, p.url, p.canonical_url, p.url_hash, p.title, p.lang, p.site,
                        p.first_seen, p.last_fetched, p.last_status, p.content_type,
                        p.render_mode, p.text_hash,
                        (SELECT e.result_json FROM extractions e
                          WHERE e.page_id = p.id
                          ORDER BY e.created_at DESC, e.id DESC LIMIT 1),
                        (SELECT s.model FROM embedding_state s WHERE s.page_id = p.id),
                        (SELECT s.chunks FROM embedding_state s WHERE s.page_id = p.id)
                 FROM pages p
                 WHERE p.outcome IN ('ok', 'not_modified')
                   AND p.authenticated = 0
                   AND p.id > ?1
                   AND (?2 IS NULL OR p.site = ?2)
                   AND (?3 IS NULL OR p.last_fetched >= ?3)
                 ORDER BY p.id
                 LIMIT ?4",
            )?;
            let rows = statement.query_map(params![after, site, since, batch as i64], |row| {
                Ok(CorpusPageRow {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    canonical_url: row.get(2)?,
                    url_hash: row.get(3)?,
                    title: row.get(4)?,
                    lang: row.get(5)?,
                    site: row.get(6)?,
                    first_seen: row.get(7)?,
                    last_fetched: row.get(8)?,
                    last_status: row.get(9)?,
                    content_type: row.get(10)?,
                    render_mode: row.get(11)?,
                    text_hash: row.get(12)?,
                    result_json: row.get(13)?,
                    embedding_model: row.get(14)?,
                    embedding_chunks: row.get(15)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    }
}
