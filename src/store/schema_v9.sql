-- Migration 009_auth_audit.sql — schema v9 (v0.8).
--
-- Additive only: one value-free audit table and its two indexes. No existing
-- table, column or row is touched, no tool surface or behaviour is added (the
-- table stays empty until an authenticated fetch writes one row per attempt).
--
-- The row carries no reference, header value, cookie, URL path, query, body or
-- client identifier: `site` is the configured profile name and `host` the
-- canonical destination host.

CREATE TABLE auth_audit (
    id            INTEGER PRIMARY KEY,
    at            TEXT    NOT NULL,   -- RFC3339 UTC
    site          TEXT    NOT NULL,   -- configured profile name
    auth_id       TEXT    NOT NULL,   -- binding id (host) when a profile has several
    method        TEXT    NOT NULL,   -- GET | HEAD
    host          TEXT    NOT NULL,   -- canonical host, no path/query
    status        INTEGER,            -- HTTP status when a response existed
    bytes         INTEGER,            -- response body bytes observed
    outcome       TEXT    NOT NULL,   -- ok | auth_rejected | auth_redirect_blocked |
                                      -- auth_secret_unavailable | auth_manager_timeout |
                                      -- auth_insecure_url | auth_host_not_allowed |
                                      -- auth_profile_unknown | robots_blocked | transport_error
    resolution_ms INTEGER NOT NULL
);
CREATE INDEX auth_audit_at ON auth_audit (at);
CREATE INDEX auth_audit_site_at ON auth_audit (site, at);
