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
    pub source_labels: Vec<String>,
}

/// Create a new topic named `name`. Returns its `id`.
///
/// Errors clearly if `name` is already taken (enforced by `topics.name`'s
/// `UNIQUE` constraint), mirroring `sources.rs`'s `map_label_conflict`
/// pattern.
pub fn create_topic(conn: &Connection, name: &str) -> Result<i64> {
    conn.execute("INSERT INTO topics (name) VALUES (?1)", params![name])
        .map_err(|err| map_topic_name_conflict(err, name))?;

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
/// source to a topic (`drip source add`/`drip source move`, bd issue
/// drip-38w.2). Unlike [`topic_id_by_name`] (whose error points at `drip
/// topic list`, appropriate for a read that just needs the exact name), the
/// fix for a missing topic here is to create it -- so the error instead
/// points at `drip topic add`.
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

/// Count how many sources currently belong to the topic named `topic_name`
/// (bd issue drip-38w.2: backs `drip topic remove`'s "refuse while non-empty"
/// guard). Errors clearly if no topic has that name, via [`topic_id_by_name`]
/// -- this is a read, so an unknown name is pointed at `drip topic list`
/// rather than `drip topic add`.
pub fn topic_source_count(conn: &Connection, topic_name: &str) -> Result<i64> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    conn.query_row(
        "SELECT COUNT(*) FROM sources WHERE topic_id = ?1",
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

/// Move the source labeled `source_label` to the topic named `topic_name`
/// (bd issue drip-38w.2: backs `drip source move` -- the only way to
/// reassign an already-saved source to a different topic now that every
/// source belongs to EXACTLY ONE topic, tracked by `sources.topic_id`).
///
/// Errors clearly if either the topic or the source doesn't exist -- an
/// unknown topic points at `drip topic add` (via [`require_topic_id`]) since
/// that's the actionable fix here, not `drip topic list`. Calling this again
/// for a source already in `topic_name` is a harmless no-op --
/// `sources::set_source_topic`'s `UPDATE` just sets the same value again.
pub fn move_source_to_topic(conn: &Connection, topic_name: &str, source_label: &str) -> Result<()> {
    let topic_id = require_topic_id(conn, topic_name)?;
    // Confirm the source itself exists first, so an unknown `source_label`
    // gets the same clear "no source named ... (run `drip source list`)"
    // message this always had, rather than whatever `set_source_topic`'s own
    // (equally clear, but not previously exercised via this path) message
    // happens to say.
    source_by_label(conn, source_label)?;

    sources::set_source_topic(conn, source_label, topic_id)
}

/// List every topic, ordered by name, with the labels of its member
/// sources (also ordered, by label) for `drip topic list` to render.
///
/// Membership is read via `sources.topic_id` (bd issue drip-38w.1), not the
/// now-inert `topic_sources` join. Unlabeled member sources (there shouldn't
/// be any -- every source that gets a `topic_id` also went through
/// `sources::upsert_source`/`set_source_topic`, both of which are only ever
/// called with an already-labeled source in this codebase -- but
/// defensively) are excluded from `source_labels`, matching `sources::list`'s
/// own `display_name IS NOT NULL` convention.
pub fn list_topics(conn: &Connection) -> Result<Vec<TopicWithSources>> {
    let mut topic_stmt = conn
        .prepare("SELECT id, name FROM topics ORDER BY name")
        .context("failed to prepare topic list query")?;

    let topics: Vec<(i64, String)> = topic_stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to list topics")?;

    let mut sources_stmt = conn
        .prepare(
            "SELECT s.display_name FROM sources s \
             WHERE s.topic_id = ?1 AND s.display_name IS NOT NULL \
             ORDER BY s.display_name",
        )
        .context("failed to prepare topic source labels query")?;

    let mut result = Vec::with_capacity(topics.len());
    for (id, name) in topics {
        let source_labels = sources_stmt
            .query_map(params![id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to list member sources for topic '{name}'"))?;
        result.push(TopicWithSources {
            id,
            name,
            source_labels,
        });
    }

    Ok(result)
}

/// Delete the topic named `name`.
///
/// With the `sources.topic_id` FK's `ON DELETE RESTRICT` (migration 0005,
/// bd issue drip-38w.1), deleting a topic that still owns any sources fails
/// at the DB layer -- there is no longer a "cascade to `topic_sources`,
/// leave `sources` alone" outcome, because `topic_sources` is inert and
/// membership lives on `sources.topic_id` itself. This function itself does
/// NOT pre-check emptiness -- `src/main.rs`'s `handle_topic` does that ahead
/// of calling this, via [`topic_source_count`], so it can refuse with a
/// clear, actionable message before ever reaching this raw DB-level `DELETE`
/// (bd issue drip-38w.2). Returns `true` if an (empty) topic existed and was
/// removed, `false` if no topic had that name.
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
/// Reads membership via `sources.topic_id` (bd issue drip-38w.1), not the
/// now-inert `topic_sources` join.
pub fn sources_for_topic(conn: &Connection, topic_name: &str) -> Result<Vec<SourceRow>> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.kind, s.identifier, s.display_name, s.topic_id, t.name \
             FROM sources s JOIN topics t ON t.id = s.topic_id \
             WHERE s.topic_id = ?1 \
             ORDER BY s.display_name",
        )
        .context("failed to prepare topic member sources query")?;

    let rows = stmt.query_map(params![topic_id], |row| {
        Ok(SourceRow {
            id: row.get(0)?,
            kind: parse_kind_column(row.get(1)?)?,
            identifier: row.get(2)?,
            display_name: row.get(3)?,
            topic_id: row.get(4)?,
            topic_name: row.get(5)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list member sources for topic '{topic_name}'"))
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

    #[test]
    fn move_source_to_topic_reassigns_it_and_appears_in_list_and_sources_for_topic() {
        let (_dir, conn) = fresh_conn();

        let tid_other = create_topic(&conn, "other").expect("create_topic should succeed");
        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_other, "rust-blog");

        move_source_to_topic(&conn, "rust", "rust-blog")
            .expect("move_source_to_topic should succeed");

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
    fn move_source_to_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();
        let tid_home = create_topic(&conn, "home").expect("create_topic should succeed");
        make_source(&conn, tid_home, "rust-blog");

        let err = move_source_to_topic(&conn, "does-not-exist", "rust-blog")
            .expect_err("missing topic should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip topic add"));
    }

    #[test]
    fn move_source_to_topic_errors_clearly_when_source_missing() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "rust").expect("create_topic should succeed");

        let err = move_source_to_topic(&conn, "rust", "does-not-exist")
            .expect_err("missing source should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip source list"));
    }

    #[test]
    fn moving_a_source_to_its_current_topic_twice_is_a_no_op() {
        let (_dir, conn) = fresh_conn();

        let tid_home = create_topic(&conn, "home").expect("create_topic should succeed");
        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_home, "rust-blog");

        move_source_to_topic(&conn, "rust", "rust-blog").expect("first move should succeed");
        move_source_to_topic(&conn, "rust", "rust-blog")
            .expect("second move should succeed as a no-op");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "no duplicate source row should be created");

        let found = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .expect("source should exist");
        assert_eq!(found.topic_id, tid_rust);
    }

    #[test]
    fn topic_source_count_reflects_current_membership() {
        let (_dir, conn) = fresh_conn();

        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        create_topic(&conn, "other").expect("create_topic should succeed");
        assert_eq!(
            topic_source_count(&conn, "rust").expect("topic_source_count should succeed"),
            0
        );

        make_source(&conn, tid_rust, "rust-blog");
        assert_eq!(
            topic_source_count(&conn, "rust").expect("topic_source_count should succeed"),
            1
        );
        assert_eq!(
            topic_source_count(&conn, "other").expect("topic_source_count should succeed"),
            0
        );

        move_source_to_topic(&conn, "other", "rust-blog").expect("move should succeed");
        assert_eq!(
            topic_source_count(&conn, "rust").expect("topic_source_count should succeed"),
            0
        );
        assert_eq!(
            topic_source_count(&conn, "other").expect("topic_source_count should succeed"),
            1
        );
    }

    #[test]
    fn topic_source_count_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err =
            topic_source_count(&conn, "does-not-exist").expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
        assert!(err.to_string().contains("drip topic list"));
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
        // New invariant under bd issue drip-38w.1's one-topic-per-source
        // model: `sources.topic_id`'s `ON DELETE RESTRICT` (migration 0005)
        // means the database itself now refuses to remove a topic that
        // still owns sources -- there is no more "cascade to
        // `topic_sources`, but leave the `sources` rows alone" outcome, since
        // `topic_sources` is inert and membership lives on
        // `sources.topic_id` directly. `src/main.rs`'s `handle_topic` guards
        // against this ahead of time (bd issue drip-38w.2) via
        // `topic_source_count`, but this test still pins today's raw
        // DB-level behavior: an `Err`, not a panic and not a silent success.
        let (_dir, conn) = fresh_conn();
        let tid_rust = create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, tid_rust, "rust-blog");

        remove_topic(&conn, "rust")
            .expect_err("removing a topic that still owns a source should fail (FK RESTRICT)");

        // Once the source is moved elsewhere, the now-empty topic can be
        // removed, and the source itself survives (removing a topic never
        // deletes the sources that were in it).
        create_topic(&conn, "other").expect("create_topic should succeed");
        move_source_to_topic(&conn, "other", "rust-blog")
            .expect("moving the source out of 'rust' should succeed");
        let removed = remove_topic(&conn, "rust").expect("removing an empty topic should succeed");
        assert!(removed);

        let still_exists = sources::find_by_label(&conn, "rust-blog")
            .expect("find_by_label should succeed")
            .expect("source should still exist after its (now-empty) topic is removed");
        assert_eq!(still_exists.topic_name, "other");
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
}
