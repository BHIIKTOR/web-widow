//! Monitor persistence (IMP-DATA-001 monitoring area, B05).
//!
//! A check is one URL, so there is no frontier here: `monitors` is the schedule,
//! `snapshots` is the append-only history and `changes` is the sparse change
//! log. Snapshot content lives in the CAS keyed by `normalized_hash`
//! (`result_hash` for schema records), so a diff can read the previous text
//! without a content column and an unchanged check costs one row.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::core::error::{Result, WidowError};
use crate::store::db::{BlobRow, Database, ProfileRow, fmt_time, parse_time};

#[derive(Debug, Clone)]
pub struct MonitorRow {
    pub id: i64,
    pub url: String,
    pub mode: String,
    pub profile_hash: Option<String>,
    pub interval_s: u64,
    pub ignore_selectors: Vec<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub next_due_at: DateTime<Utc>,
    /// Human-facing label (`project:slug` convention, not enforced). `None` for
    /// every pre-v5 row and every unlabeled monitor.
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewMonitor {
    pub url: String,
    pub mode: String,
    pub profile_hash: Option<String>,
    pub interval_s: u64,
    pub ignore_selectors_json: String,
    pub created_at: DateTime<Utc>,
    pub next_due_at: DateTime<Utc>,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: i64,
    pub monitor_id: i64,
    pub taken_at: DateTime<Utc>,
    pub fetch_event_id: Option<i64>,
    pub normalized_hash: String,
    pub raw_hash: String,
    pub result_hash: Option<String>,
    pub is_change_point: bool,
    /// How this check obtained its content (`static`, `rendered`,
    /// `rendered_partial`). `None` only for a snapshot written before v4.
    pub render_mode: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewSnapshot {
    pub monitor_id: i64,
    pub taken_at: DateTime<Utc>,
    pub fetch_event_id: i64,
    pub normalized_hash: String,
    pub raw_hash: String,
    pub result_hash: Option<String>,
    pub is_change_point: bool,
    pub render_mode: String,
    /// Blob rows to upsert with the snapshot. Bytes are already in the CAS; a
    /// crash between the file and this transaction leaves an unreferenced file,
    /// never a row without bytes (the B01 write order).
    pub blobs: Vec<BlobRow>,
}

/// A change row before its `to_snapshot` exists: the check transaction inserts
/// the snapshot first and fills the id in, so a snapshot and its change cannot
/// be committed apart.
#[derive(Debug, Clone)]
pub struct NewChangeDraft {
    pub from_snapshot: i64,
    pub detected_at: DateTime<Utc>,
    pub kind: String,
    pub stats_json: String,
    pub diff_text: String,
    pub truncated: bool,
}

/// One check outcome: the snapshot, an optional change row, and the schedule
/// advance, committed together.
#[derive(Debug, Clone)]
pub struct CheckRecord {
    pub snapshot: NewSnapshot,
    pub change: Option<NewChangeDraft>,
    pub checked_at: DateTime<Utc>,
    pub next_due_at: DateTime<Utc>,
    /// `[webhooks].enabled` at check time: enqueue one outbox row per attached
    /// target inside the same transaction only when true.
    pub webhooks_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ChangeRow {
    pub id: i64,
    pub monitor_id: i64,
    pub from_snapshot_at: DateTime<Utc>,
    pub to_snapshot_at: DateTime<Utc>,
    pub detected_at: DateTime<Utc>,
    pub kind: String,
    pub stats: Value,
    pub diff_text: String,
    pub truncated: bool,
}

const MONITOR_COLUMNS: &str = "id, url, mode, profile_hash, interval_s, ignore_selectors_json, \
                                active, created_at, last_checked_at, next_due_at, label";

fn row_to_monitor(row: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorRow> {
    let selectors_json: String = row.get(5)?;
    let last_checked_at: Option<String> = row.get(8)?;
    Ok(MonitorRow {
        id: row.get(0)?,
        url: row.get(1)?,
        mode: row.get(2)?,
        profile_hash: row.get(3)?,
        interval_s: row.get::<_, i64>(4)?.max(0) as u64,
        ignore_selectors: serde_json::from_str(&selectors_json).unwrap_or_default(),
        active: row.get::<_, i64>(6)? != 0,
        created_at: parse_time(&row.get::<_, String>(7)?)?,
        last_checked_at: match last_checked_at {
            Some(text) => Some(parse_time(&text)?),
            None => None,
        },
        next_due_at: parse_time(&row.get::<_, String>(9)?)?,
        label: row.get(10)?,
    })
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get(0)?,
        monitor_id: row.get(1)?,
        taken_at: parse_time(&row.get::<_, String>(2)?)?,
        fetch_event_id: row.get(3)?,
        normalized_hash: row.get(4)?,
        raw_hash: row.get(5)?,
        result_hash: row.get(6)?,
        is_change_point: row.get::<_, i64>(7)? != 0,
        render_mode: row.get(8)?,
    })
}

impl Database {
    pub async fn insert_profile(&self, profile: ProfileRow) -> Result<()> {
        self.write(move |connection| {
            connection.execute(
                "INSERT INTO profiles (hash, name, spec_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(hash) DO NOTHING",
                params![
                    profile.hash,
                    profile.name,
                    profile.spec_json,
                    fmt_time(profile.created_at)
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn create_monitor(&self, monitor: NewMonitor) -> Result<i64> {
        self.write_with(move |connection| {
            connection.execute(
                "INSERT INTO monitors (
                     url, mode, profile_hash, interval_s, ignore_selectors_json,
                     active, created_at, next_due_at, label
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
                params![
                    monitor.url,
                    monitor.mode,
                    monitor.profile_hash,
                    monitor.interval_s.min(i64::MAX as u64) as i64,
                    monitor.ignore_selectors_json,
                    fmt_time(monitor.created_at),
                    fmt_time(monitor.next_due_at),
                    monitor.label,
                ],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    pub async fn profile(&self, hash: &str) -> Result<Option<ProfileRow>> {
        let hash = hash.to_string();
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT hash, name, spec_json, created_at FROM profiles WHERE hash = ?1",
                    params![hash],
                    |row| {
                        Ok(ProfileRow {
                            hash: row.get(0)?,
                            name: row.get(1)?,
                            spec_json: row.get(2)?,
                            created_at: parse_time(&row.get::<_, String>(3)?)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    pub async fn monitor(&self, id: i64) -> Result<Option<MonitorRow>> {
        self.read(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {MONITOR_COLUMNS} FROM monitors WHERE id = ?1"),
                    params![id],
                    row_to_monitor,
                )
                .optional()
        })
        .await
    }

    /// Active monitors whose `next_due_at` has arrived, soonest first.
    pub async fn monitors_due(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<MonitorRow>> {
        self.read(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {MONITOR_COLUMNS} FROM monitors
                 WHERE active = 1 AND next_due_at <= ?1
                 ORDER BY next_due_at, id LIMIT ?2"
            ))?;
            let rows = statement.query_map(
                params![fmt_time(now), limit.min(i64::MAX as usize) as i64],
                row_to_monitor,
            )?;
            rows.collect()
        })
        .await
    }

    /// Active monitors in creation order; the `force` form of a bulk check.
    pub async fn monitors_active(&self, limit: usize) -> Result<Vec<MonitorRow>> {
        self.read(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {MONITOR_COLUMNS} FROM monitors WHERE active = 1 ORDER BY id LIMIT ?1"
            ))?;
            let rows = statement
                .query_map(params![limit.min(i64::MAX as usize) as i64], row_to_monitor)?;
            rows.collect()
        })
        .await
    }

    pub async fn active_monitor_count(&self) -> Result<u64> {
        self.read(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM monitors WHERE active = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .map(|count| count.max(0) as u64)
    }

    /// `monitor_list`: id-ordered page plus the matching total before `limit`.
    /// Tombstones are included only when `include_inactive` is true.
    pub async fn monitors_page(
        &self,
        limit: usize,
        include_inactive: bool,
    ) -> Result<(Vec<MonitorRow>, u64)> {
        self.read(move |connection| {
            let include = i64::from(include_inactive);
            let total: i64 = connection.query_row(
                "SELECT COUNT(*) FROM monitors WHERE (?1 = 1 OR active = 1)",
                params![include],
                |row| row.get(0),
            )?;
            let mut statement = connection.prepare(&format!(
                "SELECT {MONITOR_COLUMNS} FROM monitors
                 WHERE (?1 = 1 OR active = 1)
                 ORDER BY id LIMIT ?2"
            ))?;
            let rows = statement.query_map(
                params![include, limit.min(i64::MAX as usize) as i64],
                row_to_monitor,
            )?;
            let monitors = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((monitors, total.max(0) as u64))
        })
        .await
    }

    pub async fn latest_snapshot(&self, monitor_id: i64) -> Result<Option<SnapshotRow>> {
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT id, monitor_id, taken_at, fetch_event_id, normalized_hash,
                            raw_hash, result_hash, is_change_point, render_mode
                     FROM snapshots WHERE monitor_id = ?1
                     ORDER BY taken_at DESC, id DESC LIMIT 1",
                    params![monitor_id],
                    row_to_snapshot,
                )
                .optional()
        })
        .await
    }

    pub async fn snapshot(&self, id: i64) -> Result<Option<SnapshotRow>> {
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT id, monitor_id, taken_at, fetch_event_id, normalized_hash,
                            raw_hash, result_hash, is_change_point, render_mode
                     FROM snapshots WHERE id = ?1",
                    params![id],
                    row_to_snapshot,
                )
                .optional()
        })
        .await
    }

    /// Commit one check: snapshot + blob rows + optional change row + schedule
    /// advance in one transaction. A crash leaves either the previous check or
    /// this one, never a change point without its change row.
    pub async fn record_check(&self, record: CheckRecord) -> Result<(i64, Option<i64>)> {
        self.write_with(move |connection| {
            let transaction = connection.transaction()?;
            let snapshot = &record.snapshot;
            transaction.execute(
                "INSERT INTO snapshots (
                     monitor_id, taken_at, fetch_event_id, normalized_hash, raw_hash,
                     result_hash, is_change_point, render_mode
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    snapshot.monitor_id,
                    fmt_time(snapshot.taken_at),
                    snapshot.fetch_event_id,
                    snapshot.normalized_hash,
                    snapshot.raw_hash,
                    snapshot.result_hash,
                    i64::from(snapshot.is_change_point),
                    snapshot.render_mode,
                ],
            )?;
            let snapshot_id = transaction.last_insert_rowid();
            for blob in &snapshot.blobs {
                transaction.execute(
                    "INSERT INTO blobs (hash, size, content_type, created_at, last_referenced_at)
                     VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(hash) DO UPDATE SET last_referenced_at = excluded.last_referenced_at",
                    params![
                        blob.hash,
                        blob.size,
                        blob.content_type,
                        fmt_time(blob.created_at)
                    ],
                )?;
            }
            let change_id = match &record.change {
                Some(change) => {
                    transaction.execute(
                        "INSERT INTO changes (
                             monitor_id, from_snapshot, to_snapshot, detected_at, kind,
                             stats_json, diff_text, truncated
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            snapshot.monitor_id,
                            change.from_snapshot,
                            snapshot_id,
                            fmt_time(change.detected_at),
                            change.kind,
                            change.stats_json,
                            change.diff_text,
                            i64::from(change.truncated),
                        ],
                    )?;
                    Some(transaction.last_insert_rowid())
                }
                None => None,
            };
            // Enqueue the outbox inside the same transaction as the change row:
            // a committed change always has its notifications (or none when the
            // monitor has no attachment / webhooks are disabled).
            if record.webhooks_enabled
                && let (Some(change_id), Some(change)) = (change_id, record.change.as_ref())
            {
                let targets: Vec<String> = {
                    let mut statement = transaction.prepare(
                        "SELECT target FROM monitor_webhooks
                         WHERE monitor_id = ?1 ORDER BY target",
                    )?;
                    let rows = statement.query_map(params![snapshot.monitor_id], |row| {
                        row.get::<_, String>(0)
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let detected_at = fmt_time(change.detected_at);
                for target in targets {
                    transaction.execute(
                        "INSERT OR IGNORE INTO webhook_deliveries
                             (change_id, monitor_id, target, status, attempts,
                              next_attempt_at, created_at)
                         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4)",
                        params![change_id, snapshot.monitor_id, target, detected_at],
                    )?;
                }
            }
            transaction.execute(
                "UPDATE monitors SET last_checked_at = ?2, next_due_at = ?3 WHERE id = ?1",
                params![
                    snapshot.monitor_id,
                    fmt_time(record.checked_at),
                    fmt_time(record.next_due_at)
                ],
            )?;
            transaction.commit()?;
            Ok((snapshot_id, change_id))
        })
        .await
    }

    /// The one terminal check result for every check, success or failure.
    pub async fn advance_monitor(
        &self,
        id: i64,
        last_checked_at: DateTime<Utc>,
        next_due_at: DateTime<Utc>,
    ) -> Result<()> {
        self.write(move |connection| {
            connection.execute(
                "UPDATE monitors SET last_checked_at = ?2, next_due_at = ?3 WHERE id = ?1",
                params![id, fmt_time(last_checked_at), fmt_time(next_due_at)],
            )?;
            Ok(())
        })
        .await
    }

    /// Tombstone: history and snapshots stay. Returns false for an unknown id.
    /// Pending deliveries for the monitor become dead (no delivery after the
    /// tombstone).
    pub async fn deactivate_monitor(&self, id: i64) -> Result<bool> {
        let now = fmt_time(Utc::now());
        let changed = self
            .write_with(move |connection| {
                let changed = connection.execute(
                    "UPDATE monitors SET active = 0 WHERE id = ?1",
                    params![id],
                )?;
                if changed > 0 {
                    connection.execute(
                        "UPDATE webhook_deliveries
                         SET status = 'dead', last_error = 'monitor is inactive', next_attempt_at = ?2
                         WHERE monitor_id = ?1 AND status = 'pending'",
                        params![id, now],
                    )?;
                }
                Ok(changed)
            })
            .await?;
        Ok(changed > 0)
    }

    /// Newest-first change page. `before_id` is the id cursor (`None` starts at
    /// the newest); `limit + 1` rows tell the caller whether another page exists.
    pub async fn monitor_changes(
        &self,
        monitor_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<ChangeRow>> {
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT c.id, c.monitor_id, f.taken_at, t.taken_at, c.detected_at,
                        c.kind, c.stats_json, c.diff_text, c.truncated
                 FROM changes c
                 JOIN snapshots f ON f.id = c.from_snapshot
                 JOIN snapshots t ON t.id = c.to_snapshot
                 WHERE c.monitor_id = ?1 AND (?2 IS NULL OR c.id < ?2)
                 ORDER BY c.id DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![monitor_id, before_id, limit.min(i64::MAX as usize) as i64],
                |row| {
                    let stats_json: String = row.get(6)?;
                    Ok(ChangeRow {
                        id: row.get(0)?,
                        monitor_id: row.get(1)?,
                        from_snapshot_at: parse_time(&row.get::<_, String>(2)?)?,
                        to_snapshot_at: parse_time(&row.get::<_, String>(3)?)?,
                        detected_at: parse_time(&row.get::<_, String>(4)?)?,
                        kind: row.get(5)?,
                        stats: serde_json::from_str(&stats_json).unwrap_or(Value::Null),
                        diff_text: row.get(7)?,
                        truncated: row.get::<_, i64>(8)? != 0,
                    })
                },
            )?;
            rows.collect()
        })
        .await
    }
}

/// One change joined to its monitor and the page title (the `events` feed).
#[derive(Debug, Clone)]
pub struct EventRow {
    pub change_id: i64,
    pub monitor_id: i64,
    pub monitor_label: Option<String>,
    pub url: String,
    pub title: Option<String>,
    pub detected_at: DateTime<Utc>,
    pub kind: String,
    pub stats: Value,
    pub diff_text: String,
    pub truncated: bool,
    pub from_snapshot_at: DateTime<Utc>,
    pub to_snapshot_at: DateTime<Utc>,
}

impl Database {
    /// Newest-first change page across monitors, with the monitor label and the
    /// page title (by canonical URL). `since` is inclusive.
    pub async fn events_page(
        &self,
        monitor_id: Option<i64>,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let since = since.map(super::db::fmt_time);
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT c.id, c.monitor_id, m.label, m.url, p.title, c.detected_at, c.kind,
                        c.stats_json, c.diff_text, c.truncated, f.taken_at, t.taken_at
                 FROM changes c
                 JOIN monitors m ON m.id = c.monitor_id
                 JOIN snapshots f ON f.id = c.from_snapshot
                 JOIN snapshots t ON t.id = c.to_snapshot
                 LEFT JOIN pages p ON p.canonical_url = m.url
                 WHERE (?1 IS NULL OR c.monitor_id = ?1)
                   AND (?2 IS NULL OR c.detected_at >= ?2)
                 ORDER BY c.detected_at DESC, c.id DESC
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![monitor_id, since, limit.min(i64::MAX as usize) as i64],
                |row| {
                    let stats_json: String = row.get(7)?;
                    Ok(EventRow {
                        change_id: row.get(0)?,
                        monitor_id: row.get(1)?,
                        monitor_label: row.get(2)?,
                        url: row.get(3)?,
                        title: row.get(4)?,
                        detected_at: parse_time(&row.get::<_, String>(5)?)?,
                        kind: row.get(6)?,
                        stats: serde_json::from_str(&stats_json).unwrap_or(Value::Null),
                        diff_text: row.get(8)?,
                        truncated: row.get::<_, i64>(9)? != 0,
                        from_snapshot_at: parse_time(&row.get::<_, String>(10)?)?,
                        to_snapshot_at: parse_time(&row.get::<_, String>(11)?)?,
                    })
                },
            )?;
            rows.collect()
        })
        .await
    }
}

/// Reject a monitor id that does not exist; callers report `not_found`.
pub fn require<T>(row: Option<T>, id: i64) -> Result<T> {
    row.ok_or_else(|| WidowError::NotFound(format!("no monitor with id {id}")))
}
