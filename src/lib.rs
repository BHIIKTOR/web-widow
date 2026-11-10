//! web-widow: an agent-native web crawler exposed as an MCP server.
//!
//! Dependency direction is strictly downward:
//! `mcp -> pipeline -> (fetch | extract | store) -> core`.
//! Only `core` is pure; only `fetch` touches the network; only `store` touches
//! the data directory.

pub mod auth;
pub mod cli_http;
pub mod compare;
pub mod config;
pub mod core;
pub mod crawl;
pub mod embeddings;
pub mod events;
pub mod export;
pub mod extract;
pub mod feed;
pub mod fetch;
pub mod fts;
pub mod interaction;
pub mod mcp;
pub mod monitor;
pub mod pdf;
pub mod pipeline;
pub mod recipes;
pub mod render;
pub mod robots_cache;
pub mod rows;
pub mod search;
pub mod sitemap;
pub mod store;
#[cfg(feature = "http")]
pub mod tls;
pub mod webhooks;

pub use crate::core::error::{ErrorCode, ToolError, WidowError};
pub use crate::pipeline::{FetchPageInput, FetchPagePayload, FetchPageResult, Pipeline};

/// Crate version, advertised to MCP clients.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod send_sync_tests {
 /// The MCP server is shared across runtime threads and the fetch path runs
 /// concurrently, so these bounds are part of the design, not an accident.
 #[test]
 fn shared_components_are_send_and_sync() {
 fn assert_send_sync<T: Send + Sync>() {}
 assert_send_sync::<crate::pipeline::Pipeline>();
 assert_send_sync::<crate::store::Store>();
 assert_send_sync::<crate::fetch::http::HttpClient>();
 assert_send_sync::<crate::fetch::gate::HostGate>();
 assert_send_sync::<crate::fetch::ssrf::SsrfGate>();
 assert_send_sync::<crate::mcp::WebWidowServer>();
 #[cfg(feature = "render")]
 assert_send_sync::<crate::render::BrowserRenderer>();
 }
}
