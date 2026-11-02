-- Migration 002_crawl.sql — schema v2 (IMP-DATA-001 jobs/frontier areas).
-- Applied inside a transaction by the writer; forward-only, tracked by
-- PRAGMA user_version. `links_count` is additive to the IMP-DATA-001 column
-- list because `crawl_results` reports it per page (see EV-B04).

CREATE TABLE jobs (
    id               TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,
    spec_json        TEXT NOT NULL,
    state            TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    started_at       TEXT,
    finished_at      TEXT,
    counters_json    TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    error            TEXT
);

CREATE INDEX jobs_state ON jobs (state);

CREATE TABLE frontier (
    job_id      TEXT    NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    url_hash    TEXT    NOT NULL,
    url         TEXT    NOT NULL,
    depth       INTEGER NOT NULL,
    priority    INTEGER NOT NULL,
    state       TEXT    NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    lease_until TEXT,
    last_error  TEXT,
    links_count INTEGER NOT NULL DEFAULT 0,
    -- Near-duplicate identity of the delivered Markdown (link targets removed);
    -- the trap guard is rebuilt from this after a restart.
    content_hash TEXT,
    PRIMARY KEY (job_id, url_hash)
);

-- SQLite cannot name rowid in an index expression; every index carries it
-- implicitly, so (job_id, state, priority) already provides the
-- (job_id, state, priority, rowid) order IMP-DATA-001 asks for.
CREATE INDEX frontier_next ON frontier (job_id, state, priority);
CREATE INDEX frontier_expiry ON frontier (state, lease_until);
