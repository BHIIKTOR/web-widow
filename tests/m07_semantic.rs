//! E1 integration: local semantic index, backfill, invalidation and the
//! artifact-gated engine contract.
//!
//! The model/runtime are provisioned artifacts, not crates. Like the render and
//! PDF suites, a test that needs them prints `SKIP` and returns when
//! `WEB_WIDOW_ORT_PATH` / `WEB_WIDOW_MODEL_DIR` are absent, so the ordinary suite
//! stays hermetic. The gate and the E1 drill set both variables.

mod common;

use std::path::PathBuf;

use common::{TestEnv, start_fixture, test_config};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use web_widow::config::Config;
use web_widow::core::error::ErrorCode;
use web_widow::embeddings::{EmbeddingService, SearchSemanticInput};
use web_widow::export::CorpusExportInput;
use web_widow::fts::SearchContentInput;

fn artifacts() -> Option<(PathBuf, PathBuf)> {
 let library = PathBuf::from(std::env::var_os("WEB_WIDOW_ORT_PATH")?);
 let model = PathBuf::from(std::env::var_os("WEB_WIDOW_MODEL_DIR")?);
 (library.is_file() && model.is_dir()).then_some((library, model))
}

fn semantic_config(library: PathBuf, model: PathBuf) -> Config {
 let mut config = test_config();
 config.embeddings.enabled = true;
 config.embeddings.ort_dylib_path = Some(library);
 config.embeddings.model_dir = Some(model);
 config.embeddings.backfill_interval_ms = 1_000;
 config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_search_finds_a_paraphrase_that_fts_misses() {
 let Some((library, model)) = artifacts() else {
 eprintln!(
 "SKIP semantic_search_finds_a_paraphrase_that_fts_misses: no WEB_WIDOW_ORT_PATH/WEB_WIDOW_MODEL_DIR"
);
 return;
 };
 let fixture = start_fixture().await;
 let env = TestEnv::new(semantic_config(library, model)).await;
 let article = fixture.url("127.0.0.1", "/article");
 let large = fixture.url("127.0.0.1", "/large");
 env.fetch(&article).await.unwrap();
 env.fetch(&large).await.unwrap();

 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 assert_eq!(
 service.backfill_pass().await.unwrap(),
 2,
 "both pages embedded"
);
 let status = service.status().await.unwrap();
 assert!(status.enabled && status.available);
 assert_eq!(status.model, "minilm-l6-v2-q");
 assert_eq!(status.dim, 384);
 assert_eq!(status.embedded_pages, 2);
 assert!(status.embedded_chunks >= 2, "{status:?}");
 assert_eq!(status.pending_pages, 0, "{status:?}");
 assert!(status.vector_bytes >= 2 * 384 * 4, "{status:?}");
 assert_eq!(
 service.backfill_pass().await.unwrap(),
 0,
 "unchanged pages are skipped"
);
 // A fresh service (a restarted process) resumes from the persisted state.
 let restarted = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 assert_eq!(
 restarted.backfill_pass().await.unwrap(),
 0,
 "a restarted backfill has nothing to re-embed"
);

 // A paraphrase with no lexical overlap: FTS must miss it, semantics must not.
 let query = "seabird tally at a beacon run by a keeper";
 let lexical = env
.content
.search(SearchContentInput {
 query: query.to_string(),
 limit: None,
 })
.await
.unwrap();
 assert_eq!(
 lexical.result_count, 0,
 "FTS is expected to miss: {lexical:?}"
);
 let semantic = service
.search(SearchSemanticInput {
 query: query.to_string(),
 limit: None,
 })
.await
.unwrap();
 assert!(semantic.result_count >= 1, "{semantic:?}");
 assert_eq!(semantic.results[0].canonical_url, article);
 assert!(semantic.results[0].score > 0.0);
 let snippet = semantic.results[0].snippet.to_lowercase();
 assert!(
 snippet.contains("lighthouse") || snippet.contains("gulls"),
 "the snippet comes from the matched chunk: {snippet}"
);
 assert_eq!(semantic.index.embedded_pages, 2);
 assert_eq!(semantic.index.pending_pages, 0);

 // `limit`/`truncated`: two pages have vectors, one is returned.
 let limited = service
.search(SearchSemanticInput {
 query: query.to_string(),
 limit: Some(1),
 })
.await
.unwrap();
 assert_eq!(limited.results.len(), 1, "{limited:?}");
 assert_eq!(limited.result_count, 2, "{limited:?}");
 assert!(limited.truncated);
 assert_eq!(limited.limit_hit.as_deref(), Some("limit"));

 // Additive corpus-export metadata; vectors are never inlined.
 let export = env
.pipeline
.corpus_export(CorpusExportInput {
 format: "jsonl".to_string(),
 name: "semantic".to_string(),
 site: None,
 since: None,
 limit: None,
 include_text: None,
 include_extraction: None,
 max_bytes: None,
 })
.await
.unwrap();
 let body = std::fs::read_to_string(&export.path).unwrap();
 let row: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
 assert_eq!(row["embedding_model"], "minilm-l6-v2-q");
 assert!(row["embedding_chunks"].as_u64().unwrap() >= 1, "{row}");
 assert!(row.get("vector").is_none(), "vectors are not inlined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refetched_page_is_reembedded_and_the_old_text_is_gone() {
 let Some((library, model)) = artifacts() else {
 eprintln!("SKIP a_refetched_page_is_reembedded_and_the_old_text_is_gone: no artifacts");
 return;
 };
 let fixture = start_fixture().await;
 fixture.set_page(
 "/monitor/semantic",
 "<html><head><title>Station</title></head><body><main><article>\
 <h1>Station</h1><p>volcanic basalt eruption monitoring station on the ridge</p>\
 </article></main></body></html>");
 let env = TestEnv::new(semantic_config(library, model)).await;
 let url = fixture.url("127.0.0.1", "/monitor/semantic");
 env.fetch(&url).await.unwrap();
 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 assert_eq!(service.backfill_pass().await.unwrap(), 1);
 let before = service
.search(SearchSemanticInput {
 query: "volcanic eruption".to_string(),
 limit: None,
 })
.await
.unwrap();
 assert_eq!(before.results[0].canonical_url, url);
 assert!(
 before.results[0]
.snippet
.to_lowercase()
.contains("volcanic")
);

 tokio::time::sleep(std::time::Duration::from_millis(5)).await;
 fixture.set_page(
 "/monitor/semantic",
 "<html><head><title>Station</title></head><body><main><article>\
 <h1>Station</h1><p>coral reef bleaching survey along the tropical shallows</p>\
 </article></main></body></html>");
 env.fetch(&url).await.unwrap();
 assert_eq!(
 service.backfill_pass().await.unwrap(),
 1,
 "the newer extraction invalidates the page"
);
 let after = service
.search(SearchSemanticInput {
 query: "coral reef bleaching".to_string(),
 limit: None,
 })
.await
.unwrap();
 assert_eq!(after.results[0].canonical_url, url);
 let snippet = after.results[0].snippet.to_lowercase();
 assert!(snippet.contains("coral"), "{snippet}");
 assert!(
 !snippet.contains("volcanic"),
 "old chunks are gone: {snippet}"
);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_active_model_state_is_pruned_at_startup() {
 let Some((library, model)) = artifacts() else {
 eprintln!("SKIP a_non_active_model_state_is_pruned_at_startup: no artifacts");
 return;
 };
 let fixture = start_fixture().await;
 let env = TestEnv::new(semantic_config(library, model)).await;
 env.fetch(&fixture.url("127.0.0.1", "/article"))
.await
.unwrap();
 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 assert_eq!(service.backfill_pass().await.unwrap(), 1);

 let connection = env.db();
 let page_id: i64 = connection
.query_row("SELECT id FROM pages LIMIT 1", [], |row| row.get(0))
.unwrap();
 connection
.execute(
 "INSERT INTO embeddings (page_id, model, chunk_index, source_hash, chunk_hash,
 start_char, end_char, dim, vector, created_at)
 VALUES (?1, 'old-model', 0, 's', 'h', 0, 4, 1, ?2, '2026-01-01T00:00:00.000Z')",
 rusqlite::params![page_id, 1.0f32.to_le_bytes().to_vec()])
.unwrap();
 assert_eq!(service.prune_non_active_model().await.unwrap(), 1);
 let old: i64 = connection
.query_row(
 "SELECT COUNT(*) FROM embeddings WHERE model = 'old-model'",
 [],
 |row| row.get(0))
.unwrap();
 assert_eq!(old, 0, "the non-active model is pruned");
}

#[tokio::test]
async fn disabled_and_missing_artifacts_are_typed_and_status_is_honest() {
 let env = TestEnv::new(test_config()).await;
 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 let error = service
.search(SearchSemanticInput {
 query: "anything".to_string(),
 limit: None,
 })
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::EmbeddingUnavailable);
 let status = service.status().await.unwrap();
 assert!(!status.enabled);
 assert!(!status.available);
 assert_eq!(status.model, "minilm-l6-v2-q");
 assert_eq!(status.dim, 384);

 let mut config = test_config();
 config.embeddings.enabled = true;
 config.embeddings.ort_dylib_path = Some(PathBuf::from("/nonexistent/libonnxruntime.so"));
 let env = TestEnv::new(config).await;
 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 let status = service.status().await.unwrap();
 assert!(status.enabled);
 assert!(!status.available);
 assert!(
 status
.last_error
.as_deref()
.unwrap_or_default()
.contains("[embeddings].ort_dylib_path"),
 "{status:?}"
);
 let error = service
.search(SearchSemanticInput {
 query: "anything".to_string(),
 limit: None,
 })
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::EmbeddingUnavailable);
}

#[tokio::test]
async fn a_corrupted_model_file_is_typed_and_names_the_file() {
 let dir = tempfile::tempdir().unwrap();
 let library = dir.path().join("libonnxruntime.so");
 std::fs::write(&library, b"not a real library").unwrap();
 let model_dir = dir.path().join("model");
 std::fs::create_dir_all(model_dir.join("onnx")).unwrap();
 std::fs::write(model_dir.join("onnx/model_quantized.onnx"), b"tampered").unwrap();

 let mut config = test_config();
 config.embeddings.enabled = true;
 config.embeddings.ort_dylib_path = Some(library);
 config.embeddings.model_dir = Some(model_dir);
 let env = TestEnv::new(config).await;
 let service = EmbeddingService::new(
 std::sync::Arc::clone(&env.config),
 std::sync::Arc::clone(&env.store));
 let error = service
.search(SearchSemanticInput {
 query: "anything".to_string(),
 limit: None,
 })
.await
.unwrap_err();
 assert_eq!(error.code(), ErrorCode::EmbeddingUnavailable);
 let message = error.to_string();
 assert!(message.contains("onnx/model_quantized.onnx"), "{message}");
 assert!(message.contains("afdb6f1a"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_semantic_tool_works_end_to_end_after_the_lazy_backfill() {
 let Some((library, model)) = artifacts() else {
 eprintln!(
 "SKIP search_semantic_tool_works_end_to_end_after_the_lazy_backfill: no artifacts"
);
 return;
 };
 let fixture = start_fixture().await;
 let dir = tempfile::tempdir().unwrap();
 let extra = format!(
 "[embeddings]\nenabled = true\nort_dylib_path = {:?}\nmodel_dir = {:?}\nbackfill_interval_ms = 1000\n",
 library.display().to_string(),
 model.display().to_string()
);
 let config = common::write_binary_config(dir.path(), &extra);
 let data = dir.path().join("data");
 let transport = TokioChildProcess::new(
 tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow")).configure(|command| {
 command
.arg("--config")
.arg(&config)
.arg("--data-dir")
.arg(&data)
.arg("serve");
 }))
.expect("client transport starts");
 let client = ().serve(transport).await.expect("initialize handshake");
 let article = fixture.url("127.0.0.1", "/article");
 let fetched = call_tool(&client, "fetch_page", serde_json::json!({ "url": article })).await;
 assert_eq!(fetched["url"], article);

 // The backfill runs on its own timer; poll the read-only status.
 let mut embedded = 0u64;
 for _ in 0..40 {
 let status = call_tool(&client, "embedding_status", serde_json::json!({})).await;
 assert_eq!(status["enabled"], true);
 assert_eq!(status["available"], true, "{status}");
 embedded = status["embedded_pages"].as_u64().unwrap_or(0);
 if embedded >= 1 {
 break;
 }
 tokio::time::sleep(std::time::Duration::from_millis(500)).await;
 }
 assert!(embedded >= 1, "the lazy backfill embedded the fetched page");

 let query = "seabird tally at a beacon run by a keeper";
 let semantic = call_tool(
 &client,
 "search_semantic",
 serde_json::json!({ "query": query }))
.await;
 assert!(
 semantic["result_count"].as_u64().unwrap_or(0) >= 1,
 "{semantic}"
);
 assert_eq!(semantic["results"][0]["canonical_url"], article);
 let lexical = call_tool(
 &client,
 "search_content",
 serde_json::json!({ "query": query }))
.await;
 assert_eq!(lexical["result_count"], 0, "FTS must still miss: {lexical}");
}

async fn call_tool(
 client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
 name: &str,
 arguments: serde_json::Value) -> serde_json::Value {
 let map = arguments.as_object().cloned().unwrap_or_default();
 let result = client
.peer()
.call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(map))
.await
.unwrap_or_else(|error| panic!("{name} transport failure: {error}"));
 assert_eq!(
 result.is_error,
 Some(false),
 "{name} returned an error: {result:?}"
);
 result.structured_content.unwrap_or(serde_json::Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_status_tool_reports_disabled_without_artifacts() {
 let dir = tempfile::tempdir().unwrap();
 let config = common::write_binary_config(dir.path(), "");
 let data = dir.path().join("data");
 let transport = TokioChildProcess::new(
 tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow")).configure(|command| {
 command
.arg("--config")
.arg(&config)
.arg("--data-dir")
.arg(&data)
.arg("serve");
 }))
.expect("client transport starts");
 let client = ().serve(transport).await.expect("initialize handshake");
 let result = client
.peer()
.call_tool(CallToolRequestParams::new("embedding_status".to_string()))
.await
.expect("embedding_status call");
 assert_eq!(result.is_error, Some(false), "{result:?}");
 let payload = result.structured_content.unwrap_or(serde_json::Value::Null);
 assert_eq!(payload["enabled"], false);
 assert_eq!(payload["available"], false);
 assert_eq!(payload["model"], "minilm-l6-v2-q");
 assert_eq!(payload["dim"], 384);
 assert_eq!(payload["embedded_pages"], 0);
 assert_eq!(payload["pending_pages"], 0);

 // Every other tool still works with the semantic path unavailable.
 let lexical = client
.peer()
.call_tool(
 CallToolRequestParams::new("search_content".to_string()).with_arguments(
 serde_json::json!({ "query": "anything" })
.as_object()
.cloned()
.unwrap_or_default()))
.await
.expect("search_content call");
 assert_eq!(lexical.is_error, Some(false), "{lexical:?}");
 let payload = lexical
.structured_content
.unwrap_or(serde_json::Value::Null);
 assert_eq!(payload["result_count"], 0);
}
