-- Migration 004_monitor_render.sql — schema v4 (repair).
--
-- A monitor check's render mode is part of its result: the 304 short-circuit is
-- only sound when the previous check was static, because a rendered check
-- leaves the origin's *static* validator on `pages` while its snapshot content
-- is the serialized DOM. Pre-v4 snapshots have an unknown mode (NULL), so the
-- first post-upgrade check refetches unconditionally instead of risking a
-- missed script-driven change: NULL is unknown, never "static".
ALTER TABLE snapshots ADD COLUMN render_mode TEXT;
