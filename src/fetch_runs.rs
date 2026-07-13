//! Fetch history/audit log against the `fetch_runs`/`fetch_run_sources`
//! tables (see `migrations/0001_init.sql`).
//!
//! Design context: bd issue drip-15n.9.5. The issue's own motivation is
//! debugging dedup behavior -- confirming why a given post did or didn't
//! show up in a digest -- so a fetch run is recorded for EVERY outcome,
//! including `--dry-run` runs and runs where dedup/`--min-score` filtered
//! everything out to zero new posts. A `--dry-run` is exactly the tool
//! someone reaches for to debug dedup without committing writes, and a
//! zero-result run is itself useful debugging signal ("did the fetch fail,
//! or did dedup correctly suppress everything?").
//!
//! This does not conflict with `--dry-run`'s "no real writes" contract:
//! precedent already exists in this codebase for benign bookkeeping writes
//! during `--dry-run` (`settings::load` seeds missing settings rows
//! unconditionally; `sources::upsert_reddit_source` already runs
//! unconditionally too, via `filter_fetched_posts`, called before the
//! `--dry-run` branch in `src/main.rs`). Recording fetch history is the same
//! category of write -- it never touches the vault, the journal, or
//! `seen_items` (dedup state), so it doesn't change what a later real fetch
//! would do.
//!
//! `digest_note_path` is `None`/NULL specifically when no file was actually
//! written (a `--dry-run` fetch, or the zero-new-posts early return);
//! callers must only pass `Some(path)` on the real, non-dry-run write path,
//! using the actual path `write_digest_note` returned.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// Record one `drip fetch` invocation's outcome: a single `fetch_runs` row
/// plus one `fetch_run_sources` row per `(source_id, item_count)` pair in
/// `per_source`. Both inserts run in a single transaction, so a `fetch_runs`
/// row is never left without its source breakdown rows.
///
/// `digest_note_path` must be `None` whenever no file was actually written
/// (a `--dry-run` fetch, or a fetch where nothing new survived
/// min-score/dedup filtering) and `Some(path)` only on the real write path.
/// `post_count` is the post-dedup count -- how many new items actually ended
/// up in (or, for `--dry-run`, would end up in) the digest, not the raw
/// fetch count. `started_at` is left to the table's own default.
pub fn record(
    conn: &Connection,
    digest_note_path: Option<&std::path::Path>,
    post_count: usize,
    per_source: &[(i64, usize)],
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("failed to start transaction for fetch run recording")?;

    let path_str = digest_note_path.map(|p| p.to_string_lossy().into_owned());
    tx.execute(
        "INSERT INTO fetch_runs (digest_note_path, post_count) VALUES (?1, ?2)",
        params![path_str, post_count as i64],
    )
    .context("failed to insert fetch_runs row")?;

    let fetch_run_id = tx.last_insert_rowid();

    for (source_id, item_count) in per_source {
        tx.execute(
            "INSERT INTO fetch_run_sources (fetch_run_id, source_id, item_count) \
             VALUES (?1, ?2, ?3)",
            params![fetch_run_id, source_id, *item_count as i64],
        )
        .with_context(|| {
            format!("failed to insert fetch_run_sources row for source_id {source_id}")
        })?;
    }

    tx.commit()
        .context("failed to commit fetch run recording")?;
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

    #[test]
    fn record_with_no_path_and_no_sources_creates_a_single_zero_row() {
        let (_dir, conn) = fresh_conn();

        record(&conn, None, 0, &[]).expect("record should succeed");

        let (path, post_count): (Option<String>, i64) = conn
            .query_row(
                "SELECT digest_note_path, post_count FROM fetch_runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("exactly one fetch_runs row should exist");
        assert_eq!(path, None, "digest_note_path should be NULL");
        assert_eq!(post_count, 0);

        let source_row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM fetch_run_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            source_row_count, 0,
            "no fetch_run_sources rows should be created"
        );
    }

    #[test]
    fn record_with_a_path_and_sources_stores_exact_values() {
        let (_dir, conn) = fresh_conn();
        let source_rust = sources::upsert_reddit_source(&conn, "rust").unwrap();
        let source_programming = sources::upsert_reddit_source(&conn, "programming").unwrap();

        let path = std::path::Path::new("/vault/Resources/Reddit/2026-07-09-digest.md");
        record(
            &conn,
            Some(path),
            7,
            &[(source_rust, 5), (source_programming, 2)],
        )
        .expect("record should succeed");

        let (stored_path, post_count): (Option<String>, i64) = conn
            .query_row(
                "SELECT digest_note_path, post_count FROM fetch_runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("exactly one fetch_runs row should exist");
        assert_eq!(
            stored_path,
            Some(path.to_string_lossy().into_owned()),
            "digest_note_path should store the exact path string"
        );
        assert_eq!(post_count, 7);

        let mut rows: Vec<(i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT source_id, item_count FROM fetch_run_sources ORDER BY source_id")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        rows.sort();

        let mut expected = vec![(source_rust, 5), (source_programming, 2)];
        expected.sort();

        assert_eq!(rows, expected);
    }

    #[test]
    fn record_is_append_only_not_an_upsert() {
        let (_dir, conn) = fresh_conn();

        record(&conn, None, 0, &[]).expect("first record should succeed");
        record(&conn, None, 3, &[]).expect("second record should succeed");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM fetch_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "each call to record should create an independent row"
        );
    }

    #[test]
    fn record_with_a_duplicate_source_id_in_per_source_returns_a_clean_error_not_a_panic() {
        let (_dir, conn) = fresh_conn();
        let source_rust = sources::upsert_reddit_source(&conn, "rust").unwrap();

        // `fetch_run_sources` has PRIMARY KEY(fetch_run_id, source_id), so a
        // `per_source` slice with the same `source_id` twice (the exact
        // scenario `main.rs`'s `handle_fetch` used to be able to produce
        // before it deduplicated/disambiguated its source keys -- see
        // drip-15n.9.6's hardening pass) must surface as a normal
        // `Result::Err` here, not a panic, even though the real fix
        // upstream in `main.rs` should make this unreachable in practice.
        let result = record(&conn, None, 5, &[(source_rust, 3), (source_rust, 2)]);

        assert!(
            result.is_err(),
            "a duplicate source_id in per_source should be a clean Err, not succeed or panic"
        );
    }

    #[test]
    fn deleting_a_source_cascades_to_its_fetch_run_sources_row_only() {
        let (_dir, conn) = fresh_conn();
        let source_rust = sources::upsert_reddit_source(&conn, "rust").unwrap();
        let source_programming = sources::upsert_reddit_source(&conn, "programming").unwrap();

        record(&conn, None, 3, &[(source_rust, 2), (source_programming, 1)])
            .expect("record should succeed");

        conn.execute("DELETE FROM sources WHERE id = ?1", params![source_rust])
            .unwrap();

        let fetch_run_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM fetch_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            fetch_run_count, 1,
            "the fetch_runs row itself must survive deleting one of its sources"
        );

        let remaining_source_ids: Vec<i64> = {
            let mut stmt = conn
                .prepare("SELECT source_id FROM fetch_run_sources")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            remaining_source_ids,
            vec![source_programming],
            "only the deleted source's fetch_run_sources row should be cascade-deleted"
        );
    }
}
