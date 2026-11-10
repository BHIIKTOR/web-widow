//! Rendering port.
//!
//! The pipeline depends on [`Renderer`], never on `chromiumoxide`. The CDP
//! adapter returns HTML plus attribution; extraction stays single-path. With the
//! `render` feature off the crate still compiles and serves static pages, and an
//! `always` request gets the typed `render_unavailable` error.

use std::fmt::Debug;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::config::Config;
use crate::core::error::{Result, WidowError};

pub mod auto;
#[cfg(feature = "render")]
mod browser;
pub mod budget;
mod discovery;
pub mod session;

#[cfg(feature = "render")]
pub use browser::BrowserRenderer;
pub use discovery::{ChromeResolver, SystemChromeResolver};
pub use session::{
 CapturedShot, ConsoleEntry, HeldSession, OpenSessionFuture, ScrollTarget, SessionBlocked,
 SessionCounters, SessionGate, SessionOptions,
};

pub type RenderFuture<'a> = Pin<Box<dyn Future<Output = Result<RenderedPage>> + Send + 'a>>;
pub type CaptureFuture<'a> = Pin<Box<dyn Future<Output = Result<CapturedPage>> + Send + 'a>>;
pub type ShutdownFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Per-page resource counts that were blocked at the CDP layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockedCounts {
 pub image: usize,
 pub media: usize,
 pub font: usize,
 pub stylesheet: usize,
}

/// What the renderer returns. HTML only: no extraction policy lives here.
#[derive(Debug, Clone)]
pub struct RenderedPage {
 pub html: String,
 pub final_url: Url,
 pub status: u16,
 pub content_type: Option<String>,
 /// True when a cap or the hard ceiling stopped the page before it settled.
 pub partial: bool,
 /// The cap/ceiling that stopped the page, for logs and evidence.
 pub stop_reason: Option<String>,
 pub blocked: BlockedCounts,
 pub requests: u32,
 pub bytes: u64,
 pub duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
 Static,
 Rendered,
 RenderedPartial,
 StaticFallback,
}

impl RenderMode {
 pub fn as_str(self) -> &'static str {
 match self {
 Self::Static => "static",
 Self::Rendered => "rendered",
 Self::RenderedPartial => "rendered_partial",
 Self::StaticFallback => "static_fallback",
 }
 }
}

/// One screenshot request: a post-settle wait, never a navigation
/// (). The visual capture profile is server-fixed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
 /// Capture the scrollable content size instead of the visual viewport.
 pub full_page: bool,
 /// Extra quiet time after the normal settle, in milliseconds.
 pub wait_ms: u64,
}

/// Where the reported `width`/`height` came from. `Unknown` means
/// `Page.getLayoutMetrics` failed and nothing was invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimensionsSource {
 Content,
 Viewport,
 Unknown,
}

impl DimensionsSource {
 pub fn as_str(self) -> &'static str {
 match self {
 Self::Content => "content",
 Self::Viewport => "viewport",
 Self::Unknown => "unknown",
 }
 }
}

/// The settled page plus its PNG capture (`screenshot`). No
/// caller-supplied path or bytes ever reach the renderer.
#[derive(Debug, Clone)]
pub struct CapturedPage {
 pub page: RenderedPage,
 pub png: Vec<u8>,
 /// CSS pixels for the content size (`full_page`) or the viewport.
 pub width: Option<u32>,
 pub height: Option<u32>,
 pub dimensions_source: DimensionsSource,
 /// Requests the visual profile refused (media, `file:`); informational.
 pub failed_resources: u32,
}

/// The pipeline's port. `shutdown` and the diagnostics are default no-ops so a
/// static-only implementation stays tiny.
pub trait Renderer: Send + Sync + Debug {
 fn render<'a>(&'a self, url: &'a Url) -> RenderFuture<'a>;

 /// Capture the settled page as a PNG under the visual profile
 /// (stylesheets/fonts/images allowed, media blocked). The default is a
 /// typed `render_unavailable`: a renderer that cannot capture says so
 /// instead of returning a fake image.
 fn capture<'a>(&'a self, _url: &'a Url, _options: CaptureOptions) -> CaptureFuture<'a> {
 Box::pin(async {
 Err(WidowError::RenderUnavailable(
 "this build's renderer cannot capture screenshots".to_string()))
 })
 }

 /// Open one held page for browser interaction. The default is a typed
 /// `render_unavailable`: a renderer that cannot hold a session says so.
 fn open_session<'a>(
 &'a self,
 _url: &'a Url,
 _options: SessionOptions) -> OpenSessionFuture<'a> {
 Box::pin(async {
 Err(WidowError::RenderUnavailable(
 "this build's renderer cannot hold a browser interaction session".to_string()))
 })
 }

 /// Resolve the browser without launching it. The pipeline calls this after
 /// the SSRF check and before robots/Gate work, so a missing browser fails
 /// fast without an unrelated network request.
 fn preflight(&self) -> Result<()> {
 Ok(())
 }

 fn shutdown<'a>(&'a self) -> ShutdownFuture<'a> {
 Box::pin(async {})
 }

 fn browser_pid(&self) -> Option<u32> {
 None
 }

 fn restart_count(&self) -> usize {
 0
 }
}

/// Static-only implementation: any render request is a typed, actionable error.
#[derive(Debug, Clone)]
pub struct UnavailableRenderer {
 message: String,
}

impl UnavailableRenderer {
 pub fn new(message: impl Into<String>) -> Self {
 Self {
 message: message.into(),
 }
 }
}

impl Default for UnavailableRenderer {
 fn default() -> Self {
 Self::new(
 "this build has no `render` feature; rebuild with --features render or use render=never")
 }
}

impl Renderer for UnavailableRenderer {
 fn render<'a>(&'a self, _url: &'a Url) -> RenderFuture<'a> {
 let message = self.message.clone();
 Box::pin(async move { Err(WidowError::RenderUnavailable(message)) })
 }

 fn capture<'a>(&'a self, _url: &'a Url, _options: CaptureOptions) -> CaptureFuture<'a> {
 let message = self.message.clone();
 Box::pin(async move { Err(WidowError::RenderUnavailable(message)) })
 }

 fn open_session<'a>(
 &'a self,
 _url: &'a Url,
 _options: SessionOptions) -> OpenSessionFuture<'a> {
 let message = self.message.clone();
 Box::pin(async move { Err(WidowError::RenderUnavailable(message)) })
 }

 fn preflight(&self) -> Result<()> {
 Err(WidowError::RenderUnavailable(self.message.clone()))
 }
}

/// Construct the process-wide renderer. No browser is launched here; the first
/// render call resolves Chrome and starts it lazily.
pub fn build_renderer(config: &Config, data_dir: &Path) -> Result<Arc<dyn Renderer>> {
 #[cfg(feature = "render")]
 {
 let resolver = Arc::new(SystemChromeResolver::new(
 config.render.chrome_path.clone(),
 config.render.discovery));
 Ok(Arc::new(BrowserRenderer::new(
 config.render.clone(),
 data_dir.join("render"),
 resolver,
 config.logging.redact_query)))
 }
 #[cfg(not(feature = "render"))]
 {
 let _ = (config, data_dir);
 Ok(Arc::new(UnavailableRenderer::default()))
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn render_mode_strings_match_the_contract() {
 assert_eq!(RenderMode::Static.as_str(), "static");
 assert_eq!(RenderMode::Rendered.as_str(), "rendered");
 assert_eq!(RenderMode::RenderedPartial.as_str(), "rendered_partial");
 assert_eq!(RenderMode::StaticFallback.as_str(), "static_fallback");
 }

 #[tokio::test]
 async fn unavailable_renderer_is_typed_and_actionable() {
 let renderer = UnavailableRenderer::default();
 let url = Url::parse("https://example.com/").unwrap();
 let error = renderer.render(&url).await.unwrap_err();
 assert!(matches!(error, WidowError::RenderUnavailable(_)));
 assert!(error.to_string().contains("render"));
 }

 #[tokio::test]
 async fn unavailable_renderer_refuses_capture_without_faking_bytes() {
 let renderer = UnavailableRenderer::default();
 let url = Url::parse("https://example.com/").unwrap();
 let error = renderer
.capture(&url, CaptureOptions::default())
.await
.unwrap_err();
 assert!(matches!(error, WidowError::RenderUnavailable(_)));
 }

 #[test]
 fn dimensions_source_strings_match_the_payload_contract() {
 assert_eq!(DimensionsSource::Content.as_str(), "content");
 assert_eq!(DimensionsSource::Viewport.as_str(), "viewport");
 assert_eq!(DimensionsSource::Unknown.as_str(), "unknown");
 }
}
