-- drip SQLite schema, migration 0003 ("source labels").
--
-- Design context: bd issue drip-15n.9.6. `drip source add --name <label>`
-- needs an unambiguous way to look a source back up by that label for
-- `drip fetch --source <label>` -- this unique index is what makes that
-- lookup safe.
CREATE UNIQUE INDEX idx_sources_display_name ON sources(display_name) WHERE display_name IS NOT NULL;

PRAGMA user_version = 3;
