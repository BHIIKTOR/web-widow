//! Bulk crawl: the persisted frontier, the job worker and the `crawl_*` tool
//! service.
//!
//! One worker per job, and every fetch goes through [`Pipeline::crawl_page`], so
//! the SSRF gate, per-hop robots check, caps and render escalation are the same
//! code as `fetch_page`/`extract` (carry-forward: no crawl-side bypass).
//! Dedup is the frontier primary key; leases make a killed process resumable.

pub mod scope;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::error::{ErrorCode, Result, WidowError};
use crate::core::url::PageUrl;
use crate::extract::spec::{self, CompiledSpec, ValidatedSpec};
use crate::pipeline::{CrawlSpec, Pipeline, RenderPolicy};
use crate::store::Store;
use crate::store::jobs::JobCounters;
use crate::store::jobs::{
    CrawlResultRow, FrontierCounts, JOB_CANCELLED, JOB_DONE, JOB_FAILED, JobRow, NewJob, Seed,
    URL_DONE, URL_FAILED, URL_QUEUED, URL_SKIPPED,
};
use scope::{Decision, Scope, ScopeSpec};

const KIND_CRAWL: &str = "crawl";
const MAX_START_URLS: usize = 20;
/// `crawl_start.sitemap_urls` ceiling; each is fetched once.
const MAX_SITEMAP_URLS: usize = 5;

// ---------------------------------------------------------------------------
// Tool inputs and payloads (§3–§6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CrawlRenderPolicy {
    /// Never render; static only.
    Never,
    /// Static first, render only when the thin-content heuristic fires (default).
    /// `auto` is accepted as an input alias; the output name stays `thin_only`.
    #[serde(alias = "auto")]
    ThinOnly,
    /// Always render; slow and renderer-bound.
    Always,
}

impl CrawlRenderPolicy {
    fn pipeline_policy(self) -> RenderPolicy {
        match self {
            Self::Never => RenderPolicy::Never,
            Self::ThinOnly => RenderPolicy::Auto,
            Self::Always => RenderPolicy::Always,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlScopeInput {
    /// Path prefixes a discovered link must start with (empty = every path).
    pub include_paths: Vec<String>,
    /// Regexes matched against `path?query`; a match excludes the link.
    pub exclude_patterns: Vec<String>,
    /// Link depth limit; seeds are depth 0.
    pub max_depth: Option<u8>,
    /// Follow links whose registrable site differs from the seeds.
    pub allow_offsite: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlLimitsInput {
    /// Page ceiling for this job (clamped to the server ceiling).
    pub max_pages: Option<usize>,
    /// Requests per second per host (the server's minimum interval is a floor).
    pub per_host_rate: Option<f64>,
    /// Wall-clock ceiling for this job (clamped to the server ceiling).
    pub max_duration_s: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrawlStartInput {
    /// Absolute http(s) start URLs (at most 20). May be empty when
    /// `sitemap_urls` supplies the seeds.
    pub start_urls: Vec<String>,
    /// Sitemaps fetched before the job is persisted (at most 5). A sitemap URL
    /// is discovered seeds: the job's `scope` filters it, and an empty
    /// `start_urls` derives the site set from these URLs.
    #[serde(default)]
    pub sitemap_urls: Vec<String>,
    #[serde(default)]
    pub scope: Option<CrawlScopeInput>,
    /// Optional extraction schema, the same shape as `extract`.
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    #[serde(default)]
    pub render_policy: Option<CrawlRenderPolicy>,
    #[serde(default)]
    pub limits: Option<CrawlLimitsInput>,
    /// Operator-facing label; not used for identity.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrawlJobInput {
    pub job_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrawlResultsInput {
    pub job_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Opaque cursor from a previous `crawl_results` call for this job.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CrawlLimits {
    pub max_pages: usize,
    pub per_host_rate: f64,
    pub max_duration_s: u64,
    pub max_depth: u8,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrawlStartPayload {
    pub job_id: String,
    pub state: String,
    pub limits: CrawlLimits,
    /// Present only when `sitemap_urls` was supplied; the payload is otherwise
    /// byte-identical to the pre-sitemap contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sitemap: Option<CrawlSitemapSummary>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CrawlSitemapSummary {
    /// Every URL entry parsed from the supplied sitemaps (bounded by
    /// `[sitemap].max_urls`, `[sitemap].max_children`).
    pub urls_found: u64,
    /// Entries enqueued at depth 0 after scope filtering and dedupe.
    pub urls_enqueued: u64,
    /// Entries not enqueued: scope rejection, duplicate, bad scheme or the
    /// `max_pages` budget.
    pub urls_skipped: u64,
    /// Sitemap and child-sitemap failures; the other seeds still start.
    pub errors: Vec<CrawlSitemapError>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrawlSitemapError {
    pub url: String,
    pub code: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CrawlCounters {
    /// URLs still waiting or leased (outstanding work).
    pub queued: u64,
    pub fetched: u64,
    pub failed: u64,
    /// Near-duplicates, scope rejections and dedup collisions.
    pub skipped: u64,
    pub bytes: u64,
    pub renders: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrawlStatusPayload {
    pub job_id: String,
    pub state: String,
    pub counters: CrawlCounters,
    /// Limits that fired; empty when the job finished on its own terms.
    pub limits_hit: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub elapsed_s: u64,
    pub error_samples: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrawlResultItem {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_hash: Option<String>,
    pub links_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrawlResultsPayload {
    pub job_id: String,
    pub state: String,
    pub results: Vec<CrawlResultItem>,
    /// Pass back as `cursor` for the next page; absent when exhausted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Persisted job specification (what a restart needs to rebuild a worker)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedJob {
    start_urls: Vec<String>,
    /// Sitemap URLs supplied at start. Persisted so a restart derives the same
    /// site set when `start_urls` is empty.
    #[serde(default)]
    sitemap_urls: Vec<String>,
    scope: ScopeSpec,
    limits: CrawlLimits,
    render_policy: CrawlRenderPolicy,
    /// Profile hash + canonical JSON of the schema, so a restart reuses the same
    /// `profiles` row instead of creating a second one.
    schema: Option<PersistedSchema>,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSchema {
    canonical_json: String,
    hash: String,
}

impl PersistedJob {
    fn validate(&self) -> Result<ValidatedSpec> {
        let raw: serde_json::Value =
            serde_json::from_str(&self.schema.as_ref().expect("schema").canonical_json)
                .map_err(WidowError::internal)?;
        spec::validate(&raw)
    }

    /// The URLs whose registrable sites define the job's scope. Sitemap-derived
    /// seeds are discovered links, so when no explicit seed exists the sitemap
    /// URL supplies the site set (`allow_offsite: false` still filters
    /// cross-site entries).
    fn scope_seed_urls(&self) -> &[String] {
        if self.start_urls.is_empty() {
            &self.sitemap_urls
        } else {
            &self.start_urls
        }
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Owns the running jobs. Jobs are detached tasks; this map holds only the
/// cancellation token, so a finished job cannot be cancelled twice.
#[derive(Debug)]
pub struct CrawlManager {
    pipeline: Arc<Pipeline>,
    store: Arc<Store>,
    config: Arc<Config>,
    running: StdMutex<HashMap<String, CancellationToken>>,
    shutdown: CancellationToken,
}

impl CrawlManager {
    pub fn new(config: Arc<Config>, pipeline: Arc<Pipeline>, store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            pipeline,
            store,
            config,
            running: StdMutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        })
    }

    /// Relaunch jobs left `queued`/`running` by a previous process. Expired
    /// leases are requeued by the worker itself, so no separate recovery step.
    pub async fn resume(self: &Arc<Self>) -> Result<usize> {
        let jobs = self.store.db.jobs_unfinished_kind(KIND_CRAWL).await?;
        let mut resumed = 0usize;
        for job in jobs {
            if self.shutdown.is_cancelled() {
                break;
            }
            match serde_json::from_str::<PersistedJob>(&job.spec_json) {
                Ok(persisted) => {
                    tracing::info!(job_id = %job.id, state = %job.state, "resuming crawl job");
                    self.spawn(job.id.clone(), persisted, job.cancel_requested);
                    resumed += 1;
                }
                Err(error) => {
                    tracing::warn!(job_id = %job.id, error = %error, "job spec is not readable; marking failed");
                    self.store
                        .db
                        .finish_job(
                            &job.id,
                            JOB_FAILED,
                            job.counters,
                            Some(format!("job spec is not readable: {error}")),
                        )
                        .await?;
                }
            }
        }
        Ok(resumed)
    }

    pub async fn start(self: &Arc<Self>, input: CrawlStartInput) -> Result<CrawlStartPayload> {
        if input.start_urls.len() > MAX_START_URLS {
            return Err(WidowError::SchemaInvalid {
                pointer: "/start_urls".to_string(),
                message: format!("at most {MAX_START_URLS} start URLs are accepted"),
            });
        }
        if input.sitemap_urls.len() > MAX_SITEMAP_URLS {
            return Err(WidowError::SchemaInvalid {
                pointer: "/sitemap_urls".to_string(),
                message: format!("at most {MAX_SITEMAP_URLS} sitemap URLs are accepted"),
            });
        }
        if input.start_urls.is_empty() && input.sitemap_urls.is_empty() {
            return Err(WidowError::SchemaInvalid {
                pointer: "/start_urls".to_string(),
                message: "at least one start URL or sitemap URL is required".to_string(),
            });
        }

        // Parse seeds before anything is persisted.
        let mut start_pages: Vec<PageUrl> = Vec::with_capacity(input.start_urls.len());
        for (index, raw) in input.start_urls.iter().enumerate() {
            let page = PageUrl::parse(raw).map_err(|error| WidowError::SchemaInvalid {
                pointer: format!("/start_urls/{index}"),
                message: error.to_string(),
            })?;
            start_pages.push(page);
        }
        let mut sitemap_pages: Vec<PageUrl> = Vec::with_capacity(input.sitemap_urls.len());
        for (index, raw) in input.sitemap_urls.iter().enumerate() {
            let page = PageUrl::parse(raw).map_err(|error| WidowError::SchemaInvalid {
                pointer: format!("/sitemap_urls/{index}"),
                message: error.to_string(),
            })?;
            sitemap_pages.push(page);
        }

        let limits = self.effective_limits(input.limits.as_ref())?;
        let scope_spec = ScopeSpec {
            include_paths: input
                .scope
                .as_ref()
                .map(|scope| scope.include_paths.clone())
                .unwrap_or_default(),
            exclude_patterns: input
                .scope
                .as_ref()
                .map(|scope| scope.exclude_patterns.clone())
                .unwrap_or_default(),
            max_depth: input.scope.as_ref().and_then(|scope| scope.max_depth),
            allow_offsite: input
                .scope
                .as_ref()
                .map(|scope| scope.allow_offsite)
                .unwrap_or(false),
        };
        // Compile now: an invalid regex must reject the job before any fetch.
        // With no explicit seed, the sitemap URLs supply the site set.
        let scope_seeds: Vec<PageUrl> = if start_pages.is_empty() {
            sitemap_pages.clone()
        } else {
            start_pages.clone()
        };
        let scope = Scope::compile(&scope_spec, &scope_seeds, self.config.crawl.max_depth)?;
        let limits = CrawlLimits {
            max_depth: scope.max_depth(),
            ..limits
        };

        // Schema: validate before persisting (: reject before any fetch).
        let (schema, persisted_schema) = match input.schema.as_ref() {
            Some(raw) => {
                let validated = spec::validate(raw)?;
                spec::compile(&validated.spec)?;
                (
                    Some(validated.clone()),
                    Some(PersistedSchema {
                        canonical_json: validated.canonical_json.clone(),
                        hash: validated.hash.clone(),
                    }),
                )
            }
            None => (None, None),
        };
        let _ = schema;

        // Sitemap seeding: discovered URLs, not explicit seeds. Each sitemap and
        // child is fetched through the gate before the job is persisted.
        let mut frontier: Vec<Seed> = start_pages
            .iter()
            .map(|page| Seed {
                url: page.canonical().to_string(),
                url_hash: page.hash().to_string(),
                depth: 0,
            })
            .collect();
        let mut seen: HashSet<String> = frontier.iter().map(|seed| seed.url_hash.clone()).collect();
        let mut sitemap_summary = None;
        if !sitemap_pages.is_empty() {
            let mut summary = CrawlSitemapSummary::default();
            let mut enqueued = 0u64;
            let mut budget = limits.max_pages.saturating_sub(seen.len());
            for page in &sitemap_pages {
                match self
                    .pipeline
                    .sitemap(crate::sitemap::SitemapInput {
                        url: page.raw().to_string(),
                        limit: None,
                        include_lastmod: Some(false),
                        follow_index: Some(true),
                    })
                    .await
                {
                    Ok(payload) => {
                        summary.urls_found += payload.url_count as u64;
                        for child in &payload.sitemaps {
                            if let Some(code) = &child.error {
                                summary.errors.push(CrawlSitemapError {
                                    url: child.loc.clone(),
                                    code: code.clone(),
                                });
                            }
                        }
                        for entry in payload.urls {
                            if budget == 0 {
                                continue;
                            }
                            let Ok(target) = PageUrl::parse(&entry.loc) else {
                                continue;
                            };
                            if scope.admit(&target, 0) != Decision::Enqueue {
                                continue;
                            }
                            if !seen.insert(target.hash().to_string()) {
                                continue;
                            }
                            budget -= 1;
                            enqueued += 1;
                            frontier.push(Seed {
                                url: target.canonical().to_string(),
                                url_hash: target.hash().to_string(),
                                depth: 0,
                            });
                        }
                    }
                    Err(error) => summary.errors.push(CrawlSitemapError {
                        url: page.raw().to_string(),
                        code: error.code().as_str().to_string(),
                    }),
                }
            }
            summary.urls_enqueued = enqueued;
            summary.urls_skipped = summary.urls_found.saturating_sub(summary.urls_enqueued);
            sitemap_summary = Some(summary);
        }
        if frontier.is_empty() {
            return Err(WidowError::SchemaInvalid {
                pointer: "/start_urls".to_string(),
                message: "no usable seed: start_urls and sitemap_urls produced no fetchable URL"
                    .to_string(),
            });
        }

        let render_policy = input.render_policy.unwrap_or(CrawlRenderPolicy::ThinOnly);
        let persisted = PersistedJob {
            start_urls: start_pages
                .iter()
                .map(|seed| seed.canonical().to_string())
                .collect(),
            sitemap_urls: sitemap_pages
                .iter()
                .map(|page| page.raw().to_string())
                .collect(),
            scope: scope_spec,
            limits: limits.clone(),
            render_policy,
            schema: persisted_schema,
            name: input.name.clone(),
        };
        let spec_json = serde_json::to_string(&persisted).map_err(WidowError::internal)?;

        let job_id = uuid::Uuid::now_v7().to_string();
        self.store
            .db
            .create_job(NewJob {
                id: job_id.clone(),
                kind: KIND_CRAWL.to_string(),
                spec_json,
                created_at: Utc::now(),
                seeds: frontier,
            })
            .await?;

        self.spawn(job_id.clone(), persisted, false);
        tracing::info!(job_id = %job_id, "crawl job created");
        Ok(CrawlStartPayload {
            job_id,
            state: crate::store::jobs::JOB_QUEUED.to_string(),
            limits,
            sitemap: sitemap_summary,
        })
    }

    fn effective_limits(&self, input: Option<&CrawlLimitsInput>) -> Result<CrawlLimits> {
        let server = &self.config.crawl;
        let requested = input.cloned().unwrap_or_default();
        let max_pages = requested
            .max_pages
            .unwrap_or(server.max_pages)
            .min(server.max_pages);
        if max_pages == 0 {
            return Err(WidowError::SchemaInvalid {
                pointer: "/limits/max_pages".to_string(),
                message: "max_pages must be >= 1".to_string(),
            });
        }
        let per_host_rate = requested
            .per_host_rate
            .unwrap_or(server.default_per_host_rate);
        if !per_host_rate.is_finite() || per_host_rate <= 0.0 {
            return Err(WidowError::SchemaInvalid {
                pointer: "/limits/per_host_rate".to_string(),
                message: "per_host_rate must be a positive number".to_string(),
            });
        }
        let max_duration_s = requested
            .max_duration_s
            .unwrap_or(server.max_duration_s)
            .min(server.max_duration_s);
        if max_duration_s == 0 {
            return Err(WidowError::SchemaInvalid {
                pointer: "/limits/max_duration_s".to_string(),
                message: "max_duration_s must be >= 1".to_string(),
            });
        }
        Ok(CrawlLimits {
            max_pages,
            per_host_rate,
            max_duration_s,
            max_depth: server.max_depth,
        })
    }

    fn spawn(self: &Arc<Self>, job_id: String, persisted: PersistedJob, cancel_requested: bool) {
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
            let outcome = manager
                .run_job(&job_id, persisted, token.clone(), cancel_requested)
                .await;
            if let Err(error) = &outcome {
                tracing::warn!(job_id = %job_id, error = %error, "crawl job ended with an error");
            }
            let mut running = manager
                .running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            running.remove(&job_id);
        });
    }

    pub async fn status(&self, job_id: &str) -> Result<CrawlStatusPayload> {
        let job = self.require_job(job_id).await?;
        let counts = self.store.db.frontier_counts(job_id).await?;
        Ok(status_payload(&job, &counts))
    }

    pub async fn cancel(&self, job_id: &str) -> Result<CrawlStatusPayload> {
        let job = self.require_job(job_id).await?;
        if !matches!(job.state.as_str(), crate::store::jobs::JOB_QUEUED)
            && !matches!(job.state.as_str(), crate::store::jobs::JOB_RUNNING)
        {
            // Already finished: cancelling is a no-op, report the truth.
            let counts = self.store.db.frontier_counts(job_id).await?;
            return Ok(status_payload(&job, &counts));
        }
        self.store.db.request_job_cancel(job_id).await?;
        if let Some(token) = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(job_id)
            .cloned()
        {
            token.cancel();
        }
        let job = self.require_job(job_id).await?;
        let counts = self.store.db.frontier_counts(job_id).await?;
        Ok(status_payload(&job, &counts))
    }

    pub async fn results(&self, input: CrawlResultsInput) -> Result<CrawlResultsPayload> {
        let job = self.require_job(&input.job_id).await?;
        let limit = input
            .limit
            .unwrap_or(self.config.crawl.result_limit_default)
            .clamp(1, self.config.crawl.result_limit_max);
        let after_rowid = match input.cursor.as_deref() {
            Some(cursor) => {
                let (cursor_job, rowid) = crate::core::cursor::decode(cursor)?;
                if cursor_job != input.job_id {
                    return Err(WidowError::NotFound(format!(
                        "cursor belongs to job {cursor_job}, not {}",
                        input.job_id
                    )));
                }
                rowid
            }
            None => 0,
        };
        let profile_hash = job
            .spec_json
            .parse::<serde_json::Value>()
            .ok()
            .and_then(|value| {
                value
                    .get("schema")
                    .and_then(|schema| schema.get("hash"))
                    .and_then(|hash| hash.as_str())
                    .map(str::to_string)
            });
        let rows = self
            .store
            .db
            .crawl_results(&input.job_id, after_rowid, limit, profile_hash.as_deref())
            .await?;
        let next_cursor = if rows.len() == limit {
            rows.last()
                .map(|row| crate::core::cursor::encode(&input.job_id, row.rowid))
        } else {
            None
        };
        Ok(CrawlResultsPayload {
            job_id: input.job_id,
            state: job.state,
            results: rows.into_iter().map(result_item).collect(),
            next_cursor,
        })
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

    async fn require_job(&self, job_id: &str) -> Result<JobRow> {
        self.store
            .db
            .job(job_id)
            .await?
            .ok_or_else(|| WidowError::NotFound(format!("no crawl job with id {job_id}")))
    }

    // -----------------------------------------------------------------------
    // Worker
    // -----------------------------------------------------------------------

    async fn run_job(
        self: &Arc<Self>,
        job_id: &str,
        persisted: PersistedJob,
        token: CancellationToken,
        cancel_requested: bool,
    ) -> Result<()> {
        let started_at = Utc::now();
        self.store.db.mark_job_running(job_id, started_at).await?;

        // Rebuild the schema and scope: a restart must not change the job.
        let validated = match persisted.schema.as_ref() {
            Some(_) => Some(persisted.validate()?),
            None => None,
        };
        let compiled: Option<CompiledSpec> = match validated.as_ref() {
            Some(validated) => Some(spec::compile(&validated.spec)?),
            None => None,
        };
        let seeds: Vec<PageUrl> = persisted
            .scope_seed_urls()
            .iter()
            .filter_map(|url| PageUrl::parse(url).ok())
            .collect();
        let scope = Scope::compile(&persisted.scope, &seeds, self.config.crawl.max_depth)?;
        let policy = persisted.render_policy.pipeline_policy();
        let interval = Duration::from_secs_f64(1.0 / persisted.limits.per_host_rate.max(0.001));

        let mut counters = self
            .store
            .db
            .job(job_id)
            .await?
            .map(|job| job.counters)
            .unwrap_or_default();
        let mut fetched = counters.fetched;
        let mut seen: HashSet<String> = self.store.db.seen_content_hashes(job_id).await?;
        let mut last_requeue = Instant::now();
        let deadline =
            started_at + chrono::Duration::seconds(persisted.limits.max_duration_s as i64);

        if cancel_requested || token.is_cancelled() {
            return self.finish(job_id, JOB_CANCELLED, counters, None).await;
        }

        let terminal = loop {
            if token.is_cancelled() {
                break (JOB_CANCELLED, None);
            }
            if Utc::now() >= deadline {
                counters.note_limit("max_duration_s");
                break (JOB_DONE, None);
            }
            if fetched >= persisted.limits.max_pages as u64 {
                counters.note_limit("max_pages");
                break (JOB_DONE, None);
            }
            if last_requeue.elapsed() >= Duration::from_secs(self.config.crawl.lease_s / 2 + 1) {
                let requeued = self.store.db.requeue_expired(job_id, Utc::now()).await?;
                if requeued > 0 {
                    tracing::info!(job_id = %job_id, requeued, "expired leases requeued");
                }
                last_requeue = Instant::now();
            }

            let Some(lease) = self
                .store
                .db
                .lease_next(job_id, Utc::now(), self.config.crawl.lease_s)
                .await?
            else {
                let counts = self.store.db.frontier_counts(job_id).await?;
                if counts.outstanding() == 0 {
                    break (JOB_DONE, None);
                }
                // Only leased rows left (a dead process's leases): wait for expiry.
                tokio::select! {
                    () = token.cancelled() => break (JOB_CANCELLED, None),
                    () = tokio::time::sleep(Duration::from_millis(self.config.crawl.idle_poll_ms)) => {}
                }
                continue;
            };

            let page = match PageUrl::parse(&lease.url) {
                Ok(page) => page,
                Err(error) => {
                    counters.skipped += 1;
                    counters.push_sample(
                        format!("skipped {}: {error}", lease.url),
                        self.config.crawl.max_error_samples,
                    );
                    self.store
                        .db
                        .finish_url(
                            job_id,
                            &lease.url_hash,
                            URL_SKIPPED,
                            0,
                            None,
                            Some(&error.to_string()),
                        )
                        .await?;
                    self.store
                        .db
                        .store_job_counters(job_id, counters.clone())
                        .await?;
                    continue;
                }
            };

            let schema = match (validated.as_ref(), compiled.as_ref()) {
                (Some(validated), Some(compiled)) => Some(CrawlSpec {
                    validated,
                    compiled,
                }),
                _ => None,
            };

            match self
                .pipeline
                .crawl_page(&page, schema, policy, Some(interval), "crawl")
                .await
            {
                Ok(page_result) => {
                    fetched += 1;
                    counters.fetched += 1;
                    counters.bytes += page_result.bytes;
                    if page_result.render_mode.starts_with("rendered") {
                        counters.renders += 1;
                    }

                    let duplicate = seen.contains(&page_result.content_key);
                    if duplicate {
                        counters.skipped += 1;
                    } else {
                        seen.insert(page_result.content_key.clone());
                    }

                    let mut skipped_links = 0u64;
                    let mut candidates: Vec<Seed> = Vec::new();
                    if !duplicate {
                        for link in &page_result.links {
                            let Ok(target) = PageUrl::parse(&link.url) else {
                                skipped_links += 1;
                                continue;
                            };
                            match scope.admit(&target, lease.depth.saturating_add(1)) {
                                Decision::Enqueue => candidates.push(Seed {
                                    url: target.canonical().to_string(),
                                    url_hash: target.hash().to_string(),
                                    depth: lease.depth.saturating_add(1),
                                }),
                                Decision::Skip(_) => skipped_links += 1,
                            }
                        }
                    }
                    let requested = candidates.len() as u64;
                    let inserted = self.store.db.enqueue(job_id, candidates).await? as u64;
                    let dedup_collisions = requested.saturating_sub(inserted);
                    counters.skipped += skipped_links + dedup_collisions;

                    self.store
                        .db
                        .finish_url(
                            job_id,
                            &lease.url_hash,
                            URL_DONE,
                            page_result.links.len().min(u32::MAX as usize) as u32,
                            Some(&page_result.content_key),
                            None,
                        )
                        .await?;
                    self.store
                        .db
                        .store_job_counters(job_id, counters.clone())
                        .await?;
                    tracing::debug!(
                        job_id = %job_id,
                        url = %crate::pipeline::log_target(&page, self.config.logging.redact_query),
                        links = page_result.links.len(),
                        duplicate,
                        fetched,
                        "crawl page done"
                    );
                }
                Err(error) => {
                    let skip = matches!(
                        error.code(),
                        ErrorCode::BlockedByRobots
                            | ErrorCode::BlockedPrivateAddress
                            | ErrorCode::BlockedScheme
                            | ErrorCode::UnsupportedContentType
                            | ErrorCode::InvalidUrl
                    );
                    if skip {
                        counters.skipped += 1;
                        if error.code() != ErrorCode::BlockedByRobots {
                            counters.push_sample(
                                format!("{}: {error}", error.code().as_str()),
                                self.config.crawl.max_error_samples,
                            );
                        }
                        self.store
                            .db
                            .finish_url(
                                job_id,
                                &lease.url_hash,
                                URL_SKIPPED,
                                0,
                                None,
                                Some(&error.to_string()),
                            )
                            .await?;
                    } else if lease.attempts >= self.config.crawl.max_attempts {
                        counters.failed += 1;
                        counters.push_sample(
                            format!("{}: {error}", error.code().as_str()),
                            self.config.crawl.max_error_samples,
                        );
                        self.store
                            .db
                            .finish_url(
                                job_id,
                                &lease.url_hash,
                                URL_FAILED,
                                0,
                                None,
                                Some(&error.to_string()),
                            )
                            .await?;
                    } else {
                        // Requeue for another attempt; `attempts` was incremented
                        // when the row was leased.
                        self.store
                            .db
                            .finish_url(
                                job_id,
                                &lease.url_hash,
                                URL_QUEUED,
                                0,
                                None,
                                Some(&error.to_string()),
                            )
                            .await?;
                    }
                    self.store
                        .db
                        .store_job_counters(job_id, counters.clone())
                        .await?;
                }
            }
        };

        self.finish(job_id, terminal.0, counters, terminal.1).await
    }

    async fn finish(
        &self,
        job_id: &str,
        state: &str,
        counters: JobCounters,
        error: Option<String>,
    ) -> Result<()> {
        self.store
            .db
            .finish_job(job_id, state, counters, error)
            .await?;
        tracing::info!(job_id = %job_id, state, "crawl job finished");
        Ok(())
    }
}

fn status_payload(job: &JobRow, counts: &FrontierCounts) -> CrawlStatusPayload {
    let now = Utc::now();
    let elapsed_s = job
        .started_at
        .map(|started| {
            job.finished_at
                .unwrap_or(now)
                .signed_duration_since(started)
                .num_seconds()
                .max(0) as u64
        })
        .unwrap_or(0);
    let name = serde_json::from_str::<PersistedJob>(&job.spec_json)
        .ok()
        .and_then(|persisted| persisted.name);
    CrawlStatusPayload {
        job_id: job.id.clone(),
        state: job.state.clone(),
        counters: CrawlCounters {
            queued: counts.outstanding(),
            fetched: job.counters.fetched,
            failed: job.counters.failed,
            skipped: job.counters.skipped,
            bytes: job.counters.bytes,
            renders: job.counters.renders,
        },
        limits_hit: job.counters.limits_hit.clone(),
        started_at: job
            .started_at
            .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true)),
        elapsed_s,
        error_samples: job.counters.error_samples.clone(),
        error: job.error.clone(),
        name,
    }
}

fn result_item(row: CrawlResultRow) -> CrawlResultItem {
    CrawlResultItem {
        url: row.url,
        status: row.status.and_then(|status| u16::try_from(status).ok()),
        title: row.title,
        raw_hash: row.raw_hash,
        links_count: row.links_count.clamp(0, i64::from(u32::MAX)) as u32,
        error: row.error,
        extracted: row.extracted,
    }
}

/// A manager handle for tool calls; `Mutex` here only guards the token map.
pub type SharedManager = Arc<CrawlManager>;

#[cfg(test)]
mod tests {
    use super::{CrawlLimits, CrawlRenderPolicy, CrawlStartPayload};

    #[test]
    fn auto_is_an_input_alias_for_thin_only() {
        let parsed: CrawlRenderPolicy = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(parsed, CrawlRenderPolicy::ThinOnly);
        assert_eq!(
            parsed.pipeline_policy(),
            crate::pipeline::RenderPolicy::Auto
        );
        let canonical: CrawlRenderPolicy = serde_json::from_str("\"thin_only\"").unwrap();
        assert_eq!(parsed, canonical);
        // The output name never changes to the alias.
        assert_eq!(serde_json::to_string(&parsed).unwrap(), "\"thin_only\"");
    }

    ///: a start payload without sitemap input is byte-identical to the
    /// pre-sitemap contract (the new key is absent, not null).
    #[test]
    fn a_payload_without_a_sitemap_has_the_pre_sitemap_bytes() {
        let payload = CrawlStartPayload {
            job_id: "job".to_string(),
            state: "queued".to_string(),
            limits: CrawlLimits {
                max_pages: 1,
                per_host_rate: 1.0,
                max_duration_s: 1,
                max_depth: 5,
            },
            sitemap: None,
        };
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"job_id":"job","state":"queued","limits":{"max_pages":1,"per_host_rate":1.0,"max_duration_s":1,"max_depth":5}}"#
        );
    }
}
