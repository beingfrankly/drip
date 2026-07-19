---
name: drip
description: Use when the user wants to fetch Reddit/RSS/YouTube content into an Obsidian vault digest with the `drip` CLI, manage `drip source` entries or `drip topic` groups of sources, configure `drip` settings, or troubleshoot drip's fetch/dedup/update behavior.
---

# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts (via Reddit's own public, unauthenticated RSS/Atom feed — no API key or OAuth) and RSS/Atom feed entries (including YouTube channel feeds) from sources you register, normalizes everything into a shared `Item` type, writes them as one markdown "digest" note per fetch run into an Obsidian vault, and appends a reference bullet to that day's daily journal note.

## Command reference

### `drip init`

Interactive first-run wizard. Sets `vault_path` in `config.toml` and seeds SQLite `settings` (posts folder, daily notes folder, daily note date format, default sort, default limit). Can optionally install a daily cron entry for unattended fetches; re-running and confirming again updates that entry in place rather than duplicating it.

### `drip source add --kind rss|youtube|reddit --url <url> --name <label> --topic <name>`

Registers a source under a fetchable label. **Every source belongs to exactly one topic** — `--topic` is required, and the named topic must **already exist** (`drip source add` does NOT auto-create it). If it doesn't, this errors:

```
no topic named '<name>'; create it first with `drip topic add --name <name>`
```

So the order is always: `drip topic add --name <name>` first, then `drip source add ... --topic <name>`.

`--url`'s meaning depends on `--kind`:

- `--kind rss`: a genuine RSS/Atom feed URL (e.g. `https://blog.rust-lang.org/feed.xml`).
- `--kind youtube`: a channel id (starts with `UC`) or a `https://www.youtube.com/channel/UC.../` URL. Handle-style URLs (`/@name`) are **not** supported — resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (channel's About page, or page source for `"channelId":"UC...`).
- `--kind reddit`: the **bare subreddit name** (e.g. `rust`), not a URL — `drip` builds the subreddit's own public RSS/Atom feed URL from it.

Reddit-only flags on `source add` (ignored for other kinds):

- `--sort <hot|top|new|rising|controversial>` (default `hot`)
- `--time <hour|day|week|month|year|all>` — only meaningful with `--sort top`/`controversial`
- `--search <term>` — free-text Reddit search within the subreddit; **not** a flair filter (flair isn't exposed by this feed)

These are baked into the feed URL at `source add` time, not at fetch time.

### `drip source move --name <label> --topic <name>`

Reassigns an already-saved source to a different (existing) topic. This is the **only** way to change a source's topic after it's been registered — since a source always belongs to exactly one topic, there's no separate "add to topic"/"remove from topic" operation; you move it instead. The destination topic must already exist (same "create it first with `drip topic add`" error as `source add` if it doesn't). Moving a source to the topic it's already in is a harmless no-op.

### `drip source list`

Lists saved sources, each showing its topic:

```
- rust-hot (topic: rust, kind: reddit, url: rust)
```

### `drip source remove --name <label>`

Removes a saved source by label.

### `drip topic add|list|remove`

Topics are named groups of saved sources, so a recurring set of labels can be fetched as one unit instead of typing every member label on every `drip fetch --source ...`. **A source belongs to exactly one topic at a time** — assign it at `drip source add --topic` time, or reassign it with `drip source move --topic`. There is no many-to-many membership; `drip topic` itself only manages topics (create/list/remove), not source membership.

- `drip topic add --name <name>` — create a new (empty) topic. Errors clearly if the name is already taken.
- `drip topic remove --name <name>` — delete a topic. **Refuses if the topic still owns any sources**, with:
  ```
  topic '<name>' still has N source(s); move them to another topic first (e.g. `drip source move --name <label> --topic <other>`) before removing it
  ```
  Removing an empty topic still works. Removing an unknown topic name is still benign (prints `no topic named '<name>'`, not an error).
- `drip topic list` — list every saved topic with its member sources' labels.

### `drip fetch --source <label>[,<label>...] --topic <name>[,<name>...] --all [flags]`

Fetches one or more saved sources (comma-separated, or repeat `--source`) and/or one or more saved topics (comma-separated, or repeat `--topic`) and/or every saved source (`--all`) into one combined digest note, then appends the journal reference (unless suppressed).

Flags:

- `--sort <hot|top|new|rising|controversial>` — labels the digest note's frontmatter/header only. Falls back to the saved `default_sort` setting.
- `--time <hour|day|week|month|year|all>` — labels the digest note only.
- `-q`/`--query <term>` — labels the digest note only.
- `-n`/`--limit <n>` — caps how many items are taken **per source**, before dedup. Falls back to saved `default_limit`.
- `--tag <tag>[,<tag>...]` — adds real Obsidian tags to the digest note (repeat flag or comma-separate). Falls back to saved `default_tags`.
- `--topic <name>[,<name>...]` — each named topic (see `drip topic add`/`drip topic list`) is resolved into its member sources' labels and merged with any `--source` labels given in the same invocation. A source named by both `--source` and a `--topic` it belongs to is still fetched exactly once, not twice. An unknown topic name warns clearly (`no topic named '<name>' (run \`drip topic list\`)`) rather than aborting the whole fetch.
- `--all` — fetch every saved source (see `drip source list`), regardless of `--source`/`--topic` selection. Merges/dedups with any `--source`/`--topic` also given, so a source selected more than one way is still fetched exactly once. Since a topic is just a named group of already-saved sources, `--all` inherently covers everything any topic references — it does not need to iterate topics separately. With no saved sources at all, prints a clear message to stderr and writes nothing (`drip fetch: --all given but no sources are saved yet (run \`drip source add\` first)`). Useful for a stable unattended cron command that doesn't need to enumerate labels.
- `--folder <name>` — overrides the configured posts folder for this run only.
- `--no-journal` — skip appending a reference to the daily journal note.
- `--dry-run` — preview both writes (digest note + journal reference) without touching disk.
- `-v`/`--verbose` — diagnostic output (request URLs, rate-limit waits, loaded config/parsed args).

### `drip config show|edit|set <key> <value>`

- `show` — print current configuration (`config.toml` + settings).
- `edit` — open `config.toml` in `$EDITOR`.
- `set <key> <value>` — set one SQLite-backed setting. Valid keys: `posts_folder`, `daily_notes_folder`, `daily_note_format`, `default_sort`, `default_limit`, `default_tags`, `reddit_request_delay_secs`, `reddit_retry_max`, `reddit_retry_base_secs`.
  - `reddit_request_delay_secs` (default `10`), `reddit_retry_max` (default `4`), `reddit_retry_base_secs` (default `5`) tune how `drip fetch` paces reddit requests to avoid HTTP 429 rate-limiting — see the reddit-throttling gotcha below.

### `drip update [--check] [-y]`

Checks GitHub Releases for a newer tag than the running binary's version. `--check` reports only, without installing. `-y` skips the install confirmation prompt. Downloads and installs over the currently running binary, wherever it lives. Works on every platform drip publishes prebuilt binaries for — **Linux x86_64, macOS (x86_64 and Apple Silicon), and Windows x86_64** (the cargo-dist release targets); on any other platform it reports that no prebuilt binary is available and points at `cargo install`/the releases page instead.

## Digest format

Every fetch writes one markdown note into `Resources/drip` (the `posts_folder` setting), grouped topic → source → item:

- **Frontmatter:** `tags:` (only the user/`default_tags` tags, e.g. `drip` — `tags: []` if empty), `createdOn`, `modifiedOn`, `topics: [...]` (distinct topics referenced, first-seen order), `sources: [...]` (source labels fetched), `sort`, `time_filter`, `query`, `fetched_count`.
- **Body:** an H1 `# drip digest — <local timestamp>`, then a `**Sources:** ... · **Sort:** ... · **Query:** ...` summary line, then for each topic an H2 `## <topic>`, under it each source an H3 (`### r/<sub>` for reddit, `### <label>` for rss/youtube), under it each item as an Obsidian checkbox task: `- [ ] **[<title>](<url>)** — u/<author>` (reddit) or `— <author>` (rss/youtube), with a leading `⚠️ NSFW ` marker on NSFW items.
- No score, comment count, flair, or summary excerpt is rendered — and no LLM summaries, by design.

The checkbox items are the point: they're plain Obsidian tasks, surfaced elsewhere via an Obsidian Base and the Taskforge iOS app, so the user can tick each one off as they clip it (processed) or decide it's not interesting ("simple done") — independent of this skill or `drip` itself.

## Gotchas

- **`fetch --sort`/`--time`/`-q`/`--query` are cosmetic only.** They label the digest note's frontmatter/header and never filter or search what gets fetched. Real Reddit sort/time-window/search must be set at `drip source add --kind reddit --sort/--time/--search` registration time instead.
- **`-n`/`--limit` on `fetch` is per-source, applied before dedup.** `drip fetch --source a,b -n 5` can write up to 10 items total (5 from each source), not 5 combined.
- **Dedup is per-source, not global.** Tracked via `UNIQUE(source_id, external_id)` in `seen_items`; a crosspost of the same post into two subreddits counts as two distinct items. An item already shown in a previous digest for a given source won't reappear. If a fetch turns up nothing new for all requested sources, no digest note is written.
- **Source identity is `(kind, name)`, never bare `name`.** Two sources of different kinds may legitimately share the same label string without colliding. `--source` lists passed to `fetch` are also deduplicated (order-preserving) before fetching.
- **No credentials of any kind are ever needed.** Every source kind (Reddit, RSS, YouTube) is fetched via a plain unauthenticated HTTP GET against a public feed URL — no API key, app registration, or OAuth flow anywhere in this tool.
- **`--tag` on `fetch` adds real Obsidian tags** to the digest note (not just a label), unlike `--sort`/`--time`/`--query`.
- **The digest filename only uses the topic's name when exactly one `--topic` is given and it resolves cleanly.** With zero topics it falls back to the joined source labels; with a `--topic` name that failed to resolve (e.g. a typo) it also falls back to the joined source labels; with more than one `--topic` it joins the topic names themselves (not their member source labels).
- **Every source belongs to exactly one topic — there's no multi-assign.** `drip source add` requires `--topic`, and reassigning is `drip source move --name <label> --topic <name>`, not an "add to another topic" operation. `drip topic remove` refuses while it still owns any sources (move them out first); an empty topic can always be removed, and doing so never deletes the sources that were in it.
- **Reddit fetches are throttled + retried to dodge HTTP 429 (per-IP, global).** `drip fetch` spaces reddit requests `reddit_request_delay_secs` apart (default 10s), widening after each 429 it sees ("pressure"), retries a 429 up to `reddit_retry_max` times (default 4, honoring `Retry-After` then exponential backoff with base `reddit_retry_base_secs`, default 5s), and runs a **final retry pass** over any source still rate-limited after a longer cooldown. Anything still 429 after that is skipped for the run and picked up next run (dedup avoids dupes). RSS/YouTube feeds are never throttled. Tune via `drip config set reddit_request_delay_secs <n>` etc. if you fetch many reddit sources at once.

## Example workflow

```bash
# Create the topic first -- `drip source add` requires it to already exist
drip topic add --name rust

# Register a Reddit source with a real sort/time/search baked in, into that topic
drip source add --kind reddit --url rust --sort top --time week --search "async" --name rust-async-weekly --topic rust

# Fetch it on its own
drip fetch --source rust-async-weekly --tag rust --dry-run

# Register an RSS source into the same topic
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog --topic rust

# Fetch both together in one combined digest
drip fetch --source rust-async-weekly,rust-blog -n 5 --tag rust

# Reassign a source to a different (existing) topic
drip topic add --name programming
drip source move --name rust-blog --topic programming

# Fetch the whole topic in one go -- digest filename/header is labeled "rust"
drip fetch --topic rust --tag rust

# Fetch every saved source in one combined digest -- e.g. for a stable
# unattended cron command that doesn't need to enumerate labels
drip fetch --all --tag digest
```
