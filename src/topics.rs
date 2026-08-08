//! Topic management: labeled CRUD over `topics`/`topic_sources` (see
//! `migrations/0004_topics.sql`), backing `drip topic add/list/remove` and
//! the source-membership commands under it.
//!
//! Design context: bd issue drip-p6v.5. A topic is deliberately just a named
//! group of sources -- no fetch-param presets (sort/time/query/fetch_limit)
//! and no tags of its own, unlike the old (inert) `migrations/0002_profiles.sql`
//! schema. This module mirrors `src/sources.rs`'s conventions: `anyhow`
//! errors with clear, actionable messages, `Option`-returning lookups for
//! "not found", and `bool`-returning removals where "already gone" is a
//! success state rather than an error.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::classify::{Candidate, Section};
use crate::rules::RuleSet;
use crate::sources::{self, SourceRow};
use crate::types::SourceKind;

/// Parse a `sources.kind` TEXT column value (already read out as a
/// `String`) into a [`SourceKind`], surfacing an unrecognized value as a
/// normal `rusqlite::Error` (rather than panicking) so a row-mapping
/// closure can propagate it via `?` like any other column read. Mirrors
/// `src/sources.rs`'s own private `parse_kind_column` -- duplicated here
/// rather than imported since that one isn't `pub`.
fn parse_kind_column(raw: String) -> rusqlite::Result<SourceKind> {
    SourceKind::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unrecognized sources.kind value '{raw}'"),
            )),
        )
    })
}

/// A topic together with the labels of its member sources, as returned by
/// [`list_topics`] for `drip topic list` to render.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicWithSources {
    pub id: i64,
    pub name: String,
    /// `None` for a main topic; `Some(<main topic's name>)` for a sub-topic
    /// (bd issue drip-ho5.4). Lets a caller render the two-level hierarchy
    /// (main topics with their sub-topics indented/marked beneath them)
    /// straight from [`list_topics`]'s single flat `Vec`, without a second
    /// round-trip to work out which topics are which.
    pub parent_name: Option<String>,
    pub source_labels: Vec<String>,
}

/// Validate + normalize a candidate topic name against bd issue drip-98u.1's
/// deny-list, applied identically at `drip topic add` and `drip topic
/// rename` (bd issue drip-ho5.8). Trims leading/trailing whitespace and
/// returns the trimmed name on success.
///
/// **Rejected characters** (anywhere in the name): `,` `[` `]` `{` `}`,
/// newlines, and other control characters. **Rejected leading character**:
/// any YAML flow-sequence sigil (`&` `*` `!` `%` `@` `` ` ``) or `#`.
/// **Reserved**: `/` (kept illegal now so path addressing stays addable
/// later without colliding with existing names). **Normalized**: leading/
/// trailing whitespace is trimmed; empty (or whitespace-only) after
/// trimming is rejected. Everything else is legal -- `C++`, `Node.js`,
/// `.NET`, and `AI & ML` all pass.
///
/// Two confirmed hazards drove this (drip-98u.1's resolution): the digest
/// note's frontmatter is emitted as an UNQUOTED YAML flow sequence
/// (`digest.rs`'s `topics: [{topics_list}]`), so `,` `[` `]` `{` `}` and a
/// leading sigil would break Obsidian's parsing of it; and `--topic` uses
/// clap's `value_delimiter = ','` (`cli.rs`), so a comma in a topic name is
/// fatal a second, independent way -- it splits into two names before any
/// lookup happens.
pub fn validate_topic_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("topic name must not be empty (after trimming whitespace)");
    }

    const DENY_CHARS: [char; 5] = [',', '[', ']', '{', '}'];
    if let Some(bad) = trimmed
        .chars()
        .find(|c| DENY_CHARS.contains(c) || c.is_control())
    {
        anyhow::bail!(
            "topic name '{trimmed}' contains a disallowed character ({bad:?}); \
             , [ ] {{ }}, newlines, and other control characters are not allowed \
             (the digest note's frontmatter is an unquoted YAML list, and `--topic` \
             splits on commas)"
        );
    }

    if trimmed.contains('/') {
        anyhow::bail!(
            "topic name '{trimmed}' contains '/', which is reserved for future path addressing"
        );
    }

    const YAML_SIGILS: [char; 6] = ['&', '*', '!', '%', '@', '`'];
    let first = trimmed.chars().next().expect("checked non-empty above");
    if YAML_SIGILS.contains(&first) || first == '#' {
        anyhow::bail!(
            "topic name '{trimmed}' cannot start with '{first}' -- YAML flow-sequence sigils \
             are reserved, since the digest note's frontmatter is emitted as an unquoted list"
        );
    }

    Ok(trimmed.to_string())
}

/// Create a new topic named `name`. Returns its `id`.
///
/// `name` is validated + trimmed via [`validate_topic_name`] first (bd issue
/// drip-ho5.8/drip-98u.1). Errors clearly if `name` is already taken
/// (enforced by `topics.name`'s `UNIQUE` constraint), mirroring
/// `sources.rs`'s `map_label_conflict` pattern.
pub fn create_topic(conn: &Connection, name: &str) -> Result<i64> {
    let name = validate_topic_name(name)?;
    conn.execute("INSERT INTO topics (name) VALUES (?1)", params![name])
        .map_err(|err| map_topic_name_conflict(err, &name))?;

    let id: i64 = conn
        .query_row(
            "SELECT id FROM topics WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up topic id for '{name}'"))?;

    Ok(id)
}

/// Map a `rusqlite::Error` from [`create_topic`]'s insert into a clear
/// `anyhow` error when it's the `topics.name` unique constraint firing;
/// pass through any other error via its normal `anyhow` conversion.
fn map_topic_name_conflict(err: rusqlite::Error, name: &str) -> anyhow::Error {
    if err.to_string().contains("UNIQUE constraint failed") {
        return anyhow::anyhow!(
            "a topic named '{name}' already exists (run `drip topic list` to see saved topics)"
        );
    }
    anyhow::Error::new(err).context("failed to create topic")
}

/// Get-or-create a topic named `name`, returning its `id` either way.
///
/// Building block for the "every source belongs to a topic" invariant (bd
/// issue drip-38w.1): the fallback "Uncategorized" topic used by
/// `upsert_reddit_source`'s test fixture goes through this rather than
/// duplicating the get-or-insert logic at each call site. Unlike
/// [`create_topic`], calling this with an already-taken name is NOT an error
/// -- that's the whole point of "get or create". `drip source add` itself
/// does NOT use this (bd issue drip-38w.2): it requires an already-existing
/// topic via [`require_topic_id`], rather than silently creating one.
///
/// `#[cfg(test)]`-only (bd issue drip-38w.2): its sole caller is the
/// test-only `sources::upsert_reddit_source` fixture builder (plus test
/// modules), now that `drip source add` requires an existing topic. Gated to
/// keep it out of release builds, matching `upsert_reddit_source`'s own
/// convention.
#[cfg(test)]
pub fn get_or_create_topic(conn: &Connection, name: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO topics (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        params![name],
    )
    .with_context(|| format!("failed to get-or-create topic '{name}'"))?;

    topic_id_by_name(conn, name)
}

/// **Test-only** (bd issue drip-ho5.4): create a sub-topic named `name`
/// directly under the main topic `parent_id`, bypassing `create_topic`'s
/// still-parent-less public signature (`drip topic add --parent` is bd issue
/// drip-ho5.8's job, not this one's -- that ticket owns the CLI flag, this
/// one only needs a way to build a two-level tree in tests). `pub` at module
/// level (mirroring [`get_or_create_topic`]'s and `sources::upsert_reddit_source`'s
/// own cfg(test) convention) rather than nested inside `mod tests`, so
/// `src/main.rs`'s `handle_topic` end-to-end tests can reach it too via
/// `topics::make_sub_topic` without duplicating raw SQL of their own.
#[cfg(test)]
pub fn make_sub_topic(conn: &Connection, parent_id: i64, name: &str) -> i64 {
    conn.execute(
        "INSERT INTO topics (name, parent_id) VALUES (?1, ?2)",
        params![name, parent_id],
    )
    .expect("insert sub-topic should succeed");
    conn.query_row(
        "SELECT id FROM topics WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )
    .expect("sub-topic lookup should succeed")
}

/// Look up a topic's id by its name, returning a clear error (pointing at
/// `drip topic list`) if no topic has that name.
fn topic_id_by_name(conn: &Connection, topic_name: &str) -> Result<i64> {
    let id = conn.query_row(
        "SELECT id FROM topics WHERE name = ?1",
        params![topic_name],
        |row| row.get(0),
    );

    match id {
        Ok(id) => Ok(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(anyhow::anyhow!(
            "no topic named '{topic_name}' (run `drip topic list`)"
        )),
        Err(err) => Err(err).with_context(|| format!("failed to look up topic '{topic_name}'")),
    }
}

/// Look up a topic's id by its name, for the write paths that assign a
/// source to a topic (`drip source add`/`drip source link`/`drip topic add
/// --parent`, bd issue drip-38w.2/drip-ho5.8). Unlike [`topic_id_by_name`]
/// (whose error points at `drip topic list`, appropriate for a read that
/// just needs the exact name), the fix for a missing topic here is to
/// create it -- so the error instead points at `drip topic add`.
pub fn require_topic_id(conn: &Connection, name: &str) -> Result<i64> {
    let id = conn.query_row(
        "SELECT id FROM topics WHERE name = ?1",
        params![name],
        |row| row.get(0),
    );

    match id {
        Ok(id) => Ok(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(anyhow::anyhow!(
            "no topic named '{name}'; create it first with `drip topic add --name {name}`"
        )),
        Err(err) => Err(err).with_context(|| format!("failed to look up topic '{name}'")),
    }
}

/// Create a sub-topic named `name` directly under the main topic named
/// `parent_name` (bd issue drip-ho5.8: `drip topic add --name <n> --parent
/// <main>`). Returns the new sub-topic's `id`.
///
/// `name` is validated via [`validate_topic_name`], same as [`create_topic`].
/// `parent_name` must already exist -- an unknown parent errors via
/// [`require_topic_id`], pointing at `drip topic add` (create the main topic
/// first). Enforces **exactly two levels** in application code (bd issue
/// drip-98u.7): rejects when `parent_name` is ITSELF a sub-topic (i.e.
/// already has a non-NULL `parent_id`), since parenting under it would
/// create a third level, which the schema doesn't represent and nothing in
/// this codebase (rendering, classification, `--topic` expansion) expects.
pub fn create_sub_topic(conn: &Connection, name: &str, parent_name: &str) -> Result<i64> {
    let name = validate_topic_name(name)?;
    let parent_id = require_topic_id(conn, parent_name)?;

    let parent_parent_id: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM topics WHERE id = ?1",
            params![parent_id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up parentage for topic '{parent_name}'"))?;
    if parent_parent_id.is_some() {
        anyhow::bail!(
            "'{parent_name}' is itself a sub-topic; topics are two levels deep -- pass its own \
             main topic to --parent instead"
        );
    }

    conn.execute(
        "INSERT INTO topics (name, parent_id) VALUES (?1, ?2)",
        params![name, parent_id],
    )
    .map_err(|err| map_topic_name_conflict(err, &name))?;

    conn.query_row(
        "SELECT id FROM topics WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to look up topic id for '{name}'"))
}

/// Look up the id of the topic named `name`, requiring that it be a LEAF
/// SUB-TOPIC (bd issue drip-ho5.8, per drip-98u.7's resolution: "sources may
/// link only to leaf sub-topics"). Used by `drip source add`/`drip source
/// link` to reject linking a source directly to a main topic.
///
/// A missing name errors via [`require_topic_id`] (points at `drip topic
/// add`, since creating it is the fix for a genuinely-missing topic). A main
/// topic (`parent_id IS NULL`) errors with a message pointing at creating a
/// sub-topic under it instead, since that -- not creating another main
/// topic -- is the actionable fix here.
pub fn require_leaf_sub_topic_id(conn: &Connection, name: &str) -> Result<i64> {
    let id = require_topic_id(conn, name)?;

    let parent_id: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM topics WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up parentage for topic '{name}'"))?;

    if parent_id.is_none() {
        anyhow::bail!(
            "'{name}' is a main topic; sources link to sub-topics only -- create one with \
             `drip topic add --name <sub-topic> --parent {name}`"
        );
    }

    Ok(id)
}

/// Rename the topic `old_name` to `new_name` (bd issue drip-ho5.8, per
/// drip-98u.7's resolution: `drip topic rename`). `new_name` is validated +
/// trimmed via [`validate_topic_name`], same deny-list as [`create_topic`].
/// A missing `old_name` errors via [`topic_id_by_name`] (points at `drip
/// topic list`).
///
/// **Future-notes-only**: this updates only the DB row. It never rewrites an
/// already-written digest note -- headings are located by exact full-line
/// equality (`digest.rs`), so a same-day rename means the next fetch cannot
/// find the old heading and appends a fresh one alongside it instead.
/// Confusing but not destructive, and gone by tomorrow -- `src/main.rs`'s
/// `handle_topic` is responsible for checking today's note and printing a
/// warning about this ahead of calling this function, not for refusing the
/// rename outright.
pub fn rename_topic(conn: &Connection, old_name: &str, new_name: &str) -> Result<()> {
    let new_name = validate_topic_name(new_name)?;
    let topic_id = topic_id_by_name(conn, old_name)?;

    conn.execute(
        "UPDATE topics SET name = ?1 WHERE id = ?2",
        params![new_name, topic_id],
    )
    .map_err(|err| map_topic_name_conflict(err, &new_name))?;

    Ok(())
}

/// Reparent the sub-topic `name` to be a child of `new_parent_name` instead
/// of its current main topic (bd issue drip-ho5.8's reparent verb, per
/// drip-98u.7's resolution). Names are globally unique (drip-98u.1) and
/// keyword rules live on a source's `topic_links` row, not on parentage, so
/// this changes only which main topic's H2 `name` renders under from the
/// next digest write onward -- existing notes are untouched (same
/// future-notes-only posture as [`rename_topic`]; `src/main.rs`'s
/// `handle_topic` is responsible for the "today's note already has this
/// heading" warning).
///
/// Errors if `name` isn't itself a sub-topic (a main topic has no parent to
/// move), or if `new_parent_name` isn't itself a main topic (enforces the
/// same two-level depth cap as [`create_sub_topic`]).
pub fn reparent_topic(conn: &Connection, name: &str, new_parent_name: &str) -> Result<()> {
    let topic_id = topic_id_by_name(conn, name)?;
    let current_parent_id: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM topics WHERE id = ?1",
            params![topic_id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up parentage for topic '{name}'"))?;
    if current_parent_id.is_none() {
        anyhow::bail!(
            "'{name}' is a main topic, not a sub-topic; only a sub-topic can be reparented \
             (a main topic has no parent to move)"
        );
    }

    let new_parent_id = require_topic_id(conn, new_parent_name)?;
    let new_parent_parent_id: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM topics WHERE id = ?1",
            params![new_parent_id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up parentage for topic '{new_parent_name}'"))?;
    if new_parent_parent_id.is_some() {
        anyhow::bail!(
            "'{new_parent_name}' is itself a sub-topic; topics are two levels deep -- pass a \
             main topic to reparent under"
        );
    }

    conn.execute(
        "UPDATE topics SET parent_id = ?1 WHERE id = ?2",
        params![new_parent_id, topic_id],
    )
    .with_context(|| format!("failed to reparent topic '{name}'"))?;

    Ok(())
}

/// Count how many sub-topics are directly parented under the topic named
/// `topic_name` (bd issue drip-ho5.4, per drip-98u.7's resolution: backs
/// `drip topic remove`'s "a main topic refuses removal while it has
/// sub-topics" guard). Zero for a childless main topic and always zero for a
/// sub-topic (two-level depth cap -- a sub-topic never has children of its
/// own). Errors clearly if no topic has that name, via [`topic_id_by_name`]
/// -- this is a read, so an unknown name is pointed at `drip topic list`
/// rather than `drip topic add`.
///
/// Replaces the old one-topic-per-source-era `topic_source_count` (bd issue
/// drip-38w.2), which no longer expressed a coherent meaning once membership
/// became many-to-many via `topic_links` (bd issue drip-ho5.3's minimal
/// patch just repointed its query at that table without addressing this).
/// Paired with [`topic_link_count`] below -- see `src/main.rs`'s
/// `handle_topic`'s `Remove` branch for how the two guards combine.
pub fn topic_child_count(conn: &Connection, topic_name: &str) -> Result<i64> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    conn.query_row(
        "SELECT COUNT(*) FROM topics WHERE parent_id = ?1",
        params![topic_id],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to count sub-topics for topic '{topic_name}'"))
}

/// Count how many sources are directly linked (via `topic_links`) to the
/// topic named `topic_name` -- NOT expanded to descendants, unlike
/// [`sources_for_topic`] (bd issue drip-ho5.4, per drip-98u.7's resolution:
/// backs `drip topic remove`'s "a sub-topic refuses removal while it has
/// source links" guard). Also protects a legacy topic created before
/// hierarchy existed (bd issue drip-ho5.8's `topic add --parent`) that still
/// has a direct link from being silently removed out from under its source,
/// since such a topic has zero children and would otherwise sail past
/// [`topic_child_count`]'s guard. Errors clearly if no topic has that name,
/// via [`topic_id_by_name`].
pub fn topic_link_count(conn: &Connection, topic_name: &str) -> Result<i64> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    conn.query_row(
        "SELECT COUNT(*) FROM topic_links WHERE topic_id = ?1",
        params![topic_id],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to count sources for topic '{topic_name}'"))
}

/// Look up a source's row by its `display_name` label, returning a clear
/// error (pointing at `drip source list`) if no source has that label.
fn source_by_label(conn: &Connection, source_label: &str) -> Result<SourceRow> {
    sources::find_by_label(conn, source_label)?
        .ok_or_else(|| anyhow::anyhow!("no source named '{source_label}' (run `drip source list`)"))
}

/// Declaratively (re)configure the link between the source labeled
/// `source_label` and the topic named `topic_name` (bd issue drip-ho5.8:
/// backs `drip source link`, replacing the removed `drip source move` --
/// `topics::move_source_to_topic`/`sources::set_source_topic` expressed
/// one-topic-per-source and no longer make sense now that a source can link
/// into several sub-topics at once).
///
/// `match_terms`/`exclude_terms` REPLACE this link's entire include/exclude
/// lists wholesale, and `match_body` replaces its per-link body-matching
/// flag -- the "declarative upsert" decision on drip-98u.8: re-running this
/// with the same arguments is idempotent and produces identical state, which
/// is what makes a shell script the reproducible way to author a source's
/// rule set. Creates the `topic_links` row if it doesn't exist yet.
///
/// Never touches `seen_items` -- this is plain INSERT/DELETE/UPDATE against
/// `topic_links`/`link_rules`, not a remove-and-re-add of the source itself
/// (drip-98u.8's decisive "editing rules never resets dedup" argument).
///
/// Errors clearly if either the source or the topic doesn't exist -- an
/// unknown topic points at `drip topic add` (via [`require_topic_id`]), an
/// unknown source at `drip source list` (via [`source_by_label`]).
/// Leaf-only enforcement ("sources link to sub-topics only") is NOT done
/// here -- see [`require_leaf_sub_topic_id`], which `src/main.rs`'s
/// `handle_source` calls ahead of this for the real CLI path; this function
/// stays a permissive data-layer primitive so test fixtures elsewhere in
/// this codebase can still link directly into a bare (pre-hierarchy) topic.
pub fn link_source_to_topic(
    conn: &Connection,
    source_label: &str,
    topic_name: &str,
    match_terms: &[String],
    exclude_terms: &[String],
    match_body: bool,
) -> Result<()> {
    let source = source_by_label(conn, source_label)?;
    let topic_id = require_topic_id(conn, topic_name)?;

    conn.execute(
        "INSERT INTO topic_links (source_id, topic_id, match_body) VALUES (?1, ?2, ?3) \
         ON CONFLICT(source_id, topic_id) DO UPDATE SET match_body = excluded.match_body",
        params![source.id, topic_id, match_body as i64],
    )
    .with_context(|| format!("failed to link source '{source_label}' to topic '{topic_name}'"))?;

    let link_id: i64 = conn
        .query_row(
            "SELECT id FROM topic_links WHERE source_id = ?1 AND topic_id = ?2",
            params![source.id, topic_id],
            |row| row.get(0),
        )
        .with_context(|| {
            format!("failed to look up link id for source '{source_label}' / topic '{topic_name}'")
        })?;

    replace_link_rules(conn, link_id, "include", match_terms)?;
    replace_link_rules(conn, link_id, "exclude", exclude_terms)?;

    Ok(())
}

/// Replace every `link_rules` row of `role` ("include"/"exclude") belonging
/// to `link_id` with exactly `terms` -- the declarative-replace half of
/// [`link_source_to_topic`]. A plain DELETE-then-INSERT, per `link_id`+
/// `role` pair, so re-running with the same `terms` is idempotent (the
/// `UNIQUE (link_id, role, term)` constraint would reject a literal
/// re-insert otherwise, but the DELETE ahead of it means there's nothing
/// left to conflict with).
fn replace_link_rules(conn: &Connection, link_id: i64, role: &str, terms: &[String]) -> Result<()> {
    conn.execute(
        "DELETE FROM link_rules WHERE link_id = ?1 AND role = ?2",
        params![link_id, role],
    )
    .with_context(|| format!("failed to clear existing '{role}' rules for link id {link_id}"))?;

    for term in terms {
        conn.execute(
            "INSERT INTO link_rules (link_id, role, term) VALUES (?1, ?2, ?3)",
            params![link_id, role, term],
        )
        .with_context(|| {
            format!("failed to insert '{role}' rule '{term}' for link id {link_id}")
        })?;
    }

    Ok(())
}

/// Remove the link between the source labeled `source_label` and the topic
/// named `topic_name` (bd issue drip-ho5.8: backs `drip source unlink`).
/// `link_rules.link_id` is `ON DELETE CASCADE` (migration 0006), so this
/// also removes every rule that belonged to the link -- there is no
/// independent way to keep a rule around once its link is gone. Returns
/// `true` if a link existed and was removed, `false` if the two were never
/// linked (a benign no-op, matching [`remove_topic`]/`sources::remove_by_label`'s
/// own "already gone is a success state" convention).
///
/// Errors clearly if either the source or the topic doesn't exist -- both
/// point at their respective `list` command, since there's nothing to
/// create here (unlike [`link_source_to_topic`]'s topic side, where an
/// unknown name points at `drip topic add`).
pub fn unlink_source_from_topic(
    conn: &Connection,
    source_label: &str,
    topic_name: &str,
) -> Result<bool> {
    let source = source_by_label(conn, source_label)?;
    let topic_id = topic_id_by_name(conn, topic_name)?;

    let changed = conn
        .execute(
            "DELETE FROM topic_links WHERE source_id = ?1 AND topic_id = ?2",
            params![source.id, topic_id],
        )
        .with_context(|| {
            format!("failed to unlink source '{source_label}' from topic '{topic_name}'")
        })?;

    Ok(changed > 0)
}

/// List every topic, grouped into the two-level tree (bd issue drip-ho5.4,
/// per drip-98u.7's resolution), with the labels of its own directly-linked
/// member sources (ordered by label) for `drip topic list` to render.
///
/// Rendering choice: rather than a nested `Vec<Vec<_>>` (main topics each
/// owning a `Vec` of sub-topics), this returns one flat `Vec` ordered so
/// each main topic is immediately followed by its own sub-topics
/// (alphabetically among themselves), with `parent_name` on each row saying
/// which group it belongs to (`None` for a main topic). A caller renders the
/// tree by indenting/marking any row with `parent_name.is_some()`, without
/// needing a second data shape -- and every existing flat consumer (this
/// module's own tests' `find_topic` helper, `handle_topic`'s `List` branch)
/// keeps working against a plain `Vec` unchanged. A source linked into two
/// different sub-topics appears under each of them (its own row's
/// `source_labels`), never collapsed -- that's what "member sources" means
/// for each individual sub-topic under many-to-many `topic_links`, distinct
/// from [`sources_for_topic`]'s deliberate cross-sub-topic dedup for a
/// *main* topic's fetch expansion.
///
/// Membership is read via `topic_links` (bd issue drip-ho5.3), not the
/// now-dead `sources.topic_id` column (previously bd issue drip-38w.1) or
/// the even-older-inert `topic_sources` join. Unlabeled member sources
/// (there shouldn't be any -- every source that gets linked also went
/// through `sources::upsert_source`/[`link_source_to_topic`], both of which
/// are only ever called with an already-labeled source in this codebase --
/// but defensively) are excluded from `source_labels`, matching `sources::list`'s
/// own `display_name IS NOT NULL` convention.
pub fn list_topics(conn: &Connection) -> Result<Vec<TopicWithSources>> {
    // Ordering: group by each row's main-topic name (a main topic's own
    // group key is its own name, via `COALESCE`), main topic first within
    // its group (`parent_id IS NULL` sorts true-before-false under `DESC`,
    // since SQLite represents booleans as 1/0), then its sub-topics
    // alphabetically.
    let mut topic_stmt = conn
        .prepare(
            "SELECT t.id, t.name, p.name \
             FROM topics t LEFT JOIN topics p ON p.id = t.parent_id \
             ORDER BY COALESCE(p.name, t.name), (t.parent_id IS NULL) DESC, t.name",
        )
        .context("failed to prepare topic list query")?;

    let topics: Vec<(i64, String, Option<String>)> = topic_stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to list topics")?;

    let mut sources_stmt = conn
        .prepare(
            "SELECT s.display_name FROM sources s \
             JOIN topic_links tl ON tl.source_id = s.id \
             WHERE tl.topic_id = ?1 AND s.display_name IS NOT NULL \
             ORDER BY s.display_name",
        )
        .context("failed to prepare topic source labels query")?;

    let mut result = Vec::with_capacity(topics.len());
    for (id, name, parent_name) in topics {
        let source_labels = sources_stmt
            .query_map(params![id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to list member sources for topic '{name}'"))?;
        result.push(TopicWithSources {
            id,
            name,
            parent_name,
            source_labels,
        });
    }

    Ok(result)
}

/// Delete the topic named `name`.
///
/// Two `ON DELETE RESTRICT` FKs (migration 0006, bd issue drip-98u.10) can
/// make this raw `DELETE` fail at the DB layer: `topics.parent_id` blocks
/// deleting a main topic while it still has sub-topics, and
/// `topic_links.topic_id` blocks deleting any topic (main or sub) while it
/// still has a direct source link. (`sources.topic_id`'s own `ON DELETE
/// RESTRICT`, migration 0005, is now moot for this path -- migration 0006
/// nulls that column on every row and nothing repopulates it, so it can
/// never again be the thing blocking a delete.) This function itself does
/// NOT pre-check either condition -- `src/main.rs`'s `handle_topic` does
/// that ahead of calling this, via [`topic_child_count`] and
/// [`topic_link_count`], so it can refuse with a clear, actionable message
/// before ever reaching this raw DB-level `DELETE` and surfacing a raw
/// SQLite FK error instead (bd issue drip-ho5.4, per drip-98u.7's
/// resolution). Returns `true` if an (empty) topic existed and was removed,
/// `false` if no topic had that name.
pub fn remove_topic(conn: &Connection, name: &str) -> Result<bool> {
    let changed = conn
        .execute("DELETE FROM topics WHERE name = ?1", params![name])
        .with_context(|| format!("failed to remove topic '{name}'"))?;
    Ok(changed > 0)
}

/// Resolve the topic named `topic_name` into its full member `SourceRow`s,
/// for `drip fetch --topic` (bd issue drip-p6v.7) to expand into fetchable
/// sources. Errors clearly if the topic name doesn't exist.
///
/// Per drip-98u.7's resolution: naming a **main** topic expands to every
/// source linked into any of its sub-topics (a main owns no sources
/// directly under the intended leaf-only-attachment model, so this is what
/// makes `--topic <main>` fetch anything at all); naming a **sub-topic**
/// returns only its own directly-linked sources. The query below handles
/// both in one shot without needing to know which kind `topic_name` is: it
/// matches links whose `topic_id` is either `topic_name`'s own id (the
/// sub-topic case, and the legacy case of a pre-hierarchy topic that still
/// has a direct link -- e.g. topics created before bd issue drip-ho5.8 adds
/// `topic add --parent`) OR one of its children's ids (the main-topic
/// expansion case; empty for a sub-topic, since two-level depth means a
/// sub-topic has no children of its own). `DISTINCT` collapses a source
/// linked into two sub-topics under the same main down to one row, per
/// drip-98u.7's "results deduplicated" requirement (bd issue drip-ho5.4).
pub fn sources_for_topic(conn: &Connection, topic_name: &str) -> Result<Vec<SourceRow>> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT s.id, s.kind, s.identifier, s.display_name \
             FROM sources s JOIN topic_links tl ON tl.source_id = s.id \
             WHERE tl.topic_id = ?1 \
                OR tl.topic_id IN (SELECT id FROM topics WHERE parent_id = ?1) \
             ORDER BY s.display_name",
        )
        .context("failed to prepare topic member sources query")?;

    let rows = stmt.query_map(params![topic_id], |row| {
        Ok(SourceRow {
            id: row.get(0)?,
            kind: parse_kind_column(row.get(1)?)?,
            identifier: row.get(2)?,
            display_name: row.get(3)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list member sources for topic '{topic_name}'"))
}

/// The set of sub-topic ids "requested" by naming `topic_name` in a `drip
/// fetch --topic` invocation (bd issue drip-98u.3's "candidate set = only the
/// requested sub-topics' rules" decision, implemented by bd issue drip-ho5.6):
/// `topic_name`'s own id, plus every topic directly parented under it. For a
/// sub-topic this is just its own id (a sub-topic has no children under the
/// two-level depth cap); for a main topic it's every one of its sub-topics'
/// ids (plus the main's own id, covering a legacy pre-hierarchy topic that
/// still has a direct link -- see [`sources_for_topic`]'s doc comment for the
/// same case). Deliberately mirrors [`sources_for_topic`]'s own `tl.topic_id
/// = ?1 OR tl.topic_id IN (children)` matching set, so "which sources does
/// `--topic X` fetch" and "which sub-topics does `--topic X` request
/// classification against" never disagree. Errors clearly if no topic has
/// that name.
pub fn requested_sub_topic_ids(conn: &Connection, topic_name: &str) -> Result<Vec<i64>> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    let mut stmt = conn
        .prepare("SELECT id FROM topics WHERE id = ?1 OR parent_id = ?1")
        .context("failed to prepare requested sub-topic ids query")?;

    let ids = stmt
        .query_map(params![topic_id], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list requested sub-topic ids for '{topic_name}'"))?;

    Ok(ids)
}

/// Load `source_id`'s classification [`Candidate`]s from `topic_links`
/// (joined with `link_rules` and the linked topic's own name/parent), for
/// `classify::classify_items` to route the source's freshly-fetched items
/// into `(main topic, sub-topic)` sections (bd issue drip-ho5.6).
///
/// `requested_sub_topic_ids`, when `Some`, restricts the candidate set to
/// only the links whose `topic_id` is in that list -- bd issue drip-98u.3's
/// "candidates are only the REQUESTED sub-topics' rules" decision, which
/// applies when the caller resolved this source via `drip fetch --topic
/// <name>` (see [`requested_sub_topic_ids`]). `None` means no topic scoping
/// was requested (a direct `drip fetch --source`/`--all`), so every one of
/// the source's links is a candidate.
///
/// A linked topic with no parent (`parent_id IS NULL`) is treated as both its
/// own main topic AND its own sub-topic -- the legacy/pre-hierarchy case of a
/// topic linked to directly rather than through a two-level tree (see
/// `sources_for_topic`'s doc comment, and every pre-drip-ho5.8 `drip source
/// add --topic <name>` call, which links straight into a topic with no
/// concept of sub-topics yet).
pub fn candidates_for_source(
    conn: &Connection,
    source_id: i64,
    requested_sub_topic_ids: Option<&[i64]>,
) -> Result<Vec<Candidate>> {
    let mut stmt = conn
        .prepare(
            "SELECT tl.id, tl.topic_id, tl.match_body, t.name, p.name \
             FROM topic_links tl \
             JOIN topics t ON t.id = tl.topic_id \
             LEFT JOIN topics p ON p.id = t.parent_id \
             WHERE tl.source_id = ?1 \
             ORDER BY t.name",
        )
        .context("failed to prepare classification candidates query")?;

    let links: Vec<(i64, i64, bool, String, Option<String>)> = stmt
        .query_map(params![source_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list topic links for source id {source_id}"))?;

    let mut candidates = Vec::with_capacity(links.len());
    for (link_id, topic_id, match_body, sub_topic, parent_name) in links {
        if let Some(ids) = requested_sub_topic_ids {
            if !ids.contains(&topic_id) {
                continue;
            }
        }
        let main_topic = parent_name.unwrap_or_else(|| sub_topic.clone());
        let rules = load_link_rules(conn, link_id)?;
        candidates.push(Candidate {
            section: Section {
                main_topic,
                sub_topic,
            },
            rules,
            match_body,
        });
    }

    Ok(candidates)
}

/// Load one `topic_links` row's include/exclude rules from `link_rules`, for
/// [`candidates_for_source`].
fn load_link_rules(conn: &Connection, link_id: i64) -> Result<RuleSet> {
    let mut stmt = conn
        .prepare("SELECT role, term FROM link_rules WHERE link_id = ?1")
        .context("failed to prepare link rules query")?;

    let rows: Vec<(String, String)> = stmt
        .query_map(params![link_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list rules for link id {link_id}"))?;

    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for (role, term) in rows {
        match role.as_str() {
            "include" => include.push(term),
            "exclude" => exclude.push(term),
            other => {
                // `link_rules.role`'s CHECK constraint (migrations/0006) only
                // allows 'include'/'exclude' -- anything else would mean the
                // DB itself is inconsistent, not a normal runtime condition.
                anyhow::bail!("unrecognized link_rules.role value '{other}' for link id {link_id}")
            }
        }
    }

    Ok(RuleSet { include, exclude })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;
    use crate::sources::upsert_source;

    fn fresh_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = Config {
            db_path: Some(db_path),
            ..Config::default()
        };
        let conn = db::open(&config).expect("db open should succeed");
        (dir, conn)
    }

    /// Create a labeled RSS source directly inside `topic_id`. Takes a
    /// topic to insert into (rather than leaving the source topicless, which
    /// isn't a representable state anymore -- bd issue drip-38w.1) so tests
    /// don't accidentally spawn a stray "Uncategorized" topic that would
    /// reorder `list_topics`' name-sorted output out from under a
    /// positional-index assertion.
    fn make_source(conn: &Connection, topic_id: i64, label: &str) -> i64 {
        upsert_source(
            conn,
            SourceKind::Rss,
            &format!("https://example.com/{label}.xml"),
            Some(label),
            topic_id,
        )
        .expect("upsert_source should succeed")
    }

    /// Look up a topic by name in `list_topics`' output -- for tests where
    /// more than one topic exists, so asserting on it doesn't depend on
    /// `list_topics`' (name-sorted) ordering.
    fn find_topic<'a>(listed: &'a [TopicWithSources], name: &str) -> &'a TopicWithSources {
        listed
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("topic '{name}' not found in list_topics() output"))
    }

    #[test]
    fn create_list_remove_happy_path() {
        let (_dir, conn) = fresh_conn();

        let id = create_topic(&conn, "rust").expect("create_topic should succeed");
        assert!(id > 0);

        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "rust");
        assert!(listed[0].source_labels.is_empty());

        let removed = remove_topic(&conn, "rust").expect("remove_topic should succeed");
        assert!(removed);

        let listed_after = list_topics(&conn).expect("list_topics should succeed");
        assert!(listed_after.is_empty());
    }

    // -- Cycle 1 (bd issue drip-ho5.8, per drip-98u.1's deny-list): topic
    // name validation, applied at `topic add`/`topic rename` via
    // `validate_topic_name`.

    #[test]
    fn validate_topic_name_trims_whitespace() {
        let name = validate_topic_name("  rust  ").expect("should trim and accept");
        assert_eq!(name, "rust");
    }

    #[test]
    fn validate_topic_name_rejects_empty_after_trimming() {
        let err = validate_topic_name("   ").expect_err("whitespace-only name should be rejected");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn validate_topic_name_rejects_a_comma() {
        // The decisive case (drip-98u.1): `--topic` uses clap's
        // `value_delimiter = ','` (cli.rs), so a comma in a topic name is
        // fatal twice over -- once via unquoted YAML frontmatter
        // (digest.rs:253/`topics: [{topics_list}]`), once via `--topic`
        // itself splitting on it before any lookup happens.
        let err = validate_topic_name("Rust, News").expect_err("a comma should be rejected");
        assert!(
            err.to_string().contains(','),
            "error should mention the offending character: {err}"
        );
    }

    #[test]
    fn validate_topic_name_rejects_yaml_flow_sequence_characters() {
        for bad in ["a[b", "a]b", "a{b", "a}b"] {
            validate_topic_name(bad).expect_err(&format!("{bad:?} should be rejected"));
        }
    }

    #[test]
    fn validate_topic_name_rejects_control_characters_and_newlines() {
        validate_topic_name("a\nb").expect_err("a newline should be rejected");
        validate_topic_name("a\tb").expect_err("a control character should be rejected");
    }

    #[test]
    fn validate_topic_name_rejects_a_leading_yaml_sigil() {
        for bad in [
            "&anchor",
            "*alias",
            "!tag",
            "%directive",
            "@flow",
            "`code",
            "#comment",
        ] {
            validate_topic_name(bad).expect_err(&format!(
                "a name starting with a YAML sigil ({bad:?}) should be rejected"
            ));
        }
    }

    #[test]
    fn validate_topic_name_reserves_the_forward_slash() {
        let err = validate_topic_name("AI engineering/news")
            .expect_err("a forward slash should be rejected (reserved for future path addressing)");
        assert!(err.to_string().contains('/'));
    }

    #[test]
    fn validate_topic_name_allows_everything_else() {
        for ok in [
            "C++",
            "Node.js",
            ".NET",
            "AI & ML",
            "rust",
            "AI engineering",
        ] {
            validate_topic_name(ok).unwrap_or_else(|err| panic!("{ok:?} should be legal: {err}"));
        }
    }

    #[test]
    fn create_topic_rejects_an_invalid_name() {
        let (_dir, conn) = fresh_conn();

        let err = create_topic(&conn, "a,b").expect_err("an invalid name should be rejected");
        assert!(err.to_string().contains(','));

        assert!(
            list_topics(&conn).unwrap().is_empty(),
            "a rejected create must not leave a partial row behind"
        );
    }

    #[test]
    fn create_topic_with_taken_name_errors_clearly() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("first create should succeed");
        let err = create_topic(&conn, "rust").expect_err("duplicate name should error");

        let message = err.to_string();
        assert!(
            message.contains("rust"),
            "error should mention the name: {message}"
        );
        assert!(
            message.contains("drip topic list"),
            "error should point users at `drip topic list`: {message}"
        );
    }

    // -- bd issue drip-ho5.8: `link_source_to_topic`/`unlink_source_from_topic`
    // replace `move_source_to_topic`/`sources::set_source_topic` (removed --
    // they expressed one-topic-per-source, which is no longer a thing a
    // source can even be under many-to-many `topic_links`). Reassignment is
    // now unlink-then-link rather than a single wholesale "move".

    #[test]
    fn link_then_unlink_reassigns_a_source_between_topics() {
        let (_dir, conn) = fresh_conn();

        let tid_other = create_topic(&conn, "other").expect("create_topic should succeed");
        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_other, "rust-blog");

        link_source_to_topic(&conn, "rust-blog", "rust", &[], &[], false)
            .expect("link_source_to_topic should succeed");
        let unlinked = unlink_source_from_topic(&conn, "rust-blog", "other")
            .expect("unlink_source_from_topic should succeed");
        assert!(unlinked);

        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert_eq!(
            find_topic(&listed, "rust").source_labels,
            vec!["rust-blog".to_string()]
        );
        assert!(
            find_topic(&listed, "other").source_labels.is_empty(),
            "source should have moved out of its original topic"
        );

        let members = sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].display_name, Some("rust-blog".to_string()));
    }

    #[test]
    fn link_source_to_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();
        let tid_home = create_topic(&conn, "home").expect("create_topic should succeed");
        make_source(&conn, tid_home, "rust-blog");

        let err = link_source_to_topic(&conn, "rust-blog", "does-not-exist", &[], &[], false)
            .expect_err("missing topic should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip topic add"));
    }

    #[test]
    fn link_source_to_topic_errors_clearly_when_source_missing() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "rust").expect("create_topic should succeed");

        let err = link_source_to_topic(&conn, "does-not-exist", "rust", &[], &[], false)
            .expect_err("missing source should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip source list"));
    }

    #[test]
    fn linking_a_source_to_the_same_topic_twice_is_idempotent_not_a_duplicate() {
        let (_dir, conn) = fresh_conn();

        let tid_home = create_topic(&conn, "home").expect("create_topic should succeed");
        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_home, "rust-blog");

        link_source_to_topic(
            &conn,
            "rust-blog",
            "rust",
            &["hook".to_string()],
            &[],
            false,
        )
        .expect("first link should succeed");
        link_source_to_topic(
            &conn,
            "rust-blog",
            "rust",
            &["hook".to_string()],
            &[],
            false,
        )
        .expect("second, identical link should succeed as a no-op");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "one link into 'home' (from make_source) plus one into 'rust' -- no duplicate row \
             for re-running the same 'rust' link twice"
        );

        let found = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .expect("source should exist");
        let listed = sources::list_with_topics(&conn).expect("list_with_topics should succeed");
        let listed_source = listed
            .iter()
            .find(|s| s.source.id == found.id)
            .expect("source should be in list_with_topics() output");
        assert_eq!(
            listed_source.topics,
            vec!["home".to_string(), "rust".to_string()]
        );
    }

    #[test]
    fn link_source_to_topic_replaces_the_include_and_exclude_lists_wholesale() {
        let (_dir, conn) = fresh_conn();
        let tid = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid, "rust-blog");

        link_source_to_topic(
            &conn,
            "rust-blog",
            "rust",
            &["hook".to_string(), "skill".to_string()],
            &["pricing".to_string()],
            false,
        )
        .expect("first link should succeed");

        // Re-running with a DIFFERENT --match list must REPLACE, not append.
        link_source_to_topic(
            &conn,
            "rust-blog",
            "rust",
            &["agent".to_string()],
            &[],
            true,
        )
        .expect("second link should succeed and replace the rule set");

        let source_id = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .unwrap()
            .id;
        let candidates = candidates_for_source(&conn, source_id, None)
            .expect("candidates_for_source should succeed");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].rules.include, vec!["agent".to_string()]);
        assert!(
            candidates[0].rules.exclude.is_empty(),
            "the old exclude term should have been replaced away, not kept alongside the new one"
        );
        assert!(candidates[0].match_body);
    }

    #[test]
    fn unlink_source_from_topic_cascades_its_rules() {
        let (_dir, conn) = fresh_conn();
        let tid = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid, "rust-blog");
        link_source_to_topic(
            &conn,
            "rust-blog",
            "rust",
            &["hook".to_string()],
            &[],
            false,
        )
        .expect("link should succeed");

        let unlinked =
            unlink_source_from_topic(&conn, "rust-blog", "rust").expect("unlink should succeed");
        assert!(unlinked);

        let link_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(link_count, 0);
        let rule_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM link_rules", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rule_count, 0,
            "the link's rules should be cascade-deleted along with it"
        );

        // The source itself, and the topic, must survive.
        assert!(sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .is_some());
        assert!(list_topics(&conn).unwrap().iter().any(|t| t.name == "rust"));
    }

    #[test]
    fn unlink_source_from_topic_is_a_benign_no_op_when_never_linked() {
        let (_dir, conn) = fresh_conn();
        let tid = create_topic(&conn, "rust").expect("create_topic should succeed");
        create_topic(&conn, "other").expect("create_topic should succeed");
        make_source(&conn, tid, "rust-blog");

        let unlinked = unlink_source_from_topic(&conn, "rust-blog", "other")
            .expect("unlinking a never-linked pair should succeed");
        assert!(!unlinked);
    }

    #[test]
    fn unlink_source_from_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();
        let tid = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid, "rust-blog");

        let err = unlink_source_from_topic(&conn, "rust-blog", "does-not-exist")
            .expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn unlink_source_from_topic_errors_clearly_when_source_missing() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "rust").expect("create_topic should succeed");

        let err = unlink_source_from_topic(&conn, "does-not-exist", "rust")
            .expect_err("missing source should error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    // -- Cycle B (bd issue drip-ho5.4): `topic_source_count` replaced by a
    // child-count (backs "a main topic refuses removal while it has
    // sub-topics") and a link-count (backs "a sub-topic refuses removal
    // while it has source links"), per drip-98u.7's resolution.

    #[test]
    fn topic_link_count_reflects_current_direct_membership() {
        let (_dir, conn) = fresh_conn();

        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        create_topic(&conn, "other").expect("create_topic should succeed");
        assert_eq!(
            topic_link_count(&conn, "rust").expect("topic_link_count should succeed"),
            0
        );

        make_source(&conn, tid_rust, "rust-blog");
        assert_eq!(
            topic_link_count(&conn, "rust").expect("topic_link_count should succeed"),
            1
        );
        assert_eq!(
            topic_link_count(&conn, "other").expect("topic_link_count should succeed"),
            0
        );

        unlink_source_from_topic(&conn, "rust-blog", "rust").expect("unlink should succeed");
        link_source_to_topic(&conn, "rust-blog", "other", &[], &[], false)
            .expect("link should succeed");
        assert_eq!(
            topic_link_count(&conn, "rust").expect("topic_link_count should succeed"),
            0
        );
        assert_eq!(
            topic_link_count(&conn, "other").expect("topic_link_count should succeed"),
            1
        );
    }

    #[test]
    fn topic_link_count_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err =
            topic_link_count(&conn, "does-not-exist").expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
        assert!(err.to_string().contains("drip topic list"));
    }

    #[test]
    fn topic_link_count_does_not_expand_to_descendants() {
        // Unlike `sources_for_topic`, `topic_link_count` deliberately does
        // NOT expand a main topic into its sub-topics' links -- the removal
        // guard needs to know whether *this* topic has direct links, not
        // whether its descendants (if any) do, since the descendant guard
        // (`topic_child_count`) already fires first for a main with
        // sub-topics.
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_general = make_sub_topic(&conn, tid_claude, "Claude (general)");
        make_source(&conn, tid_general, "s1");

        assert_eq!(
            topic_link_count(&conn, "Claude").expect("topic_link_count should succeed"),
            0,
            "a main topic's own link count should not include its sub-topics' links"
        );
        assert_eq!(
            topic_link_count(&conn, "Claude (general)").expect("topic_link_count should succeed"),
            1
        );
    }

    #[test]
    fn topic_child_count_reflects_direct_sub_topics_only() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        assert_eq!(
            topic_child_count(&conn, "Claude").expect("topic_child_count should succeed"),
            0,
            "a childless main topic has zero sub-topics"
        );

        make_sub_topic(&conn, tid_claude, "Claude (general)");
        make_sub_topic(&conn, tid_claude, "cc hooks");
        assert_eq!(
            topic_child_count(&conn, "Claude").expect("topic_child_count should succeed"),
            2
        );

        assert_eq!(
            topic_child_count(&conn, "Claude (general)").expect("topic_child_count should succeed"),
            0,
            "a sub-topic never has children of its own (two-level depth cap)"
        );
    }

    #[test]
    fn topic_child_count_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err =
            topic_child_count(&conn, "does-not-exist").expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
        assert!(err.to_string().contains("drip topic list"));
    }

    // -- Cycle 2 (bd issue drip-ho5.8, per drip-98u.7's resolution):
    // `create_sub_topic` (`topic add --parent`) enforces two-level depth in
    // application code; `require_leaf_sub_topic_id` enforces leaf-only
    // source attachment.

    #[test]
    fn create_sub_topic_creates_it_parented_under_the_named_main_topic() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");

        let sub_id = create_sub_topic(&conn, "cc hooks", "Claude")
            .expect("creating a sub-topic under an existing main topic should succeed");

        let listed = list_topics(&conn).expect("list_topics should succeed");
        let sub = find_topic(&listed, "cc hooks");
        assert_eq!(sub.parent_name, Some("Claude".to_string()));
        assert_ne!(sub_id, tid_claude);
    }

    #[test]
    fn create_sub_topic_validates_its_own_name() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "Claude").expect("create_topic should succeed");

        let err = create_sub_topic(&conn, "a,b", "Claude")
            .expect_err("an invalid sub-topic name should be rejected");
        assert!(err.to_string().contains(','));
    }

    #[test]
    fn create_sub_topic_errors_clearly_when_parent_missing() {
        let (_dir, conn) = fresh_conn();

        let err = create_sub_topic(&conn, "cc hooks", "does-not-exist")
            .expect_err("a missing parent topic should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip topic add"));
    }

    #[test]
    fn create_sub_topic_rejects_a_parent_that_is_itself_a_sub_topic() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        make_sub_topic(&conn, tid_claude, "Claude (general)");

        let err = create_sub_topic(&conn, "third level", "Claude (general)").expect_err(
            "naming a sub-topic as the --parent should be rejected -- topics are two levels deep",
        );
        let message = err.to_string();
        assert!(
            message.contains("Claude (general)"),
            "error should name the offending parent: {message}"
        );
        assert!(
            message.to_lowercase().contains("two level")
                || message.to_lowercase().contains("sub-topic"),
            "error should explain the two-level depth cap: {message}"
        );
    }

    #[test]
    fn require_leaf_sub_topic_id_accepts_a_sub_topic() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_sub = make_sub_topic(&conn, tid_claude, "cc hooks");

        let id = require_leaf_sub_topic_id(&conn, "cc hooks")
            .expect("a genuine sub-topic should be accepted");
        assert_eq!(id, tid_sub);
    }

    #[test]
    fn require_leaf_sub_topic_id_rejects_a_main_topic() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "Claude").expect("create_topic should succeed");

        let err = require_leaf_sub_topic_id(&conn, "Claude")
            .expect_err("a main topic should be rejected as a link target");
        let message = err.to_string();
        assert!(message.contains("Claude"));
        assert!(
            message.contains("drip topic add"),
            "error should point at creating a sub-topic: {message}"
        );
    }

    #[test]
    fn require_leaf_sub_topic_id_errors_clearly_when_missing() {
        let (_dir, conn) = fresh_conn();

        let err = require_leaf_sub_topic_id(&conn, "does-not-exist")
            .expect_err("a missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    // -- Cycle 3 (bd issue drip-ho5.8, per drip-98u.7's resolution): `drip
    // topic rename`/the reparent verb, both re-validating against
    // drip-98u.1's deny-list.

    #[test]
    fn rename_topic_updates_the_name_and_preserves_membership() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_sub = make_sub_topic(&conn, tid_claude, "loop engineering");
        make_source(&conn, tid_sub, "s1");

        rename_topic(&conn, "loop engineering", "agent loops").expect("rename should succeed");

        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert!(
            !listed.iter().any(|t| t.name == "loop engineering"),
            "the old name should no longer exist"
        );
        let renamed = find_topic(&listed, "agent loops");
        assert_eq!(renamed.parent_name, Some("Claude".to_string()));
        assert_eq!(renamed.source_labels, vec!["s1".to_string()]);
    }

    #[test]
    fn rename_topic_validates_the_new_name() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "Claude").expect("create_topic should succeed");

        let err = rename_topic(&conn, "Claude", "a,b")
            .expect_err("an invalid new name should be rejected");
        assert!(err.to_string().contains(','));

        // Nothing should have been renamed.
        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert!(listed.iter().any(|t| t.name == "Claude"));
    }

    #[test]
    fn rename_topic_errors_clearly_when_old_name_missing() {
        let (_dir, conn) = fresh_conn();

        let err = rename_topic(&conn, "does-not-exist", "new-name")
            .expect_err("a missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn reparent_topic_moves_a_sub_topic_under_a_different_main() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_rust = create_topic(&conn, "Rust").expect("create_topic should succeed");
        make_sub_topic(&conn, tid_claude, "cc hooks");

        reparent_topic(&conn, "cc hooks", "Rust").expect("reparent should succeed");

        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert_eq!(
            find_topic(&listed, "cc hooks").parent_name,
            Some("Rust".to_string())
        );
        assert_eq!(topic_child_count(&conn, "Claude").unwrap(), 0);
        assert_eq!(topic_child_count(&conn, "Rust").unwrap(), 1);
        let _ = tid_rust;
    }

    #[test]
    fn reparent_topic_rejects_a_main_topic_as_the_thing_being_moved() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "Claude").expect("create_topic should succeed");
        create_topic(&conn, "Rust").expect("create_topic should succeed");

        let err = reparent_topic(&conn, "Claude", "Rust")
            .expect_err("a main topic has no parent to move");
        assert!(err.to_string().contains("Claude"));
    }

    #[test]
    fn reparent_topic_rejects_a_new_parent_that_is_itself_a_sub_topic() {
        let (_dir, conn) = fresh_conn();
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_rust = create_topic(&conn, "Rust").expect("create_topic should succeed");
        make_sub_topic(&conn, tid_claude, "cc hooks");
        make_sub_topic(&conn, tid_rust, "rust news");

        let err = reparent_topic(&conn, "cc hooks", "rust news")
            .expect_err("reparenting under a sub-topic should be rejected -- two levels only");
        assert!(err.to_string().contains("rust news"));
    }

    #[test]
    fn require_topic_id_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err =
            require_topic_id(&conn, "does-not-exist").expect_err("missing topic should error");
        let message = err.to_string();
        assert!(
            message.contains("does-not-exist"),
            "error should mention the name: {message}"
        );
        assert!(
            message.contains("drip topic add"),
            "error should point users at `drip topic add`: {message}"
        );
    }

    #[test]
    fn remove_topic_fails_via_fk_restrict_while_it_still_owns_a_source() {
        // Pins the raw DB-level behavior `remove_topic` relies on
        // `src/main.rs`'s `handle_topic` to guard ahead of time (bd issue
        // drip-ho5.4). The actual blocking FK today is `topic_links.topic_id`
        // `ON DELETE RESTRICT` (migration 0006) -- membership lives in
        // `topic_links` now, not `sources.topic_id` (that column is nulled
        // by migration 0006's backfill and never repopulated, so its own
        // `ON DELETE RESTRICT` from migration 0005 is moot here; this test's
        // comment used to attribute the failure to that dead column, back
        // when it was still bd issue drip-38w.1's one-topic-per-source
        // model). `handle_topic` guards against this via [`topic_link_count`]
        // (and, for a main topic with sub-topics, [`topic_child_count`]),
        // but this test still pins today's raw DB-level behavior: an `Err`,
        // not a panic and not a silent success.
        let (_dir, conn) = fresh_conn();
        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_rust, "rust-blog");

        remove_topic(&conn, "rust")
            .expect_err("removing a topic that still owns a source should fail (FK RESTRICT)");

        // Once the source is moved elsewhere (unlink-then-link, since a
        // wholesale "move" primitive no longer exists under many-to-many
        // links), the now-empty topic can be removed, and the source itself
        // survives (removing a topic never deletes the sources that were in
        // it).
        create_topic(&conn, "other").expect("create_topic should succeed");
        unlink_source_from_topic(&conn, "rust-blog", "rust")
            .expect("unlinking the source from 'rust' should succeed");
        link_source_to_topic(&conn, "rust-blog", "other", &[], &[], false)
            .expect("linking the source into 'other' should succeed");
        let removed = remove_topic(&conn, "rust").expect("removing an empty topic should succeed");
        assert!(removed);

        let still_exists = sources::find_by_label(&conn, "rust-blog")
            .expect("find_by_label should succeed")
            .expect("source should still exist after its (now-empty) topic is removed");

        // `SourceRow` no longer carries topic membership (bd issue
        // drip-ho5.3) -- assert via `list_with_topics` instead.
        let listed = sources::list_with_topics(&conn).expect("list_with_topics should succeed");
        let listed_source = listed
            .iter()
            .find(|s| s.source.id == still_exists.id)
            .expect("source should be in list_with_topics() output");
        assert_eq!(listed_source.topics, vec!["other".to_string()]);
    }

    #[test]
    fn remove_topic_returns_false_for_unknown_name_without_side_effects() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "rust").expect("create_topic should succeed");

        let removed = remove_topic(&conn, "does-not-exist").expect("remove_topic should succeed");
        assert!(!removed);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM topics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "removing an unknown topic must not touch existing rows"
        );
    }

    #[test]
    fn removing_a_source_removes_it_from_sources_for_topic() {
        let (_dir, conn) = fresh_conn();

        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_rust, "rust-blog");

        let members_before =
            sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert_eq!(members_before.len(), 1);

        sources::remove_by_label(&conn, "rust-blog").expect("remove_by_label should succeed");

        let members_after =
            sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert!(members_after.is_empty());
    }

    #[test]
    fn sources_for_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err =
            sources_for_topic(&conn, "does-not-exist").expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
        assert!(err.to_string().contains("drip topic list"));
    }

    // -- Cycle A (bd issue drip-ho5.4): `sources_for_topic` expands a MAIN
    // topic to all its leaf descendants (drip-98u.7's resolution), while a
    // sub-topic still returns only its own directly-linked sources.

    #[test]
    fn sources_for_topic_expands_a_main_topic_to_all_its_sub_topics() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_general = make_sub_topic(&conn, tid_claude, "Claude (general)");
        let tid_hooks = make_sub_topic(&conn, tid_claude, "cc hooks");

        make_source(&conn, tid_general, "s1");
        make_source(&conn, tid_hooks, "s2");

        let members = sources_for_topic(&conn, "Claude")
            .expect("sources_for_topic on a main topic should succeed");
        let labels: Vec<String> = members
            .iter()
            .map(|s| s.display_name.clone().unwrap())
            .collect();
        assert_eq!(
            labels,
            vec!["s1".to_string(), "s2".to_string()],
            "naming the main topic should return sources from every sub-topic beneath it"
        );
    }

    #[test]
    fn sources_for_topic_on_a_sub_topic_returns_only_its_own_directly_linked_sources() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_general = make_sub_topic(&conn, tid_claude, "Claude (general)");
        let tid_hooks = make_sub_topic(&conn, tid_claude, "cc hooks");

        make_source(&conn, tid_general, "s1");
        make_source(&conn, tid_hooks, "s2");

        let members = sources_for_topic(&conn, "Claude (general)")
            .expect("sources_for_topic on a sub-topic should succeed");
        assert_eq!(
            members.len(),
            1,
            "naming a sub-topic should return only its own linked sources"
        );
        assert_eq!(members[0].display_name, Some("s1".to_string()));
    }

    #[test]
    fn sources_for_topic_dedupes_a_source_linked_into_two_sub_topics_under_the_same_main() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_general = make_sub_topic(&conn, tid_claude, "Claude (general)");
        let tid_hooks = make_sub_topic(&conn, tid_claude, "cc hooks");

        let s1 = make_source(&conn, tid_general, "s1");
        // Link the same source into the second sub-topic too, exercising the
        // many-to-many shape `topic_links` exists for.
        conn.execute(
            "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
            params![s1, tid_hooks],
        )
        .expect("second link insert should succeed");

        let members = sources_for_topic(&conn, "Claude")
            .expect("sources_for_topic on the main topic should succeed");
        assert_eq!(
            members.len(),
            1,
            "a source linked to two sub-topics under the same main must appear once, not twice"
        );
        assert_eq!(members[0].display_name, Some("s1".to_string()));
    }

    // -- Cycle C (bd issue drip-ho5.4): `list_topics` under many-to-many --
    // a source linked into two sub-topics must show up under both, and the
    // returned rows carry enough shape (`parent_name`, grouped ordering) for
    // `drip topic list` to render the two-level tree rather than a
    // now-misleading flat list.

    #[test]
    fn list_topics_shows_a_source_linked_into_two_sub_topics_under_both() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        let tid_general = make_sub_topic(&conn, tid_claude, "Claude (general)");
        let tid_hooks = make_sub_topic(&conn, tid_claude, "cc hooks");

        let shared_id = make_source(&conn, tid_general, "shared-source");
        conn.execute(
            "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
            params![shared_id, tid_hooks],
        )
        .expect("second link insert should succeed");

        let listed = list_topics(&conn).expect("list_topics should succeed");

        assert_eq!(
            find_topic(&listed, "Claude (general)").source_labels,
            vec!["shared-source".to_string()],
            "a source linked into two sub-topics should be listed under the first"
        );
        assert_eq!(
            find_topic(&listed, "cc hooks").source_labels,
            vec!["shared-source".to_string()],
            "a source linked into two sub-topics should be listed under the second too"
        );
    }

    #[test]
    fn list_topics_marks_sub_topics_with_their_parent_name() {
        let (_dir, conn) = fresh_conn();

        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        create_topic(&conn, "other").expect("create_topic should succeed");
        make_sub_topic(&conn, tid_claude, "Claude (general)");
        make_sub_topic(&conn, tid_claude, "cc hooks");

        let listed = list_topics(&conn).expect("list_topics should succeed");

        assert_eq!(
            find_topic(&listed, "Claude").parent_name,
            None,
            "a main topic has no parent"
        );
        assert_eq!(
            find_topic(&listed, "other").parent_name,
            None,
            "a childless main topic still has no parent"
        );
        assert_eq!(
            find_topic(&listed, "Claude (general)").parent_name,
            Some("Claude".to_string())
        );
        assert_eq!(
            find_topic(&listed, "cc hooks").parent_name,
            Some("Claude".to_string())
        );
    }

    #[test]
    fn list_topics_groups_each_main_topic_with_its_sub_topics_in_order() {
        let (_dir, conn) = fresh_conn();

        // Deliberately created out of the eventual display order, so this
        // test can't pass by accident of insertion order.
        let tid_claude = create_topic(&conn, "Claude").expect("create_topic should succeed");
        create_topic(&conn, "Another").expect("create_topic should succeed");
        make_sub_topic(&conn, tid_claude, "cc hooks");
        make_sub_topic(&conn, tid_claude, "Claude (general)");

        let listed = list_topics(&conn).expect("list_topics should succeed");
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["Another", "Claude", "Claude (general)", "cc hooks"],
            "topics should be grouped by main topic (alphabetically), each main followed by \
             its own sub-topics (also alphabetically) -- not a flat name sort, which would \
             interleave 'Another' between 'Claude' and its sub-topics"
        );
    }
}
