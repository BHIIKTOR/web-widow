//! B06 B6-2 — a real MCP SDK client over stdio: initialize, list every tool
//! and exercise the four capability families against the loopback fixture.
//!
//! This is deliberately a different implementation from the hand-rolled
//! `McpChild` JSON-RPC client used by the earlier batches: it uses rmcp's
//! client role and child-process transport, so the wire shape an MCP SDK
//! enforces is what is tested.

mod common;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("object arguments").clone()
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    value: Value,
) -> Value {
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args(value)))
        .await
        .unwrap_or_else(|error| panic!("{name} transport failure: {error}"));
    assert_eq!(
        result.is_error,
        Some(false),
        "{name} returned an in-band error: {result:?}"
    );
    result.structured_content.unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_sdk_client_uses_every_tool_family_over_stdio() {
    let fixture = common::start_fixture().await;
    fixture.set_page(
        "/monitor/sdk",
        "<html><head><title>SDK</title></head><body><main><article>\
         <h1>SDK monitor</h1><p>stable sdk monitor content</p>\
         </article></main></body></html>",
    );
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
        }),
    )
    .expect("client transport starts");
    let client = ().serve(transport).await.expect("initialize handshake");

    let tools = client.list_all_tools().await.expect("tools/list");
    let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "blob_get",
            "corpus_export",
            "crawl_cancel",
            "crawl_results",
            "crawl_start",
            "crawl_status",
            "embedding_status",
            "events",
            "extract",
            "extract_rows",
            "fetch_feed",
            "fetch_page",
            "interact_act",
            "interact_close",
            "interact_open",
            "interact_state",
            "monitor_changes",
            "monitor_check",
            "monitor_create",
            "monitor_delete",
            "monitor_list",
            "pdf_extract",
            "recipe_apply",
            "recipe_list",
            "recipe_save",
            "robots_cache",
            "screenshot",
            "search",
            "search_content",
            "search_semantic",
            "sitemap",
            "snapshot_compare",
            "webhook_status",
            "webhook_target"
        ],
        "the SDK client sees the complete tool surface (34 tools)"
    );
    // Every advertised output schema names a root `type`: strict clients
    // (pydantic `ListToolsResult` in Hermes) reject a schema that only carries
    // `anyOf`. `events` is an untagged enum, so it needs the explicit root type
    // while keeping its `anyOf` variants.
    for tool in &tools {
        let schema = tool
            .output_schema
            .as_ref()
            .unwrap_or_else(|| panic!("{} advertises no outputSchema", tool.name));
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{} advertises an outputSchema without a root type: {schema:?}",
            tool.name
        );
    }
    let events_schema = tools
        .iter()
        .find(|tool| tool.name == "events")
        .and_then(|tool| tool.output_schema.clone())
        .expect("events advertises an outputSchema");
    assert!(
        events_schema.get("anyOf").is_some(),
        "events keeps its variant union: {events_schema:?}"
    );
    // The two searches must stay distinguishable in their advertised text.
    let description = |name: &str| -> String {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .and_then(|tool| tool.description.clone())
            .unwrap_or_default()
            .to_string()
    };
    let discovery = description("search");
    let local = description("search_content");
    assert!(discovery.contains("DuckDuckGo"), "{discovery}");
    assert!(local.contains("LOCAL store"), "{local}");
    assert!(
        local.contains("`search` is web discovery; `search_content` reads only the local store."),
        "the local tool states the distinction: {local}"
    );
    let interact = description("interact_act");
    assert!(
        interact.contains("NO caller-supplied JavaScript"),
        "{interact}"
    );
    assert!(interact.contains("no text-based targeting"), "{interact}");
    let interact_schema = tools
        .iter()
        .find(|tool| tool.name == "interact_act")
        .map(|tool| serde_json::to_string(&tool.input_schema).unwrap_or_default())
        .unwrap_or_default();
    assert!(!interact_schema.contains("\"script\""), "{interact_schema}");
    assert!(!interact_schema.contains("\"eval\""), "{interact_schema}");

    let read_only = tools
        .iter()
        .find(|tool| tool.name == "search_content")
        .and_then(|tool| tool.annotations.as_ref())
        .and_then(|annotations| annotations.read_only_hint);
    assert_eq!(read_only, Some(true));

    let fetched = call(
        &client,
        "fetch_page",
        json!({ "url": fixture.url("127.0.0.1", "/article") }),
    )
    .await;
    assert_eq!(fetched["status"], json!(200), "{fetched}");
    assert_eq!(fetched["title"], json!("Fixture Article"));
    assert_eq!(fetched["render_mode"], json!("static"));

    // The local FTS tool: the fetch above already indexed this page, so the
    // same session finds it without any network call.
    let local_hits = call(&client, "search_content", json!({ "query": "lighthouse" })).await;
    assert_eq!(local_hits["result_count"], json!(1), "{local_hits}");
    assert_eq!(
        local_hits["results"][0]["url"],
        json!(fixture.url("127.0.0.1", "/article"))
    );
    assert!(
        local_hits["results"][0]["snippet"]
            .as_str()
            .unwrap_or_default()
            .contains("lighthouse"),
        "{local_hits}"
    );
    let invalid = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("search_content".to_string())
                .with_arguments(args(json!({ "query": "   " }))),
        )
        .await
        .expect("in-band error transport");
    assert_eq!(invalid.is_error, Some(true), "{invalid:?}");
    let structured = invalid.structured_content.unwrap_or(Value::Null);
    assert_eq!(
        structured["error"]["code"],
        json!("schema_invalid"),
        "{structured}"
    );
    assert!(
        structured["error"]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("/query"),
        "{structured}"
    );

    let extracted = call(
        &client,
        "extract",
        json!({
            "url": fixture.url("127.0.0.1", "/extract/items/"),
            "schema": { "container": ".item", "fields": { "title": { "selector": "h2" } } }
        }),
    )
    .await;
    assert_eq!(
        extracted["records"].as_array().unwrap().len(),
        2,
        "{extracted}"
    );

    let started = call(
        &client,
        "crawl_start",
        json!({
            "start_urls": [fixture.url("127.0.0.1", "/site/variant")],
            "render_policy": "never",
            "limits": { "max_pages": 1, "per_host_rate": 50.0, "max_duration_s": 30 }
        }),
    )
    .await;
    let job_id = started["job_id"].as_str().expect("job id").to_string();
    let terminal = loop {
        let status = call(&client, "crawl_status", json!({ "job_id": job_id })).await;
        if !matches!(status["state"].as_str(), Some("queued" | "running")) {
            break status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(terminal["state"], json!("done"), "{terminal}");
    assert_eq!(terminal["counters"]["fetched"], json!(1), "{terminal}");

    let created = call(
        &client,
        "monitor_create",
        json!({
            "url": fixture.url("127.0.0.1", "/monitor/sdk"),
            "interval_s": 3600
        }),
    )
    .await;
    let monitor_id = created["monitor_id"].as_i64().expect("monitor id");
    let checked = call(
        &client,
        "monitor_check",
        json!({ "monitor_id": monitor_id, "force": true }),
    )
    .await;
    assert_eq!(checked["checked"], json!(1), "{checked}");
    let history = call(
        &client,
        "monitor_changes",
        json!({ "monitor_id": monitor_id }),
    )
    .await;
    assert_eq!(history["changes"], json!([]), "{history}");

    // B02: a bulk check through the real SDK client is honest about pending.
    let bulk = call(
        &client,
        "monitor_check",
        json!({ "all": true, "force": true }),
    )
    .await;
    assert_eq!(bulk["pending"], json!(true), "{bulk}");
    assert!(bulk["job_id"].is_string(), "{bulk}");
    assert_eq!(bulk["checked"], json!(0), "{bulk}");

    client.cancel().await.expect("client shutdown");
}
