mod classify;
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
mod rules;
mod settings;
mod sources;
mod topics;
mod types;
mod update;
mod youtube;

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rusqlite::Connection;

use classify::Section;
use cli::{Cli, Commands, ConfigAction, FetchArgs, SourceAction, TopicAction, UpdateArgs};
use config::Config;
use digest::{digest_filename, preview_digest_note, write_digest_note, DigestRun, SourceGroup};
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
/// to avoid `fetch_one_source` running (and recording seen/fetch-history
/// rows) for the same source twice in one run.
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
                        // Every member of a topic was assigned via
                        // `drip source add`'s `--topic` or `drip source
                        // move` (`topics::move_source_to_topic`), both of
                        // which require a labeled source, so
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

/// Pre-request spacing for a reddit fetch: `base` widened by accumulated 429
/// "pressure" (the IP limit is global, so each 429 spaces out the REST of
/// the run, not just retries of the source that got limited), clamped to a
/// sane ceiling so a long run can never stall indefinitely (bd issue
/// drip-6xz, follow-up to drip-hja's fixed, un-adaptive 5s throttle).
/// `pressure` 0 -> `base` unchanged; 1 -> 2x; 2 -> 3x; etc.
fn reddit_pre_request_delay(base: Duration, pressure: u32) -> Duration {
    let widened = base.saturating_mul(1 + pressure);
    widened.min(Duration::from_secs(60))
}

/// Outcome of fetching one saved source label, as aggregated by
/// `fetch_one_source`'s caller in the two-pass coordinator below (bd issue
/// drip-6xz). Keeps the per-source fetch body (lookup, throttle, `rss::fetch`,
/// dedup, classify, bookkeeping) written exactly once even though it now
/// runs across up to two passes.
enum SourceResult {
    /// Fetched successfully (possibly zero routed items after classification
    /// and per-source `--limit` truncation).
    Fetched {
        group: SourceGroup,
        source_id: i64,
        /// This source's contribution to `DigestRun::items_by_subtopic`:
        /// one entry per `(main topic, sub-topic)` it routed at least one
        /// (post-truncation) item into (bd issue drip-98u.5/drip-ho5.6's
        /// "late fan-out" -- classification happens once per source here,
        /// producing section buckets PLUS the `routed_items` set below, both
        /// from the SAME classification pass).
        sections: Vec<(Section, Vec<Item>)>,
        /// The distinct items that landed in at least one section, after
        /// `--limit` truncation (bd issue drip-98u.4: the cap applies to the
        /// routed set, not the raw fetch, and runs LAST in the per-source
        /// pipeline -- see this function's own doc comment). This is exactly
        /// what gets `record_seen`'d and counted in `fetch_run_sources.item_count`
        /// -- one call, one row, per source, by construction (bd issue
        /// drip-98u.5: "this makes the confirmed fetch_run_sources PRIMARY
        /// KEY crash impossible").
        routed_items: Vec<Item>,
        /// Items that matched zero candidate sub-topics (bd issue
        /// drip-98u.3's "zero-match drop" outcome) -- reported, never
        /// recorded seen.
        dropped_count: usize,
        /// Items rejected by the source-level, title-only exclude pre-filter
        /// before any candidate routing ran (bd issue drip-98u.3's "source
        /// exclude" outcome) -- reported, never recorded seen.
        excluded_count: usize,
    },
    /// Still HTTP 429 after exhausting inline retries -- eligible for the
    /// final retry pass.
    RateLimited,
    /// Any other failure, or an unknown/lookup-failed label -- not retried.
    Skipped,
}

/// Fetch one saved source label and run it through the full per-source
/// pipeline (bd issue drip-98u.4/drip-98u.5, implemented by drip-ho5.6):
///
///     fetch -> filter_unseen -> classify (source exclude pre-filter,
///              then candidate routing) -> truncate_to_limit
///
/// `truncate_to_limit` runs LAST, against the classified **routed set**
/// (distinct items that landed in at least one section) -- not the raw
/// fetch. Moving it here (rather than right after `fetch`, as it used to
/// run) was measured, not a style preference: truncating first can leave
/// zero routed items at the shipped default limit, since a source's
/// noisiest items often sort first (see `main.rs`'s own module docs / bd
/// issue drip-98u.4's resolution for the measurement).
///
/// `requested_sub_topic_ids`, when `Some`, restricts classification to only
/// the caller's requested sub-topics' rules (bd issue drip-98u.3) -- set by
/// the caller when this label was resolved via one or more `--topic` names
/// and never named directly via `--source`/`--all`; `None` means no topic
/// scoping was requested, so every one of the source's own `topic_links`
/// rows is a candidate.
///
/// Shared verbatim by both passes of the two-pass coordinator in
/// `handle_fetch` so a rate-limited source retried in pass 2 goes through
/// identical logic to pass 1 (bd issue drip-6xz). `made_request`/`pressure`
/// are threaded through by the caller (not stored here) because they need to
/// persist across BOTH passes and across every source within a pass, not
/// reset per-call.
#[allow(clippy::too_many_arguments)]
fn fetch_one_source(
    conn: &Connection,
    label: &str,
    verbose: bool,
    limit: u32,
    delay: Duration,
    retry_max: u32,
    retry_base: Duration,
    pressure: u32,
    made_request: &mut bool,
    requested_sub_topic_ids: Option<&[i64]>,
) -> SourceResult {
    let source_row = match sources::find_by_label(conn, label) {
        Ok(Some(row)) => row,
        Ok(None) => {
            eprintln!(
                "warning: no saved source named '{label}' (run `drip source list` to see saved sources)"
            );
            return SourceResult::Skipped;
        }
        Err(err) => {
            eprintln!("warning: failed to look up source '{label}': {err}");
            return SourceResult::Skipped;
        }
    };

    // Only reddit feeds get throttled -- genuine RSS/YouTube feeds have no
    // known rate-limit problem, and delaying them here would needlessly
    // slow real fetches and the mockito-based e2e tests (which use
    // `SourceKind::Rss`).
    if *made_request && source_row.kind == SourceKind::Reddit {
        let sleep_for = reddit_pre_request_delay(delay, pressure);
        vprintln(
            verbose,
            format!(
                "throttling: sleeping {}s before reddit fetch to avoid 429 (pressure {pressure})",
                sleep_for.as_secs()
            ),
        );
        std::thread::sleep(sleep_for);
    }

    let outcome = match source_row.kind {
        SourceKind::Rss | SourceKind::Youtube | SourceKind::Reddit => {
            rss::fetch(&source_row.identifier, verbose, retry_max, retry_base)
        }
    };
    *made_request = true;

    match outcome {
        rss::FetchOutcome::Fetched(items) => {
            let total = items.len();
            let unseen = match dedup::filter_unseen(conn, source_row.id, items) {
                Ok(unseen) => unseen,
                Err(err) => {
                    eprintln!("warning: {label}: {err}");
                    return SourceResult::Skipped;
                }
            };
            let new_count = unseen.len();
            let skipped = total - new_count;

            let source_excludes = match sources::source_excludes(conn, source_row.id) {
                Ok(terms) => terms,
                Err(err) => {
                    eprintln!("warning: {label}: failed to load source excludes: {err}");
                    return SourceResult::Skipped;
                }
            };
            let candidates =
                match topics::candidates_for_source(conn, source_row.id, requested_sub_topic_ids) {
                    Ok(candidates) => candidates,
                    Err(err) => {
                        eprintln!("warning: {label}: failed to load classification rules: {err}");
                        return SourceResult::Skipped;
                    }
                };

            // Classify BEFORE truncating (bd issue drip-98u.4) -- cloned
            // because `unseen` is also needed afterwards, in its original
            // feed order, to compute the routed set's feed-order truncation
            // below.
            let classify_result =
                classify::classify_items(unseen.clone(), &source_excludes, &candidates);

            let routed_in_feed_order: Vec<Item> = unseen
                .into_iter()
                .filter(|it| classify_result.routed_ids.contains(&it.id))
                .collect();
            let routed_items = truncate_to_limit(routed_in_feed_order, limit);
            let truncated_ids: std::collections::HashSet<&str> =
                routed_items.iter().map(|it| it.id.as_str()).collect();

            // Build this source's section contributions from the SAME
            // classification pass, filtered down to the (possibly smaller,
            // post-`--limit`) truncated routed set -- an item dropped by
            // truncation is dropped from every section it would have
            // rendered under, not just some of them. Iterates `candidates`
            // (not `classify_result.sections` directly) purely for a
            // deterministic, DB-query-ordered section sequence.
            let mut sections: Vec<(Section, Vec<Item>)> = Vec::new();
            for candidate in &candidates {
                if let Some(items) = classify_result.sections.get(&candidate.section) {
                    let filtered: Vec<Item> = items
                        .iter()
                        .filter(|it| truncated_ids.contains(it.id.as_str()))
                        .cloned()
                        .collect();
                    if !filtered.is_empty() {
                        sections.push((candidate.section.clone(), filtered));
                    }
                }
            }

            let dropped_count = classify_result.dropped_count();
            let excluded_count = classify_result.excluded_count();
            let routed = routed_items.len();

            if skipped > 0 {
                println!(
                    "{label}: fetched {total} item(s), {new_count} new ({skipped} already seen), \
                     {routed} routed ({dropped_count} dropped, {excluded_count} excluded)"
                );
            } else {
                println!(
                    "{label}: fetched {new_count} item(s), {routed} routed ({dropped_count} \
                     dropped, {excluded_count} excluded)"
                );
            }
            for dropped in &classify_result.dropped {
                vprintln(verbose, format!("  dropped: {}", dropped.title));
            }

            SourceResult::Fetched {
                group: SourceGroup {
                    kind: source_row.kind,
                    name: label.to_string(),
                },
                source_id: source_row.id,
                sections,
                routed_items,
                dropped_count,
                excluded_count,
            }
        }
        rss::FetchOutcome::RateLimited => {
            eprintln!(
                "warning: {label}: still rate-limited (HTTP 429) after retrying; will retry \
                 again after a cooldown at the end of this run"
            );
            SourceResult::RateLimited
        }
        rss::FetchOutcome::Failed(err) => {
            eprintln!("warning: {label}: {err}");
            SourceResult::Skipped
        }
    }
}

/// Merge `section`'s `items` into `acc`, extending an already-present entry
/// for the same `Section` (from an earlier source in this run) rather than
/// pushing a second, duplicate entry -- so two different sources that both
/// route into e.g. `(AI engineering, hooks)` end up under ONE `### hooks`
/// heading, not two (bd issue drip-98u.5/drip-ho5.6).
fn merge_section_items(acc: &mut Vec<(Section, Vec<Item>)>, section: Section, items: Vec<Item>) {
    if let Some((_, existing)) = acc.iter_mut().find(|(s, _)| *s == section) {
        existing.extend(items);
    } else {
        acc.push((section, items));
    }
}

/// Resolve, for each label in a `drip fetch` invocation, the sub-topic ids
/// classification should be RESTRICTED to (bd issue drip-98u.3's "candidate
/// set = only the requested sub-topics' rules" decision).
///
/// `None` for a label means no topic scoping was requested for it -- it was
/// named directly via `--source` or expanded via `--all` -- so
/// `fetch_one_source` classifies against every one of that source's own
/// `topic_links` rows. `Some(ids)` means the label was resolved ONLY via one
/// or more `--topic` names, restricted to the union of those names'
/// requested sub-topic ids (via `topics::requested_sub_topic_ids`); naming
/// the SAME label directly via `--source`/`--all` overrides this to
/// unrestricted, since an explicit `--source <label>` reads as "give me
/// everything this source can classify into", not scoped to whatever topic
/// also happens to link it.
///
/// Errors resolving a `--topic` name are swallowed here (as "no restriction
/// contribution from this topic") rather than duplicating a warning --
/// `resolve_topic_labels` already reports any unknown `--topic` name to the
/// user.
fn resolve_label_restrictions(
    conn: &Connection,
    source_labels: &[String],
    topic_names: &[String],
    all_labels: &[String],
) -> std::collections::HashMap<String, Option<Vec<i64>>> {
    let mut restrictions: std::collections::HashMap<String, Option<Vec<i64>>> =
        std::collections::HashMap::new();

    for topic_name in topic_names {
        let Ok(ids) = topics::requested_sub_topic_ids(conn, topic_name) else {
            continue;
        };
        let Ok(members) = topics::sources_for_topic(conn, topic_name) else {
            continue;
        };
        for member in members {
            let Some(label) = member.display_name else {
                continue;
            };
            restrictions
                .entry(label)
                .and_modify(|existing| {
                    if let Some(set) = existing {
                        for id in &ids {
                            if !set.contains(id) {
                                set.push(*id);
                            }
                        }
                    }
                })
                .or_insert_with(|| Some(ids.clone()));
        }
    }

    for label in source_labels {
        restrictions.insert(label.clone(), None);
    }
    for label in all_labels {
        restrictions.insert(label.clone(), None);
    }

    restrictions
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
    // Labels added via `--all`, tracked separately from `combined_labels`
    // (bd issue drip-l4o's own resolution is unaffected -- this is purely so
    // `resolve_label_restrictions` below knows which labels came from `--all`
    // specifically, since those are unrestricted just like a direct
    // `--source`).
    let mut all_labels: Vec<String> = Vec::new();
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
                combined_labels.push(label.clone());
                all_labels.push(label);
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

    // Which sub-topics classification should be RESTRICTED to, per label (bd
    // issue drip-98u.3) -- see `resolve_label_restrictions`'s own doc comment.
    let restrictions = resolve_label_restrictions(&conn, &args.source, &args.topic, &all_labels);
    let restriction_for =
        |label: &str| -> Option<&[i64]> { restrictions.get(label).and_then(|r| r.as_deref()) };

    // Accumulators shared across the fetch loop below (bd issue drip-98u.5's
    // "late fan-out": the pipeline stays keyed per source right up to this
    // point -- `fetch_one_source` classifies once per source and returns
    // BOTH its section contributions and its routed set in one shot, so
    // every per-source consumer below (`sources_seen`, `per_source`,
    // `seen_to_record`) gets exactly one entry per source, by construction).
    let mut sources_seen: Vec<SourceGroup> = Vec::new();
    let mut items_by_subtopic: Vec<(Section, Vec<Item>)> = Vec::new();
    let mut per_source: Vec<(i64, usize)> = Vec::new();
    let mut seen_to_record: Vec<(i64, Vec<Item>)> = Vec::new();
    let mut total_new_posts: usize = 0;
    let mut total_dropped: usize = 0;
    let mut total_excluded: usize = 0;

    let mut record_fetched = |group: SourceGroup,
                              source_id: i64,
                              sections: Vec<(Section, Vec<Item>)>,
                              routed_items: Vec<Item>,
                              dropped_count: usize,
                              excluded_count: usize| {
        sources_seen.push(group);
        for (section, items) in sections {
            merge_section_items(&mut items_by_subtopic, section, items);
        }
        per_source.push((source_id, routed_items.len()));
        total_new_posts += routed_items.len();
        total_dropped += dropped_count;
        total_excluded += excluded_count;
        seen_to_record.push((source_id, routed_items));
    };

    // Tracks whether a network fetch has already happened THIS run (across
    // BOTH passes), so the reddit throttle below never delays the very
    // first fetch (nothing to space out from yet) -- only requests after
    // it.
    let mut made_request = false;
    // Accumulated 429 "pressure" for this run -- widens reddit spacing for
    // every request after a rate-limit hit, since the limit is per-IP
    // global rather than per-subreddit (bd issue drip-6xz). Capped well
    // below `reddit_pre_request_delay`'s own 60s ceiling so it can't grow
    // unboundedly across a long run.
    let mut pressure: u32 = 0;
    let delay = Duration::from_secs(settings.reddit_request_delay_secs as u64);
    let retry_max = settings.reddit_retry_max;
    let retry_base = Duration::from_secs(settings.reddit_retry_base_secs as u64);

    // Pass 1: fetch every requested source once. Anything still
    // `RateLimited` after exhausting `rss::fetch`'s own inline retries is
    // collected for a final retry pass below, rather than dropped for the
    // run outright (bd issue drip-6xz).
    let mut rate_limited: Vec<String> = Vec::new();
    for label in &sources_to_fetch {
        match fetch_one_source(
            &conn,
            label,
            args.verbose,
            resolved.limit,
            delay,
            retry_max,
            retry_base,
            pressure,
            &mut made_request,
            restriction_for(label),
        ) {
            SourceResult::Fetched {
                group,
                source_id,
                sections,
                routed_items,
                dropped_count,
                excluded_count,
            } => record_fetched(
                group,
                source_id,
                sections,
                routed_items,
                dropped_count,
                excluded_count,
            ),
            SourceResult::RateLimited => {
                pressure = (pressure + 1).min(8);
                rate_limited.push(label.clone());
            }
            SourceResult::Skipped => {}
        }
    }

    // Pass 2: a longer cooldown, then retry exactly the sources that were
    // still rate-limited after pass 1's own inline retries -- the IP-global
    // limit means the whole run needs to breathe, not just the one source
    // that got 429'd (bd issue drip-6xz).
    if !rate_limited.is_empty() {
        let cooldown = delay.saturating_mul(3).max(Duration::from_secs(30));
        println!(
            "{} source(s) rate-limited; cooling down {}s then retrying: {}",
            rate_limited.len(),
            cooldown.as_secs(),
            rate_limited.join(", ")
        );
        std::thread::sleep(cooldown);

        let retry_queue = std::mem::take(&mut rate_limited);
        for label in &retry_queue {
            match fetch_one_source(
                &conn,
                label,
                args.verbose,
                resolved.limit,
                delay,
                retry_max,
                retry_base,
                pressure,
                &mut made_request,
                restriction_for(label),
            ) {
                SourceResult::Fetched {
                    group,
                    source_id,
                    sections,
                    routed_items,
                    dropped_count,
                    excluded_count,
                } => record_fetched(
                    group,
                    source_id,
                    sections,
                    routed_items,
                    dropped_count,
                    excluded_count,
                ),
                SourceResult::RateLimited => {
                    pressure = (pressure + 1).min(8);
                    rate_limited.push(label.clone());
                }
                SourceResult::Skipped => {}
            }
        }

        for label in &rate_limited {
            eprintln!(
                "warning: {label}: still rate-limited after the final retry pass; it will be \
                 picked up on the next run (dedup avoids duplicates)"
            );
        }
    }

    if sources_seen.is_empty() {
        eprintln!("drip fetch: no sources fetched successfully; nothing to write");
        return Ok(());
    }

    // Report the run's dropped/excluded totals (bd issue drip-98u.3/
    // drip-ho5.6) -- the per-source lines above already broke this down
    // per-label; this is the whole-run summary. `-v` additionally lists each
    // dropped item's title (printed per-source, above, via `vprintln`).
    if total_dropped > 0 || total_excluded > 0 {
        println!(
            "drip fetch: {total_dropped} item(s) dropped (matched no sub-topic), \
             {total_excluded} item(s) excluded (source-level pre-filter)"
        );
    }

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
        items_by_subtopic,
        sources: sources_seen,
        created_at: chrono::Utc::now(),
    };

    let posts_folder = args.folder.as_deref().unwrap_or(&settings.posts_folder);

    if args.dry_run {
        println!("--- dry run: digest note preview ---");
        let preview = preview_digest_note(&config.vault_path, posts_folder, &run)?;
        println!("{preview}");

        if args.no_journal {
            vprintln(
                args.verbose,
                "drip fetch: --no-journal set; would skip daily journal update",
            );
        } else {
            let filename = digest_filename(&run);
            let digest_basename = filename.trim_end_matches(".md");
            let bullet =
                journal::digest_bullet(digest_basename, &run.source_groups(), total_new_posts);
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

    let path = write_digest_note(&config.vault_path, posts_folder, &run)?;
    println!("drip fetch: wrote digest note to {}", path.display());

    // Record what actually got written into the digest as seen (drip-
    // 15n.9.4), so a future fetch doesn't re-surface it. Deliberately placed
    // only on this non-dry-run path -- `--dry-run` returns above and never
    // reaches here -- and deliberately independent of `--no-journal`, since
    // this is about the digest note, not the journal. Exactly ONE call per
    // source (bd issue drip-98u.5), with that source's full routed set --
    // "recorded seen IFF written to a digest" (bd issue drip-98u.3).
    for (source_id, items) in &seen_to_record {
        dedup::record_seen(&conn, *source_id, items)?;
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

        let daily_path = journal::ensure_daily_note(
            &config.vault_path,
            &settings.daily_notes_folder,
            &settings.daily_note_format,
        )?;
        journal::append_digest_reference(
            &daily_path,
            &digest_basename,
            &run.source_groups(),
            total_new_posts,
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

/// Handle `drip source add/link/unlink/remove/list` (drip-15n.9.6, reworked
/// by bd issue drip-ho5.8 for the two-level topic tree + many-to-many
/// `topic_links`): CRUD over the labeled, non-Reddit sources managed via
/// `src/sources.rs`'s labeled-CRUD functions, plus managing a source's links
/// into sub-topics (and each link's keyword rules) via `src/topics.rs`.
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
            // `drip source add` requires an already-existing LEAF sub-topic
            // (bd issue drip-ho5.8, per drip-98u.7's "sources link to
            // sub-topics only" -- it does NOT auto-create one, and rejects
            // a bare main topic with an actionable message pointing at
            // `drip topic add --parent`).
            let topic_id = topics::require_leaf_sub_topic_id(&conn, &args.topic)?;
            let source_id =
                sources::upsert_source(&conn, args.kind, &identifier, Some(&args.name), topic_id)?;
            // Source-level excludes are declarative/replacing, same as
            // `drip source link`'s `--match`/`--exclude` (bd issue
            // drip-ho5.8) -- an omitted `--exclude` clears any existing
            // terms, matching the "always sets full state" convention.
            sources::set_source_excludes(&conn, source_id, &args.exclude)?;
            println!(
                "saved source '{}' (topic: {}, kind: {}, url: {})",
                args.name,
                args.topic,
                args.kind.as_str(),
                identifier
            );
        }
        SourceAction::Link(args) => {
            // Leaf-only enforcement happens here, ahead of the data-layer
            // write (bd issue drip-ho5.8) -- `topics::link_source_to_topic`
            // itself stays permissive so test fixtures elsewhere can still
            // build a bare (pre-hierarchy) linked topic directly.
            topics::require_leaf_sub_topic_id(&conn, &args.topic)?;
            topics::link_source_to_topic(
                &conn,
                &args.name,
                &args.topic,
                &args.match_terms,
                &args.exclude,
                args.match_body,
            )?;
            println!("linked source '{}' to topic '{}'", args.name, args.topic);
        }
        SourceAction::Unlink { name, topic } => {
            if topics::unlink_source_from_topic(&conn, name, topic)? {
                println!("unlinked source '{name}' from topic '{topic}'");
            } else {
                println!("source '{name}' was not linked to topic '{topic}'");
            }
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
                    let excludes = sources::source_excludes(&conn, row.id)?;
                    if !excludes.is_empty() {
                        println!("    source-exclude: {}", excludes.join(", "));
                    }
                    // `candidates_for_source` (bd issue drip-ho5.6) already
                    // has exactly the shape `drip source list` wants to
                    // print here -- each link's section + rules + match_body
                    // -- so it's reused rather than a second query.
                    let candidates = topics::candidates_for_source(&conn, row.id, None)?;
                    if candidates.is_empty() {
                        println!("    (not linked to any sub-topic)");
                    }
                    for candidate in &candidates {
                        let mut parts = Vec::new();
                        if !candidate.rules.include.is_empty() {
                            parts.push(format!("match={}", candidate.rules.include.join(",")));
                        }
                        if !candidate.rules.exclude.is_empty() {
                            parts.push(format!("exclude={}", candidate.rules.exclude.join(",")));
                        }
                        if candidate.match_body {
                            parts.push("match-body".to_string());
                        }
                        let detail = if parts.is_empty() {
                            "ruleless".to_string()
                        } else {
                            parts.join(" ")
                        };
                        println!(
                            "    -> {} ({}): {}",
                            candidate.section.sub_topic, candidate.section.main_topic, detail
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// One (source, sub-topic) candidate link's outcome against a `drip topic
/// test --title` synthetic item -- see [`topic_test`].
struct TopicTestLine {
    source_label: String,
    main_topic: String,
    sub_topic: String,
    outcome: classify::ItemOutcome,
    /// Which of the candidate's include terms actually fired (bd issue
    /// drip-ho5.8's "which terms fired" requirement). Empty either when the
    /// candidate is ruleless (ANY item matches, so nothing "fired") or when
    /// it didn't match at all.
    fired_terms: Vec<String>,
}

/// Pure(-ish; reads `conn`) computation backing `drip topic test --title`
/// (bd issue drip-ho5.8, per drip-98u.8's resolution point 3): classify a
/// synthetic item -- `title` only, no body, since there's no fetched item to
/// read a body from -- against EVERY saved source's sub-topic links, one
/// [`TopicTestLine`] per (source, sub-topic) candidate. Offline and
/// deterministic: no network, no fetch, reuses `classify::classify_item`
/// unchanged (called once per candidate, so each link's own outcome is
/// visible individually, rather than only the coarser per-source aggregate
/// `classify_item`'s normal caller works with).
fn topic_test(conn: &Connection, title: &str) -> Result<Vec<TopicTestLine>> {
    let item = Item {
        id: "drip-topic-test".to_string(),
        title: title.to_string(),
        url: String::new(),
        comments_url: None,
        author: None,
        published_at: None,
        summary: None,
        score: None,
        num_comments: None,
        flair: None,
        nsfw: false,
    };

    let mut lines = Vec::new();
    for source in sources::list_with_topics(conn)? {
        let label = source
            .source
            .display_name
            .clone()
            .unwrap_or_else(|| "?".to_string());
        let excludes = sources::source_excludes(conn, source.source.id)?;
        let candidates = topics::candidates_for_source(conn, source.source.id, None)?;

        for candidate in &candidates {
            let outcome =
                classify::classify_item(&item, &excludes, std::slice::from_ref(candidate));
            let fired_terms = match outcome {
                classify::ItemOutcome::Routed(_) => {
                    rules::matching_terms(&candidate.rules.include, &item.title)
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                }
                _ => Vec::new(),
            };
            lines.push(TopicTestLine {
                source_label: label.clone(),
                main_topic: candidate.section.main_topic.clone(),
                sub_topic: candidate.section.sub_topic.clone(),
                outcome,
                fired_terms,
            });
        }
    }

    Ok(lines)
}

/// Handle `drip topic add/rename/reparent/remove/list/test` (bd issue
/// drip-p6v.6, reworked by bd issue drip-ho5.8 for the two-level topic
/// tree): CRUD over topics themselves, via `src/topics.rs`'s labeled-CRUD
/// functions. Source-to-topic linking happens at `drip source add`/`drip
/// source link`/`drip source unlink` instead (`handle_source` above), since
/// a source can now link into several sub-topics at once.
fn handle_topic(action: &TopicAction, config: &Config) -> Result<()> {
    let conn = db::open(config)?;
    match action {
        TopicAction::Add { name, parent: None } => {
            topics::create_topic(&conn, name)?;
            println!("created topic '{name}'");
        }
        TopicAction::Add {
            name,
            parent: Some(parent),
        } => {
            topics::create_sub_topic(&conn, name, parent)?;
            println!("created sub-topic '{name}' under '{parent}'");
        }
        TopicAction::Rename { name, to } => {
            // Checked BEFORE the rename (bd issue drip-ho5.8, per
            // drip-98u.7's resolution) -- the heading text still reads
            // `name` at this point; renaming first would make the check a
            // no-op every time, since the OLD heading (if any) never
            // matches the NEW name.
            let settings = settings::load(&conn)?;
            let had_heading = digest::todays_note_has_heading_for(
                &config.vault_path,
                &settings.posts_folder,
                name,
            )?;

            topics::rename_topic(&conn, name, to)?;
            println!("renamed topic '{name}' to '{to}'");

            if had_heading {
                println!(
                    "warning: today's digest note already has a section for '{name}'; a fetch \
                     today will add '{to}' as a NEW section alongside it rather than updating \
                     the existing one (future-notes-only rename)"
                );
            }
        }
        TopicAction::Reparent { name, parent } => {
            let settings = settings::load(&conn)?;
            let had_heading = digest::todays_note_has_heading_for(
                &config.vault_path,
                &settings.posts_folder,
                name,
            )?;

            topics::reparent_topic(&conn, name, parent)?;
            println!("moved topic '{name}' under '{parent}'");

            if had_heading {
                println!(
                    "warning: today's digest note already has a section for '{name}'; a fetch \
                     today may render it under its previous main topic's heading alongside the \
                     new one (future-notes-only reparent)"
                );
            }
        }
        TopicAction::Remove { name } => {
            // Check existence + "any descendant" before ever touching
            // `remove_topic` (bd issue drip-ho5.4, per drip-98u.7's
            // resolution "refuse while any descendant exists"): a missing
            // topic stays the pre-existing benign "no topic named" print
            // (not an error); a main topic that still has sub-topics is
            // refused citing them; a topic (main or sub) that still has a
            // direct source link is refused citing that -- either way with
            // an actionable message rather than surfacing the raw FK
            // `RESTRICT` error `remove_topic` itself would otherwise hit.
            // The child-count check runs first: for a main topic with
            // sub-topics, "remove the sub-topics first" is the actionable
            // fix, even if (contrary to the intended leaf-only-attachment
            // model) the main also happens to carry a direct legacy link.
            let child_count = match topics::topic_child_count(&conn, name) {
                Ok(count) => count,
                Err(_) => {
                    println!("no topic named '{name}'");
                    return Ok(());
                }
            };

            if child_count > 0 {
                anyhow::bail!(
                    "topic '{name}' still has {child_count} sub-topic(s); remove them first"
                );
            }

            let link_count = topics::topic_link_count(&conn, name)?;
            if link_count > 0 {
                anyhow::bail!(
                    "topic '{name}' still has {link_count} source(s); unlink them first (e.g. \
                     `drip source unlink --name <label> --topic {name}`) before removing it"
                );
            }

            topics::remove_topic(&conn, name)?;
            println!("removed topic '{name}'");
        }
        TopicAction::List => {
            // `list_topics` already groups each main topic with its own
            // sub-topics in display order (bd issue drip-ho5.4); render that
            // as a two-level tree by indenting any row that has a parent
            // (a sub-topic) two spaces under its main topic, rather than a
            // flat list that no longer distinguishes the two.
            let saved = topics::list_topics(&conn)?;
            if saved.is_empty() {
                println!("no topics saved yet");
            } else {
                for topic in &saved {
                    let indent = if topic.parent_name.is_some() {
                        "  "
                    } else {
                        ""
                    };
                    if topic.source_labels.is_empty() {
                        println!("{indent}- {} (no sources)", topic.name);
                    } else {
                        println!(
                            "{indent}- {}: {}",
                            topic.name,
                            topic.source_labels.join(", ")
                        );
                    }
                }
            }
        }
        TopicAction::Test { title } => {
            let lines = topic_test(&conn, title)?;
            if lines.is_empty() {
                println!("no sources are linked to any sub-topic yet");
                return Ok(());
            }

            let mut routed = Vec::new();
            for line in &lines {
                let target = format!("{} -> {}", line.source_label, line.sub_topic);
                match &line.outcome {
                    classify::ItemOutcome::Excluded => {
                        println!("{target}  EXCLUDED (source-level exclude)");
                    }
                    classify::ItemOutcome::Dropped => {
                        println!("{target}  no match");
                    }
                    classify::ItemOutcome::Routed(_) => {
                        let why = if line.fired_terms.is_empty() {
                            "no include terms; matches everything".to_string()
                        } else {
                            line.fired_terms.join(", ")
                        };
                        println!("{target}  MATCH  ({why})");
                        routed.push(format!("{} > {}", line.main_topic, line.sub_topic));
                    }
                }
            }

            if routed.is_empty() {
                println!("would route to: (nothing)");
            } else {
                println!("would route to: {}", routed.join(", "));
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

    let release =
        update::fetch_latest_release(update::GITHUB_API_BASE, update::REPO, args.verbose)?;

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

    let expected = update::expected_asset_name().ok_or_else(|| {
        anyhow::anyhow!(
            "no prebuilt drip binary is published for your platform ({}/{}); install from source \
             with `cargo install --path .` or download from the Releases page",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let asset = update::find_asset(&release, expected).ok_or_else(|| {
        anyhow::anyhow!(
            "release {} has no asset named '{expected}' (expected for this platform)",
            release.tag_name
        )
    })?;

    if !args.yes {
        match read_prompt(&format!("Install {}? (y/N)", release.tag_name), Some("n"))? {
            Some(answer)
                if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") => {}
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
    use cli::SourceAddArgs;
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

    #[test]
    fn reddit_pre_request_delay_widens_with_pressure_and_clamps_to_a_ceiling() {
        let base = Duration::from_secs(10);

        assert_eq!(
            reddit_pre_request_delay(base, 0),
            base,
            "zero pressure should leave the base delay unchanged"
        );
        assert_eq!(
            reddit_pre_request_delay(base, 1),
            Duration::from_secs(20),
            "pressure 1 should double the base delay"
        );
        assert_eq!(
            reddit_pre_request_delay(base, 2),
            Duration::from_secs(30),
            "pressure 2 should triple the base delay"
        );
        assert_eq!(
            reddit_pre_request_delay(base, 100),
            Duration::from_secs(60),
            "a large pressure should clamp to the 60s ceiling, not grow unboundedly"
        );
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

        handle_topic(
            &TopicAction::Add {
                name: "rust".to_string(),
                parent: None,
            },
            &config,
        )
        .expect("adding a new topic should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "rust");
    }

    #[test]
    fn handle_topic_add_with_taken_name_errors_clearly() {
        let (_dir, config) = fresh_config();

        handle_topic(
            &TopicAction::Add {
                name: "rust".to_string(),
                parent: None,
            },
            &config,
        )
        .unwrap();
        let err = handle_topic(
            &TopicAction::Add {
                name: "rust".to_string(),
                parent: None,
            },
            &config,
        )
        .expect_err("duplicate topic name should error");

        assert!(err.to_string().contains("already exists"));
    }

    /// Build a `SourceAddArgs` for `handle_source(&SourceAction::Add(...))`
    /// end-to-end tests -- an RSS source labeled `name`, tied to `topic`, at
    /// a throwaway `https://example.com/{name}.xml` URL. Sort/time/search
    /// only matter for `--kind reddit`, so they're pinned to harmless
    /// defaults here.
    fn rss_source_add_args(name: &str, topic: &str) -> SourceAddArgs {
        SourceAddArgs {
            kind: SourceKind::Rss,
            url: format!("https://example.com/{name}.xml"),
            name: name.to_string(),
            topic: topic.to_string(),
            exclude: Vec::new(),
            sort: Sort::Hot,
            time: None,
            search: None,
        }
    }

    #[test]
    fn handle_source_add_requires_existing_topic() {
        let (_dir, config) = fresh_config();

        let err = handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "nope")),
            &config,
        )
        .expect_err("adding a source under a nonexistent topic should error");

        let message = err.to_string();
        assert!(
            message.contains("nope"),
            "error should mention the unknown topic name: {message}"
        );
        assert!(
            message.contains("drip topic add"),
            "error should point users at `drip topic add`: {message}"
        );
    }

    /// Create a main topic `main` plus one leaf sub-topic `sub` under it, via
    /// `handle_topic` (bd issue drip-ho5.8) -- the two-level shape required
    /// by leaf-only source attachment. Returns nothing; callers name `sub`
    /// again wherever a `--topic` value is needed.
    fn add_main_and_sub_topic(config: &Config, main: &str, sub: &str) {
        handle_topic(
            &TopicAction::Add {
                name: main.to_string(),
                parent: None,
            },
            config,
        )
        .expect("creating the main topic should succeed");
        handle_topic(
            &TopicAction::Add {
                name: sub.to_string(),
                parent: Some(main.to_string()),
            },
            config,
        )
        .expect("creating the sub-topic should succeed");
    }

    fn source_link_args(
        name: &str,
        topic: &str,
        match_terms: &[&str],
        exclude: &[&str],
        match_body: bool,
    ) -> SourceAction {
        SourceAction::Link(cli::SourceLinkArgs {
            name: name.to_string(),
            topic: topic.to_string(),
            match_terms: match_terms.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            match_body,
        })
    }

    #[test]
    fn handle_source_add_ties_source_to_topic() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");

        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .expect("adding a source under an existing leaf sub-topic should succeed");

        let conn = db::open(&config).unwrap();
        let found = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .expect("source should exist");

        // `SourceRow` no longer carries topic membership (bd issue
        // drip-ho5.3) -- assert via `topic_names_for_source` instead.
        let topics = sources::topic_names_for_source(&conn, found.id).unwrap();
        assert_eq!(topics, vec!["rust (general)".to_string()]);
    }

    #[test]
    fn handle_source_add_rejects_linking_to_a_main_topic() {
        let (_dir, config) = fresh_config();
        handle_topic(
            &TopicAction::Add {
                name: "rust".to_string(),
                parent: None,
            },
            &config,
        )
        .unwrap();

        let err = handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust")),
            &config,
        )
        .expect_err("linking directly to a main topic should be rejected");

        let message = err.to_string();
        assert!(message.contains("rust"));
        assert!(
            message.contains("drip topic add"),
            "error should point at creating a sub-topic: {message}"
        );
    }

    #[test]
    fn handle_source_add_writes_source_excludes() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");

        let mut args = rss_source_add_args("rust-blog", "rust (general)");
        args.exclude = vec!["megathread".to_string()];
        handle_source(&SourceAction::Add(args), &config).expect("add should succeed");

        let conn = db::open(&config).unwrap();
        let id = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            sources::source_excludes(&conn, id).unwrap(),
            vec!["megathread".to_string()]
        );
    }

    // -- bd issue drip-ho5.8: `drip source link`/`drip source unlink`
    // replace `drip source move` (removed -- it expressed
    // one-topic-per-source).

    #[test]
    fn handle_source_link_then_unlink_round_trip() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        add_main_and_sub_topic(&config, "news", "news (general)");

        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .expect("adding a source under 'rust (general)' should succeed");

        {
            let conn = db::open(&config).unwrap();
            let listed = topics::list_topics(&conn).unwrap();
            let rust_topic = listed
                .iter()
                .find(|t| t.name == "rust (general)")
                .expect("'rust (general)' topic should be in list_topics() output");
            assert_eq!(rust_topic.source_labels, vec!["rust-blog".to_string()]);
        }

        handle_source(
            &source_link_args("rust-blog", "news (general)", &[], &[], false),
            &config,
        )
        .expect("linking into a second leaf sub-topic should succeed");
        handle_source(
            &SourceAction::Unlink {
                name: "rust-blog".to_string(),
                topic: "rust (general)".to_string(),
            },
            &config,
        )
        .expect("unlinking from the original sub-topic should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        let rust_topic = listed
            .iter()
            .find(|t| t.name == "rust (general)")
            .expect("'rust (general)' topic should be in list_topics() output");
        assert!(
            rust_topic.source_labels.is_empty(),
            "source should have moved out of 'rust (general)'"
        );
        let news_topic = listed
            .iter()
            .find(|t| t.name == "news (general)")
            .expect("'news (general)' topic should be in list_topics() output");
        assert_eq!(news_topic.source_labels, vec!["rust-blog".to_string()]);
    }

    #[test]
    fn handle_source_link_replaces_match_and_exclude_lists_declaratively() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .unwrap();

        handle_source(
            &source_link_args(
                "rust-blog",
                "rust (general)",
                &["hook", "skill"],
                &["pricing"],
                false,
            ),
            &config,
        )
        .expect("first link should succeed");
        handle_source(
            &source_link_args("rust-blog", "rust (general)", &["agent"], &[], true),
            &config,
        )
        .expect("re-running link with different terms should replace, not append");

        let conn = db::open(&config).unwrap();
        let id = sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .unwrap()
            .id;
        let candidates = topics::candidates_for_source(&conn, id, None).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].rules.include, vec!["agent".to_string()]);
        assert!(candidates[0].rules.exclude.is_empty());
        assert!(candidates[0].match_body);
    }

    #[test]
    fn handle_source_link_rejects_linking_to_a_main_topic() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .unwrap();

        let err = handle_source(
            &source_link_args("rust-blog", "rust", &[], &[], false),
            &config,
        )
        .expect_err("linking directly to a main topic should be rejected");
        assert!(err.to_string().contains("rust"));
    }

    #[test]
    fn handle_source_unlink_of_a_never_linked_pair_is_a_benign_no_op() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        add_main_and_sub_topic(&config, "news", "news (general)");
        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .unwrap();

        handle_source(
            &SourceAction::Unlink {
                name: "rust-blog".to_string(),
                topic: "news (general)".to_string(),
            },
            &config,
        )
        .expect("unlinking a never-linked pair should succeed as a no-op");
    }

    #[test]
    fn handle_source_link_errors_clearly_when_topic_missing() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .unwrap();

        let err = handle_source(
            &source_link_args("rust-blog", "does-not-exist", &[], &[], false),
            &config,
        )
        .expect_err("linking into a nonexistent topic should error");

        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn handle_source_link_errors_clearly_when_source_missing() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");

        let err = handle_source(
            &source_link_args("does-not-exist", "rust (general)", &[], &[], false),
            &config,
        )
        .expect_err("moving an unknown source should error");

        assert!(err.to_string().contains("does-not-exist"));
    }

    #[test]
    fn handle_topic_remove_deletes_an_existing_topic() {
        let (_dir, config) = fresh_config();
        handle_topic(
            &TopicAction::Add {
                name: "rust".to_string(),
                parent: None,
            },
            &config,
        )
        .unwrap();

        handle_topic(
            &TopicAction::Remove {
                name: "rust".to_string(),
            },
            &config,
        )
        .expect("removing an existing (empty) topic should succeed");

        let conn = db::open(&config).unwrap();
        assert!(topics::list_topics(&conn).unwrap().is_empty());
    }

    #[test]
    fn handle_topic_remove_refuses_when_topic_has_sources() {
        // Adapted for bd issue drip-ho5.8's leaf-only attachment: a topic
        // can only directly own a source if it's a LEAF sub-topic now (a
        // bare main topic can no longer be `--topic`'d directly, per
        // `handle_source_add_rejects_linking_to_a_main_topic`), so this
        // targets the leaf, not the main -- exercising the same "still has
        // sources; unlink them first" guard end-to-end through
        // `handle_source`/`handle_topic`.
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "rust", "rust (general)");
        handle_source(
            &SourceAction::Add(rss_source_add_args("rust-blog", "rust (general)")),
            &config,
        )
        .unwrap();

        let err = handle_topic(
            &TopicAction::Remove {
                name: "rust (general)".to_string(),
            },
            &config,
        )
        .expect_err("removing a non-empty sub-topic should be refused");

        let message = err.to_string();
        assert!(
            message.contains("source"),
            "error should mention it still has sources: {message}"
        );
        assert!(
            message.contains("unlink"),
            "error should point at `drip source unlink`: {message}"
        );

        // Nothing should have been deleted.
        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert!(listed.iter().any(|t| t.name == "rust (general)"));
        assert!(sources::find_by_label(&conn, "rust-blog")
            .unwrap()
            .is_some());
    }

    // -- Cycle B (bd issue drip-ho5.4): `drip topic remove` gains a second
    // guard for the two-level tree -- a main topic refuses removal while it
    // has sub-topics, distinct from the existing "still has sources" guard
    // for a topic (main or sub) that still has a direct link (drip-98u.7).

    #[test]
    fn handle_topic_remove_refuses_a_main_topic_with_a_sub_topic_via_the_guard_not_a_raw_fk_error()
    {
        let (_dir, config) = fresh_config();
        let conn = db::open(&config).unwrap();
        let tid_claude = topics::create_topic(&conn, "Claude").unwrap();
        topics::make_sub_topic(&conn, tid_claude, "Claude (general)");
        drop(conn);

        let err = handle_topic(
            &TopicAction::Remove {
                name: "Claude".to_string(),
            },
            &config,
        )
        .expect_err("removing a main topic with a sub-topic should be refused");

        let message = err.to_string();
        assert!(
            message.contains("sub-topic"),
            "error should mention it still has sub-topics: {message}"
        );
        assert!(
            !message.to_lowercase().contains("foreign key"),
            "the guard should produce a clear message, not a raw SQLite FK error: {message}"
        );

        // Nothing should have been deleted.
        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert!(listed.iter().any(|t| t.name == "Claude"));
        assert!(listed.iter().any(|t| t.name == "Claude (general)"));
    }

    #[test]
    fn handle_topic_remove_refuses_a_sub_topic_with_a_source_link() {
        let (_dir, config) = fresh_config();
        let sub_id = {
            let conn = db::open(&config).unwrap();
            let tid_claude = topics::create_topic(&conn, "Claude").unwrap();
            let sub_id = topics::make_sub_topic(&conn, tid_claude, "Claude (general)");
            sources::upsert_source(
                &conn,
                SourceKind::Rss,
                "https://example.com/s1.xml",
                Some("s1"),
                sub_id,
            )
            .unwrap();
            sub_id
        };
        assert!(sub_id > 0);

        let err = handle_topic(
            &TopicAction::Remove {
                name: "Claude (general)".to_string(),
            },
            &config,
        )
        .expect_err("removing a sub-topic with a linked source should be refused");

        let message = err.to_string();
        assert!(
            message.contains("source"),
            "error should mention it still has sources: {message}"
        );
        assert!(
            message.contains("unlink"),
            "error should point at `drip source unlink`: {message}"
        );

        let conn = db::open(&config).unwrap();
        assert!(topics::list_topics(&conn)
            .unwrap()
            .iter()
            .any(|t| t.name == "Claude (general)"));
    }

    #[test]
    fn handle_topic_remove_of_an_empty_leaf_sub_topic_succeeds() {
        let (_dir, config) = fresh_config();
        {
            let conn = db::open(&config).unwrap();
            let tid_claude = topics::create_topic(&conn, "Claude").unwrap();
            topics::make_sub_topic(&conn, tid_claude, "Claude (general)");
        }

        handle_topic(
            &TopicAction::Remove {
                name: "Claude (general)".to_string(),
            },
            &config,
        )
        .expect("removing an empty leaf sub-topic should succeed");

        let conn = db::open(&config).unwrap();
        assert!(!topics::list_topics(&conn)
            .unwrap()
            .iter()
            .any(|t| t.name == "Claude (general)"));
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

    // -- bd issue drip-ho5.8: `topic add --parent`/`rename`/`reparent`/`test`.

    #[test]
    fn handle_topic_add_with_parent_creates_a_sub_topic() {
        let (_dir, config) = fresh_config();
        handle_topic(
            &TopicAction::Add {
                name: "Claude".to_string(),
                parent: None,
            },
            &config,
        )
        .unwrap();

        handle_topic(
            &TopicAction::Add {
                name: "cc hooks".to_string(),
                parent: Some("Claude".to_string()),
            },
            &config,
        )
        .expect("creating a sub-topic under an existing main topic should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        let sub = listed
            .iter()
            .find(|t| t.name == "cc hooks")
            .expect("sub-topic should be listed");
        assert_eq!(sub.parent_name, Some("Claude".to_string()));
    }

    #[test]
    fn handle_topic_add_rejects_a_name_with_a_comma() {
        // The decisive case (drip-98u.1): also confirms `topic add --name`
        // itself does NOT use `value_delimiter`, so this exercises
        // `validate_topic_name`'s own rejection, not clap splitting the
        // value first.
        let (_dir, config) = fresh_config();

        let err = handle_topic(
            &TopicAction::Add {
                name: "Rust, News".to_string(),
                parent: None,
            },
            &config,
        )
        .expect_err("a comma in a topic name should be rejected");
        assert!(err.to_string().contains(','));
    }

    #[test]
    fn handle_topic_add_with_parent_rejects_a_two_level_violation() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "Claude", "Claude (general)");

        let err = handle_topic(
            &TopicAction::Add {
                name: "third level".to_string(),
                parent: Some("Claude (general)".to_string()),
            },
            &config,
        )
        .expect_err("parenting under a sub-topic should be rejected");
        assert!(err.to_string().contains("Claude (general)"));
    }

    #[test]
    fn handle_topic_rename_updates_the_name() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "Claude", "loop engineering");

        handle_topic(
            &TopicAction::Rename {
                name: "loop engineering".to_string(),
                to: "agent loops".to_string(),
            },
            &config,
        )
        .expect("rename should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert!(!listed.iter().any(|t| t.name == "loop engineering"));
        assert!(listed.iter().any(|t| t.name == "agent loops"));
    }

    #[test]
    fn handle_topic_rename_warns_when_todays_note_already_has_the_old_heading() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        add_main_and_sub_topic(&config, "Claude", "loop engineering");

        let run = digest::DigestRun {
            sort: Sort::Hot,
            time: None,
            query: None,
            tags: vec![],
            items_by_subtopic: vec![],
            sources: vec![],
            created_at: chrono::Utc::now(),
        };
        // A rendered note needs at least one section to have a heading at
        // all -- write one directly under the sub-topic being renamed,
        // mirroring `digest.rs`'s own `write_digest_note` shape.
        let posts_dir = vault_dir.path().join("Resources/Reddit");
        std::fs::create_dir_all(&posts_dir).unwrap();
        let filename = digest::digest_filename(&run);
        std::fs::write(
            posts_dir.join(&filename),
            "## Claude\n\n### loop engineering\n\n- [ ] **[x](https://example.com)**\n",
        )
        .unwrap();

        // Renaming should still succeed -- the warning is advisory, not a
        // refusal (bd issue drip-ho5.8, per drip-98u.7's resolution).
        handle_topic(
            &TopicAction::Rename {
                name: "loop engineering".to_string(),
                to: "agent loops".to_string(),
            },
            &config,
        )
        .expect("rename should succeed even when today's note already has the old heading");

        let conn = db::open(&config).unwrap();
        assert!(topics::list_topics(&conn)
            .unwrap()
            .iter()
            .any(|t| t.name == "agent loops"));
    }

    #[test]
    fn handle_topic_reparent_moves_a_sub_topic_under_a_different_main() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "Claude", "cc hooks");
        handle_topic(
            &TopicAction::Add {
                name: "Rust".to_string(),
                parent: None,
            },
            &config,
        )
        .unwrap();

        handle_topic(
            &TopicAction::Reparent {
                name: "cc hooks".to_string(),
                parent: "Rust".to_string(),
            },
            &config,
        )
        .expect("reparent should succeed");

        let conn = db::open(&config).unwrap();
        let listed = topics::list_topics(&conn).unwrap();
        assert_eq!(
            listed
                .iter()
                .find(|t| t.name == "cc hooks")
                .unwrap()
                .parent_name,
            Some("Rust".to_string())
        );
    }

    #[test]
    fn topic_test_reports_match_no_match_and_exclusion() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "Claude", "cc hooks");
        handle_source(
            &SourceAction::Add(rss_source_add_args("cc-hooks-feed", "cc hooks")),
            &config,
        )
        .unwrap();
        handle_source(
            &source_link_args("cc-hooks-feed", "cc hooks", &["hook"], &[], false),
            &config,
        )
        .unwrap();

        let conn = db::open(&config).unwrap();

        let matching = topic_test(&conn, "Claude Code hooks changed how I work").unwrap();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].sub_topic, "cc hooks");
        assert_eq!(
            matching[0].outcome,
            classify::ItemOutcome::Routed(vec![classify::Section {
                main_topic: "Claude".to_string(),
                sub_topic: "cc hooks".to_string(),
            }])
        );
        assert_eq!(matching[0].fired_terms, vec!["hook".to_string()]);

        let not_matching = topic_test(&conn, "My game demo is on Steam").unwrap();
        assert_eq!(not_matching[0].outcome, classify::ItemOutcome::Dropped);
        assert!(not_matching[0].fired_terms.is_empty());
    }

    #[test]
    fn topic_test_reports_source_level_exclusion() {
        let (_dir, config) = fresh_config();
        add_main_and_sub_topic(&config, "Claude", "cc hooks");
        let mut args = rss_source_add_args("cc-hooks-feed", "cc hooks");
        args.exclude = vec!["megathread".to_string()];
        handle_source(&SourceAction::Add(args), &config).unwrap();

        let conn = db::open(&config).unwrap();
        let lines = topic_test(&conn, "Claude Model Performance Megathread").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].outcome, classify::ItemOutcome::Excluded);
    }

    #[test]
    fn handle_topic_test_succeeds_with_no_sources_linked() {
        let (_dir, config) = fresh_config();

        handle_topic(
            &TopicAction::Test {
                title: "anything".to_string(),
            },
            &config,
        )
        .expect("topic test should succeed even with nothing linked yet");
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
    fn register_mocked_rss_source(
        conn: &Connection,
        server: &mut mockito::ServerGuard,
        label: &str,
    ) {
        let _mock = server
            .mock("GET", format!("/{label}.xml").as_str())
            .with_status(200)
            .with_header("content-type", "application/rss+xml")
            .with_body(rss_fixture(label))
            .create();

        let url = format!("{}/{label}.xml", server.url());
        let topic_id = topics::get_or_create_topic(conn, "Uncategorized")
            .expect("get_or_create_topic should succeed");
        sources::upsert_source(conn, SourceKind::Rss, &url, Some(label), topic_id)
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
            let typescript_tid = topics::create_topic(&conn, "typescript").unwrap();
            for label in ["a", "b", "c"] {
                {
                    let source_id = sources::find_by_label(&conn, label).unwrap().unwrap().id;
                    conn.execute(
                        "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2) ON CONFLICT(source_id, topic_id) DO NOTHING",
                        rusqlite::params![source_id, typescript_tid],
                    )
                    .unwrap();
                }
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
    fn fetch_with_topic_produces_iso_date_daily_digest_filename() {
        // The digest filename is now just the local ISO date plus a "Daily
        // digest" suffix -- no topic/source-label parenthetical (`--topic`
        // still resolves which sources get fetched; it just no longer
        // affects the filename).
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            for label in ["a", "b", "c"] {
                register_mocked_rss_source(&conn, &mut server, label);
            }
            let typescript_tid = topics::create_topic(&conn, "typescript").unwrap();
            for label in ["a", "b", "c"] {
                {
                    let source_id = sources::find_by_label(&conn, label).unwrap().unwrap().id;
                    conn.execute(
                        "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2) ON CONFLICT(source_id, topic_id) DO NOTHING",
                        rusqlite::params![source_id, typescript_tid],
                    )
                    .unwrap();
                }
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

        let expected = format!(
            "{} - Daily digest.md",
            chrono::Local::now().format("%Y-%m-%d")
        );
        assert_eq!(
            filename, expected,
            "expected the digest filename to be the plain ISO-date + 'Daily digest' name, \
             with no topic/source-label parenthetical:\n{filename}"
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
                let typescript_tid = topics::create_topic(&conn, "typescript").unwrap();
                for label in ["a", "b", "c"] {
                    {
                        let source_id = sources::find_by_label(&conn, label).unwrap().unwrap().id;
                        conn.execute(
                        "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2) ON CONFLICT(source_id, topic_id) DO NOTHING",
                        rusqlite::params![source_id, typescript_tid],
                    )
                    .unwrap();
                    }
                }
            }
        }

        handle_fetch(&fetch_args_with_topics(&[], &["typescript"]), &config_topic)
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
            let typescript_tid = topics::create_topic(&conn, "typescript").unwrap();
            for label in ["x", "b", "c"] {
                let source_id = sources::find_by_label(&conn, label).unwrap().unwrap().id;
                // Replace the "Uncategorized" link `register_mocked_rss_source`
                // creates with one into "typescript" -- a direct `--source x`
                // fetch (unlike a `--topic`-scoped one) uses EVERY one of the
                // source's links as candidates, so leaving both links in
                // place would make "x" multi-match into both sections,
                // defeating this test's "exactly once" assertion below.
                conn.execute(
                    "DELETE FROM topic_links WHERE source_id = ?1",
                    rusqlite::params![source_id],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
                    rusqlite::params![source_id, typescript_tid],
                )
                .unwrap();
            }
        }

        // "x" is named by both `--source` AND the `typescript` topic it
        // belongs to -- it must be fetched exactly once, not twice.
        handle_fetch(&fetch_args_with_topics(&["x"], &["typescript"]), &config)
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

    // -- bd issue drip-ho5.6: late fan-out crash-prevention test. Confirms
    // the pipeline reshape makes the previously-confirmed
    // `fetch_run_sources` PRIMARY KEY(fetch_run_id, source_id) crash
    // impossible: a single source linked into TWO sub-topics (so its one
    // item multi-matches into both, per bd issue drip-98u.3's "no
    // precedence" decision) must still produce exactly ONE
    // `fetch_run_sources` row, not two.

    #[test]
    fn a_source_feeding_two_subtopics_does_not_panic_and_records_exactly_one_fetch_run_sources_row()
    {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        let source_id = {
            let conn = db::open(&config).unwrap();
            // `register_mocked_rss_source` links "a" into a ruleless link
            // under "Uncategorized" -- an empty include list matches every
            // item (`RuleSet::matches`), so a SECOND ruleless link into a
            // different sub-topic guarantees this source's one fetched item
            // multi-matches into both, without needing real keyword rules.
            register_mocked_rss_source(&conn, &mut server, "a");
            let second_topic_id = topics::create_topic(&conn, "second").unwrap();
            let source_id = sources::find_by_label(&conn, "a").unwrap().unwrap().id;
            conn.execute(
                "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
                rusqlite::params![source_id, second_topic_id],
            )
            .expect("second topic_links insert should succeed");
            source_id
        };

        handle_fetch(&fetch_args(&["a"]), &config)
            .expect("fetch of a source multi-matching into two sub-topics must not panic");

        // The multi-match rendered in both sub-topics' sections (sanity: the
        // fan-out actually happened, this isn't a vacuously-passing test).
        let note = read_only_digest_note(vault_dir.path());
        assert_eq!(
            note.matches("Post from a").count(),
            2,
            "the source's one item should render under BOTH sub-topics it's \
             linked into:\n{note}"
        );

        // The crash this test guards against: `per_source` (and therefore
        // `fetch_run_sources`, PRIMARY KEY(fetch_run_id, source_id)) must
        // carry exactly one row for this source, not one per section it fed.
        let conn = db::open(&config).unwrap();
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fetch_run_sources WHERE source_id = ?1",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            row_count, 1,
            "exactly one fetch_run_sources row per source, regardless of how \
             many sub-topics it fed"
        );

        let item_count: i64 = conn
            .query_row(
                "SELECT item_count FROM fetch_run_sources WHERE source_id = ?1",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            item_count, 1,
            "item_count should be the source's DISTINCT routed-item count \
             (1), not the sum across the two sections it multi-matched into (2)"
        );
    }

    // -- bd issue drip-98u.3: "candidates are only the REQUESTED sub-topics'
    // rules" -- `drip fetch --topic <name>` must classify against ONLY the
    // rules of the sub-topic(s) `<name>` resolves to, even though the
    // fetched source is also linked into other sub-topics with their own
    // (here, more permissive) rules.

    #[test]
    fn fetch_via_topic_restricts_classification_to_only_that_topics_rules() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            // `register_mocked_rss_source` links "a" into a RULELESS
            // "Uncategorized" link -- an empty include list matches
            // everything, so this is the "wide" link.
            register_mocked_rss_source(&conn, &mut server, "a");
            let source_id = sources::find_by_label(&conn, "a").unwrap().unwrap().id;

            // A second, "narrow" sub-topic with a real keyword rule that the
            // fixture's fetched item ("Post from a") does NOT satisfy.
            let narrow_topic_id = topics::create_topic(&conn, "narrow").unwrap();
            conn.execute(
                "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
                rusqlite::params![source_id, narrow_topic_id],
            )
            .unwrap();
            let link_id: i64 = conn
                .query_row(
                    "SELECT id FROM topic_links WHERE source_id = ?1 AND topic_id = ?2",
                    rusqlite::params![source_id, narrow_topic_id],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO link_rules (link_id, role, term) VALUES (?1, 'include', 'quantum')",
                rusqlite::params![link_id],
            )
            .unwrap();
        }

        // `--topic narrow` must restrict classification to ONLY "narrow"'s
        // rule -- the wide, ruleless "Uncategorized" link must not also be
        // consulted just because the same source happens to be linked into
        // it too. The item fails "narrow"'s "quantum" rule, so nothing
        // should be routed, and no digest note should be written.
        handle_fetch(&fetch_args_with_topics(&[], &["narrow"]), &config)
            .expect("fetch with --topic should succeed even when nothing routes");

        let posts_dir = vault_dir.path().join("Resources/Reddit");
        let wrote_nothing = !posts_dir.exists()
            || std::fs::read_dir(&posts_dir)
                .expect("failed to read posts dir")
                .next()
                .is_none();
        assert!(
            wrote_nothing,
            "restricted to 'narrow', the item should match nothing and \
             write no digest note"
        );

        // A direct `--source a` fetch (no topic scoping requested) must
        // classify against EVERY one of the source's links, including the
        // wide, ruleless one -- and route successfully.
        handle_fetch(&fetch_args(&["a"]), &config)
            .expect("direct --source fetch should succeed and route via the unrestricted link");

        let note = read_only_digest_note(vault_dir.path());
        assert!(
            note.contains("Post from a"),
            "unrestricted --source fetch should route via the wide, \
             ruleless link:\n{note}"
        );
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
        let tid = topics::get_or_create_topic(&conn, "Uncategorized").unwrap();

        sources::upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/a.xml",
            Some("a"),
            tid,
        )
        .unwrap();
        sources::upsert_source(
            &conn,
            SourceKind::Rss,
            "https://example.com/b.xml",
            Some("b"),
            tid,
        )
        .unwrap();
        let typescript_tid = topics::create_topic(&conn, "typescript").unwrap();
        for label in ["a", "b"] {
            let source_id = sources::find_by_label(&conn, label).unwrap().unwrap().id;
            conn.execute(
                "INSERT INTO topic_links (source_id, topic_id) VALUES (?1, ?2)",
                rusqlite::params![source_id, typescript_tid],
            )
            .unwrap();
        }

        let (labels, warnings) = resolve_topic_labels(&conn, &["typescript".to_string()]);

        assert_eq!(labels, vec!["a".to_string(), "b".to_string()]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolve_topic_labels_warns_clearly_on_an_unknown_topic_name() {
        let (_dir, conn) = fresh_conn();

        let (labels, warnings) = resolve_topic_labels(&conn, &["does-not-exist".to_string()]);

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

    #[test]
    fn fetch_twice_same_day_merges_new_items_without_clobbering_earlier_ones() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            register_mocked_rss_source(&conn, &mut server, "a");
        }
        handle_fetch(&fetch_args(&["a"]), &config).expect("first fetch should succeed");

        {
            let conn = db::open(&config).unwrap();
            register_mocked_rss_source(&conn, &mut server, "b");
        }
        handle_fetch(&fetch_args(&["a", "b"]), &config).expect("second fetch should succeed");

        // Still exactly one digest note for the day, now holding BOTH sources'
        // items: 'a' (from run 1, not clobbered) and 'b' (merged in on run 2).
        let note = read_only_digest_note(vault_dir.path());
        assert!(
            note.contains("Post from a"),
            "run-1 item must survive the second run:\n{note}"
        );
        assert!(
            note.contains("Post from b"),
            "run-2 item must be merged in:\n{note}"
        );
    }

    #[test]
    fn fetch_twice_same_day_with_no_new_items_leaves_the_note_untouched() {
        let (_db_dir, vault_dir, config) = fresh_config_with_vault();
        let mut server = mockito::Server::new();

        {
            let conn = db::open(&config).unwrap();
            register_mocked_rss_source(&conn, &mut server, "a");
        }
        handle_fetch(&fetch_args(&["a"]), &config).expect("first fetch should succeed");
        let after_first = read_only_digest_note(vault_dir.path());

        // Nothing new the second time (same feed, already recorded seen) ->
        // the note must be left byte-for-byte unchanged.
        handle_fetch(&fetch_args(&["a"]), &config).expect("second fetch should succeed");
        let after_second = read_only_digest_note(vault_dir.path());

        assert_eq!(
            after_first, after_second,
            "a no-new-items re-run must not change the note"
        );
        assert!(after_first.contains("Post from a"));
    }
}
