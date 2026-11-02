//! Held-page session port (IMP-DEC-039/040).
//!
//! The interaction layer drives a held page only through [`HeldSession`], so it
//! never names `chromiumoxide` and the static-only build still compiles. The
//! browser adapter returns a session after the same gated open as a render; the
//! session installs its own CDP Fetch handler (SSRF for every request, robots +
//! host lease for documents, `file:`/`ws:`/`wss:` refused).

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::DimensionsSource;
use crate::core::error::Result;
use crate::fetch::gate::HostGate;
use crate::fetch::http::HopGuard;
use crate::fetch::ssrf::SsrfGate;

pub type HeldFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub type OpenSessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn HeldSession>>> + Send + 'a>>;

/// The gates every session request is checked against (IMP-DEC-040).
#[derive(Debug, Clone)]
pub struct SessionGate {
    pub ssrf: Arc<SsrfGate>,
    pub hop_guard: Arc<dyn HopGuard>,
    pub host_gate: Arc<HostGate>,
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub gate: SessionGate,
    /// How long the page may stay network-quiet after a navigation.
    pub settle_ms: u64,
}

/// One console/log line. Page text is untrusted data and is never executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConsoleEntry {
    /// `log | info | warn | error | debug`.
    pub level: String,
    /// Plain text, at most 500 characters.
    pub text: String,
    /// RFC3339 UTC.
    pub at: String,
    /// `console` (`Runtime.consoleAPICalled`) or `log` (`Log.entryAdded`).
    pub source: String,
}

/// Why a request was refused inside the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBlocked {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollTarget<'a> {
    Element(&'a str),
    Top,
    Bottom,
}

/// Live per-session request counters. `vet_observed` proves the SSRF gate ran;
/// `paused` counts every pause the handler saw (including refusals), and
/// `child_targets` counts attached worker/iframe targets. Exposed permanently
/// so a test can never confuse "0 hits" with "never paused".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionCounters {
    pub vet_observed: u64,
    pub paused: u64,
    pub child_targets: u64,
}

/// A PNG capture of the held page.
#[derive(Debug, Clone)]
pub struct CapturedShot {
    pub png: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub dimensions_source: DimensionsSource,
}

/// The operations a held page exposes. All page mutation happens here; the
/// interaction layer owns validation, timing and persistence.
pub trait HeldSession: Send + Sync + Debug {
    fn url<'a>(&'a self) -> HeldFuture<'a, Option<String>>;
    fn title<'a>(&'a self) -> HeldFuture<'a, Option<String>>;
    /// Last document status observed (200 when none was seen).
    fn status(&self) -> u16;
    /// Click the `index`-th match of `selector` at its clickable point, then
    /// let the page settle (or a navigation finish).
    fn click<'a>(&'a self, selector: &'a str, index: usize) -> HeldFuture<'a, ()>;
    /// Focus, optionally clear (select-all + Backspace), then type. Returns the
    /// number of characters typed; the text itself never leaves this method.
    fn type_text<'a>(
        &'a self,
        selector: &'a str,
        index: usize,
        text: &'a str,
        clear: bool,
    ) -> HeldFuture<'a, usize>;
    /// Presence (not visibility) of at least one match.
    fn has_selector<'a>(&'a self, selector: &'a str) -> HeldFuture<'a, bool>;
    fn scroll<'a>(&'a self, target: ScrollTarget<'a>) -> HeldFuture<'a, ()>;
    fn screenshot<'a>(&'a self, full_page: bool) -> HeldFuture<'a, CapturedShot>;
    fn console<'a>(&'a self, limit: usize) -> HeldFuture<'a, Vec<ConsoleEntry>>;
    /// Take (and clear) the first refusal recorded since the last call.
    fn take_blocked<'a>(&'a self) -> HeldFuture<'a, Option<SessionBlocked>>;
    /// Take the first refusal as the original typed error (open fidelity).
    fn take_blocked_error<'a>(&'a self) -> HeldFuture<'a, Option<crate::core::error::WidowError>>;
    /// True once a `Page.windowOpen` event was observed; popups are not followed.
    fn popup_opened(&self) -> bool;
    /// Requests currently in flight (Network domain events).
    fn pending_requests(&self) -> usize;
    /// Live request/target counters (lower bounds, monotonic).
    fn counters(&self) -> SessionCounters;
    fn close<'a>(&'a self) -> HeldFuture<'a, ()>;
}
