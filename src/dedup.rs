//! Dedup ledger operations against the `seen_items` table (see
//! `migrations/0001_init.sql`).
//!
//! Design context: bd issue drip-15n.9.4. Dedup scope here is deliberately
//! PER-SOURCE, not global: a crosspost of the same Reddit post into two
//! different subreddits is two distinct rows in `seen_items` (two different
//! `sources` rows, e.g. one for r/rust, one for r/programming), because it's
//! genuinely different content in a different community context. This module
//! never dedups across sources -- every lookup and write here is scoped to a
//! single `source_id`.

use anyhow::{Context, Result};
use rusqlite::params;
use rusqlite::Connection;

use crate::item::Item;

/// Return the subset of `items` that do NOT already have a `seen_items` row
/// for `source_id`. Read-only -- issues no writes, so it's safe to call
/// during `--dry-run`.
pub fn filter_unseen(conn: &Connection, source_id: i64, items: Vec<Item>) -> Result<Vec<Item>> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM seen_items WHERE source_id = ?1 AND external_id = ?2")
        .context("failed to prepare seen_items lookup statement")?;

    let mut unseen = Vec::with_capacity(items.len());
    for item in items {
        let already_seen = stmt
            .exists(params![source_id, item.id])
            .with_context(|| format!("failed to check seen_items for item '{}'", item.id))?;
        if !already_seen {
            unseen.push(item);
        }
    }

    Ok(unseen)
}

/// Record `items` as seen for `source_id`. Idempotent: relies on the
/// `UNIQUE (source_id, external_id)` constraint on `seen_items` plus
/// `INSERT OR IGNORE`, so recording the same item twice is a no-op rather
/// than an error. `first_seen_at` is left to the table's own default.
pub fn record_seen(conn: &Connection, source_id: i64, items: &[Item]) -> Result<()> {
    let mut stmt = conn
        .prepare(
            "INSERT OR IGNORE INTO seen_items (source_id, external_id, title, url) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .context("failed to prepare seen_items insert statement")?;

    for item in items {
        stmt.execute(params![source_id, item.id, item.title, item.url])
            .with_context(|| format!("failed to record item '{}' as seen", item.id))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;
    use crate::sources;

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

    fn sample_item(id: &str, title: &str) -> Item {
        Item {
            id: id.to_string(),
            title: title.to_string(),
            url: format!("https://reddit.com/r/rust/comments/{id}/post/"),
            comments_url: None,
            author: None,
            published_at: None,
            summary: None,
            score: None,
            num_comments: None,
            flair: None,
            nsfw: false,
        }
    }

    #[test]
    fn filter_unseen_excludes_a_post_already_recorded_for_the_same_source() {
        let (_dir, conn) = fresh_conn();
        let source_id = sources::upsert_reddit_source(&conn, "rust").unwrap();

        let item = sample_item("abc123", "First post");
        record_seen(&conn, source_id, &[item.clone()]).expect("record_seen should succeed");

        let filtered =
            filter_unseen(&conn, source_id, vec![item]).expect("filter_unseen should succeed");

        assert!(
            filtered.is_empty(),
            "an item already seen for this source should be filtered out"
        );
    }

    #[test]
    fn filter_unseen_scopes_dedup_per_source_not_globally() {
        let (_dir, conn) = fresh_conn();
        let source_rust = sources::upsert_reddit_source(&conn, "rust").unwrap();
        let source_programming = sources::upsert_reddit_source(&conn, "programming").unwrap();

        let item = sample_item("abc123", "Crossposted post");
        record_seen(&conn, source_rust, &[item.clone()])
            .expect("record_seen for r/rust should succeed");

        // Seeing the item in r/rust must NOT suppress it in r/programming --
        // dedup is per-source, not global.
        let filtered = filter_unseen(&conn, source_programming, vec![item])
            .expect("filter_unseen for r/programming should succeed");

        assert_eq!(
            filtered.len(),
            1,
            "an item seen only under a different source must still be considered unseen here"
        );
    }

    #[test]
    fn record_seen_is_idempotent_and_does_not_create_duplicate_rows() {
        let (_dir, conn) = fresh_conn();
        let source_id = sources::upsert_reddit_source(&conn, "rust").unwrap();

        let item = sample_item("abc123", "First post");

        record_seen(&conn, source_id, &[item.clone()]).expect("first record_seen should succeed");
        record_seen(&conn, source_id, &[item.clone()])
            .expect("second record_seen should not error on duplicate");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM seen_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "no duplicate seen_items row should be created");
    }

    #[test]
    fn filter_unseen_returns_everything_when_nothing_recorded_yet() {
        let (_dir, conn) = fresh_conn();
        let source_id = sources::upsert_reddit_source(&conn, "rust").unwrap();

        let items = vec![
            sample_item("abc123", "First post"),
            sample_item("def456", "Second post"),
        ];

        let filtered =
            filter_unseen(&conn, source_id, items.clone()).expect("filter_unseen should succeed");

        assert_eq!(filtered, items);
    }
}
