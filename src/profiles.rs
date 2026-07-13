//! DB-backed replacement for `Config.profiles: Vec<Profile>` (see
//! `src/config.rs`'s history) -- named multi-subreddit fetch presets,
//! selectable via `drip fetch --profile <name>` and managed with
//! `drip profile add/remove/list`.
//!
//! Design context: bd issue drip-15n.9.3. A profile is a different
//! granularity than a `sources` row (drip-15n.9.1): a profile spans multiple
//! subreddits under one shared sort/time/query/limit/tags preset, while
//! `sources` models one row per individual feed. This module layers
//! `profiles` + the `profile_sources`/`profile_tags` join tables
//! (`migrations/0002_profiles.sql`) on top of the existing `sources` table
//! rather than replacing it.

use anyhow::{Context, Result};
use clap::ValueEnum;
use rusqlite::{params, Connection};

use crate::sources;
use crate::types::{Sort, TimeFilter};

/// A saved profile's fetch parameters, fully resolved from the database --
/// subreddits joined in from `sources` via `profile_sources`, tags joined in
/// from `profile_tags`. Shaped to be as close to a drop-in replacement for
/// the old TOML-based `Profile` struct as possible, so `resolve_fetch_params`
/// in `src/main.rs` barely has to change.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProfile {
    pub subreddits: Vec<String>,
    pub sort: Sort,
    pub time: Option<TimeFilter>,
    pub query: Option<String>,
    pub limit: u32,
    pub tags: Vec<String>,
}

/// Default fetch limit when a profile row's `fetch_limit` is somehow NULL.
/// In practice `upsert` always writes a concrete limit (the CLI always has
/// one, defaulting to 10) -- this only matters for a hand-edited or
/// otherwise unusual row.
fn default_limit() -> u32 {
    10
}

/// Decode a `profiles.sort` TEXT value (nullable in the schema even though
/// `upsert` always writes one) into a [`Sort`], defaulting to `Sort::Hot`
/// when NULL and erroring clearly if the stored value isn't a valid sort.
fn decode_sort(raw: Option<String>) -> Result<Sort> {
    match raw {
        Some(raw) => Sort::parse(&raw)
            .with_context(|| format!("stored profile sort '{raw}' is not a valid sort")),
        None => Ok(Sort::default()),
    }
}

/// Decode a `profiles.time_filter` TEXT value into an `Option<TimeFilter>`,
/// erroring clearly if a non-NULL stored value isn't a valid time filter.
fn decode_time(raw: Option<String>) -> Result<Option<TimeFilter>> {
    match raw {
        Some(raw) => {
            let time = TimeFilter::from_str(&raw, false).map_err(|err| {
                anyhow::anyhow!(
                    "stored profile time filter '{raw}' is not a valid time filter: {err}"
                )
            })?;
            Ok(Some(time))
        }
        None => Ok(None),
    }
}

/// Create the `profiles` row for `name` if it doesn't exist yet, or replace
/// its `sort`/`time_filter`/`query`/`fetch_limit` in place if it does
/// (matching the old TOML `upsert_profile`'s replace-rather-than-duplicate
/// semantics). Every subreddit in `subreddits` gets a `sources` row (created
/// via [`sources::upsert_reddit_source`] if missing), and the profile's
/// `profile_sources`/`profile_tags` links are replaced entirely -- old links
/// for this profile are deleted and the new set inserted -- so re-running
/// `profile add` with a different subreddit list or tag set on an existing
/// name doesn't leave stale links around. Runs in a single transaction.
pub fn upsert(
    conn: &Connection,
    name: &str,
    subreddits: &[String],
    sort: Sort,
    time: Option<TimeFilter>,
    query: Option<&str>,
    limit: u32,
    tags: &[String],
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("failed to start transaction for profile upsert")?;

    let time_str = time.map(|t| t.as_str());
    tx.execute(
        "INSERT INTO profiles (name, sort, time_filter, query, fetch_limit) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(name) DO UPDATE SET \
             sort = excluded.sort, \
             time_filter = excluded.time_filter, \
             query = excluded.query, \
             fetch_limit = excluded.fetch_limit",
        params![name, sort.as_str(), time_str, query, limit],
    )
    .with_context(|| format!("failed to upsert profile '{name}'"))?;

    let profile_id: i64 = tx
        .query_row(
            "SELECT id FROM profiles WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up profile id for '{name}' after upsert"))?;

    tx.execute(
        "DELETE FROM profile_sources WHERE profile_id = ?1",
        params![profile_id],
    )
    .with_context(|| format!("failed to clear existing subreddit links for profile '{name}'"))?;
    tx.execute(
        "DELETE FROM profile_tags WHERE profile_id = ?1",
        params![profile_id],
    )
    .with_context(|| format!("failed to clear existing tags for profile '{name}'"))?;

    for subreddit in subreddits {
        let source_id = sources::upsert_reddit_source(&tx, subreddit)?;
        tx.execute(
            "INSERT INTO profile_sources (profile_id, source_id) VALUES (?1, ?2)",
            params![profile_id, source_id],
        )
        .with_context(|| format!("failed to link subreddit '{subreddit}' to profile '{name}'"))?;
    }

    for tag in tags {
        tx.execute(
            "INSERT INTO profile_tags (profile_id, tag) VALUES (?1, ?2)",
            params![profile_id, tag],
        )
        .with_context(|| format!("failed to link tag '{tag}' to profile '{name}'"))?;
    }

    tx.commit()
        .with_context(|| format!("failed to commit upsert of profile '{name}'"))?;
    Ok(())
}

/// Delete the `profiles` row named `name`. Returns `true` if a row was
/// deleted, `false` if no profile had that name (mirroring the old TOML
/// `remove_profile`'s bool-returning contract).
///
/// Relies on `ON DELETE CASCADE` (and `PRAGMA foreign_keys = ON`, guaranteed
/// by `db::open`) to clean up `profile_sources`/`profile_tags` rows for the
/// removed profile. The underlying `sources` rows those subreddits had are
/// deliberately left in place -- a subreddit might independently matter
/// later (dedup, a future general subscription concept) even without a
/// profile referencing it.
pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let changed = conn
        .execute("DELETE FROM profiles WHERE name = ?1", params![name])
        .with_context(|| format!("failed to remove profile '{name}'"))?;
    Ok(changed > 0)
}

/// Look up the profile named `name`, fully resolved (subreddits/tags joined
/// in). Returns `None` if no profile has that name.
pub fn find(conn: &Connection, name: &str) -> Result<Option<ResolvedProfile>> {
    let row = conn.query_row(
        "SELECT id, sort, time_filter, query, fetch_limit FROM profiles WHERE name = ?1",
        params![name],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        },
    );

    let (profile_id, sort_raw, time_raw, query, limit_raw) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to look up profile '{name}'")),
    };

    let resolved = build_resolved(conn, profile_id, sort_raw, time_raw, query, limit_raw)?;
    Ok(Some(resolved))
}

/// List every saved profile, fully resolved, ordered by name.
pub fn list(conn: &Connection) -> Result<Vec<(String, ResolvedProfile)>> {
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> = {
        let mut stmt = conn
            .prepare("SELECT id, name, sort, time_filter, query, fetch_limit FROM profiles ORDER BY name")
            .context("failed to prepare profile list query")?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to list profiles")?
    };

    let mut result = Vec::with_capacity(rows.len());
    for (id, name, sort_raw, time_raw, query, limit_raw) in rows {
        let resolved = build_resolved(conn, id, sort_raw, time_raw, query, limit_raw)?;
        result.push((name, resolved));
    }
    Ok(result)
}

/// Shared "join in subreddits/tags and decode sort/time" step used by both
/// [`find`] and [`list`].
fn build_resolved(
    conn: &Connection,
    profile_id: i64,
    sort_raw: Option<String>,
    time_raw: Option<String>,
    query: Option<String>,
    limit_raw: Option<i64>,
) -> Result<ResolvedProfile> {
    let subreddits = load_subreddits(conn, profile_id)?;
    let tags = load_tags(conn, profile_id)?;
    let sort = decode_sort(sort_raw)?;
    let time = decode_time(time_raw)?;
    let limit = limit_raw.map(|l| l as u32).unwrap_or_else(default_limit);

    Ok(ResolvedProfile {
        subreddits,
        sort,
        time,
        query,
        limit,
        tags,
    })
}

fn load_subreddits(conn: &Connection, profile_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT s.identifier FROM profile_sources ps \
             JOIN sources s ON s.id = ps.source_id \
             WHERE ps.profile_id = ?1 ORDER BY s.identifier",
        )
        .context("failed to prepare profile subreddits query")?;
    let rows = stmt.query_map(params![profile_id], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<String>>>()
        .context("failed to load profile's subreddits")
}

fn load_tags(conn: &Connection, profile_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT tag FROM profile_tags WHERE profile_id = ?1 ORDER BY tag")
        .context("failed to prepare profile tags query")?;
    let rows = stmt.query_map(params![profile_id], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<String>>>()
        .context("failed to load profile's tags")
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

    fn subs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn upsert_then_find_round_trips_a_fresh_profile() {
        let (_dir, conn) = fresh_conn();

        upsert(
            &conn,
            "weekly-rust",
            &subs(&["rust", "programming"]),
            Sort::Top,
            Some(TimeFilter::Week),
            None,
            25,
            &subs(&["rust"]),
        )
        .expect("upsert should succeed");

        let resolved = find(&conn, "weekly-rust")
            .expect("find should succeed")
            .expect("profile should exist");

        assert_eq!(
            resolved.subreddits,
            vec!["programming".to_string(), "rust".to_string()]
        );
        assert_eq!(resolved.sort, Sort::Top);
        assert_eq!(resolved.time, Some(TimeFilter::Week));
        assert_eq!(resolved.query, None);
        assert_eq!(resolved.limit, 25);
        assert_eq!(resolved.tags, vec!["rust".to_string()]);
    }

    #[test]
    fn find_returns_none_for_unknown_profile() {
        let (_dir, conn) = fresh_conn();

        let resolved = find(&conn, "does-not-exist").expect("find should succeed");
        assert!(resolved.is_none());
    }

    #[test]
    fn upsert_with_same_name_replaces_sources_and_tags_entirely() {
        let (_dir, conn) = fresh_conn();

        upsert(
            &conn,
            "weekly-rust",
            &subs(&["rust", "programming"]),
            Sort::Top,
            Some(TimeFilter::Week),
            None,
            25,
            &subs(&["rust"]),
        )
        .expect("first upsert should succeed");

        upsert(
            &conn,
            "weekly-rust",
            &subs(&["golang"]),
            Sort::Hot,
            None,
            None,
            10,
            &subs(&["go", "weekly"]),
        )
        .expect("second upsert should succeed");

        let resolved = find(&conn, "weekly-rust")
            .expect("find should succeed")
            .expect("profile should still exist");

        assert_eq!(resolved.subreddits, vec!["golang".to_string()]);
        assert_eq!(resolved.sort, Sort::Hot);
        assert_eq!(resolved.time, None);
        assert_eq!(resolved.limit, 10);
        assert_eq!(resolved.tags, vec!["go".to_string(), "weekly".to_string()]);

        // Only one profiles row should exist -- this was a replace, not an
        // append.
        let profile_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(profile_count, 1);
    }

    #[test]
    fn remove_deletes_the_profile_but_not_its_underlying_sources() {
        let (_dir, conn) = fresh_conn();

        upsert(
            &conn,
            "weekly-rust",
            &subs(&["rust", "programming"]),
            Sort::Top,
            Some(TimeFilter::Week),
            None,
            25,
            &subs(&["rust"]),
        )
        .expect("upsert should succeed");

        let removed = remove(&conn, "weekly-rust").expect("remove should succeed");
        assert!(removed);

        let resolved = find(&conn, "weekly-rust").expect("find should succeed");
        assert!(resolved.is_none(), "profile row should be gone");

        let source_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sources WHERE identifier IN ('rust', 'programming')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            source_count, 2,
            "underlying sources rows must survive profile removal"
        );
    }

    #[test]
    fn remove_returns_false_and_changes_nothing_for_unknown_name() {
        let (_dir, conn) = fresh_conn();

        upsert(&conn, "a", &subs(&["rust"]), Sort::Hot, None, None, 10, &[]).unwrap();

        let removed = remove(&conn, "does-not-exist").expect("remove should succeed");
        assert!(!removed);

        let profile_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(profile_count, 1);
    }

    #[test]
    fn list_reflects_multiple_saved_profiles() {
        let (_dir, conn) = fresh_conn();

        upsert(
            &conn,
            "weekly-rust",
            &subs(&["rust", "programming"]),
            Sort::Top,
            Some(TimeFilter::Week),
            None,
            25,
            &subs(&["rust"]),
        )
        .unwrap();
        upsert(
            &conn,
            "daily-news",
            &subs(&["worldnews"]),
            Sort::New,
            None,
            None,
            5,
            &[],
        )
        .unwrap();

        let profiles = list(&conn).expect("list should succeed");

        assert_eq!(profiles.len(), 2);
        // Ordered by name: "daily-news" < "weekly-rust".
        assert_eq!(profiles[0].0, "daily-news");
        assert_eq!(profiles[0].1.subreddits, vec!["worldnews".to_string()]);
        assert_eq!(profiles[1].0, "weekly-rust");
        assert_eq!(profiles[1].1.tags, vec!["rust".to_string()]);
    }
}
