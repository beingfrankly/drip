mod cli;
mod config;
mod credentials;
mod cron;
mod db;
mod dedup;
mod digest;
mod fetch_runs;
mod item;
mod journal;
mod profiles;
mod reddit;
mod reddit_feed;
mod rss;
mod settings;
mod sources;
mod types;
mod youtube;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rusqlite::Connection;

use cli::{Cli, Commands, ConfigAction, FetchArgs, ProfileAction, SourceAction, SourceKind};
use config::Config;
use digest::{digest_filename, render_digest_note, write_digest_note, DigestRun, SourceGroup};
use item::Item;
use reddit::{Post, RedditClient};
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
        Commands::Profile { action } => handle_profile(action, &config),
        Commands::Source { action } => handle_source(action, &config),
    }
}

/// Fetch parameters after resolving `--profile` (if any) against the saved
/// profiles in `config`. See [`resolve_fetch_params`].
#[derive(Debug, Clone)]
struct ResolvedFetchParams {
    subreddit: Vec<String>,
    sort: Sort,
    time: Option<TimeFilter>,
    query: Option<String>,
    limit: u32,
    tag: Vec<String>,
}

/// Resolve the effective fetch parameters for `args`, loading them from a
/// saved profile when appropriate.
///
/// - `--profile <name>` with no `-s/--subreddit` given: look up `name` via
///   [`profiles::find`] and use that profile's subreddits/sort/time/query/
///   limit/tags. Errors clearly if no such profile exists.
/// - No `--profile`, or `--profile <name>` together with `-s/--subreddit`
///   (the latter per drip-15n.8: the profile name is only a label there --
///   for the digest filename/tags -- explicit flags win and the profile's
///   own sort/time/query/limit/tags are intentionally NOT applied): falls
///   back to `settings.default_sort`/`default_limit`/`default_tags` for
///   whichever of `sort`/`limit`/`tag` weren't given as explicit flags
///   (drip-15n.10). `time` has no settings-backed default and is passed
///   through as-is.
///
/// `min_score`/`folder`/`no_journal`/`dry_run`/`verbose` are orthogonal to
/// this resolution and are read directly from `args` by the caller.
fn resolve_fetch_params(
    args: &FetchArgs,
    conn: &Connection,
    settings: &settings::Settings,
) -> Result<ResolvedFetchParams> {
    if let Some(name) = &args.profile {
        if args.subreddit.is_empty() {
            let profile = profiles::find(conn, name)?.with_context(|| {
                format!("no profile named '{name}' (run `drip profile list` to see saved profiles)")
            })?;

            return Ok(ResolvedFetchParams {
                subreddit: profile.subreddits,
                sort: profile.sort,
                time: profile.time,
                query: profile.query,
                limit: profile.limit,
                tag: profile.tags,
            });
        }
    }

    Ok(ResolvedFetchParams {
        subreddit: args.subreddit.clone(),
        sort: args.sort.unwrap_or(settings.default_sort),
        time: args.time,
        query: args.query.clone(),
        limit: args.limit.unwrap_or(settings.default_limit),
        tag: if args.tag.is_empty() {
            settings.default_tags.clone()
        } else {
            args.tag.clone()
        },
    })
}

/// Deduplicate `items` while preserving first-occurrence order. Used to
/// guard the `-s/--subreddit` and `--source` lists against exact duplicates
/// (e.g. `-s rust,rust`) before they drive a fetch loop -- both to avoid a
/// wasted duplicate network fetch and, more importantly, to avoid two
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

/// Apply the `--min-score` and `--no-nsfw` filters, then apply per-source
/// dedup (drip-15n.9.4) against an already-known `source_id`, returning the
/// surviving items. Source-kind-agnostic -- works the same whether `items`
/// came from Reddit or RSS.
///
/// Order matters: `min_score`/`no_nsfw` are applied BEFORE dedup, and only
/// the items that survive both ever reach the returned `Vec<Item>` -- so an
/// item excluded by `--min-score` or `--no-nsfw` is never passed to
/// `dedup::filter_unseen` and therefore never gets recorded as seen by a
/// later `dedup::record_seen` call over this function's output. That's what
/// lets an item whose score later rises above the threshold (or whose NSFW
/// flag later changes) reappear in a future digest.
///
/// Items with `score: None` (every RSS item today) always pass the
/// min-score filter -- `--min-score` only excludes items that HAVE a score
/// below the threshold; it has no meaning for a source kind without a score
/// concept. Similarly, every RSS/Atom item has `nsfw: false` (see
/// `src/rss.rs`), so `--no-nsfw` only has a real effect on Reddit-origin
/// items today.
///
/// A pure function of its inputs plus the DB (no network), so the
/// min-score/no-nsfw/dedup interaction is unit-testable without mocking
/// Reddit.
fn filter_items(
    conn: &Connection,
    source_id: i64,
    mut items: Vec<Item>,
    min_score: Option<i64>,
    no_nsfw: bool,
) -> Result<Vec<Item>> {
    if let Some(min_score) = min_score {
        items.retain(|item| item.score.map_or(true, |s| s >= min_score));
    }

    if no_nsfw {
        items.retain(|item| !item.nsfw);
    }

    dedup::filter_unseen(conn, source_id, items)
}

/// Convert `posts` into `Item`s, resolve `subreddit`'s `source_id`, and
/// apply [`filter_items`] (min-score/no-nsfw then dedup), returning both the
/// resolved `source_id` and the surviving items.
///
/// Every item here comes from a Reddit post (via `Item::from`), so `score`
/// is always `Some` and `filter_items`'s min-score check is equivalent to
/// filtering on `Post::score` directly (likewise `nsfw` mirrors
/// `Post::over_18`).
fn filter_fetched_posts(
    conn: &Connection,
    subreddit: &str,
    posts: Vec<Post>,
    min_score: Option<i64>,
    no_nsfw: bool,
) -> Result<(i64, Vec<Item>)> {
    let items: Vec<Item> = posts.into_iter().map(Item::from).collect();

    let source_id = sources::upsert_reddit_source(conn, subreddit)?;
    let items = filter_items(conn, source_id, items, min_score, no_nsfw)?;

    Ok((source_id, items))
}

fn handle_fetch(args: &FetchArgs, config: &Config) -> Result<()> {
    vprintln(args.verbose, format!("parsed fetch args:\n{:#?}", args));

    // `posts_folder`/`daily_notes_folder`/`daily_note_format` live in the
    // `settings` table now, not on `Config` -- see `src/settings.rs`. And
    // `--profile` lookups are DB-backed now too -- see `src/profiles.rs`.
    // Open the connection up front so both of those can share it.
    let conn = db::open(config)?;
    let settings = settings::load(&conn)?;

    let mut resolved = resolve_fetch_params(args, &conn, &settings)?;
    vprintln(
        args.verbose,
        format!("resolved fetch params:\n{:#?}", resolved),
    );

    if resolved.subreddit.is_empty() && args.source.is_empty() {
        eprintln!("drip fetch: no --subreddit or --source given, nothing to fetch");
        return Ok(());
    }

    // Deduplicate both lists up front, preserving first-occurrence order --
    // an exact duplicate (e.g. `-s rust,rust`) would otherwise trigger a
    // wasted duplicate fetch AND produce two `SourceGroup`s that resolve to
    // the same `source_id`, which crashes `fetch_runs::record`'s
    // `PRIMARY KEY(fetch_run_id, source_id)` insert further down.
    resolved.subreddit = dedup_preserving_order(&resolved.subreddit);
    let sources_to_fetch = dedup_preserving_order(&args.source);

    // `groups`/`source_ids` are shared across both source kinds below.
    // `source_ids` is keyed by `(kind, name)` rather than bare `name` so a
    // Reddit subreddit and an RSS `--source` label that happen to share the
    // same string (e.g. both named "rust") resolve to genuinely distinct
    // keys and never collide -- a bare-`name` key previously let one
    // silently overwrite the other's `source_id` in this map, corrupting
    // which source's `seen_items`/`fetch_run_sources` rows the other
    // group's items got attributed to. Exact duplicates within a single
    // list (e.g. `-s rust,rust`) are handled separately, by deduplicating
    // `resolved.subreddit`/`args.source` up front via
    // `dedup_preserving_order` before either fetch loop runs below.
    let mut groups: Vec<(SourceGroup, Vec<Item>)> = Vec::new();
    let mut source_ids: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();

    // Reddit credentials are only needed -- and only loaded -- when there's
    // actually a subreddit to fetch. A `drip fetch --source <label>` run
    // (RSS only, no `-s`/profile subreddits) must not require Reddit
    // credentials to be configured at all.
    if !resolved.subreddit.is_empty() {
        let mut client = match credentials::load_credentials() {
            Ok((client_id, client_secret)) => RedditClient::new(client_id, client_secret),
            Err(err) => {
                eprintln!("drip fetch: {err}");
                return Ok(());
            }
        };

        let results = client.fetch_many(
            &resolved.subreddit,
            resolved.query.as_deref(),
            resolved.sort,
            resolved.time,
            resolved.limit,
            args.verbose,
        );

        for (subreddit, result) in results {
            match result {
                Ok(posts) => {
                    // "total" here is the count AFTER min_score but BEFORE
                    // dedup, so the printed summary distinguishes "filtered
                    // by score" from "filtered as already seen". Computed
                    // via a borrow so `posts` can still be moved into
                    // `filter_fetched_posts` below without duplicating the
                    // dedup lookup.
                    let total = match args.min_score {
                        Some(min_score) => posts.iter().filter(|p| p.score >= min_score).count(),
                        None => posts.len(),
                    };
                    let (source_id, filtered) = filter_fetched_posts(
                        &conn,
                        &subreddit,
                        posts,
                        args.min_score,
                        args.no_nsfw,
                    )?;
                    let new = filtered.len();
                    let skipped = total - new;

                    if skipped > 0 {
                        println!(
                            "r/{subreddit}: fetched {total} post(s), {new} new ({skipped} already seen)"
                        );
                    } else {
                        println!("r/{subreddit}: fetched {new} post(s)");
                    }

                    source_ids.insert(("reddit".to_string(), subreddit.clone()), source_id);
                    groups.push((
                        SourceGroup {
                            kind: "reddit".to_string(),
                            name: subreddit,
                        },
                        filtered,
                    ));
                }
                Err(err) => eprintln!("warning: {err}"),
            }
        }
    }

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
                            let total = items.len();
                            let filtered = filter_items(
                                &conn,
                                source_row.id,
                                items,
                                args.min_score,
                                args.no_nsfw,
                            )?;
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
        profile: args.profile.clone(),
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

/// Interactive first-run setup wizard: prompts for Reddit credentials and
/// vault layout, saves credentials to the OS keyring (falling back to a
/// clear warning if that fails), and writes the resulting `Config` to disk.
fn handle_init() -> Result<()> {
    println!("drip init: first-run setup\n");
    println!(
        "You'll need a Reddit \"script\" app's client id and secret. If you don't have one \
         yet, create one at https://www.reddit.com/prefs/apps\n"
    );

    let client_id = prompt_required("Reddit client id")?;
    let client_secret = prompt_required("Reddit client secret")?;

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

    match credentials::save_credentials(&client_id, &client_secret) {
        Ok(()) => println!("\nsaved Reddit credentials to the OS keyring."),
        Err(err) => {
            eprintln!("\nwarning: {err}");
            eprintln!(
                "your answers are still safe: your Reddit client id/secret weren't saved to \
                 the keyring, but you can set DRIP_REDDIT_CLIENT_ID and \
                 DRIP_REDDIT_CLIENT_SECRET manually instead. Saving the rest of the config now."
            );
        }
    }

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

    println!("you're ready -- try `drip fetch -s <subreddit>`");

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

/// Prompt for a value with no default, looping until non-empty input is
/// given. If stdin hits EOF before that happens, gives up and returns an
/// empty string rather than looping forever.
fn prompt_required(label: &str) -> Result<String> {
    loop {
        match read_prompt(label, None)? {
            None => return Ok(String::new()),
            Some(value) if !value.is_empty() => return Ok(value),
            Some(_) => println!("{label} is required."),
        }
    }
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

    let profile = prompt_or_default(
        "Profile name to fetch (leave blank to specify subreddits/sources directly)",
        "",
    )?;

    let fetch_args = if !profile.trim().is_empty() {
        format!("--profile {}", profile.trim())
    } else {
        let subreddits = prompt_or_default("Subreddits (comma-separated, blank for none)", "")?;
        let sources =
            prompt_or_default("Saved source labels (comma-separated, blank for none)", "")?;

        let mut parts = Vec::new();
        if !subreddits.trim().is_empty() {
            parts.push(format!("-s {}", subreddits.trim()));
        }
        if !sources.trim().is_empty() {
            parts.push(format!("--source {}", sources.trim()));
        }

        if parts.is_empty() {
            println!(
                "warning: no subreddits or source labels were given -- there's nothing to \
                 fetch, so no cron entry will be installed."
            );
            return Ok(());
        }

        parts.join(" ")
    };

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

fn handle_profile(action: &ProfileAction, config: &Config) -> Result<()> {
    // Profiles are DB-backed now (drip-15n.9.3), not part of `config.toml`
    // -- see `src/profiles.rs`. Reuse the same `db::open` pattern already
    // used by `handle_fetch`/`handle_init`/`handle_config`.
    let conn = db::open(config)?;

    match action {
        ProfileAction::Add(args) => {
            profiles::upsert(
                &conn,
                &args.name,
                &args.subreddit,
                args.sort,
                args.time,
                args.query.as_deref(),
                args.limit,
                &args.tag,
            )?;
            println!("saved profile '{}'", args.name);
        }
        ProfileAction::Remove { name } => {
            if profiles::remove(&conn, name)? {
                println!("removed profile '{name}'");
            } else {
                println!("no profile named '{name}'");
            }
        }
        ProfileAction::List => {
            let saved = profiles::list(&conn)?;
            if saved.is_empty() {
                println!("no profiles saved yet");
            } else {
                for (name, profile) in &saved {
                    println!(
                        "- {} (subreddits: {}, sort: {:?}, time: {:?}, query: {:?}, limit: {}, tags: {})",
                        name,
                        profile.subreddits.join(", "),
                        profile.sort,
                        profile.time,
                        profile.query,
                        profile.limit,
                        profile.tags.join(", ")
                    );
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `FetchArgs` with all the flag-resolution-relevant fields set
    /// explicitly, and sensible defaults for the orthogonal ones
    /// (min_score/folder/no_journal/dry_run/verbose), so tests only need to
    /// spell out what they care about.
    fn fetch_args(profile: Option<&str>, subreddit: &[&str]) -> FetchArgs {
        FetchArgs {
            subreddit: subreddit.iter().map(|s| s.to_string()).collect(),
            sort: None,
            time: None,
            query: None,
            limit: None,
            min_score: None,
            no_nsfw: false,
            folder: None,
            tag: Vec::new(),
            profile: profile.map(|s| s.to_string()),
            no_journal: false,
            dry_run: false,
            verbose: false,
            source: Vec::new(),
        }
    }

    /// A fresh, DB-backed `weekly-rust` profile fixture (rust+programming,
    /// top, week, limit 25, tag "rust"), matching the fixture the old
    /// TOML-based tests used before profiles moved into SQLite
    /// (drip-15n.9.3). `profiles::find`'s subreddits join is ordered
    /// alphabetically (see `src/profiles.rs`), so "programming" sorts before
    /// "rust" -- callers asserting on `resolved.subreddit` order should
    /// expect that.
    fn conn_with_weekly_rust_profile() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("drip.db");
        let config = Config {
            db_path: Some(db_path),
            ..Config::default()
        };
        let conn = db::open(&config).expect("db open should succeed");

        profiles::upsert(
            &conn,
            "weekly-rust",
            &["rust".to_string(), "programming".to_string()],
            Sort::Top,
            Some(TimeFilter::Week),
            None,
            25,
            &["rust".to_string()],
        )
        .expect("profile fixture upsert should succeed");

        (dir, conn)
    }

    /// A fresh, empty DB-backed connection -- for the `filter_fetched_posts`
    /// dedup/min-score interaction tests below, which don't need any
    /// profile fixture.
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

    /// A minimal `Post` fixture with just the fields relevant to
    /// `filter_fetched_posts` tests (id/title/url/score) populated
    /// meaningfully; the rest get sane placeholder defaults. Always
    /// `over_18: false` -- use [`sample_post_with_nsfw`] when a test needs
    /// to control that field.
    fn sample_post(id: &str, score: i64) -> Post {
        sample_post_with_nsfw(id, score, false)
    }

    /// Same fixture as [`sample_post`], but with `over_18` (which
    /// `Item::from(Post)` maps to `Item.nsfw` -- see `src/item.rs`) set
    /// explicitly, for `--no-nsfw` filtering tests.
    fn sample_post_with_nsfw(id: &str, score: i64, over_18: bool) -> Post {
        Post {
            id: id.to_string(),
            title: format!("Post {id}"),
            author: "someone".to_string(),
            subreddit: "rust".to_string(),
            permalink: format!("https://reddit.com/r/rust/comments/{id}/post/"),
            url: format!("https://reddit.com/r/rust/comments/{id}/post/"),
            is_self: true,
            selftext: None,
            score,
            upvote_ratio: 0.9,
            num_comments: 0,
            created_utc: 1_700_000_000.0,
            link_flair_text: None,
            over_18,
        }
    }

    #[test]
    fn resolve_fetch_params_loads_matching_profile_when_no_subreddit_given() {
        let (_dir, conn) = conn_with_weekly_rust_profile();
        let args = fetch_args(Some("weekly-rust"), &[]);
        let settings = settings::load(&conn).unwrap();

        let resolved =
            resolve_fetch_params(&args, &conn, &settings).expect("profile should resolve");

        assert_eq!(
            resolved.subreddit,
            vec!["programming".to_string(), "rust".to_string()]
        );
        assert_eq!(resolved.sort, Sort::Top);
        assert_eq!(resolved.time, Some(TimeFilter::Week));
        assert_eq!(resolved.query, None);
        assert_eq!(resolved.limit, 25);
        assert_eq!(resolved.tag, vec!["rust".to_string()]);
    }

    #[test]
    fn resolve_fetch_params_errors_clearly_for_unknown_profile() {
        let (_dir, conn) = conn_with_weekly_rust_profile();
        let args = fetch_args(Some("does-not-exist"), &[]);
        let settings = settings::load(&conn).unwrap();

        let err = resolve_fetch_params(&args, &conn, &settings)
            .expect_err("unknown profile should error");

        let message = err.to_string();
        assert!(
            message.contains("no profile named 'does-not-exist'"),
            "unexpected error message: {message}"
        );
        assert!(
            message.contains("drip profile list"),
            "error should point users at `drip profile list`: {message}"
        );
    }

    #[test]
    fn resolve_fetch_params_keeps_profile_as_label_only_when_subreddit_also_given() {
        let (_dir, conn) = conn_with_weekly_rust_profile();
        let mut args = fetch_args(Some("weekly-rust"), &["golang"]);
        args.sort = Some(Sort::New);
        args.limit = Some(5);
        let settings = settings::load(&conn).unwrap();

        let resolved = resolve_fetch_params(&args, &conn, &settings)
            .expect("profile+subreddit should keep flags, not error");

        // Explicit flags win; the profile's own values (rust/programming,
        // top, week, limit 25, tag "rust") must NOT leak through.
        assert_eq!(resolved.subreddit, vec!["golang".to_string()]);
        assert_eq!(resolved.sort, Sort::New);
        assert_eq!(resolved.time, None);
        assert_eq!(resolved.limit, 5);
        // `--tag` wasn't given, so this falls back to `settings.default_tags`
        // ("reddit" on a fresh DB) -- not the profile's own tag ("rust"),
        // and not empty either (drip-15n.10).
        assert_eq!(resolved.tag, vec!["reddit".to_string()]);
    }

    #[test]
    fn filter_fetched_posts_with_no_min_score_and_nothing_seen_returns_everything() {
        let (_dir, conn) = fresh_conn();

        let posts = vec![sample_post("abc123", 5), sample_post("def456", 50)];
        let expected: Vec<Item> = posts.iter().cloned().map(Item::from).collect();
        let (source_id, filtered) = filter_fetched_posts(&conn, "rust", posts, None, false)
            .expect("filter_fetched_posts should succeed");

        assert_eq!(filtered, expected);
        assert_eq!(
            source_id,
            sources::upsert_reddit_source(&conn, "rust").unwrap(),
            "returned source_id should be the real DB id for this subreddit"
        );
    }

    #[test]
    fn min_score_excluded_post_is_not_marked_seen_so_it_can_reappear_later() {
        let (_dir, conn) = fresh_conn();

        let post_a = sample_post("post-a", 5);
        let post_b = sample_post("post-b", 50);

        // First fetch: min_score = 10 excludes post_a.
        let (source_id, filtered) = filter_fetched_posts(
            &conn,
            "rust",
            vec![post_a.clone(), post_b.clone()],
            Some(10),
            false,
        )
        .expect("first filter_fetched_posts call should succeed");
        assert_eq!(filtered, vec![Item::from(post_b.clone())]);

        // Simulate what handle_fetch does after a successful write: record
        // only what survived filtering as seen. post_a never reaches this
        // call, so it must never be marked seen.
        dedup::record_seen(&conn, source_id, &filtered).expect("record_seen should succeed");

        // Second fetch, later, with no min_score threshold this time: post_a
        // should reappear (never marked seen), post_b should now be
        // excluded (marked seen in the first round).
        let (_source_id2, filtered2) = filter_fetched_posts(
            &conn,
            "rust",
            vec![post_a.clone(), post_b.clone()],
            None,
            false,
        )
        .expect("second filter_fetched_posts call should succeed");

        assert_eq!(
            filtered2,
            vec![Item::from(post_a)],
            "post excluded by min_score earlier must reappear; post already seen must not"
        );
    }

    #[test]
    fn no_nsfw_excludes_marked_items_when_flag_is_true() {
        let (_dir, conn) = fresh_conn();

        let nsfw_post = sample_post_with_nsfw("nsfw-post", 5, true);
        let clean_post = sample_post_with_nsfw("clean-post", 5, false);

        let (_source_id, filtered) = filter_fetched_posts(
            &conn,
            "rust",
            vec![nsfw_post, clean_post.clone()],
            None,
            true,
        )
        .expect("filter_fetched_posts should succeed");

        assert_eq!(
            filtered,
            vec![Item::from(clean_post)],
            "--no-nsfw should exclude only the item marked nsfw"
        );
    }

    #[test]
    fn nsfw_items_are_included_by_default_when_flag_is_false() {
        let (_dir, conn) = fresh_conn();

        let nsfw_post = sample_post_with_nsfw("nsfw-post", 5, true);
        let clean_post = sample_post_with_nsfw("clean-post", 5, false);
        let expected: Vec<Item> = vec![nsfw_post.clone(), clean_post.clone()]
            .into_iter()
            .map(Item::from)
            .collect();

        let (_source_id, filtered) =
            filter_fetched_posts(&conn, "rust", vec![nsfw_post, clean_post], None, false)
                .expect("filter_fetched_posts should succeed");

        assert_eq!(
            filtered, expected,
            "default (no --no-nsfw) behavior must include nsfw items, unchanged from today"
        );
    }

    #[test]
    fn no_nsfw_excluded_post_is_not_marked_seen_so_it_can_reappear_later() {
        let (_dir, conn) = fresh_conn();

        let nsfw_post = sample_post_with_nsfw("nsfw-post", 5, true);
        let clean_post = sample_post_with_nsfw("clean-post", 5, false);

        // First fetch: --no-nsfw excludes nsfw_post.
        let (source_id, filtered) = filter_fetched_posts(
            &conn,
            "rust",
            vec![nsfw_post.clone(), clean_post.clone()],
            None,
            true,
        )
        .expect("first filter_fetched_posts call should succeed");
        assert_eq!(filtered, vec![Item::from(clean_post.clone())]);

        // Simulate what handle_fetch does after a successful write: record
        // only what survived filtering as seen. nsfw_post never reaches this
        // call, so it must never be marked seen.
        dedup::record_seen(&conn, source_id, &filtered).expect("record_seen should succeed");

        // Second fetch, later, with --no-nsfw off this time: nsfw_post
        // should reappear (never marked seen), clean_post should now be
        // excluded (marked seen in the first round).
        let (_source_id2, filtered2) = filter_fetched_posts(
            &conn,
            "rust",
            vec![nsfw_post.clone(), clean_post.clone()],
            None,
            false,
        )
        .expect("second filter_fetched_posts call should succeed");

        assert_eq!(
            filtered2,
            vec![Item::from(nsfw_post)],
            "item excluded by --no-nsfw earlier must reappear; item already seen must not"
        );
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

        let args = fetch_args(None, &["rust"]);

        let resolved =
            resolve_fetch_params(&args, &conn, &settings).expect("no-profile fetch should resolve");

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

        let mut args = fetch_args(None, &["rust"]);
        args.sort = Some(Sort::New);
        args.limit = Some(3);

        let resolved =
            resolve_fetch_params(&args, &conn, &settings).expect("no-profile fetch should resolve");

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
    fn resolve_fetch_params_combined_profile_and_subreddit_still_falls_back_to_settings_not_profile(
    ) {
        let (_dir, conn) = conn_with_weekly_rust_profile();

        // The `weekly-rust` fixture profile is sort=Top, time=Week, limit=25,
        // tag=["rust"]. Set settings defaults to something else again, so
        // three distinct values are in play: the (unset) flag, the profile's
        // own value, and the settings default -- proving which one wins.
        settings::set_raw(&conn, "default_sort", "new").unwrap();
        settings::set_raw(&conn, "default_limit", "7").unwrap();
        let settings = settings::load(&conn).unwrap();

        // `--profile` given TOGETHER WITH `-s/--subreddit`: per drip-15n.8,
        // the profile is a label only here. `sort`/`limit` are left as
        // `None` (flags not explicitly given).
        let args = fetch_args(Some("weekly-rust"), &["golang"]);

        let resolved = resolve_fetch_params(&args, &conn, &settings)
            .expect("profile+subreddit should keep resolving, not error");

        assert_eq!(
            resolved.sort,
            Sort::New,
            "combined profile+subreddit case must fall back to settings.default_sort, \
             not the profile's own sort (Top)"
        );
        assert_eq!(
            resolved.limit, 7,
            "combined profile+subreddit case must fall back to settings.default_limit, \
             not the profile's own limit (25)"
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
}
