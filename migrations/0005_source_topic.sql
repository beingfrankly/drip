-- drip SQLite schema, migration 0005 ("source topic").
--
-- Design context: bd issue drip-38w.1 (epic drip-38w, "Topic-structured
-- digests"). Every source now belongs to EXACTLY ONE topic, replacing the
-- many-to-many `topic_sources` model introduced in migration 0004. Digests
-- render topic (H2) -> source (H3) -> items, so each source needs a single
-- owning topic to render under.
--
-- `topic_sources` is intentionally NOT dropped -- migrations are additive-only
-- (the same convention that left migration 0002's profiles tables in place;
-- see CLAUDE.md). It goes INERT: after this migration nothing reads or writes
-- it, and `sources.topic_id` is the single source of truth for topic
-- membership from here on. The backfill below reads it one last time.
--
-- Nullability: SQLite's `ALTER TABLE ... ADD COLUMN` cannot add a NOT NULL
-- column without a constant default, and a column carrying a REFERENCES clause
-- must default to NULL. So `topic_id` is added nullable at the SQL level; the
-- "every source has a topic" invariant is enforced in application code
-- (src/sources.rs) on insert/move, and the backfill below leaves no NULLs
-- behind for any existing row. (Mind the `fetch_limit`-not-`limit` reserved-word
-- gotcha noted in migration 0001 / CLAUDE.md when touching this schema.)

-- Add the owning-topic FK column. ON DELETE RESTRICT so the database itself
-- refuses to delete a topic that still owns sources -- defense in depth behind
-- the app-level "refuse to remove a non-empty topic" check added in bd issue
-- drip-38w.2.
ALTER TABLE sources ADD COLUMN topic_id INTEGER REFERENCES topics(id) ON DELETE RESTRICT;

-- Backfill 1: every source already grouped under a topic (via the legacy
-- topic_sources join) adopts that topic. A source that was in more than one
-- topic collapses to its lowest topic id -- deterministic; the user can
-- re-organise later if they want a different owner.
UPDATE sources
SET topic_id = (
    SELECT MIN(ts.topic_id) FROM topic_sources ts WHERE ts.source_id = sources.id
)
WHERE id IN (SELECT source_id FROM topic_sources);

-- Backfill 2: any source still without a topic (never grouped -- e.g. a source
-- added before topics existed) goes into a catch-all "Uncategorized" topic, so
-- the "every source has a topic" invariant holds for every existing row.
-- Create the topic only if such sources exist, and reuse it if it somehow
-- already exists. On a fresh (sourceless) database this creates nothing.
INSERT INTO topics (name)
SELECT 'Uncategorized'
WHERE EXISTS (SELECT 1 FROM sources WHERE topic_id IS NULL)
  AND NOT EXISTS (SELECT 1 FROM topics WHERE name = 'Uncategorized');

UPDATE sources
SET topic_id = (SELECT id FROM topics WHERE name = 'Uncategorized')
WHERE topic_id IS NULL;

PRAGMA user_version = 5;
