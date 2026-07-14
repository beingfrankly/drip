-- drip SQLite schema, migration 0004 ("topics").
--
-- Design context: bd issue drip-p6v.2. A topic is a named group of sources
-- (e.g. "rust", "news") -- a deliberately leaner rehydration of the shape
-- `migrations/0002_profiles.sql` used to have (that file is inert dead
-- schema, not reused here). Unlike the old profiles table, a topic carries
-- no fetch-param presets of its own (no sort/time_filter/query/fetch_limit)
-- and no tags join table -- those already live on `sources`/`source_tags`.
-- `topics` is just a name; `topic_sources` is just the join.
CREATE TABLE topics (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- topic_sources: join table between topics and sources. Pure join, no extra
-- columns -- membership either exists or it doesn't.
CREATE TABLE topic_sources (
    topic_id   INTEGER NOT NULL REFERENCES topics(id) ON DELETE CASCADE,
    source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    PRIMARY KEY (topic_id, source_id)
);

PRAGMA user_version = 4;
