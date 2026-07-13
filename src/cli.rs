//! CLI surface for `drip`, defined with clap's derive API.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::types::{Sort, TimeFilter};

#[derive(Debug, Parser)]
#[command(
    name = "drip",
    version,
    about = "Fetch Reddit posts into your Obsidian vault",
    long_about = "drip fetches hot/trending Reddit posts from subreddits you choose, writes \
                  them as a digest note into your Obsidian vault, and links that note from \
                  your daily journal note."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Fetch posts from one or more subreddits and write a digest note
    Fetch(FetchArgs),
    /// Interactively set up drip for first use
    Init,
    /// View or edit the drip configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage saved fetch profiles
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Manage saved non-Reddit sources (RSS feeds, etc.)
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
}

#[derive(Debug, Clone, Args)]
pub struct FetchArgs {
    /// Subreddit(s) to fetch from. Repeat the flag or pass a comma-separated list.
    #[arg(short = 's', long = "subreddit", value_delimiter = ',')]
    pub subreddit: Vec<String>,

    /// Sort order for the fetched posts. Falls back to the saved
    /// `default_sort` setting when not given.
    #[arg(long, value_enum)]
    pub sort: Option<Sort>,

    /// Time window filter (only meaningful for top/controversial/search)
    #[arg(long, value_enum)]
    pub time: Option<TimeFilter>,

    /// Search query
    #[arg(short = 'q', long = "query")]
    pub query: Option<String>,

    /// Number of posts to fetch. Falls back to the saved `default_limit`
    /// setting when not given.
    #[arg(short = 'n', long = "limit")]
    pub limit: Option<u32>,

    /// Only include posts with at least this score
    #[arg(long = "min-score")]
    pub min_score: Option<i64>,

    /// Exclude NSFW items from the digest (default: NSFW items are included)
    #[arg(long = "no-nsfw")]
    pub no_nsfw: bool,

    /// Override the configured posts folder for this run
    #[arg(long)]
    pub folder: Option<String>,

    /// Tag(s) to add to the digest note. Repeat the flag or pass a comma-separated list.
    /// Falls back to the saved `default_tags` setting when not given.
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,

    /// Use a saved profile instead of (or as a base for) the flags above
    #[arg(long)]
    pub profile: Option<String>,

    /// Skip appending a reference to the daily journal note
    #[arg(long = "no-journal")]
    pub no_journal: bool,

    /// Print what would happen without writing anything
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Verbose logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Saved non-Reddit source label(s) to include in this fetch (see `drip
    /// source add`/`drip source list`). Repeat the flag or pass a
    /// comma-separated list.
    #[arg(long = "source", value_delimiter = ',')]
    pub source: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the current configuration
    Show,
    /// Open the configuration file in an editor
    Edit,
    /// Set a database-backed setting (posts_folder, daily_notes_folder,
    /// daily_note_format, default_sort, default_limit, default_tags)
    Set {
        /// Setting name (posts_folder, daily_notes_folder,
        /// daily_note_format, default_sort, default_limit, default_tags)
        key: String,
        /// New value for the setting
        value: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProfileAction {
    /// Save a new profile
    Add(ProfileAddArgs),
    /// Remove a saved profile
    Remove {
        /// Name of the profile to remove
        #[arg(long)]
        name: String,
    },
    /// List saved profiles
    List,
}

#[derive(Debug, Clone, Args)]
pub struct ProfileAddArgs {
    /// Name to save this profile under
    #[arg(long)]
    pub name: String,

    /// Subreddit(s) this profile fetches from. Repeat the flag or pass a comma-separated list.
    #[arg(short = 's', long = "subreddit", value_delimiter = ',')]
    pub subreddit: Vec<String>,

    /// Sort order for this profile
    #[arg(long, value_enum, default_value_t = Sort::Hot)]
    pub sort: Sort,

    /// Time window filter (only meaningful for top/controversial/search)
    #[arg(long, value_enum)]
    pub time: Option<TimeFilter>,

    /// Search query for this profile
    #[arg(short = 'q', long = "query")]
    pub query: Option<String>,

    /// Number of posts this profile fetches
    #[arg(short = 'n', long = "limit", default_value_t = 10)]
    pub limit: u32,

    /// Tag(s) this profile applies. Repeat the flag or pass a comma-separated list.
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum SourceAction {
    /// Register a new non-Reddit source
    Add(SourceAddArgs),
    /// Remove a saved source
    Remove {
        #[arg(long)]
        name: String,
    },
    /// List saved sources
    List,
}

#[derive(Debug, Clone, Args)]
pub struct SourceAddArgs {
    #[arg(long, value_enum)]
    pub kind: SourceKind,
    /// The feed URL for `--kind rss`. For `--kind youtube`, also accepts a
    /// bare YouTube channel id (starts with "UC") or a
    /// https://www.youtube.com/channel/UC.../ URL -- see `src/youtube.rs`
    /// for how that gets resolved to the channel's Atom feed URL. For
    /// `--kind reddit`, this is the bare subreddit name (e.g. `rust`), not a
    /// URL -- see `src/reddit_feed.rs` for how that gets resolved to a
    /// subreddit RSS/Atom feed URL.
    #[arg(long)]
    pub url: String,
    #[arg(long)]
    pub name: String,

    /// Sort order for this source (only meaningful with --kind reddit;
    /// ignored otherwise)
    #[arg(long, value_enum, default_value_t = Sort::Hot)]
    pub sort: Sort,

    /// Time window filter (only meaningful with --kind reddit and --sort
    /// top/controversial)
    #[arg(long, value_enum)]
    pub time: Option<TimeFilter>,

    /// Restrict to posts matching this search term within the subreddit
    /// (only meaningful with --kind reddit). This is a Reddit search query
    /// -- e.g. --search tasks finds posts mentioning "tasks" -- NOT a flair
    /// filter; flair isn't exposed by this feed.
    #[arg(long = "search")]
    pub search: Option<String>,
}

/// The kind of a `drip source add`-registered source. Deliberately its own
/// enum (rather than reusing `sources::kind` strings directly in the CLI
/// layer) so it can be extended (`Youtube`, added for bd issue drip-15n.9.7;
/// `Reddit`, added for bd issue drip-khu) without redesigning this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum SourceKind {
    Rss,
    Youtube,
    Reddit,
}
