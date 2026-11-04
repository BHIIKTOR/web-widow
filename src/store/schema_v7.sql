-- Migration 007_recipes_screenshots.sql — schema v7 (v0.6).
--
-- Additive only: the naming/version layer for extraction recipes and
-- the reference table that keeps screenshot blobs alive. No existing table,
-- column or row is touched, and no tool surface is added.

-- Recipe naming/version layer. The spec itself stays in `profiles`
-- (content-addressed `spec_json`/`hash`); a version row points at the profile
-- hash that a monitor keeps even when the recipe advances.
CREATE TABLE recipes (
    name            TEXT PRIMARY KEY,
    current_version INTEGER NOT NULL CHECK (current_version >= 1),
    description     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE recipe_versions (
    name         TEXT    NOT NULL REFERENCES recipes (name) ON DELETE CASCADE,
    version      INTEGER NOT NULL,
    profile_hash TEXT    NOT NULL REFERENCES profiles (hash),
    created_at   TEXT    NOT NULL,
    PRIMARY KEY (name, version)
);

CREATE INDEX recipe_versions_profile ON recipe_versions (profile_hash);

-- Screenshot references. The CAS has no other reference for an image, so
-- without this table the existing blob GC would delete it. `page_id` is
-- nullable: a session screenshot may be taken before the page is stored, and a
-- page deletion must not delete the image (`ON DELETE SET NULL`).
CREATE TABLE screenshots (
    id          INTEGER PRIMARY KEY,
    page_id     INTEGER REFERENCES pages (id) ON DELETE SET NULL,
    hash        TEXT    NOT NULL REFERENCES blobs (hash) ON DELETE CASCADE,
    format      TEXT    NOT NULL,
    width       INTEGER,
    height      INTEGER,
    -- `screenshot` | `interaction`
    source      TEXT    NOT NULL,
    captured_at TEXT    NOT NULL
);

CREATE INDEX screenshots_page ON screenshots (page_id, captured_at DESC);
CREATE INDEX screenshots_hash ON screenshots (hash);
