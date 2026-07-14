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
        Err(err) => {
            Err(err).with_context(|| format!("failed to look up topic '{topic_name}'"))
        }
    }
}

/// Look up a source's row by its `display_name` label, returning a clear
/// error (pointing at `drip source list`) if no source has that label.
fn source_by_label(conn: &Connection, source_label: &str) -> Result<SourceRow> {
    sources::find_by_label(conn, source_label)?.ok_or_else(|| {
        anyhow::anyhow!("no source named '{source_label}' (run `drip source list`)")
    })
}

/// Add the source labeled `source_label` to the topic named `topic_name`.
///
/// Errors clearly if either the topic or the source doesn't exist. Adding
/// the same source to the same topic twice is a no-op, not an error and not
/// a duplicate row -- enforced via `ON CONFLICT(topic_id, source_id) DO
/// NOTHING` against `topic_sources`'s `(topic_id, source_id)` primary key.
pub fn add_source_to_topic(conn: &Connection, topic_name: &str, source_label: &str) -> Result<()> {
    let topic_id = topic_id_by_name(conn, topic_name)?;
    let source = source_by_label(conn, source_label)?;

    conn.execute(
        "INSERT INTO topic_sources (topic_id, source_id) VALUES (?1, ?2) \
         ON CONFLICT(topic_id, source_id) DO NOTHING",
        params![topic_id, source.id],
    )
    .with_context(|| {
        format!("failed to add source '{source_label}' to topic '{topic_name}'")
    })?;

    Ok(())
}

/// Remove the source labeled `source_label` from the topic named
/// `topic_name`.
///
/// Errors clearly if either the topic or the source itself doesn't exist
/// (same as [`add_source_to_topic`]), but NOT if the source simply wasn't a
/// member of that topic -- "already gone" is a success state for a removal.
pub fn remove_source_from_topic(
    conn: &Connection,
    topic_name: &str,
    source_label: &str,
) -> Result<()> {
    let topic_id = topic_id_by_name(conn, topic_name)?;
    let source = source_by_label(conn, source_label)?;

    conn.execute(
        "DELETE FROM topic_sources WHERE topic_id = ?1 AND source_id = ?2",
        params![topic_id, source.id],
    )
    .with_context(|| {
        format!("failed to remove source '{source_label}' from topic '{topic_name}'")
    })?;

    Ok(())
}

/// List every topic, ordered by name, with the labels of its member
/// sources (also ordered, by label) for `drip topic list` to render.
///
/// Unlabeled member sources (there shouldn't be any -- `add_source_to_topic`
/// only ever attaches sources looked up by label -- but defensively) are
/// excluded from `source_labels`, matching `sources::list`'s own
/// `display_name IS NOT NULL` convention.
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
            "SELECT s.display_name FROM topic_sources ts \
             JOIN sources s ON s.id = ts.source_id \
             WHERE ts.topic_id = ?1 AND s.display_name IS NOT NULL \
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

/// Delete the topic named `name`. Cascades to `topic_sources` automatically
/// via the FK (`ON DELETE CASCADE`), but does NOT touch the `sources` rows
/// themselves -- removing a topic never deletes the sources that were in
/// it. Returns `true` if a topic existed and was removed, `false` if no
/// topic had that name.
pub fn remove_topic(conn: &Connection, name: &str) -> Result<bool> {
    let changed = conn
        .execute("DELETE FROM topics WHERE name = ?1", params![name])
        .with_context(|| format!("failed to remove topic '{name}'"))?;
    Ok(changed > 0)
}

/// Resolve the topic named `topic_name` into its full member `SourceRow`s,
/// for `drip fetch --topic` (bd issue drip-p6v.7) to expand into fetchable
/// sources. Errors clearly if the topic name doesn't exist.
pub fn sources_for_topic(conn: &Connection, topic_name: &str) -> Result<Vec<SourceRow>> {
    let topic_id = topic_id_by_name(conn, topic_name)?;

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.kind, s.identifier, s.display_name FROM topic_sources ts \
             JOIN sources s ON s.id = ts.source_id \
             WHERE ts.topic_id = ?1 \
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

    fn make_source(conn: &Connection, label: &str) -> i64 {
        upsert_source(
            conn,
            SourceKind::Rss,
            &format!("https://example.com/{label}.xml"),
            Some(label),
        )
        .expect("upsert_source should succeed")
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
        assert!(message.contains("rust"), "error should mention the name: {message}");
        assert!(
            message.contains("drip topic list"),
            "error should point users at `drip topic list`: {message}"
        );
    }

    #[test]
    fn add_source_to_topic_attaches_it_and_appears_in_list_and_sources_for_topic() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");

        add_source_to_topic(&conn, "rust", "rust-blog")
            .expect("add_source_to_topic should succeed");

        let listed = list_topics(&conn).expect("list_topics should succeed");
        assert_eq!(listed[0].source_labels, vec!["rust-blog".to_string()]);

        let members = sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].display_name, Some("rust-blog".to_string()));
    }

    #[test]
    fn add_source_to_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();
        make_source(&conn, "rust-blog");

        let err = add_source_to_topic(&conn, "does-not-exist", "rust-blog")
            .expect_err("missing topic should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip topic list"));
    }

    #[test]
    fn add_source_to_topic_errors_clearly_when_source_missing() {
        let (_dir, conn) = fresh_conn();
        create_topic(&conn, "rust").expect("create_topic should succeed");

        let err = add_source_to_topic(&conn, "rust", "does-not-exist")
            .expect_err("missing source should error");
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip source list"));
    }

    #[test]
    fn adding_the_same_source_to_a_topic_twice_is_a_no_op() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");

        add_source_to_topic(&conn, "rust", "rust-blog")
            .expect("first add should succeed");
        add_source_to_topic(&conn, "rust", "rust-blog")
            .expect("second add should succeed as a no-op");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "no duplicate topic_sources row should be created");
    }

    #[test]
    fn remove_source_from_topic_detaches_it() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");
        add_source_to_topic(&conn, "rust", "rust-blog").expect("add should succeed");

        remove_source_from_topic(&conn, "rust", "rust-blog")
            .expect("remove_source_from_topic should succeed");

        let members = sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert!(members.is_empty());
    }

    #[test]
    fn remove_source_from_topic_is_not_an_error_when_not_a_member() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");

        // Never added -- removing should still succeed, not error.
        remove_source_from_topic(&conn, "rust", "rust-blog")
            .expect("removing a non-member should succeed as a no-op");
    }

    #[test]
    fn remove_source_from_topic_still_errors_when_topic_or_source_missing() {
        let (_dir, conn) = fresh_conn();
        make_source(&conn, "rust-blog");

        let err = remove_source_from_topic(&conn, "does-not-exist", "rust-blog")
            .expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));

        create_topic(&conn, "rust").expect("create_topic should succeed");
        let err = remove_source_from_topic(&conn, "rust", "does-not-exist")
            .expect_err("missing source should error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn removing_a_topic_does_not_delete_its_member_sources() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");
        add_source_to_topic(&conn, "rust", "rust-blog").expect("add should succeed");

        remove_topic(&conn, "rust").expect("remove_topic should succeed");

        let still_exists = sources::find_by_label(&conn, "rust-blog")
            .expect("find_by_label should succeed")
            .expect("source should still exist after its topic is removed");
        assert_eq!(still_exists.display_name, Some("rust-blog".to_string()));
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
        assert_eq!(count, 1, "removing an unknown topic must not touch existing rows");
    }

    #[test]
    fn removing_a_source_removes_it_from_any_topic_it_belonged_to() {
        let (_dir, conn) = fresh_conn();

        create_topic(&conn, "rust").expect("create_topic should succeed");
        make_source(&conn, "rust-blog");
        add_source_to_topic(&conn, "rust", "rust-blog").expect("add should succeed");

        let count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        sources::remove_by_label(&conn, "rust-blog").expect("remove_by_label should succeed");

        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count_after, 0,
            "topic_sources row should be cascade-deleted when its source is removed"
        );

        let members = sources_for_topic(&conn, "rust").expect("sources_for_topic should succeed");
        assert!(members.is_empty());
    }

    #[test]
    fn sources_for_topic_errors_clearly_when_topic_missing() {
        let (_dir, conn) = fresh_conn();

        let err = sources_for_topic(&conn, "does-not-exist")
            .expect_err("missing topic should error");
        assert!(err.to_string().contains("does-not-exist"));
        assert!(err.to_string().contains("drip topic list"));
    }
}
