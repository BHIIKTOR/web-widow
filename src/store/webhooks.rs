//! Webhook outbox persistence ( W1).
//!
//! Enqueue happens inside the check transaction (see
//! [`super::monitor::Database::record_check`]); this module owns the attachment
//! set, the due-row scan, attempt results, status reads and the retention
//! prune. No network code lives here.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use crate::core::error::Result;

use super::db::{Database, fmt_time, parse_time};

/// Maximum targets attached to one monitor.
pub const MAX_TARGETS_PER_MONITOR: usize = 8;
/// Delivery rows attempted in one dispatcher pass.
pub const DELIVERY_PASS: usize = 20;

/// One row the dispatcher should attempt.
#[derive(Debug, Clone)]
pub struct DueDelivery {
 pub id: i64,
 pub change_id: i64,
 pub monitor_id: i64,
 pub target: String,
 pub attempts: i64,
}

/// One delivery as `webhook_status` shows it (never a URL/secret).
#[derive(Debug, Clone)]
pub struct DeliveryRow {
 pub id: i64,
 pub change_id: i64,
 pub monitor_id: i64,
 pub target: String,
 pub status: String,
 pub attempts: i64,
 pub last_status: Option<i64>,
 pub next_attempt_at: String,
 pub created_at: String,
 pub delivered_at: Option<String>,
 pub last_error: Option<String>,
}

/// One monitor with its attached target names.
#[derive(Debug, Clone)]
pub struct MonitorAttachmentRow {
 pub monitor_id: i64,
 pub label: Option<String>,
 pub targets: Vec<String>,
}

/// The change/monitor data a payload needs.
#[derive(Debug, Clone)]
pub struct DeliveryContext {
 pub monitor_url: String,
 pub monitor_label: Option<String>,
 pub monitor_active: bool,
 pub detected_at: DateTime<Utc>,
 pub kind: String,
 pub stats_json: String,
 pub diff_text: String,
 pub truncated: bool,
 pub from_snapshot_at: DateTime<Utc>,
 pub to_snapshot_at: DateTime<Utc>,
}

/// The result of one attempt.
#[derive(Debug, Clone)]
pub struct DeliveryUpdate {
 /// `pending`, `delivered` or `dead`.
 pub status: &'static str,
 pub attempts: i64,
 pub next_attempt_at: DateTime<Utc>,
 pub last_status: Option<u16>,
 pub last_error: Option<String>,
 pub delivered_at: Option<DateTime<Utc>>,
}

impl Database {
 /// Replace one monitor's attachment set in a single transaction.
 pub async fn set_monitor_webhooks(
 &self,
 monitor_id: i64,
 targets: &[String],
 now: DateTime<Utc>) -> Result<()> {
 let targets = targets.to_vec();
 self.write(move |connection| {
 let transaction = connection.transaction()?;
 transaction.execute(
 "DELETE FROM monitor_webhooks WHERE monitor_id = ?1",
 params![monitor_id])?;
 for target in &targets {
 transaction.execute(
 "INSERT OR IGNORE INTO monitor_webhooks (monitor_id, target, created_at)
 VALUES (?1, ?2, ?3)",
 params![monitor_id, target, fmt_time(now)])?;
 }
 transaction.commit()
 })
.await
 }

 pub async fn monitor_webhook_targets(&self, monitor_id: i64) -> Result<Vec<String>> {
 self.read(move |connection| {
 let mut statement = connection.prepare(
 "SELECT target FROM monitor_webhooks WHERE monitor_id = ?1 ORDER BY target")?;
 let rows = statement.query_map(params![monitor_id], |row| row.get::<_, String>(0))?;
 rows.collect()
 })
.await
 }

 /// Every monitor with at least one attached target, grouped.
 pub async fn webhook_attachments(&self) -> Result<Vec<MonitorAttachmentRow>> {
 self.read(|connection| {
 let mut statement = connection.prepare(
 "SELECT m.id, m.label, w.target
 FROM monitors m JOIN monitor_webhooks w ON w.monitor_id = m.id
 ORDER BY m.id, w.target")?;
 let rows = statement.query_map([], |row| {
 Ok((
 row.get::<_, i64>(0)?,
 row.get::<_, Option<String>>(1)?,
 row.get::<_, String>(2)?))
 })?;
 let mut attachments: Vec<MonitorAttachmentRow> = Vec::new();
 for row in rows {
 let (monitor_id, label, target) = row?;
 match attachments.last_mut() {
 Some(last) if last.monitor_id == monitor_id => last.targets.push(target),
 _ => attachments.push(MonitorAttachmentRow {
 monitor_id,
 label,
 targets: vec![target],
 }),
 }
 }
 Ok(attachments)
 })
.await
 }

 /// Due `pending` rows, oldest next attempt first, bounded by `limit`.
 pub async fn due_webhook_deliveries(
 &self,
 now: DateTime<Utc>,
 limit: usize) -> Result<Vec<DueDelivery>> {
 self.read(move |connection| {
 let mut statement = connection.prepare(
 "SELECT id, change_id, monitor_id, target, attempts
 FROM webhook_deliveries
 WHERE status = 'pending' AND next_attempt_at <= ?1
 ORDER BY next_attempt_at, id LIMIT ?2")?;
 let rows = statement.query_map(
 params![fmt_time(now), limit.min(i64::MAX as usize) as i64],
 |row| {
 Ok(DueDelivery {
 id: row.get(0)?,
 change_id: row.get(1)?,
 monitor_id: row.get(2)?,
 target: row.get(3)?,
 attempts: row.get(4)?,
 })
 })?;
 rows.collect()
 })
.await
 }

 pub async fn update_webhook_delivery(&self, id: i64, update: DeliveryUpdate) -> Result<()> {
 let next = fmt_time(update.next_attempt_at);
 let delivered = update.delivered_at.map(fmt_time);
 self.write(move |connection| {
 connection.execute(
 "UPDATE webhook_deliveries
 SET status = ?2, attempts = ?3, next_attempt_at = ?4,
 last_status = ?5, last_error = ?6, delivered_at = ?7
 WHERE id = ?1",
 params![
 id,
 update.status,
 update.attempts,
 next,
 update.last_status.map(i64::from),
 update.last_error,
 delivered,
 ])?;
 Ok(())
 })
.await
 }

 /// The change/monitor data one delivery renders. `None` when the change was
 /// pruned (the delivery is then dead).
 pub async fn webhook_delivery_context(
 &self,
 change_id: i64) -> Result<Option<DeliveryContext>> {
 self.read(move |connection| {
 connection
.query_row(
 "SELECT m.url, m.label, m.active, c.detected_at, c.kind, c.stats_json,
 c.diff_text, c.truncated, f.taken_at, t.taken_at
 FROM changes c
 JOIN monitors m ON m.id = c.monitor_id
 JOIN snapshots f ON f.id = c.from_snapshot
 JOIN snapshots t ON t.id = c.to_snapshot
 WHERE c.id = ?1",
 params![change_id],
 |row| {
 Ok(DeliveryContext {
 monitor_url: row.get(0)?,
 monitor_label: row.get(1)?,
 monitor_active: row.get::<_, i64>(2)? != 0,
 detected_at: parse_time(&row.get::<_, String>(3)?)?,
 kind: row.get(4)?,
 stats_json: row.get(5)?,
 diff_text: row.get(6)?,
 truncated: row.get::<_, i64>(7)? != 0,
 from_snapshot_at: parse_time(&row.get::<_, String>(8)?)?,
 to_snapshot_at: parse_time(&row.get::<_, String>(9)?)?,
 })
 })
.optional()
 })
.await
 }

 /// Newest-first delivery page for `webhook_status`.
 pub async fn webhook_delivery_page(&self, limit: usize) -> Result<Vec<DeliveryRow>> {
 self.read(move |connection| {
 let mut statement = connection.prepare(
 "SELECT id, change_id, monitor_id, target, status, attempts, last_status,
 next_attempt_at, created_at, delivered_at, last_error
 FROM webhook_deliveries
 ORDER BY id DESC LIMIT ?1")?;
 let rows =
 statement.query_map(params![limit.min(i64::MAX as usize) as i64], |row| {
 Ok(DeliveryRow {
 id: row.get(0)?,
 change_id: row.get(1)?,
 monitor_id: row.get(2)?,
 target: row.get(3)?,
 status: row.get(4)?,
 attempts: row.get(5)?,
 last_status: row.get(6)?,
 next_attempt_at: row.get(7)?,
 created_at: row.get(8)?,
 delivered_at: row.get(9)?,
 last_error: row.get(10)?,
 })
 })?;
 rows.collect()
 })
.await
 }

 /// `(pending, dead)` counts for `webhook_status`.
 pub async fn webhook_delivery_counts(&self) -> Result<(u64, u64)> {
 self.read(|connection| {
 let pending: i64 = connection.query_row(
 "SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'pending'",
 [],
 |row| row.get(0))?;
 let dead: i64 = connection.query_row(
 "SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'dead'",
 [],
 |row| row.get(0))?;
 Ok((
 u64::try_from(pending).unwrap_or(0),
 u64::try_from(dead).unwrap_or(0)))
 })
.await
 }

 /// Earliest `next_attempt_at` among pending rows, if any.
 pub async fn next_webhook_attempt_at(&self) -> Result<Option<DateTime<Utc>>> {
 self.read(|connection| {
 let text: Option<String> = connection
.query_row(
 "SELECT MIN(next_attempt_at) FROM webhook_deliveries WHERE status = 'pending'",
 [],
 |row| row.get(0))
.optional()?
.flatten();
 match text {
 Some(value) => Ok(Some(parse_time(&value)?)),
 None => Ok(None),
 }
 })
.await
 }
}
