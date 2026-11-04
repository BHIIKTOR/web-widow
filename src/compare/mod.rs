//! `snapshot_compare`: an explicit pair diff.
//!
//! Two kinds, one shape. **monitor:** the ids are `snapshots.id` and both must
//! belong to the same monitor; text comes from the snapshot's CAS
//! `normalized_hash`, and a schema monitor's canonical records (CAS
//! `result_hash`) are compared record-by-record. **page:** the ids are
//! `fetch_events.id`, both must have a stored extraction and the same canonical
//! URL; the delivered text is compared exactly as stored (no snapshot
//! normalization).
//!
//! This is not `monitor_changes`: that tool is monitor-scoped, change-point and
//! cursor based. This one takes an explicit pair (adjacent or not) and never
//! mutates a row. Missing versions and missing stored bytes are `not_found` with
//! the constraint named, never an empty text.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::error::{Result, WidowError};
use crate::monitor::normalize;
use crate::store::Store;
use crate::store::compare::PageVersionRow;
use crate::store::monitor::SnapshotRow;

/// Characters kept when the caller is silent.
pub const DEFAULT_MAX_CHARS: usize = 20_000;
/// Hard ceiling for `max_chars`.
pub const MAX_MAX_CHARS: usize = 100_000;
/// Changed record indices reported before the list is bounded.
pub const MAX_RECORD_CHANGES: usize = 20;

/// `monitor` or `page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareKind {
    Monitor,
    Page,
}

impl CompareKind {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "monitor" => Ok(Self::Monitor),
            "page" => Ok(Self::Page),
            other => Err(WidowError::SchemaInvalid {
                pointer: "/kind".to_string(),
                message: format!("unknown compare kind `{other}`: expected monitor or page"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Page => "page",
        }
    }
}

/// `snapshot_compare` input.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SnapshotCompareInput {
    /// `monitor` (ids are `snapshots.id`) or `page` (ids are `fetch_events.id`).
    pub kind: String,
    /// The older version's id.
    pub from: i64,
    /// The newer version's id.
    pub to: i64,
    /// Include the unified diff (default true).
    #[serde(default)]
    pub include_diff: Option<bool>,
    /// Diff character cap, 1..=100000 (default 20000).
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// The two ids echoed back.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompareIds {
    pub from: i64,
    pub to: i64,
}

/// The two version timestamps (snapshot `taken_at` / fetch `started_at`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompareTimes {
    pub from: String,
    pub to: String,
}

/// Full counts; the diff may be capped, these never are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompareStats {
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
    pub words_added: usize,
    pub words_removed: usize,
    pub records_added: usize,
    pub records_removed: usize,
    pub records_changed: usize,
}

/// `snapshot_compare` result.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SnapshotComparePayload {
    pub kind: String,
    pub id: CompareIds,
    pub taken_at: CompareTimes,
    pub stats: CompareStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The diff was cut at `max_chars`; `stats` still carries the full counts.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_hit: Option<String>,
    /// First [`MAX_RECORD_CHANGES`] indices whose record differs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub record_changes: Vec<usize>,
}

/// A validated compare request.
#[derive(Debug, Clone)]
pub struct ComparePlan {
    pub kind: CompareKind,
    pub from: i64,
    pub to: i64,
    pub include_diff: bool,
    pub max_chars: usize,
}

impl ComparePlan {
    pub fn validate(input: SnapshotCompareInput) -> Result<Self> {
        let kind = CompareKind::parse(&input.kind)?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
        if !(1..=MAX_MAX_CHARS).contains(&max_chars) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/max_chars".to_string(),
                message: format!("max_chars must be 1..={MAX_MAX_CHARS}, got {max_chars}"),
            });
        }
        Ok(Self {
            kind,
            from: input.from,
            to: input.to,
            include_diff: input.include_diff.unwrap_or(true),
            max_chars,
        })
    }
}

/// A loaded side of the comparison.
struct Side {
    text: String,
    records: Option<Vec<serde_json::Map<String, Value>>>,
    taken_at: String,
}

/// Run the compare. The caller has validated the plan.
pub async fn run(store: &Store, plan: &ComparePlan) -> Result<SnapshotComparePayload> {
    let (from, to) = match plan.kind {
        CompareKind::Monitor => monitor_sides(store, plan).await?,
        CompareKind::Page => page_sides(store, plan).await?,
    };

    let (stats, diff) = normalize::diff_stats(&from.text, &to.text);
    let mut payload = SnapshotComparePayload {
        kind: plan.kind.as_str().to_string(),
        id: CompareIds {
            from: plan.from,
            to: plan.to,
        },
        taken_at: CompareTimes {
            from: from.taken_at,
            to: to.taken_at,
        },
        stats: CompareStats {
            added: stats.lines_added,
            removed: stats.lines_removed,
            words_added: stats.words_added,
            words_removed: stats.words_removed,
            records_added: 0,
            records_removed: 0,
            records_changed: 0,
        },
        diff: None,
        truncated: false,
        limit_hit: None,
        record_changes: Vec::new(),
    };

    if let (Some(previous), Some(current)) = (from.records.as_ref(), to.records.as_ref()) {
        let (added, removed, changed, indices) = compare_records(previous, current);
        payload.stats.records_added = added;
        payload.stats.records_removed = removed;
        payload.stats.records_changed = changed;
        payload.record_changes = indices;
    }

    if plan.include_diff {
        let (kept, cut) = crate::core::truncate::truncate_chars(&diff, plan.max_chars);
        payload.diff = Some(kept);
        payload.truncated = cut;
        payload.limit_hit = cut.then(|| "max_chars".to_string());
    }
    Ok(payload)
}

async fn monitor_sides(store: &Store, plan: &ComparePlan) -> Result<(Side, Side)> {
    let from = require_snapshot(store, plan.from).await?;
    let to = require_snapshot(store, plan.to).await?;
    if from.monitor_id != to.monitor_id {
        return Err(WidowError::SchemaInvalid {
            pointer: "/to".to_string(),
            message: format!(
                "mismatch: snapshot {} belongs to monitor {} but snapshot {} belongs to monitor {}",
                plan.from, from.monitor_id, plan.to, to.monitor_id
            ),
        });
    }
    let from_text = read_text(store, &from.normalized_hash, "snapshot", plan.from).await?;
    let to_text = read_text(store, &to.normalized_hash, "snapshot", plan.to).await?;
    let from_records = read_records(store, from.result_hash.as_deref(), plan.from).await?;
    let to_records = read_records(store, to.result_hash.as_deref(), plan.to).await?;
    // A non-schema snapshot pair has no records on either side; a schema pair
    // whose records are gone must not silently compare as empty.
    let records = match (from_records, to_records) {
        (None, None) => (None, None),
        (previous, current) => (
            Some(previous.unwrap_or_default()),
            Some(current.unwrap_or_default()),
        ),
    };
    Ok((
        Side {
            text: from_text,
            records: records.0,
            taken_at: render_time(&from),
        },
        Side {
            text: to_text,
            records: records.1,
            taken_at: render_time(&to),
        },
    ))
}

fn render_time(snapshot: &SnapshotRow) -> String {
    snapshot
        .taken_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn require_snapshot(store: &Store, id: i64) -> Result<SnapshotRow> {
    store
        .db
        .snapshot(id)
        .await?
        .ok_or_else(|| WidowError::NotFound(format!("snapshot {id} does not exist")))
}

async fn read_text(store: &Store, hash: &str, what: &str, id: i64) -> Result<String> {
    let bytes = store.blobs.read(hash).await?.ok_or_else(|| {
        WidowError::NotFound(format!(
            "{what} {id} text is not in the content store (hash {hash})"
        ))
    })?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

async fn read_records(
    store: &Store,
    hash: Option<&str>,
    id: i64,
) -> Result<Option<Vec<Map<String, Value>>>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    let bytes = store.blobs.read(hash).await?.ok_or_else(|| {
        WidowError::NotFound(format!(
            "snapshot {id} records are not in the content store (hash {hash})"
        ))
    })?;
    let records = serde_json::from_slice::<Vec<Map<String, Value>>>(&bytes).map_err(|error| {
        WidowError::internal(format!("stored snapshot records are not JSON: {error}"))
    })?;
    Ok(Some(records))
}

async fn page_sides(store: &Store, plan: &ComparePlan) -> Result<(Side, Side)> {
    let from = require_page_version(store, plan.from).await?;
    let to = require_page_version(store, plan.to).await?;
    if from.canonical_url != to.canonical_url {
        return Err(WidowError::SchemaInvalid {
            pointer: "/to".to_string(),
            message: format!(
                "mismatch: fetch event {} is `{}` but fetch event {} is `{}`",
                plan.from, from.canonical_url, plan.to, to.canonical_url
            ),
        });
    }
    let from_side = page_side(&from)?;
    let to_side = page_side(&to)?;
    Ok((from_side, to_side))
}

fn page_side(row: &PageVersionRow) -> Result<Side> {
    let result_json = row.result_json.as_deref().ok_or_else(|| {
        WidowError::NotFound(format!(
            "fetch event {} has no stored extraction (mode `{}`, status {}); a 304 or a fetch without extraction cannot be compared",
            row.id,
            row.mode,
            row.http_status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "none".to_string())
        ))
    })?;
    let text = crate::store::fts::index_text_from_stored(Some(result_json)).ok_or_else(|| {
        WidowError::NotFound(format!(
            "fetch event {} stores no comparable text (neither delivered Markdown nor records)",
            row.id
        ))
    })?;
    let records = serde_json::from_str::<Value>(result_json)
        .ok()
        .and_then(|value| value.get("records").cloned())
        .and_then(|value| serde_json::from_value::<Vec<Map<String, Value>>>(value).ok());
    Ok(Side {
        text,
        records,
        taken_at: row.started_at.clone(),
    })
}

async fn require_page_version(store: &Store, id: i64) -> Result<PageVersionRow> {
    store
        .db
        .page_version(id)
        .await?
        .ok_or_else(|| WidowError::NotFound(format!("fetch event {id} does not exist")))
}

/// Count added/removed records by position and the changed indices in the shared
/// range, bounded to [`MAX_RECORD_CHANGES`] reported indices.
pub fn compare_records(
    previous: &[Map<String, Value>],
    current: &[Map<String, Value>],
) -> (usize, usize, usize, Vec<usize>) {
    let added = current.len().saturating_sub(previous.len());
    let removed = previous.len().saturating_sub(current.len());
    let shared = previous.len().min(current.len());
    let mut changed = 0usize;
    let mut indices = Vec::new();
    for index in 0..shared {
        if previous[index] != current[index] {
            changed += 1;
            if indices.len() < MAX_RECORD_CHANGES {
                indices.push(index);
            }
        }
    }
    (added, removed, changed, indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn records(values: &[Value]) -> Vec<Map<String, Value>> {
        values
            .iter()
            .map(|value| value.as_object().cloned().unwrap_or_default())
            .collect()
    }

    #[test]
    fn record_compare_counts_positional_adds_removes_and_changes() {
        let previous = records(&[json!({"a": 1}), json!({"a": 2})]);
        let current = records(&[json!({"a": 1}), json!({"a": 3}), json!({"a": 4})]);
        let (added, removed, changed, indices) = compare_records(&previous, &current);
        assert_eq!((added, removed, changed), (1, 0, 1));
        assert_eq!(indices, vec![1]);

        let (added, removed, changed, _) = compare_records(&current, &previous);
        assert_eq!((added, removed, changed), (0, 1, 1));
    }

    #[test]
    fn the_changed_index_list_is_bounded() {
        let previous = records(&vec![json!({"i": 0}); 40]);
        let current: Vec<Map<String, Value>> = (0..40)
            .map(|index| json!({"i": index}).as_object().cloned().unwrap_or_default())
            .collect();
        let (_, _, changed, indices) = compare_records(&previous, &current);
        assert_eq!(changed, 39);
        assert_eq!(indices.len(), MAX_RECORD_CHANGES);
        assert_eq!(indices[0], 1);
    }
}
