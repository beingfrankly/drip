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
/// Deliberately carries NO topic information (bd issue drip-ho5.3, decided
/// 2026-08-07): a source describes a feed, not its memberships, and the
/// one-topic-per-source assumption `topic_id`/`topic_name` used to encode is
/// no longer representable now that a source can link into more than one
/// sub-topic (`migrations/0006_topic_tree.sql`'s `topic_links` table). Code
/// that needs a source's linked sub-topics reaches for [`SourceWithTopics`]
/// (via [`list_with_topics`]) instead.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub id: i64,
    pub kind: SourceKind,
    pub identifier: String,
    pub display_name: Option<String>,
}

/// A labeled source together with the names of every sub-topic it's linked
/// into (via `topic_links`), sorted -- backs `drip source list`. Mirrors the
/// existing `TopicWithSources` precedent (`src/topics.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceWithTopics {
    pub source: SourceRow,
    pub topics: Vec<String>,
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
/// `topic_id` is the topic this source is linked into (bd issue drip-ho5.3):
/// rather than writing `sources.topic_id` (now dead -- migration 0006 nulls
/// it, and nothing repopulates it going forward), this creates a row in
/// `topic_links` instead, the many-to-many table that replaces it as the
/// source-of-truth for topic membership. `topic_links` has `UNIQUE
/// (source_id, topic_id)`, so re-adding an already-linked `(kind,
/// identifier)` into the SAME topic is a no-op rather than a duplicate link
/// -- this keeps `upsert_source` idempotent end-to-end, matching its
/// existing idempotent-upsert behaviour for the `sources` row itself.
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
            "INSERT INTO sources (kind, identifier, display_name) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(kind, identifier) DO UPDATE SET \
                display_name = excluded.display_name",
            params![kind, identifier, label],
        ),
        None => conn.execute(
            "INSERT INTO sources (kind, identifier) VALUES (?1, ?2) \
             ON CONFLICT(kind, identifier) DO NOTHING",
            params![kind, identifier],
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

    conn.execute(
        "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2) \
         ON CONFLICT(source_id, topic_id) DO NOTHING",
        params![id, topic_id],
    )
    .with_context(|| format!("failed to link source '{identifier}' to topic {topic_id}"))?;

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
///
/// Reads `sources` alone -- no join against `topics`/`topic_links` (bd issue
/// drip-ho5.3). The previous `INNER JOIN topics ON t.id = s.topic_id`
/// silently returned no rows for any source once `sources.topic_id` was
/// NULL, which migration 0006 makes true of every source (existing rows via
/// its backfill, new rows because `upsert_source` no longer writes that
/// column at all). A source's topic membership is looked up separately, via
/// [`list_with_topics`], when a caller actually needs it.
pub fn find_by_label(conn: &Connection, label: &str) -> Result<Option<SourceRow>> {
    let row = conn.query_row(
        "SELECT id, kind, identifier, display_name FROM sources WHERE display_name = ?1",
        params![label],
        |row| {
            Ok(SourceRow {
                id: row.get(0)?,
                kind: parse_kind_column(row.get(1)?)?,
                identifier: row.get(2)?,
                display_name: row.get(3)?,
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
///
/// No join against `topics`/`topic_links` -- see [`find_by_label`]'s doc
/// comment for why (bd issue drip-ho5.3).
pub fn list(conn: &Connection) -> Result<Vec<SourceRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, identifier, display_name FROM sources \
             WHERE display_name IS NOT NULL ORDER BY display_name",
        )
        .context("failed to prepare source list query")?;

    let rows = stmt.query_map([], |row| {
        Ok(SourceRow {
            id: row.get(0)?,
            kind: parse_kind_column(row.get(1)?)?,
            identifier: row.get(2)?,
            display_name: row.get(3)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to list sources")
}

/// The names of every sub-topic `source_id` is linked into (via
/// `topic_links`), sorted. Building block for [`list_with_topics`]; also
/// used directly by `src/main.rs`'s per-source fetch path (bd issue
/// drip-ho5.3) to derive a `SourceGroup`'s digest-heading topic now that
/// `SourceRow` itself no longer carries one -- see that call site's own
/// comment (points at bd issue drip-98u.5 for the eventual replacement).
pub fn topic_names_for_source(conn: &Connection, source_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT t.name FROM topic_links tl JOIN topics t ON t.id = tl.topic_id \
             WHERE tl.source_id = ?1 ORDER BY t.name",
        )
        .context("failed to prepare source topic-links query")?;

    let names = stmt
        .query_map(params![source_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list linked topics for source id {source_id}"))?;

    Ok(names)
}

/// The title-only exclude terms configured for `source_id` in
/// `source_excludes` (`migrations/0006_topic_tree.sql`) -- the pre-filter
/// `classify::classify_item` rejects an item with, before any candidate
/// sub-topic routing runs (bd issue drip-98u.3, loaded here for bd issue
/// drip-ho5.6's pipeline). Empty when the source has none configured, which
/// is the common case today (bd issue drip-ho5.8 owns the CLI to author
/// these; nothing writes to `source_excludes` yet).
pub fn source_excludes(conn: &Connection, source_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT term FROM source_excludes WHERE source_id = ?1")
        .context("failed to prepare source_excludes query")?;

    let terms = stmt
        .query_map(params![source_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to list source_excludes for source id {source_id}"))?;

    Ok(terms)
}

/// [`list`] every labeled source, each paired with the names of every
/// sub-topic it's linked into (via [`topic_names_for_source`]), sorted --
/// backs `drip source list` (bd issue drip-ho5.3). Mirrors `src/topics.rs`'s
/// `list_topics` precedent: one query for the parent rows, one query per row
/// for the child rows, rather than a single join (which would need
/// row-collapsing logic for a source linked into more than one sub-topic).
pub fn list_with_topics(conn: &Connection) -> Result<Vec<SourceWithTopics>> {
    let sources = list(conn)?;

    let mut result = Vec::with_capacity(sources.len());
    for source in sources {
        let topics = topic_names_for_source(conn, source.id)?;
        result.push(SourceWithTopics { source, topics });
    }

    Ok(result)
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

/// Declaratively (re)configure `source_id`'s title-only exclude terms in
/// `source_excludes` (`migrations/0006_topic_tree.sql`) -- backs `drip
/// source add --exclude` (bd issue drip-ho5.8). REPLACES the source's entire
/// exclude list wholesale, same "declarative upsert" convention as
/// `crate::topics::link_source_to_topic`'s `--match`/`--exclude` (drip-98u.8):
/// re-running with the same `terms` is idempotent and produces identical
/// state. A plain DELETE-then-INSERT, so this never touches `seen_items` or
/// any `topic_links`/`link_rules` row -- source-level excludes are a
/// pre-filter that runs before any topic-link classification.
pub fn set_source_excludes(conn: &Connection, source_id: i64, terms: &[String]) -> Result<()> {
    conn.execute(
        "DELETE FROM source_excludes WHERE source_id = ?1",
        params![source_id],
    )
    .with_context(|| format!("failed to clear existing excludes for source id {source_id}"))?;

    for term in terms {
        conn.execute(
            "INSERT INTO source_excludes (source_id, term) VALUES (?1, ?2)",
            params![source_id, term],
        )
        .with_context(|| format!("failed to insert exclude '{term}' for source id {source_id}"))?;
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

        // `SourceRow` itself no longer carries topic membership (bd issue
        // drip-ho5.3) -- `list_with_topics` is where that lives now.
        let listed = list_with_topics(&conn).expect("list_with_topics should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].topics, vec!["Uncategorized".to_string()]);
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

    // -- bd issue drip-ho5.8: `set_source_excludes` replaces
    // `set_source_topic` (removed -- it expressed one-topic-per-source,
    // which is no longer meaningful under many-to-many `topic_links`;
    // reassignment is now `topics::unlink_source_from_topic` +
    // `topics::link_source_to_topic` instead).

    #[test]
    fn set_source_excludes_is_readable_via_source_excludes() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();
        let id = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        set_source_excludes(&conn, id, &["megathread".to_string(), "hiring".to_string()])
            .expect("set_source_excludes should succeed");

        let terms = source_excludes(&conn, id).expect("source_excludes should succeed");
        assert_eq!(terms.len(), 2);
        assert!(terms.contains(&"megathread".to_string()));
        assert!(terms.contains(&"hiring".to_string()));
    }

    #[test]
    fn set_source_excludes_replaces_the_list_wholesale() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();
        let id = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        set_source_excludes(&conn, id, &["megathread".to_string()])
            .expect("first set should succeed");
        set_source_excludes(&conn, id, &["hiring".to_string()])
            .expect("second set should replace the first, not append");

        let terms = source_excludes(&conn, id).expect("source_excludes should succeed");
        assert_eq!(
            terms,
            vec!["hiring".to_string()],
            "re-running with a different list should replace it wholesale, not accumulate"
        );
    }

    #[test]
    fn set_source_excludes_with_an_empty_list_clears_any_existing_terms() {
        let (_dir, conn) = fresh_conn();
        let tid = crate::topics::get_or_create_topic(&conn, "Uncategorized").unwrap();
        let id = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid,
        )
        .expect("upsert should succeed");

        set_source_excludes(&conn, id, &["megathread".to_string()])
            .expect("first set should succeed");
        set_source_excludes(&conn, id, &[]).expect("clearing with an empty list should succeed");

        assert!(source_excludes(&conn, id).unwrap().is_empty());
    }

    #[test]
    fn find_by_label_and_list_find_sources_even_when_topic_id_is_null() {
        // Migration 0006 nulls every `sources.topic_id` (bd issue drip-ho5.3
        // -- see the migration's own header comment and bd-ho5.3's bug
        // inventory). `find_by_label`/`list`'s old `INNER JOIN topics ON
        // t.id = s.topic_id` silently excluded a source the moment that
        // column went NULL. Apply the FULL migration chain via `db::open`
        // (so the schema is exactly what a real post-0006 database has),
        // seed a source directly with a NULL `topic_id` -- the exact
        // post-migration shape -- and assert both lookups still find it.
        let (_dir, conn) = fresh_conn();

        conn.execute(
            "INSERT INTO sources (kind, identifier, display_name) VALUES ('rss', \
             'https://example.com/feed.xml', 'rust-blog')",
            [],
        )
        .expect("raw insert with NULL topic_id should succeed");

        let found = find_by_label(&conn, "rust-blog")
            .expect("find_by_label should succeed")
            .expect("a source with a NULL topic_id should still be found");
        assert_eq!(found.display_name, Some("rust-blog".to_string()));
        assert_eq!(found.identifier, "https://example.com/feed.xml");

        let listed = list(&conn).expect("list should succeed");
        assert_eq!(
            listed.len(),
            1,
            "list() should also find the source despite its NULL topic_id"
        );
    }

    #[test]
    fn list_with_topics_returns_every_linked_sub_topic_sorted() {
        let (_dir, conn) = fresh_conn();
        let tid_zeta = crate::topics::get_or_create_topic(&conn, "zeta").unwrap();
        let tid_alpha = crate::topics::get_or_create_topic(&conn, "alpha").unwrap();

        let id = upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/feed.xml",
            Some("rust-blog"),
            tid_zeta,
        )
        .expect("upsert should succeed");

        // Link the same source into a SECOND sub-topic directly -- exercises
        // the many-to-many shape `topic_links` exists for (a source with two
        // sub-topics), even though `upsert_source`'s own signature still
        // only takes one `topic_id` at a time in this slice.
        conn.execute(
            "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
            params![id, tid_alpha],
        )
        .expect("second link insert should succeed");

        let listed = list_with_topics(&conn).expect("list_with_topics should succeed");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source.display_name, Some("rust-blog".to_string()));
        assert_eq!(
            listed[0].topics,
            vec!["alpha".to_string(), "zeta".to_string()],
            "linked sub-topics should be returned sorted by name"
        );
    }
}
