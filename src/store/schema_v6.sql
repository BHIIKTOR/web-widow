-- Migration 006_fts.sql — schema v6 (M05-field-fidelity-fts, Lane B).
--
-- Local full-text index over stored page content (IMP-DEC-029). One row per
-- `pages` row, `rowid = pages.id`; the store path replaces it inside the same
-- transaction as the page write (explicit upsert, no triggers, IMP-DEC-029).
--
-- The indexed text is stored here, so `snippet()` can answer after the
-- extraction rows age out of retention. That is a deliberate DB-size trade
-- (disclosed in docs/operations.md): bounded per page by `[fts].max_index_chars`,
-- outside the 20 GiB blob budget.
CREATE VIRTUAL TABLE pages_fts USING fts5(
    title,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Small key/value bookkeeping: the one-time backfill marker and future
-- maintenance state. No page data lives here.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
