-- Migration 005_monitor_label.sql — schema v5.
--
-- A label is human-facing metadata for monitor_list. Identity stays the row id;
-- labels are neither unique nor pattern-enforced. NULL = unlabeled,
-- which is the state of every pre-v5 row.
ALTER TABLE monitors ADD COLUMN label TEXT;
