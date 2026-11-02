//! A2 — the real binary, over stdio JSON-RPC, against the fixture server.
//!
//! Calling handlers directly would prove neither the transport nor the schema
//! serialization, so this test spawns `web-widow`, speaks MCP to it and asserts
//! that stdout carries nothing but JSON-RPC frames.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

struct McpProcess {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    frames: usize,
}

impl McpProcess {
    async fn spawn(config: &Path, data_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_web-widow"))
            .arg("--config")
            .arg(config)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("binary should start");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
            frames: 0,
        }
    }

    async fn initialize(&mut self) -> Value {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "web-widow-test", "version": "0.1.0" }
                }),
            )
            .await;
        self.notify("notifications/initialized", json!({})).await;
        response
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_frame(&frame).await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_frame(&frame).await;
        self.read_until_id(id).await
    }

    async fn write_frame(&mut self, frame: &Value) {
        let mut line = serde_json::to_string(frame).unwrap();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(30), self.stdout.read_line(&mut line))
            .await
            .expect("timed out waiting for a JSON-RPC frame")
            .expect("stdout should be readable");
        assert!(read > 0, "server closed stdout unexpectedly");
        self.frames += 1;
        let trimmed = line.trim();
        assert!(!trimmed.is_empty(), "blank line on stdout: {line:?}");
        let value: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|error| panic!("stdout frame is not JSON ({error}): {trimmed:?}"));
        assert_eq!(
            value["jsonrpc"], "2.0",
            "stdout carried a non-protocol line: {trimmed:?}"
        );
        value
    }

    async fn read_until_id(&mut self, id: u64) -> Value {
        loop {
            let frame = self.read_frame().await;
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    fn frames(&self) -> usize {
        self.frames
    }
}

#[tokio::test]
async fn stdio_contract_exposes_all_tools() {
    let fixture = common::start_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(
        dir.path(),
        &format!(
            "[search]\nhtml_url = \"{}\"\nlite_url = \"{}\"\n",
            fixture.url("127.0.0.1", "/serp/html"),
            fixture.url("127.0.0.1", "/serp/lite")
        ),
    );
    let data = dir.path().join("data");
    let mut mcp = McpProcess::spawn(&config, &data).await;

    let init = mcp.initialize().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "web-widow");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap_or("")
            .contains("untrusted"),
        "instructions must warn that fetched content is untrusted"
    );

    let tools = mcp.request("tools/list", json!({})).await;
    let list = tools["result"]["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = list
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
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
        "M07 W1 surface exposes thirty-four tools"
    );
    // B4-16 — advertised schemas never carry schemars' non-standard integer
    // formats, while types and bounds are untouched.
    const NON_STANDARD: &[&str] = &[
        "uint", "uint8", "uint16", "uint32", "uint64", "int8", "int16", "int32", "int64", "double",
    ];
    fn formats(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(format)) = map.get("format") {
                    out.push(format.clone());
                }
                for child in map.values() {
                    formats(child, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    formats(item, out);
                }
            }
            _ => {}
        }
    }
    for tool in list {
        let name = tool["name"].as_str().unwrap_or_default();
        let mut found = Vec::new();
        formats(&tool["inputSchema"], &mut found);
        if let Some(output) = tool.get("outputSchema") {
            formats(output, &mut found);
        }
        for format in &found {
            assert!(
                !NON_STANDARD.contains(&format.as_str()),
                "{name} advertises non-standard format {format}"
            );
        }
    }
    let monitor_list = list
        .iter()
        .find(|tool| tool["name"] == "monitor_list")
        .expect("monitor_list tool");
    let limit = &monitor_list["inputSchema"]["properties"]["limit"];
    let limit_text = limit.to_string();
    assert!(limit_text.contains("integer"), "{limit_text}");
    assert!(limit_text.contains("minimum"), "{limit_text}");

    // Tools that fetch must state the two safety facts; job-state tools need a
    // description but not a copy of the boilerplate.
    for tool in list {
        let name = tool["name"].as_str().unwrap_or_default();
        let description = tool["description"].as_str().unwrap_or("");
        assert!(!description.is_empty(), "{name} has no description");
        if matches!(
            name,
            "fetch_page" | "fetch_feed" | "extract" | "crawl_start" | "monitor_create" | "search"
        ) {
            assert!(description.contains("untrusted"), "{name}: {description}");
            assert!(description.contains("robots"), "{name}: {description}");
        }
    }

    let fetch = list
        .iter()
        .find(|tool| tool["name"] == "fetch_page")
        .expect("fetch_page tool");
    let properties = fetch["inputSchema"]["properties"]
        .as_object()
        .expect("input schema properties");
    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "freshness_s",
            "include_content",
            "include_links",
            "max_chars",
            "mode",
            "render",
            "url"
        ],
        "fetch_page input must match the contract"
    );
    assert!(
        fetch["outputSchema"]["properties"]
            .as_object()
            .expect("output schema")
            .contains_key("markdown_chars")
    );

    let search = list
        .iter()
        .find(|tool| tool["name"] == "search")
        .expect("search tool");
    assert_eq!(
        search["annotations"]["readOnlyHint"],
        json!(true),
        "search is read-only"
    );
    assert_eq!(
        search["annotations"]["openWorldHint"],
        json!(true),
        "search reaches the open web"
    );
    let search_properties = search["inputSchema"]["properties"]
        .as_object()
        .expect("search input schema properties");
    let mut search_keys: Vec<&str> = search_properties.keys().map(String::as_str).collect();
    search_keys.sort();
    assert_eq!(
        search_keys,
        vec!["freshness", "limit", "query", "site"],
        "search input must match the contract"
    );
    assert!(
        search["outputSchema"]["properties"]
            .as_object()
            .expect("search output schema")
            .contains_key("result_count")
    );

    let monitor_list = list
        .iter()
        .find(|tool| tool["name"] == "monitor_list")
        .expect("monitor_list tool");
    assert_eq!(
        monitor_list["annotations"]["readOnlyHint"],
        json!(true),
        "monitor_list is read-only"
    );
    assert_eq!(
        monitor_list["annotations"]["openWorldHint"],
        json!(false),
        "monitor_list does not reach the network"
    );
    let monitor_list_properties = monitor_list["inputSchema"]["properties"]
        .as_object()
        .expect("monitor_list input schema properties");
    let mut monitor_list_keys: Vec<&str> =
        monitor_list_properties.keys().map(String::as_str).collect();
    monitor_list_keys.sort();
    assert_eq!(monitor_list_keys, vec!["include_inactive", "limit"]);

    let searched = mcp
        .call_tool(
            "search",
            json!({ "query": "rust async runtime comparison" }),
        )
        .await;
    assert_eq!(searched["result"]["isError"], json!(false), "{searched}");
    let payload = &searched["result"]["structuredContent"];
    assert_eq!(payload["engine"], json!("duckduckgo_html"));
    assert_eq!(payload["result_count"], json!(10));
    assert_eq!(
        payload["results"][0]["url"],
        json!("https://rustify.rs/articles/rust-async-runtimes-tokio-vs-async-std-2026")
    );
    let text = searched["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.starts_with("1. Rust Async Runtimes:"),
        "search text is a numbered list: {text}"
    );

    let extract = list
        .iter()
        .find(|tool| tool["name"] == "extract")
        .expect("extract tool");
    let extract_properties = extract["inputSchema"]["properties"]
        .as_object()
        .expect("extract input schema properties");
    let mut extract_keys: Vec<&str> = extract_properties.keys().map(String::as_str).collect();
    extract_keys.sort();
    assert_eq!(
        extract_keys,
        vec![
            "max_chars",
            "max_field_chars",
            "render",
            "schema",
            "url",
            "use_cache"
        ]
    );
    let mut required: Vec<&str> = extract["inputSchema"]["required"]
        .as_array()
        .expect("required list")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    required.sort();
    assert_eq!(required, vec!["schema", "url"]);
    let output = extract["outputSchema"]["properties"]
        .as_object()
        .expect("extract output schema");
    for field in [
        "records",
        "record_count",
        "field_errors",
        "truncated",
        "render_mode",
    ] {
        assert!(output.contains_key(field), "missing output field {field}");
    }

    let response = mcp
        .call_tool(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/article") }),
        )
        .await;
    let result = &response["result"];
    assert_eq!(result["isError"], json!(false));
    let structured = &result["structuredContent"];
    assert_eq!(structured["status"], 200);
    assert_eq!(structured["content_type"], "text/html");
    assert_eq!(structured["title"], "Fixture Article");
    assert_eq!(structured["render_mode"], "static");
    assert_eq!(structured["cached"], false);
    assert!(structured["markdown_chars"].as_u64().unwrap() > 50);
    assert_eq!(structured["truncated"], false);
    assert_eq!(structured["limit_hit"], Value::Null);
    assert!(structured["fetched_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(structured["links"], json!([]));
    assert_eq!(structured["links_requested"], false);
    assert!(structured.get("render_quality").is_none());
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("content_status: untrusted-data"));
    assert!(text.contains("lighthouse keeper counted forty-one gulls"));

    // Errors are typed and in-band, not transport failures.
    let blocked = mcp
        .call_tool(
            "fetch_page",
            json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
        )
        .await;
    assert_eq!(blocked["result"]["isError"], json!(true));
    assert_eq!(
        blocked["result"]["structuredContent"]["error"]["code"],
        "blocked_private_address"
    );

    let robots = mcp
        .call_tool(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.5", "/article") }),
        )
        .await;
    assert_eq!(robots["result"]["isError"], json!(true));
    assert_eq!(
        robots["result"]["structuredContent"]["error"]["code"],
        "blocked_by_robots"
    );

    let invalid = mcp
        .call_tool("fetch_page", json!({ "url": "not-a-url" }))
        .await;
    assert_eq!(
        invalid["result"]["structuredContent"]["error"]["code"],
        "invalid_url"
    );

    let render = mcp
        .call_tool(
            "fetch_page",
            json!({ "url": fixture.url("127.0.0.1", "/article"), "render": "always" }),
        )
        .await;
    assert_eq!(
        render["result"]["structuredContent"]["error"]["code"],
        "render_unavailable"
    );

    // The second tool works over the same transport and fixture server.
    let extracted = mcp
        .call_tool(
            "extract",
            json!({
                "url": fixture.url("127.0.0.1", "/extract/items"),
                "schema": {
                    "container": ".item",
                    "fields": {
                        "name": { "selector": "h2" },
                        "href": { "selector": "a", "transform": "absolute_url" }
                    }
                }
            }),
        )
        .await;
    let result = &extracted["result"];
    assert_eq!(result["isError"], json!(false));
    let structured = &result["structuredContent"];
    assert_eq!(structured["record_count"], 2);
    assert_eq!(structured["records"][0]["name"], "First item");
    assert_eq!(structured["truncated"], false);
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(text.contains("records: 2"), "{text}");
    assert!(text.contains("href | name"), "{text}");

    let bad_spec = mcp
        .call_tool(
            "extract",
            json!({
                "url": fixture.url("127.0.0.1", "/extract/items"),
                "schema": { "fields": { "x": { "selector": "p", "transform": "regex" } } }
            }),
        )
        .await;
    assert_eq!(bad_spec["result"]["isError"], json!(true));
    assert_eq!(
        bad_spec["result"]["structuredContent"]["error"]["code"],
        "schema_invalid"
    );

    // The crawl tools share the same transport; one page is enough to prove the
    // wiring, the persisted frontier and the results join.
    let crawl = mcp
        .call_tool(
            "crawl_start",
            json!({
                "start_urls": [fixture.url("127.0.0.1", "/site/variant")],
                "render_policy": "never",
                "limits": { "max_pages": 1, "per_host_rate": 50.0, "max_duration_s": 30 }
            }),
        )
        .await;
    assert_eq!(crawl["result"]["isError"], json!(false), "{crawl}");
    let job_id = crawl["result"]["structuredContent"]["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(crawl["result"]["structuredContent"]["state"], "queued");
    let limits = &crawl["result"]["structuredContent"]["limits"];
    assert_eq!(limits["max_pages"], 1);
    assert_eq!(limits["per_host_rate"], 50.0);
    assert_eq!(limits["max_depth"], 5);

    let mut terminal = None;
    for _ in 0..200 {
        let status = mcp
            .call_tool("crawl_status", json!({ "job_id": job_id }))
            .await;
        let state = status["result"]["structuredContent"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !matches!(state.as_str(), "queued" | "running") {
            terminal = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let terminal = terminal.expect("crawl finished");
    assert_eq!(terminal["result"]["structuredContent"]["state"], "done");
    assert_eq!(
        terminal["result"]["structuredContent"]["counters"]["fetched"],
        1
    );
    assert_eq!(
        terminal["result"]["structuredContent"]["limits_hit"],
        json!(["max_pages"])
    );

    let results = mcp
        .call_tool("crawl_results", json!({ "job_id": job_id, "limit": 10 }))
        .await;
    assert_eq!(results["result"]["isError"], json!(false));
    let rows = results["result"]["structuredContent"]["results"]
        .as_array()
        .expect("results array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], json!(200));
    assert!(rows[0]["raw_hash"].as_str().is_some());
    assert_eq!(rows[0]["links_count"], 0);

    let cancel = mcp
        .call_tool("crawl_cancel", json!({ "job_id": job_id }))
        .await;
    assert_eq!(cancel["result"]["isError"], json!(false));
    assert_eq!(cancel["result"]["structuredContent"]["state"], "done");

    let missing = mcp
        .call_tool(
            "crawl_status",
            json!({ "job_id": "0192f3aa-0000-7000-8000-0000000000ff" }),
        )
        .await;
    assert_eq!(missing["result"]["isError"], json!(true));
    assert_eq!(
        missing["result"]["structuredContent"]["error"]["code"],
        "not_found"
    );

    // Monitoring over the same transport: create, check, read, delete.
    fixture.set_page(
        "/monitor/wire",
        "<html><head><title>Wire</title></head><body><main><article>\
         <h1>Wire monitor</h1><p>stable wire content</p></article></main></body></html>",
    );
    let created = mcp
        .call_tool(
            "monitor_create",
            json!({
                "url": fixture.url("127.0.0.1", "/monitor/wire"),
                "interval_s": 3600,
                "ignore_selectors": []
            }),
        )
        .await;
    assert_eq!(created["result"]["isError"], json!(false), "{created}");
    let monitor_id = created["result"]["structuredContent"]["monitor_id"]
        .as_i64()
        .expect("monitor id");
    assert!(created["result"]["structuredContent"]["next_due_at"].is_string());

    let checked = mcp
        .call_tool(
            "monitor_check",
            json!({ "monitor_id": monitor_id, "force": true }),
        )
        .await;
    assert_eq!(checked["result"]["isError"], json!(false), "{checked}");
    assert_eq!(checked["result"]["structuredContent"]["checked"], json!(1));

    let history = mcp
        .call_tool(
            "monitor_changes",
            json!({ "monitor_id": monitor_id, "limit": 10 }),
        )
        .await;
    assert_eq!(history["result"]["isError"], json!(false));
    assert_eq!(history["result"]["structuredContent"]["changes"], json!([]));

    let deleted = mcp
        .call_tool("monitor_delete", json!({ "monitor_id": monitor_id }))
        .await;
    assert_eq!(deleted["result"]["isError"], json!(false));
    assert_eq!(
        deleted["result"]["structuredContent"]["active"],
        json!(false)
    );

    assert!(mcp.frames() >= 19, "expected several protocol frames");
    // Closing stdin ends the session cleanly (protocol/EOF shutdown).
    drop(mcp.stdin.take());
}

#[cfg(feature = "render")]
#[tokio::test]
async fn render_modes_over_the_mcp_wire() {
    let Some(chrome) = common::skip_without_chrome("render_modes_over_the_mcp_wire") else {
        return;
    };
    let fixture = common::start_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_rendering_binary_config(dir.path(), &chrome);
    let data = dir.path().join("data");
    let mut mcp = McpProcess::spawn(&config, &data).await;
    mcp.initialize().await;
    let url = fixture.url("127.0.0.1", "/js/noscript");

    let never = mcp
        .call_tool("fetch_page", json!({ "url": url, "render": "never" }))
        .await;
    assert_eq!(never["result"]["isError"], json!(false));
    assert_eq!(
        never["result"]["structuredContent"]["render_mode"],
        "static"
    );

    let always = mcp
        .call_tool("fetch_page", json!({ "url": url, "render": "always" }))
        .await;
    assert_eq!(always["result"]["isError"], json!(false));
    let structured = &always["result"]["structuredContent"];
    assert_eq!(structured["render_mode"], "rendered");
    assert!(structured["word_count"].as_u64().unwrap_or(0) >= 55);
    let text = always["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("render_mode: rendered"), "{text}");
    assert!(text.contains("injectedword"), "{text}");

    let auto = mcp
        .call_tool("fetch_page", json!({ "url": url, "render": "auto" }))
        .await;
    assert_eq!(auto["result"]["isError"], json!(false));
    assert_eq!(
        auto["result"]["structuredContent"]["render_mode"],
        "rendered"
    );

    let extracted = mcp
        .call_tool(
            "extract",
            json!({
                "url": url,
                "render": "always",
                "schema": {
                    "container": "article",
                    "fields": { "title": { "selector": "h1" } }
                }
            }),
        )
        .await;
    assert_eq!(extracted["result"]["isError"], json!(false));
    assert_eq!(
        extracted["result"]["structuredContent"]["render_mode"],
        "rendered"
    );
    assert_eq!(
        extracted["result"]["structuredContent"]["records"][0]["title"],
        "Rendered Article"
    );

    // Let the binary run its renderer shutdown before the test process ends.
    drop(mcp.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(15), mcp.child.wait())
        .await
        .expect("server should exit after EOF")
        .unwrap();
    assert!(status.success(), "exit status: {status:?}");
}

#[cfg(feature = "render")]
#[tokio::test]
async fn render_always_with_missing_chrome_keeps_the_server_up() {
    let fixture = common::start_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("definitely-not-chrome");
    let config = common::write_rendering_binary_config(dir.path(), &missing);
    let data = dir.path().join("data");
    let mut mcp = McpProcess::spawn(&config, &data).await;
    mcp.initialize().await;

    let render = mcp
        .call_tool(
            "fetch_page",
            json!({
                "url": fixture.url("127.0.0.1", "/js/noscript"),
                "render": "always"
            }),
        )
        .await;
    assert_eq!(render["result"]["isError"], json!(true));
    let error = &render["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], "render_unavailable");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains("render.chrome_path"), "{message}");
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );

    // The same process keeps serving: a static call and the second tool both work.
    let static_call = mcp
        .call_tool(
            "fetch_page",
            json!({
                "url": fixture.url("127.0.0.1", "/article"),
                "render": "never"
            }),
        )
        .await;
    assert_eq!(static_call["result"]["isError"], json!(false));
    assert_eq!(
        static_call["result"]["structuredContent"]["render_mode"],
        "static"
    );

    let extracted = mcp
        .call_tool(
            "extract",
            json!({
                "url": fixture.url("127.0.0.1", "/extract/items"),
                "render": "never",
                "schema": {
                    "container": ".item",
                    "fields": { "name": { "selector": "h2" } }
                }
            }),
        )
        .await;
    assert_eq!(extracted["result"]["isError"], json!(false));
    assert_eq!(extracted["result"]["structuredContent"]["record_count"], 2);

    drop(mcp.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait())
        .await
        .expect("server should exit after EOF")
        .unwrap();
    assert!(status.success(), "exit status: {status:?}");
}

#[tokio::test]
async fn extract_top_level_spec_error_names_the_schema_envelope() {
    let fixture = common::start_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");
    let mut mcp = McpProcess::spawn(&config, &data).await;
    mcp.initialize().await;

    // The wrong envelope: the spec one level too high. The real MCP path (not a
    // hand-built error string) must name the documented envelope.
    let rejected = mcp
        .call_tool(
            "extract",
            json!({
                "url": fixture.url("127.0.0.1", "/extract/items"),
                "container": ".item",
                "fields": { "name": { "selector": "h2" } }
            }),
        )
        .await;
    let serialized = serde_json::to_string(&rejected).unwrap();
    assert!(
        serialized.contains("extract expects the extraction spec under `schema`"),
        "{serialized}"
    );
    assert!(
        serialized.contains("{url, schema: {container?, fields}}"),
        "{serialized}"
    );

    // The documented envelope still works in the same process.
    let accepted = mcp
        .call_tool(
            "extract",
            json!({
                "url": fixture.url("127.0.0.1", "/extract/items"),
                "render": "never",
                "schema": {
                    "container": ".item",
                    "fields": { "name": { "selector": "h2" } }
                }
            }),
        )
        .await;
    assert_eq!(accepted["result"]["isError"], json!(false), "{accepted}");
    assert_eq!(accepted["result"]["structuredContent"]["record_count"], 2);

    drop(mcp.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait())
        .await
        .expect("server should exit after EOF")
        .unwrap();
    assert!(status.success(), "exit status: {status:?}");
}

#[tokio::test]
async fn stdio_stdout_stays_clean_and_exit_is_zero_on_eof() {
    let fixture = common::start_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");
    let mut mcp = McpProcess::spawn(&config, &data).await;
    mcp.initialize().await;
    mcp.call_tool(
        "fetch_page",
        json!({ "url": fixture.url("127.0.0.1", "/plain") }),
    )
    .await;

    // Close stdin: the server must exit cleanly and stdout must have contained
    // only the frames we already validated.
    drop(mcp.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait())
        .await
        .expect("server should exit after EOF")
        .unwrap();
    assert!(status.success(), "exit status: {status:?}");
}
