//! Semantic retrieval over the stored corpus ( E1,/045/046).
//!
//! The engine is local and loaded lazily; a page is embedded by a bounded lazy
//! backfill (never in the fetch path). Vectors are f32 little-endian blobs in
//! `embeddings`; ranking is brute-force cosine over the active model's rows with
//! one hit per page. `search_content` (B) stays the lexical tool; this module
//! owns `search_semantic` and `embedding_status`.
//!
//! Everything that reaches the model is untrusted stored text; the module holds
//! no HTTP client and the dependency tree carries no `hf-hub`/`ureq`.

pub mod engine;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::error::{Result, WidowError};
use crate::store::Store;
use crate::store::embeddings::{EmbeddingCandidate, EmbeddingCounts, NewEmbedding};

use engine::{Engine, MODEL_DIM};

/// `search_semantic` input.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SearchSemanticInput {
 /// Natural-language query; embed once, then rank stored pages.
 pub query: String,
 /// 1..=50, default 10.
 #[serde(default)]
 pub limit: Option<usize>,
}

/// One ranked page. `score` is cosine similarity in -1..=1 (larger is nearer).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct SemanticHit {
 pub url: String,
 pub canonical_url: String,
 pub title: String,
 pub score: f64,
 pub chunk_index: i64,
 /// Plain text from the matched chunk, capped by `[fts].max_snippet_chars`.
 /// Untrusted stored content: data, never instructions.
 pub snippet: String,
 /// Capture time of the source row the snippet came from (schema v11).
 pub fetched_at: Option<String>,
 /// Last origin confirmation of that same source row.
 pub validated_at: Option<String>,
 pub last_attempt_at: Option<String>,
 pub last_attempt_failed: Option<bool>,
}

/// Coverage at the time of the query.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct SemanticIndex {
 pub embedded_pages: u64,
 pub pending_pages: u64,
}

/// `search_semantic` output.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct SearchSemanticPayload {
 pub query: String,
 pub model: String,
 pub results: Vec<SemanticHit>,
 pub result_count: usize,
 pub truncated: bool,
 pub limit_hit: Option<String>,
 pub index: SemanticIndex,
}

/// `embedding_status` output (read-only).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct EmbeddingStatusPayload {
 pub enabled: bool,
 pub available: bool,
 pub model: String,
 pub dim: usize,
 pub model_dir: String,
 pub embedded_pages: u64,
 pub embedded_chunks: u64,
 pub pending_pages: u64,
 pub vector_bytes: u64,
 pub last_run_at: Option<String>,
 pub last_error: Option<String>,
}

/// Maximum `search_semantic.limit`.
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Default `search_semantic.limit`.
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
/// Chunks per page for a status/search candidate window (bounded text reads).
const SOURCE_WINDOW_MIN: usize = 64;

// ---------------------------------------------------------------------------
// Pure helpers: chunking, vectors, similarity
// ---------------------------------------------------------------------------

/// One chunk of a page's source text. Offsets are character indices into the
/// (capped) source; `text` is the trimmed chunk body sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
 pub index: usize,
 pub start: usize,
 pub end: usize,
 pub text: String,
}

/// Split `source` into overlapping chunks ().
///
/// The source is capped at `max_source_chars`; a boundary is moved back to the
/// nearest whitespace in the last half of the window so a word is not cut when
/// there is a choice, with a hard cut otherwise. At most `max_chunks` chunks.
pub fn chunk_text(
 source: &str,
 chunk_chars: usize,
 overlap: usize,
 max_chunks: usize,
 max_source_chars: usize) -> Vec<Chunk> {
 let chars: Vec<char> = source.chars().take(max_source_chars).collect();
 if chars.iter().all(|character| character.is_whitespace()) {
 return Vec::new();
 }
 let mut chunks = Vec::new();
 let mut start = 0usize;
 while start < chars.len() && chunks.len() < max_chunks {
 let mut end = (start + chunk_chars).min(chars.len());
 if end < chars.len() {
 let floor = start + chunk_chars / 2;
 if let Some(boundary) = (floor..end)
.rev()
.find(|index| chars[*index].is_whitespace())
 && boundary > start
 {
 end = boundary;
 }
 }
 if end <= start {
 end = (start + 1).min(chars.len());
 }
 let raw: String = chars[start..end].iter().collect();
 if !raw.trim().is_empty() {
 chunks.push(Chunk {
 index: chunks.len(),
 start,
 end,
 text: raw.trim().to_string(),
 });
 }
 if end >= chars.len() {
 break;
 }
 let next = end.saturating_sub(overlap);
 start = if next <= start { end } else { next };
 }
 chunks
}

/// BLAKE3 hex of the source/chunk text.
pub fn text_hash(text: &str) -> String {
 blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// f32 little-endian blob for the `embeddings.vector` column.
pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
 let mut bytes = Vec::with_capacity(vector.len() * 4);
 for value in vector {
 bytes.extend_from_slice(&value.to_le_bytes());
 }
 bytes
}

/// Decode a stored vector; `None` when the blob is not `dim` f32 values.
pub fn decode_vector(bytes: &[u8], dim: usize) -> Option<Vec<f32>> {
 if bytes.len() != dim * 4 {
 return None;
 }
 let mut vector = Vec::with_capacity(dim);
 for index in 0..dim {
 let offset = index * 4;
 vector.push(f32::from_le_bytes([
 bytes[offset],
 bytes[offset + 1],
 bytes[offset + 2],
 bytes[offset + 3],
 ]));
 }
 Some(vector)
}

/// Cosine similarity; mismatched or zero vectors score 0.
pub fn cosine(left: &[f32], right: &[f32]) -> f64 {
 if left.len() != right.len() || left.is_empty() {
 return 0.0;
 }
 let mut dot = 0.0f64;
 let mut left_norm = 0.0f64;
 let mut right_norm = 0.0f64;
 for (left, right) in left.iter().zip(right) {
 let left = f64::from(*left);
 let right = f64::from(*right);
 dot += left * right;
 left_norm += left * left;
 right_norm += right * right;
 }
 if left_norm == 0.0 || right_norm == 0.0 {
 0.0
 } else {
 dot / (left_norm.sqrt() * right_norm.sqrt())
 }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

enum EngineSlot {
 Empty,
 Ready(Arc<Engine>),
 Failed(String),
}

#[derive(Debug, Clone, Default)]
struct RuntimeStatus {
 last_run_at: Option<String>,
 last_error: Option<String>,
}

/// The semantic index as the tools see it: lazy engine load, backfill, search
/// and status. One instance per process; production constructs it in `serve`.
pub struct EmbeddingService {
 config: Arc<Config>,
 store: Arc<Store>,
 slot: tokio::sync::Mutex<EngineSlot>,
 status: Mutex<RuntimeStatus>,
}

impl std::fmt::Debug for EmbeddingService {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("EmbeddingService")
.field("model", &self.config.embeddings.model)
.field("enabled", &self.config.embeddings.enabled)
.finish_non_exhaustive()
 }
}

fn disabled_error() -> WidowError {
 WidowError::EmbeddingUnavailable(
 "embeddings are disabled: set [embeddings].enabled = true and provision the model/library"
.to_string())
}

impl EmbeddingService {
 pub fn new(config: Arc<Config>, store: Arc<Store>) -> Self {
 Self {
 config,
 store,
 slot: tokio::sync::Mutex::new(EngineSlot::Empty),
 status: Mutex::new(RuntimeStatus::default()),
 }
 }

 /// Resolve and load the engine once. A failure is cached: repeated calls are
 /// the same typed error, not a load attempt per call.
 async fn engine(&self) -> Result<Arc<Engine>> {
 if !self.config.embeddings.enabled {
 return Err(disabled_error());
 }
 let mut slot = self.slot.lock().await;
 match &*slot {
 EngineSlot::Ready(engine) => return Ok(Arc::clone(engine)),
 EngineSlot::Failed(message) => {
 return Err(WidowError::EmbeddingUnavailable(message.clone()));
 }
 EngineSlot::Empty => {}
 }
 let settings = self.config.embeddings.clone();
 let data_dir = self.store.paths().root().to_path_buf();
 let loaded = tokio::task::spawn_blocking(move || {
 let manifest = engine::manifest(&settings.model).ok_or_else(|| {
 WidowError::EmbeddingUnavailable(format!(
 "[embeddings].model `{}` is not a known model id",
 settings.model
))
 })?;
 let paths = engine::resolve_paths(&settings, &data_dir, manifest)?;
 let engine = Engine::load(&paths, manifest)?;
 Ok::<Arc<Engine>, WidowError>(Arc::new(engine))
 })
.await
.map_err(|error| WidowError::Internal(format!("embedding load task failed: {error}")))?;
 match loaded {
 Ok(engine) => {
 *slot = EngineSlot::Ready(Arc::clone(&engine));
 Ok(engine)
 }
 Err(error) => {
 let message = error.to_string();
 tracing::warn!(error = %message, "embedding engine unavailable");
 *slot = EngineSlot::Failed(message.clone());
 Err(WidowError::EmbeddingUnavailable(message))
 }
 }
 }

 fn slot_failure(&self) -> Option<String> {
 // The slot is async; this helper is only used by the sync status path,
 // which reads the cached message under `try_lock` where possible.
 match self.slot.try_lock() {
 Ok(slot) => match &*slot {
 EngineSlot::Failed(message) => Some(message.clone()),
 _ => None,
 },
 Err(_) => None,
 }
 }

 fn model_dir(&self) -> String {
 let settings = &self.config.embeddings;
 match &settings.model_dir {
 Some(dir) => dir.display().to_string(),
 None => self
.store
.paths()
.root()
.join("models")
.join(&settings.model)
.display()
.to_string(),
 }
 }

 fn set_error(&self, message: &str) {
 let mut status = self
.status
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner());
 status.last_error = Some(message.to_string());
 }

 fn set_run(&self, at: String) {
 let mut status = self
.status
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner());
 status.last_run_at = Some(at);
 }

 /// `embedding_status`: honest coverage counts; never fails on a missing
 /// artifact (it reports `available = false` and the cached reason).
 pub async fn status(&self) -> Result<EmbeddingStatusPayload> {
 let settings = &self.config.embeddings;
 let model = settings.model.clone();
 let counts: EmbeddingCounts = self.store.db.embedding_counts(&model).await?;
 let dim = engine::manifest(&model).map_or(0, |manifest| manifest.dim);
 let available = if settings.enabled {
 self.engine().await.is_ok()
 } else {
 false
 };
 let runtime = self
.status
.lock()
.unwrap_or_else(|poisoned| poisoned.into_inner())
.clone();
 let last_error = if settings.enabled {
 runtime.last_error.or_else(|| self.slot_failure())
 } else {
 None
 };
 Ok(EmbeddingStatusPayload {
 enabled: settings.enabled,
 available,
 model,
 dim,
 model_dir: self.model_dir(),
 embedded_pages: counts.embedded_pages,
 embedded_chunks: counts.embedded_chunks,
 pending_pages: counts.pending_pages,
 vector_bytes: counts.vector_bytes,
 last_run_at: runtime.last_run_at,
 last_error,
 })
 }

 /// `search_semantic`: embed the query once and rank one hit per page.
 pub async fn search(&self, input: SearchSemanticInput) -> Result<SearchSemanticPayload> {
 let settings = &self.config.embeddings;
 let query = crate::fts::validate_query(&input.query, settings.query_max_chars)?;
 let limit = input.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
 crate::fts::validate_limit(limit, MAX_SEARCH_LIMIT)?;
 let model = settings.model.clone();
 let engine = self.engine().await?;

 let query_engine = Arc::clone(&engine);
 let query_text = query.clone();
 let mut vectors = tokio::task::spawn_blocking(move || query_engine.embed(&[query_text]))
.await
.map_err(|error| {
 WidowError::EmbeddingFailed(format!("query embedding task failed: {error}"))
 })??;
 let query_vector = vectors.pop().ok_or_else(|| {
 WidowError::EmbeddingFailed("the query produced no vector".to_string())
 })?;

 let dim = engine.dim();
 let rows = self.store.db.semantic_chunks(&model).await?;
 let mut best: HashMap<i64, BestChunk> = HashMap::new();
 let mut skipped_vectors = 0usize;
 for row in rows {
 let Some(vector) = decode_vector(&row.vector, dim) else {
 skipped_vectors += 1;
 continue;
 };
 let score = cosine(&query_vector, &vector);
 let entry = BestChunk {
 score,
 chunk_index: row.chunk_index,
 source_hash: row.source_hash,
 start: row.start_char,
 end: row.end_char,
 url: row.url,
 canonical_url: row.canonical_url,
 title: row.title.unwrap_or_default(),
 };
 match best.get(&row.page_id) {
 Some(current)
 if current.score > score
 || (current.score == score && current.chunk_index <= row.chunk_index) => {}
 _ => {
 best.insert(row.page_id, entry);
 }
 }
 }
 if skipped_vectors > 0 {
 tracing::warn!(skipped_vectors, "ignored malformed stored vectors");
 }
 let total_pages = best.len();
 let mut ranked: Vec<(i64, BestChunk)> = best.into_iter().collect();
 ranked.sort_by(|left, right| {
 right
.1
.score
.partial_cmp(&left.1.score)
.unwrap_or(std::cmp::Ordering::Equal)
.then_with(|| left.1.chunk_index.cmp(&right.1.chunk_index))
.then_with(|| right.0.cmp(&left.0))
 });

 // Read the current source only for the pages we may return; a page whose
 // text changed since it was embedded is skipped (its vectors are stale).
 let window = ranked.len().min((limit * 4).max(SOURCE_WINDOW_MIN));
 let ids: Vec<i64> = ranked.iter().take(window).map(|(id, _)| *id).collect();
 let sources = self.store.db.page_sources(&ids).await?;
 let mut results = Vec::with_capacity(limit.min(ranked.len()));
 for (page_id, chunk) in ranked.iter() {
 if results.len() >= limit {
 break;
 }
 let Some(source) = sources.get(page_id) else {
 continue;
 };
 let Some(text) =
 crate::store::fts::index_text_from_stored(source.result_json.as_deref())
 else {
 continue;
 };
 if text_hash(&text) != chunk.source_hash {
 continue;
 }
 let freshness = source.freshness();
 results.push(SemanticHit {
 url: chunk.url.clone(),
 canonical_url: chunk.canonical_url.clone(),
 title: chunk.title.clone(),
 score: chunk.score,
 chunk_index: chunk.chunk_index,
 snippet: snippet_for(
 &text,
 chunk.start,
 chunk.end,
 self.config.fts.max_snippet_chars),
 fetched_at: freshness.fetched_at,
 validated_at: freshness.validated_at,
 last_attempt_at: freshness.last_attempt_at,
 last_attempt_failed: freshness.last_attempt_failed,
 });
 }
 let counts = self.store.db.embedding_counts(&model).await?;
 let truncated = total_pages > results.len();
 Ok(SearchSemanticPayload {
 query,
 model,
 results,
 result_count: total_pages,
 truncated,
 limit_hit: truncated.then(|| "limit".to_string()),
 index: SemanticIndex {
 embedded_pages: counts.embedded_pages,
 pending_pages: counts.pending_pages,
 },
 })
 }

 /// Delete rows for every model except the active one (startup, bounded).
 pub async fn prune_non_active_model(&self) -> Result<usize> {
 let settings = &self.config.embeddings;
 if !settings.enabled {
 return Ok(0);
 }
 let pruned = self
.store
.db
.prune_embeddings_except(&settings.model)
.await?;
 if pruned > 0 {
 tracing::info!(
 pruned,
 model = %settings.model,
 "pruned embedding rows for a non-active model"
);
 }
 Ok(pruned)
 }

 /// One bounded backfill pass. Returns pages embedded (0 when there was
 /// nothing to do). Errors are returned so the loop can log and wait.
 pub async fn backfill_pass(&self) -> Result<usize> {
 let settings = &self.config.embeddings;
 if !settings.enabled {
 return Ok(0);
 }
 let model = settings.model.clone();
 let engine = self.engine().await?;
 let candidates = self
.store
.db
.embedding_candidates(
 &model,
 settings.backfill_pages_per_pass as i64,
 self.config.auth.index_authenticated)
.await?;
 let mut embedded = 0usize;
 let mut skipped = 0usize;
 for candidate in candidates {
 match self
.embed_candidate(&engine, &model, settings, &candidate)
.await
 {
 Ok(true) => embedded += 1,
 Ok(false) => skipped += 1,
 Err(error) => {
 self.set_error(&error.to_string());
 return Err(error);
 }
 }
 }
 if skipped > 0 {
 tracing::debug!(skipped, "backfill skipped pages with no embeddable text");
 }
 let at = crate::store::db::fmt_time(Utc::now());
 self.set_run(at);
 Ok(embedded)
 }

 /// `Ok(false)` means the page had no embeddable text (state recorded).
 async fn embed_candidate(
 &self,
 engine: &Arc<Engine>,
 model: &str,
 settings: &crate::config::EmbeddingsConfig,
 candidate: &EmbeddingCandidate) -> Result<bool> {
 let text = crate::store::fts::index_text_from_stored(candidate.result_json.as_deref())
.unwrap_or_default();
 let source_hash = text_hash(&text);
 let at = crate::store::db::fmt_time(Utc::now());
 if text.trim().is_empty() {
 self.store
.db
.write_embedding_state(&source_hash, candidate.page_id, model, 0, &at)
.await?;
 return Ok(false);
 }
 let chunks = chunk_text(
 &text,
 settings.chunk_chars,
 settings.chunk_overlap,
 settings.max_chunks_per_page,
 settings.max_source_chars);
 if chunks.is_empty() {
 self.store
.db
.write_embedding_state(&source_hash, candidate.page_id, model, 0, &at)
.await?;
 return Ok(false);
 }
 let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
 let engine = Arc::clone(engine);
 let vectors = tokio::task::spawn_blocking(move || engine.embed(&texts))
.await
.map_err(|error| {
 WidowError::EmbeddingFailed(format!("chunk embedding task failed: {error}"))
 })??;
 let rows: Vec<NewEmbedding> = chunks
.iter()
.zip(vectors)
.map(|(chunk, vector)| NewEmbedding {
 chunk_index: chunk.index as i64,
 chunk_hash: text_hash(&chunk.text),
 start_char: chunk.start as i64,
 end_char: chunk.end as i64,
 dim: vector.len() as i64,
 vector: encode_vector(&vector),
 })
.collect();
 self.store
.db
.replace_page_embeddings(candidate.page_id, model, &source_hash, &rows, &at)
.await?;
 Ok(true)
 }

 /// Spawn the lazy backfill loop (serve only). The first pass runs at once;
 /// then one pass per `backfill_interval_ms` with `MissedTickBehavior::Delay`.
 pub fn spawn_backfill(
 self: Arc<Self>,
 cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
 let interval =
 Duration::from_millis(self.config.embeddings.backfill_interval_ms.max(1_000));
 tokio::spawn(async move {
 match self.prune_non_active_model().await {
 Ok(_) => {}
 Err(error) => tracing::warn!(error = %error, "embedding prune failed"),
 }
 let mut ticker = tokio::time::interval(interval);
 ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
 loop {
 tokio::select! {
 () = cancel.cancelled() => {
 tracing::info!("embedding backfill stopped");
 break;
 }
 _ = ticker.tick() => {
 match self.backfill_pass().await {
 Ok(0) => {}
 Ok(pages) => tracing::info!(pages, "embedding backfill pass complete"),
 Err(error) => {
 self.set_error(&error.to_string());
 tracing::warn!(error = %error, "embedding backfill pass failed");
 }
 }
 }
 }
 }
 })
 }
}

#[derive(Debug, Clone)]
struct BestChunk {
 score: f64,
 chunk_index: i64,
 source_hash: String,
 start: i64,
 end: i64,
 url: String,
 canonical_url: String,
 title: String,
}

fn snippet_for(text: &str, start: i64, end: i64, max_chars: usize) -> String {
 let chars: Vec<char> = text.chars().collect();
 let start = start.max(0) as usize;
 let end = end.max(0) as usize;
 if start >= chars.len() || end <= start {
 return crate::core::truncate::truncate_chars(text, max_chars).0;
 }
 let end = end.min(chars.len());
 let snippet: String = chars[start..end].iter().collect();
 crate::core::truncate::truncate_chars(snippet.trim(), max_chars).0
}

/// The v1 dimension, exposed for tests and `embedding_status` before load.
pub const fn default_dim() -> usize {
 MODEL_DIM
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn chunking_respects_offsets_overlap_and_the_cap() {
 let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
 let chunks = chunk_text(text, 20, 5, 10, 10_000);
 assert!(!chunks.is_empty());
 for chunk in &chunks {
 assert!(chunk.end > chunk.start);
 assert_eq!(
 text.chars()
.skip(chunk.start)
.take(chunk.end - chunk.start)
.collect::<String>()
.trim(),
 chunk.text
);
 }
 assert!(chunks.len() < 10, "the source is short");
 }

 #[test]
 fn chunking_caps_chunks_and_source() {
 let text = "word ".repeat(500);
 let chunks = chunk_text(&text, 50, 10, 3, 10_000);
 assert_eq!(chunks.len(), 3);
 assert_eq!(chunks[0].index, 0);
 assert_eq!(chunks[2].index, 2);
 let capped = chunk_text(&text, 50, 10, 100, 120);
 assert!(capped.iter().all(|chunk| chunk.end <= 120));
 }

 #[test]
 fn whitespace_never_becomes_a_chunk() {
 assert!(chunk_text(" \n\t ", 100, 10, 10, 10_000).is_empty());
 assert!(chunk_text("", 100, 10, 10, 10_000).is_empty());
 }

 #[test]
 fn vectors_round_trip_and_reject_a_wrong_length() {
 let vector = vec![0.5f32, -1.25, 3.0, 0.0];
 let bytes = encode_vector(&vector);
 assert_eq!(bytes.len(), 16);
 assert_eq!(decode_vector(&bytes, 4), Some(vector));
 assert_eq!(decode_vector(&bytes, 3), None);
 assert_eq!(decode_vector(&[0, 0], 4), None);
 }

 #[test]
 fn cosine_is_one_for_identical_and_zero_for_orthogonal() {
 let a = [1.0f32, 2.0, 3.0];
 assert!((cosine(&a, &a) - 1.0).abs() < 1e-9);
 assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
 assert_eq!(cosine(&[0.0, 0.0], &a), 0.0);
 assert_eq!(cosine(&[1.0], &a), 0.0);
 }

 #[test]
 fn snippet_slices_the_match_and_clamps() {
 assert_eq!(snippet_for("abcdefghij", 2, 5, 100), "cde");
 assert_eq!(snippet_for("abcdefghij", 8, 99, 100), "ij");
 assert_eq!(snippet_for("abcdefghij", 0, 99, 4), "abcd");
 assert_eq!(snippet_for("abcdefghij", 50, 60, 100), "abcdefghij");
 }

 #[test]
 fn source_hashes_are_stable_and_distinct() {
 assert_eq!(text_hash("same"), text_hash("same"));
 assert_ne!(text_hash("one"), text_hash("two"));
 }

 #[test]
 fn the_default_dimension_matches_the_manifest() {
 assert_eq!(default_dim(), 384);
 }
}
