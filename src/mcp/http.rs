//! Loopback streamable HTTP transport (IMP-DEC-003 / ITEM-007).
//!
//! Same handler as stdio, no extra capability. Bound to a caller-supplied
//! address; a non-loopback bind requires the explicit operator flag in `main`.
//! Every request must carry `Authorization: Bearer <token>`; the token comes
//! from the environment or a `0600` file and is never logged.
//!
//! Shutdown is cooperative: the caller cancels the token, in-flight requests
//! get a bounded grace period, then the process cancels jobs and closes the
//! renderer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use super::WebWidowServer;
use crate::core::token::constant_time_eq;
use crate::events::EventsService;

/// The feed route inputs: its own capability token, never the MCP bearer.
pub struct FeedRoute {
    pub enabled: bool,
    pub token: Option<String>,
    pub events: Arc<EventsService>,
}

/// The feed is loopback-only: it may be enabled only on a loopback bind.
/// `main` refuses startup and [`serve`] re-checks, so a library caller cannot
/// bypass the posture.
pub fn feed_bind_allowed(addr: &SocketAddr, feed_enabled: bool) -> bool {
    !feed_enabled || addr.ip().is_loopback()
}

#[derive(Clone)]
struct FeedState {
    enabled: bool,
    token: Option<Arc<str>>,
    events: Arc<EventsService>,
}

async fn require_bearer(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()));
    if authorized {
        return next.run(request).await;
    }
    let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// Request-level transport diagnostics: method, path, status, the first eight
/// characters of `Mcp-Session-Id` (or `none`) and duration.
///
/// Deliberately never reads the Authorization header, the request body or the
/// query string. INFO for status >= 400 (a field failure must be visible at the
/// default level); DEBUG otherwise. This middleware is layered **outside** the
/// bearer check so it also sees `401`s.
async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let session = request
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "none".to_string());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if status >= 400 {
        tracing::info!(
            method = %method,
            path = %path,
            status,
            session = %session,
            duration_ms,
            "http request"
        );
    } else {
        tracing::debug!(
            method = %method,
            path = %path,
            status,
            session = %session,
            duration_ms,
            "http request"
        );
    }
    response
}

/// Serve `server` over streamable HTTP at `/mcp` until `shutdown` is cancelled.
///
/// The caller has already validated the bind address, the token and the
/// loopback policy; this function only enforces the token on the wire.
pub async fn serve(
    server: WebWidowServer,
    addr: SocketAddr,
    token: String,
    feed: FeedRoute,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if !feed_bind_allowed(&local, feed.enabled) {
        // Drop the listener without serving: a non-loopback bind must never
        // expose the feed (or its token check) on another interface.
        drop(listener);
        return Err(format!(
            "refusing to serve the change feed on a non-loopback address ({local}); \
             set `[events].feed_enabled = false` for an MCP-only network deployment"
        )
        .into());
    }
    let token: Arc<str> = Arc::from(token);

    // Defaults keep Host validation to loopback. A deliberate non-loopback bind
    // still only accepts the bound IP (remote exposure is out of M01 scope).
    let allowed_hosts = vec![
        local.ip().to_string(),
        local.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown.child_token())
        // JSON for simple request/response tools; the service falls back to SSE
        // when a handler emits a notification first.
        .with_json_response(true)
        // Stateless streamable HTTP (IMP-DEC-013): every POST is served directly;
        // a supplied session id is not consulted, so stale/unknown/absent ids
        // cannot 404. `initialize` still completes the handshake.
        .with_legacy_session_mode(false)
        .with_allowed_hosts(allowed_hosts);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    // The bearer is scoped to the MCP nest only. The feed route lives outside it
    // and carries its own capability token.
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(token, require_bearer));
    let feed_state = FeedState {
        enabled: feed.enabled,
        token: feed.token.map(Arc::from),
        events: Arc::clone(&feed.events),
    };
    let feed_router = Router::new()
        .route("/feeds/changes.xml", get(feed_changes))
        .with_state(feed_state);
    let router = Router::new()
        .merge(mcp)
        .merge(feed_router)
        // Outer: request diagnostics for both routes, so 401/404s are visible.
        .layer(middleware::from_fn(access_log));

    if feed.enabled {
        feed.events
            .runtime()
            .set_url(Some(format!("http://{local}/feeds/changes.xml")));
    }
    tracing::info!(
        %local,
        auth = "bearer",
        stateless = true,
        feed = if feed.enabled { "enabled" } else { "disabled" },
        "serving MCP over streamable HTTP at /mcp"
    );
    let outcome = axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await;
    feed.events.runtime().set_url(None);
    outcome?;
    Ok(())
}

/// `GET /feeds/changes.xml?token=<capability token>`: the RSS document from the
/// local store. Read-only, no outbound fetch, loopback-only by the bind policy.
/// Missing/wrong token → 401; disabled → 404; `If-None-Match` matching the body
/// hash → 304. The access log records the path only (never the query).
async fn feed_changes(State(state): State<FeedState>, request: Request) -> Response {
    if !state.enabled {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(expected) = state.token.as_deref() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let provided = request
        .uri()
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "token")
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_default();
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let xml = match state.events.feed_xml().await {
        Ok(xml) => xml,
        Err(error) => {
            tracing::warn!(code = error.code().as_str(), "feed document failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "feed unavailable").into_response();
        }
    };
    let etag = format!("\"{}\"", blake3::hash(xml.as_bytes()).to_hex());
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"\"")),
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=60"),
        );
        return response;
    }
    let mut response = (StatusCode::OK, xml).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/rss+xml; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"\"")),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=60"),
    );
    response
}

#[cfg(test)]
mod tests {
    use crate::core::token::constant_time_eq;

    #[test]
    fn the_feed_bind_predicate_is_loopback_only() {
        use std::net::SocketAddr;
        let loopback: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:8765".parse().unwrap();
        let lan: SocketAddr = "192.168.1.10:8765".parse().unwrap();
        assert!(super::feed_bind_allowed(&loopback, true));
        assert!(!super::feed_bind_allowed(&any, true));
        assert!(!super::feed_bind_allowed(&lan, true));
        assert!(super::feed_bind_allowed(&any, false));
        assert!(super::feed_bind_allowed(&lan, false));
    }

    #[test]
    fn bearer_comparison_is_exact() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-toke", b"secret-token"));
        assert!(!constant_time_eq(b"secret-tokeN", b"secret-token"));
        assert!(!constant_time_eq(b"", b"secret-token"));
    }
}
