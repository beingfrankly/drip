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
        assert_eq!(version, 3);
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
        assert_eq!(version, 3);

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
}
