-- drip SQLite schema, migration 0001 ("init").
--
-- Design context: bd issue drip-15n.9.1 (part of epic drip-15n.9,
-- "Migrate drip to SQLite-backed storage"). This file is the schema
-- deliverable for that issue; it is not yet wired into the app -- that is
-- drip-15n.9.2 (add rusqlite + DB init/migration bootstrap), which should
-- embed and execute this file verbatim against a fresh database.
--
-- Versioning: `PRAGMA user_version` is used as the migration marker per
-- drip-15n.9.2's description. This file brings a fresh DB from
-- user_version 0 to user_version 1.
--
-- Runtime note for whoever wires this up (drip-15n.9.2): SQLite does not
-- persist `PRAGMA foreign_keys` in the database file -- it must be turned ON
-- by the application on every connection (e.g. immediately after `rusqlite::
-- Connection::open`) for the `ON DELETE CASCADE` clauses below to actually
-- take effect. Without it, foreign keys are declared but silently
-- unenforced.

-- ---------------------------------------------------------------------------
-- sources: one row per subscribed source (subreddit today; RSS feed /
-- YouTube channel later). Supersedes Config.profiles: Vec<Profile> in
-- src/config.rs.
--
-- `kind` intentionally allows 'rss' and 'youtube' now even though nothing
-- produces them yet, so adding those source types later doesn't require a
-- breaking schema migration -- just new code paths that INSERT rows with
-- those kinds.
--
-- reddit-specific fetch params (`sort`, `time_filter`, `query`,
-- `fetch_limit`) are nullable because they don't apply to every kind (an RSS
-- feed has no "sort"). `fetch_limit` is deliberately not named `limit`:
-- confirmed by hand against sqlite3 3.53.3 that an unquoted `limit` column
-- name is a hard parse error in CREATE TABLE / INSERT / SELECT ("near
-- "limit": syntax error"), because SQLite treats LIMIT as a reserved
-- keyword in unquoted-identifier positions. It would still work if always
-- quoted (e.g. `"limit"`), but that forces every future reference to
-- remember to quote it, so `fetch_limit` was chosen instead, consistent
-- with the issue's suggested naming.
CREATE TABLE sources (
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('reddit', 'rss', 'youtube')),
    identifier    TEXT NOT NULL,
    display_name  TEXT,
    sort          TEXT CHECK (sort IN ('hot', 'top', 'new', 'rising', 'controversial')),
    time_filter   TEXT CHECK (time_filter IN ('hour', 'day', 'week', 'month', 'year', 'all')),
    query         TEXT,
    fetch_limit   INTEGER,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (kind, identifier)
);

-- source_tags: normalized per-source tags (was Profile.tags: Vec<String>).
-- Normalized rather than a JSON/comma text column on `sources` because tags
-- are per-row and queryable ("give me all sources tagged X") -- exactly the
-- kind of relational structure this migration exists to introduce. Compare
-- with `settings.default_tags` below, which stays JSON-in-text because it is
-- a single global list, not a per-row set worth a join.
CREATE TABLE source_tags (
    source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    tag        TEXT NOT NULL,
    PRIMARY KEY (source_id, tag)
);

-- ---------------------------------------------------------------------------
-- seen_items: dedup ledger. Dedup scope is PER-SOURCE by design (drip-
-- 15n.9.4) -- a crosspost of the same Reddit post into two different
-- subreddits is two distinct rows here, since it's genuinely different
-- content in a different community context. The UNIQUE(source_id,
-- external_id) constraint below IS the dedup mechanism: a later
-- `INSERT OR IGNORE` against this table is how drip-15n.9.4 will implement
-- dedup without a separate existence check.
CREATE TABLE seen_items (
    id             INTEGER PRIMARY KEY,
    source_id      INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    external_id    TEXT NOT NULL,
    title          TEXT,
    url            TEXT,
    first_seen_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (source_id, external_id)
);

-- ---------------------------------------------------------------------------
-- fetch_runs: history/audit log of each `drip fetch` invocation
-- (drip-15n.9.5). `digest_note_path` is nullable because a `--dry-run` fetch
-- produces no file. `post_count` is the POST-DEDUP count -- how many NEW
-- items actually ended up in the digest, not the raw fetch count.
CREATE TABLE fetch_runs (
    id                 INTEGER PRIMARY KEY,
    started_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    digest_note_path   TEXT,
    post_count         INTEGER NOT NULL DEFAULT 0
);

-- fetch_run_sources: join table so a run's per-source breakdown is
-- queryable, not just the run's single aggregate `post_count`.
CREATE TABLE fetch_run_sources (
    fetch_run_id  INTEGER NOT NULL REFERENCES fetch_runs(id) ON DELETE CASCADE,
    source_id     INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    item_count    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (fetch_run_id, source_id)
);

-- ---------------------------------------------------------------------------
-- settings: simple key-value store for the non-bootstrap fields that used to
-- live in Config (posts_folder, daily_notes_folder, daily_note_format,
-- default_sort, default_limit, default_tags). See the bd issue's --design
-- note for the full reasoning on why `vault_path` (and an optional
-- `db_path` override) stay in config.toml instead of moving here.
--
-- List-valued settings (e.g. default_tags) are stored as a JSON-encoded
-- array in `value` -- SQLite has no native array type, and a single global
-- default list doesn't warrant a normalized table the way per-source tags
-- do (see `source_tags` above).
CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

PRAGMA user_version = 1;
