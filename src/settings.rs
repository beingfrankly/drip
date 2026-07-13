//! Key-value settings stored in the SQLite `settings` table (see
//! `migrations/0001_init.sql`).
//!
//! Design context: bd issue drip-15n.9.8 (part of epic drip-15n.9,
//! "Migrate drip to SQLite-backed storage"). These are the fields that used
//! to live on `Config` (`posts_folder`, `daily_notes_folder`,
//! `daily_note_format`, `default_sort`, `default_limit`, `default_tags`)
//! before this issue moved them out of `config.toml` and into the database.
//! Only `vault_path` and `db_path` remain on `Config`, because the DB's own
//! location has to be resolvable before the DB can be opened -- that's the
//! one thing that can never live inside the DB itself.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::types::Sort;

/// The six known setting keys. Exposed so callers (e.g. the `drip config
/// set` CLI validator) can list valid keys without duplicating this list.
pub const KEYS: [&str; 6] = [
    "posts_folder",
    "daily_notes_folder",
    "daily_note_format",
    "default_sort",
    "default_limit",
    "default_tags",
];

/// The settings-table-backed configuration fields that used to live on
/// `Config`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub posts_folder: String,
    pub daily_notes_folder: String,
    pub daily_note_format: String,
    pub default_sort: Sort,
    pub default_limit: u32,
    pub default_tags: Vec<String>,
}

fn default_posts_folder() -> String {
    "Resources/Reddit".to_string()
}

fn default_daily_notes_folder() -> String {
    "Journal/Daily notes".to_string()
}

fn default_daily_note_format() -> String {
    "%Y-%m-%d".to_string()
}

fn default_sort() -> Sort {
    Sort::Hot
}

fn default_limit() -> u32 {
    10
}

fn default_tags() -> Vec<String> {
    vec!["reddit".to_string()]
}

/// Read the raw text value stored for `key`, if any. This is the low-level
/// primitive both [`load`] and the CLI-facing setter build on -- it does no
/// validation of `key` itself.
pub fn get_raw(conn: &Connection, key: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    );

    match result {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read setting '{key}'")),
    }
}

/// Upsert `value` for `key`. This is the low-level primitive both [`load`]'s
/// seeding step and the CLI-facing `drip config set` build on -- it does no
/// validation of `key` or `value` itself; that's the caller's job (see
/// `validate_and_encode` for the CLI-facing validation).
pub fn set_raw(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .with_context(|| format!("failed to write setting '{key}'"))?;
    Ok(())
}

/// Seed any of the six known setting keys that are MISSING with their
/// hardcoded defaults, via `INSERT OR IGNORE` so an already-present value
/// (e.g. one a user customized) is never clobbered. Safe to call on every
/// [`load`] -- a fresh DB gets a complete set of defaults, and a DB that
/// predates a newly-added setting key in some future version gets just the
/// missing key filled in.
fn seed_missing(conn: &Connection) -> Result<()> {
    let defaults: [(&str, String); 6] = [
        ("posts_folder", default_posts_folder()),
        ("daily_notes_folder", default_daily_notes_folder()),
        ("daily_note_format", default_daily_note_format()),
        ("default_sort", default_sort().as_str().to_string()),
        ("default_limit", default_limit().to_string()),
        (
            "default_tags",
            serde_json::to_string(&default_tags())
                .context("failed to encode default tags as JSON")?,
        ),
    ];

    for (key, value) in defaults {
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .with_context(|| format!("failed to seed default for setting '{key}'"))?;
    }

    Ok(())
}

/// Seed any missing setting keys with their hardcoded defaults (see
/// [`seed_missing`]), then read all six back into a populated [`Settings`].
pub fn load(conn: &Connection) -> Result<Settings> {
    seed_missing(conn)?;

    let posts_folder = get_raw(conn, "posts_folder")?
        .context("missing 'posts_folder' setting after seeding defaults")?;
    let daily_notes_folder = get_raw(conn, "daily_notes_folder")?
        .context("missing 'daily_notes_folder' setting after seeding defaults")?;
    let daily_note_format = get_raw(conn, "daily_note_format")?
        .context("missing 'daily_note_format' setting after seeding defaults")?;

    let default_sort_raw = get_raw(conn, "default_sort")?
        .context("missing 'default_sort' setting after seeding defaults")?;
    let default_sort = Sort::parse(&default_sort_raw).with_context(|| {
        format!("stored 'default_sort' setting '{default_sort_raw}' is not a valid sort")
    })?;

    let default_limit_raw = get_raw(conn, "default_limit")?
        .context("missing 'default_limit' setting after seeding defaults")?;
    let default_limit: u32 = default_limit_raw.parse().with_context(|| {
        format!("stored 'default_limit' setting '{default_limit_raw}' is not a valid number")
    })?;

    let default_tags_raw = get_raw(conn, "default_tags")?
        .context("missing 'default_tags' setting after seeding defaults")?;
    let default_tags: Vec<String> = serde_json::from_str(&default_tags_raw).with_context(|| {
        format!("stored 'default_tags' setting '{default_tags_raw}' is not valid JSON")
    })?;

    Ok(Settings {
        posts_folder,
        daily_notes_folder,
        daily_note_format,
        default_sort,
        default_limit,
        default_tags,
    })
}

/// Validate that `key` is one of the six known setting names, and that
/// `value` parses correctly for that key's type, returning the encoded
/// string that should actually be stored (identical to `value` for the
/// plain string keys; a normalized encoding for
/// `default_sort`/`default_limit`/`default_tags`).
///
/// Used by the `drip config set` CLI handler so invalid input never reaches
/// the database, even though `value` is a plain TEXT column that would
/// technically accept anything.
pub fn validate_and_encode(key: &str, value: &str) -> Result<String> {
    match key {
        "posts_folder" | "daily_notes_folder" | "daily_note_format" => Ok(value.to_string()),
        "default_sort" => {
            let sort = Sort::parse(value).with_context(|| {
                format!(
                    "'{value}' is not a valid sort (expected one of: hot, top, new, rising, controversial)"
                )
            })?;
            Ok(sort.as_str().to_string())
        }
        "default_limit" => {
            let limit: u32 = value.parse().with_context(|| {
                format!("'{value}' is not a valid limit (expected a non-negative whole number)")
            })?;
            Ok(limit.to_string())
        }
        "default_tags" => {
            let tags: Vec<String> = serde_json::from_str(value).with_context(|| {
                format!(
                    "'{value}' is not a valid JSON array of strings (e.g. [\"reddit\",\"rust\"])"
                )
            })?;
            serde_json::to_string(&tags).context("failed to re-encode default tags as JSON")
        }
        other => {
            anyhow::bail!(
                "unknown setting '{other}' (valid keys: {})",
                KEYS.join(", ")
            )
        }
    }
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
    fn load_on_fresh_db_returns_hardcoded_defaults() {
        let (_dir, conn) = fresh_conn();

        let settings = load(&conn).expect("load should succeed");

        assert_eq!(settings.posts_folder, "Resources/Reddit");
        assert_eq!(settings.daily_notes_folder, "Journal/Daily notes");
        assert_eq!(settings.daily_note_format, "%Y-%m-%d");
        assert_eq!(settings.default_sort, Sort::Hot);
        assert_eq!(settings.default_limit, 10);
        assert_eq!(settings.default_tags, vec!["reddit".to_string()]);
    }

    #[test]
    fn load_twice_after_mutation_reflects_change_without_reseeding_default() {
        let (_dir, conn) = fresh_conn();

        let first = load(&conn).expect("first load should succeed");
        assert_eq!(first.posts_folder, "Resources/Reddit");

        set_raw(&conn, "posts_folder", "Custom/Folder").expect("set_raw should succeed");

        let second = load(&conn).expect("second load should succeed");
        assert_eq!(
            second.posts_folder, "Custom/Folder",
            "second load must reflect the mutation, not reset it back to default"
        );
        // Everything else should still be untouched defaults.
        assert_eq!(second.daily_notes_folder, "Journal/Daily notes");
    }

    #[test]
    fn round_trips_default_sort_through_set_raw_and_load() {
        let (_dir, conn) = fresh_conn();

        set_raw(&conn, "default_sort", "top").expect("set_raw should succeed");

        let settings = load(&conn).expect("load should succeed");
        assert_eq!(settings.default_sort, Sort::Top);
    }

    #[test]
    fn round_trips_default_tags_through_set_raw_and_load() {
        let (_dir, conn) = fresh_conn();

        let tags = vec!["rust".to_string(), "programming".to_string()];
        set_raw(
            &conn,
            "default_tags",
            &serde_json::to_string(&tags).unwrap(),
        )
        .expect("set_raw should succeed");

        let settings = load(&conn).expect("load should succeed");
        assert_eq!(settings.default_tags, tags);
    }

    #[test]
    fn validate_and_encode_accepts_valid_key_and_value() {
        let encoded = validate_and_encode("default_sort", "top").expect("should be valid");
        assert_eq!(encoded, "top");

        let encoded =
            validate_and_encode("posts_folder", "Custom/Folder").expect("should be valid");
        assert_eq!(encoded, "Custom/Folder");
    }

    #[test]
    fn validate_and_encode_rejects_unknown_key_clearly() {
        let err = validate_and_encode("not_a_real_key", "whatever").expect_err("should error");
        let message = err.to_string();
        assert!(
            message.contains("unknown setting 'not_a_real_key'"),
            "{message}"
        );
        assert!(message.contains("posts_folder"), "{message}");
    }

    #[test]
    fn validate_and_encode_rejects_invalid_value_for_known_key() {
        let err = validate_and_encode("default_limit", "not-a-number").expect_err("should error");
        assert!(err.to_string().contains("not a valid limit"));

        let err = validate_and_encode("default_sort", "bogus").expect_err("should error");
        assert!(err.to_string().contains("not a valid sort"));
    }
}
