-- drip SQLite schema, migration 0002 ("profiles").
--
-- Design context: bd issue drip-15n.9.3 (part of epic drip-15n.9,
-- "Migrate drip to SQLite-backed storage"). This resolves a granularity gap
-- left open by migration 0001: `sources` models one row per individual feed
-- (one subreddit), but the CLI-facing concept of a "profile"
-- (`drip profile add`/`--profile <name>`) spans MULTIPLE subreddits under
-- one shared sort/time/query/limit/tags preset. A profile is a different
-- granularity than a source, not a replacement for it, so this migration
-- layers `profiles` on top of the existing `sources` table (rather than
-- replacing it) via two join tables. See drip-15n.9.3's `bd show` design
-- note for the full reasoning.
--
-- Named multi-source fetch presets (`drip fetch --profile <name>`).
-- Supersedes Config.profiles: Vec<Profile> in src/config.rs. Layers on top
-- of the existing `sources` table (drip-15n.9.1/migrations/0001_init.sql)
-- rather than replacing it, since a profile spans multiple sources under one
-- shared preset while a source is one individual feed. See drip-15n.9.3's
-- `bd show` design note for the full reasoning.

CREATE TABLE profiles (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    sort         TEXT CHECK (sort IN ('hot', 'top', 'new', 'rising', 'controversial')),
    time_filter  TEXT CHECK (time_filter IN ('hour', 'day', 'week', 'month', 'year', 'all')),
    query        TEXT,
    fetch_limit  INTEGER,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE profile_sources (
    profile_id  INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    PRIMARY KEY (profile_id, source_id)
);

CREATE TABLE profile_tags (
    profile_id  INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    tag         TEXT NOT NULL,
    PRIMARY KEY (profile_id, tag)
);

PRAGMA user_version = 2;
