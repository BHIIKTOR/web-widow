//! Change monitoring: schedule, snapshots, diffs and the `monitor_*` tools
//!
//! One check is one URL. It shares the pipeline's conditional-acquisition path
//! (so the SSRF gate, robots policy, caps and the 304 short-circuit are the same
//! code as `fetch_page`/`extract`) but it is deliberately **not** a crawl page:
//! crawls fetch unconditionally by design, and a monitor must keep conditional
//! GET. A check produces exactly one terminal result — a snapshot on success, an
//! advanced schedule and a recorded fetch error on failure — with no retries.
//!
//! No scheduler lives in this process: the operator drives time with
//! `web-widow run-due`; `monitor_check` is on demand.

pub mod normalize;
pub mod scheduler;

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use scraper::Selector;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::cursor;
use crate::core::error::{Result, WidowError};
use crate::core::url::{PageUrl, content_hash};
use crate::extract::Mode;
use crate::extract::spec::{self, CompiledSpec, ValidatedSpec};
use crate::pipeline::{MonitorSpec, Pipeline};
use crate::store::Store;
use crate::store::db::{BlobRow, ProfileRow};
use crate::store::jobs::JobCounters;
use crate::store::jobs::{JOB_CANCELLED, JOB_DONE, JOB_FAILED, NewJob};
use crate::store::monitor::{CheckRecord, MonitorRow, NewChangeDraft, NewMonitor, NewSnapshot};
use normalize::{ChangeKind, ChangeStats};

const KIND_MONITOR_CHECK: &str = "monitor_check";
const MONITOR_LIST_DEFAULT: usize = 100;
const MONITOR_LIST_MAX: usize = 500;

// ---------------------------------------------------------------------------
// Tool inputs and payloads (§7–§10)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorCreateInput {
    /// Absolute http(s) URL to watch. Stored canonicalized (tracking parameters
    /// stripped, query pairs sorted), the rule.
    pub url: String,
    /// `main` (default), `full` or `text`; `raw` is rejected (a snapshot is
    /// content, not untrusted HTML).
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Optional extraction schema (the same shape as `extract`): field-level
    /// comparison is performed in addition to the text diff.
    #[serde(default)]
    pub schema: Option<Value>,
    /// Check interval in seconds; bounded by `[monitor]`.
    #[serde(default)]
    pub interval_s: Option<u64>,
    /// CSS subtrees removed from the DOM before Markdown conversion.
    #[serde(default)]
    pub ignore_selectors: Vec<String>,
    /// Optional human-facing label (documented convention `project:slug`).
    /// Trimmed; 1..=64 characters; control characters rejected. Labels are not
    /// unique and not pattern-enforced.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorCreatePayload {
    pub monitor_id: i64,
    pub next_due_at: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorListInput {
    /// Include tombstones (`monitor_delete` sets `active = 0`). Default false.
    #[serde(default)]
    pub include_inactive: Option<bool>,
    /// Page size, bounded 1..=500 (default 100). `truncated`/`limit_hit` report
    /// when the page is smaller than the matching total.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorListItem {
    pub monitor_id: i64,
    pub url: String,
    /// `null` for unlabeled monitors (including every pre-v5 row).
    pub label: Option<String>,
    pub mode: String,
    pub active: bool,
    pub has_schema: bool,
    pub interval_s: u64,
    pub created_at: String,
    pub last_checked_at: Option<String>,
    pub next_due_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorListPayload {
    pub monitors: Vec<MonitorListItem>,
    pub count: usize,
    /// Matching rows before `limit`.
    pub total: usize,
    pub truncated: bool,
    pub limit_hit: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorCheckInput {
    /// Check this monitor now.
    #[serde(default)]
    pub monitor_id: Option<i64>,
    /// Check every active monitor (respecting `next_due_at` unless `force`).
    #[serde(default)]
    pub all: Option<bool>,
    /// Ignore `next_due_at` and check even when not due.
    #[serde(default)]
    pub force: Option<bool>,
    /// Bound the inline wait for `all: true`, in milliseconds (default 0 =
    /// enqueue and return at once). Rejected with `monitor_id` and above
    /// `[monitor] max_wait_ms`.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorChangeSummary {
    pub monitor_id: i64,
    pub change_id: i64,
    pub kind: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorCheckFailure {
    pub monitor_id: i64,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorCheckPayload {
    pub checked: usize,
    /// True exactly when `job_id` is present: work is still queued in the
    /// background. False on a completed synchronous/inline check.
    pub pending: bool,
    pub changed: Vec<MonitorChangeSummary>,
    /// Checks that ended in a terminal error (one result, no retries). Additive
    /// to the contract: `checked: 1, changed: []` must not be read as "unchanged"
    /// when the origin returned 404.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<MonitorCheckFailure>,
    /// Present for `all: true`: the `monitor_check` job (poll with `crawl_status`;
    /// read the results through `monitor_changes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorChangesInput {
    pub monitor_id: i64,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Opaque cursor from a previous page (newest-first).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include the capped unified diff (default true).
    #[serde(default)]
    pub include_diff: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorChangeItem {
    pub change_id: i64,
    /// `taken_at` of the snapshot the change is measured from.
    pub from_snapshot: String,
    /// `taken_at` of the snapshot that detected the change.
    pub to_snapshot: String,
    pub detected_at: String,
    pub kind: String,
    pub stats: ChangeStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The diff was capped; `stats` still carries the full counts.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorChangesPayload {
    pub monitor_id: i64,
    pub changes: Vec<MonitorChangeItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorDeleteInput {
    pub monitor_id: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorDeletePayload {
    pub monitor_id: i64,
    /// Always false: deletion is a tombstone, history stays queryable.
    pub active: bool,
}

/// `web-widow run-due` output (stdout JSON).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MonitorDueSummary {
    pub checked: usize,
    /// Active monitors not due in this pass (including any beyond `limit`).
    pub skipped: usize,
    pub changed: Vec<MonitorChangeSummary>,
    pub failed: Vec<MonitorCheckFailure>,
}

// ---------------------------------------------------------------------------
// Internal outcomes
// ---------------------------------------------------------------------------

enum CheckOutcome {
    /// Deleted or not due; no request was made.
    Skipped,
    /// A snapshot was recorded and the content is unchanged.
    Unchanged,
    Changed(MonitorChangeSummary),
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MonitorManager {
    pipeline: Arc<Pipeline>,
    store: Arc<Store>,
    config: Arc<Config>,
    /// One async mutex per monitor: two checks of the same monitor cannot
    /// interleave their snapshot/change transactions.
    check_locks: StdMutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    running: StdMutex<HashMap<String, CancellationToken>>,
    shutdown: CancellationToken,
}

impl MonitorManager {
    pub fn new(config: Arc<Config>, pipeline: Arc<Pipeline>, store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            pipeline,
            store,
            config,
            check_locks: StdMutex::new(HashMap::new()),
            running: StdMutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        })
    }

    // -- lifecycle ---------------------------------------------------------

    pub async fn create(&self, input: MonitorCreateInput) -> Result<MonitorCreatePayload> {
        let page = PageUrl::parse(&input.url)?;
        let mode = input.mode.unwrap_or(Mode::Main);
        if mode == Mode::Raw {
            return Err(WidowError::SchemaInvalid {
                pointer: "/mode".to_string(),
                message: "a monitor stores extracted content; `raw` (untrusted HTML) is not a snapshot mode"
                    .to_string(),
            });
        }
        let interval_s = input
            .interval_s
            .unwrap_or(self.config.monitor.default_interval_s);
        if interval_s < self.config.monitor.min_interval_s
            || interval_s > self.config.monitor.max_interval_s
        {
            return Err(WidowError::SchemaInvalid {
                pointer: "/interval_s".to_string(),
                message: format!(
                    "interval_s must be within {}..={} seconds",
                    self.config.monitor.min_interval_s, self.config.monitor.max_interval_s
                ),
            });
        }
        if input.ignore_selectors.len() > self.config.monitor.max_ignore_selectors {
            return Err(WidowError::SchemaInvalid {
                pointer: "/ignore_selectors".to_string(),
                message: format!(
                    "at most {} ignore selectors are accepted",
                    self.config.monitor.max_ignore_selectors
                ),
            });
        }
        for (index, text) in input.ignore_selectors.iter().enumerate() {
            Selector::parse(text).map_err(|_| WidowError::SchemaInvalid {
                pointer: format!("/ignore_selectors/{index}"),
                message: format!("invalid CSS selector `{text}`"),
            })?;
        }
        let label = match input.label.as_deref() {
            Some(raw) => {
                let trimmed = raw.trim();
                if !(1..=64).contains(&trimmed.chars().count()) {
                    return Err(WidowError::SchemaInvalid {
                        pointer: "/label".to_string(),
                        message: "label must be 1..=64 characters after trimming".to_string(),
                    });
                }
                if trimmed.chars().any(char::is_control) {
                    return Err(WidowError::SchemaInvalid {
                        pointer: "/label".to_string(),
                        message: "label must not contain control characters".to_string(),
                    });
                }
                Some(trimmed.to_string())
            }
            None => None,
        };

        let now = Utc::now();
        let (profile_hash, profile) = match input.schema.as_ref() {
            Some(raw) => {
                let validated = spec::validate(raw)?;
                spec::compile(&validated.spec)?;
                let profile = ProfileRow {
                    hash: validated.hash.clone(),
                    name: None,
                    spec_json: validated.canonical_json,
                    created_at: now,
                };
                (Some(validated.hash), Some(profile))
            }
            None => (None, None),
        };
        if let Some(profile) = profile {
            self.store.db.insert_profile(profile).await?;
        }

        let ignore_selectors_json =
            serde_json::to_string(&input.ignore_selectors).map_err(WidowError::internal)?;
        let monitor_id = self
            .store
            .db
            .create_monitor(NewMonitor {
                url: page.canonical().to_string(),
                mode: mode.as_str().to_string(),
                profile_hash,
                interval_s,
                ignore_selectors_json,
                created_at: now,
                // Due immediately: the first check establishes the baseline.
                next_due_at: now,
                label: label.clone(),
            })
            .await?;
        tracing::info!(monitor_id, url = %page.canonical(), mode = mode.as_str(), label = label.as_deref().unwrap_or(""), "monitor created");
        Ok(MonitorCreatePayload {
            monitor_id,
            next_due_at: format_time(now),
        })
    }

    /// Mark jobs left running by a crashed process as failed. Checks are
    /// idempotent and monitors stay due, so `run-due` picks them up; nothing is
    /// silently replayed.
    pub async fn resume(&self) -> Result<usize> {
        let jobs = self
            .store
            .db
            .jobs_unfinished_kind(KIND_MONITOR_CHECK)
            .await?;
        for job in &jobs {
            self.store
                .db
                .finish_job(
                    &job.id,
                    JOB_FAILED,
                    job.counters.clone(),
                    Some(
                        "monitor check job was interrupted by a restart; monitors remain due and run-due will re-check them"
                            .to_string(),
                    ),
                )
                .await?;
        }
        Ok(jobs.len())
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        let running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for token in running.values() {
            token.cancel();
        }
    }

    // -- check -------------------------------------------------------------

    pub async fn check(self: &Arc<Self>, input: MonitorCheckInput) -> Result<MonitorCheckPayload> {
        let force = input.force.unwrap_or(false);
        if input.monitor_id.is_some() && input.wait_ms.is_some() {
            return Err(WidowError::SchemaInvalid {
                pointer: "/wait_ms".to_string(),
                message: "wait_ms is only valid with all: true; a single monitor check is already synchronous"
                    .to_string(),
            });
        }
        if let Some(wait_ms) = input.wait_ms
            && wait_ms > self.config.monitor.max_wait_ms
        {
            return Err(WidowError::SchemaInvalid {
                pointer: "/wait_ms".to_string(),
                message: format!(
                    "wait_ms must be at most {} milliseconds",
                    self.config.monitor.max_wait_ms
                ),
            });
        }
        match (input.monitor_id, input.all.unwrap_or(false)) {
            (Some(monitor_id), false) => self.check_one_payload(monitor_id, force).await,
            (None, true) => match input.wait_ms.unwrap_or(0) {
                0 => self.enqueue_check_all(force).await,
                wait_ms => self.check_all_with_wait(force, wait_ms).await,
            },
            _ => Err(WidowError::SchemaInvalid {
                pointer: "/monitor_id".to_string(),
                message: "supply exactly one of `monitor_id` or `all: true`".to_string(),
            }),
        }
    }

    async fn check_one_payload(&self, monitor_id: i64, force: bool) -> Result<MonitorCheckPayload> {
        // Existence is a tool error; a failed check is a successful call with a
        // `failed` entry (the check itself is a terminal result).
        self.store
            .db
            .monitor(monitor_id)
            .await?
            .ok_or_else(|| WidowError::NotFound(format!("no monitor with id {monitor_id}")))?;
        let mut payload = MonitorCheckPayload {
            checked: 0,
            pending: false,
            changed: Vec::new(),
            failed: Vec::new(),
            job_id: None,
        };
        match self.check_one(monitor_id, force).await {
            Ok(CheckOutcome::Skipped) => {}
            Ok(CheckOutcome::Unchanged) => payload.checked = 1,
            Ok(CheckOutcome::Changed(change)) => {
                payload.checked = 1;
                payload.changed.push(change);
            }
            Err(error) if error.code() == crate::core::error::ErrorCode::NotFound => {
                return Err(error);
            }
            Err(error) => {
                payload.checked = 1;
                payload.failed.push(MonitorCheckFailure {
                    monitor_id,
                    code: error.code().as_str().to_string(),
                    message: error.to_string(),
                });
            }
        }
        Ok(payload)
    }

    async fn enqueue_check_all(self: &Arc<Self>, force: bool) -> Result<MonitorCheckPayload> {
        let job_id = self.enqueue_check_job(force, None).await?;
        Ok(MonitorCheckPayload {
            checked: 0,
            pending: true,
            changed: Vec::new(),
            failed: Vec::new(),
            job_id: Some(job_id),
        })
    }

    /// Inline sequential checks up to `wait_ms`, then **one** job for the
    /// remainder. The deadline is checked between monitors; a check already in
    /// flight is allowed to finish and its typed result is included.
    async fn check_all_with_wait(
        self: &Arc<Self>,
        force: bool,
        wait_ms: u64,
    ) -> Result<MonitorCheckPayload> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let monitors = self.check_set(force).await?;
        let mut payload = MonitorCheckPayload {
            checked: 0,
            pending: false,
            changed: Vec::new(),
            failed: Vec::new(),
            job_id: None,
        };
        let mut remaining = Vec::new();
        for (index, monitor) in monitors.iter().enumerate() {
            if Instant::now() >= deadline {
                remaining = monitors[index..].iter().map(|monitor| monitor.id).collect();
                break;
            }
            match self.check_one(monitor.id, force).await {
                Ok(CheckOutcome::Skipped) => {}
                Ok(CheckOutcome::Unchanged) => payload.checked += 1,
                Ok(CheckOutcome::Changed(change)) => {
                    payload.checked += 1;
                    payload.changed.push(change);
                }
                Err(error) => {
                    payload.checked += 1;
                    payload.failed.push(MonitorCheckFailure {
                        monitor_id: monitor.id,
                        code: error.code().as_str().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }
        if !remaining.is_empty() {
            let job_id = self.enqueue_check_job(force, Some(remaining)).await?;
            payload.job_id = Some(job_id);
            payload.pending = true;
        }
        Ok(payload)
    }

    async fn check_set(&self, force: bool) -> Result<Vec<MonitorRow>> {
        if force {
            self.store
                .db
                .monitors_active(self.config.monitor.max_checks_per_call)
                .await
        } else {
            self.store
                .db
                .monitors_due(Utc::now(), self.config.monitor.max_checks_per_call)
                .await
        }
    }

    async fn enqueue_check_job(
        self: &Arc<Self>,
        force: bool,
        monitor_ids: Option<Vec<i64>>,
    ) -> Result<String> {
        let job_id = uuid::Uuid::now_v7().to_string();
        let spec_json = match &monitor_ids {
            Some(ids) => serde_json::json!({ "force": force, "monitor_ids": ids }).to_string(),
            None => serde_json::json!({ "force": force }).to_string(),
        };
        self.store
            .db
            .create_job(NewJob {
                id: job_id.clone(),
                kind: KIND_MONITOR_CHECK.to_string(),
                spec_json,
                created_at: Utc::now(),
                seeds: Vec::new(),
            })
            .await?;
        self.spawn_check_job(job_id.clone(), force, monitor_ids);
        Ok(job_id)
    }

    fn spawn_check_job(
        self: &Arc<Self>,
        job_id: String,
        force: bool,
        monitor_ids: Option<Vec<i64>>,
    ) {
        let token = CancellationToken::new();
        {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            running.insert(job_id.clone(), token.clone());
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = manager
                .run_check_job(&job_id, force, monitor_ids, token)
                .await
            {
                tracing::warn!(job_id = %job_id, error = %error, "monitor check job failed");
            }
            let mut running = manager
                .running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            running.remove(&job_id);
        });
    }

    async fn run_check_job(
        self: &Arc<Self>,
        job_id: &str,
        force: bool,
        monitor_ids: Option<Vec<i64>>,
        token: CancellationToken,
    ) -> Result<()> {
        self.store.db.mark_job_running(job_id, Utc::now()).await?;
        let mut counters = JobCounters::default();
        let monitors = match monitor_ids {
            Some(ids) => {
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(row) = self.store.db.monitor(id).await? {
                        rows.push(row);
                    }
                }
                rows
            }
            None => {
                let active = self.store.db.active_monitor_count().await?;
                let monitors = self.check_set(force).await?;
                counters.skipped = active.saturating_sub(monitors.len() as u64);
                monitors
            }
        };

        let mut cancelled = false;
        for monitor in monitors {
            if token.is_cancelled() || self.shutdown.is_cancelled() {
                cancelled = true;
                break;
            }
            match self.check_one(monitor.id, force).await {
                Ok(CheckOutcome::Skipped) => counters.skipped += 1,
                Ok(CheckOutcome::Unchanged | CheckOutcome::Changed(_)) => counters.fetched += 1,
                Err(error) => {
                    counters.failed += 1;
                    counters.push_sample(
                        format!("{}: {error}", error.code().as_str()),
                        self.config.crawl.max_error_samples,
                    );
                }
            }
            self.store
                .db
                .store_job_counters(job_id, counters.clone())
                .await?;
        }
        let state = if cancelled { JOB_CANCELLED } else { JOB_DONE };
        self.store
            .db
            .finish_job(job_id, state, counters, None)
            .await?;
        Ok(())
    }

    /// Check every active monitor that is due (one-shot CLI path, shared with
    /// the tool's bulk form).
    pub async fn run_due(&self, limit: usize) -> Result<MonitorDueSummary> {
        let limit = limit.clamp(1, self.config.monitor.max_checks_per_call);
        let active = self.store.db.active_monitor_count().await?;
        let monitors = self.store.db.monitors_due(Utc::now(), limit).await?;
        let mut summary = MonitorDueSummary::default();
        for monitor in monitors {
            match self.check_one(monitor.id, false).await {
                Ok(CheckOutcome::Skipped) => {}
                Ok(CheckOutcome::Unchanged) => summary.checked += 1,
                Ok(CheckOutcome::Changed(change)) => {
                    summary.checked += 1;
                    summary.changed.push(change);
                }
                Err(error) if error.code() == crate::core::error::ErrorCode::NotFound => {}
                Err(error) => {
                    summary.checked += 1;
                    summary.failed.push(MonitorCheckFailure {
                        monitor_id: monitor.id,
                        code: error.code().as_str().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }
        summary.skipped = active.saturating_sub(summary.checked as u64) as usize;
        Ok(summary)
    }

    /// The single check implementation: one conditional request, one terminal
    /// result. A failure advances the schedule and returns the error; no
    /// snapshot is fabricated and no retry is attempted here.
    async fn check_one(&self, monitor_id: i64, force: bool) -> Result<CheckOutcome> {
        let lock = self.lock_for(monitor_id);
        let _guard = lock.lock().await;

        let monitor = self
            .store
            .db
            .monitor(monitor_id)
            .await?
            .ok_or_else(|| WidowError::NotFound(format!("no monitor with id {monitor_id}")))?;
        if !monitor.active {
            return Ok(CheckOutcome::Skipped);
        }
        if !force && monitor.next_due_at > Utc::now() {
            return Ok(CheckOutcome::Skipped);
        }

        let mode = Mode::parse(&monitor.mode)?;
        let selectors = compile_selectors(&monitor.ignore_selectors)?;
        let previous = self.store.db.latest_snapshot(monitor_id).await?;
        let (validated, compiled) = self.monitor_schema(&monitor).await?;
        let schema = match (validated.as_ref(), compiled.as_ref()) {
            (Some(validated), Some(compiled)) => Some(MonitorSpec {
                validated,
                compiled,
            }),
            _ => None,
        };

        // Conditional GET is sound only when the previous snapshot exists **and**
        // was obtained statically. A rendered check leaves the origin's static
        // validator on `pages` while its snapshot content is the serialized DOM;
        // a pre-v4 snapshot has an unknown (NULL) mode and could be exactly that
        // state. In both cases sending the validator and accepting a 304 would
        // skip rendering and could miss a script/API change. A first check has no
        // previous snapshot, so it also sends no validator and gets a normal 200.
        let allow_not_modified = allows_not_modified(previous.as_ref());
        let page = match self
            .pipeline
            .monitor_fetch(
                &monitor.url,
                mode,
                &selectors,
                schema,
                previous.as_ref().map(|snapshot| snapshot.raw_hash.as_str()),
                allow_not_modified,
            )
            .await
        {
            Ok(page) => page,
            Err(error) => {
                // One terminal check result: advance the schedule, no snapshot.
                let checked_at = Utc::now();
                self.store
                    .db
                    .advance_monitor(
                        monitor_id,
                        checked_at,
                        checked_at + chrono::Duration::seconds(monitor.interval_s as i64),
                    )
                    .await?;
                return Err(error);
            }
        };

        let taken_at = Utc::now();
        let next_due_at = taken_at + chrono::Duration::seconds(monitor.interval_s as i64);

        if page.not_modified {
            let previous = previous.ok_or_else(|| {
                WidowError::internal("a 304 check has no previous snapshot to reuse")
            })?;
            self.store
                .db
                .record_check(CheckRecord {
                    snapshot: NewSnapshot {
                        monitor_id,
                        taken_at,
                        fetch_event_id: page.fetch_event_id,
                        normalized_hash: previous.normalized_hash,
                        raw_hash: previous.raw_hash,
                        result_hash: previous.result_hash,
                        is_change_point: false,
                        // A 304 is only accepted from a static previous check.
                        render_mode: previous.render_mode.unwrap_or_else(|| "static".to_string()),
                        blobs: Vec::new(),
                    },
                    change: None,
                    checked_at: taken_at,
                    next_due_at,
                    webhooks_enabled: self.config.webhooks.enabled,
                })
                .await?;
            return Ok(CheckOutcome::Unchanged);
        }

        let normalized = normalize::normalize(&page.markdown);
        let normalized_hash = normalize::normalized_hash(&normalized);
        let result_hash = page
            .records_json
            .as_deref()
            .map(|json| content_hash(json.as_bytes()));

        let baseline = previous.is_none();
        let text_changed = previous
            .as_ref()
            .is_some_and(|snapshot| snapshot.normalized_hash != normalized_hash);
        let records_changed = previous
            .as_ref()
            .is_some_and(|snapshot| snapshot.result_hash != result_hash);

        // Compare watched fields even when the text is byte-identical: an
        // attribute-only change (price, stock flag) carries no Markdown signal.
        let mut changed_field_names = Vec::new();
        let mut previous_records_unavailable = false;
        if let Some(current_records) = page.records.as_ref() {
            let current = normalize::aggregate_fields(current_records);
            let previous_records = match previous.as_ref().and_then(|s| s.result_hash.as_ref()) {
                Some(hash) => match self.store.blobs.read(hash).await? {
                    Some(bytes) => serde_json::from_slice::<
                        Vec<serde_json::Map<String, serde_json::Value>>,
                    >(&bytes)
                    .ok(),
                    None => None,
                },
                None => None,
            };
            previous_records_unavailable =
                previous.as_ref().is_some_and(|s| s.result_hash.is_some())
                    && previous_records.is_none();
            let previous_aggregate = previous_records
                .as_ref()
                .map(|records| normalize::aggregate_fields(records));
            changed_field_names =
                normalize::changed_fields(previous_aggregate.as_ref(), Some(&current));
        }
        let changed =
            !baseline && (text_changed || records_changed || !changed_field_names.is_empty());

        // Store only content that differs: an unchanged check grows no blob.
        let mut blobs = Vec::new();
        if previous
            .as_ref()
            .is_none_or(|snapshot| snapshot.normalized_hash != normalized_hash)
        {
            let stored = self.store.put_blob(normalized.as_bytes()).await?;
            debug_assert_eq!(stored, normalized_hash);
            blobs.push(BlobRow {
                hash: normalized_hash.clone(),
                size: normalized.len() as i64,
                content_type: Some("text/markdown; charset=utf-8".to_string()),
                created_at: taken_at,
            });
        }
        if let (Some(result_hash), Some(json)) = (result_hash.as_ref(), page.records_json.as_ref())
            && previous.as_ref().is_none_or(|snapshot| {
                snapshot.result_hash.as_deref() != Some(result_hash.as_str())
            })
        {
            let stored = self.store.put_blob(json.as_bytes()).await?;
            debug_assert_eq!(&stored, result_hash);
            blobs.push(BlobRow {
                hash: result_hash.clone(),
                size: json.len() as i64,
                content_type: Some("application/json".to_string()),
                created_at: taken_at,
            });
        }

        let change = if changed {
            let (mut kind, mut stats, diff) = match previous.as_ref().filter(|_| text_changed) {
                Some(previous) => match self.store.blobs.read(&previous.normalized_hash).await? {
                    Some(bytes) => {
                        let previous_text = String::from_utf8_lossy(&bytes);
                        normalize::classify(
                            &previous_text,
                            &normalized,
                            self.config.monitor.cosmetic_max_changed_lines,
                        )
                    }
                    None => (
                        ChangeKind::Substantive,
                        ChangeStats {
                            previous_unavailable: true,
                            ..ChangeStats::default()
                        },
                        String::new(),
                    ),
                },
                None => (ChangeKind::None, ChangeStats::default(), String::new()),
            };
            if previous_records_unavailable {
                stats.previous_unavailable = true;
            }
            stats.changed_fields = changed_field_names;
            if !stats.changed_fields.is_empty() || (kind == ChangeKind::None && changed) {
                // A watched field (or a records-only difference we cannot name)
                // is substantive: field monitoring must not be outvoted by an
                // unchanged text diff.
                kind = ChangeKind::Substantive;
            }
            if stats.previous_unavailable {
                kind = ChangeKind::Substantive;
            }
            let (diff_text, truncated) =
                normalize::cap_diff(&diff, self.config.monitor.max_diff_bytes);
            ChangeRecord {
                kind,
                stats_json: serde_json::to_string(&stats).map_err(WidowError::internal)?,
                diff_text,
                truncated,
                summary: change_summary(&stats, kind),
            }
        } else {
            ChangeRecord::default()
        };

        let (draft, change_kind, change_summary) = if changed {
            let ChangeRecord {
                kind,
                stats_json,
                diff_text,
                truncated,
                summary,
            } = change;
            (
                Some(NewChangeDraft {
                    from_snapshot: previous.as_ref().map(|snapshot| snapshot.id).ok_or_else(
                        || WidowError::internal("a change without a previous snapshot"),
                    )?,
                    detected_at: taken_at,
                    kind: kind.as_str().to_string(),
                    stats_json,
                    diff_text,
                    truncated,
                }),
                kind.as_str().to_string(),
                summary,
            )
        } else {
            (None, String::new(), String::new())
        };

        let (_, change_id) = self
            .store
            .db
            .record_check(CheckRecord {
                snapshot: NewSnapshot {
                    monitor_id,
                    taken_at,
                    fetch_event_id: page.fetch_event_id,
                    normalized_hash,
                    raw_hash: page.raw_hash,
                    result_hash,
                    is_change_point: changed,
                    render_mode: page.render_mode,
                    blobs,
                },
                change: draft,
                checked_at: taken_at,
                next_due_at,
                webhooks_enabled: self.config.webhooks.enabled,
            })
            .await?;

        match (changed, change_id) {
            (true, Some(change_id)) => Ok(CheckOutcome::Changed(MonitorChangeSummary {
                monitor_id,
                change_id,
                kind: change_kind,
                summary: change_summary,
            })),
            _ => Ok(CheckOutcome::Unchanged),
        }
    }

    /// Rebuild the monitor's schema from its stored profile. The stored profile
    /// hash is authoritative: if a future semantic version changes what the same
    /// JSON validates to, the monitor keeps its recorded identity and a warning
    /// records the drift.
    async fn monitor_schema(
        &self,
        monitor: &MonitorRow,
    ) -> Result<(Option<ValidatedSpec>, Option<CompiledSpec>)> {
        let Some(hash) = monitor.profile_hash.as_ref() else {
            return Ok((None, None));
        };
        let profile =
            self.store.db.profile(hash).await?.ok_or_else(|| {
                WidowError::internal(format!("monitor profile {hash} is missing"))
            })?;
        let raw: Value = serde_json::from_str(&profile.spec_json).map_err(WidowError::internal)?;
        let mut validated = spec::validate(&raw)?;
        if &validated.hash != hash {
            tracing::warn!(
                monitor_id = monitor.id,
                recorded = %hash,
                current = %validated.hash,
                "spec profile semantics changed; keeping the recorded profile identity"
            );
            validated.hash = hash.clone();
        }
        let compiled = spec::compile(&validated.spec)?;
        Ok((Some(validated), Some(compiled)))
    }

    fn lock_for(&self, monitor_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .check_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(monitor_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    // -- history -----------------------------------------------------------

    pub async fn list(&self, input: MonitorListInput) -> Result<MonitorListPayload> {
        let include_inactive = input.include_inactive.unwrap_or(false);
        let limit = input
            .limit
            .unwrap_or(MONITOR_LIST_DEFAULT)
            .clamp(1, MONITOR_LIST_MAX);
        let (rows, total) = self.store.db.monitors_page(limit, include_inactive).await?;
        let total = total as usize;
        let count = rows.len();
        let monitors = rows
            .into_iter()
            .map(|row| MonitorListItem {
                monitor_id: row.id,
                url: row.url,
                label: row.label,
                mode: row.mode,
                active: row.active,
                has_schema: row.profile_hash.is_some(),
                interval_s: row.interval_s,
                created_at: format_time(row.created_at),
                last_checked_at: row.last_checked_at.map(format_time),
                next_due_at: format_time(row.next_due_at),
            })
            .collect();
        Ok(MonitorListPayload {
            monitors,
            count,
            total,
            truncated: count < total,
            limit_hit: if count < total {
                Some("limit".to_string())
            } else {
                None
            },
        })
    }

    pub async fn changes(&self, input: MonitorChangesInput) -> Result<MonitorChangesPayload> {
        let monitor = self
            .store
            .db
            .monitor(input.monitor_id)
            .await?
            .ok_or_else(|| {
                WidowError::NotFound(format!("no monitor with id {}", input.monitor_id))
            })?;
        let limit = input
            .limit
            .unwrap_or(self.config.monitor.changes_limit_default)
            .clamp(1, self.config.monitor.changes_limit_max);
        let scope = format!("monitor:{}", monitor.id);
        let before_id = match input.cursor.as_deref() {
            Some(text) => {
                let (cursor_scope, id) = cursor::decode(text)?;
                if cursor_scope != scope {
                    return Err(WidowError::NotFound(
                        "cursor belongs to another monitor".to_string(),
                    ));
                }
                Some(id)
            }
            None => None,
        };
        let mut rows = self
            .store
            .db
            .monitor_changes(monitor.id, before_id, limit + 1)
            .await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|row| cursor::encode(&scope, row.id))
        } else {
            None
        };
        let include_diff = input.include_diff.unwrap_or(true);
        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            changes.push(MonitorChangeItem {
                change_id: row.id,
                from_snapshot: format_time(row.from_snapshot_at),
                to_snapshot: format_time(row.to_snapshot_at),
                detected_at: format_time(row.detected_at),
                kind: row.kind,
                stats: serde_json::from_value(row.stats).map_err(|error| {
                    WidowError::internal(format!("stored change stats are not readable: {error}"))
                })?,
                diff: if include_diff {
                    Some(row.diff_text)
                } else {
                    None
                },
                truncated: row.truncated,
            });
        }
        Ok(MonitorChangesPayload {
            monitor_id: monitor.id,
            changes,
            next_cursor,
        })
    }

    pub async fn delete(&self, input: MonitorDeleteInput) -> Result<MonitorDeletePayload> {
        self.store
            .db
            .monitor(input.monitor_id)
            .await?
            .ok_or_else(|| {
                WidowError::NotFound(format!("no monitor with id {}", input.monitor_id))
            })?;
        self.store.db.deactivate_monitor(input.monitor_id).await?;
        Ok(MonitorDeletePayload {
            monitor_id: input.monitor_id,
            active: false,
        })
    }
}

/// The change row assembled before the snapshot id exists.
#[derive(Debug, Default)]
struct ChangeRecord {
    kind: ChangeKind,
    stats_json: String,
    diff_text: String,
    truncated: bool,
    summary: String,
}

/// The conditional-GET predicate. `None` (no previous snapshot) and a NULL
/// render mode (pre-v4 row) are both "no validator", never "static".
fn allows_not_modified(previous: Option<&crate::store::monitor::SnapshotRow>) -> bool {
    matches!(
        previous.and_then(|snapshot| snapshot.render_mode.as_deref()),
        Some("static")
    )
}

fn compile_selectors(texts: &[String]) -> Result<Vec<Selector>> {
    let mut selectors = Vec::with_capacity(texts.len());
    for (index, text) in texts.iter().enumerate() {
        selectors.push(
            Selector::parse(text).map_err(|_| WidowError::SchemaInvalid {
                pointer: format!("/ignore_selectors/{index}"),
                message: format!("invalid CSS selector `{text}`"),
            })?,
        );
    }
    Ok(selectors)
}

fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn change_summary(stats: &ChangeStats, kind: ChangeKind) -> String {
    if stats.previous_unavailable {
        return "previous snapshot content unavailable; recorded as a new baseline".to_string();
    }
    if !stats.changed_fields.is_empty() {
        return format!("fields changed: {}", stats.changed_fields.join(", "));
    }
    let mut summary = format!("{} lines changed", stats.changed_lines);
    if kind == ChangeKind::Cosmetic {
        summary.push_str(" (cosmetic)");
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::monitor::SnapshotRow;

    fn snapshot(render_mode: Option<&str>) -> SnapshotRow {
        SnapshotRow {
            id: 1,
            monitor_id: 1,
            taken_at: Utc::now(),
            fetch_event_id: Some(1),
            normalized_hash: "n".repeat(64),
            raw_hash: "r".repeat(64),
            result_hash: None,
            is_change_point: false,
            render_mode: render_mode.map(str::to_string),
        }
    }

    #[test]
    fn conditional_get_needs_an_existing_static_snapshot() {
        assert!(
            !allows_not_modified(None),
            "a first check sends no validator and gets a normal 200"
        );
        assert!(
            !allows_not_modified(Some(&snapshot(None))),
            "a pre-v4 NULL mode is unknown, never static"
        );
        assert!(!allows_not_modified(Some(&snapshot(Some("rendered")))));
        assert!(!allows_not_modified(Some(&snapshot(Some(
            "rendered_partial"
        )))));
        assert!(allows_not_modified(Some(&snapshot(Some("static")))));
    }
}
