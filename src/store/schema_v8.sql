-- Migration 008_embeddings_webhooks.sql — schema v8 (semantic events).
--
-- Additive only: embedding rows + per-page state, monitor→target
-- attachments, and the durable webhook delivery outbox. No existing table,
-- column or row is touched, and no tool surface or behaviour is added by P0.

-- One row per (page, model, chunk). Vectors are f32 little-endian blobs; `dim`
-- is validated so a read can reject a truncated/corrupt blob without guessing.
CREATE TABLE embeddings (
    page_id     INTEGER NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    model       TEXT    NOT NULL,
    chunk_index INTEGER NOT NULL,
    source_hash TEXT    NOT NULL,   -- BLAKE3 of the full page source text used
    chunk_hash  TEXT    NOT NULL,   -- BLAKE3 of this chunk's text
    start_char  INTEGER NOT NULL,
    end_char    INTEGER NOT NULL,
    dim         INTEGER NOT NULL CHECK (dim > 0 AND dim <= 4096),
    vector      BLOB    NOT NULL,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (page_id, model, chunk_index),
    CHECK (end_char > start_char),
    CHECK (length(vector) = dim * 4)
);
CREATE INDEX embeddings_model_page ON embeddings (model, page_id);

-- One row per embedded page for a model: the source text identity and chunk
-- count, so the backfill can skip unchanged pages and prune stale chunks.
CREATE TABLE embedding_state (
    page_id     INTEGER PRIMARY KEY REFERENCES pages (id) ON DELETE CASCADE,
    model       TEXT    NOT NULL,
    source_hash TEXT    NOT NULL,
    chunks      INTEGER NOT NULL CHECK (chunks >= 0),
    embedded_at TEXT    NOT NULL
);
CREATE INDEX embedding_state_model ON embedding_state (model);

-- A monitor may be attached to zero or more operator-named targets. The URL and
-- secret live only in the operator's config file; the DB stores the name.
CREATE TABLE monitor_webhooks (
    monitor_id INTEGER NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
    target     TEXT    NOT NULL,
    created_at TEXT    NOT NULL,
    PRIMARY KEY (monitor_id, target)
);

-- Durable delivery outbox. Enqueued inside the check transaction; attempted by
-- the dispatcher, never by the check path.
CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY,
    change_id       INTEGER NOT NULL REFERENCES changes (id) ON DELETE CASCADE,
    monitor_id      INTEGER NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
    target          TEXT    NOT NULL,
    status          TEXT    NOT NULL CHECK (status IN ('pending', 'delivered', 'dead')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TEXT    NOT NULL,
    last_status     INTEGER,
    last_error      TEXT,
    created_at      TEXT    NOT NULL,
    delivered_at    TEXT,
    UNIQUE (change_id, target)
);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (status, next_attempt_at);
CREATE INDEX webhook_deliveries_change ON webhook_deliveries (change_id);
