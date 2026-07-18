mod cli;
mod config;
mod cron;
mod db;
mod dedup;
mod digest;
mod fetch_runs;
mod item;
mod journal;
mod reddit_feed;
mod rss;
mod settings;
mod sources;
mod topics;
mod types;
mod update;
mod youtube;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rusqlite::Connection;

use cli::{Cli, Commands, ConfigAction, FetchArgs, SourceAction, TopicAction, UpdateArgs};
use config::Config;
use digest::{digest_filename, render_digest_note, write_digest_note, DigestRun, SourceGroup};
use item::Item;
use types::{Sort, SourceKind, TimeFilter};

/// Print `msg` when `verbose` is true; a no-op otherwise. This is the single
/// gate for verbose-only diagnostic output (request URLs, rate-limit
/// sleeps, parsed-args/config dumps, token requests). Normal output --
/// what got written, what failed -- always prints unconditionally via plain
/// `println!`/`eprintln!` and never goes through this helper.
pub(crate) fn vprintln(verbose: bool, msg: impl AsRef<str>) {
    if verbose {
        println!("{}", msg.as_ref());
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    let config_path = Config::config_path()?;

    // `-v/--verbose` currently only lives on `fetch`'s args, so this dump is
    // gated on that subcommand specifically rather than a global flag.
    if let Commands::Fetch(args) = &cli.command {
        vprintln(
            args.verbose,
            format!("drip config path: {}", config_path.display()),
        );
        vprintln(args.verbose, format!("loaded config:\n{:#?}", config));
    }

    match &cli.command {
        Commands::Fetch(args) => handle_fetch(args, &config),
        Commands::Init => handle_init(),
        Commands::Config { action } => handle_config(action, &config),
        Commands::Source { action } => handle_source(action, &config),
        Commands::Topic { action } => handle_topic(action, &config),
        Commands::Update(args) => handle_update(args),
    }
}

/// Fetch parameters after resolving defaults against `settings`. See
/// [`resolve_fetch_params`].
#[derive(Debug, Clone)]
struct ResolvedFetchParams {
    sort: Sort,
    time: Option<TimeFilter>,
    query: Option<String>,
    limit: u32,
    tag: Vec<String>,
}

/// Resolve the effective fetch parameters for `args`, falling back to
/// `settings.default_sort`/`default_limit`/`default_tags` for whichever of
/// `sort`/`limit`/`tag` weren't given as explicit flags (drip-15n.10). `time`
/// has no settings-backed default and is passed through as-is.
///
/// Of the fields returned here, only `limit`/`tag` affect what actually gets
/// fetched/written (see [`truncate_to_limit`] and `DigestRun.tags`);
/// `sort`/`time`/`query` only label the digest note's own frontmatter/header
/// (bd issue drip-1uk.10) -- see `FetchArgs`' doc comments in `src/cli.rs`.
///
/// `folder`/`no_journal`/`dry_run`/`verbose`/`source` are orthogonal to this
/// resolution and are read directly from `args` by the caller.
fn resolve_fetch_params(args: &FetchArgs, settings: &settings::Settings) -> ResolvedFetchParams {
    ResolvedFetchParams {
        sort: args.sort.unwrap_or(settings.default_sort),
        time: args.time,
        query: args.query.clone(),
        limit: args.limit.unwrap_or(settings.default_limit),
        tag: if args.tag.is_empty() {
            settings.default_tags.clone()
        } else {
            args.tag.clone()
        },
    }
}

/// Deduplicate `items` while preserving first-occurrence order. Used to
/// guard the combined `--source`+`--topic`-resolved label list against
/// exact duplicates (e.g. `--source rust,rust`, or a source named by both
/// `--source` and a `--topic` it belongs to) before it drives a fetch loop
/// -- both to avoid a wasted duplicate network fetch and, more importantly,
/// to avoid two `SourceGroup`s resolving to the same `source_id` later in
/// `handle_fetch` (see the `source_ids`/`groups` comment there for why that
/// matters).
fn dedup_preserving_order(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

/// Resolve `topic_names` (each a `--topic` label) into the labels of their
/// member sources, via `topics::sources_for_topic`. Returns the resolved
/// labels together with a warning message for each problem encountered along
/// the way (an unknown topic name, or -- defensively -- a member source
/// with no `display_name` label). Callers are expected to `eprintln!` each
/// warning themselves (prefixed the same way the existing per-`--source`-
/// label warnings in `handle_fetch`'s fetch loop are), rather than aborting
/// the whole fetch over one bad `--topic` name.
///
/// Kept as its own pure(-ish; it reads `conn`) function, separate from
/// `handle_fetch`, so this resolution step -- and its warning text -- is
/// unit-testable without a real fetch (bd issue drip-p6v.7).
fn resolve_topic_labels(conn: &Connection, topic_names: &[String]) -> (Vec<String>, Vec<String>) {
    let mut labels = Vec::new();
    let mut warnings = Vec::new();

    for topic_name in topic_names {
        match topics::sources_for_topic(conn, topic_name) {
            Ok(members) => {
                for member in members {
                    match member.display_name {
                        Some(label) => labels.push(label),
                        // Every member of a topic was attached via
                        // `add_source_to_topic`, which requires a
                        // labeled (`find_by_label`-resolved) source, so
                        // `display_name` should always be `Some` here. A
                        // `None` would mean the data itself is
                        // inconsistent, not that the user did anything
                        // wrong -- skip it with a warning rather than
                        // panicking.
                        None => warnings.push(format!(
                            "topic '{topic_name}' has a member source (id {}) with no label; \
                             skipping it (this indicates a data-integrity issue, not a normal \
                             user error)",
                            member.id
                        )),
                    }
                }
            }
            // `sources_for_topic`'s own error text already names the topic
            // and points at `drip topic list`, mirroring the clarity bar
            // set by the existing "no saved source named ... (run `drip
            // source list` ...)" warning for an unknown `--source` label.
            Err(err) => warnings.push(err.to_string()),
        }
    }

    (labels, warnings)
}

/// Cap a single source's freshly-fetched `items` (before dedup) at
/// `--limit`/`-n` (falling back to `settings.default_limit`), keeping the
/// first `limit` entries in feed order and dropping the rest. Applied
/// per-source, not to the digest as a whole, so `--limit 5` across three
/// `--source` labels can still produce up to 15 items total.
fn truncate_to_limit(mut items: Vec<Item>, limit: u32) -> Vec<Item> {
    items.truncate(limit as usize);
    items
}

fn handle_fetch(args: &FetchArgs, config: &Config) -> Result<()> {
    vprintln(args.verbose, format!("parsed fetch args:\n{:#?}", args));

    // `posts_folder`/`daily_notes_folder`/`daily_note_format` live in the
    // `settings` table now, not on `Config` -- see `src/settings.rs`. Open
    // the connection up front so both of those can share it.
    let conn = db::open(config)?;
    let settings = settings::load(&conn)?;

    let resolved = resolve_fetch_params(args, &settings);
    vprintln(
        args.verbose,
        format!("resolved fetch params:\n{:#?}", resolved),
    );

    if args.source.is_empty() && args.topic.is_empty() && !args.all {
        eprintln!("drip fetch: no --source, --topic, or --all given, nothing to fetch");
        return Ok(());
    }

    // Resolve `--topic` names into their member sources' labels and merge
    // them with `--source`'s labels into ONE unified list (bd issue
    // drip-p6v.7) -- this is purely a resolution step in front of the
    // existing fetch loop below, not a second/parallel pipeline. Any
    // problem resolving a topic (an unknown topic name, or -- defensively
    // -- a member source with no label) is reported as a warning rather
    // than aborting the whole fetch, the same way an unknown `--source`
    // label is handled below.
    let (topic_labels, topic_warnings) = resolve_topic_labels(&conn, &args.topic);
    for warning in &topic_warnings {
        eprintln!("warning: {warning}");
    }
    let mut combined_labels: Vec<String> = args.source.clone();
    combined_labels.extend(topic_labels);

    // `--all` means "every saved (labeled) source" (bd issue drip-l4o) --
    // since topics are just named groups of already-saved sources, fetching
    // all sources inherently covers everything any topic references, so this
    // expands into the same `combined_labels` list `--source`/`--topic` feed
    // into, rather than a separate pipeline. The dedup guard right below
    // already handles overlap with `--source`/`--topic`, so a source named
    // both ways is still fetched exactly once.
    if args.all {
        let all_sources = sources::list(&conn)?;
        if all_sources.is_empty() {
            eprintln!(
                "drip fetch: --all given but no sources are saved yet (run `drip source add` first)"
            );
            return Ok(());
        }
        for row in all_sources {
            if let Some(label) = row.display_name {
                combined_labels.push(label);
            }
        }
    }

    // Deduplicate up front, preserving first-occurrence order -- an exact
    // duplicate (e.g. `--source rust,rust`, or a source named by both
    // `--source` and a `--topic` it belongs to) would otherwise trigger a
    // wasted duplicate fetch AND produce two `SourceGroup`s that resolve to
    // the same `source_id`, which crashes `fetch_runs::record`'s
    // `PRIMARY KEY(fetch_run_id, source_id)` insert further down.
    let sources_to_fetch = dedup_preserving_order(&combined_labels);

    // `groups`/`source_ids` are shared across the fetch loop below.
    // `source_ids` is keyed by `(kind, name)` rather than bare `name` so
    // sources of different kinds that happen to share the same label string
    // resolve to genuinely distinct keys and never collide -- a bare-`name`
    // key previously let one silently overwrite the other's `source_id` in
    // this map, corrupting which source's `seen_items`/`fetch_run_sources`
    // rows the other group's items got attributed to. Exact duplicates
    // within `args.source` are handled separately, by deduplicating it up
    // front via `dedup_preserving_order` before the fetch loop runs below.
    let mut groups: Vec<(SourceGroup, Vec<Item>)> = Vec::new();
    let mut source_ids: std::collections::HashMap<(SourceKind, String), i64> =
        std::collections::HashMap::new();

    if !sources_to_fetch.is_empty() {
        for label in &sources_to_fetch {
            match sources::find_by_label(&conn, label) {
                Ok(Some(source_row)) => {
                    let fetch_result = match source_row.kind {
                        SourceKind::Rss | SourceKind::Youtube | SourceKind::Reddit => {
                            rss::fetch(&source_row.identifier, args.verbose)
                        }
                    };
                    match fetch_result {
                        Ok(items) => {
                            let items = truncate_to_limit(items, resolved.limit);
                            let total = items.len();
                            let filtered =
                                dedup::filter_unseen(&conn, source_row.id, items)?;
                            let new = filtered.len();
                            let skipped = total - new;

                            if skipped > 0 {
                                println!(
                                    "{label}: fetched {total} item(s), {new} new ({skipped} already seen)"
                                );
                            } else {
                                println!("{label}: fetched {new} item(s)");
                            }

                            source_ids.insert((source_row.kind, label.clone()), source_row.id);
                            groups.push((
                                SourceGroup {
                                    kind: source_row.kind,
                                    name: label.clone(),
                                },
                                filtered,
                            ));
                        }
                        Err(err) => eprintln!("warning: {label}: {err}"),
                    }
                }
                Ok(None) => eprintln!(
                    "warning: no saved source named '{label}' (run `drip source list` to see saved sources)"
                ),
                Err(err) => eprintln!("warning: failed to look up source '{label}': {err}"),
            }
        }
    }

    if groups.is_empty() {
        eprintln!("drip fetch: no sources fetched successfully; nothing to write");
        return Ok(());
    }

    // Computed before `groups` is moved into `DigestRun` below -- only
    // borrows, so it must be taken before the move, not necessarily adjacent
    // to it. Feeds `fetch_runs::record`'s per-source breakdown (drip-
    // 15n.9.5) on every outcome below, including the zero-new-items and
    // `--dry-run` cases.
    let per_source: Vec<(i64, usize)> = groups
        .iter()
        .filter_map(|(group, items)| {
            source_ids
                .get(&(group.kind, group.name.clone()))
                .map(|&id| (id, items.len()))
        })
        .collect();

    let total_new_posts: usize = groups.iter().map(|(_, items)| items.len()).sum();
    if total_new_posts == 0 {
        fetch_runs::record(&conn, None, 0, &per_source)?;
        println!("drip fetch: no new posts found; nothing to write");
        return Ok(());
    }

    // Label the digest's filename with the `--topic` name when exactly one
    // topic was given and it resolved successfully (bd issue drip-p6v.8) --
    // `topic_warnings` being empty is what "resolved successfully" means
    // here, since `resolve_topic_labels` only ever pushes a warning for an
    // unknown topic name or a data-integrity issue with one of its members.
    // Zero topics falls back to `None` (the existing joined-source-labels
    // behavior, unchanged). More than one topic isn't covered by the bd
    // issue's acceptance criteria; join the topic names the same way
    // multiple sources already join in `source_labels()`, rather than
    // picking one arbitrarily or over-engineering a dedicated format.
    let topic_label = match args.topic.as_slice() {
        [] => None,
        [single] if topic_warnings.is_empty() => Some(single.clone()),
        [_single_with_warnings] => None,
        multiple => Some(multiple.join(", ")),
    };

    let run = DigestRun {
        sort: resolved.sort,
        time: resolved.time,
        query: resolved.query.clone(),
        tags: resolved.tag.clone(),
        items_by_source: groups,
        topic: topic_label,
        created_at: chrono::Utc::now(),
    };

    if args.dry_run {
        println!("--- dry run: digest note preview ---");
        println!("{}", render_digest_note(&run));

        if args.no_journal {
            vprintln(
                args.verbose,
                "drip fetch: --no-journal set; would skip daily journal update",
            );
        } else {
            let filename = digest_filename(&run);
            let digest_basename = filename.trim_end_matches(".md");
            let post_count: usize = run
                .items_by_source
                .iter()
                .map(|(_, items)| items.len())
                .sum();
            let bullet = journal::digest_bullet(digest_basename, &run.source_groups(), post_count);
            let daily_path = journal::daily_note_path(
                &config.vault_path,
                &settings.daily_notes_folder,
                &settings.daily_note_format,
            );

            println!("--- dry run: journal reference preview ---");
            println!("would append to daily note: {}", daily_path.display());
            println!("{bullet}");
        }
        fetch_runs::record(&conn, None, total_new_posts, &per_source)?;
        return Ok(());
    }

    if config.vault_path.as_os_str().is_empty() {
        eprintln!(
            "drip fetch: no vault configured; run `drip init` first to set your Obsidian vault path"
        );
        return Ok(());
    }

    let posts_folder = args.folder.as_deref().unwrap_or(&settings.posts_folder);
    let path = write_digest_note(&config.vault_path, posts_folder, &run)?;
    println!("drip fetch: wrote digest note to {}", path.display());

    // Record what actually got written into the digest as seen (drip-
    // 15n.9.4), so a future fetch doesn't re-surface it. Deliberately placed
    // only on this non-dry-run path -- `--dry-run` returns above and never
    // reaches here -- and deliberately independent of `--no-journal`, since
    // this is about the digest note, not the journal.
    for (group, items) in &run.items_by_source {
        if let Some(source_id) = source_ids.get(&(group.kind, group.name.clone())) {
            dedup::record_seen(&conn, *source_id, items)?;
        }
    }

    // Record this fetch run's history (drip-15n.9.5) -- a real file was
    // actually written on this path, so `digest_note_path` is `Some(&path)`
    // here (and only here; the dry-run and zero-new-posts paths above pass
    // `None`).
    fetch_runs::record(&conn, Some(&path), total_new_posts, &per_source)?;

    if args.no_journal {
        vprintln(
            args.verbose,
            "drip fetch: --no-journal set; skipping daily journal update",
        );
    } else {
        let digest_basename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let post_count: usize = run
            .items_by_source
            .iter()
            .map(|(_, items)| items.len())
            .sum();

        let daily_path = journal::ensure_daily_note(
            &config.vault_path,
            &settings.daily_notes_folder,
            &settings.daily_note_format,
        )?;
        journal::append_digest_reference(
            &daily_path,
            &digest_basename,
            &run.source_groups(),
            post_count,
        )?;
        println!("drip fetch: updated daily note at {}", daily_path.display());
    }

    Ok(())
}

/// Interactive first-run setup wizard: prompts for vault layout and default
/// fetch settings, and writes the resulting `Config` to disk.
fn handle_init() -> Result<()> {
    println!("drip init: first-run setup\n");

    let vault_path = prompt_vault_path()?;

    let posts_folder = prompt_or_default("Posts folder", "Resources/Reddit")?;
    let daily_notes_folder = prompt_or_default("Daily notes folder", "Journal/Daily notes")?;

    println!(
        "note: daily note format is a chrono strftime format (e.g. %Y-%m-%d), not \
         Obsidian's own moment.js daily-notes format."
    );
    let daily_note_format = prompt_or_default("Daily note format", "%Y-%m-%d")?;

    let default_sort_input =
        prompt_or_default("Default sort (hot/top/new/rising/controversial)", "hot")?;
    let default_sort = Sort::from_str(&default_sort_input, true).unwrap_or(Sort::Hot);

    let default_limit_input = prompt_or_default("Default limit", "10")?;
    let default_limit: u32 = default_limit_input.trim().parse().unwrap_or(10);

    let default_tags_input = prompt_or_default("Default tags (comma-separated)", "reddit")?;
    let default_tags: Vec<String> = default_tags_input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // `posts_folder`/`daily_notes_folder`/`daily_note_format`/`default_sort`/
    // `default_limit`/`default_tags` live in the `settings` table now, not
    // on `Config` -- see `src/settings.rs`. `Config` itself only holds the
    // bootstrap fields needed to open the database in the first place.
    let config = Config {
        vault_path,
        ..Config::default()
    };

    config.save()?;

    let config_path = Config::config_path()?;
    println!("\nconfig saved to {}", config_path.display());

    let mut setup_succeeded = false;
    match db::open(&config) {
        Ok(conn) => {
            let db_path = db::resolve_db_path(&config)?;
            println!("database created and migrated at {}", db_path.display());

            settings::set_raw(&conn, "posts_folder", &posts_folder)?;
            settings::set_raw(&conn, "daily_notes_folder", &daily_notes_folder)?;
            settings::set_raw(&conn, "daily_note_format", &daily_note_format)?;
            settings::set_raw(&conn, "default_sort", default_sort.as_str())?;
            settings::set_raw(&conn, "default_limit", &default_limit.to_string())?;
            settings::set_raw(
                &conn,
                "default_tags",
                &serde_json::to_string(&default_tags)
                    .context("failed to encode default tags as JSON")?,
            )?;
            setup_succeeded = true;
        }
        Err(err) => {
            eprintln!(
                "\nwarning: config was saved successfully, but setting up the database failed: {err}"
            );
            eprintln!(
                "drip fetch will try again to create/migrate the database on its own, but you \
                 may want to investigate now."
            );
        }
    }

    // The cron step conceptually belongs at the end of a *successful* setup
    // -- it needs no new state of its own, but there's no point offering to
    // schedule unattended fetches if the setup that fetch depends on didn't
    // actually finish.
    if setup_succeeded {
        println!();
        if let Err(err) = maybe_setup_cron() {
            eprintln!("\nwarning: setting up the cron entry failed: {err}");
            eprintln!(
                "the rest of `drip init` already succeeded -- see README.md's \"## Running \
                 unattended (cron / systemd timer)\" section for manual setup instructions."
            );
        }
    }

    println!(
        "you're ready -- register a source with `drip source add --kind reddit --url \
         <subreddit> --name <label>` (or --kind rss/youtube), then try `drip fetch --source \
         <label>`"
    );

    Ok(())
}

/// Read one line from stdin. `Ok(None)` means stdin hit EOF (no more input
/// to read) -- callers must treat that as "give up gracefully", not "loop
/// again", since a further read would just return EOF again instantly.
fn read_prompt(label: &str, default: Option<&str>) -> Result<Option<String>> {
    match default {
        Some(d) => print!("{label} [{d}]: "),
        None => print!("{label}: "),
    }
    std::io::stdout().flush()?;

    let mut input = String::new();
    let bytes_read = std::io::stdin().read_line(&mut input)?;
    if bytes_read == 0 {
        return Ok(None);
    }

    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(Some(default.unwrap_or("").to_string()))
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Prompt with a default shown in brackets; an empty line (or EOF) accepts
/// the default.
fn prompt_or_default(label: &str, default: &str) -> Result<String> {
    Ok(read_prompt(label, Some(default))?.unwrap_or_else(|| default.to_string()))
}

/// Prompt for the vault path, validating that it exists as a directory. If
/// it doesn't, warns and asks the user to confirm using it anyway (default:
/// no, re-prompt). Gives up and accepts whatever was last entered if stdin
/// hits EOF, so this can never loop forever.
fn prompt_vault_path() -> Result<std::path::PathBuf> {
    loop {
        let input = match read_prompt("Obsidian vault path", None)? {
            None => return Ok(std::path::PathBuf::new()),
            Some(input) => input,
        };
        let path = std::path::PathBuf::from(&input);

        if path.is_dir() {
            return Ok(path);
        }

        println!(
            "warning: '{}' does not exist as a directory.",
            path.display()
        );
        match read_prompt("Use it anyway? (y/N)", Some("n"))? {
            None => return Ok(path),
            Some(confirm) if confirm.eq_ignore_ascii_case("y") => return Ok(path),
            _ => println!("let's try again."),
        }
    }
}

/// Optional final step of `drip init`: offer to install a daily cron entry
/// that runs `drip fetch` unattended. Skips silently if declined. Any
/// failure here (parsing, `crontab` shelling out, etc.) is returned to the
/// caller, which prints a warning and points at the README's manual
/// fallback instructions -- it must never fail `drip init` as a whole.
fn maybe_setup_cron() -> Result<()> {
    match read_prompt("Set up a daily unattended fetch via cron? (y/N)", Some("n"))? {
        None => return Ok(()),
        Some(answer) if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") => {}
        Some(_) => return Ok(()),
    }

    let sources = prompt_or_default("Saved source labels (comma-separated, blank for none)", "")?;

    if sources.trim().is_empty() {
        println!(
            "warning: no source labels were given -- there's nothing to fetch, so no cron \
             entry will be installed."
        );
        return Ok(());
    }

    let fetch_args = format!("--source {}", sources.trim());

    let (hour, minute) = loop {
        match read_prompt("Time to run daily (HH:MM, 24h)", Some("08:00"))? {
            None => return Ok(()),
            Some(input) => match cron::parse_time(&input) {
                Ok(parsed) => break parsed,
                Err(err) => println!("'{input}' isn't a valid time ({err}) -- let's try again."),
            },
        }
    };

    let binary_path = match std::env::current_exe() {
        Ok(path) => path.display().to_string(),
        Err(err) => {
            eprintln!(
                "warning: couldn't resolve the running binary's own path ({err}); falling back \
                 to \"drip\" -- you may need to fix the path in your crontab by hand if it's \
                 not on cron's PATH."
            );
            "drip".to_string()
        }
    };

    let home =
        std::env::var("HOME").context("could not determine $HOME to build the cron log path")?;
    let log_path = std::path::Path::new(&home).join(".local/log/drip.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create the log directory at {} (cron's `>>` redirect would fail \
                 silently without it)",
                parent.display()
            )
        })?;
    }

    let new_block = cron::build_line(
        hour,
        minute,
        &binary_path,
        &fetch_args,
        &log_path.display().to_string(),
    );
    let existing = cron::read_crontab()?;
    let merged = cron::upsert_line(&existing, cron::MARKER, &new_block);
    cron::write_crontab(&merged)?;

    println!("\ninstalled cron entry:\n{new_block}");

    Ok(())
}

fn handle_config(action: &ConfigAction, config: &Config) -> Result<()> {
    match action {
        ConfigAction::Show => {
            println!("config.toml (bootstrap):\n{:#?}", config);

            match db::open(config) {
                Ok(conn) => match settings::load(&conn) {
                    Ok(current_settings) => {
                        println!("\nsettings (database):\n{:#?}", current_settings);
                    }
                    Err(err) => {
                        eprintln!("\nwarning: failed to load settings from database: {err}")
                    }
                },
                Err(err) => eprintln!("\nwarning: failed to open database: {err}"),
            }
        }
        ConfigAction::Edit => {
            let path = Config::config_path()?;
            if !path.exists() {
                // `config` is already `Config::default()` in this case
                // (that's what `Config::load()` returns when no file
                // exists) -- write it out so there's something to open.
                config.save()?;
            }

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("failed to launch editor '{editor}'"))?;

            if !status.success() {
                eprintln!(
                    "drip config edit: editor '{editor}' exited with a non-zero status ({status})"
                );
            }
        }
        ConfigAction::Set { key, value } => {
            let conn = db::open(config)?;
            let encoded = settings::validate_and_encode(key, value)?;
            settings::set_raw(&conn, key, &encoded)?;
            println!("saved setting '{key}' = {value}");
        }
    }
    Ok(())
}

/// Handle `drip source add/remove/list` (drip-15n.9.6): CRUD over the
/// labeled, non-Reddit sources managed via `src/sources.rs`'s labeled-CRUD
/// functions.
fn handle_source(action: &SourceAction, config: &Config) -> Result<()> {
    let conn = db::open(config)?;
    match action {
        SourceAction::Add(args) => {
            let identifier = match args.kind {
                SourceKind::Rss => args.url.clone(),
                SourceKind::Youtube => youtube::channel_feed_url(&args.url)?,
                SourceKind::Reddit => reddit_feed::subreddit_feed_url(
                    &args.url,
                    args.sort,
                    args.time,
                    args.search.as_deref(),
                )?,
            };
            sources::upsert_source(&conn, args.kind, &identifier, Some(&args.name))?;
            println!(
                "saved source '{}' (kind: {}, url: {})",
                args.name,
                args.kind.as_str(),
                identifier
            );
        }
        SourceAction::Remove { name } => {
            if sources::remove_by_label(&conn, name)? {
                println!("removed source '{name}'");
            } else {
                println!("no source named '{name}'");
            }
        }
        SourceAction::List => {
            let saved = sources::list(&conn)?;
            if saved.is_empty() {
                println!("no sources saved yet");
            } else {
                for row in &saved {
                    println!(
                        "- {} (kind: {}, url: {})",
                        row.display_name.as_deref().unwrap_or("?"),
                        row.kind.as_str(),
                        row.identifier
                    );
                }
            }
        }
    }
    Ok(())
}

/// Handle `drip topic add/add-source/remove-source/remove/list` (bd issue
/// drip-p6v.6): CRUD over topics and their source membership, via
/// `src/topics.rs`'s labeled-CRUD functions.
fn handle_topic(action: &TopicAction, config: &Config) -> Result<()> {
    let conn = db::open(config)?;
    match action {
        TopicAction::Add { name } => {
            topics::create_topic(&conn, name)?;
            println!("created topic '{name}'");
        }
        TopicAction::AddSource { topic, source } => {
            topics::add_source_to_topic(&conn, topic, source)?;
            println!("added source '{source}' to topic '{topic}'");
        }
        TopicAction::RemoveSource { topic, source } => {
            topics::remove_source_from_topic(&conn, topic, source)?;
            println!("removed source '{source}' from topic '{topic}'");
        }
        TopicAction::Remove { name } => {
            if topics::remove_topic(&conn, name)? {
                println!("removed topic '{name}'");
            } else {
                println!("no topic named '{name}'");
            }
        }
        TopicAction::List => {
            let saved = topics::list_topics(&conn)?;
            if saved.is_empty() {
                println!("no topics saved yet");
            } else {
                for topic in &saved {
                    if topic.source_labels.is_empty() {
                        println!("- {} (no sources)", topic.name);
                    } else {
                        println!("- {}: {}", topic.name, topic.source_labels.join(", "));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Handle `drip update` (bd issue drip-01g.6): check GitHub's Releases API
/// for a newer tagged release than the running binary and, if found and
/// confirmed, download and install it in place. See `src/update.rs` for the
/// underlying pure logic/HTTP/filesystem operations this orchestrates.
fn handle_update(args: &UpdateArgs) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("current version: v{current}");

    let release = update::fetch_latest_release(update::GITHUB_API_BASE, update::REPO, args.verbose)?;

    if !update::is_newer(current, &release.tag_name) {
        println!("drip is up to date (v{current}).");
        return Ok(());
    }

    println!(
        "a newer version is available: {} (current: v{current})",
        release.tag_name
    );

    if args.check {
        return Ok(());
    }

    let expected = update::expected_asset_name(&release.tag_name);
    let asset = update::find_asset(&release, &expected).ok_or_else(|| {
        anyhow::anyhow!(
            "no release asset named '{expected}' was found for {} -- this platform may not be \
             published yet",
            release.tag_name
        )
    })?;

    if !args.yes {
        match read_prompt(&format!("Install {}? (y/N)", release.tag_name), Some("n"))? {
            Some(answer) if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") => {}
            _ => {
                println!("update cancelled");
                return Ok(());
            }
        }
    }

    let tmpdir = tempfile::tempdir().context("failed to create a temp directory for the update")?;

    update::download_asset(
        &asset.browser_download_url,
        &tmpdir.path().join(&asset.name),
        args.verbose,
    )?;

    let extracted = update::extract_binary(&tmpdir.path().join(&asset.name), tmpdir.path())?;

    let current_exe =
        std::env::current_exe().context("failed to resolve the running binary's own path")?;

    update::install_binary(&extracted, &current_exe)?;

    println!(
        "updated to {} -- installed at {}",
        release.tag_name,
        current_exe.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Build a `FetchArgs` with all the flag-resolution-relevant fields set
    /// explicitly, and sensible defaults for the orthogonal ones
    /// (folder/no_journal/dry_run/verbose), so tests only need to spell out
    /// what they care about.
    fn fetch_args(source: &[&str]) -> FetchArgs {
        FetchArgs {
            sort: None,
            time: None,
            query: None,
            limit: None,
            folder: None,
            tag: Vec::new(),
            no_journal: false,
            dry_run: false,
            verbose: false,
            source: source.iter().map(|s| s.to_string()).collect(),
            topic: Vec::new(),
            all: false,
        }
    }

    /// Like [`fetch_args`], but also sets `--topic` labels -- for
    /// `handle_fetch`'s `--topic` resolution tests (bd issue drip-p6v.7).
    fn fetch_args_with_topics(source: &[&str], topic: &[&str]) -> FetchArgs {
        FetchArgs {
            topic: topic.iter().map(|s| s.to_string()).collect(),
            ..fetch_args(source)
        }
    }

    /// Like [`fetch_args`], but sets `--all` with no explicit `--source`/
    /// `--topic` -- for `handle_fetch`'s `--all` resolution tests (bd issue
    /// drip-l4o).
    fn fetch_args_all() -> FetchArgs {
        FetchArgs {
            all: true,
            ..fetch_args(&[])
        }
    }

    /// A fresh, empty DB-backed connection -- for the settings-defaults
    /// fallback tests below.
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
    fn resolve_fetch_params_falls_back_to_settings_defaults_when_no_profile_and_flags_not_given() {
        let (_dir, conn) = fresh_conn();

        settings::set_raw(&conn, "default_sort", "top").unwrap();
        settings::set_raw(&conn, "default_limit", "25").unwrap();
        settings::set_raw(
            &conn,
            "default_tags",
            &serde_json::to_string(&vec!["custom".to_string()]).unwrap(),
        )
        .unwrap();
        let settings = settings::load(&conn).unwrap();

        let args = fetch_args(&["rust"]);

        let resolved = resolve_fetch_params(&args, &settings);

        assert_eq!(resolved.sort, Sort::Top);
        assert_eq!(resolved.limit, 25);
        assert_eq!(resolved.tag, vec!["custom".to_string()]);
    }

    #[test]
    fn resolve_fetch_params_prefers_explicit_flags_over_settings_defaults() {
        let (_dir, conn) = fresh_conn();

        settings::set_raw(&conn, "default_sort", "top").unwrap();
        settings::set_raw(&conn, "default_limit", "25").unwrap();
        let settings = settings::load(&conn).unwrap();

        let mut args = fetch_args(&["rust"]);
        args.sort = Some(Sort::New);
        args.limit = Some(3);

        let resolved = resolve_fetch_params(&args, &settings);

        assert_eq!(
            resolved.sort,
            Sort::New,
            "explicit --sort must win over settings.default_sort"
        );
        assert_eq!(
            resolved.limit, 3,
            "explicit --limit must win over settings.default_limit"
        );
    }

    #[test]
    fn dedup_preserving_order_drops_exact_duplicates_keeping_first_occurrence_order() {
        let input = vec!["rust".to_string(), "rust".to_string(), "golang".to_string()];
        let deduped = dedup_preserving_order(&input);

        assert_eq!(deduped, vec!["rust".to_string(), "golang".to_string()]);
    }

    #[test]
    fn dedup_preserving_order_is_a_no_op_on_a_list_with_no_duplicates() {
        let input = vec![
            "rust".to_string(),
            "golang".to_string(),
            "python".to_string(),
        ];
        let deduped = dedup_preserving_order(&input);

        assert_eq!(deduped, input);
    }

    fn sample_item(id: &str) -> Item {
        Item {
            id: id.to_string(),
            title: format!("Item {id}"),
            url: format!("https://example.com/{id}"),
            comments_url: None,
            author: None,
            published_at: None,
            summary: None,
            score: None,
            num_comments: None,
            flair: None,
            nsfw: false,
        }
    }

    #[test]
    fn truncate_to_limit_keeps_only_the_first_n_items_in_order() {
        let items = vec![sample_item("a"), sample_item("b"), sample_item("c")];

        let truncated = truncate_to_limit(items, 2);

        assert_eq!(
            truncated,
            vec![sample_item("a"), sample_item("b")],
            "should keep the first `limit` items, in their original order"
        );
    }

    #[test]
    fn truncate_to_limit_is_a_no_op_when_fewer_items_than_the_limit() {
        let items = vec![sample_item("a"), sample_item("b")];

        let truncated = truncate_to_limit(items.clone(), 10);

        assert_eq!(truncated, items);
    }

    /// A fresh, temp-dir-backed `Config` for `handle_topic` end-to-end
    /// tests -- mirrors `fresh_conn` above, but `handle_topic` opens its own
    /// connection from the `Config`, so tests need the `Config` itself
    /// rather than an already-open `Connection`.
    fn fresh_config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = Config {
            db_path: Some(db_path),
            ..Config::default()
        };
        (dir, config)
    }

    #[test]
    fn handle_topic_add_creates_a_topic() {
        let (_dir, config) = fresh_config();

        handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config)
            .expect("adding a new topic should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "rust");
    }

    #[test]
    fn handle_topic_add_with_taken_name_errors_clearly() {
        let (_dir, config) = fresh_config();

        handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config).unwrap();
        let err = handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config)
            .expect_err("duplicate topic name should error");

        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn handle_topic_add_source_and_remove_source_round_trip() {
        let (_dir, config) = fresh_config();

        handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config).unwrap();
        {
            let conn = db::open(&config).unwrap();
            sources::upsert_source(
                &conn,
                SourceKind::Rss,
                "https://example.com/rust.xml",
                Some("rust-blog"),
            )
            .unwrap();
        }

        handle_topic(
            &TopicAction::AddSource {
                topic: "rust".to_string(),
                source: "rust-blog".to_string(),
            },
            &config,
        )
        .expect("adding a saved source to an existing topic should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert_eq!(listed[0].source_labels, vec!["rust-blog".to_string()]);
        drop(conn);

        handle_topic(
            &TopicAction::RemoveSource {
                topic: "rust".to_string(),
                source: "rust-blog".to_string(),
            },
            &config,
        )
        .expect("removing a member source should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert!(listed[0].source_labels.is_empty());
    }

    #[test]
    fn handle_topic_add_source_errors_clearly_when_topic_missing() {
        let (_dir, config) = fresh_config();
        {
            let conn = db::open(&config).unwrap();
            sources::upsert_source(
                &conn,
                SourceKind::Rss,
                "https://example.com/rust.xml",
                Some("rust-blog"),
            )
            .unwrap();
        }

        let err = handle_topic(
            &TopicAction::AddSource {
                topic: "does-not-exist".to_string(),
                source: "rust-blog".to_string(),
            },
            &config,
        )
        .expect_err("missing topic should error");

        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn handle_topic_add_source_errors_clearly_when_source_missing() {
        let (_dir, config) = fresh_config();
        handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config).unwrap();

        let err = handle_topic(
            &TopicAction::AddSource {
                topic: "rust".to_string(),
                source: "does-not-exist".to_string(),
            },
            &config,
        )
        .expect_err("missing source should error");

        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn handle_topic_remove_deletes_an_existing_topic() {
        let (_dir, config) = fresh_config();
        handle_topic(&TopicAction::Add { name: "rust".to_string() }, &config).unwrap();

        handle_topic(&TopicAction::Remove { name: "rust".to_string() }, &config)
            .expect("removing an existing topic should succeed");

        let conn = db::open(&config).unwrap();
        assert!(topics::list_topics(&conn).unwrap().is_empty());
    }

    #[test]
    fn handle_topic_remove_of_unknown_name_is_not_an_error() {
        let (_dir, config) = fresh_config();

        handle_topic(
            &TopicAction::Remove {
                name: "does-not-exist".to_string(),
            },
            &config,
        )
        .expect("removing an unknown topic should succeed (not-found is printed, not an error)");
    }

    #[test]
    fn handle_topic_list_succeeds_on_an_empty_db() {
        let (_dir, config) = fresh_config();

        handle_topic(&TopicAction::List, &config).expect("listing with no topics should succeed");
    }

    // -- `--topic` resolution/wiring tests (bd issue drip-p6v.7) --

    /// A minimal RSS 2.0 fixture with one `<item>`, labeled by `id` so
    /// different mocked sources produce distinguishable items -- mirrors
    /// `src/rss.rs`'s own `RSS_FIXTURE` test fixture.
    fn rss_fixture(id: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Feed {id}</title>
    <link>https://example.com/</link>
    <description>Feed {id}</description>
    <item>
      <title>Post from {id}</title>
      <link>https://example.com/{id}/post</link>
      <guid>https://example.com/{id}/post</guid>
      <pubDate>Mon, 06 Jul 2026 12:00:00 GMT</pubDate>
      <description>A post from {id}.</description>
    </item>
  </channel>
</rss>"#
        )
    }

    /// Register a saved RSS source labeled `label`, backed by a mocked feed
    /// served by `server` at `/{label}.xml` returning [`rss_fixture`]`(label)`.
    fn register_mocked_rss_source(conn: &Connection, server: &mut mockito::ServerGuard, label: &str) {
        let _mock = server
            .mock("GET", format!("/{label}.xml").as_str())
            .with_status(200)
            .with_header("content-type", "application/rss+xml")
            .with_body(rss_fixture(label))
            .create();

        let url = format!("{}/{label}.xml", server.url());
        sources::upsert_source(conn, SourceKind::Rss, &url, Some(label))
            .expect("upsert_source should succeed");
    }

    /// A fresh, temp-dir-backed `Config` with a real `vault_path` set
    /// (unlike `fresh_config` above, which leaves `vault_path` empty) -- for
    /// `handle_fetch` end-to-end tests below, which need `write_digest_note`
    /// to actually succeed.
    fn fresh_config_with_vault() -> (tempfile::TempDir, tempfile::TempDir, Config) {
        let db_dir = tempfile::tempdir().expect("tempdir");
        let vault_dir = tempfile::tempdir().expect("tempdir");
        let db_path = db_dir.path().join("drip.db");
        let config = Config {
            vault_path: vault_dir.path().to_path_buf(),
            db_path: Some(db_path),
        };
        (db_dir, vault_dir, config)
    }

    /// Read the single digest note written under the default
    /// `posts_folder` ("Resources/Reddit") inside `vault_dir`, as a string.
    /// Panics if there isn't exactly one file there -- every test using this
    /// helper expects exactly one fetch run to have written exactly one note.
    fn read_only_digest_note(vault_dir: &std::path::Path) -> String {
        let posts_dir = vault_dir.join("Resources/Reddit");
        let mut entries: Vec<_> = std::fs::read_dir(&posts_dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", posts_dir.display()))
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one digest note in {}",
            posts_dir.display()
        );
        std::fs::read_to_string(entries.remove(0).path()).expect("failed to read digest note")
    }

    #[test]
    fn fetch_with_topic_fetches_all_member_sources_into_one_digest() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            for label in ["a", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            topics::create_topic(&conn, "typescript").unwrap();
            for label in ["a", "b", "c"] {
                topics::add_source_to_topic(&conn, "typescript", label).unwrap();
            }
        }

        handle_fetch(&fetch_args_with_topics(&[], &["typescript"]), &config)
            .expect("fetch with --topic should succeed");

        let note = read_only_digest_note(vault_dir.path());
        for label in ["a", "b", "c"] {
            assert!(
                note.contains(&format!("Post from {label}")),
                "digest note should include an item from source '{label}':\n{note}"
            );
        }
    }

    #[test]
    fn fetch_with_single_topic_labels_digest_filename_with_topic_name() {
        // bd issue drip-p6v.8: a single `--topic` resolves to a filename
        // label of the topic name itself, not the joined member-source
        // labels ("a, b, c").
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            for label in ["a", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            topics::create_topic(&conn, "typescript").unwrap();
            for label in ["a", "b", "c"] {
                topics::add_source_to_topic(&conn, "typescript", label).unwrap();
            }
        }

        handle_fetch(&fetch_args_with_topics(&[], &["typescript"]), &config)
            .expect("fetch with --topic should succeed");

        let posts_dir = vault_dir.path().join("Resources/Reddit");
        let mut entries: Vec<_> = std::fs::read_dir(&posts_dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", posts_dir.display()))
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one digest note in {}",
            posts_dir.display()
        );
        let filename = entries
            .remove(0)
            .file_name()
            .to_str()
            .expect("filename should be valid UTF-8")
            .to_string();

        assert!(
            filename.contains("(typescript)"),
            "expected the digest filename to be labeled with the topic name, not the joined \
             member-source labels:\n{filename}"
        );
        assert!(
            !filename.contains("a, b, c"),
            "digest filename should not fall back to the joined member-source labels:\n{filename}"
        );
    }

    #[test]
    fn fetch_with_topic_is_identical_to_the_equivalent_source_list() {
        // Two separate configs/vaults, one driven by `--topic typescript`
        // (whose members are a/b/c) and one by the equivalent `--source
        // a,b,c` -- both should produce a digest note mentioning the same
        // three fetched items.
        let (_db_dir_topic, vault_dir_topic, config_topic) = fresh_config_with_vault();
        let (_db_dir_source, vault_dir_source, config_source) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        for (conn_config, use_topic) in [(&config_topic, true), (&config_source, false)] {
            let conn = db::open(conn_config).unwrap();
            for label in ["a", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            if use_topic {
                topics::create_topic(&conn, "typescript").unwrap();
                for label in ["a", "b", "c"] {
                    topics::add_source_to_topic(&conn, "typescript", label).unwrap();
                }
            }
        }

        handle_fetch(
            &fetch_args_with_topics(&[], &["typescript"]),
            &config_topic,
        )
        .expect("fetch with --topic should succeed");
        handle_fetch(&fetch_args(&["a", "b", "c"]), &config_source)
            .expect("fetch with --source should succeed");

        let note_via_topic = read_only_digest_note(vault_dir_topic.path());
        let note_via_source = read_only_digest_note(vault_dir_source.path());

        for label in ["a", "b", "c"] {
            let needle = format!("Post from {label}");
            assert!(note_via_topic.contains(&needle));
            assert!(note_via_source.contains(&needle));
        }
    }

    #[test]
    fn fetch_with_overlapping_source_and_topic_fetches_the_shared_source_once() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            for label in ["x", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            topics::create_topic(&conn, "typescript").unwrap();
            for label in ["x", "b", "c"] {
                topics::add_source_to_topic(&conn, "typescript", label).unwrap();
            }
        }

        // "x" is named by both `--source` AND the `typescript` topic it
        // belongs to -- it must be fetched exactly once, not twice.
        handle_fetch(
            &fetch_args_with_topics(&["x"], &["typescript"]),
            &config,
        )
        .expect("fetch with overlapping --source/--topic should succeed");

        let note = read_only_digest_note(vault_dir.path());
        let x_occurrences = note.matches("Post from x").count();
        assert_eq!(
            x_occurrences, 1,
            "source 'x' named by both --source and --topic should appear exactly once:\n{note}"
        );
        // Sanity: the other topic members still made it in too.
        assert!(note.contains("Post from b"));
        assert!(note.contains("Post from c"));
    }

    #[test]
    fn fetch_with_all_fetches_every_saved_source() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            for label in ["a", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            // Deliberately no topic created -- `--all` means "every saved
            // source" and must not depend on any topic membership.
        }

        handle_fetch(&fetch_args_all(), &config).expect("fetch with --all should succeed");

        let note = read_only_digest_note(vault_dir.path());
        for label in ["a", "b", "c"] {
            assert!(
                note.contains(&format!("Post from {label}")),
                "digest note should include an item from source '{label}':\n{note}"
            );
        }
    }

    #[test]
    fn fetch_with_all_on_empty_db_writes_nothing() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();

        handle_fetch(&fetch_args_all(), &config)
            .expect("fetch with --all on an empty db should still return Ok");

        let posts_dir = vault_dir.path().join("Resources/Reddit");
        let wrote_nothing = !posts_dir.exists()
            || std::fs::read_dir(&posts_dir)
                .expect("failed to read posts dir")
                .next()
                .is_none();
        assert!(
            wrote_nothing,
            "no digest note should be written when --all is given but no sources are saved"
        );
    }

    #[test]
    fn resolve_topic_labels_returns_member_labels_for_a_known_topic() {
        let (_dir, conn) = fresh_conn();

        sources::upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/a.xml",
            Some("a"),
        )
        .unwrap();
        sources::upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/b.xml",
            Some("b"),
        )
        .unwrap();
        topics::create_topic(&conn, "typescript").unwrap();
        topics::add_source_to_topic(&conn, "typescript", "a").unwrap();
        topics::add_source_to_topic(&conn, "typescript", "b").unwrap();

        let (labels, warnings) =
            resolve_topic_labels(&conn, &["typescript".to_string()]);

        assert_eq!(labels, vec!["a".to_string(), "b".to_string()]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolve_topic_labels_warns_clearly_on_an_unknown_topic_name() {
        let (_dir, conn) = fresh_conn();

        let (labels, warnings) =
            resolve_topic_labels(&conn, &["does-not-exist".to_string()]);

        assert!(labels.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("does-not-exist"),
            "warning should name the unknown topic: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("drip topic list"),
            "warning should point users at `drip topic list`, matching the clarity of the \
             existing unknown --source warning: {}",
            warnings[0]
        );
    }
}
