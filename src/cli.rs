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
    /// Manage saved non-Reddit sources (RSS feeds, etc.). `drip source add`
    /// requires an existing (leaf sub-topic) `--topic` and creates one
    /// ruleless (accept-everything) link; `drip source link`/`drip source
    /// unlink` manage a source's links -- and each link's keyword rules --
    /// into further sub-topics from there (bd issue drip-ho5.8; see
    /// `Commands::Topic`'s doc comment for the two-level topic tree these
    /// link into).
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Manage topics: a two-level tree of main topics and their sub-topics
    /// (bd issue drip-ho5.8, migration `0006_topic_tree.sql`). A source
    /// links into one or more LEAF sub-topics -- never a main topic directly
    /// -- via `drip source add --topic`/`drip source link --topic`, each
    /// link carrying its own keyword rules (`--match`/`--exclude`) that
    /// route a fetched item into that sub-topic. This subcommand manages the
    /// topics themselves (create/rename/reparent/remove/list) and offers an
    /// offline explain surface (`drip topic test`); it does not manage
    /// source-to-topic links -- that's `drip source add`/`link`/`unlink`.
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

    /// Caps how many items are WRITTEN per source, applied AFTER dedup and
    /// keyword-rule classification, not before (bd issue drip-98u.4) -- the
    /// per-source pipeline is fetch -> dedup -> classify -> truncate, so
    /// this is "at most N items routed from this source", never "take the
    /// first N raw fetched items and see what routes" (truncating the raw
    /// feed first can leave zero routable items if a source's noisiest
    /// posts happen to sort first). Falls back to the saved `default_limit`
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
    /// daily_note_format, default_sort, default_limit, default_tags,
    /// reddit_request_delay_secs, reddit_retry_max, reddit_retry_base_secs)
    Set {
        /// Setting name (posts_folder, daily_notes_folder,
        /// daily_note_format, default_sort, default_limit, default_tags,
        /// reddit_request_delay_secs, reddit_retry_max,
        /// reddit_retry_base_secs)
        key: String,
        /// New value for the setting
        value: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SourceAction {
    /// Register a new non-Reddit source
    Add(SourceAddArgs),
    /// Declaratively (re)configure the link between a saved source and a
    /// sub-topic (bd issue drip-ho5.8). `--match`/`--exclude` REPLACE that
    /// link's entire include/exclude term lists -- re-running the same
    /// command is idempotent and produces identical state, which is what
    /// makes a shell script the reproducible way to author a source's rule
    /// set. Never touches the source's dedup ledger (`seen_items`): editing
    /// a link's rules is a plain row-level change against `link_rules`, not
    /// a remove-and-re-add of the source itself.
    Link(SourceLinkArgs),
    /// Remove the link (and its keyword rules) between a saved source and a
    /// sub-topic. A source with no remaining links still exists (it just
    /// routes nowhere until linked again) -- this never deletes the source
    /// itself.
    Unlink {
        /// Label of the source to unlink (see `drip source list`)
        #[arg(long)]
        name: String,
        /// Sub-topic to unlink from
        #[arg(long)]
        topic: String,
    },
    /// Remove a saved source
    Remove {
        #[arg(long)]
        name: String,
    },
    /// List saved sources, each with its links and their rules
    List,
}

#[derive(Debug, Clone, Args)]
pub struct SourceLinkArgs {
    /// Label of the source to link (see `drip source list`)
    #[arg(long)]
    pub name: String,
    /// Sub-topic to link into -- must already exist and be a LEAF sub-topic
    /// (create one with `drip topic add --name <sub-topic> --parent <main>`);
    /// linking directly to a main topic is rejected.
    #[arg(long)]
    pub topic: String,
    /// Include terms: REPLACES this link's entire include list wholesale.
    /// An item must match at least one to route here, unless the list is
    /// empty, in which case everything matches. Repeat the flag or pass a
    /// comma-separated list. Omitting this flag entirely clears the include
    /// list (declarative: this command always sets the link's FULL state,
    /// never appends to it).
    #[arg(long = "match", value_delimiter = ',')]
    pub match_terms: Vec<String>,
    /// Exclude terms: REPLACES this link's entire exclude list wholesale.
    /// Any match rejects the item outright, regardless of the include side.
    /// Repeat the flag or pass a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,
    /// Also match an item's body/summary text, not just its title, for this
    /// link specifically.
    #[arg(long = "match-body")]
    pub match_body: bool,
}

#[derive(Debug, Subcommand)]
pub enum TopicAction {
    /// Create a new topic
    Add {
        #[arg(long)]
        name: String,
        /// Create this as a sub-topic under an existing MAIN topic, rather
        /// than a new main topic. Topics are exactly two levels deep --
        /// naming a sub-topic here (one that itself already has a parent)
        /// is rejected.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Rename a topic. Future-notes-only: updates the DB but never rewrites
    /// an already-written digest note. If TODAY's digest note already has a
    /// section under the old name, this warns (a same-day fetch will add a
    /// second, differently-named section alongside it rather than updating
    /// the existing one) rather than rewriting the note.
    Rename {
        #[arg(long)]
        name: String,
        #[arg(long = "to")]
        to: String,
    },
    /// Move a sub-topic to a different main topic. Same future-notes-only
    /// warning as `rename` if today's digest note already has a section for
    /// it under its previous main topic.
    Reparent {
        /// The sub-topic to move
        #[arg(long)]
        name: String,
        /// Its new main topic (must already exist and not itself be a
        /// sub-topic)
        #[arg(long)]
        parent: String,
    },
    /// Remove a topic. Refuses while it has any descendant: a main topic
    /// refuses while it still has sub-topics; a topic (main or sub) refuses
    /// while it still has directly-linked sources -- unlink them first with
    /// `drip source unlink`.
    Remove {
        #[arg(long)]
        name: String,
    },
    /// List saved topics as a two-level tree, each with its member sources
    List,
    /// Offline, no-network explain surface (bd issue drip-ho5.8): classify a
    /// synthetic item (title only) against every saved source's sub-topic
    /// links, printing which links match, which terms fired, and where the
    /// item would land. Answers "why did nothing land in this sub-topic?"
    /// without spending a real fetch.
    Test {
        #[arg(long)]
        title: String,
    },
}

#[derive(Debug, Clone, Args)]
pub struct SourceAddArgs {
    #[arg(long, value_enum)]
    pub kind: SourceKind,
    /// The feed URL for `--kind rss`. For `--kind youtube`, also accepts a
    /// bare YouTube channel id (starts with "UC"), a
    /// https://www.youtube.com/channel/UC.../ URL, or a @handle /
    /// https://www.youtube.com/@handle URL -- see `src/youtube.rs` for how
    /// that gets resolved to the channel's Atom feed URL (a @handle
    /// resolves via a one-time network fetch at `source add` time; the
    /// other forms resolve with no network). For `--kind reddit`, this is
    /// the bare subreddit name (e.g. `rust`), not a URL -- see
    /// `src/reddit_feed.rs` for how that gets resolved to a subreddit
    /// RSS/Atom feed URL.
    #[arg(long)]
    pub url: String,
    #[arg(long)]
    pub name: String,

    /// The (leaf) sub-topic this source links into. Must already exist --
    /// create it with `drip topic add --name <sub-topic> --parent <main>`.
    /// Creates exactly one ruleless (accept-everything) link; use `drip
    /// source link` afterwards to add keyword rules or link into further
    /// sub-topics.
    #[arg(long)]
    pub topic: String,

    /// Source-level, title-only exclude terms -- a pre-filter applied
    /// before this source's items are matched against any sub-topic's
    /// rules at all. REPLACES the source's entire exclude list wholesale
    /// (declarative, same convention as `drip source link`'s `--match`/
    /// `--exclude`). Repeat the flag or pass a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,

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
