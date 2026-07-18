//! CLI surface for `drip`, defined with clap's derive API.

use clap::{Args, Parser, Subcommand};

use crate::types::{Sort, SourceKind, TimeFilter};

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
    /// Manage saved non-Reddit sources (RSS feeds, etc.)
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Manage topics: named groups of saved sources
    Topic {
        #[command(subcommand)]
        action: TopicAction,
    },
    /// Check for and install a newer release
    Update(UpdateArgs),
}

#[derive(Debug, Clone, Args)]
pub struct FetchArgs {
    /// Sort label for the digest note's frontmatter/header. Falls back to
    /// the saved `default_sort` setting when not given. Does NOT filter or
    /// affect what's fetched -- for a Reddit source, control the actual
    /// sort at `drip source add --kind reddit --sort` time instead.
    #[arg(long, value_enum)]
    pub sort: Option<Sort>,

    /// Time window label for the digest note's frontmatter/header. Does NOT
    /// filter or affect what's fetched -- for a Reddit source, control the
    /// actual time window at `drip source add --kind reddit --time` time
    /// instead.
    #[arg(long, value_enum)]
    pub time: Option<TimeFilter>,

    /// Query label for the digest note's frontmatter/header. Does NOT
    /// search or affect what's fetched -- for a Reddit source, control the
    /// actual search term at `drip source add --kind reddit --search` time
    /// instead.
    #[arg(short = 'q', long = "query")]
    pub query: Option<String>,

    /// Number of posts to fetch. Falls back to the saved `default_limit`
    /// setting when not given.
    #[arg(short = 'n', long = "limit")]
    pub limit: Option<u32>,

    /// Override the configured posts folder for this run
    #[arg(long)]
    pub folder: Option<String>,

    /// Tag(s) to add to the digest note. Repeat the flag or pass a comma-separated list.
    /// Falls back to the saved `default_tags` setting when not given.
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,

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

    /// Saved topic name(s) to include in this fetch (see `drip topic add`/
    /// `drip topic list`). Repeat the flag or pass a comma-separated list.
    /// Each named topic is resolved into its member sources' labels, which
    /// are then merged with any `--source` labels given in the same
    /// invocation -- a source named by both `--source` and a `--topic` it
    /// belongs to is still fetched exactly once, not twice.
    #[arg(long = "topic", value_delimiter = ',')]
    pub topic: Vec<String>,

    /// Fetch every saved source (see `drip source list`), ignoring the need
    /// for explicit `--source`/`--topic` selection. Merges with any
    /// `--source`/`--topic` also given (a source selected more than one way
    /// is still fetched exactly once). With no saved sources, prints a clear
    /// message and does nothing.
    #[arg(long = "all")]
    pub all: bool,
}

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Check for a newer version without downloading or installing it
    #[arg(long)]
    pub check: bool,
    /// Skip the confirmation prompt before installing
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Verbose logging
    #[arg(short = 'v', long)]
    pub verbose: bool,
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

#[derive(Debug, Subcommand)]
pub enum TopicAction {
    /// Create a new topic
    Add {
        #[arg(long)]
        name: String,
    },
    /// Add a saved source to a topic
    AddSource {
        #[arg(long)]
        topic: String,
        #[arg(long)]
        source: String,
    },
    /// Remove a saved source from a topic
    RemoveSource {
        #[arg(long)]
        topic: String,
        #[arg(long)]
        source: String,
    },
    /// Remove a topic
    Remove {
        #[arg(long)]
        name: String,
    },
    /// List saved topics and their member sources
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
