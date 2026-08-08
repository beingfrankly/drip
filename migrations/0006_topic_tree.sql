-- drip SQLite schema, migration 0006 ("topic tree").
--
-- Design context: bd issue drip-ho5.2 (implementation), resolved shape per
-- bd issue drip-98u.10 (epic drip-98u, "Two-level topics with per-item
-- keyword classification"). Read drip-98u.10's resolution comment for the
-- full reasoning; this header only restates the WHY for the schema itself.
--
-- SHAPE + BACKFILL, both in this one file. This file adds the new
-- columns/tables that a two-level topic tree and per-source keyword
-- classification need, migrates every pre-existing topic/source row into
-- that shape, and bumps `user_version` to 6 -- all inside the single
-- `execute_batch` call `db::run_migrations` makes per migration file, so
-- schema and backfill land as one atomic unit rather than two migrations
-- that could be applied out of step with each other. (An earlier draft of
-- this file split shape from backfill into "a later migration's job" --
-- that plan was abandoned; see drip-ho5.2.)
--
-- Why a two-level tree: drip-98u.7 settled on two-level depth (main topic ->
-- sub-topic), enforced in APPLICATION code rather than the schema -- SQLite
-- cannot express "links may only target non-root topics" without a trigger,
-- the same reasoning already used for "every source has a topic" at
-- migrations/0005_source_topic.sql:15-21.
--
-- Why a many-to-many link table instead of reusing `sources.topic_id`: a
-- source's topic membership used to be a single FK column (migration 0005),
-- but the new model lets one source route into several sub-topics with
-- different keyword rules each -- e.g. r/rust's items might split across
-- "Rust > releases" and "Rust > general" by keyword. That needs its own
-- link row per (source, sub-topic) pair, each carrying its own rules and its
-- own `match_body` opt-in, which a single scalar column cannot represent.
-- `sources.topic_id` cannot be dropped (migrations are additive-only and
-- never edited after shipping, CLAUDE.md) so it is left in place, but the
-- backfill below (step 4) nulls it out on every existing row once that row
-- has an equivalent link row -- see drip-98u.10's resolution, section 3.
-- Not merely "leave it inert" like `topic_sources` before it: its
-- `ON DELETE RESTRICT` FK (migrations/0005_source_topic.sql:27) would
-- otherwise phantom-block deleting an old main topic forever, since a
-- still-populated `topic_id` keeps pointing at it even after every source
-- has a link elsewhere.
--
-- Why flat `link_rules(link_id, role, term)` rows instead of a JSON blob on
-- the link: settled by drip-98u.2's resolution (recorded on drip-98u.10) --
-- flat rows keep individual rules SQL-queryable and editable per-term
-- (add/remove one keyword without rewriting a JSON document), at the cost of
-- one extra join. `role` is constrained to 'include'/'exclude' so a link's
-- rule set can express both "match if title/body contains X" and "but never
-- if it contains Y" without two separate tables.
--
-- Why a purpose-named table for source-level excludes rather than the
-- existing `source_tags` (migrations/0001_init.sql:59-63) or a link_rules
-- row with a NULL link_id: source-level excludes are a TITLE-ONLY
-- pre-filter that applies before an item is ever matched against any link's
-- rules -- a different concept from a link-level exclude (which only
-- applies within one sub-topic's classification, and may check the body via
-- that link's `match_body`). `source_tags` happens to have the right shape
-- (source_id, text) but is semantically a labelling table, never written by
-- any app code today -- overloading it with exclude terms would be
-- confusing per drip-98u.10's resolution. A NULL-link_id row in `link_rules`
-- would force every future query against that table to remember to filter
-- NULLs out or in depending on intent; a separate table makes the two kinds
-- of exclude un-confusable by construction.
--
-- Nullability of `topics.parent_id`: SQLite's `ALTER TABLE ... ADD COLUMN`
-- cannot add a NOT NULL column carrying a REFERENCES clause without a
-- constant default, and a self-referencing FK has no sensible non-NULL
-- default -- the same constraint already documented at
-- migrations/0005_source_topic.sql:15-21 for `sources.topic_id`. NULL means
-- "this topic is a main topic"; a non-NULL value means "this topic is a
-- sub-topic of the topic it points to". `topics.name` stays UNIQUE
-- (migrations/0004_topics.sql:12, retained per drip-98u.1) -- this migration
-- adds parentage, not a namespace change, so sub-topic names stay globally
-- unique.
--
-- FK delete postures, all ON DELETE RESTRICT for topic-side references
-- (defense in depth behind app-level "refuse to remove while non-empty"
-- checks, the same posture migration 0005 established for
-- `sources.topic_id`): a main topic refuses removal while it still has
-- sub-topics (`topics.parent_id`); a sub-topic refuses removal while it
-- still has links (`topic_links.topic_id`). The source-side references
-- (`topic_links.source_id`, `source_excludes.source_id`) are ON DELETE
-- CASCADE, matching migration 0004's `topic_sources.source_id` precedent --
-- deleting a source should clean up its own link/exclude rows, not block
-- the delete. `link_rules.link_id` is also ON DELETE CASCADE: a rule row
-- has no independent meaning once its owning link is gone.

-- Add parentage to topics. NULL = main topic (no parent); non-NULL = a
-- sub-topic of the referenced topic.
ALTER TABLE topics ADD COLUMN parent_id INTEGER REFERENCES topics(id) ON DELETE RESTRICT;

-- Why UNIQUE (source_id, topic_id) on topic_links, UNIQUE (link_id, role,
-- term) on link_rules, and UNIQUE (source_id, term) on source_excludes: the
-- declarative-upsert CLI shape settled on drip-98u.8 (`drip source link`
-- with `--match` REPLACING a link's rule set) assumes there is exactly ONE
-- link per (source, sub-topic) pair to upsert into -- "the link between
-- this source and this sub-topic", not "a link" among several. Without the
-- constraint, a duplicate link row for the same (source, sub-topic) would
-- render the same item twice under one sub-topic's H3 section in the
-- digest, once per matching link. The trade-off this accepts: `match_body`
-- lives on the link row (not the pair), so one source cannot have two
-- differently-configured links into the same sub-topic (e.g. one matching
-- title-only, another also matching body) -- it gets exactly one link, one
-- `match_body` setting, into any given sub-topic. link_rules and
-- source_excludes get the equivalent per-row dedup for the same reason:
-- redundant identical rule/exclude rows are pure noise, never a distinct
-- rule.
--
-- topic_links: the many-to-many that replaces sources.topic_id as the
-- source-of-truth for topic membership going forward (once the backfill
-- below nulls sources.topic_id out). Each row is one source's link into one
-- sub-topic, with `match_body` as a per-link opt-in to matching an Item's
-- summary/body text in addition to its title (drip-98u.2). `match_body` is
-- stored as SQLite's conventional 0/1 integer boolean.
CREATE TABLE topic_links (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    topic_id    INTEGER NOT NULL REFERENCES topics(id) ON DELETE RESTRICT,
    match_body  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (source_id, topic_id)
);

-- link_rules: flat keyword rules attached to a topic_links row. `role`
-- distinguishes an "include" term (matching accepts the item) from an
-- "exclude" term (matching rejects it regardless of any include match). A
-- link with zero rules is "ruleless" -- an empty include list accepts
-- everything, which is what makes the behaviour-preserving backfill below
-- possible (drip-98u.2, drip-98u.10).
CREATE TABLE link_rules (
    id       INTEGER PRIMARY KEY,
    link_id  INTEGER NOT NULL REFERENCES topic_links(id) ON DELETE CASCADE,
    role     TEXT NOT NULL CHECK (role IN ('include', 'exclude')),
    term     TEXT NOT NULL,
    UNIQUE (link_id, role, term)
);

-- source_excludes: source-level, title-only exclude terms -- a pre-filter
-- applied before an item is matched against any of its source's
-- topic_links rules at all. Deliberately its own purpose-named table rather
-- than reusing `source_tags` (a labelling table, never written by any app
-- code) or a NULL-link_id row in `link_rules` (would make every future
-- query on that table have to account for a second, unrelated meaning of a
-- NULL link_id) -- see the header comment above.
CREATE TABLE source_excludes (
    id         INTEGER PRIMARY KEY,
    source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    term       TEXT NOT NULL,
    UNIQUE (source_id, term)
);

-- ---------------------------------------------------------------------------
-- BACKFILL. Migrates every pre-existing topic/source row into the shape
-- above, per drip-98u.10's resolution section 2-4. Ordered as:
--
--   1. Existing topics become main topics -- free, nothing to run: the
--      `ALTER TABLE ... ADD COLUMN parent_id` above already defaulted every
--      existing row's `parent_id` to NULL, and NULL is exactly what "main
--      topic" means.
--   2. Give each pre-existing (main) topic exactly one deterministic
--      `<name> (general)` sub-topic.
--   3. Give each existing source exactly one ruleless link into the
--      sub-topic that belongs to its CURRENT `sources.topic_id` -- must run
--      before step 4, since it is this column that drives the join below.
--   4. Null every `sources.topic_id` -- must run last (see the header
--      comment's "Why a many-to-many link table" section for why nulling,
--      not just leaving inert, is required).
--
-- Step 2's hazard, and how it's avoided: `INSERT INTO topics ... SELECT ...
-- FROM topics` reads from the very table it inserts into, and SQLite's
-- behaviour when a statement's target table also appears in its own SELECT
-- is not something to rely on casually -- naive versions of this pattern
-- can see their own newly-inserted rows mid-statement and cascade. This one
-- is safe by construction rather than by luck: the WHERE clause below only
-- ever matches `parent_id IS NULL` rows, and every row this very statement
-- inserts is given a non-NULL `parent_id` (the original topic's id) as part
-- of the same INSERT -- so even if SQLite's query plan revisits a
-- freshly-inserted row while the statement is still running, that row's
-- `parent_id` is never NULL and therefore can never re-match the filter.
-- Proved by `migration_0006_backfill_creates_main_and_general_sub_topics`
-- in src/db.rs: seeded with exactly 2 pre-existing topics, it asserts
-- exactly 2 (never 4, never more) sub-topics land, and that no sub-topic
-- name doubles up into "... (general) (general)".
INSERT INTO topics (name, parent_id)
SELECT name || ' (general)', id
FROM topics
WHERE parent_id IS NULL;

-- Step 3: one ruleless (accept-all) link per existing source, into the
-- sub-topic created above for that source's CURRENT `sources.topic_id`.
-- Ruleless because an empty include list accepts every item -- this is what
-- makes the migration behaviour-preserving: the first post-upgrade fetch
-- routes exactly what it already routes today, with no invented keyword
-- terms. Deliberately identical in shape to what `drip source add --topic
-- X` creates for a brand-new source (bd issue drip-98u.8) -- migrated and
-- newly-added sources are indistinguishable in the schema afterwards. The
-- join relies on step 2 having created exactly one sub-topic per main
-- topic, so `sub.parent_id = s.topic_id` identifies a single row.
INSERT INTO topic_links (source_id, topic_id)
SELECT s.id, sub.id
FROM sources s
JOIN topics sub ON sub.parent_id = s.topic_id
WHERE s.topic_id IS NOT NULL;

-- Step 4: null every source's old topic_id, now that step 3 has captured
-- that membership as a topic_links row instead. Required, not merely tidy
-- -- see the header comment's discussion of `sources.topic_id`'s
-- ON DELETE RESTRICT FK for why leaving it populated would phantom-block
-- future topic deletion.
UPDATE sources SET topic_id = NULL;

PRAGMA user_version = 6;
