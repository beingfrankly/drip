//! Shared value types used by both the CLI parser (clap) and the config
//! file (serde/TOML). Keeping them in one place ensures the CLI's
//! `--sort hot` style values line up exactly with what gets persisted to
//! `config.toml`.

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
    /// Lowercase string form used when building Reddit API URLs
    /// (e.g. `https://oauth.reddit.com/r/{sub}/{sort}`), and when encoding
    /// this value for storage in the `settings` table (see
    /// [`crate::settings`]).
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
