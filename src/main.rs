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
mod types;
mod update;
mod youtube;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use cli::{Cli, Commands, ConfigAction, FetchArgs, SourceAction, SourceKind, UpdateArgs};
use config::Config;
use digest::{digest_filename, render_digest_note, write_digest_note, DigestRun, SourceGroup};
use item::Item;
use types::{Sort, TimeFilter};

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
/// guard the `--source` list against exact duplicates (e.g. `--source
/// rust,rust`) before it drives a fetch loop -- both to avoid a wasted
/// duplicate network fetch and, more importantly, to avoid two
/// `SourceGroup`s resolving to the same `source_id` later in `handle_fetch`
/// (see the `source_ids`/`groups` comment there for why that matters).
fn dedup_preserving_order(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
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

    if args.source.is_empty() {
        eprintln!("drip fetch: no --source given, nothing to fetch");
        return Ok(());
    }

    // Deduplicate up front, preserving first-occurrence order -- an exact
    // duplicate (e.g. `--source rust,rust`) would otherwise trigger a wasted
    // duplicate fetch AND produce two `SourceGroup`s that resolve to the
    // same `source_id`, which crashes `fetch_runs::record`'s
    // `PRIMARY KEY(fetch_run_id, source_id)` insert further down.
    let sources_to_fetch = dedup_preserving_order(&args.source);

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
    let mut source_ids: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();

    if !sources_to_fetch.is_empty() {
        for label in &sources_to_fetch {
            match sources::find_by_label(&conn, label) {
                Ok(Some(source_row)) => {
                    let fetch_result = match source_row.kind.as_str() {
                        "rss" | "youtube" | "reddit" => {
                            rss::fetch(&source_row.identifier, args.verbose)
                        }
                        other => Err(anyhow::anyhow!(
                            "source '{label}' has unsupported kind '{other}'"
                        )),
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

                            source_ids
                                .insert((source_row.kind.clone(), label.clone()), source_row.id);
                            groups.push((
                                SourceGroup {
                                    kind: source_row.kind.clone(),
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
                .get(&(group.kind.clone(), group.name.clone()))
                .map(|&id| (id, items.len()))
        })
        .collect();

    let total_new_posts: usize = groups.iter().map(|(_, items)| items.len()).sum();
    if total_new_posts == 0 {
        fetch_runs::record(&conn, None, 0, &per_source)?;
        println!("drip fetch: no new posts found; nothing to write");
        return Ok(());
    }

    let run = DigestRun {
        sort: resolved.sort,
        time: resolved.time,
        query: resolved.query.clone(),
        tags: resolved.tag.clone(),
        items_by_source: groups,
        profile: None,
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
        if let Some(source_id) = source_ids.get(&(group.kind.clone(), group.name.clone())) {
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
            let (kind_str, identifier) = match args.kind {
                SourceKind::Rss => ("rss", args.url.clone()),
                SourceKind::Youtube => ("youtube", youtube::channel_feed_url(&args.url)?),
                SourceKind::Reddit => (
                    "reddit",
                    reddit_feed::subreddit_feed_url(
                        &args.url,
                        args.sort,
                        args.time,
                        args.search.as_deref(),
                    )?,
                ),
            };
            sources::upsert_source(&conn, kind_str, &identifier, Some(&args.name))?;
            println!(
                "saved source '{}' (kind: {kind_str}, url: {})",
                args.name, identifier
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
                        row.kind,
                        row.identifier
                    );
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
}
