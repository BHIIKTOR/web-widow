//! `run-due --http`: a cron client that checks due monitors through a running
//! server and never opens the data directory (IMP-DEC-023).
//!
//! The client carries `Mcp-Session-Id` when the server returns one (legacy
//! servers require it, stateless servers ignore it), disables redirects so the
//! bearer cannot leave the host, and prints exactly one JSON object.

use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

/// Everything that can go wrong before a `monitor_check` payload exists.
#[derive(Debug)]
pub struct CliHttpError {
    message: String,
}

impl CliHttpError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliHttpError {}

/// Check every due monitor through `url` and return the one-line payload. The
/// caller resolves the bearer token; it is never placed in an error message.
pub async fn run_due(url: &str, token: &str, wait_ms: u64) -> Result<Value, CliHttpError> {
    let endpoint = Url::parse(url)
        .map_err(|error| CliHttpError::new(format!("invalid --http URL: {error}")))?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(CliHttpError::new("--http URL must be http or https"));
    }
    let client = reqwest::Client::builder()
        // The bearer must never follow a Location to another host.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(wait_ms.saturating_add(120_000)))
        .build()
        .map_err(|error| CliHttpError::new(format!("cannot build the HTTP client: {error}")))?;

    let initialize = post(
        &client,
        &endpoint,
        token,
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "web-widow-run-due", "version": crate::VERSION}
            }
        }),
    )
    .await?;
    let session = initialize.session;
    if initialize.payload.get("error").is_some() {
        return Err(CliHttpError::new(format!(
            "initialize failed: {}",
            initialize.payload["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
        )));
    }

    // The initialized notification is required by the protocol; stateless
    // servers accept it without a body.
    let _ = post(
        &client,
        &endpoint,
        token,
        session.as_deref(),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    )
    .await?;

    let call = post(
        &client,
        &endpoint,
        token,
        session.as_deref(),
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "monitor_check",
                "arguments": {"all": true, "wait_ms": wait_ms}
            }
        }),
    )
    .await?;
    if let Some(error) = call.payload.get("error") {
        return Err(CliHttpError::new(format!(
            "monitor_check failed: {}",
            error["message"].as_str().unwrap_or("JSON-RPC error")
        )));
    }
    let result = call
        .payload
        .get("result")
        .ok_or_else(|| CliHttpError::new("monitor_check returned no result"))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let error = result
            .get("structuredContent")
            .and_then(|structured| structured.get("error"));
        let code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("tool_error");
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("tool error");
        return Err(CliHttpError::new(format!(
            "monitor_check failed: {code}: {message}"
        )));
    }
    let payload = result
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| CliHttpError::new("monitor_check returned no structuredContent"))?;
    let Value::Object(mut fields) = payload else {
        return Err(CliHttpError::new("monitor_check payload is not an object"));
    };
    // `via`/`url` first; the payload's own keys follow.
    let mut out = serde_json::Map::new();
    out.insert("via".to_string(), Value::String("http".to_string()));
    out.insert("url".to_string(), Value::String(endpoint.to_string()));
    for (key, value) in std::mem::take(&mut fields) {
        out.insert(key, value);
    }
    Ok(Value::Object(out))
}

struct PostResult {
    payload: Value,
    session: Option<String>,
}

async fn post(
    client: &reqwest::Client,
    endpoint: &Url,
    token: &str,
    session: Option<&str>,
    body: &Value,
) -> Result<PostResult, CliHttpError> {
    let mut request =
        client
            .post(endpoint.clone())
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .body(serde_json::to_vec(body).map_err(|error| {
                CliHttpError::new(format!("cannot encode the request: {error}"))
            })?);
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    let response = request
        .send()
        .await
        .map_err(|error| CliHttpError::new(format!("transport error: {error}")))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(CliHttpError::new(format!(
            "server answered {status} to a redirect-disabled client"
        )));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(CliHttpError::new("server rejected the bearer token (401)"));
    }
    if !status.is_success() {
        return Err(CliHttpError::new(format!("server answered {status}")));
    }
    let session = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response
        .text()
        .await
        .map_err(|error| CliHttpError::new(format!("cannot read the response: {error}")))?;
    let payload = parse_body(&body, &content_type)?;
    Ok(PostResult { payload, session })
}

/// A JSON response or a streamable-HTTP SSE body whose last `data:` line is the
/// JSON-RPC message.
fn parse_body(body: &str, content_type: &str) -> Result<Value, CliHttpError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    if content_type.contains("text/event-stream") || !trimmed.starts_with('{') {
        for line in trimmed.lines().rev() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                return serde_json::from_str(data)
                    .map_err(|error| CliHttpError::new(format!("bad SSE payload: {error}")));
            }
        }
        return Err(CliHttpError::new("no JSON-RPC payload in the SSE response"));
    }
    serde_json::from_str(trimmed)
        .map_err(|error| CliHttpError::new(format!("bad JSON response: {error}")))
}
