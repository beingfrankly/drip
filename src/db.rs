//! SQLite connection opening + migration bootstrap for `drip`.
//!
//! Design context: bd issue drip-15n.9.2 (part of epic drip-15n.9,
//! "Migrate drip to SQLite-backed storage"). This module is purely the DB
//! plumbing -- opening a connection, running migrations, and making sure a
//! working, migrated, empty database exists. It does NOT migrate
//! `Config`'s settings fields (drip-15n.9.8), sources/profiles
//! (drip-15n.9.3), or dedup logic (drip-15n.9.4) -- those are separate,
//! later issues.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use rusqlite::Connection;

use crate::config::Config;

/// Ordered list of (target `user_version`, embedded SQL) pairs. Each
/// migration's SQL is embedded at compile time via `include_str!`, so the
/// binary is self-contained and doesn't need the `migrations/` directory to
/// exist at runtime on the user's machine after `cargo install`.
///
/// To add a future migration: append a new `(N, include_str!(...))` entry
/// with the next version number. The runner below applies any migration
/// whose version is greater than the database's current `user_version`, in
/// ascending order, so entries must stay sorted by version.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_profiles.sql")),
    (3, include_str!("../migrations/0003_source_labels.sql")),
    (4, include_str!("../migrations/0004_topics.sql")),
    (5, include_str!("../migrations/0005_source_topic.sql")),
];

/// Resolve the default location for `drip.db`: the same directory as
/// `Config::config_path()` (i.e. `ProjectDirs::from("", "", "drip")`'s
/// config dir), filename `drip.db`.
pub fn default_db_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "drip")
        .context("could not determine a config directory for this platform")?;
    Ok(proj_dirs.config_dir().join("drip.db"))
}

/// Resolve the effective database path for `config`: `config.db_path` if
/// set, else [`default_db_path`].
pub fn resolve_db_path(config: &Config) -> Result<PathBuf> {
    match &config.db_path {
        Some(path) => Ok(path.clone()),
        None => default_db_path(),
    }
}

/// Open (creating if necessary) the `drip` SQLite database for `config`,
/// enable foreign key enforcement on this connection, and bring the schema
/// up to date via [`MIGRATIONS`].
///
/// Foreign keys are turned on IMMEDIATELY after opening -- SQLite does not
/// persist `PRAGMA foreign_keys` in the database file itself, so it must be
/// set per-connection, every time, or the schema's `ON DELETE CASCADE`
/// clauses are silently unenforced (see `migrations/0001_init.sql`'s header
/// comment for the same gotcha, flagged at schema-design time).
pub fn open(config: &Config) -> Result<Connection> {
    let path = resolve_db_path(config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create database directory at {}",
                parent.display()
            )
        })?;
    }

    let conn = Connection::open(&path)
        .with_context(|| format!("failed to open database at {}", path.display()))?;

    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("failed to enable foreign key enforcement")?;

    run_migrations(&conn)
        .with_context(|| format!("failed to migrate database at {}", path.display()))?;

    Ok(conn)
}

/// Bring `conn`'s schema up to the latest version known to this binary
/// (the highest version in [`MIGRATIONS`]), applying any pending migrations
/// in ascending order, each inside its own transaction.
///
/// Migration files (like `migrations/0001_init.sql`) end with their own
/// `PRAGMA user_version = N;` statement, executed as part of the same
/// batch/transaction as the rest of that file's DDL, rather than having
/// this runner strip trailing PRAGMAs and set the version itself
/// afterwards. This is more robust against a future migration file with an
/// unexpected structure (e.g. one that sets `user_version` mid-file for its
/// own reasons, or one written by hand without realizing a runner would
/// silently overwrite its own version bump) -- the file's own statement is
/// the single source of truth for "what version does running this file
/// completely bring the DB to", and the runner only decides *whether* to
/// run it, never *what version to record*.
fn run_migrations(conn: &Connection) -> Result<()> {
    let current_version: i64 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;

    let latest_known_version = MIGRATIONS.iter().map(|(v, _)| *v).max().unwrap_or(0);
    if current_version > latest_known_version {
        bail!(
            "database is at user_version {current_version}, but this build of drip only knows \
             migrations up to version {latest_known_version}. Refusing to touch it -- you likely \
             need a newer version of drip."
        );
    }

    for (version, sql) in MIGRATIONS {
        if *version > current_version {
            conn.execute_batch(sql).with_context(|| {
                format!("failed to apply migration to reach user_version {version}")
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn config_with_db_path(path: PathBuf) -> Config {
        Config {
            db_path: Some(path),
            ..Config::default()
        }
    }

    #[test]
    fn open_on_fresh_path_creates_db_file_at_latest_user_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path.clone());

        let conn = open(&config).expect("open should succeed");

        assert!(db_path.exists());
        let version: i64 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn opening_an_already_migrated_db_a_second_time_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path.clone());

        let conn = open(&config).expect("first open should succeed");
        conn.execute(
            "INSERT INTO sources (kind, identifier) VALUES ('reddit', 'test')",
            [],
        )
        .expect("insert should succeed");
        drop(conn);

        let conn2 = open(&config).expect("second open should succeed and be a no-op");
        let version: i64 = conn2
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);

        let identifier: String = conn2
            .query_row(
                "SELECT identifier FROM sources WHERE kind = 'reddit'",
                [],
                |row| row.get(0),
            )
            .expect("previously inserted row should still exist");
        assert_eq!(identifier, "test");
    }

    #[test]
    fn open_enables_foreign_keys_on_the_returned_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path);

        let conn = open(&config).expect("open should succeed");

        let foreign_keys_on: i64 = conn
            .query_row("PRAGMA foreign_keys;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys_on, 1);
    }

    #[test]
    fn deleting_a_source_cascades_to_its_seen_items() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path);

        let conn = open(&config).expect("open should succeed");

        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (1, 'reddit', 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO seen_items (source_id, external_id) VALUES (1, 'abc123')",
            [],
        )
        .unwrap();

        let seen_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM seen_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(seen_count_before, 1);

        conn.execute("DELETE FROM sources WHERE id = 1", [])
            .unwrap();

        let seen_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM seen_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            seen_count_after, 0,
            "seen_items should be cascade-deleted with its source"
        );
    }

    #[test]
    fn open_migrates_a_fresh_db_to_user_version_5() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path);

        let conn = open(&config).expect("open should succeed");

        let version: i64 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn deleting_a_topic_cascades_to_its_topic_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path);

        let conn = open(&config).expect("open should succeed");

        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (1, 'reddit', 'rust')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO topics (id, name) VALUES (1, 'tech')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO topic_sources (topic_id, source_id) VALUES (1, 1)",
            [],
        )
        .unwrap();

        let topic_sources_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(topic_sources_count_before, 1);

        conn.execute("DELETE FROM topics WHERE id = 1", []).unwrap();

        let topic_sources_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            topic_sources_count_after, 0,
            "topic_sources should be cascade-deleted with its topic"
        );
    }

    #[test]
    fn deleting_a_source_cascades_to_its_topic_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = config_with_db_path(db_path);

        let conn = open(&config).expect("open should succeed");

        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (1, 'reddit', 'rust')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO topics (id, name) VALUES (1, 'tech')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO topic_sources (topic_id, source_id) VALUES (1, 1)",
            [],
        )
        .unwrap();

        let topic_sources_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(topic_sources_count_before, 1);

        conn.execute("DELETE FROM sources WHERE id = 1", [])
            .unwrap();

        let topic_sources_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            topic_sources_count_after, 0,
            "topic_sources should be cascade-deleted with its source"
        );
    }

    /// Exercise migration 0005's backfill directly, rather than through
    /// [`open`] (which always jumps straight to the latest `user_version`
    /// and so can never observe a database mid-way between 0004 and 0005).
    /// Drives a raw connection through migrations 1-4 by hand, seeds legacy
    /// `topic_sources` fixture data that predates the one-topic-per-source
    /// model, then applies migration 0005 (`MIGRATIONS[4]`) and asserts its
    /// three documented backfill behaviors (see
    /// `migrations/0005_source_topic.sql`'s header comment): a
    /// single-topic source keeps its topic, a multi-topic source collapses
    /// to its lowest topic id, and a topicless source lands in a
    /// newly-created "Uncategorized" topic -- and that `topic_sources`
    /// itself is left untouched (still present, purely inert going
    /// forward).
    #[test]
    fn migration_0005_backfills_topic_id_from_legacy_topic_sources() {
        let conn = Connection::open_in_memory().expect("failed to open in-memory connection");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("failed to enable foreign keys");

        // Apply migrations 1-4 by hand, in order, to reach the pre-0005
        // schema state.
        for (_version, sql) in &MIGRATIONS[0..4] {
            conn.execute_batch(sql)
                .expect("failed to apply a pre-0005 migration");
        }

        // Two legacy topics, explicit ids so the "collapses to the MIN
        // topic id" assertion below is unambiguous.
        conn.execute("INSERT INTO topics (id, name) VALUES (1, 'alpha')", [])
            .unwrap();
        conn.execute("INSERT INTO topics (id, name) VALUES (2, 'beta')", [])
            .unwrap();

        // (a) a source in exactly one topic.
        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (1, 'rss', 'single-topic')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_sources (topic_id, source_id) VALUES (1, 1)",
            [],
        )
        .unwrap();

        // (b) a source in TWO topics -- should collapse to the lowest
        // topic id (1, not 2).
        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (2, 'rss', 'multi-topic')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_sources (topic_id, source_id) VALUES (2, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_sources (topic_id, source_id) VALUES (1, 2)",
            [],
        )
        .unwrap();

        // (c) a source in NO topic -- should land in a newly-created
        // "Uncategorized" topic.
        conn.execute(
            "INSERT INTO sources (id, kind, identifier) VALUES (3, 'rss', 'no-topic')",
            [],
        )
        .unwrap();

        let topic_sources_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(topic_sources_count_before, 3);

        // Now apply migration 0005 itself.
        let (version, sql) = &MIGRATIONS[4];
        assert_eq!(*version, 5, "MIGRATIONS[4] should be the 0005 entry");
        conn.execute_batch(sql)
            .expect("failed to apply migration 0005");

        let topic_id_of = |source_id: i64| -> Option<i64> {
            conn.query_row(
                "SELECT topic_id FROM sources WHERE id = ?1",
                params![source_id],
                |row| row.get(0),
            )
            .unwrap()
        };

        assert_eq!(
            topic_id_of(1),
            Some(1),
            "a source already in exactly one topic should keep that topic"
        );
        assert_eq!(
            topic_id_of(2),
            Some(1),
            "a source in two topics should collapse to the MIN topic id"
        );

        let uncategorized_id: i64 = conn
            .query_row(
                "SELECT id FROM topics WHERE name = 'Uncategorized'",
                [],
                |row| row.get(0),
            )
            .expect("an 'Uncategorized' topic should have been created");
        assert_eq!(
            topic_id_of(3),
            Some(uncategorized_id),
            "a source with no legacy topic membership should land in 'Uncategorized'"
        );

        let every_source_has_a_topic: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sources WHERE topic_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            every_source_has_a_topic, 0,
            "no source should be left with a NULL topic_id after the backfill"
        );

        let topic_sources_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM topic_sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            topic_sources_count_after, 3,
            "topic_sources rows must survive the backfill untouched -- the table goes inert, \
             not dropped or cleared"
        );
    }
}
