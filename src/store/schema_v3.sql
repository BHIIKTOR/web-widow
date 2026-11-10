-- Migration 003_monitor.sql — schema v3 ( monitoring area).
-- Applied inside a transaction by the writer; forward-only, tracked by
-- PRAGMA user_version. `snapshots.result_hash` is the monitor's schema result
-- identity (the list has the column); the bytes live in the CAS so
-- a field diff can read the previous values without a second column.

CREATE TABLE monitors (
 id INTEGER PRIMARY KEY,
 -- Canonical URL (B04 rule): tracking parameters stripped, query pairs sorted.
 url TEXT NOT NULL,
 mode TEXT NOT NULL,
 -- Versioned profile identity (`spec/v1/...`) when the monitor has a schema.
 profile_hash TEXT,
 interval_s INTEGER NOT NULL,
 ignore_selectors_json TEXT NOT NULL DEFAULT '[]',
 active INTEGER NOT NULL DEFAULT 1,
 created_at TEXT NOT NULL,
 last_checked_at TEXT,
 next_due_at TEXT NOT NULL
);

CREATE INDEX monitors_due ON monitors (active, next_due_at, id);

CREATE TABLE snapshots (
 id INTEGER PRIMARY KEY,
 monitor_id INTEGER NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
 taken_at TEXT NOT NULL,
 -- The fetch event that produced this snapshot (a 304 event for unchanged
 -- checks): the check-level record, one per check.
 fetch_event_id INTEGER REFERENCES fetch_events (id) ON DELETE SET NULL,
 -- CAS hash of the normalized Markdown; also the "content changed" key.
 normalized_hash TEXT NOT NULL,
 raw_hash TEXT NOT NULL,
 -- CAS hash of the canonical records JSON for a schema monitor.
 result_hash TEXT,
 is_change_point INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX snapshots_monitor ON snapshots (monitor_id, taken_at DESC, id DESC);

CREATE TABLE changes (
 id INTEGER PRIMARY KEY,
 monitor_id INTEGER NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
 from_snapshot INTEGER NOT NULL REFERENCES snapshots (id),
 to_snapshot INTEGER NOT NULL REFERENCES snapshots (id),
 detected_at TEXT NOT NULL,
 kind TEXT NOT NULL,
 stats_json TEXT NOT NULL,
 diff_text TEXT NOT NULL,
 truncated INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX changes_monitor ON changes (monitor_id, id DESC);
