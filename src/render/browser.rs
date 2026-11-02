//! In-process CDP renderer (IMP-DEC-002, ITEM-004).
//!
//! One browser process for the server's lifetime, one ephemeral browser context
//! per page, a throwaway profile directory under the data directory, and a
//! bounded restart budget. The renderer returns HTML only: extraction remains
//! the single downstream path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, EnableParams as DomEnableParams, GetAttributesParams, GetContentQuadsParams,
    GetDocumentParams, GetNodeForLocationParams, Node as DomNode, QuerySelectorParams,
    ScrollIntoViewIfNeededParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams as FetchEnableParams, EventRequestPaused,
    FailRequestParams, RequestPattern, RequestStage,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide::cdp::browser_protocol::log::{EnableParams as LogEnableParams, EventEntryAdded};
use chromiumoxide::cdp::browser_protocol::network::{
    BlockPattern, ErrorReason, EventDataReceived, EventLoadingFailed, EventLoadingFinished,
    EventRequestWillBeSent, EventResponseReceived, ResourceType, SetBlockedUrLsParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat,
    EnableParams as PageEnableParams, EventLoadEventFired, EventWindowOpen, FrameId,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CloseTargetParams, CreateBrowserContextParams, CreateTargetParams, EventAttachedToTarget,
    EventTargetCreated, FilterEntry, SetAutoAttachParams, TargetFilter,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventConsoleApiCalled, EventExceptionThrown,
    EventExecutionContextCreated, EventExecutionContextsCleared, RemoteObject,
};
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use futures_util::StreamExt;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use url::Url;

use super::budget::LaunchGate;
use super::discovery::ChromeResolver;
use super::session::{
    CapturedShot, ConsoleEntry, HeldFuture, HeldSession, OpenSessionFuture, ScrollTarget,
    SessionBlocked, SessionCounters, SessionGate, SessionOptions,
};
use super::{
    BlockedCounts, CaptureFuture, CaptureOptions, CapturedPage, DimensionsSource, RenderFuture,
    RenderedPage, Renderer, ShutdownFuture,
};
use crate::config::RenderConfig;
use crate::core::error::{Result, WidowError};

#[derive(Debug)]
pub struct BrowserRenderer {
    config: RenderConfig,
    root: PathBuf,
    profile_dir: PathBuf,
    resolver: Arc<dyn ChromeResolver>,
    session: Mutex<Option<BrowserSession>>,
    restart_serial: Mutex<()>,
    gate: Mutex<LaunchGate>,
    semaphore: Arc<Semaphore>,
    generation: AtomicU64,
    current_generation: AtomicU64,
    pid: AtomicU32,
    restarts: AtomicUsize,
    redact_query: bool,
}

#[derive(Debug)]
struct BrowserSession {
    browser: Arc<Browser>,
    handler: JoinHandle<()>,
    generation: u64,
    pid: u32,
}

/// Which render profile a page uses. The text profile (fetch/extract) blocks
/// stylesheets, fonts and images to keep Markdown extraction cheap. The visual
/// profile (`screenshot`, IMP-DEC-032) allows them so the capture matches what
/// a user sees; media stays blocked in both. The profile is server-fixed: no
/// tool argument selects it.
#[derive(Debug, Clone, Copy)]
struct Profile {
    block_images: bool,
    block_stylesheets: bool,
    block_fonts: bool,
    block_media: bool,
    max_requests: u32,
    max_bytes: u64,
    /// `stop_reason` names for the two caps, per profile.
    request_cap: &'static str,
    byte_cap: &'static str,
}

impl Profile {
    fn text(config: &RenderConfig) -> Self {
        Self {
            block_images: true,
            block_stylesheets: true,
            block_fonts: true,
            block_media: true,
            max_requests: config.max_requests,
            max_bytes: config.max_bytes,
            request_cap: "max_requests",
            byte_cap: "max_bytes",
        }
    }

    fn visual(config: &RenderConfig) -> Self {
        Self {
            block_images: false,
            block_stylesheets: false,
            block_fonts: false,
            block_media: true,
            max_requests: config.visual_max_requests,
            max_bytes: config.visual_max_bytes,
            request_cap: "visual_max_requests",
            byte_cap: "visual_max_bytes",
        }
    }

    fn blocks(&self, resource_type: &ResourceType) -> bool {
        match resource_type {
            ResourceType::Image => self.block_images,
            ResourceType::Media => self.block_media,
            ResourceType::Font => self.block_fonts,
            ResourceType::Stylesheet => self.block_stylesheets,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RenderPlan {
    Text,
    /// Capture after the normal settle, under the visual profile.
    Visual(CaptureOptions),
}

#[derive(Debug)]
struct CaptureData {
    png: Vec<u8>,
    width: Option<u32>,
    height: Option<u32>,
    dimensions_source: DimensionsSource,
    failed_resources: u32,
}

#[derive(Debug)]
struct PageOutcome {
    page: RenderedPage,
    capture: Option<CaptureData>,
}

impl BrowserRenderer {
    pub fn new(
        config: RenderConfig,
        root: PathBuf,
        resolver: Arc<dyn ChromeResolver>,
        redact_query: bool,
    ) -> Self {
        let profile_dir = root.join("chrome-profile");
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_pages.max(1)));
        let gate = LaunchGate::new(
            config.restart_max,
            Duration::from_secs(config.restart_window_s.max(1)),
        );
        Self {
            config,
            root,
            profile_dir,
            resolver,
            session: Mutex::new(None),
            restart_serial: Mutex::new(()),
            gate: Mutex::new(gate),
            semaphore,
            generation: AtomicU64::new(0),
            current_generation: AtomicU64::new(0),
            pid: AtomicU32::new(0),
            restarts: AtomicUsize::new(0),
            redact_query,
        }
    }

    fn wipe_profile(&self) -> Result<()> {
        if self.profile_dir.exists() {
            std::fs::remove_dir_all(&self.profile_dir).map_err(|error| {
                WidowError::RenderUnavailable(format!(
                    "cannot wipe the throwaway Chrome profile {}: {error}",
                    self.profile_dir.display()
                ))
            })?;
        }
        std::fs::create_dir_all(&self.profile_dir).map_err(|error| {
            WidowError::RenderUnavailable(format!(
                "cannot create the throwaway Chrome profile {}: {error}",
                self.profile_dir.display()
            ))
        })
    }

    async fn launch_locked(
        &self,
    ) -> std::result::Result<BrowserSession, (WidowError, Option<u64>)> {
        let now = Instant::now();
        // Only relaunches consult the budget: with `restart_max = 0` the first
        // launch must still work, otherwise "no restarts" would disable rendering.
        if !self.gate.lock().await.may_launch(now) {
            return Err((
                WidowError::RenderUnavailable(format!(
                    "renderer restart budget exhausted: {} restarts in {} s; rendering stays unavailable until the window drains",
                    self.config.restart_max, self.config.restart_window_s
                )),
                None,
            ));
        }

        let chrome = match self.resolver.resolve() {
            Ok(chrome) => chrome,
            Err(error) => return Err((error, None)),
        };
        self.wipe_profile().map_err(|error| (error, None))?;

        let browser_config = BrowserConfig::builder()
            .chrome_executable(&chrome)
            .user_data_dir(&self.profile_dir)
            .new_headless_mode()
            .disable_cache()
            .enable_request_intercept()
            .launch_timeout(Duration::from_millis(self.config.launch_timeout_ms.max(1)))
            .request_timeout(Duration::from_millis(
                self.config.ceiling_ms.saturating_add(5_000),
            ))
            // Popups are not traversed. This switch is honoured by Chromium's
            // test/headless modes; the ephemeral context is still disposed.
            .arg("block-new-web-contents")
            // Browser-internal egress reducers (RISK-044): no DNS prefetch and
            // no non-proxied UDP for WebRTC. These are launch-level, not the
            // Fetch gate.
            .arg("--dns-prefetch-disable")
            .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp")
            .build()
            .map_err(|error| {
                (
                    WidowError::RenderUnavailable(format!(
                        "invalid Chrome launch configuration for {}: {error}",
                        chrome.display()
                    )),
                    None,
                )
            })?;

        // From here on, any further attempt is a relaunch and needs budget.
        self.gate.lock().await.mark_attempted();
        let (mut browser, handler) = match Browser::launch(browser_config).await {
            Ok(value) => value,
            Err(error) => {
                let message = error.to_string();
                if message.to_ascii_lowercase().contains("sandbox") {
                    tracing::warn!(
                        "Chrome launch failed with the sandbox enabled; the renderer never disables it automatically"
                    );
                }
                // A failed launch is a health-check failure: it consumes one
                // restart slot so a crash loop cannot spawn processes forever.
                self.gate.lock().await.note_failure(Instant::now());
                return Err((
                    WidowError::RenderUnavailable(format!(
                        "Chrome could not be launched from {} (sandbox remains enabled): {message}",
                        chrome.display()
                    )),
                    None,
                ));
            }
        };

        let pid = browser
            .get_mut_child()
            .and_then(|child| child.inner.id())
            .unwrap_or(0);

        // The handler drives the CDP connection; every command, including the
        // health check, needs it polled. Spawn it before sending anything.
        let mut handler = handler;
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(error) = event {
                    tracing::warn!(error = %error, "render browser handler ended");
                    break;
                }
            }
        });

        match tokio::time::timeout(
            Duration::from_millis(self.config.launch_timeout_ms.max(1)),
            browser.version(),
        )
        .await
        {
            Ok(Ok(version)) => tracing::info!(
                browser = %version.product,
                protocol = %version.protocol_version,
                chrome = %chrome.display(),
                pid,
                "render browser launched"
            ),
            Ok(Err(error)) => {
                handler_task.abort();
                let _ = browser.kill().await;
                self.gate.lock().await.note_failure(Instant::now());
                return Err((
                    WidowError::RenderUnavailable(format!(
                        "Chrome started but did not answer a CDP health check: {error}"
                    )),
                    None,
                ));
            }
            Err(_) => {
                handler_task.abort();
                let _ = browser.kill().await;
                self.gate.lock().await.note_failure(Instant::now());
                return Err((
                    WidowError::RenderUnavailable(
                        "Chrome started but the CDP health check timed out".to_string(),
                    ),
                    None,
                ));
            }
        }

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.current_generation.store(generation, Ordering::SeqCst);
        self.pid.store(pid, Ordering::SeqCst);
        Ok(BrowserSession {
            browser: Arc::new(browser),
            handler: handler_task,
            generation,
            pid,
        })
    }

    async fn close_session(&self, session: BrowserSession) {
        let BrowserSession {
            mut browser,
            handler,
            generation: _,
            pid,
        } = session;
        let children = if pid != 0 {
            descendant_pids(pid)
        } else {
            Vec::new()
        };
        // A live held session keeps its own Arc<Browser>; graceful close then
        // waits for it. The pid-based kill below still ends the tree.
        if let Some(browser) = Arc::get_mut(&mut browser) {
            let _ = tokio::time::timeout(Duration::from_secs(3), browser.close()).await;
            let _ = browser.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(3), browser.wait()).await;
        }
        // A graceful close should have ended the tree; kill any lingering
        // renderer/zygote process so it cannot recreate the profile directory.
        kill_pids(&children);
        handler.abort();
        let _ = handler.await;
    }

    async fn new_page(
        &self,
    ) -> std::result::Result<(Page, BrowserContextId, u64), (WidowError, Option<u64>)> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.launch_locked().await?);
        }
        let session = guard.as_ref().expect("session was just ensured to exist");
        let context_id = session
            .browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await
            .map_err(|error| {
                (
                    WidowError::RenderFailed(format!(
                        "could not create an ephemeral browser context: {error}"
                    )),
                    Some(session.generation),
                )
            })?;
        let mut params = CreateTargetParams::new("about:blank");
        params.browser_context_id = Some(context_id.clone());
        let page = session.browser.new_page(params).await.map_err(|error| {
            (
                WidowError::RenderFailed(format!("could not create a render page: {error}")),
                Some(session.generation),
            )
        })?;
        Ok((page, context_id, session.generation))
    }

    async fn health_check(&self, generation: u64) -> bool {
        let guard = self.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return false;
        };
        if session.generation != generation {
            return true;
        }
        matches!(
            tokio::time::timeout(Duration::from_secs(1), session.browser.version()).await,
            Ok(Ok(_))
        )
    }

    async fn invalidate(&self, generation: u64, reason: &str) -> Result<()> {
        let _serial = self.restart_serial.lock().await;
        if self.current_generation.load(Ordering::SeqCst) != generation {
            return Ok(());
        }
        {
            let mut guard = self.session.lock().await;
            self.pid.store(0, Ordering::SeqCst);
            if let Some(session) = guard.take() {
                self.close_session(session).await;
            }
        }
        let allowed = self.gate.lock().await.note_failure(Instant::now());
        self.restarts.fetch_add(1, Ordering::SeqCst);
        if !allowed {
            return Err(WidowError::RenderUnavailable(format!(
                "renderer restart budget exhausted after {reason}; rendering is unavailable until the {}-second window drains",
                self.config.restart_window_s
            )));
        }
        tracing::warn!(reason, "render browser restarting");
        Ok(())
    }

    async fn dispose_context(&self, generation: u64, context_id: BrowserContextId) {
        let guard = self.session.lock().await;
        if let Some(session) = guard.as_ref()
            && session.generation == generation
        {
            let _ = session.browser.dispose_browser_context(context_id).await;
        }
    }

    async fn page_once(
        &self,
        page: Page,
        context_id: BrowserContextId,
        generation: u64,
        target: &Url,
        started: Instant,
        plan: RenderPlan,
    ) -> Result<PageOutcome> {
        let outcome = self.drive_page(&page, target, started, plan).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), page.close()).await;
        self.dispose_context(generation, context_id).await;
        outcome
    }

    /// One page through the permit, launch/recovery and retry path. Both
    /// `render` and `capture` share it so the gate/robots/caps/settle path
    /// cannot drift between them.
    async fn drive(&self, target: &Url, plan: RenderPlan) -> Result<PageOutcome> {
        let _permit =
            self.semaphore.acquire().await.map_err(|_| {
                WidowError::RenderUnavailable("renderer is shutting down".to_string())
            })?;
        let started = Instant::now();
        let (page, context_id, generation) = match self.new_page().await {
            Ok(value) => value,
            Err((error, Some(generation))) => {
                self.invalidate(generation, &error.to_string()).await?;
                self.new_page().await.map_err(|(error, _)| error)?
            }
            Err((error, None)) => return Err(error),
        };

        match self
            .page_once(page, context_id, generation, target, started, plan)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                if self.health_check(generation).await {
                    return Err(error);
                }
                self.invalidate(generation, &error.to_string()).await?;
                let (page, context_id, generation) =
                    self.new_page().await.map_err(|(error, _)| error)?;
                self.page_once(page, context_id, generation, target, started, plan)
                    .await
            }
        }
    }

    async fn drive_page(
        &self,
        page: &Page,
        target: &Url,
        started: Instant,
        plan: RenderPlan,
    ) -> Result<PageOutcome> {
        let profile = match plan {
            RenderPlan::Text => Profile::text(&self.config),
            RenderPlan::Visual(_) => Profile::visual(&self.config),
        };
        let state = Arc::new(NetworkState::new(started, profile));

        page.execute(PageEnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Page.enable failed: {error}")))?;
        page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Network.enable failed: {error}")))?;

        let mut load_events = page
            .event_listener::<EventLoadEventFired>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("load listener failed: {error}")))?;
        let request_paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("fetch listener failed: {error}")))?;
        let data_received = page
            .event_listener::<EventDataReceived>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("network listener failed: {error}"))
            })?;
        let responses = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("response listener failed: {error}"))
            })?;
        let finished = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("finish listener failed: {error}"))
            })?;
        let request_started = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("request listener failed: {error}"))
            })?;

        let intercept_task = tokio::spawn(intercept_requests(
            request_paused,
            page.clone(),
            Arc::clone(&state),
        ));
        let bytes_task = tokio::spawn(track_bytes(data_received, page.clone(), Arc::clone(&state)));
        let finished_task =
            tokio::spawn(track_finished(finished, page.clone(), Arc::clone(&state)));
        let response_task = tokio::spawn(track_responses(responses, Arc::clone(&state)));
        let activity_task = tokio::spawn(track_activity(request_started, Arc::clone(&state)));

        let ceiling = Duration::from_millis(self.config.ceiling_ms.max(1));
        let deadline = tokio::time::Instant::now() + ceiling;
        let std_deadline = started + ceiling;
        let navigation = tokio::time::timeout_at(deadline, page.goto(target.clone())).await;
        let mut partial = false;
        let mut stop_reason: Option<String> = None;

        match navigation {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                stop_tasks(
                    intercept_task,
                    bytes_task,
                    finished_task,
                    response_task,
                    activity_task,
                )
                .await;
                return Err(WidowError::RenderFailed(format!(
                    "navigation to {} failed: {error}",
                    target
                )));
            }
            Err(_) => {
                partial = true;
                stop_reason = Some("navigation_ceiling".to_string());
            }
        }

        if !partial {
            match tokio::time::timeout_at(deadline, load_events.next()).await {
                Ok(Some(_)) => {
                    state.touch();
                    let quiet = Duration::from_millis(self.config.settle_ms);
                    loop {
                        if let Some(reason) = state.cap() {
                            partial = true;
                            stop_reason = Some(reason.to_string());
                            break;
                        }
                        let now = Instant::now();
                        if now >= std_deadline {
                            partial = true;
                            stop_reason = Some("ceiling".to_string());
                            break;
                        }
                        let quiet_at = state.last_activity() + quiet;
                        if now >= quiet_at {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Ok(None) => {
                    stop_tasks(
                        intercept_task,
                        bytes_task,
                        finished_task,
                        response_task,
                        activity_task,
                    )
                    .await;
                    return Err(WidowError::RenderFailed(
                        "the browser event stream ended before the load event".to_string(),
                    ));
                }
                Err(_) => {
                    partial = true;
                    stop_reason = Some("load_ceiling".to_string());
                }
            }
        }

        // A cap can fire while a slow navigation command is still pending; the
        // cap is the more specific and more honest reason.
        if let Some(reason) = state.cap() {
            partial = true;
            stop_reason = Some(reason.to_string());
        }

        if partial || stop_reason.is_some() {
            state.trigger_stop(page).await;
        }

        let capture = match plan {
            RenderPlan::Text => None,
            RenderPlan::Visual(options) => {
                let capture = self.capture_settled(page, options, &state).await?;
                // A slow asset can hit a visual cap during the post-settle
                // wait; the capture still exists, but it is honest to report
                // the cap that fired.
                if let Some(reason) = state.cap() {
                    partial = true;
                    stop_reason = Some(reason.to_string());
                }
                Some(capture)
            }
        };

        let html = match tokio::time::timeout(Duration::from_secs(3), page.content()).await {
            Ok(Ok(html)) => html,
            Ok(Err(error)) => {
                stop_tasks(
                    intercept_task,
                    bytes_task,
                    finished_task,
                    response_task,
                    activity_task,
                )
                .await;
                return Err(WidowError::RenderFailed(format!(
                    "could not serialize the rendered DOM: {error}"
                )));
            }
            Err(_) => {
                stop_tasks(
                    intercept_task,
                    bytes_task,
                    finished_task,
                    response_task,
                    activity_task,
                )
                .await;
                return Err(WidowError::RenderFailed(
                    "timed out serializing the rendered DOM".to_string(),
                ));
            }
        };

        let final_url = page
            .url()
            .await
            .ok()
            .flatten()
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| target.clone());
        let status = state.main_status();
        let content_type = state.main_mime();
        let blocked = state.blocked();
        let requests = state.requests();
        let bytes = state.bytes();

        stop_tasks(
            intercept_task,
            bytes_task,
            finished_task,
            response_task,
            activity_task,
        )
        .await;

        tracing::info!(
            target = %crate::pipeline::log_url(target.as_str(), self.redact_query),
            final_url = %crate::pipeline::log_url(final_url.as_str(), self.redact_query),
            status,
            requests,
            bytes,
            partial,
            stop_reason = stop_reason.as_deref().unwrap_or("settled"),
            images_blocked = blocked.image,
            media_blocked = blocked.media,
            fonts_blocked = blocked.font,
            stylesheets_blocked = blocked.stylesheet,
            captured = capture.is_some(),
            duration_ms = started.elapsed().as_millis(),
            "render page completed"
        );

        Ok(PageOutcome {
            page: RenderedPage {
                html,
                final_url,
                status,
                content_type,
                partial,
                stop_reason,
                blocked,
                requests,
                bytes,
                duration: started.elapsed(),
            },
            capture,
        })
    }

    /// Post-settle wait, then `Page.captureScreenshot` (PNG) and the layout
    /// metrics that name the reported dimensions (IMP-DEC-032). A metrics
    /// failure yields `width`/`height` `None` and `dimensions_source`
    /// `unknown`; it never invents a size. A capture failure is a typed
    /// `render_failed`, not an empty image.
    async fn capture_settled(
        &self,
        page: &Page,
        options: CaptureOptions,
        state: &NetworkState,
    ) -> Result<CaptureData> {
        if options.wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(options.wait_ms.min(10_000))).await;
        }
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .capture_beyond_viewport(options.full_page)
            .full_page(options.full_page)
            .build();
        let png = match tokio::time::timeout(Duration::from_secs(20), page.screenshot(params)).await
        {
            Ok(Ok(png)) => png,
            Ok(Err(error)) => {
                return Err(WidowError::RenderFailed(format!(
                    "Page.captureScreenshot failed: {error}"
                )));
            }
            Err(_) => {
                return Err(WidowError::RenderFailed(
                    "timed out capturing the page screenshot".to_string(),
                ));
            }
        };
        let (width, height, dimensions_source) = match tokio::time::timeout(
            Duration::from_secs(5),
            page.layout_metrics(),
        )
        .await
        {
            Ok(Ok(metrics)) if options.full_page => (
                css_px(metrics.css_content_size.width),
                css_px(metrics.css_content_size.height),
                DimensionsSource::Content,
            ),
            Ok(Ok(metrics)) => (
                css_px(metrics.css_layout_viewport.client_width as f64),
                css_px(metrics.css_layout_viewport.client_height as f64),
                DimensionsSource::Viewport,
            ),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "Page.getLayoutMetrics failed; screenshot dimensions stay unknown");
                (None, None, DimensionsSource::Unknown)
            }
            Err(_) => {
                tracing::warn!(
                    "Page.getLayoutMetrics timed out; screenshot dimensions stay unknown"
                );
                (None, None, DimensionsSource::Unknown)
            }
        };
        Ok(CaptureData {
            png,
            width,
            height,
            dimensions_source,
            failed_resources: state.failed_resources(),
        })
    }
}

/// `DOM.Node.nodeType` for an element node.
const ELEMENT_NODE: i64 = 1;

/// Hard ceiling on paused requests per session, *including refused ones*, so a
/// page cannot turn the gate into an unbounded vet/DNS channel (review F6).
const MAX_SESSION_PAUSED_REQUESTS: u64 = 500;
/// Rolling typed strings scrubbed from console output (RISK-045/F8).
const MAX_TYPED_SECRETS: usize = 8;

/// Server-owned document guard template. `__NONCE__` is per-session and is
/// exposed only through a non-configurable probe function; the cancellation
/// channel is a closure behind a getter-only, non-configurable property so page
/// script can read it but cannot forge or suppress it (review G5).
const POPUP_GUARD_JS_TEMPLATE: &str = r#"(() => {
  const nonce = "__NONCE__";
  // Every primitive the guard relies on is captured at install time, before any
  // page script runs on this document. A page that later replaces
  // Object.freeze, getAttribute, querySelector, composedPath, preventDefault or
  // Reflect.apply cannot change what the guard already holds (review B2f/F3-F4).
  const doc = document;
  const win = window;
  const apply = Reflect.apply;
  const defineProperty = Object.defineProperty;
  const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  const freeze = Object.freeze;
  const assign = Object.assign;
  const str = String;
  const slice = String.prototype.slice;
  const charCodeAt = String.prototype.charCodeAt;
  const fromCharCode = String.fromCharCode;
  const elementProto = Element.prototype;
  const nodeProto = Node.prototype;
  const nativeGetAttribute = elementProto.getAttribute;
  const nativeClosest = elementProto.closest;
  const nativeAttachShadow = elementProto.attachShadow;
  const nativeGetRootNode = nodeProto.getRootNode;
  const nativeDocGetElementById = Document.prototype.getElementById;
  const nativeFragmentGetElementById =
    typeof DocumentFragment !== 'undefined' ? DocumentFragment.prototype.getElementById : null;
  const nativeQuerySelector = Document.prototype.querySelector;
  const eventProto = Event.prototype;
  const nativeComposedPath = eventProto.composedPath;
  const nativePreventDefault = eventProto.preventDefault;
  const nativeStopImmediate = eventProto.stopImmediatePropagation;
  const nativeDispatchEvent = EventTarget.prototype.dispatchEvent;
  const nativeAddEventListener = EventTarget.prototype.addEventListener;
  const getterOf = (proto, name) => {
    const descriptor = proto ? getOwnPropertyDescriptor(proto, name) : null;
    return descriptor && descriptor.get ? descriptor.get : null;
  };
  const localNameGetter =
    getterOf(nodeProto, 'localName') || getterOf(elementProto, 'localName');
  const typeGetter = getterOf(eventProto, 'type');
  const buttonFormGetter = getterOf(HTMLButtonElement.prototype, 'form');
  const inputFormGetter = getterOf(HTMLInputElement.prototype, 'form');
  const labelControlGetter = getterOf(HTMLLabelElement.prototype, 'control');
  const submitterGetter =
    typeof SubmitEvent !== 'undefined' ? getterOf(SubmitEvent.prototype, 'submitter') : null;
  const localNameOf = (node) => {
    if (!node || !localNameGetter) return '';
    try { return str(apply(localNameGetter, node, [])); } catch (_) { return ''; }
  };
  const attr = (node, name) => {
    if (!node || !nativeGetAttribute) return null;
    try { return apply(nativeGetAttribute, node, [name]); } catch (_) { return null; }
  };
  const baseTarget = () => {
    if (!nativeQuerySelector) return '';
    try {
      const base = apply(nativeQuerySelector, doc, ['base[target]']);
      return base ? (attr(base, 'target') || '') : '';
    } catch (_) { return ''; }
  };
  // ASCII-only code-point fold, byte-equivalent to the server's
  // `eq_ignore_ascii_case`: a Unicode `toLowerCase` could classify a future
  // keyword differently in the two predicates (review B2j low).
  const asciiLower = (value) => {
    const text = str(value);
    let folded = '';
    for (let index = 0; index < text.length; index += 1) {
      const code = apply(charCodeAt, text, [index]);
      folded += (code >= 65 && code <= 90) ? apply(fromCharCode, undefined, [code + 32]) : text[index];
    }
    return folded;
  };
  const opensNewContext = (target) => {
    // An empty effective target falls to the document's base target; a
    // whitespace-only name is a named browsing context (g30/g35/g36 measured
    // both). The submitter/form precedence decides the target string, not
    // whether the base applies. Keywords are ASCII case-insensitive.
    const effective = (target === null || target === undefined || target === '')
      ? baseTarget()
      : target;
    if (!effective) return false;
    const keyword = asciiLower(effective);
    if (keyword === '_self' || keyword === '_top' || keyword === '_parent') return false;
    return true;
  };
  let cancelledSeq = 0;
  let cancelledValue = null;
  // frozen with the captured primitive; the object lives only in this closure
  // and is reachable from the page as a read-only view (review B2f/F3).
  const mark = (kind, target) => {
    cancelledSeq += 1;
    cancelledValue = freeze({
      seq: cancelledSeq,
      kind: str(kind),
      target: apply(slice, str(target || ''), [0, 200]),
    });
  };
  try {
    defineProperty(win, '__wwSessionGuard', {
      value: function () { return nonce; },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  try {
    defineProperty(win, '__wwCancelled', {
      get: function () { return cancelledValue; },
      set: function (_) {},
      configurable: false,
    });
  } catch (_) {}
  const denyOpen = function () {
    mark('popup_blocked', '_blank');
    return null;
  };
  try {
    defineProperty(win, 'open', {
      value: denyOpen,
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  try {
    const descriptor = getOwnPropertyDescriptor(Window.prototype, 'open');
    if (descriptor && descriptor.value) {
      defineProperty(Window.prototype, 'open', {
        value: denyOpen,
        writable: false,
        configurable: false,
      });
    }
  } catch (_) {}
  try {
    defineProperty(elementProto, 'attachShadow', {
      value: function (init) {
        return apply(nativeAttachShadow, this, [assign({}, init || {}, { mode: 'open' })]);
      },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  try {
    const proto = HTMLFormElement.prototype;
    const nativeSubmit = proto.submit;
    defineProperty(proto, 'submit', {
      value: function () {
        // The UA reads the content attribute; a page-writable reflected
        // property must not decide the policy (review G4).
        const target = attr(this, 'target');
        if (opensNewContext(target)) {
          mark('navigation_cancelled', target);
          return;
        }
        return apply(nativeSubmit, this, []);
      },
      writable: false,
      configurable: false,
    });
    const nativeRequest = proto.requestSubmit;
    if (nativeRequest) {
      defineProperty(proto, 'requestSubmit', {
        value: function (submitter) {
          // A **present** submitter formtarget takes precedence over the form
          // target (even when empty); an absent one falls back to it. An empty
          // effective target then falls to the base target (g35/g36).
          const override = submitter ? attr(submitter, 'formtarget') : null;
          const target = override !== null ? override : attr(this, 'target');
          if (opensNewContext(target)) {
            mark('navigation_cancelled', target);
            return;
          }
          return apply(nativeRequest, this, [submitter]);
        },
        writable: false,
        configurable: false,
      });
    }
  } catch (_) {}
  // The form owner of a submit control: a non-empty `form` content attribute
  // resolves by id in the element's own tree (a sibling form in a shadow tree is
  // invisible to an event path and to closest()); otherwise the ancestor form
  // applies. Review B2g/F1.
  const formOwnerOf = (node, name) => {
    const reference = attr(node, 'form');
    if (reference) {
      let root = null;
      if (nativeGetRootNode) {
        try { root = apply(nativeGetRootNode, node, []); } catch (_) { root = null; }
      }
      const getById = root && root.nodeType === 9
        ? nativeDocGetElementById
        : nativeFragmentGetElementById;
      if (getById && root) {
        try {
          const candidate = apply(getById, root, [reference]);
          if (candidate && localNameOf(candidate).toLowerCase() === 'form') return candidate;
        } catch (_) {}
      }
      return null;
    }
    const formGetter = name === 'input' ? inputFormGetter : buttonFormGetter;
    if (formGetter) {
      try {
        const owner = apply(formGetter, node, []);
        if (owner) return owner;
      } catch (_) {}
    }
    if (nativeClosest) {
      try { return apply(nativeClosest, node, ['form']); } catch (_) {}
    }
    return null;
  };
  const labelControlOf = (node) => {
    if (!labelControlGetter) return null;
    try { return apply(labelControlGetter, node, []); } catch (_) { return null; }
  };
  const activatorTarget = (node) => {
    const name = localNameOf(node).toLowerCase();
    if (name === 'label') {
      // A label click activates its labeled control; the control may be a
      // sibling in a shadow tree and is never in the event path (review B2h).
      const control = labelControlOf(node);
      return control ? activatorTarget(control) : null;
    }
    if (name === 'a') {
      // An href-less anchor is not interactive content (spec/Blink: IsLink).
      if (attr(node, 'href') === null) return null;
      const target = attr(node, 'target');
      return opensNewContext(target) ? (target || baseTarget()) : null;
    }
    if (name === 'area') {
      const target = attr(node, 'target');
      return opensNewContext(target) ? (target || baseTarget()) : null;
    }
    if (name === 'button' || name === 'input') {
      const type = (attr(node, 'type') || '').toLowerCase();
      const isSubmit = name === 'button'
        ? (type === '' || type === 'submit')
        : (type === 'submit' || type === 'image');
      if (!isSubmit) return null;
      const form = formOwnerOf(node, name);
      const override = attr(node, 'formtarget');
      const target = override !== null ? override : (form ? attr(form, 'target') : '');
      return opensNewContext(target) ? (target || baseTarget()) : null;
    }
    return null;
  };
  // Page-script activation: element.click() and dispatchEvent('click') are
  // intercepted before the native call, including inside shadow roots whose
  // event path never reaches the window listener (review F5).
  try {
    const proto = HTMLElement.prototype;
    const nativeClick = proto.click;
    defineProperty(proto, 'click', {
      value: function () {
        const target = activatorTarget(this);
        if (target !== null) {
          mark('navigation_cancelled', target);
          return;
        }
        return apply(nativeClick, this, []);
      },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  try {
    const proto = EventTarget.prototype;
    defineProperty(proto, 'dispatchEvent', {
      value: function (event) {
        let type = '';
        if (event && typeGetter) {
          try { type = str(apply(typeGetter, event, [])); } catch (_) { type = ''; }
        }
        if (type === 'click') {
          const target = activatorTarget(this);
          if (target !== null) {
            mark('navigation_cancelled', target);
            return false;
          }
        }
        return apply(nativeDispatchEvent, this, [event]);
      },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  const pathOf = (event) => {
    if (!event || !nativeComposedPath) return [];
    try {
      const path = apply(nativeComposedPath, event, []);
      return Array.isArray(path) ? path : [];
    } catch (_) { return []; }
  };
  // Interactive content (HTML spec): a label click that targets one of these,
  // or a descendant of one, does not run the label's activation behaviour
  // (review B2i/F2).
  const isInteractiveContent = (node) => {
    const name = asciiLower(localNameOf(node));
    if (name === 'a') {
      // The spec conditions the anchor on `href`; the UA forwards label
      // activation for an href-less anchor (g33d measured it).
      return attr(node, 'href') !== null;
    }
    if (name === 'button' || name === 'details' || name === 'embed' ||
        name === 'iframe' || name === 'label' || name === 'select' || name === 'textarea') {
      return true;
    }
    if (name === 'input') {
      // `input` is interactive content only outside the Hidden state.
      return asciiLower(attr(node, 'type') || '') !== 'hidden';
    }
    if (name === 'audio' || name === 'video') return attr(node, 'controls') !== null;
    if (name === 'img') return attr(node, 'usemap') !== null;
    return false;
  };
  const cancel = (event) => {
    const path = pathOf(event);
    for (let index = 0; index < path.length; index += 1) {
      const node = path[index];
      const name = asciiLower(localNameOf(node));
      if (name === 'label') {
        let suppressed = false;
        for (let inner = 0; inner < index; inner += 1) {
          if (isInteractiveContent(path[inner])) { suppressed = true; break; }
        }
        const control = suppressed ? null : labelControlOf(node);
        const target = control ? activatorTarget(control) : null;
        if (target !== null) {
          apply(nativePreventDefault, event, []);
          apply(nativeStopImmediate, event, []);
          mark('navigation_cancelled', target);
          return;
        }
      }
      if ((name === 'a' || name === 'area') && attr(node, 'href') !== null &&
          opensNewContext(attr(node, 'target'))) {
        apply(nativePreventDefault, event, []);
        apply(nativeStopImmediate, event, []);
        mark('navigation_cancelled', attr(node, 'target'));
        return;
      }
    }
    for (const node of path) {
      const name = localNameOf(node).toLowerCase();
      if (name !== 'button' && name !== 'input') continue;
      const type = (attr(node, 'type') || '').toLowerCase();
      const isSubmit = name === 'button'
        ? (type === '' || type === 'submit')
        : (type === 'submit' || type === 'image');
      if (!isSubmit) continue;
      const form = formOwnerOf(node, name);
      // A present `formtarget` (even empty) is used verbatim; only an absent
      // one falls back to the form target (g35 measured the empty member as
      // same-tab).
      const override = attr(node, 'formtarget');
      const target = override !== null ? override : (form ? attr(form, 'target') : '');
      if (opensNewContext(target)) {
        apply(nativePreventDefault, event, []);
        apply(nativeStopImmediate, event, []);
        mark('navigation_cancelled', target);
        return;
      }
    }
  };
  // A submit event's effective target is the submitter's non-empty
  // `formtarget`, else the form's `target` — the same rule the click path and
  // the server use. Reading only the form's target over-blocks a
  // `formtarget="_self"` submit inside a `_blank` form (review B2h low #2).
  const submitTarget = (event, form) => {
    let submitter = null;
    if (submitterGetter) {
      try { submitter = apply(submitterGetter, event, []); } catch (_) { submitter = null; }
    }
    if (submitter) {
      const override = attr(submitter, 'formtarget');
      if (override !== null) return override;
    }
    return form ? attr(form, 'target') : '';
  };
  const submitCancel = (event) => {
    for (const node of pathOf(event)) {
      if (localNameOf(node).toLowerCase() !== 'form') continue;
      const target = submitTarget(event, node);
      // `submitTarget` returns the submitter's formtarget when present (even
      // empty), otherwise the form target; the base resolution applies after.
      if (opensNewContext(target)) {
        apply(nativePreventDefault, event, []);
        apply(nativeStopImmediate, event, []);
        mark('navigation_cancelled', target);
        return;
      }
    }
  };
  const installListeners = () => {
    if (!nativeAddEventListener) return;
    try { apply(nativeAddEventListener, win, ['click', cancel, true]); } catch (_) {}
    try { apply(nativeAddEventListener, win, ['submit', submitCancel, true]); } catch (_) {}
  };
  installListeners();
  // document.open() — and the implicit open behind document.write() on a loaded
  // document — removes every listener registered on the Document and its Window
  // while the patched prototypes and this closure survive. Re-assert the
  // listener set after each of those calls; re-adding the same reference is a
  // no-op while it is still installed (review B2g/F2).
  const reassertAfterDocumentWrite = (proto, method) => {
    try {
      const native = proto[method];
      if (typeof native !== 'function') return;
      defineProperty(proto, method, {
        value: function (...args) {
          const result = apply(native, this, args);
          installListeners();
          return result;
        },
        writable: false,
        configurable: false,
      });
    } catch (_) {}
  };
  reassertAfterDocumentWrite(Document.prototype, 'open');
  reassertAfterDocumentWrite(Document.prototype, 'write');
  reassertAfterDocumentWrite(Document.prototype, 'writeln');
  try {
    defineProperty(win, 'SharedWorker', {
      value: function () {
        mark('worker_blocked', 'shared_worker');
        throw new Error('SharedWorker is disabled in an interaction session');
      },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
  try {
    defineProperty(ServiceWorkerContainer.prototype, 'register', {
      value: function () {
        return Promise.reject(new Error('service workers are disabled in an interaction session'));
      },
      writable: false,
      configurable: false,
    });
  } catch (_) {}
})();
"#;

/// The isolated-world heartbeat carries its own secret, never the main-world
/// guard nonce, and a distinct prefix: the page can call `__wwSessionGuard()`
/// and read that nonce, so a shared value would let it forge the heartbeat
/// (review B2g/F4). The console handler additionally refuses heartbeats that
/// arrive from a default (main-world) execution context.
const INTEGRITY_GUARD_JS_TEMPLATE: &str = r#"
(() => {
  try { console.debug('WWIW2:__IWNONCE__'); } catch (_) {}
})();
"#;

fn popup_guard_js(nonce: &str) -> String {
    POPUP_GUARD_JS_TEMPLATE.replace("__NONCE__", nonce)
}

fn integrity_guard_js(nonce: &str) -> String {
    INTEGRITY_GUARD_JS_TEMPLATE.replace("__IWNONCE__", nonce)
}

/// A CSS-pixel measurement as a non-negative whole number; a non-finite or
/// negative value is unknown rather than invented.
fn css_px(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value.round().clamp(0.0, u32::MAX as f64) as u32)
}

impl Renderer for BrowserRenderer {
    fn render<'a>(&'a self, url: &'a Url) -> RenderFuture<'a> {
        Box::pin(async move {
            self.drive(url, RenderPlan::Text)
                .await
                .map(|outcome| outcome.page)
        })
    }

    fn open_session<'a>(&'a self, url: &'a Url, options: SessionOptions) -> OpenSessionFuture<'a> {
        Box::pin(async move {
            let session = self.open_session_impl(url, options).await?;
            Ok(Box::new(session) as Box<dyn HeldSession>)
        })
    }

    fn capture<'a>(&'a self, url: &'a Url, options: CaptureOptions) -> CaptureFuture<'a> {
        Box::pin(async move {
            let outcome = self.drive(url, RenderPlan::Visual(options)).await?;
            let capture = outcome.capture.ok_or_else(|| {
                WidowError::internal("the visual render plan returned no capture")
            })?;
            Ok(CapturedPage {
                page: outcome.page,
                png: capture.png,
                width: capture.width,
                height: capture.height,
                dimensions_source: capture.dimensions_source,
                failed_resources: capture.failed_resources,
            })
        })
    }

    fn preflight(&self) -> Result<()> {
        self.resolver.resolve().map(|_| ())
    }

    fn shutdown<'a>(&'a self) -> ShutdownFuture<'a> {
        Box::pin(async move {
            let pid = self.pid.swap(0, Ordering::SeqCst);
            let children = if pid == 0 {
                Vec::new()
            } else {
                descendant_pids(pid)
            };
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.take() {
                self.close_session(session).await;
            }
            // Belt and braces: the pid captured at launch must be gone even if
            // the session handle was already taken by a restart path.
            if pid != 0 {
                let _ = std::process::Command::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            kill_pids(&children);
            // Chromium can recreate a lock/heap file for a few milliseconds
            // after the process exits; retry the wipe before declaring success.
            for _ in 0..40 {
                let _ = tokio::fs::remove_dir_all(&self.profile_dir).await;
                if !self.profile_dir.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let _ = tokio::fs::remove_dir(&self.root).await;
        })
    }

    fn browser_pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    fn restart_count(&self) -> usize {
        self.restarts.load(Ordering::SeqCst)
    }
}

impl Drop for BrowserRenderer {
    fn drop(&mut self) {
        // A test or an early-exit server may drop the renderer while Chrome is
        // still running. `kill_on_drop` normally handles the direct child, but
        // an orphaned Chrome has been observed when the runtime exits first, so
        // the renderer kills by pid and removes its throwaway profile on drop.
        let mut pid = self.pid.swap(0, Ordering::SeqCst);
        let mut children = Vec::new();
        if let Ok(mut guard) = self.session.try_lock()
            && let Some(mut session) = guard.take()
        {
            session.handler.abort();
            pid = session.pid;
            if let Some(browser) = Arc::get_mut(&mut session.browser)
                && let Some(child_pid) = browser.get_mut_child().and_then(|child| child.inner.id())
            {
                pid = child_pid;
            }
            children = descendant_pids(pid);
        }
        if pid != 0 {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        kill_pids(&children);
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

fn descendant_pids(root: u32) -> Vec<u32> {
    let mut edges: Vec<(u32, u32)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        if let Some(ppid) = fields.get(1).and_then(|value| value.parse::<u32>().ok()) {
            edges.push((pid, ppid));
        }
    }
    let mut descendants = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &edges {
            if *ppid == parent && !descendants.contains(pid) {
                descendants.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    descendants
}

fn kill_pids(pids: &[u32]) {
    for pid in pids.iter().rev() {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[derive(Debug)]
struct NetworkState {
    started: Instant,
    last_activity_ms: AtomicU64,
    requests: AtomicU32,
    bytes: AtomicU64,
    finished_bytes: AtomicU64,
    image: AtomicU32,
    media: AtomicU32,
    font: AtomicU32,
    stylesheet: AtomicU32,
    cap: StdMutex<Option<&'static str>>,
    stop_sent: AtomicBool,
    main_status: AtomicU16,
    main_mime: StdMutex<Option<String>>,
    /// Requests a profile policy refused (media/`file:`); informational.
    refused: AtomicU32,
    profile: Profile,
}

impl NetworkState {
    fn new(started: Instant, profile: Profile) -> Self {
        Self {
            started,
            last_activity_ms: AtomicU64::new(0),
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            finished_bytes: AtomicU64::new(0),
            image: AtomicU32::new(0),
            media: AtomicU32::new(0),
            font: AtomicU32::new(0),
            stylesheet: AtomicU32::new(0),
            cap: StdMutex::new(None),
            stop_sent: AtomicBool::new(false),
            main_status: AtomicU16::new(0),
            main_mime: StdMutex::new(None),
            refused: AtomicU32::new(0),
            profile,
        }
    }

    fn profile(&self) -> Profile {
        self.profile
    }

    fn note_refused(&self) {
        self.refused.fetch_add(1, Ordering::SeqCst);
    }

    fn failed_resources(&self) -> u32 {
        self.refused.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        let millis = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.last_activity_ms.store(millis, Ordering::SeqCst);
    }

    fn last_activity(&self) -> Instant {
        self.started + Duration::from_millis(self.last_activity_ms.load(Ordering::SeqCst))
    }

    fn requests(&self) -> u32 {
        self.requests.load(Ordering::SeqCst)
    }

    fn bytes(&self) -> u64 {
        self.bytes
            .load(Ordering::SeqCst)
            .max(self.finished_bytes.load(Ordering::SeqCst))
    }

    fn blocked(&self) -> BlockedCounts {
        BlockedCounts {
            image: self.image.load(Ordering::SeqCst) as usize,
            media: self.media.load(Ordering::SeqCst) as usize,
            font: self.font.load(Ordering::SeqCst) as usize,
            stylesheet: self.stylesheet.load(Ordering::SeqCst) as usize,
        }
    }

    fn cap(&self) -> Option<&'static str> {
        *self
            .cap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_cap(&self, reason: &'static str) -> bool {
        let mut cap = self
            .cap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cap.is_none() {
            *cap = Some(reason);
            true
        } else {
            false
        }
    }

    fn main_status(&self) -> u16 {
        match self.main_status.load(Ordering::SeqCst) {
            0 => 200,
            status => status,
        }
    }

    fn main_mime(&self) -> Option<String> {
        self.main_mime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn trigger_stop(&self, page: &Page) {
        if self.stop_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = page.evaluate("window.stop()").await;
    }
}

async fn intercept_requests(
    mut paused: chromiumoxide::listeners::EventStream<EventRequestPaused>,
    page: Page,
    state: Arc<NetworkState>,
) {
    while let Some(event) = paused.next().await {
        state.touch();
        if event.response_status_code.is_some() {
            let _ = page
                .execute(ContinueRequestParams::new(event.request_id.clone()))
                .await;
            continue;
        }
        // Count every resource type that reaches this point, whether the
        // profile allows it or not: the counters are per-page diagnostics.
        match event.resource_type {
            ResourceType::Image => {
                state.image.fetch_add(1, Ordering::SeqCst);
            }
            ResourceType::Media => {
                state.media.fetch_add(1, Ordering::SeqCst);
            }
            ResourceType::Font => {
                state.font.fetch_add(1, Ordering::SeqCst);
            }
            ResourceType::Stylesheet => {
                state.stylesheet.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        let blocked =
            state.profile().blocks(&event.resource_type) || event.request.url.starts_with("file:");
        if blocked {
            state.note_refused();
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }

        let count = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if count > state.profile().max_requests {
            state.set_cap(state.profile().request_cap);
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            state.trigger_stop(&page).await;
            continue;
        }
        let _ = page
            .execute(ContinueRequestParams::new(event.request_id.clone()))
            .await;
    }
}

async fn track_bytes(
    mut received: chromiumoxide::listeners::EventStream<EventDataReceived>,
    page: Page,
    state: Arc<NetworkState>,
) {
    while let Some(event) = received.next().await {
        state.touch();
        let length = event.encoded_data_length.max(0) as u64;
        let total = state.bytes.fetch_add(length, Ordering::SeqCst) + length;
        if total > state.profile().max_bytes && state.set_cap(state.profile().byte_cap) {
            state.trigger_stop(&page).await;
        }
    }
}

async fn track_finished(
    mut finished: chromiumoxide::listeners::EventStream<EventLoadingFinished>,
    page: Page,
    state: Arc<NetworkState>,
) {
    while let Some(event) = finished.next().await {
        state.touch();
        let length = event.encoded_data_length.max(0.0) as u64;
        let total = state.finished_bytes.fetch_add(length, Ordering::SeqCst) + length;
        if total > state.profile().max_bytes && state.set_cap(state.profile().byte_cap) {
            state.trigger_stop(&page).await;
        }
    }
}

async fn track_responses(
    mut responses: chromiumoxide::listeners::EventStream<EventResponseReceived>,
    state: Arc<NetworkState>,
) {
    while let Some(event) = responses.next().await {
        // Document responses include redirect hops and (rarely) subframes. The
        // last one is the best available final-document status; iframes are out
        // of B03 scope, so a subframe response is a disclosed approximation.
        if event.r#type != ResourceType::Document {
            continue;
        }
        state.touch();
        let status = event.response.status.clamp(0, u16::MAX as i64) as u16;
        if status >= 100 {
            state.main_status.store(status, Ordering::SeqCst);
        }
        *state
            .main_mime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(event.response.mime_type.clone());
    }
}

async fn track_activity(
    mut requests: chromiumoxide::listeners::EventStream<EventRequestWillBeSent>,
    state: Arc<NetworkState>,
) {
    while requests.next().await.is_some() {
        state.touch();
    }
}

async fn stop_tasks(
    intercept: JoinHandle<()>,
    bytes: JoinHandle<()>,
    finished: JoinHandle<()>,
    responses: JoinHandle<()>,
    activity: JoinHandle<()>,
) {
    intercept.abort();
    bytes.abort();
    finished.abort();
    responses.abort();
    activity.abort();
    let _ = intercept.await;
    let _ = bytes.await;
    let _ = finished.await;
    let _ = responses.await;
    let _ = activity.await;
}

// ---------------------------------------------------------------------------
// Held-page browser sessions (IMP-DEC-039/040)
// ---------------------------------------------------------------------------

/// Shared per-session request state. Every field is observed by the Fetch
/// handler and the network event tasks; the interaction layer only reads the
/// public accessors on [`BrowserHeldSession`].
#[derive(Debug)]
struct SessionNet {
    gate: SessionGate,
    blocked: StdMutex<Option<SessionBlocked>>,
    /// Request ids currently in flight (Network.requestWillBeSent .. finish).
    pending: StdMutex<std::collections::HashSet<String>>,
    /// Politeness leases held for in-flight document requests.
    leases: StdMutex<std::collections::HashMap<String, crate::fetch::gate::HostLease>>,
    status: AtomicU16,
    requests: AtomicU32,
    bytes: AtomicU64,
    stop_new: AtomicBool,
    /// Once a visual cap fires it bounds the rest of the session (sticky).
    cap: StdMutex<Option<&'static str>>,
    paused: AtomicU64,
    seen: AtomicU64,
    vet_observed: AtomicU64,
    child_targets: AtomicU64,
    /// Typed strings (last few), scrubbed from console output (RISK-045/F8).
    typed: StdMutex<std::collections::VecDeque<String>>,
    /// The first refusal of the top-level document (open fidelity); a blocked
    /// subresource must not fail an otherwise usable open.
    document_error: StdMutex<Option<WidowError>>,
    /// The top-level frame id: the first Document request claims it. Any other
    /// frame's document is a subframe (out of scope) and is refused.
    main_frame: StdMutex<Option<chromiumoxide::cdp::browser_protocol::page::FrameId>>,
    /// Per-session guard nonce; the probe function returns it and the isolated
    /// world heartbeat logs it (review G5/G6).
    guard_nonce: String,
    /// Secret printed only by the isolated integrity world.
    iw_nonce: String,
    guard_enabled: bool,
    last_cancel_seq: AtomicU64,
    iw_heartbeat: AtomicBool,
    /// Main-world (default) execution contexts. A heartbeat from one of these
    /// is page output, not the isolated world (review B2g/F4).
    default_contexts: StdMutex<std::collections::HashSet<i64>>,
    max_requests: u32,
    max_bytes: u64,
}

impl SessionNet {
    fn record_refusal(&self, error: WidowError) {
        self.record_block(error.code().as_str(), Some(error.to_string()));
    }

    fn record_document_refusal(&self, error: WidowError) {
        self.record_block(error.code().as_str(), Some(error.to_string()));
        let mut slot = self
            .document_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    fn remember_typed(&self, text: &str) {
        if text.chars().count() < 4 {
            return;
        }
        let mut typed = self
            .typed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if typed.len() >= MAX_TYPED_SECRETS {
            typed.pop_front();
        }
        typed.push_back(text.to_string());
    }

    fn scrub(&self, text: &str) -> String {
        let typed = self
            .typed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        scrub_secrets(text, typed.iter().map(String::as_str))
    }

    fn record_block(&self, reason: impl Into<String>, detail: Option<String>) {
        let mut slot = self
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(SessionBlocked {
                reason: reason.into(),
                detail,
            });
        }
    }

    /// An action-level refusal. Unlike a subresource refusal it always names
    /// itself: the prior reason is carried in the detail instead of masking the
    /// click's own refusal (review B2f/F6).
    fn record_action_block(&self, reason: impl Into<String>, detail: Option<String>) {
        let mut slot = self
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prior = slot.take().map(|blocked| blocked.reason);
        let detail = match (detail, prior) {
            (Some(detail), Some(prior)) => Some(format!("{detail}; prior={prior}")),
            (Some(detail), None) => Some(detail),
            (None, Some(prior)) => Some(format!("prior={prior}")),
            (None, None) => None,
        };
        *slot = Some(SessionBlocked {
            reason: reason.into(),
            detail: detail.map(|detail| crate::core::truncate::truncate_chars(&detail, 300).0),
        });
    }
}

/// A live held page. Dropping it without `close` aborts the event tasks; the
/// page and context are ended by the browser's own shutdown.
#[derive(Debug)]
struct BrowserHeldSession {
    page: Page,
    browser: Arc<Browser>,
    context_id: BrowserContextId,
    _permit: tokio::sync::OwnedSemaphorePermit,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    console: Arc<StdMutex<std::collections::VecDeque<ConsoleEntry>>>,
    net: Arc<SessionNet>,
    popup: Arc<AtomicBool>,
    settle_ms: u64,
    closed: AtomicBool,
}

impl BrowserRenderer {
    async fn open_session_impl(
        &self,
        target: &Url,
        options: SessionOptions,
    ) -> Result<BrowserHeldSession> {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| WidowError::RenderUnavailable("renderer is shutting down".to_string()))?;
        let (page, context_id, generation) = match self.new_page().await {
            Ok(value) => value,
            Err((error, Some(generation))) => {
                self.invalidate(generation, &error.to_string()).await?;
                self.new_page().await.map_err(|(error, _)| error)?
            }
            Err((error, None)) => return Err(error),
        };
        let built = self
            .build_session(&page, target, options, permit, context_id.clone())
            .await;
        match built {
            Ok(session) => Ok(session),
            Err(error) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), page.close()).await;
                self.dispose_context(generation, context_id).await;
                Err(error)
            }
        }
    }

    async fn build_session(
        &self,
        page: &Page,
        target: &Url,
        options: SessionOptions,
        permit: tokio::sync::OwnedSemaphorePermit,
        context_id: BrowserContextId,
    ) -> Result<BrowserHeldSession> {
        let Some(session) = self
            .session
            .lock()
            .await
            .as_ref()
            .map(|s| s.browser.clone())
        else {
            return Err(WidowError::RenderFailed(
                "the render browser disappeared while opening a session".to_string(),
            ));
        };

        page.execute(SetDeviceMetricsOverrideParams::new(
            i64::from(options.viewport_width),
            i64::from(options.viewport_height),
            1.0,
            false,
        ))
        .await
        .map_err(|error| WidowError::RenderFailed(format!("viewport override failed: {error}")))?;
        page.execute(PageEnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Page.enable failed: {error}")))?;
        // Popups are not followed: prevent their creation at the document level
        // (server-owned constant, not caller JS). A page/iframe target that is
        // still created is closed by the target handlers below.
        // Debug-only test hook for the G0 discriminator: the guard-disabled
        // probe must be able to measure the close-on-attach backstop alone.
        // Release builds always install the guard.
        #[cfg(debug_assertions)]
        let guard_disabled = std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_some();
        #[cfg(not(debug_assertions))]
        let guard_disabled = false;
        #[cfg(debug_assertions)]
        let integrity_disabled = std::env::var_os("WEB_WIDOW_TEST_DISABLE_INTEGRITY").is_some();
        #[cfg(not(debug_assertions))]
        let integrity_disabled = false;
        let guard_nonce = uuid::Uuid::now_v7().to_string();
        // The isolated world gets its own secret. The page can call
        // `__wwSessionGuard()` and read the main-world nonce, so a shared value
        // would let it forge the heartbeat (review B2g/F4).
        let iw_nonce = uuid::Uuid::now_v7().to_string();
        if !guard_disabled {
            let mut guard =
                AddScriptToEvaluateOnNewDocumentParams::new(popup_guard_js(&guard_nonce));
            guard.run_immediately = Some(true);
            page.execute(guard).await.map_err(|error| {
                WidowError::RenderFailed(format!("popup guard installation failed: {error}"))
            })?;
            if !integrity_disabled {
                let mut integrity =
                    AddScriptToEvaluateOnNewDocumentParams::new(integrity_guard_js(&iw_nonce));
                integrity.world_name = Some("web-widow-integrity".to_string());
                integrity.run_immediately = Some(true);
                page.execute(integrity).await.map_err(|error| {
                    WidowError::RenderFailed(format!(
                        "integrity guard installation failed: {error}"
                    ))
                })?;
            }
        }
        page.execute(DomEnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("DOM.enable failed: {error}")))?;
        page.execute(chromiumoxide::cdp::browser_protocol::network::EnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Network.enable failed: {error}")))?;
        page.execute(RuntimeEnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Runtime.enable failed: {error}")))?;
        page.execute(LogEnableParams::default())
            .await
            .map_err(|error| WidowError::RenderFailed(format!("Log.enable failed: {error}")))?;

        // Defense in depth next to the per-request classification below.
        let blocked_patterns = ["file://*", "ws://*", "wss://*"]
            .into_iter()
            .map(|pattern| BlockPattern::new(pattern, true))
            .collect::<Vec<_>>();
        page.execute(SetBlockedUrLsParams {
            url_patterns: Some(blocked_patterns),
        })
        .await
        .map_err(|error| {
            WidowError::RenderFailed(format!("Network.setBlockedURLs failed: {error}"))
        })?;

        let pattern = RequestPattern::builder()
            .url_pattern("*")
            .request_stage(RequestStage::Request)
            .build();
        page.execute(
            FetchEnableParams::builder()
                .patterns([pattern])
                .handle_auth_requests(false)
                .build(),
        )
        .await
        .map_err(|error| WidowError::RenderFailed(format!("Fetch.enable failed: {error}")))?;

        // Related page/iframe targets (popups, OOPIFs) do not inherit the page
        // Fetch domain: attach them paused and close them before they run
        // (IMP-RISK-044, review F1/F2). Dedicated workers are filtered out --
        // their requests are observed on the page target's Fetch domain.
        let target_filter = TargetFilter::new(vec![
            FilterEntry {
                exclude: Some(false),
                r#type: Some("page".to_string()),
            },
            FilterEntry {
                exclude: Some(false),
                r#type: Some("iframe".to_string()),
            },
            FilterEntry {
                exclude: Some(false),
                r#type: Some("service_worker".to_string()),
            },
            FilterEntry {
                exclude: Some(false),
                r#type: Some("shared_worker".to_string()),
            },
        ]);
        page.execute(
            SetAutoAttachParams::builder()
                .auto_attach(true)
                .wait_for_debugger_on_start(true)
                .flatten(true)
                .filter(target_filter)
                .build()
                .map_err(|error| {
                    WidowError::RenderFailed(format!("Target.setAutoAttach invalid: {error}"))
                })?,
        )
        .await
        .map_err(|error| {
            WidowError::RenderFailed(format!("Target.setAutoAttach failed: {error}"))
        })?;
        let attached_targets = page
            .event_listener::<EventAttachedToTarget>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("target listener failed: {error}"))
            })?;
        let browser_attached_targets = session
            .event_listener::<EventAttachedToTarget>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("browser target listener failed: {error}"))
            })?;
        let target_created = session
            .event_listener::<EventTargetCreated>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("target-created listener failed: {error}"))
            })?;
        session
            .execute(
                chromiumoxide::cdp::browser_protocol::target::SetDiscoverTargetsParams::new(true),
            )
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("Target.setDiscoverTargets failed: {error}"))
            })?;
        let main_target = Some(page.target_id().clone());

        // Listeners must exist before the navigation so the initial document is
        // observed (and gated) like every later request.
        let mut load_events = page
            .event_listener::<EventLoadEventFired>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("load listener failed: {error}")))?;
        let paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("fetch listener failed: {error}")))?;
        let console_events = page
            .event_listener::<EventConsoleApiCalled>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("console listener failed: {error}"))
            })?;
        let log_events = page
            .event_listener::<EventEntryAdded>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("log listener failed: {error}")))?;
        let exceptions = page
            .event_listener::<EventExceptionThrown>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("exception listener failed: {error}"))
            })?;
        let contexts_created = page
            .event_listener::<EventExecutionContextCreated>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("context listener failed: {error}"))
            })?;
        let contexts_cleared = page
            .event_listener::<EventExecutionContextsCleared>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("context-cleared listener failed: {error}"))
            })?;
        let requests = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("network listener failed: {error}"))
            })?;
        let finished = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("finish listener failed: {error}"))
            })?;
        let failed = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("failure listener failed: {error}"))
            })?;
        let responses = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("response listener failed: {error}"))
            })?;
        let window_events = page
            .event_listener::<EventWindowOpen>()
            .await
            .map_err(|error| WidowError::RenderFailed(format!("popup listener failed: {error}")))?;

        let net = Arc::new(SessionNet {
            gate: options.gate.clone(),
            blocked: StdMutex::new(None),
            pending: StdMutex::new(std::collections::HashSet::new()),
            leases: StdMutex::new(std::collections::HashMap::new()),
            status: AtomicU16::new(0),
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            stop_new: AtomicBool::new(false),
            cap: StdMutex::new(None),
            paused: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            vet_observed: AtomicU64::new(0),
            child_targets: AtomicU64::new(0),
            typed: StdMutex::new(std::collections::VecDeque::new()),
            document_error: StdMutex::new(None),
            main_frame: StdMutex::new(None),
            guard_nonce,
            iw_nonce,
            guard_enabled: !guard_disabled,
            last_cancel_seq: AtomicU64::new(0),
            iw_heartbeat: AtomicBool::new(false),
            default_contexts: StdMutex::new(std::collections::HashSet::new()),
            max_requests: self.config.visual_max_requests,
            max_bytes: self.config.visual_max_bytes,
        });
        let console = Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let popup = Arc::new(AtomicBool::new(false));

        let fetch_page = page.clone();
        let fetch_net = Arc::clone(&net);
        let console_page = page.clone();
        let console_ring = Arc::clone(&console);
        let log_ring = Arc::clone(&console);
        let popup_flag = Arc::clone(&popup);

        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(async move {
            session_fetch_loop(paused, fetch_page, fetch_net).await;
        }));
        let console_net = Arc::clone(&net);
        tasks.push(tokio::spawn(async move {
            track_session_console(console_events, console_page, console_ring, console_net).await;
        }));
        let context_net = Arc::clone(&net);
        tasks.push(tokio::spawn(async move {
            track_execution_contexts(contexts_created, contexts_cleared, context_net).await;
        }));
        tasks.push(tokio::spawn(async move {
            track_session_log(log_events, log_ring).await;
        }));
        tasks.push(tokio::spawn({
            let ring = Arc::clone(&console);
            async move { track_session_exceptions(exceptions, ring).await }
        }));
        tasks.push(tokio::spawn({
            let net = Arc::clone(&net);
            async move { track_session_requests(requests, net).await }
        }));
        tasks.push(tokio::spawn({
            let net = Arc::clone(&net);
            async move { track_session_finished(finished, net).await }
        }));
        tasks.push(tokio::spawn({
            let net = Arc::clone(&net);
            async move { track_session_failed(failed, net).await }
        }));
        tasks.push(tokio::spawn({
            let net = Arc::clone(&net);
            async move { track_session_responses(responses, net).await }
        }));
        tasks.push(tokio::spawn(async move {
            let mut stream = window_events;
            while stream.next().await.is_some() {
                popup_flag.store(true, Ordering::SeqCst);
            }
        }));
        let seen_targets = Arc::new(StdMutex::new(std::collections::HashSet::new()));
        tasks.push(tokio::spawn(track_session_targets(
            attached_targets,
            Arc::clone(&session),
            Arc::clone(&net),
            main_target.clone(),
            Arc::clone(&seen_targets),
            context_id.clone(),
        )));
        tasks.push(tokio::spawn(track_session_targets(
            browser_attached_targets,
            Arc::clone(&session),
            Arc::clone(&net),
            main_target.clone(),
            Arc::clone(&seen_targets),
            context_id.clone(),
        )));
        tasks.push(tokio::spawn(track_created_targets(
            target_created,
            Arc::clone(&session),
            Arc::clone(&net),
            main_target,
            Arc::clone(&seen_targets),
            context_id.clone(),
        )));

        let ceiling = Duration::from_millis(self.config.ceiling_ms.max(1));
        match tokio::time::timeout(ceiling, page.goto(target.clone())).await {
            Ok(Ok(_)) => {
                if let Some(blocked) = net
                    .document_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    return Err(blocked);
                }
            }
            Ok(Err(error)) => {
                if let Some(blocked) = net
                    .document_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    return Err(blocked);
                }
                return Err(WidowError::RenderFailed(format!(
                    "session navigation to {target} failed: {error}"
                )));
            }
            Err(_) => {
                if let Some(blocked) = net
                    .document_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    return Err(blocked);
                }
                return Err(WidowError::RenderFailed(format!(
                    "session navigation to {target} timed out"
                )));
            }
        }
        // A page with a paused OOPIF may never fire load; do not hold the open
        // for the full ceiling on it.
        let _ = tokio::time::timeout(Duration::from_secs(3), load_events.next()).await;
        let session = BrowserHeldSession {
            page: page.clone(),
            browser: session,
            context_id,
            _permit: permit,
            tasks: StdMutex::new(tasks),
            console,
            net,
            popup,
            settle_ms: options.settle_ms,
            closed: AtomicBool::new(false),
        };
        session.settle().await;
        session.guard_integrity().await?;
        // Give the response listener a moment to see the document status; a
        // missing status is reported as 200, never invented as success.
        for _ in 0..20 {
            if session.net.status.load(Ordering::SeqCst) != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(session)
    }
}

fn clean_target(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    crate::core::truncate::truncate_chars(&cleaned, 200).0
}

/// One node of the hit chain with attributes read from the browser process
/// (`DOM.getAttributes`), never through page JavaScript (review B2f/F1).
struct ChainNode {
    local_name: String,
    attributes: Vec<String>,
}

fn attribute_of(attributes: &[String], name: &str) -> Option<String> {
    attributes
        .chunks(2)
        .find(|pair| pair[0].eq_ignore_ascii_case(name))
        .map(|pair| pair[1].clone())
}

/// Whether a target name opens a new browsing context. Only the **exact empty
/// string** falls back to the document's base target; a whitespace-only name is
/// a named context and the UA creates one (g30 measures it). Keyword names are
/// ASCII case-insensitive (review B2i/F1). The guard applies the same rule, so
/// the two predicates cannot diverge.
fn opens_new_context(target: &str, base_target: &str) -> bool {
    // An empty **effective** target falls to the document's base target, then
    // to `_self`; the submitter/form precedence decides which target string is
    // effective, not whether the base applies (g36 measured that an empty
    // `formtarget` under `<base target>` creates a child context).
    let effective = if target.is_empty() {
        base_target
    } else {
        target
    };
    if effective.is_empty() {
        return false;
    }
    !effective.eq_ignore_ascii_case("_self")
        && !effective.eq_ignore_ascii_case("_top")
        && !effective.eq_ignore_ascii_case("_parent")
}

fn effective_target(target: &str, base_target: &str) -> String {
    if target.is_empty() {
        base_target.to_string()
    } else {
        target.to_string()
    }
}

/// A submit control's form owner, resolved in the control's own tree before the
/// pure predicate runs (review B2g/F1, B2i low).
#[derive(Clone)]
enum FormOwner {
    /// The control has a form owner; its content `target`, if any.
    Resolved(Option<String>),
    /// No form owner: no ancestor form in its own tree, or a `form` reference
    /// that does not resolve to a form. The control cannot submit.
    NoOwner,
}

/// The innermost tree (document or shadow root) containing `chain[index]`.
/// Resolved per node, so a chain that crosses a declarative boundary never uses
/// the hit node's tree for an outer-tree control (review B2h low #1).
fn tree_root_at(chain: &[DomNode], index: usize) -> Option<&DomNode> {
    chain[..=index]
        .iter()
        .rev()
        .find(|node| node.node_name == "#document" || node.shadow_root_type.is_some())
}

/// Like [`tree_root_at`], but returns the index so the ancestor-form search can
/// stop at the boundary.
fn tree_root_index(chain: &[DomNode], index: usize) -> Option<usize> {
    (0..=index).rev().find(|candidate| {
        let node = &chain[*candidate];
        node.node_name == "#document" || node.shadow_root_type.is_some()
    })
}

/// The nearest ancestor form **inside the node's own tree**; an outer-tree form
/// above the shadow boundary is not a form owner (review B2i low).
fn nearest_ancestor_form(chain: &[DomNode], index: usize) -> Option<&DomNode> {
    let root = tree_root_index(chain, index)?;
    chain[root..index]
        .iter()
        .rev()
        .find(|node| node.local_name.eq_ignore_ascii_case("form"))
}

/// The first element in tree order with `id`, inside one tree. Nested shadow
/// roots are different trees and are not descended.
fn first_element_with_id<'a>(root: &'a DomNode, id: &str) -> Option<&'a DomNode> {
    for child in root.children.iter().flatten() {
        if attribute_of(child.attributes.as_deref().unwrap_or(&[]), "id").as_deref() == Some(id) {
            return Some(child);
        }
        if let Some(found) = first_element_with_id(child, id) {
            return Some(found);
        }
    }
    None
}

/// Resolve `form="id"`: the first element in the tree with that id, and only if
/// it is a form. A duplicate id that names a non-form is no association (the
/// guard stops at the same element).
fn find_form_in_tree<'a>(root: &'a DomNode, id: &str) -> Option<&'a DomNode> {
    first_element_with_id(root, id).filter(|node| node.local_name.eq_ignore_ascii_case("form"))
}

/// Labelable elements (HTML spec). `input type=hidden` is not labelable.
fn is_labelable(node: &DomNode) -> bool {
    match node.local_name.to_ascii_lowercase().as_str() {
        "button" | "meter" | "output" | "progress" | "select" | "textarea" => true,
        "input" => !attribute_of(node.attributes.as_deref().unwrap_or(&[]), "type")
            .map(|kind| kind.eq_ignore_ascii_case("hidden"))
            .unwrap_or(false),
        _ => false,
    }
}

/// Interactive content (HTML spec). A label click that targets one of these, or
/// a descendant of one, does not run the label's activation behaviour, so the
/// labeled control must not be modelled (review B2i/F2).
fn is_interactive_content(node: &DomNode) -> bool {
    let attributes = node.attributes.as_deref().unwrap_or(&[]);
    match node.local_name.to_ascii_lowercase().as_str() {
        // The spec conditions the anchor on `href` (an href-less `<a>` is a
        // placeholder link, not interactive content); Blink's
        // `HTMLAnchorElement::IsInteractiveContent` is `IsLink()`. g33d
        // measured that the UA forwards the label activation for it.
        "a" => attribute_of(attributes, "href").is_some(),
        "button" | "details" | "embed" | "iframe" | "label" | "select" | "textarea" => true,
        // `input` is interactive content only outside the Hidden state (the
        // same condition as `is_labelable`); a `tabindex` element is focusable
        // but not interactive content, and listing it released the label's
        // control (g33 measured the escape).
        "input" => !attribute_of(attributes, "type")
            .map(|kind| kind.eq_ignore_ascii_case("hidden"))
            .unwrap_or(false),
        "audio" | "video" => attribute_of(attributes, "controls").is_some(),
        "img" => attribute_of(attributes, "usemap").is_some(),
        _ => false,
    }
}

/// The control a `<label>` activates: `for` names the first element with that id
/// in the label's own tree, if labelable; otherwise the first labelable
/// descendant. A label click on that control is what the press will activate
/// (review B2h high).
fn find_labeled_control<'a>(tree_root: &'a DomNode, label: &'a DomNode) -> Option<&'a DomNode> {
    let reference = attribute_of(label.attributes.as_deref().unwrap_or(&[]), "for")
        .filter(|value| !value.trim().is_empty());
    if let Some(reference) = reference {
        return first_element_with_id(tree_root, &reference).filter(|node| is_labelable(node));
    }
    fn first_labelable(node: &DomNode) -> Option<&DomNode> {
        for child in node.children.iter().flatten() {
            if is_labelable(child) {
                return Some(child);
            }
            if let Some(found) = first_labelable(child) {
                return Some(found);
            }
        }
        None
    }
    first_labelable(label)
}

/// Evaluate a node chain: an anchor/area (localName, content target) or a
/// submit control (formtarget / form-owner target) that opens a new context.
/// `base_target` is the document's first `<base target>`.
fn chain_new_context(
    chain: &[ChainNode],
    base_target: &str,
    owners: &[FormOwner],
) -> Option<(&'static str, String)> {
    for node in chain {
        let name = node.local_name.to_ascii_lowercase();
        if name == "a" || name == "area" {
            // An anchor/area without `href` is not a link: the UA navigates
            // nowhere (Blink `IsLink()`), so it must not be refused (review
            // B2l low).
            if attribute_of(&node.attributes, "href").is_none() {
                continue;
            }
            let target = attribute_of(&node.attributes, "target").unwrap_or_default();
            if opens_new_context(&target, base_target) {
                return Some((
                    "navigation_cancelled",
                    effective_target(&target, base_target),
                ));
            }
        }
    }
    for (index, node) in chain.iter().enumerate() {
        let name = node.local_name.to_ascii_lowercase();
        if name != "button" && name != "input" {
            continue;
        }
        let kind = attribute_of(&node.attributes, "type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_submit = if name == "button" {
            kind.is_empty() || kind == "submit"
        } else {
            kind == "submit" || kind == "image"
        };
        if !is_submit {
            continue;
        }
        let form_target = match owners.get(index) {
            Some(FormOwner::Resolved(target)) => target.clone().unwrap_or_default(),
            _ => continue,
        };
        // A **present** submitter `formtarget` takes precedence over the form
        // target (even when empty); an absent one falls back to it. An empty
        // effective target then falls to the base target (g35 measured the
        // precedence, g36 the base resolution).
        let target = match attribute_of(&node.attributes, "formtarget") {
            Some(value) => value,
            None => form_target,
        };
        if opens_new_context(&target, base_target) {
            return Some((
                "navigation_cancelled",
                effective_target(&target, base_target),
            ));
        }
    }
    None
}

fn find_node_chain(
    node: &DomNode,
    target: &BackendNodeId,
    chain: &mut Vec<DomNode>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    chain.push(node.clone());
    if &node.backend_node_id == target {
        return true;
    }
    for child in node.children.iter().flatten() {
        if find_node_chain(child, target, chain, budget) {
            return true;
        }
    }
    for shadow in node.shadow_roots.iter().flatten() {
        if find_node_chain(shadow, target, chain, budget) {
            return true;
        }
    }
    chain.pop();
    false
}

/// The hit node's own document: the innermost `#document` in the chain that
/// already runs from the root document down to the node, cross-checked against
/// the frame id the browser reported for the hit. A child-frame hit node
/// therefore gets its own frame's `<base target>` (review B2f/F1).
fn chain_document<'a>(chain: &'a [DomNode], frame: &FrameId) -> Option<&'a DomNode> {
    let document = chain
        .iter()
        .rev()
        .find(|node| node.node_name == "#document")?;
    // `DOM.getDocument` does not populate `frameId` on the document node in
    // every Chrome build, but it does on the frame's root element. The deepest
    // frame id in the chain is the hit node's frame.
    if let Some(observed) = chain.iter().rev().find_map(|node| node.frame_id.as_ref())
        && observed != frame
    {
        return None;
    }
    Some(document)
}

impl BrowserHeldSession {
    fn settle_ms(&self) -> Duration {
        Duration::from_millis(self.settle_ms.max(50))
    }

    /// Wait for network quiet after an action. Bounded so a page that keeps
    /// requesting cannot stall an action forever.
    async fn settle(&self) {
        let quiet = self.settle_ms();
        let deadline = tokio::time::Instant::now() + quiet + Duration::from_secs(3);
        let mut quiet_since = tokio::time::Instant::now();
        loop {
            let pending = self.pending_requests();
            if pending == 0 {
                if quiet_since.elapsed() >= quiet {
                    return;
                }
            } else {
                quiet_since = tokio::time::Instant::now();
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn element(
        &self,
        selector: &str,
        index: usize,
    ) -> Result<chromiumoxide::element::Element> {
        let elements =
            self.page
                .find_elements(selector)
                .await
                .map_err(|error| WidowError::ActionFailed {
                    reason: "selector_invalid".to_string(),
                    message: format!("selector {selector:?} could not be evaluated: {error}"),
                })?;
        let count = elements.len();
        elements
            .into_iter()
            .nth(index)
            .ok_or_else(|| WidowError::ActionFailed {
                reason: "selector_no_match".to_string(),
                message: format!(
                    "selector {selector:?} matched {count} element(s), index {index} requested"
                ),
            })
    }

    async fn guard_integrity(&self) -> Result<()> {
        if !self.net.guard_enabled {
            return Ok(());
        }
        for _ in 0..40 {
            if self.net.iw_heartbeat.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !self.net.iw_heartbeat.load(Ordering::SeqCst) {
            return Err(WidowError::RenderFailed(
                "interaction integrity guard heartbeat missing".to_string(),
            ));
        }
        let nonce = self.net.guard_nonce.clone();
        let expression = format!(
            "(() => {{ try {{ return typeof window.__wwSessionGuard === 'function' && window.__wwSessionGuard() === {nonce:?}; }} catch (_) {{ return false; }} }})()"
        );
        let ok = self
            .page
            .evaluate_expression(expression)
            .await
            .ok()
            .and_then(|result| result.into_value::<bool>().ok())
            .unwrap_or(false);
        if !ok {
            return Err(WidowError::RenderFailed(
                "interaction guard is missing or tampered".to_string(),
            ));
        }
        Ok(())
    }

    /// The first `<base>` element that has a `target` attribute, read from the
    /// browser process with `DOM.querySelector` + `DOM.getAttributes`. Page
    /// JavaScript can neither intercept nor fake this read (review B2f/F1).
    async fn base_target_for(&self, document: &DomNode) -> Result<Option<String>> {
        let selected = self
            .page
            .execute(QuerySelectorParams::new(
                document.node_id,
                "base[target]".to_string(),
            ))
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("DOM.querySelector(base[target]) failed: {error}"))
            })?;
        if *selected.result.node_id.inner() == 0 {
            return Ok(None);
        }
        let attributes = self
            .page
            .execute(GetAttributesParams::new(selected.result.node_id))
            .await
            .map_err(|error| {
                WidowError::RenderFailed(format!("DOM.getAttributes(base) failed: {error}"))
            })?;
        Ok(attribute_of(&attributes.result.attributes, "target"))
    }

    /// Server-side pre-dispatch check at one point: the deepest node there, its
    /// ancestor chain from the pierced DOM tree (shadow roots included), its
    /// own document's `<base target>`, and every attribute read through
    /// `DOM.getAttributes`. No page-realm expression is involved. An
    /// unresolvable tree, document or attribute read refuses the click with a
    /// typed reason instead of dispatching unchecked (review B2f/F1-F3).
    async fn pre_click_new_context(&self, point: &Point) -> Result<Option<(String, String)>> {
        if !self.net.guard_enabled {
            return Ok(None);
        }
        let refusal = |reason: &str, detail: String| Some((reason.to_string(), detail));
        // `ignore_pointer_events_none` makes the hit test see what the click
        // will hit: a `pointer-events:none` overlay is transparent to the
        // press, so it must be transparent to the check as well (review
        // B2g/F3).
        let hit_params = match GetNodeForLocationParams::builder()
            .x(point.x as i64)
            .y(point.y as i64)
            .ignore_pointer_events_none(true)
            .build()
        {
            Ok(params) => params,
            Err(error) => {
                return Ok(refusal(
                    "click_point_unavailable",
                    format!("the hit-test parameters were invalid: {error}"),
                ));
            }
        };
        let hit = match self.page.execute(hit_params).await {
            Ok(hit) => hit,
            Err(error) => {
                return Ok(refusal(
                    "click_point_unavailable",
                    format!("the click point could not be resolved: {error}"),
                ));
            }
        };
        let document = match tokio::time::timeout(
            Duration::from_secs(5),
            self.page
                .execute(GetDocumentParams::builder().depth(-1).pierce(true).build()),
        )
        .await
        {
            Ok(Ok(document)) => document,
            Ok(Err(error)) => {
                return Ok(refusal(
                    "walk_truncated",
                    format!("the DOM tree could not be read: {error}"),
                ));
            }
            Err(_) => {
                return Ok(refusal(
                    "walk_truncated",
                    "the DOM tree could not be read in time".to_string(),
                ));
            }
        };
        let mut chain = Vec::new();
        let mut budget = 50_000usize;
        if !find_node_chain(
            &document.result.root,
            &hit.result.backend_node_id,
            &mut chain,
            &mut budget,
        ) {
            return Ok(refusal(
                "walk_truncated",
                "the activation node was not found in the DOM tree".to_string(),
            ));
        }
        if let Some(found) = self.evaluate_chain(&chain, &hit.result.frame_id).await? {
            return Ok(Some(found));
        }
        // Label delegation: a hit `<label>` activates a control that may live
        // outside the hit chain — a sibling in a closed declarative shadow root,
        // where the guard's composed path is retargeted to the host and `submit`
        // is not composed. Resolve the labeled control and evaluate it with the
        // same rule (review B2h high).
        let Some(label_index) = chain
            .iter()
            .rposition(|node| node.local_name.eq_ignore_ascii_case("label"))
        else {
            return Ok(None);
        };
        // The label's activation behaviour does nothing when the event targets
        // interactive content inside the label (the UA runs that content
        // instead), so the labeled control must not be modelled for this click
        // (review B2i/F2).
        if chain[label_index + 1..].iter().any(is_interactive_content) {
            return Ok(None);
        }
        let Some(tree_root) = tree_root_at(&chain, label_index) else {
            return Ok(refusal(
                "walk_truncated",
                "the label's tree root was not found in the DOM tree".to_string(),
            ));
        };
        let Some(control) = find_labeled_control(tree_root, &chain[label_index]) else {
            return Ok(None);
        };
        let mut control_chain = Vec::new();
        let mut budget = 50_000usize;
        if !find_node_chain(
            &document.result.root,
            &control.backend_node_id,
            &mut control_chain,
            &mut budget,
        ) {
            return Ok(refusal(
                "walk_truncated",
                "the labeled control was not found in the DOM tree".to_string(),
            ));
        }
        self.evaluate_chain(&control_chain, &hit.result.frame_id)
            .await
    }

    /// Evaluate one node chain against the new-context rule. Everything is read
    /// from the browser process: the chain's document for `<base target>`, each
    /// element's attributes with `DOM.getAttributes`, and each submit control's
    /// form owner resolved in that control's own tree.
    async fn evaluate_chain(
        &self,
        chain: &[DomNode],
        frame: &FrameId,
    ) -> Result<Option<(String, String)>> {
        let refusal = |reason: &str, detail: String| Some((reason.to_string(), detail));
        let Some(document_node) = chain_document(chain, frame) else {
            return Ok(refusal(
                "walk_truncated",
                "the node's document was not found in the DOM tree".to_string(),
            ));
        };
        let base_target = match self.base_target_for(document_node).await {
            Ok(target) => target.unwrap_or_default(),
            Err(error) => {
                return Ok(refusal(
                    "walk_truncated",
                    format!("the base target could not be read: {error}"),
                ));
            }
        };
        let mut facts = Vec::with_capacity(chain.len());
        for node in chain {
            // Only element nodes carry attributes; a document node rejects
            // `DOM.getAttributes` and is never an activation candidate.
            let attributes = if node.node_type == ELEMENT_NODE {
                match self
                    .page
                    .execute(GetAttributesParams::new(node.node_id))
                    .await
                {
                    Ok(attributes) => attributes.result.attributes,
                    Err(error) => {
                        return Ok(refusal(
                            "walk_truncated",
                            format!("node attributes could not be read: {error}"),
                        ));
                    }
                }
            } else {
                Vec::new()
            };
            facts.push(ChainNode {
                local_name: node.local_name.clone(),
                attributes,
            });
        }
        // A submit control's form owner is not necessarily an ancestor: a
        // non-empty `form` attribute names a form in the control's own tree,
        // which for a shadow tree is a sibling invisible to the event path
        // (review B2g/F1, B2h low #1).
        let mut owners = Vec::with_capacity(facts.len());
        for (index, fact) in facts.iter().enumerate() {
            let name = fact.local_name.to_ascii_lowercase();
            if name != "button" && name != "input" {
                owners.push(FormOwner::NoOwner);
                continue;
            }
            let reference =
                attribute_of(&fact.attributes, "form").filter(|value| !value.is_empty());
            let Some(tree_root) = tree_root_at(chain, index) else {
                return Ok(refusal(
                    "walk_truncated",
                    "the control's tree root was not found in the DOM tree".to_string(),
                ));
            };
            // A non-empty `form` reference resolves by id in the control's own
            // tree; otherwise the nearest ancestor form in that same tree. An
            // outer-tree form above a shadow boundary is not an owner.
            let form = match reference {
                Some(reference) => find_form_in_tree(tree_root, &reference),
                None => nearest_ancestor_form(chain, index),
            };
            let Some(form) = form else {
                owners.push(FormOwner::NoOwner);
                continue;
            };
            let attributes = match self
                .page
                .execute(GetAttributesParams::new(form.node_id))
                .await
            {
                Ok(attributes) => attributes.result.attributes,
                Err(error) => {
                    return Ok(refusal(
                        "walk_truncated",
                        format!("the form owner's attributes could not be read: {error}"),
                    ));
                }
            };
            owners.push(FormOwner::Resolved(attribute_of(&attributes, "target")));
        }
        Ok(
            chain_new_context(&facts, &base_target, &owners).map(|(kind, target)| {
                (
                    kind.to_string(),
                    format!("target={}", clean_target(&target)),
                )
            }),
        )
    }

    /// Scroll the element with the browser's own `DOM.scrollIntoViewIfNeeded`.
    /// `Element::scroll_into_view` runs page JavaScript (`this.scrollIntoView`)
    /// and would give the page a callback between the check and the dispatch
    /// (review B2f/F2).
    async fn scroll_element_into_view(&self, backend: BackendNodeId) -> Result<()> {
        let params = ScrollIntoViewIfNeededParams::builder()
            .backend_node_id(backend)
            .build();
        self.page.execute(params).await.map_err(|error| {
            WidowError::RenderFailed(format!("scroll into view failed: {error}"))
        })?;
        Ok(())
    }

    /// The element's click point, computed once from the browser process's
    /// content quads. `None` means the element has no usable box.
    async fn element_point(&self, backend: BackendNodeId) -> Result<Option<Point>> {
        let params = GetContentQuadsParams::builder()
            .backend_node_id(backend)
            .build();
        let quads =
            self.page.execute(params).await.map_err(|error| {
                WidowError::RenderFailed(format!("content quads failed: {error}"))
            })?;
        let Some(quad) = quads.result.quads.first() else {
            return Ok(None);
        };
        let vertices = quad.inner();
        if vertices.len() < 8 {
            return Ok(None);
        }
        let x = (vertices[0] + vertices[2] + vertices[4] + vertices[6]) / 4.0;
        let y = (vertices[1] + vertices[3] + vertices[5] + vertices[7]) / 4.0;
        if !x.is_finite() || !y.is_finite() {
            return Ok(None);
        }
        Ok(Some(Point::new(x, y)))
    }

    async fn point_is_stable(&self, backend: BackendNodeId, point: &Point) -> Result<bool> {
        match self.element_point(backend).await? {
            Some(current) => Ok(current.x == point.x && current.y == point.y),
            None => Ok(false),
        }
    }

    /// A server-owned mouse event at the verified point. No page code decides
    /// the coordinates (review B2f/F2).
    async fn dispatch_mouse(&self, point: &Point, kind: DispatchMouseEventType) -> Result<()> {
        let hover = kind == DispatchMouseEventType::MouseMoved;
        let pressed = kind == DispatchMouseEventType::MousePressed;
        let params = DispatchMouseEventParams::builder()
            .r#type(kind)
            .x(point.x)
            .y(point.y)
            .button(if hover {
                MouseButton::None
            } else {
                MouseButton::Left
            })
            .buttons(if pressed { 1 } else { 0 })
            .click_count(if hover { 0 } else { 1 })
            .build()
            .map_err(|error| WidowError::internal(format!("dispatchMouseEvent params: {error}")))?;
        self.page
            .execute(params)
            .await
            .map_err(|error| WidowError::RenderFailed(format!("mouse dispatch failed: {error}")))?;
        Ok(())
    }

    async fn clear_focused_input(&self) -> Result<()> {
        let key_event =
            |kind: DispatchKeyEventType, key: &str, code: &str, vk: i64, modifiers: Option<i64>| {
                let mut builder = DispatchKeyEventParams::builder()
                    .r#type(kind)
                    .key(key.to_string())
                    .code(code.to_string())
                    .windows_virtual_key_code(vk)
                    .native_virtual_key_code(vk);
                if let Some(modifiers) = modifiers {
                    builder = builder.modifiers(modifiers);
                }
                builder.build()
            };
        // Select all (Ctrl+A) then Backspace: a server-owned keyboard sequence,
        // never caller JavaScript.
        for (kind, key, code, vk, modifiers) in [
            (DispatchKeyEventType::RawKeyDown, "a", "KeyA", 65, Some(2)),
            (DispatchKeyEventType::KeyUp, "a", "KeyA", 65, Some(2)),
            (
                DispatchKeyEventType::RawKeyDown,
                "Backspace",
                "Backspace",
                8,
                None,
            ),
            (
                DispatchKeyEventType::KeyUp,
                "Backspace",
                "Backspace",
                8,
                None,
            ),
        ] {
            let params = key_event(kind, key, code, vk, modifiers)
                .map_err(|error| WidowError::internal(format!("key event: {error}")))?;
            self.page
                .execute(params)
                .await
                .map_err(|error| WidowError::RenderFailed(format!("key event failed: {error}")))?;
        }
        Ok(())
    }
}

impl HeldSession for BrowserHeldSession {
    fn url<'a>(&'a self) -> HeldFuture<'a, Option<String>> {
        Box::pin(async move {
            self.page
                .url()
                .await
                .map_err(|error| WidowError::RenderFailed(format!("Page.url failed: {error}")))
        })
    }

    fn title<'a>(&'a self) -> HeldFuture<'a, Option<String>> {
        Box::pin(async move {
            self.page
                .get_title()
                .await
                .map_err(|error| WidowError::RenderFailed(format!("Page.getTitle failed: {error}")))
        })
    }

    fn status(&self) -> u16 {
        match self.net.status.load(Ordering::SeqCst) {
            0 => 200,
            status => status,
        }
    }

    fn click<'a>(&'a self, selector: &'a str, index: usize) -> HeldFuture<'a, ()> {
        Box::pin(async move {
            self.guard_integrity().await?;
            let element = self.element(selector, index).await?;
            // A same-tab navigation may follow the click; listen before it so
            // the load is not missed.
            let mut load_events = self
                .page
                .event_listener::<EventLoadEventFired>()
                .await
                .map_err(|error| {
                    WidowError::RenderFailed(format!("load listener failed: {error}"))
                })?;
            // Server-owned dispatch (review B2f/F2): the browser scrolls the
            // element, the point is computed from the browser's own content
            // quads, and the click is dispatched with Input.dispatchMouseEvent
            // at that exact point. `Element::click` is not used: it calls page
            // JavaScript (`scrollIntoView`) and recomputes the point after the
            // check.
            let backend = element.backend_node_id;
            self.scroll_element_into_view(backend).await?;
            let Some(point) = self.element_point(backend).await? else {
                self.net.record_action_block(
                    "click_point_unavailable",
                    Some("the element has no content box; the click is refused".to_string()),
                );
                return Ok(());
            };
            // The one page-visible step before the check: a hover at the point.
            // Page handlers run here, and the check reads the DOM afterwards, so
            // a hover-triggered layout change is caught before anything is
            // pressed (review F2).
            self.dispatch_mouse(&point, DispatchMouseEventType::MouseMoved)
                .await?;
            if !self.point_is_stable(backend, &point).await? {
                self.net.record_action_block(
                    "click_point_moved",
                    Some("the element moved after the hover; the click is refused".to_string()),
                );
                return Ok(());
            }
            if let Some((reason, detail)) = self.pre_click_new_context(&point).await? {
                self.net.record_action_block(reason, Some(detail));
                return Ok(());
            }
            // Re-read the box immediately before the press: a layout change
            // between the check and the dispatch refuses the click rather than
            // dispatching at a point that was never verified (review F2).
            if !self.point_is_stable(backend, &point).await? {
                self.net.record_action_block(
                    "click_point_moved",
                    Some("the element moved after the check; the click is refused".to_string()),
                );
                return Ok(());
            }
            self.dispatch_mouse(&point, DispatchMouseEventType::MousePressed)
                .await?;
            self.dispatch_mouse(&point, DispatchMouseEventType::MouseReleased)
                .await?;
            // A non-navigating click must not pay a navigation-sized wait: race
            // the load event against one settle window, then wait for network
            // quiet (which also covers a navigation that started late).
            let quiet = self.settle_ms();
            tokio::select! {
                _ = load_events.next() => {}
                () = tokio::time::sleep(quiet) => {}
            }
            drop(load_events);
            self.settle().await;
            // The document guard records a cancelled popup/navigation in the
            // page; read it with a server-owned expression and surface it on
            // the action (a silent cancellation would mislead the agent).
            let cancelled = self
                .page
                .evaluate_expression("window.__wwCancelled")
                .await
                .ok()
                .and_then(|result| result.into_value::<serde_json::Value>().ok())
                .filter(|value| !value.is_null());
            if let Some(value) = cancelled {
                let seq = value["seq"].as_u64().unwrap_or(0);
                if seq > self.net.last_cancel_seq.load(Ordering::SeqCst) {
                    self.net.last_cancel_seq.store(seq, Ordering::SeqCst);
                    let kind = match value["kind"].as_str().unwrap_or("") {
                        "popup_blocked" | "navigation_cancelled" | "worker_blocked" => {
                            value["kind"].as_str().unwrap_or("")
                        }
                        _ => "",
                    };
                    if !kind.is_empty() {
                        let target = clean_target(value["target"].as_str().unwrap_or(""));
                        self.net.record_action_block(
                            kind,
                            (!target.is_empty()).then(|| format!("target={target}")),
                        );
                    }
                }
            }
            Ok(())
        })
    }

    fn type_text<'a>(
        &'a self,
        selector: &'a str,
        index: usize,
        text: &'a str,
        clear: bool,
    ) -> HeldFuture<'a, usize> {
        Box::pin(async move {
            self.guard_integrity().await?;
            // Remember before the keystrokes so an input handler that logs the
            // value during typing is already scrubbable.
            self.net.remember_typed(text);
            let element = self.element(selector, index).await?;
            element
                .focus()
                .await
                .map_err(|error| WidowError::RenderFailed(format!("focus failed: {error}")))?;
            if clear {
                self.clear_focused_input().await?;
            }
            element
                .type_str(text)
                .await
                .map_err(|error| WidowError::RenderFailed(format!("type failed: {error}")))?;
            Ok(text.chars().count())
        })
    }

    fn has_selector<'a>(&'a self, selector: &'a str) -> HeldFuture<'a, bool> {
        Box::pin(async move {
            let elements = self.page.find_elements(selector).await.map_err(|error| {
                WidowError::ActionFailed {
                    reason: "selector_invalid".to_string(),
                    message: format!("selector {selector:?} could not be evaluated: {error}"),
                }
            })?;
            Ok(!elements.is_empty())
        })
    }

    fn scroll<'a>(&'a self, target: ScrollTarget<'a>) -> HeldFuture<'a, ()> {
        Box::pin(async move {
            self.guard_integrity().await?;
            match target {
                ScrollTarget::Element(selector) => {
                    let element = self.element(selector, 0).await?;
                    self.scroll_element_into_view(element.backend_node_id)
                        .await?;
                }
                // A server-owned constant expression; there is no caller JS.
                ScrollTarget::Top => {
                    self.page
                        .evaluate_expression("window.scrollTo(0, 0)")
                        .await
                        .map_err(|error| {
                            WidowError::RenderFailed(format!("scroll failed: {error}"))
                        })?;
                }
                ScrollTarget::Bottom => {
                    self.page
                        .evaluate_expression("window.scrollTo(0, document.body.scrollHeight)")
                        .await
                        .map_err(|error| {
                            WidowError::RenderFailed(format!("scroll failed: {error}"))
                        })?;
                }
            }
            self.settle().await;
            Ok(())
        })
    }

    fn screenshot<'a>(&'a self, full_page: bool) -> HeldFuture<'a, CapturedShot> {
        Box::pin(async move {
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .capture_beyond_viewport(full_page)
                .full_page(full_page)
                .build();
            let png =
                match tokio::time::timeout(Duration::from_secs(20), self.page.screenshot(params))
                    .await
                {
                    Ok(Ok(png)) => png,
                    Ok(Err(error)) => {
                        return Err(WidowError::RenderFailed(format!(
                            "Page.captureScreenshot failed: {error}"
                        )));
                    }
                    Err(_) => {
                        return Err(WidowError::RenderFailed(
                            "timed out capturing the session screenshot".to_string(),
                        ));
                    }
                };
            let (width, height, dimensions_source) = match tokio::time::timeout(
                Duration::from_secs(5),
                self.page.layout_metrics(),
            )
            .await
            {
                Ok(Ok(metrics)) if full_page => (
                    css_px(metrics.css_content_size.width),
                    css_px(metrics.css_content_size.height),
                    DimensionsSource::Content,
                ),
                Ok(Ok(metrics)) => (
                    css_px(metrics.css_layout_viewport.client_width as f64),
                    css_px(metrics.css_layout_viewport.client_height as f64),
                    DimensionsSource::Viewport,
                ),
                _ => (None, None, DimensionsSource::Unknown),
            };
            Ok(CapturedShot {
                png,
                width,
                height,
                dimensions_source,
            })
        })
    }

    fn console<'a>(&'a self, limit: usize) -> HeldFuture<'a, Vec<ConsoleEntry>> {
        Box::pin(async move {
            let entries: Vec<ConsoleEntry> = {
                let ring = self
                    .console
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let skip = ring.len().saturating_sub(limit);
                ring.iter().skip(skip).cloned().collect()
            };
            Ok(entries
                .into_iter()
                .map(|mut entry| {
                    entry.text = self.net.scrub(&entry.text);
                    entry
                })
                .collect())
        })
    }

    fn take_blocked_error<'a>(&'a self) -> HeldFuture<'a, Option<WidowError>> {
        Box::pin(async move {
            Ok(self
                .net
                .document_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take())
        })
    }

    fn take_blocked<'a>(&'a self) -> HeldFuture<'a, Option<SessionBlocked>> {
        Box::pin(async move {
            // A cap stays in effect for the rest of the session, while an
            // ordinary refusal is per-action. When both exist, the per-action
            // reason wins the field and the cap is carried in the detail, so a
            // cancelled navigation is not masked by the cap (review G7).
            let cap = *self
                .net
                .cap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let transient = self
                .net
                .blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            Ok(match (cap, transient) {
                (Some(cap), Some(blocked)) => Some(SessionBlocked {
                    reason: blocked.reason,
                    detail: Some(match blocked.detail {
                        Some(detail) => format!("{detail}; cap={cap}"),
                        None => format!("cap={cap}"),
                    }),
                }),
                (Some(cap), None) => Some(SessionBlocked {
                    reason: cap.to_string(),
                    detail: Some("session cap is still in effect".to_string()),
                }),
                (None, transient) => transient,
            })
        })
    }

    fn popup_opened(&self) -> bool {
        // Per action: only the action that observed the event reports it.
        self.popup.swap(false, Ordering::SeqCst)
    }

    fn pending_requests(&self) -> usize {
        self.net
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn counters(&self) -> SessionCounters {
        SessionCounters {
            vet_observed: self.net.vet_observed.load(Ordering::SeqCst),
            paused: self.net.paused.load(Ordering::SeqCst),
            child_targets: self.net.child_targets.load(Ordering::SeqCst),
        }
    }

    fn close<'a>(&'a self) -> HeldFuture<'a, ()> {
        Box::pin(async move {
            if self.closed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            {
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for task in tasks.drain(..) {
                    task.abort();
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), self.page.clone().close()).await;
            let _ = self
                .browser
                .dispose_browser_context(self.context_id.clone())
                .await;
            Ok(())
        })
    }
}

impl Drop for BrowserHeldSession {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

/// Close any related page/iframe target as soon as it is attached, so a popup
/// or cross-site iframe can never use an ungated network stack. The target is
/// paused (`waitForDebuggerOnStart`) and is closed instead of resumed.
async fn track_session_targets(
    mut events: chromiumoxide::listeners::EventStream<EventAttachedToTarget>,
    browser: Arc<Browser>,
    net: Arc<SessionNet>,
    main_target: Option<chromiumoxide::cdp::browser_protocol::target::TargetId>,
    seen: Arc<StdMutex<std::collections::HashSet<String>>>,
    context_id: BrowserContextId,
) {
    while let Some(event) = events.next().await {
        // The browser-level listeners see every target in the browser; a
        // session may only touch its own ephemeral context.
        if event.target_info.browser_context_id.as_ref() != Some(&context_id) {
            continue;
        }
        let target_id = event.target_info.target_id.clone();
        if main_target.as_ref() == Some(&target_id) {
            continue;
        }
        net.child_targets.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
            target_type = %event.target_info.r#type,
            waiting = event.waiting_for_debugger,
            url = %crate::pipeline::log_url(&event.target_info.url, true),
            "session attached a related target"
        );
        if !seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.as_ref().to_string())
        {
            continue;
        }
        let kind = event.target_info.r#type.clone();
        if kind == "page" {
            net.record_block(
                "child_target_closed",
                Some("a popup target was closed before it could run".to_string()),
            );
            let _ = browser.execute(CloseTargetParams::new(target_id)).await;
            continue;
        }
        // Iframe and service-worker targets stay paused (never resumed): their
        // script cannot run, and a target that was not paused is closed.
        if !event.waiting_for_debugger {
            let _ = browser.execute(CloseTargetParams::new(target_id)).await;
        }
    }
}

async fn track_created_targets(
    mut events: chromiumoxide::listeners::EventStream<EventTargetCreated>,
    browser: Arc<Browser>,
    net: Arc<SessionNet>,
    main_target: Option<chromiumoxide::cdp::browser_protocol::target::TargetId>,
    seen: Arc<StdMutex<std::collections::HashSet<String>>>,
    context_id: BrowserContextId,
) {
    while let Some(event) = events.next().await {
        // The browser-level listeners see every target in the browser; a
        // session may only touch its own ephemeral context.
        if event.target_info.browser_context_id.as_ref() != Some(&context_id) {
            continue;
        }
        let target_id = event.target_info.target_id.clone();
        if main_target.as_ref() == Some(&target_id) {
            continue;
        }
        let kind = event.target_info.r#type.clone();
        if kind != "page" {
            continue;
        }
        tracing::debug!(
            target_type = %kind,
            url = %crate::pipeline::log_url(&event.target_info.url, true),
            "session saw a new page target"
        );
        net.child_targets.fetch_add(1, Ordering::SeqCst);
        if !seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.as_ref().to_string())
        {
            continue;
        }
        if kind == "page" {
            net.record_block(
                "child_target_closed",
                Some("a popup target was closed before it could run".to_string()),
            );
        }
        let _ = browser.execute(CloseTargetParams::new(target_id)).await;
    }
}

/// Replace every remembered typed string (and its form-encoded spellings) in a
/// console line. Errors are reported in the session, so a page that logs its own
/// field value cannot echo a typed secret back to the agent (RISK-045/F8).
fn scrub_secrets<'a>(text: &str, secrets: impl Iterator<Item = &'a str>) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.chars().count() < 4 {
            continue;
        }
        let encoded: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
        let variants = [
            encoded.clone(),
            encoded.replace("%20", "+"),
            encoded.replace('+', "%20"),
        ];
        for needle in std::iter::once(secret.to_string()).chain(variants) {
            if !needle.is_empty() {
                out = out.replace(&needle, "<redacted>");
            }
        }
    }
    out
}

/// Fail closed on any response-stage pause: the session enables Request-stage
/// Fetch only, so this should be unreachable, and if it ever becomes reachable
/// the request must not be continued unvetted.
fn response_stage_is_refused(event: &EventRequestPaused) -> bool {
    event.response_status_code.is_some()
}

async fn session_fetch_loop(
    mut paused: chromiumoxide::listeners::EventStream<EventRequestPaused>,
    page: Page,
    net: Arc<SessionNet>,
) {
    while let Some(event) = paused.next().await {
        net.paused.fetch_add(1, Ordering::SeqCst);
        let seen = net.seen.fetch_add(1, Ordering::SeqCst) + 1;
        let fail = |reason: &str, detail: Option<String>| {
            net.record_block(reason, detail);
        };
        if response_stage_is_refused(&event) {
            fail(
                "blocked_response_stage",
                Some("a response-stage pause is refused, never continued unvetted".to_string()),
            );
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        if seen > MAX_SESSION_PAUSED_REQUESTS {
            fail(
                "request_cap",
                Some(format!(
                    "session paused-request cap ({MAX_SESSION_PAUSED_REQUESTS}) reached"
                )),
            );
            *net.cap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some("request_cap");
            net.stop_new.store(true, Ordering::SeqCst);
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        if net.stop_new.load(Ordering::SeqCst) {
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        let is_document = event.resource_type == ResourceType::Document;
        // Subframes are out of scope for an interaction session: their
        // document would run in a target the page Fetch domain does not
        // cover. The first Document is the top-level navigation; any other
        // frame id is a subframe and is refused before it loads.
        let is_main_document = if is_document {
            let frame = event.frame_id.clone();
            let is_subframe = {
                let mut main = net
                    .main_frame
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match main.as_ref() {
                    None => {
                        *main = Some(frame);
                        false
                    }
                    Some(expected) => expected != &frame,
                }
            };
            if is_subframe {
                fail(
                    "blocked_iframe",
                    Some("subframes are not rendered in an interaction session".to_string()),
                );
                let _ = page
                    .execute(FailRequestParams::new(
                        event.request_id.clone(),
                        ErrorReason::BlockedByClient,
                    ))
                    .await;
                continue;
            }
            true
        } else {
            false
        };
        let Ok(url) = Url::parse(&event.request.url) else {
            let error = WidowError::BlockedScheme {
                scheme: "unparsable".to_string(),
            };
            if is_main_document {
                net.record_document_refusal(error);
            } else {
                net.record_refusal(error);
            }
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        };
        match url.scheme() {
            "data" | "blob" => {
                // No network: nothing to vet.
                let _ = page
                    .execute(ContinueRequestParams::new(event.request_id.clone()))
                    .await;
                continue;
            }
            "http" | "https" => {}
            other => {
                let error = WidowError::BlockedScheme {
                    scheme: other.to_string(),
                };
                if is_document {
                    net.record_document_refusal(error);
                } else {
                    net.record_refusal(error);
                }
                let _ = page
                    .execute(FailRequestParams::new(
                        event.request_id.clone(),
                        ErrorReason::BlockedByClient,
                    ))
                    .await;
                continue;
            }
        }
        if event.resource_type == ResourceType::Media {
            // The visual profile keeps media blocked.
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        net.vet_observed.fetch_add(1, Ordering::SeqCst);
        if let Err(error) = net.gate.ssrf.vet(&url).await {
            if is_main_document {
                net.record_document_refusal(error);
            } else {
                net.record_refusal(error);
            }
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        // robots.txt governs page fetches, not assets; every request still
        // takes a per-host lease, so the session inherits the ≤1 req/s
        // politeness floor (review F5).
        let delay = if event.resource_type == ResourceType::Document {
            match tokio::time::timeout(Duration::from_secs(10), net.gate.hop_guard.check(&url))
                .await
            {
                Ok(Ok(delay)) => delay,
                Ok(Err(error)) => {
                    net.record_document_refusal(error);
                    let _ = page
                        .execute(FailRequestParams::new(
                            event.request_id.clone(),
                            ErrorReason::BlockedByClient,
                        ))
                        .await;
                    continue;
                }
                Err(_) => {
                    net.record_document_refusal(WidowError::Timeout(
                        "robots.txt check timed out for a document request".to_string(),
                    ));
                    let _ = page
                        .execute(FailRequestParams::new(
                            event.request_id.clone(),
                            ErrorReason::BlockedByClient,
                        ))
                        .await;
                    continue;
                }
            }
        } else {
            None
        };
        let host = url.host_str().unwrap_or_default().to_string();
        let lease = net.gate.host_gate.acquire(&host, delay).await;
        if let Some(network_id) = event.network_id.as_ref() {
            net.leases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(network_id.as_ref().to_string(), lease);
        }
        let count = net.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if count > net.max_requests {
            *net.cap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some("visual_max_requests");
            net.stop_new.store(true, Ordering::SeqCst);
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
            continue;
        }
        let _ = page
            .execute(ContinueRequestParams::new(event.request_id.clone()))
            .await;
    }
}

async fn track_session_console(
    mut events: chromiumoxide::listeners::EventStream<EventConsoleApiCalled>,
    _page: Page,
    ring: Arc<StdMutex<std::collections::VecDeque<ConsoleEntry>>>,
    net: Arc<SessionNet>,
) {
    // The heartbeat is the exact isolated-world secret, from a context that is
    // not the page's main world. Anything else is page output and stays visible
    // (a page can no longer hide its own lines behind the internal prefix).
    let heartbeat = format!("WWIW2:{}", net.iw_nonce);
    while let Some(event) = events.next().await {
        let entry = console_entry(&event);
        let from_main_world = net
            .default_contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(event.execution_context_id.inner());
        if heartbeat_accepted(entry.text == heartbeat, from_main_world) {
            net.iw_heartbeat.store(true, Ordering::SeqCst);
            continue;
        }
        push_console_entry(&ring, entry);
    }
}

/// Whether an execution context is the page's main world. `auxData.isDefault`
/// is authoritative when it is a boolean; a context with no usable flag is
/// classified by its name (every main world has an empty name, an isolated
/// world carries its world name). Unknown metadata therefore fails closed for
/// heartbeat acceptance (review B2h low #3).
fn is_default_context(name: &str, aux_data: Option<&serde_json::Value>) -> bool {
    match aux_data
        .and_then(|aux| aux.get("isDefault"))
        .and_then(|value| value.as_bool())
    {
        Some(is_default) => is_default,
        None => name.is_empty(),
    }
}

/// The heartbeat is consumed only when the isolated world's secret matches and
/// the event did not come from a known main world.
fn heartbeat_accepted(secret_matches: bool, context_is_known_default: bool) -> bool {
    secret_matches && !context_is_known_default
}

/// Track which execution contexts are the page's main worlds, so the
/// isolated-world heartbeat cannot be forged from the page (review B2g/F4).
async fn track_execution_contexts(
    mut created: chromiumoxide::listeners::EventStream<EventExecutionContextCreated>,
    mut cleared: chromiumoxide::listeners::EventStream<EventExecutionContextsCleared>,
    net: Arc<SessionNet>,
) {
    loop {
        tokio::select! {
            event = created.next() => {
                let Some(event) = event else { return };
                if is_default_context(&event.context.name, event.context.aux_data.as_ref()) {
                    net.default_contexts
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(*event.context.id.inner());
                }
            }
            event = cleared.next() => {
                let Some(_) = event else { return };
                net.default_contexts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
            }
        }
    }
}

async fn track_session_log(
    mut events: chromiumoxide::listeners::EventStream<EventEntryAdded>,
    ring: Arc<StdMutex<std::collections::VecDeque<ConsoleEntry>>>,
) {
    while let Some(event) = events.next().await {
        push_console_entry(&ring, log_entry(&event));
    }
}

async fn track_session_exceptions(
    mut events: chromiumoxide::listeners::EventStream<EventExceptionThrown>,
    ring: Arc<StdMutex<std::collections::VecDeque<ConsoleEntry>>>,
) {
    while let Some(event) = events.next().await {
        let details = &event.exception_details;
        let text = details
            .exception
            .as_ref()
            .map(remote_text)
            .unwrap_or_else(|| details.text.clone());
        push_console_entry(
            &ring,
            ConsoleEntry {
                level: "error".to_string(),
                text: bounded_console_text(&text),
                at: event_time(*event.timestamp.inner()),
                source: "log".to_string(),
            },
        );
    }
}

async fn track_session_requests(
    mut events: chromiumoxide::listeners::EventStream<EventRequestWillBeSent>,
    net: Arc<SessionNet>,
) {
    while let Some(event) = events.next().await {
        net.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(event.request_id.as_ref().to_string());
    }
}

async fn track_session_finished(
    mut events: chromiumoxide::listeners::EventStream<EventLoadingFinished>,
    net: Arc<SessionNet>,
) {
    while let Some(event) = events.next().await {
        let id = event.request_id.as_ref().to_string();
        net.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        net.leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        let length = event.encoded_data_length.max(0.0) as u64;
        let total = net.bytes.fetch_add(length, Ordering::SeqCst) + length;
        if total > net.max_bytes && !net.stop_new.swap(true, Ordering::SeqCst) {
            *net.cap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some("visual_max_bytes");
        }
    }
}

async fn track_session_failed(
    mut events: chromiumoxide::listeners::EventStream<EventLoadingFailed>,
    net: Arc<SessionNet>,
) {
    while let Some(event) = events.next().await {
        let id = event.request_id.as_ref().to_string();
        net.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        net.leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }
}

async fn track_session_responses(
    mut events: chromiumoxide::listeners::EventStream<EventResponseReceived>,
    net: Arc<SessionNet>,
) {
    while let Some(event) = events.next().await {
        if event.r#type != ResourceType::Document || event.response.status < 100 {
            continue;
        }
        net.status.store(
            event.response.status.clamp(0, u16::MAX as i64) as u16,
            Ordering::SeqCst,
        );
    }
}

fn push_console_entry(
    ring: &Arc<StdMutex<std::collections::VecDeque<ConsoleEntry>>>,
    entry: ConsoleEntry,
) {
    let mut ring = ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if ring.len() >= 200 {
        ring.pop_front();
    }
    ring.push_back(entry);
}

fn bounded_console_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    crate::core::truncate::truncate_chars(&cleaned, 500).0
}

fn console_level(
    kind: &chromiumoxide::cdp::js_protocol::runtime::ConsoleApiCalledType,
) -> &'static str {
    use chromiumoxide::cdp::js_protocol::runtime::ConsoleApiCalledType as Kind;
    match kind {
        Kind::Log => "log",
        Kind::Debug => "debug",
        Kind::Info => "info",
        Kind::Error => "error",
        Kind::Warning | Kind::Assert => "warn",
        _ => "log",
    }
}

fn console_entry(event: &EventConsoleApiCalled) -> ConsoleEntry {
    let text = event
        .args
        .iter()
        .map(remote_text)
        .collect::<Vec<_>>()
        .join(" ");
    ConsoleEntry {
        level: console_level(&event.r#type).to_string(),
        text: bounded_console_text(&text),
        at: event_time(*event.timestamp.inner()),
        source: "console".to_string(),
    }
}

fn log_entry(event: &EventEntryAdded) -> ConsoleEntry {
    let level = match event.entry.level {
        chromiumoxide::cdp::browser_protocol::log::LogEntryLevel::Verbose => "debug",
        chromiumoxide::cdp::browser_protocol::log::LogEntryLevel::Info => "info",
        chromiumoxide::cdp::browser_protocol::log::LogEntryLevel::Warning => "warn",
        chromiumoxide::cdp::browser_protocol::log::LogEntryLevel::Error => "error",
    };
    ConsoleEntry {
        level: level.to_string(),
        text: bounded_console_text(&event.entry.text),
        at: event_time(*event.entry.timestamp.inner()),
        source: "log".to_string(),
    }
}

/// Reduce a console argument to plain text: primitive value first, then the
/// object's description, then its bounded preview. Never expands live objects.
fn remote_text(arg: &RemoteObject) -> String {
    if let Some(value) = &arg.value {
        match value {
            serde_json::Value::String(text) => return text.clone(),
            serde_json::Value::Null => return "null".to_string(),
            primitive => return primitive.to_string(),
        }
    }
    if let Some(value) = &arg.unserializable_value {
        return value.inner().to_string();
    }
    if let Some(description) = &arg.description {
        return description.clone();
    }
    if let Some(preview) = &arg.preview {
        if let Some(description) = &preview.description {
            return description.clone();
        }
        let properties = preview
            .properties
            .iter()
            .take(8)
            .map(|property| match &property.value {
                Some(value) => format!("{}: {value}", property.name),
                None => property.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        return format!("{{{properties}}}");
    }
    format!("{:?}", arg.r#type).to_ascii_lowercase()
}

/// Console/log timestamps arrive as epoch milliseconds on the verified Chrome;
/// a seconds-scale value is accepted too rather than producing a year 58805.
fn event_time(value: f64) -> String {
    if value.is_finite() && value > 0.0 {
        let millis = if value > 1e11 {
            value as i64
        } else if value > 1e8 {
            (value * 1000.0) as i64
        } else {
            0
        };
        if millis > 0
            && let Some(time) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)
        {
            return time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        }
    }
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paused_event(status: Option<i64>) -> EventRequestPaused {
        let mut value = serde_json::json!({
            "requestId": "1",
            "request": {
                "url": "http://127.0.0.1/",
                "method": "GET",
                "headers": {},
                "initialPriority": "High",
                "referrerPolicy": "no-referrer"
            },
            "frameId": "FRAME",
            "resourceType": "Document"
        });
        if let Some(status) = status {
            value["responseStatusCode"] = serde_json::json!(status);
        }
        serde_json::from_value(value).expect("synthetic paused event")
    }

    #[test]
    fn a_response_stage_pause_is_refused_never_continued_unvetted() {
        assert!(
            response_stage_is_refused(&paused_event(Some(200))),
            "a response-stage event must be refused"
        );
        assert!(!response_stage_is_refused(&paused_event(None)));
    }

    #[test]
    fn typed_secrets_are_scrubbed_in_raw_and_encoded_forms() {
        let scrubbed = scrub_secrets("VALUE:secret-42", ["secret-42"].into_iter());
        assert!(!scrubbed.contains("secret-42"), "{scrubbed}");
        assert!(scrubbed.contains("<redacted>"), "{scrubbed}");
        let spaced = scrub_secrets("x a b c y", ["a b c"].into_iter());
        assert!(!spaced.contains("a b c"), "{spaced}");
        let encoded = scrub_secrets("x a%20b%20c y", ["a b c"].into_iter());
        assert!(!encoded.contains("a%20b%20c"), "{encoded}");
        let plus = scrub_secrets("x a+b+c y", ["a b c"].into_iter());
        assert!(!plus.contains("a+b+c"), "{plus}");
    }
}

/// Pure tests for the new-context model and the heartbeat classification. They
/// need no browser: the nodes are deserialized CDP `DOM.Node` values, so the
/// tree-shape rules (same tree, duplicate ids, nesting) are exercised directly
/// (review B2h low #1 and low #3).
#[cfg(test)]
mod b2h_tests {
    use super::*;
    use serde_json::json;

    fn document(node_id: i64, children: Vec<DomNode>) -> DomNode {
        serde_json::from_value(json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": 9,
            "nodeName": "#document",
            "localName": "",
            "nodeValue": "",
            "children": children,
        }))
        .expect("document node")
    }

    fn shadow_root(node_id: i64, children: Vec<DomNode>) -> DomNode {
        serde_json::from_value(json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": 11,
            "nodeName": "#document-fragment",
            "localName": "",
            "nodeValue": "",
            "shadowRootType": "open",
            "children": children,
        }))
        .expect("shadow root node")
    }

    fn element(node_id: i64, local_name: &str, attributes: &[(&str, &str)]) -> DomNode {
        let flat: Vec<String> = attributes
            .iter()
            .flat_map(|(name, value)| [name.to_string(), value.to_string()])
            .collect();
        let mut value = json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": 1,
            "nodeName": local_name.to_ascii_uppercase(),
            "localName": local_name,
            "nodeValue": "",
        });
        if !flat.is_empty() {
            value["attributes"] = json!(flat);
        }
        serde_json::from_value(value).expect("element node")
    }

    fn element_with_children(node_id: i64, local_name: &str, children: Vec<DomNode>) -> DomNode {
        let mut node = element(node_id, local_name, &[]);
        node.children = Some(children);
        node
    }

    fn element_with_shadow(node_id: i64, local_name: &str, shadow: DomNode) -> DomNode {
        let mut node = element(node_id, local_name, &[]);
        node.shadow_roots = Some(vec![shadow]);
        node
    }

    fn fact(local_name: &str, attributes: &[(&str, &str)]) -> ChainNode {
        ChainNode {
            local_name: local_name.to_string(),
            attributes: attributes
                .iter()
                .flat_map(|(name, value)| [name.to_string(), value.to_string()])
                .collect(),
        }
    }

    #[test]
    fn duplicate_id_that_is_not_a_form_is_no_association() {
        let root = document(
            1,
            vec![
                element(2, "div", &[("id", "f")]),
                element(3, "form", &[("id", "f"), ("target", "_blank")]),
            ],
        );
        assert!(
            find_form_in_tree(&root, "f").is_none(),
            "a later form with a duplicated id is not the association"
        );
    }

    #[test]
    fn nested_shadow_root_is_a_different_tree() {
        let inner = shadow_root(
            4,
            vec![element(5, "form", &[("id", "f"), ("target", "_blank")])],
        );
        let root = document(1, vec![element_with_shadow(2, "div", inner.clone())]);
        assert!(
            find_form_in_tree(&root, "f").is_none(),
            "a form inside a nested shadow root is not in the outer tree"
        );
        assert!(find_form_in_tree(&inner, "f").is_some());
    }

    #[test]
    fn tree_root_is_resolved_per_chain_node() {
        let chain = vec![
            document(1, vec![]),
            element(2, "div", &[]),
            shadow_root(3, vec![]),
            element(4, "label", &[]),
        ];
        assert_eq!(tree_root_at(&chain, 0).expect("doc").node_id.inner(), &1);
        assert_eq!(tree_root_at(&chain, 1).expect("doc").node_id.inner(), &1);
        assert_eq!(tree_root_at(&chain, 2).expect("shadow").node_id.inner(), &3);
        assert_eq!(tree_root_at(&chain, 3).expect("shadow").node_id.inner(), &3);
    }

    #[test]
    fn nearest_ancestor_form_stops_at_the_tree_boundary() {
        // document > form(_blank) > host > #shadow-root > form(_self) > input
        let chain = vec![
            document(1, vec![]),
            element(2, "form", &[("target", "_blank")]),
            element(3, "div", &[]),
            shadow_root(4, vec![]),
            element(5, "form", &[("target", "_self")]),
            element(6, "input", &[("type", "submit")]),
        ];
        let owner = nearest_ancestor_form(&chain, 5).expect("inner form");
        assert_eq!(owner.node_id.inner(), &5, "the nearest same-tree form wins");
        // An outer-tree form above the shadow boundary is not an owner.
        let chain = vec![
            document(1, vec![]),
            element(2, "form", &[("target", "_blank")]),
            element(3, "div", &[]),
            shadow_root(4, vec![]),
            element(5, "input", &[("type", "submit")]),
        ];
        assert!(nearest_ancestor_form(&chain, 4).is_none());
    }

    #[test]
    fn no_form_owner_skips_the_control() {
        let chain = vec![
            fact("form", &[("target", "_blank")]),
            fact("input", &[("type", "submit"), ("form", "nope")]),
        ];
        let owners = vec![FormOwner::NoOwner, FormOwner::NoOwner];
        assert!(
            chain_new_context(&chain, "", &owners).is_none(),
            "a control with no form owner cannot submit"
        );
    }

    #[test]
    fn empty_formtarget_precedence_and_base_resolution() {
        // g35 measured: a present-but-empty formtarget takes precedence over a
        // `_blank` form target, and with no base that is same-tab.
        let chain = vec![
            fact("form", &[("target", "_blank")]),
            fact("button", &[("formtarget", "")]),
        ];
        let owner = FormOwner::Resolved(Some("_blank".to_string()));
        assert!(
            chain_new_context(&chain, "", &[owner.clone(), owner.clone()]).is_none(),
            "an empty formtarget overrides the form target with _self"
        );
        // g36 measured: an empty effective target under `<base target=_blank>`
        // creates a child context, so the base must be applied after the
        // precedence step.
        assert_eq!(
            chain_new_context(&chain, "_blank", &[owner.clone(), owner]),
            Some(("navigation_cancelled", "_blank".to_string()))
        );
    }

    #[test]
    fn href_less_anchors_and_areas_are_not_navigators() {
        let chain = vec![fact("a", &[("target", "_blank")])];
        assert!(chain_new_context(&chain, "", &[]).is_none());
        let chain = vec![fact("area", &[("target", "_blank")])];
        assert!(chain_new_context(&chain, "", &[]).is_none());
        let chain = vec![fact("a", &[("target", "_blank"), ("href", "/next")])];
        assert_eq!(
            chain_new_context(&chain, "", &[]),
            Some(("navigation_cancelled", "_blank".to_string()))
        );
    }

    #[test]
    fn submitter_formtarget_self_overrides_a_blank_form() {
        let chain = vec![
            fact("form", &[("target", "_blank")]),
            fact("button", &[("formtarget", "_self")]),
        ];
        let owners = vec![
            FormOwner::Resolved(Some("_blank".to_string())),
            FormOwner::Resolved(None),
        ];
        assert!(chain_new_context(&chain, "", &owners).is_none());
    }

    #[test]
    fn target_normalisation_matches_the_measured_ua_behaviour() {
        // g30 measured that a whitespace-only name is a named browsing context.
        assert!(opens_new_context(" ", ""));
        assert!(opens_new_context(" ", "_self"));
        assert!(opens_new_context("named", ""));
        assert!(opens_new_context("", "_blank"));
        assert!(!opens_new_context("", ""));
        assert!(!opens_new_context("_self", "_blank"));
        assert!(!opens_new_context("_SELF", "_blank"));
        assert!(!opens_new_context("_Top", "_blank"));
        assert!(!opens_new_context("_PARENT", "_blank"));
        // `_blank ` (trailing space) is a named context, not the keyword.
        assert!(opens_new_context("_blank ", "_self"));
        assert!(is_interactive_content(&element(
            1,
            "a",
            &[("href", "/next.html")]
        )));
        // A `tabindex` element is focusable, not interactive content: the UA
        // still runs the label activation for it (g33 measured the escape when
        // this member was listed).
        assert!(!is_interactive_content(&element(
            2,
            "div",
            &[("tabindex", "0")]
        )));
        assert!(is_interactive_content(&element(
            3,
            "input",
            &[("type", "submit")]
        )));
        assert!(!is_interactive_content(&element(
            4,
            "input",
            &[("type", "hidden")]
        )));
        assert!(!is_interactive_content(&element(
            5,
            "input",
            &[("type", "HIDDEN")]
        )));
    }

    /// The membership is exactly the spec's interactive-content enumeration.
    /// Mapping (HTML "interactive content"): `a[href]`, `audio[controls]`,
    /// `button`, `details`, `embed`, `iframe`, `img[usemap]`,
    /// `input:not([type=hidden])`, `label`, `select`, `textarea`,
    /// `video[controls]`. The same table is in the n2 residual addendum.
    #[test]
    fn membership_is_exactly_the_spec_interactive_content_set() {
        // Members.
        assert!(is_interactive_content(&element(1, "a", &[("href", "/x")])));
        assert!(is_interactive_content(&element(2, "a", &[("href", "")])));
        assert!(is_interactive_content(&element(
            3,
            "audio",
            &[("controls", "")]
        )));
        assert!(is_interactive_content(&element(4, "button", &[])));
        assert!(is_interactive_content(&element(5, "details", &[])));
        assert!(is_interactive_content(&element(6, "embed", &[])));
        assert!(is_interactive_content(&element(7, "iframe", &[])));
        assert!(is_interactive_content(&element(
            8,
            "img",
            &[("usemap", "#m")]
        )));
        assert!(is_interactive_content(&element(
            9,
            "input",
            &[("type", "text")]
        )));
        assert!(is_interactive_content(&element(10, "label", &[])));
        assert!(is_interactive_content(&element(11, "select", &[])));
        assert!(is_interactive_content(&element(12, "textarea", &[])));
        assert!(is_interactive_content(&element(
            13,
            "video",
            &[("controls", "")]
        )));
        // Non-members: the conditions the spec puts on its members, and
        // elements the category does not contain (including `tabindex`,
        // `area`, `object`, `summary` and `contenteditable`).
        assert!(!is_interactive_content(&element(14, "a", &[])));
        assert!(!is_interactive_content(&element(15, "audio", &[])));
        assert!(!is_interactive_content(&element(16, "img", &[])));
        assert!(!is_interactive_content(&element(
            17,
            "input",
            &[("type", "hidden")]
        )));
        assert!(!is_interactive_content(&element(18, "video", &[])));
        assert!(!is_interactive_content(&element(
            19,
            "span",
            &[("tabindex", "0")]
        )));
        assert!(!is_interactive_content(&element(
            20,
            "area",
            &[("href", "/x")]
        )));
        assert!(!is_interactive_content(&element(21, "object", &[])));
        assert!(!is_interactive_content(&element(22, "summary", &[])));
        assert!(!is_interactive_content(&element(
            23,
            "div",
            &[("contenteditable", "")]
        )));
    }

    #[test]
    fn label_resolution_uses_for_then_descendants() {
        let root = shadow_root(
            10,
            vec![
                element(11, "form", &[("id", "f"), ("target", "_blank")]),
                element(12, "label", &[("for", "s")]),
                element(
                    13,
                    "input",
                    &[("id", "s"), ("type", "submit"), ("form", "f")],
                ),
            ],
        );
        let label = root.children.as_ref().expect("children")[1].clone();
        let control = find_labeled_control(&root, &label).expect("for control");
        assert_eq!(
            control.node_id.inner(),
            &13,
            "for= names the sibling control"
        );

        let non_labelable = shadow_root(20, vec![element(21, "div", &[("id", "s")])]);
        let label = element(22, "label", &[("for", "s")]);
        assert!(
            find_labeled_control(&non_labelable, &label).is_none(),
            "for= pointing at a non-labelable element is no control"
        );

        let wrapping = element_with_children(
            30,
            "label",
            vec![element(31, "span", &[]), element(32, "button", &[])],
        );
        let control = find_labeled_control(&non_labelable, &wrapping).expect("descendant");
        assert_eq!(control.node_id.inner(), &32, "first labelable descendant");
    }

    #[test]
    fn heartbeat_context_classification() {
        assert!(is_default_context("", None), "no metadata, empty name");
        assert!(
            !is_default_context("web-widow-integrity", None),
            "an isolated world carries its name"
        );
        assert!(
            is_default_context("", Some(&json!({"isDefault": "true"}))),
            "a non-boolean flag falls back to the name"
        );
        assert!(!is_default_context(
            "web-widow-integrity",
            Some(&json!({"isDefault": "true"}))
        ));
        assert!(!is_default_context("", Some(&json!({"isDefault": false}))));
        assert!(is_default_context("x", Some(&json!({"isDefault": true}))));
        // An unknown context id: only the isolated world's secret decides.
        assert!(heartbeat_accepted(true, false));
        assert!(!heartbeat_accepted(true, true));
        assert!(!heartbeat_accepted(false, false));
    }
}
