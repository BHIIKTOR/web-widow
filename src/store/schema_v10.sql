-- Migration 010_authenticated_pages.sql — schema v10 ( B5b).
--
-- Additive only: two page labels and their index. No existing column, row or
-- value is rewritten; `authenticated` defaults to 0 so every pre-v10 page stays
-- unauthenticated by construction, and the upgrade never changes a count.
--
-- `auth_site` is the configured profile name (the same value as `auth_audit.site`),
-- never a reference, cookie or value.

ALTER TABLE pages ADD COLUMN authenticated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pages ADD COLUMN auth_site TEXT;

CREATE INDEX pages_authenticated ON pages (authenticated, id);
