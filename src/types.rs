//! Shared value types used by both the CLI parser (clap) and the domain/DB
//! layer. Keeping them in one place ensures the CLI's `--sort hot` style
//! values line up exactly with what gets persisted to `config.toml`
//! (`Sort`/`TimeFilter`) or to a `sources.kind` TEXT column (`SourceKind`).

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Reddit listing sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Hot,
    Top,
    New,
    Rising,
    Controversial,
}

impl Sort {
    /// Lowercase string form used when building a subreddit's RSS/Atom feed
    /// URL (e.g. `https://www.reddit.com/r/{sub}/{sort}/.rss`, see
    /// [`crate::reddit_feed`]), and when encoding this value for storage in
    /// the `settings` table (see [`crate::settings`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            Sort::Hot => "hot",
            Sort::Top => "top",
            Sort::New => "new",
            Sort::Rising => "rising",
            Sort::Controversial => "controversial",
        }
    }

    /// Parse the lowercase string form produced by [`Sort::as_str`] back
    /// into a `Sort`. Returns `None` for anything else (case-sensitive,
    /// matching `as_str()`'s output exactly) -- this is a small hand-rolled
    /// counterpart to `as_str()`, not clap's `ValueEnum::from_str`, since
    /// this is used for settings-table storage round-tripping rather than
    /// CLI argument parsing.
    pub fn parse(s: &str) -> Option<Sort> {
        match s {
            "hot" => Some(Sort::Hot),
            "top" => Some(Sort::Top),
            "new" => Some(Sort::New),
            "rising" => Some(Sort::Rising),
            "controversial" => Some(Sort::Controversial),
            _ => None,
        }
    }
}

/// Time window filter, only meaningful for `top`/`controversial`/search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum TimeFilter {
    Hour,
    Day,
    Week,
    Month,
    Year,
    All,
}

impl TimeFilter {
    /// Lowercase string form used for the Reddit API's `t` query parameter.
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeFilter::Hour => "hour",
            TimeFilter::Day => "day",
            TimeFilter::Week => "week",
            TimeFilter::Month => "month",
            TimeFilter::Year => "year",
            TimeFilter::All => "all",
        }
    }
}

/// The kind of a `sources` row: `reddit`, `rss`, or `youtube`. Shared
/// between the CLI parser (`drip source add --kind` accepts these via
/// clap's `ValueEnum`, see `src/cli.rs`'s `SourceAddArgs`) and the
/// domain/DB layer (`SourceRow.kind` in `src/sources.rs`, `SourceGroup.kind`
/// in `src/digest.rs`), rather than flattening to `String`/`&str` the
/// moment it crosses into non-CLI code (bd issue drip-p6v.1). This is what
/// lets `handle_fetch`'s dispatch (`src/main.rs`) be an exhaustive `match`
/// over the three known kinds, compiler-enforced to stay exhaustive if a
/// fourth kind is ever added, instead of a runtime string match with a
/// catch-all "unsupported kind" arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum SourceKind {
    Rss,
    Youtube,
    Reddit,
}

impl SourceKind {
    /// Lowercase string form stored in the `sources.kind` TEXT column (see
    /// `migrations/0001_init.sql`'s `kind IN ('reddit', 'rss', 'youtube')`
    /// CHECK constraint) -- SQLite has no enum type, so this hand-written
    /// method (mirroring [`Sort::as_str`]) is the String<->enum conversion
    /// boundary, not a serde derive.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Rss => "rss",
            SourceKind::Youtube => "youtube",
            SourceKind::Reddit => "reddit",
        }
    }

    /// Parse the lowercase string form produced by [`SourceKind::as_str`]
    /// (i.e. a `sources.kind` column value) back into a `SourceKind`.
    /// Returns `None` for anything else (case-sensitive, matching
    /// `as_str()`'s output exactly) -- mirrors [`Sort::parse`].
    pub fn parse(s: &str) -> Option<SourceKind> {
        match s {
            "rss" => Some(SourceKind::Rss),
            "youtube" => Some(SourceKind::Youtube),
            "reddit" => Some(SourceKind::Reddit),
            _ => None,
        }
    }

    /// Whether this source kind should get Reddit-specific *rendering*
    /// treatment in digest/journal output: `r/{name}` headings and labels
    /// (see [`SourceKind::heading_prefix`]), a `u/{name}` author prefix, and
    /// a `reddit/{name}` tag. This is the single source of truth for that
    /// decision (bd issue drip-p6v.4) -- `src/digest.rs` and
    /// `src/journal.rs` call this (directly, or via
    /// [`SourceKind::heading_prefix`]) instead of each re-deriving
    /// `kind == SourceKind::Reddit` at their own rendering call sites.
    ///
    /// Not to be confused with `src/main.rs`'s exhaustive
    /// `match source_row.kind { .. }` fetch dispatch (bd issue drip-p6v.1) --
    /// that's a different concern (which fetcher to call), left untouched
    /// by this method.
    pub fn is_reddit(&self) -> bool {
        matches!(self, SourceKind::Reddit)
    }

    /// `name` rendered with this kind's display-label convention: `r/{name}`
    /// for Reddit-origin groups (matching Reddit's own subreddit naming),
    /// bare `{name}` for everything else. Used by `src/digest.rs`'s header
    /// summary line and per-group heading, and by `src/journal.rs`'s digest
    /// bullet -- all three want exactly the same `r/`-vs-bare choice.
    pub fn heading_prefix(&self, name: &str) -> String {
        if self.is_reddit() {
            format!("r/{name}")
        } else {
            name.to_string()
        }
    }
}
