//! Embedding persistence (M07 E1, IMP-DEC-044): candidates, vector rows,
//! state, coverage counts and the reads `search_semantic` needs.
//!
//! All writes go through the single writer closure; vectors are f32
//! little-endian blobs whose length the schema pins to `dim * 4`.

use std::collections::HashMap;

use rusqlite::{params, params_from_iter};

use crate::core::error::Result;

use super::db::Database;

/// The source shape shared by the candidate scan and the read-time hash check:
/// the most recent extraction that carries delivered Markdown, else the most
/// recent extraction of any shape.
const SOURCE_JSON: &str = "(SELECT e.result_json FROM extractions e
        WHERE e.page_id = p.id
          AND CASE WHEN json_valid(e.result_json)
                   THEN json_type(e.result_json, '$.markdown') = 'text'
                   ELSE 0 END
        ORDER BY e.created_at DESC, e.id DESC LIMIT 1)";

const LATEST_JSON: &str = "(SELECT e.result_json FROM extractions e
        WHERE e.page_id = p.id
        ORDER BY e.created_at DESC, e.id DESC LIMIT 1)";

/// One page the backfill should (re)embed.
#[derive(Debug, Clone)]
pub struct EmbeddingCandidate {
    pub page_id: i64,
    /// Markdown-first source payload; `None` when the page has no extraction.
    pub result_json: Option<String>,
}

/// One vector row to insert for a page.
#[derive(Debug, Clone)]
pub struct NewEmbedding {
    pub chunk_index: i64,
    pub chunk_hash: String,
    pub start_char: i64,
    pub end_char: i64,
    pub dim: i64,
    pub vector: Vec<u8>,
}

/// Coverage counters for `embedding_status` and `search_semantic.index`.
#[derive(Debug, Clone, Default)]
pub struct EmbeddingCounts {
    pub embedded_pages: u64,
    pub embedded_chunks: u64,
    pub pending_pages: u64,
    pub vector_bytes: u64,
}

/// One stored vector with the page metadata ranking returns.
#[derive(Debug, Clone)]
pub struct SemanticChunkRow {
    pub page_id: i64,
    pub chunk_index: i64,
    pub source_hash: String,
    pub start_char: i64,
    pub end_char: i64,
    pub vector: Vec<u8>,
    pub url: String,
    pub canonical_url: String,
    pub title: Option<String>,
}

impl Database {
    /// Pages with a stored extraction whose state is missing, for another
    /// model, or older than a newer extraction; newest first.
    pub async fn embedding_candidates(
        &self,
        model: &str,
        limit: i64,
    ) -> Result<Vec<EmbeddingCandidate>> {
        let model = model.to_string();
        self.read(move |connection| {
            let sql = format!(
                "SELECT p.id, COALESCE({SOURCE_JSON}, {LATEST_JSON})
                 FROM pages p
                 LEFT JOIN embedding_state s ON s.page_id = p.id
                 WHERE EXISTS (SELECT 1 FROM extractions e WHERE e.page_id = p.id)
                   AND (s.page_id IS NULL
                        OR s.model <> ?1
                        OR EXISTS (SELECT 1 FROM extractions e
                                   WHERE e.page_id = p.id AND e.created_at > s.embedded_at))
                 ORDER BY COALESCE(p.last_fetched, p.first_seen) DESC, p.id DESC
                 LIMIT ?2"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params![model, limit], |row| {
                Ok(EmbeddingCandidate {
                    page_id: row.get(0)?,
                    result_json: row.get(1)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    }

    /// Replace a page's rows for the active model and advance its state in one
    /// transaction. Rows for another model are pruned at the same boundary.
    pub async fn replace_page_embeddings(
        &self,
        page_id: i64,
        model: &str,
        source_hash: &str,
        rows: &[NewEmbedding],
        embedded_at: &str,
    ) -> Result<()> {
        let model = model.to_string();
        let source_hash = source_hash.to_string();
        let embedded_at = embedded_at.to_string();
        let rows = rows.to_vec();
        let chunk_count = rows.len() as i64;
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "DELETE FROM embeddings WHERE page_id = ?1 AND model <> ?2",
                params![page_id, model],
            )?;
            transaction.execute(
                "DELETE FROM embeddings WHERE page_id = ?1 AND model = ?2",
                params![page_id, model],
            )?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO embeddings
                         (page_id, model, chunk_index, source_hash, chunk_hash,
                          start_char, end_char, dim, vector, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )?;
                for row in &rows {
                    statement.execute(params![
                        page_id,
                        model,
                        row.chunk_index,
                        source_hash,
                        row.chunk_hash,
                        row.start_char,
                        row.end_char,
                        row.dim,
                        &row.vector,
                        embedded_at,
                    ])?;
                }
            }
            transaction.execute(
                "INSERT INTO embedding_state (page_id, model, source_hash, chunks, embedded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(page_id) DO UPDATE SET
                     model = excluded.model,
                     source_hash = excluded.source_hash,
                     chunks = excluded.chunks,
                     embedded_at = excluded.embedded_at",
                params![page_id, model, source_hash, chunk_count, embedded_at],
            )?;
            transaction.commit()
        })
        .await
    }

    /// Record a page with no embeddable text so the scan does not revisit it.
    pub async fn write_embedding_state(
        &self,
        source_hash: &str,
        page_id: i64,
        model: &str,
        chunks: i64,
        embedded_at: &str,
    ) -> Result<()> {
        let source_hash = source_hash.to_string();
        let model = model.to_string();
        let embedded_at = embedded_at.to_string();
        self.write(move |connection| {
            connection.execute(
                "INSERT INTO embedding_state (page_id, model, source_hash, chunks, embedded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(page_id) DO UPDATE SET
                     model = excluded.model,
                     source_hash = excluded.source_hash,
                     chunks = excluded.chunks,
                     embedded_at = excluded.embedded_at",
                params![page_id, model, source_hash, chunks, embedded_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete every embedding row/state for a model other than `model`.
    pub async fn prune_embeddings_except(&self, model: &str) -> Result<usize> {
        let model = model.to_string();
        self.write_with(move |connection| {
            let transaction = connection.transaction()?;
            let embeddings =
                transaction.execute("DELETE FROM embeddings WHERE model <> ?1", params![model])?;
            let states = transaction.execute(
                "DELETE FROM embedding_state WHERE model <> ?1",
                params![model],
            )?;
            transaction.commit()?;
            Ok(embeddings + states)
        })
        .await
    }

    /// Coverage counts for the active model.
    pub async fn embedding_counts(&self, model: &str) -> Result<EmbeddingCounts> {
        let model = model.to_string();
        self.read(move |connection| {
            let embedded_pages: i64 = connection.query_row(
                "SELECT COUNT(*) FROM embedding_state WHERE model = ?1 AND chunks > 0",
                params![model],
                |row| row.get(0),
            )?;
            let embedded_chunks: i64 = connection.query_row(
                "SELECT COUNT(*) FROM embeddings WHERE model = ?1",
                params![model],
                |row| row.get(0),
            )?;
            let vector_bytes: i64 = connection.query_row(
                "SELECT COALESCE(SUM(length(vector)), 0) FROM embeddings WHERE model = ?1",
                params![model],
                |row| row.get(0),
            )?;
            let pending_pages: i64 = connection.query_row(
                "SELECT COUNT(*) FROM pages p
                 LEFT JOIN embedding_state s ON s.page_id = p.id
                 WHERE EXISTS (SELECT 1 FROM extractions e WHERE e.page_id = p.id)
                   AND (s.page_id IS NULL
                        OR s.model <> ?1
                        OR EXISTS (SELECT 1 FROM extractions e
                                   WHERE e.page_id = p.id AND e.created_at > s.embedded_at))",
                params![model],
                |row| row.get(0),
            )?;
            Ok(EmbeddingCounts {
                embedded_pages: u64::try_from(embedded_pages).unwrap_or(0),
                embedded_chunks: u64::try_from(embedded_chunks).unwrap_or(0),
                pending_pages: u64::try_from(pending_pages).unwrap_or(0),
                vector_bytes: u64::try_from(vector_bytes).unwrap_or(0),
            })
        })
        .await
    }

    /// Every stored vector for the active model, with its page metadata.
    pub async fn semantic_chunks(&self, model: &str) -> Result<Vec<SemanticChunkRow>> {
        let model = model.to_string();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT e.page_id, e.chunk_index, e.source_hash, e.start_char, e.end_char,
                        e.vector, p.url, p.canonical_url, p.title
                 FROM embeddings e
                 JOIN pages p ON p.id = e.page_id
                 WHERE e.model = ?1
                 ORDER BY e.page_id, e.chunk_index",
            )?;
            let rows = statement.query_map(params![model], |row| {
                Ok(SemanticChunkRow {
                    page_id: row.get(0)?,
                    chunk_index: row.get(1)?,
                    source_hash: row.get(2)?,
                    start_char: row.get(3)?,
                    end_char: row.get(4)?,
                    vector: row.get(5)?,
                    url: row.get(6)?,
                    canonical_url: row.get(7)?,
                    title: row.get(8)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    }

    /// Current source payload for a bounded set of pages, same markdown-first
    /// shape as the backfill. Missing ids are absent from the map.
    pub async fn page_sources(&self, page_ids: &[i64]) -> Result<HashMap<i64, Option<String>>> {
        if page_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = page_ids.to_vec();
        let placeholders = vec!["?"; ids.len()].join(",");
        self.read(move |connection| {
            let sql = format!(
                "SELECT p.id, COALESCE({SOURCE_JSON}, {LATEST_JSON})
                 FROM pages p WHERE p.id IN ({placeholders})"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            let mut sources = HashMap::new();
            for row in rows {
                let (id, json) = row?;
                sources.insert(id, json);
            }
            Ok(sources)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FtsConfig;
    use crate::store::db::Database;
    use chrono::Utc;
    use std::path::PathBuf;

    fn open() -> (Database, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("embeddings.db");
        let db = Database::open(&path, 1, &FtsConfig::default()).unwrap();
        (db, path)
    }

    fn insert_page(connection: &rusqlite::Connection, page_id: i64, text: &str, at: &str) {
        connection
            .execute(
                "INSERT INTO pages (id, url, canonical_url, url_hash, first_seen, last_fetched, outcome)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?4, 'ok')",
                params![
                    page_id,
                    format!("https://example.com/{page_id}"),
                    format!("{page_id:064x}"),
                    at
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO fetch_events (page_id, started_at, duration_ms, mode)
                 VALUES (?1, ?2, 0, 'fetch')",
                params![page_id, at],
            )
            .unwrap();
        let event_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO extractions (page_id, fetch_event_id, result_json, result_hash,
                                          record_count, created_at)
                 VALUES (?1, ?2, ?3, 'r', 1, ?4)",
                params![
                    page_id,
                    event_id,
                    serde_json::json!({"markdown": text}).to_string(),
                    at
                ],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn candidates_state_counts_and_prune_round_trip() {
        let (db, path) = open();
        let at = crate::store::db::fmt_time(Utc::now());
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .unwrap();
            insert_page(&connection, 1, "first page text", &at);
            insert_page(&connection, 2, "second page text", &at);
        }
        let model = "minilm-l6-v2-q";
        let candidates = db.embedding_candidates(model, 10).await.unwrap();
        assert_eq!(candidates.len(), 2);

        let rows = vec![NewEmbedding {
            chunk_index: 0,
            chunk_hash: "h".repeat(64),
            start_char: 0,
            end_char: 4,
            dim: 1,
            vector: 1.0f32.to_le_bytes().to_vec(),
        }];
        db.replace_page_embeddings(1, model, "s1", &rows, &at)
            .await
            .unwrap();
        let counts = db.embedding_counts(model).await.unwrap();
        assert_eq!(counts.embedded_pages, 1);
        assert_eq!(counts.embedded_chunks, 1);
        assert_eq!(counts.pending_pages, 1);
        assert_eq!(counts.vector_bytes, 4);

        let remaining = db.embedding_candidates(model, 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].page_id, 2);

        db.prune_embeddings_except("other-model").await.unwrap();
        let counts = db.embedding_counts(model).await.unwrap();
        assert_eq!(counts.embedded_pages, 0);
        assert_eq!(counts.embedded_chunks, 0);
    }

    #[tokio::test]
    async fn a_newer_extraction_makes_the_page_a_candidate_again() {
        let (db, path) = open();
        let model = "minilm-l6-v2-q";
        let first = crate::store::db::fmt_time(Utc::now());
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .unwrap();
            insert_page(&connection, 1, "first page text", &first);
        }
        let candidates = db.embedding_candidates(model, 10).await.unwrap();
        let text = crate::store::fts::index_text_from_stored(candidates[0].result_json.as_deref())
            .unwrap();
        db.replace_page_embeddings(
            1,
            model,
            &crate::embeddings::text_hash(&text),
            &[NewEmbedding {
                chunk_index: 0,
                chunk_hash: "h".repeat(64),
                start_char: 0,
                end_char: 4,
                dim: 1,
                vector: 1.0f32.to_le_bytes().to_vec(),
            }],
            &first,
        )
        .await
        .unwrap();
        assert!(db.embedding_candidates(model, 10).await.unwrap().is_empty());

        let later = crate::store::db::fmt_time(Utc::now() + chrono::Duration::seconds(1));
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .unwrap();
            let event_id: i64 = connection
                .query_row("SELECT id FROM fetch_events WHERE page_id = 1", [], |row| {
                    row.get(0)
                })
                .unwrap();
            connection
                .execute(
                    "INSERT INTO extractions (page_id, fetch_event_id, result_json, result_hash,
                                              record_count, created_at)
                     VALUES (1, ?1, ?2, 'r2', 1, ?3)",
                    params![
                        event_id,
                        serde_json::json!({"markdown": "changed page text"}).to_string(),
                        later
                    ],
                )
                .unwrap();
        }
        let candidates = db.embedding_candidates(model, 10).await.unwrap();
        assert_eq!(
            candidates.len(),
            1,
            "a newer extraction invalidates the state"
        );
    }
}
