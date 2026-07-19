//! Source management: ensuring a `sources` row exists for a given kind +
//! identifier pair (see `migrations/0001_init.sql`), plus (drip-15n.9.6) the
//! labeled-source CRUD backing `drip source add/list/remove`.
//!
//! Design context: bd issue drip-15n.9.3 introduced [`upsert_reddit_source`]
//! as the building block the (since-removed, bd issue drip-1uk.2) `drip
//! profile add` command used to make sure every subreddit it referenced had
//! a `sources` row before linking it into `profile_sources`; it's now
//! `#[cfg(test)]`-only, kept as a test fixture builder (bd issue drip-1uk.9).
//! bd issue drip-15n.9.6 generalizes the general case into [`upsert_source`]
//! (any `kind`, optionally labeled via `display_name`) plus
//! [`find_by_label`]/[`list`]/[`remove_by_label`] for the `drip source`
//! command family.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::types::SourceKind;

/// A single `sources` row, as returned by the labeled-source lookups below.
///
/// `topic_id`/`topic_name` are always populated (bd issue drip-38w.1: every
/// source belongs to EXACTLY ONE topic, tracked by `sources.topic_id`) --
/// `find_by_label`/`list` join against `topics` to fetch them rather than
/// leaving them optional.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub id: i64,
    pub kind: SourceKind,
    pub identifier: String,
    pub display_name: Option<String>,
    pub topic_id: i64,
    pub topic_name: String,
}

/// Parse a `sources.kind` TEXT column value (already read out as a
/// `String`) into a [`SourceKind`], surfacing an unrecognized value as a
/// normal `rusqlite::Error` (rather than panicking) so a row-mapping
/// closure can propagate it via `?` like any other column read. In
/// practice this should never fail -- `migrations/0001_init.sql`'s `kind IN
/// ('reddit', 'rss', 'youtube')` CHECK constraint rejects anything else at
/// write time -- but row-mapping closures can't return `anyhow::Error`, so
/// this is the String<->enum conversion boundary this module owns.
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

/// Ensure a `sources` row exists for `(kind, identifier)`, creating it if
/// necessary, and return its `id`.
///
/// Idempotent on `(kind, identifier)` -- enforced by the `UNIQUE (kind,
/// identifier)` constraint on `sources`. When `display_name` is `Some`, it is
/// set (or updated) on that row; when `None`, any existing label is left
/// untouched -- a caller that doesn't care about labeling (e.g. Reddit's own
/// `upsert_reddit_source` below) must never clobber a label a `drip source
/// add` call gave this row.
///
/// `topic_id` is the row's owning topic (bd issue drip-38w.1: every source
/// belongs to EXACTLY ONE topic). On a fresh insert it's always set. On the
/// `Some(label)` conflict branch (re-adding an already-labeled source) it is
/// ALSO updated, on the theory that re-running `drip source add` for an
/// existing label is a deliberate re-assignment, not just a label refresh; on
/// the `None`-label branch (`ON CONFLICT DO NOTHING`) it's only used for a
/// genuinely fresh row, matching that branch's existing idempotency -- an
/// unlabeled re-upsert (e.g. `upsert_reddit_source`) never touches an
/// existing row's topic.
///
/// If `display_name` is `Some(x)` and `x` is already claimed by a DIFFERENT
/// `(kind, identifier)` pair, the `idx_sources_display_name` unique index
/// (`migrations/0003_source_labels.sql`) rejects the write; that raw SQLite
/// constraint error is caught here and mapped to a clear message pointing at
/// `drip source list`/`drip source remove`.
pub fn upsert_source(
    conn: &Connection,
    kind: SourceKind,
    identifier: &str,
    display_name: Option<&str>,
    topic_id: i64,
) -> Result<i64> {
    let kind = kind.as_str();
    let result = match display_name {
        Some(label) => conn.execute(
            "INSERT INTO sources (kind, identifier, display_name, topic_id) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(kind, identifier) DO UPDATE SET \
                display_name = excluded.display_name, topic_id = excluded.topic_id",
            params![kind, identifier, label, topic_id],
        ),
        None => conn.execute(
            "INSERT INTO sources (kind, identifier, topic_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT(kind, identifier) DO NOTHING",
            params![kind, identifier, topic_id],
        ),
    };

    result.map_err(|err| map_label_conflict(err, display_name))?;

    let id: i64 = conn
        .query_row(
            "SELECT id FROM sources WHERE kind = ?1 AND identifier = ?2",
            params![kind, identifier],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up source id for {kind} '{identifier}'"))?;

    Ok(id)
}

/// Map a `rusqlite::Error` from the `upsert_source` write above into a clear
/// `anyhow` error when it's the `idx_sources_display_name` unique constraint
/// firing because `display_name` is already claimed by a different source;
/// pass through any other error via its normal `anyhow` conversion.
fn map_label_conflict(err: rusqlite::Error, display_name: Option<&str>) -> anyhow::Error {
    if let Some(label) = display_name {
        if err.to_string().contains("UNIQUE constraint failed") {
            return anyhow::anyhow!(
                "a source named '{label}' already exists (run `drip source list` to see saved \
                 sources, or `drip source remove --name {label}` first)"
            );
        }
    }
    anyhow::Error::new(err).context("failed to upsert source")
}

/// Ensure a `sources` row exists for the reddit subreddit `subreddit`
/// (`kind = 'reddit'`), creating it if necessary, and return its `id`.
///
/// Idempotent: calling this twice with the same `subreddit` returns the same
/// id both times rather than creating a duplicate row. A thin wrapper around
/// [`upsert_source`] with no label -- Reddit sources created this way were
/// unlabeled and didn't show up in `drip source list`, which is specifically
/// for the sources this module's labeled-CRUD functions manage.
///
/// Test-only (bd issue drip-1uk.9): its only production callers were the
/// OAuth `-s/--subreddit` fetch path and `drip profile add`, both removed
/// (bd issue drip-1uk.1/.2) now that drip is RSS-only for Reddit. Kept
/// `#[cfg(test)]` as a convenience fixture builder for
/// `dedup.rs`/`fetch_runs.rs`/this module's own tests, which need a `sources`
/// row to exist without caring about labeling.
///
/// Signature deliberately unchanged by bd issue drip-38w.1's one-topic-per-
/// source model -- callers outside this module don't care which topic a
/// fixture source lands in, so this gets-or-creates an "Uncategorized" topic
/// internally rather than pushing a `topic_id` param onto every caller.
#[cfg(test)]
pub fn upsert_reddit_source(conn: &Connection, subreddit: &str) -> Result<i64> {
    let topic_id = crate::topics::get_or_create_topic(conn, "Uncategorized")?;
    upsert_source(conn, SourceKind::Reddit, subreddit, None, topic_id)
}

/// Look up a labeled source by its `display_name`. Returns `None` if no
/// source has that label.
pub fn find_by_label(conn: &Connection, label: &str) -> Result<Option<SourceRow>> {
    let row = conn.query_row(
        "SELECT s.id, s.kind, s.identifier, s.display_name, s.topic_id, t.name \
         FROM sources s JOIN topics t ON t.id = s.topic_id \
         WHERE s.display_name = ?1",
        params![label],
        |row| {
            Ok(SourceRow {
                id: row.get(0)?,
                kind: parse_kind_column(row.get(1)?)?,
                identifier: row.get(2)?,
                display_name: row.get(3)?,
                topic_id: row.get(4)?,
                topic_name: row.get(5)?,
            })
        },
    );

    match row {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to look up source '{label}'")),
    }
}

/// List every labeled source (`display_name IS NOT NULL`), ordered by
/// `display_name`. Intentionally excludes unlabeled sources -- those were
/// Reddit sources created implicitly via the now-removed `-s`/`drip profile
/// add` (bd issue drip-1uk.1/.2); `drip source list` is specifically for the
/// sources this module's labeled-CRUD functions manage.
pub fn list(conn: &Connection) -> Result<Vec<SourceRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.kind, s.identifier, s.display_name, s.topic_id, t.name \
             FROM sources s JOIN topics t ON t.id = s.topic_id \
             WHERE s.display_name IS NOT NULL ORDER BY s.display_name",
        )
        .context("failed to prepare source list query")?;

    let rows = stmt.query_map([], |row| {
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
        .context("failed to list sources")
}

/// Delete the source row whose `display_name` is `label`. Returns `true` if
/// a row was deleted, `false` if no source had that label.
pub fn remove_by_label(conn: &Connection, label: &str) -> Result<bool> {
    let changed = conn
        .execute(
            "DELETE FROM sources WHERE display_name = ?1",
            params![label],
        )
        .with_context(|| format!("failed to remove source '{label}'"))?;
    Ok(changed > 0)
}

/// Assign/move the source labeled `source_label` to the topic `topic_id`
/// (bd issue drip-38w.1). This is the single place `sources.topic_id` is
/// ever updated after a source's initial insert -- both `drip topic
/// add-source` (reassign) and `drip topic remove-source` (fall back to
/// "Uncategorized") go through this, via `crate::topics`'s repointed
/// wrappers.
///
/// Errors clearly if no source has `source_label` -- mirrors
/// `crate::topics::source_by_label`'s "no source named ... (run `drip source
/// list`)" message, since callers here are typically already holding a topic
/// name and need the same clarity bar for an unknown source label.
pub fn set_source_topic(conn: &Connection, source_label: &str, topic_id: i64) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE sources SET topic_id = ?2 WHERE display_name = ?1",
            params![source_label, topic_id],
        )
        .with_context(|| format!("failed to set topic for source '{source_label}'"))?;

    if changed == 0 {
        anyhow::bail!("no source named '{source_label}' (run `drip source list`)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;

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

    #[test]
    fn upsert_reddit_source_is_idempotent() {
        let (_dir, conn) = fresh_conn();

        let id1 = upsert_reddit_source(&conn, "rust").expect("first upsert should succeed");
        let id2 = upsert_reddit_source(&conn, "rust").expect("second upsert should succeed");

        assert_eq!(
            id1, id2,
            "same subreddit should resolve to the same source id"
        );

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "no duplicate source row should be created");
    }

    #[test]
    fn upsert_source_with_a_label_is_findable_by_that_label() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        let found = find_by_label(&conn, "rust-blog")
            .expect("find_by_label should succeed")
            .expect("source should exist");

        assert_eq!(found.kind, SourceKind::Rss);
        assert_eq!(found.identifier, "https://example.com/feed.xml");
        assert_eq!(found.display_name, Some("rust-blog".to_string()));
        assert_eq!(found.topic_id, tid);
        assert_eq!(found.topic_name, "Uncategorized");
    }

    #[test]
    fn upsert_source_twice_with_same_identifier_and_new_label_renames_it() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        let id1 = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("old-name"),
            tid,
        )
        .expect("first upsert should succeed");
        let id2 = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("new-name"),
            tid,
        )
        .expect("second upsert should succeed");

        assert_eq!(
            id1, id2,
            "same (kind, identifier) should resolve to the same row"
        );

        assert!(
            find_by_label(&conn, "old-name").unwrap().is_none(),
            "old label should no longer resolve"
        );
        let found = find_by_label(&conn, "new-name")
            .unwrap()
            .expect("new label should resolve");
        assert_eq!(found.id, id1);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "renaming must not create a second row");
    }

    #[test]
    fn upsert_source_with_a_label_claimed_by_a_different_identifier_errors_clearly() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed-a.xml",
            Some("taken"),
            tid,
        )
        .expect("first upsert should succeed");

        let err = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed-b.xml",
            Some("taken"),
            tid,
        )
        .expect_err("claiming an already-used label for a different source should error");

        let message = err.to_string();
        assert!(
            message.contains("taken"),
            "error should mention the label: {message}"
        );
        assert!(
            message.contains("drip source list"),
            "error should point users at `drip source list`: {message}"
        );

        // No duplicate/corrupt row should have been created for feed-b.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "the failed upsert must not leave a stray row behind"
        );
    }

    #[test]
    fn list_returns_only_labeled_sources() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("labeled upsert should succeed");
        upsert_reddit_source(&conn, "rust").expect("unlabeled upsert should succeed");

        let listed = list(&conn).expect("list should succeed");

        assert_eq!(
            listed.len(),
            1,
            "unlabeled sources must not appear in list()"
        );
        assert_eq!(listed[0].display_name, Some("rust-blog".to_string()));
    }

    #[test]
    fn remove_by_label_deletes_the_row_and_reports_success() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        let removed = remove_by_label(&conn, "rust-blog").expect("remove should succeed");
        assert!(removed);

        assert!(find_by_label(&conn, "rust-blog").unwrap().is_none());
    }

    #[test]
    fn remove_by_label_returns_false_for_unknown_label_without_side_effects() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        let removed = remove_by_label(&conn, "does-not-exist").expect("remove should succeed");
        assert!(!removed);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "removing an unknown label must not touch existing rows"
        );
    }

    #[test]
    fn set_source_topic_moves_the_source_to_the_given_topic() {
        let (_dir, conn) = fresh_conn();
        let tid_a = crate::topics::get_or_create_topic(&conn, "a").unwrap();
        let tid_b = crate::topics::get_or_create_topic(&conn, "b").unwrap();

        upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid_a,
        )
        .expect("upsert should succeed");

        set_source_topic(&conn, "rust-blog", tid_b).expect("set_source_topic should succeed");

        let found = find_by_label(&conn, "rust-blog")
            .unwrap()
            .expect("source should still exist");
        assert_eq!(found.topic_id, tid_b);
        assert_eq!(found.topic_name, "b");
    }

    #[test]
    fn set_source_topic_errors_clearly_for_an_unknown_label() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        let err = set_source_topic(&conn, "does-not-exist", tid)
            .expect_err("unknown source label should error");

        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("drip source list"));
    }
}
