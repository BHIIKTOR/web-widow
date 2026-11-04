-- Migration 001_init.sql — schema v1.
-- Applied inside a transaction by the writer; forward-only, tracked by PRAGMA
-- user_version. and add their own tables in later migrations.

CREATE TABLE pages (
    id            INTEGER PRIMARY KEY,
    url           TEXT    NOT NULL,
    canonical_url TEXT    NOT NULL,
    url_hash      TEXT    NOT NULL UNIQUE,
    site          TEXT,
    first_seen    TEXT    NOT NULL,
    last_fetched  TEXT,
    last_status   INTEGER,
    etag          TEXT,
    last_modified TEXT,
    content_type  TEXT,
    charset       TEXT,
    raw_hash      TEXT,
    text_hash     TEXT,
    title         TEXT,
    lang          TEXT,
    render_mode   TEXT,
    outcome       TEXT    NOT NULL
);

CREATE INDEX pages_site ON pages (site, last_fetched DESC);

CREATE TABLE fetch_events (
    id           INTEGER PRIMARY KEY,
    page_id      INTEGER NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    started_at   TEXT    NOT NULL,
    duration_ms  INTEGER NOT NULL,
    http_status  INTEGER,
    mode         TEXT    NOT NULL,
    raw_hash     TEXT,
    text_hash    TEXT,
    bytes        INTEGER,
    error_kind   TEXT,
    error_detail TEXT
);

CREATE INDEX fetch_events_page ON fetch_events (page_id, started_at DESC, id DESC);
CREATE INDEX fetch_events_started ON fetch_events (started_at);

CREATE TABLE blobs (
    hash               TEXT    PRIMARY KEY,
    size               INTEGER NOT NULL,
    content_type       TEXT,
    created_at         TEXT    NOT NULL,
    last_referenced_at TEXT    NOT NULL
);

CREATE INDEX blobs_created ON blobs (created_at);

CREATE TABLE profiles (
    hash       TEXT PRIMARY KEY,
    name       TEXT,
    spec_json  TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE extractions (
    id             INTEGER PRIMARY KEY,
    page_id        INTEGER NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    fetch_event_id INTEGER NOT NULL REFERENCES fetch_events (id) ON DELETE CASCADE,
    profile_hash   TEXT,
    result_json    TEXT    NOT NULL,
    result_hash    TEXT    NOT NULL,
    record_count   INTEGER NOT NULL,
    created_at     TEXT    NOT NULL
);

CREATE INDEX extractions_page ON extractions (page_id, created_at DESC);
CREATE INDEX extractions_created ON extractions (created_at);

CREATE TABLE hosts (
    host                 TEXT PRIMARY KEY,
    robots_fetched_at    TEXT,
    robots_status        INTEGER,
    robots_hash          TEXT,
    robots_body          BLOB,
    -- Effective cache expiry: min(24 h, Cache-Control max-age) for a successful
    -- file, 60 min for a failure. 's column list does not name it;
    -- it exists so the stated TTL rule does not need a second cache.
    robots_expires_at    TEXT,
    next_allowed_at      TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
