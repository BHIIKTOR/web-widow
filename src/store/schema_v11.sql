-- Migration 011_corpus_freshness.sql — schema v11 (IMP-DEC-FRESHNESS-003).
--
-- Additive only: no existing column is rewritten in place and no row is deleted.
-- The three `UPDATE` statements are the conservative backfill: they move the old
-- `pages.last_fetched` into the attempt pair and then re-derive the success
-- clock from retained fetch events, so only a provable usable 200 survives as a
-- success clock. Every other new column keeps its `NULL`/`0` default: no
-- capture, validation or provenance is invented for legacy rows.
--
-- `write_seq` is the write identity: it increments once per committed usable
-- 200, and `content_write_seq` / `index_write_seq` / `validators_write_seq` /
-- `extractions.source_write_seq` name the write that produced each artifact.

ALTER TABLE pages ADD COLUMN write_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pages ADD COLUMN content_fetched_at TEXT;
ALTER TABLE pages ADD COLUMN content_validated_at TEXT;
ALTER TABLE pages ADD COLUMN content_write_seq INTEGER;
ALTER TABLE pages ADD COLUMN index_fetched_at TEXT;
ALTER TABLE pages ADD COLUMN index_validated_at TEXT;
ALTER TABLE pages ADD COLUMN index_write_seq INTEGER;
ALTER TABLE pages ADD COLUMN validators_write_seq INTEGER;
ALTER TABLE pages ADD COLUMN last_attempt_at TEXT;
ALTER TABLE pages ADD COLUMN last_attempt_failed INTEGER;

ALTER TABLE extractions ADD COLUMN source_write_seq INTEGER;

-- 1. The last attempt's completion time and outcome class become the attempt
-- pair. The old row cannot distinguish a usable 200 from a bound 304, so a
-- legacy `not_modified` is `NULL` (success class unknown), exactly as a live
-- unbound 304 is.
UPDATE pages
SET last_attempt_at = last_fetched,
 last_attempt_failed = CASE
 WHEN outcome = 'ok' THEN 0
 WHEN outcome LIKE 'error:%' THEN 1
 ELSE NULL
 END;

-- 2. The success clock is the newest *retained* event that proves a usable 200:
-- no error, HTTP 200, and either a stored body hash or an extraction created
-- by that same event. `pages.text_hash` is never consulted, and a 304 event
-- is never an anchor.
UPDATE pages
SET last_fetched = (
 SELECT fe.started_at
 FROM fetch_events fe
 WHERE fe.page_id = pages.id
 AND fe.error_kind IS NULL
 AND fe.http_status = 200
 AND (
 fe.text_hash IS NOT NULL
 OR EXISTS (SELECT 1 FROM extractions x WHERE x.fetch_event_id = fe.id)
)
 ORDER BY fe.started_at DESC, fe.id DESC
 LIMIT 1
);
