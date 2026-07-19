# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->


## What drip is

`drip` is a Rust CLI that fetches hot/trending Reddit posts and RSS/Atom feed entries from sources you choose (Reddit via its own public, unauthenticated RSS/Atom feeds, not scraping and not the OAuth JSON API), normalizes both into a shared `Item` type, writes them as one markdown "digest" note per fetch run into your Obsidian vault, and appends a reference bullet to that day's daily journal note. Saved sources can also be grouped into named `drip topic`s, so a recurring set can be fetched with one `--topic <name>` instead of every member's `--source` label. See `README.md` for install steps and the full command reference (`drip init`, `drip fetch`, `drip source`, `drip topic`, `drip config`) — this file doesn't repeat that, only what a coding agent needs to orient itself in the source.

## How drip is used

The core loop, once installed (`cargo install --path .`):

1. `drip init` — interactive first-run wizard. Vault path goes to `config.toml`. Everything else (folders, date format, defaults) seeds the SQLite `settings` table. Also optionally sets up a daily cron entry for unattended fetches.
2. `drip source add --kind rss|youtube|reddit --url <url> --name <label> --topic <name>` / `drip source move --name <label> --topic <name>` / `drip source list` / `drip source remove --name <label>` — register/manage sources under a fetchable label. **Every source belongs to exactly one topic** (bd issue drip-38w.1): `source add`'s `--topic` is required and must name an already-existing topic — it errors (`no topic named '<name>'; create it first with \`drip topic add --name <name>\``) rather than auto-creating one (bd issue drip-38w.2); `source move` is the only way to reassign an already-saved source to a different topic; `source list` prints each source's topic (`- <label> (topic: <topic>, kind: <kind>, url: <url>)`). YouTube channels are fetched via their own Atom feed (`src/youtube.rs` resolves a channel id/URL to that feed URL); Reddit subreddits are fetched via Reddit's own public RSS/Atom feed (`src/reddit_feed.rs` builds that feed's URL, optionally with a sort/time window/search term baked in at registration time) — neither needs any API key, app registration, or OAuth. Fetching for both is delegated entirely to the same RSS client.
3. `drip topic add --name <name>` / `remove --name <name>` / `list` — manage topics, named groups of already-saved sources; `drip topic` no longer manages source membership itself (the previous pair of topic-scoped attach/detach subcommands is gone, bd issue drip-38w.2 — assignment now happens at `drip source add`/`drip source move` time instead, since a source belongs to exactly one topic). `drip topic remove` refuses while the topic still owns any sources (`topic '<name>' still has N source(s); move them to another topic first...`); an empty topic can always be removed, and an unknown name is still a benign no-op print. `drip fetch --topic <name>` resolves a topic into its member sources' labels so a recurring set doesn't need every `--source` label spelled out each time (`src/topics.rs`, bd issue drip-p6v).
4. `drip fetch --source <label>` — fetches one or more saved sources (comma-separated or repeated flag) and/or one or more saved topics via `--topic <name>` (same repeatable/comma-separated form; merged with `--source`, a source named by both fetched once) into one combined digest note, appends the journal reference. Every source now having exactly one topic (bd issue drip-38w.1) means the resulting digest note is always topic-grouped: an H2 `## <topic>` per distinct topic referenced, an H3 per source under it, each item a checkbox task line — see `src/digest.rs`'s row below (bd issue drip-38w.3). `--dry-run` previews both writes without touching disk. `--sort`/`--time`/`-q`/`--query` only affect the digest note's own frontmatter/header labeling — they do not filter or search what gets fetched; that's controlled per-source at `drip source add --kind reddit` time instead (clarified in their own `--help` text now, bd issue drip-1uk.10). `--tag` adds real Obsidian tags. `-n`/`--limit` does have a real effect: it caps how many items are taken from each source's fetched feed, per source, before dedup (`truncate_to_limit` in `src/main.rs`, bd issue drip-1uk.9). `--all` fetches every saved source (via `sources::list`) into one digest, merged/deduped with any `--source`/`--topic` also given — a stable command for unattended cron runs that doesn't need label enumeration (bd issue drip-l4o).
5. `drip config show/edit/set` — inspect `config.toml` + settings, edit `config.toml` in `$EDITOR`, or set one SQLite-backed setting key (`posts_folder`, `daily_notes_folder`, `daily_note_format`, `default_sort`, `default_limit`, `default_tags`, and the reddit-fetch pacing knobs `reddit_request_delay_secs`/`reddit_retry_max`/`reddit_retry_base_secs`, bd issue drip-6xz).
6. `drip update [--check] [-y]` — checks GitHub Releases for a tag newer than `env!("CARGO_PKG_VERSION")` and, if found and confirmed, downloads + installs it over the running binary (`src/update.rs`, bd issue drip-01g.6). `--check` stops after reporting; `-y` skips the confirmation prompt. Works on every target `.github/workflows/release.yml` (cargo-dist) builds: Linux x86_64, macOS x86_64 + aarch64, and Windows x86_64 — `update::asset_name_for` maps `(os, arch)` to the matching cargo-dist asset (`.tar.xz` on unix, `.zip` on Windows; note cargo-dist asset names carry no version), `extract_binary` unpacks via `tar -xf` (unix) / PowerShell `Expand-Archive` (Windows), and `install_binary` uses an atomic rename over the running exe on unix and a rename-running-exe-aside dance on Windows (bd issue drip-01g.7). Any other platform gets a clear "no prebuilt binary" error pointing at `cargo install`.

Full flag reference and worked examples: `README.md`.

## Architecture Overview

Two storage layers, split by a bootstrap chicken-and-egg constraint (the DB's own location has to be resolvable before the DB can be opened):

- `config.toml` (`src/config.rs`) — bootstrap-only fields: `vault_path`, optional `db_path` override.
- SQLite (`src/db.rs`, schema in `migrations/0001_init.sql` + `migrations/0002_profiles.sql` + `migrations/0003_source_labels.sql` + `migrations/0004_topics.sql` + `migrations/0005_source_topic.sql`) — sources, settings, fetch history, and a dedup ledger. (`migrations/0002_profiles.sql`'s `profiles`/`profile_sources`/`profile_tags` tables are inert leftover schema from the since-removed `drip profile` feature, bd issue drip-1uk.2 — migrations are additive-only and never dropped after shipping, so they're still there but nothing references them.) `migrations/0004_topics.sql` adds `topics`/`topic_sources` for named, cascade-deleting source groups, fetchable via `drip fetch --topic` (bd issue drip-p6v) — a deliberately leaner rehydration of the shape `0002_profiles.sql` used to have, with no fetch-param presets of its own. `migrations/0005_source_topic.sql` (bd issue drip-38w.1, epic drip-38w "Topic-structured digests") adds `sources.topic_id` (`FK -> topics(id) ON DELETE RESTRICT`) and switches to a **one-topic-per-source** model: every source belongs to exactly one topic from here on, tracked directly on the `sources` row. `0004`'s many-to-many `topic_sources` join is now itself inert alongside the `0002` profiles tables — nothing reads or writes it after `0005`'s one-time backfill (which also seeds a catch-all "Uncategorized" topic for any pre-existing topicless source) — kept only because migrations are additive-only.

A normalized `Item` type (`src/item.rs`) is what flows through digest rendering and dedup — every source kind (Reddit-via-RSS, genuine RSS/Atom, YouTube) produces `Item` directly via `rss::fetch`, so `dedup.rs`/`digest.rs`/`journal.rs` never special-case one source kind vs. another beyond a `SourceGroup.kind` string used purely for cosmetic rendering choices (e.g. `## r/{name}` vs `## {name}`).

Module map — read each file's own header doc-comment for the *why*, not just the *what*, before changing it:

| Module | Responsibility |
|---|---|
| `src/main.rs` | CLI dispatch (`handle_fetch`/`handle_init`/`handle_config`/`handle_source`/`handle_topic`); `resolve_fetch_params` resolves `--sort`/`--limit`/`--tag` against saved `settings` defaults; `resolve_topic_labels` resolves `--topic` names into member source labels for `handle_fetch` to merge with `--source`; `handle_fetch`'s fetch coordinator (`fetch_one_source` + `reddit_pre_request_delay`, bd issue drip-6xz) paces reddit requests (configurable delay, adaptive 429 "pressure", final retry pass over still-rate-limited sources) |
| `src/cli.rs` | clap subcommand/flag definitions, including `Commands::Source`/`SourceAction`, `Commands::Topic`/`TopicAction`, and `FetchArgs.source`/`FetchArgs.topic` |
| `src/types.rs` | `Sort`/`TimeFilter` enums shared by CLI, DB storage, and `src/reddit_feed.rs`'s feed URL construction |
| `src/item.rs` | The normalized `Item` type shared across source kinds |
| `src/rss.rs` | RSS/Atom fetch client (`feed-rs`-based), produces `Item` directly — used for genuine RSS/Atom feeds, and for `--kind reddit`/`youtube` sources once their URL is resolved. Returns a typed `FetchOutcome` (`Fetched`/`RateLimited`/`Failed`) so `handle_fetch` can distinguish a 429-exhaustion from a real error; inline 429 retry (`retry_delay`, honors `Retry-After` else exponential, capped) with `max_retries`/`base` passed in from settings (bd issue drip-hja/drip-6xz) |
| `src/youtube.rs` | Pure, no-network resolution of a channel id/URL into its Atom feed URL; fetching itself is delegated to `src/rss.rs` |
| `src/reddit_feed.rs` | Pure, no-network construction of a subreddit's unauthenticated public RSS/Atom feed URL (hot/top/new/search) — fetching itself is delegated to `src/rss.rs` |
| `src/digest.rs` | Renders + writes one digest markdown note per fetch run; body groups by topic (H2, first-seen order) then by `SourceGroup` (H3) within it, with each item rendered as a flat Obsidian checkbox task line rather than the old numbered-list-with-metadata format (bd issue drip-38w.3) |
| `src/journal.rs` | Finds/creates today's daily note, appends a kind-aware digest reference bullet under `## Reddit` |
| `src/db.rs` | Opens the SQLite connection, enables foreign keys, runs pending migrations |
| `src/update.rs` | `drip update`'s support code -- GitHub Releases API check, version comparison, per-platform cargo-dist asset selection (`asset_name_for`), download, archive extraction (shelled out, no archive crate: `tar -xf` on unix, PowerShell `Expand-Archive` on Windows, binary then located by name under the extraction dir since the `.tar.xz` nests it in a `drip-<triple>/` subdir while the `.zip` puts `drip.exe` at the root), and install over the running binary (atomic same-dir rename on unix; rename-running-exe-aside on Windows) -- bd issue drip-01g.7 |
| `src/sources.rs` | `upsert_source` (idempotent `sources` row upsert, now topic-scoped via a required `topic_id`) plus `find_by_label`/`list`/`remove_by_label`/`set_source_topic` backing `drip source`; `set_source_topic` is the single place `sources.topic_id` is ever updated post-insert, called by `topics::move_source_to_topic` for `drip source move` (bd issue drip-38w.1/.2); `upsert_reddit_source` is `#[cfg(test)]`-only, a fixture builder for `dedup.rs`/`fetch_runs.rs`/its own tests (bd issue drip-1uk.9) |
| `src/topics.rs` | `create_topic`/`list_topics`/`remove_topic`/`sources_for_topic` backing `drip topic add/list/remove`, plus `require_topic_id` (looks up a topic id or errors pointing at `drip topic add`, used by `drip source add`/`drip source move`), `move_source_to_topic` (backs `drip source move`, thin wrapper over `sources::set_source_topic`), and `topic_source_count` (backs `drip topic remove`'s refuse-while-non-empty guard) — the old `add_source_to_topic`/`remove_source_from_topic` many-to-many membership functions are gone (bd issue drip-38w.1/.2: a source belongs to exactly one topic now, assigned at `drip source add`/`drip source move` time instead). `sources_for_topic` is also what `handle_fetch`'s `--topic` resolution in `src/main.rs` calls to expand a topic into its member sources (bd issue drip-p6v) |
| `src/settings.rs` | Key-value `settings` table (folders, date format, defaults, and the reddit-fetch pacing knobs `reddit_request_delay_secs`/`reddit_retry_max`/`reddit_retry_base_secs`, bd issue drip-6xz) |
| `src/dedup.rs` | Per-source dedup against `seen_items` (`filter_unseen`/`record_seen`), generic over `Item` |
| `src/fetch_runs.rs` | Logs every fetch invocation's outcome to `fetch_runs`/`fetch_run_sources` |

For schema details (columns, constraints, why a field is nullable, the `fetch_limit`-not-`limit` reserved-word gotcha), read the migration files directly — their header comments are the source of truth, not a summary here.

For the design reasoning behind any of the above (why SQLite, why dedup is per-source, why RSS support introduced a normalized `Item` type, why Reddit's OAuth path was removed in favor of RSS-only), check bd: `bd show drip-15n.9` for the storage-migration epic, `bd show drip-15n.9.6` for the RSS design decision specifically, `bd show drip-1uk` for the OAuth-removal epic, `bd show drip-15n` for the project epic, `bd ready`/`bd list --status=open` for current work.

## Conventions & Patterns

- **Explicit params over whole-`Config` params.** `digest.rs`/`journal.rs` take `vault_path`/`posts_folder`/etc. as separate arguments rather than a `&Config`, so they stay unit-testable without a real config file. Follow this for new vault-writing code.
- **`PRAGMA foreign_keys` is per-connection, not persisted by SQLite.** `db::open` sets it immediately after every `Connection::open`. Any new code path that opens its own connection must do the same, or `ON DELETE CASCADE` silently stops working.
- **Migrations are additive and versioned via `PRAGMA user_version`.** Add a new `migrations/000N_*.sql` ending in its own `PRAGMA user_version = N;`, then register it in `db::MIGRATIONS`. Never edit an already-shipped migration file.
- **No credentials of any kind are needed or stored anywhere.** `drip` fetches every source kind (Reddit, RSS, YouTube) via a plain unauthenticated HTTP GET against a public feed URL — there is no API key, OAuth flow, or credential storage in this codebase (there used to be, for Reddit's OAuth `client_credentials` API; it was removed, see bd issue drip-1uk).
- **Dedup is per-source, not global.** A crosspost of the same post into two subreddits counts as two distinct items, per `seen_items`'s `UNIQUE(source_id, external_id)` constraint. `src/dedup.rs`'s `filter_unseen`/`record_seen` only ever look up/write a single `source_id` at a time.
- **Source identity across kinds is `(kind, name)`, never bare `name`.** `main.rs`'s `handle_fetch` keys its `source_ids` map by `(kind, name)` tuples specifically because two sources of different kinds can legitimately share a label string (e.g. both named "rust") without being the same source — keying by bare name caused a confirmed crash + silent dedup corruption during drip-15n.9.6's review. `--source` lists are also deduplicated (order-preserving) before fetching, since an exact duplicate entry causes a `fetch_run_sources` primary-key collision otherwise.
- **Tests never hit real services or real user state.** RSS calls are mocked with `mockito`; vault/config/DB paths use `tempfile::tempdir()` fixtures. Never point a test at the real `~/.config/drip/` or a real Obsidian vault.
- **`reqwest` uses the `native-tls` feature, not `rustls-tls`.** Confirmed live: Reddit's edge returns a hard `403 Forbidden` to `reqwest`'s `rustls` backend even as the first request in a freshly-reset rate-limit window, while `curl` from the same machine succeeds — looks like TLS-client fingerprinting, not ordinary rate-limiting (`429`). Switching to `native-tls` (the system's real TLS library) resolved it; verified end-to-end against real `r/rust`. This means `drip` now needs system OpenSSL dev headers to build (`libssl-dev`/`pkg-config` on Debian/Ubuntu — see README Prerequisites), unlike `rusqlite`'s `bundled` feature, which needs nothing external. Don't revert to `rustls-tls` without re-confirming this is still needed — Reddit's bot-detection behavior could change.

## Claude Code Skill Maintenance

- **Keep `.claude/skills/drip/SKILL.md` in sync with the real CLI.** Any change touching `src/cli.rs` (new/removed/renamed subcommand or flag, changed default, changed semantics) or adding a new source kind or settings key (`src/types.rs`, `src/settings.rs`) must update `.claude/skills/drip/SKILL.md` in the same change. A skill that's drifted out of sync with the real CLI is worse than no skill at all — an agent will confidently give wrong flag guidance.

## Build & Test

```bash
cargo build
cargo test
```

No lint/format gate is currently wired into CI (there is no CI config in this repo) — run `cargo fmt`/`cargo clippy` locally as good practice, but they aren't enforced.
