//! Jobs and frontier persistence (IMP-DATA-001, IMP-B04).
//!
//! Dedup is a database invariant: every enqueue is `INSERT ... ON CONFLICT DO
//! NOTHING` against `PRIMARY KEY(job_id, url_hash)`, so a second enqueue of the
//! same canonical URL cannot create a row even if a caller forgets to check.
//!
//! Leases are rows, not in-memory state: a worker marks a row `leased` with a
//! `lease_until`, and any expired lease is requeued with `attempts+1`. That is
//! what makes a killed process resumable without a second queue service.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::Result;
use crate::store::db::{Database, fmt_time, parse_time};

pub const JOB_QUEUED: &str = "queued";
pub const JOB_RUNNING: &str = "running";
pub const JOB_DONE: &str = "done";
pub const JOB_FAILED: &str = "failed";
pub const JOB_CANCELLED: &str = "cancelled";

pub const URL_QUEUED: &str = "queued";
pub const URL_LEASED: &str = "leased";
pub const URL_DONE: &str = "done";
pub const URL_FAILED: &str = "failed";
pub const URL_SKIPPED: &str = "skipped";

/// Counters and samples shown by `crawl_status` (IMP-CONTRACT-001 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCounters {
    pub fetched: u64,
    pub failed: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub renders: u64,
    /// Limit names that fired, e.g. `max_pages`.
    #[serde(default)]
    pub limits_hit: Vec<String>,
    /// Bounded error samples, newest last; the cap is enforced by the caller's
    /// config (`crawl.max_error_samples`, max 5 per contract).
    #[serde(default)]
    pub error_samples: Vec<String>,
}

impl JobCounters {
    pub fn merge(&mut self, other: &JobCounters) {
        self.fetched += other.fetched;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.bytes += other.bytes;
        self.renders += other.renders;
        for limit in &other.limits_hit {
            if !self.limits_hit.contains(limit) {
                self.limits_hit.push(limit.clone());
            }
        }
        for sample in &other.error_samples {
            self.push_sample(sample.clone(), usize::MAX);
        }
    }

    pub fn push_sample(&mut self, sample: String, max: usize) {
        self.error_samples.push(sample);
        while self.error_samples.len() > max {
            self.error_samples.remove(0);
        }
    }

    pub fn note_limit(&mut self, limit: &str) {
        if !self.limits_hit.iter().any(|known| known == limit) {
            self.limits_hit.push(limit.to_string());
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub kind: String,
    pub spec_json: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub counters: JobCounters,
    pub cancel_requested: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub kind: String,
    /// Canonical start request (scope, limits, spec) so a restart can rebuild it.
    pub spec_json: String,
    pub created_at: DateTime<Utc>,
    pub seeds: Vec<Seed>,
}

#[derive(Debug, Clone)]
pub struct Seed {
    pub url: String,
    pub url_hash: String,
    pub depth: u8,
}

#[derive(Debug, Clone)]
pub struct LeasedUrl {
    pub url: String,
    pub url_hash: String,
    pub depth: u8,
    pub attempts: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierCounts {
    pub queued: u64,
    pub leased: u64,
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
}

impl FrontierCounts {
    pub fn outstanding(&self) -> u64 {
        self.queued + self.leased
    }
}

#[derive(Debug, Clone)]
pub struct CrawlResultRow {
    pub rowid: i64,
    pub url: String,
    pub state: String,
    pub status: Option<i64>,
    pub title: Option<String>,
    pub raw_hash: Option<String>,
    pub links_count: i64,
    pub error: Option<String>,
    /// Records from the job's schema profile, when a schema was supplied.
    pub extracted: Option<Value>,
}

impl Database {
    pub async fn create_job(&self, job: NewJob) -> Result<()> {
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO jobs (id, kind, spec_json, state, created_at, counters_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    job.id,
                    job.kind,
                    job.spec_json,
                    JOB_QUEUED,
                    fmt_time(job.created_at),
                    serde_json::to_string(&JobCounters::default()).unwrap_or_else(|_| "{}".into()),
                ],
            )?;
            for seed in &job.seeds {
                transaction.execute(
                    "INSERT INTO frontier (job_id, url_hash, url, depth, priority, state)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (job_id, url_hash) DO NOTHING",
                    params![
                        job.id,
                        seed.url_hash,
                        seed.url,
                        i64::from(seed.depth),
                        i64::from(seed.depth),
                        URL_QUEUED
                    ],
                )?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn job(&self, id: &str) -> Result<Option<JobRow>> {
        let id = id.to_string();
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT id, kind, spec_json, state, created_at, started_at, finished_at,
                            counters_json, cancel_requested, error
                     FROM jobs WHERE id = ?1",
                    params![id],
                    row_to_job,
                )
                .optional()
        })
        .await
    }

    /// Unfinished jobs of one kind. Crawl and monitor-check jobs share the table
    /// but each manager resumes only its own kind; a monitor job must not be
    /// parsed as a crawl spec.
    pub async fn jobs_unfinished_kind(&self, kind: &str) -> Result<Vec<JobRow>> {
        let kind = kind.to_string();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, kind, spec_json, state, created_at, started_at, finished_at,
                        counters_json, cancel_requested, error
                 FROM jobs WHERE kind = ?1 AND state IN (?2, ?3) ORDER BY created_at",
            )?;
            let rows = statement.query_map(params![kind, JOB_QUEUED, JOB_RUNNING], row_to_job)?;
            rows.collect()
        })
        .await
    }

    pub async fn mark_job_running(&self, id: &str, started_at: DateTime<Utc>) -> Result<()> {
        let id = id.to_string();
        self.write(move |connection| {
            connection.execute(
                "UPDATE jobs SET state = ?2, started_at = COALESCE(started_at, ?3) WHERE id = ?1",
                params![id, JOB_RUNNING, fmt_time(started_at)],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn finish_job(
        &self,
        id: &str,
        state: &str,
        counters: JobCounters,
        error: Option<String>,
    ) -> Result<()> {
        let id = id.to_string();
        let state = state.to_string();
        let counters_json = serde_json::to_string(&counters).unwrap_or_else(|_| "{}".into());
        self.write(move |connection| {
            connection.execute(
                "UPDATE jobs SET state = ?2, finished_at = ?3, counters_json = ?4, error = ?5
                 WHERE id = ?1",
                params![
                    id,
                    state,
                    fmt_time(Utc::now()),
                    counters_json,
                    error.as_deref()
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn store_job_counters(&self, id: &str, counters: JobCounters) -> Result<()> {
        let id = id.to_string();
        let counters_json = serde_json::to_string(&counters).unwrap_or_else(|_| "{}".into());
        self.write(move |connection| {
            connection.execute(
                "UPDATE jobs SET counters_json = ?2 WHERE id = ?1",
                params![id, counters_json],
            )?;
            Ok(())
        })
        .await
    }

    /// Set the cooperative cancel flag. Returns true when the job was still
    /// unfinished (a finished job is left untouched).
    pub async fn request_job_cancel(&self, id: &str) -> Result<bool> {
        let owned = id.to_string();
        self.write(move |connection| {
            connection.execute(
                "UPDATE jobs SET cancel_requested = 1
                 WHERE id = ?1 AND state IN (?2, ?3)",
                params![owned, JOB_QUEUED, JOB_RUNNING],
            )?;
            Ok(())
        })
        .await?;
        // Read back so the caller reports the truth rather than assuming.
        Ok(self.job(id).await?.is_some_and(|job| job.cancel_requested))
    }

    /// Lease the next queued URL (lowest depth first, then discovery order).
    /// Returns `None` when nothing is queued.
    pub async fn lease_next(
        &self,
        job_id: &str,
        now: DateTime<Utc>,
        lease_s: u64,
    ) -> Result<Option<LeasedUrl>> {
        let job_id = job_id.to_string();
        let lease_until = fmt_time(now + chrono::Duration::seconds(lease_s as i64));
        let leased = self
            .write_with(move |connection| {
                let mut statement = connection.prepare(
                    "UPDATE frontier
                     SET state = ?2, attempts = attempts + 1, lease_until = ?3
                     WHERE rowid = (
                         SELECT rowid FROM frontier
                         WHERE job_id = ?1 AND state = ?4
                         ORDER BY priority, rowid LIMIT 1
                     )
                     RETURNING url, url_hash, depth, attempts",
                )?;
                let rows = statement.query_map(
                    params![job_id, URL_LEASED, lease_until, URL_QUEUED],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )?;
                let mut leased = None;
                for row in rows {
                    leased = Some(row?);
                }
                Ok(leased)
            })
            .await?;
        Ok(leased.map(|(url, url_hash, depth, attempts)| LeasedUrl {
            url,
            url_hash,
            depth: depth.clamp(0, i64::from(u8::MAX)) as u8,
            attempts: attempts.clamp(0, i64::from(u32::MAX)) as u32,
        }))
    }

    pub async fn finish_url(
        &self,
        job_id: &str,
        url_hash: &str,
        state: &str,
        links_count: u32,
        content_hash: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<()> {
        let job_id = job_id.to_string();
        let url_hash = url_hash.to_string();
        let state = state.to_string();
        let content_hash = content_hash.map(str::to_string);
        let last_error = last_error.map(str::to_string);
        self.write(move |connection| {
            connection.execute(
                "UPDATE frontier
                 SET state = ?3, lease_until = NULL, links_count = ?4,
                     content_hash = COALESCE(?5, content_hash), last_error = ?6
                 WHERE job_id = ?1 AND url_hash = ?2",
                params![
                    job_id,
                    url_hash,
                    state,
                    i64::from(links_count),
                    content_hash,
                    last_error
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Requeue leases whose holder is gone. Returns how many were requeued.
    pub async fn requeue_expired(&self, job_id: &str, now: DateTime<Utc>) -> Result<usize> {
        let job_id = job_id.to_string();
        self.write_with(move |connection| {
            connection.execute(
                "UPDATE frontier SET state = ?2, lease_until = NULL
                 WHERE job_id = ?1 AND state = ?3 AND lease_until IS NOT NULL AND lease_until < ?4",
                params![job_id, URL_QUEUED, URL_LEASED, fmt_time(now)],
            )
        })
        .await
    }

    /// Enqueue discovery candidates. Dedup is the primary-key constraint.
    pub async fn enqueue(&self, job_id: &str, seeds: Vec<Seed>) -> Result<usize> {
        if seeds.is_empty() {
            return Ok(0);
        }
        let job_id = job_id.to_string();
        self.write_with(move |connection| {
            let transaction = connection.transaction()?;
            let mut inserted = 0usize;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO frontier (job_id, url_hash, url, depth, priority, state)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (job_id, url_hash) DO NOTHING",
                )?;
                for seed in &seeds {
                    inserted += statement.execute(params![
                        job_id,
                        seed.url_hash,
                        seed.url,
                        i64::from(seed.depth),
                        i64::from(seed.depth),
                        URL_QUEUED
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(inserted)
        })
        .await
    }

    pub async fn frontier_counts(&self, job_id: &str) -> Result<FrontierCounts> {
        let job_id = job_id.to_string();
        self.read(move |connection| {
            let mut counts = FrontierCounts::default();
            let mut statement = connection
                .prepare("SELECT state, COUNT(*) FROM frontier WHERE job_id = ?1 GROUP BY state")?;
            let rows = statement.query_map(params![job_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (state, count) = row?;
                let count = count.max(0) as u64;
                match state.as_str() {
                    URL_QUEUED => counts.queued = count,
                    URL_LEASED => counts.leased = count,
                    URL_DONE => counts.done = count,
                    URL_FAILED => counts.failed = count,
                    URL_SKIPPED => counts.skipped = count,
                    _ => {}
                }
            }
            Ok(counts)
        })
        .await
    }

    /// Text hashes already delivered by this job, so a restart keeps the
    /// near-duplicate trap guard instead of re-following collapsed links.
    /// Near-duplicate identities already delivered by this job, so a restart
    /// keeps the trap guard instead of re-following collapsed links.
    pub async fn seen_content_hashes(&self, job_id: &str) -> Result<HashSet<String>> {
        let job_id = job_id.to_string();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT content_hash FROM frontier
                 WHERE job_id = ?1 AND state = ?2 AND content_hash IS NOT NULL",
            )?;
            let rows =
                statement.query_map(params![job_id, URL_DONE], |row| row.get::<_, String>(0))?;
            let mut hashes = HashSet::new();
            for row in rows {
                hashes.insert(row?);
            }
            Ok(hashes)
        })
        .await
    }

    /// One row per attempted page (`done`/`failed`), joined to the page record and
    /// to the job schema's extraction profile.
    pub async fn crawl_results(
        &self,
        job_id: &str,
        after_rowid: i64,
        limit: usize,
        profile_hash: Option<&str>,
    ) -> Result<Vec<CrawlResultRow>> {
        let job_id = job_id.to_string();
        let profile_hash = profile_hash.map(str::to_string);
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT f.rowid, f.url, f.state, p.last_status, p.title, p.raw_hash,
                        f.links_count, f.last_error, e.result_json
                 FROM frontier f
                 LEFT JOIN pages p ON p.url_hash = f.url_hash
                 -- One row per frontier row: `extractions` may legitimately hold
                 -- several rows for one page+profile (an uncached `extract`, a
                 -- second crawl, a resumed lease). Bind the latest one.
                 LEFT JOIN extractions e
                        ON e.id = (
                            SELECT MAX(e2.id) FROM extractions e2
                            WHERE e2.page_id = p.id AND e2.profile_hash = ?4
                        )
                 WHERE f.job_id = ?1 AND f.rowid > ?2 AND f.state IN (?5, ?6)
                 ORDER BY f.rowid LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    job_id,
                    after_rowid,
                    limit as i64,
                    profile_hash,
                    URL_DONE,
                    URL_FAILED
                ],
                |row| {
                    let extracted_json: Option<String> = row.get(8)?;
                    let extracted = extracted_json
                        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
                        .and_then(|value| value.get("records").cloned());
                    Ok(CrawlResultRow {
                        rowid: row.get(0)?,
                        url: row.get(1)?,
                        state: row.get(2)?,
                        status: row.get(3)?,
                        title: row.get(4)?,
                        raw_hash: row.get(5)?,
                        links_count: row.get(6)?,
                        error: row.get(7)?,
                        extracted,
                    })
                },
            )?;
            rows.collect()
        })
        .await
    }

    /// Whether a row with this rowid exists for the job (cursor validation).
    pub async fn crawl_row_exists(&self, job_id: &str, rowid: i64) -> Result<bool> {
        let job_id = job_id.to_string();
        self.read(move |connection| {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT rowid FROM frontier WHERE job_id = ?1 AND rowid = ?2",
                    params![job_id, rowid],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(found.is_some())
        })
        .await
    }
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    let counters_json: String = row.get(7)?;
    let counters = serde_json::from_str::<JobCounters>(&counters_json).unwrap_or_default();
    let started_at: Option<String> = row.get(5)?;
    let finished_at: Option<String> = row.get(6)?;
    Ok(JobRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        spec_json: row.get(2)?,
        state: row.get(3)?,
        created_at: parse_time(&row.get::<_, String>(4)?)?,
        started_at: match started_at {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        finished_at: match finished_at {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        counters,
        cancel_requested: row.get::<_, i64>(8)? != 0,
        error: row.get(9)?,
    })
}
