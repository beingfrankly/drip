---
name: drip
description: Use when the user wants to fetch Reddit/RSS/YouTube content into an Obsidian vault digest with the `drip` CLI, manage `drip source` entries or `drip topic` groups of sources, configure `drip` settings, or troubleshoot drip's fetch/dedup/update behavior.
---

# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts (via Reddit's own public, unauthenticated RSS/Atom feed — no API key or OAuth) and RSS/Atom feed entries (including YouTube channel feeds) from sources you register, normalizes everything into a shared `Item` type, writes them as one markdown "digest" note per fetch run into an Obsidian vault, and appends a reference bullet to that day's daily journal note.

## Command reference

### `drip init`

Interactive first-run wizard. Sets `vault_path` in `config.toml` and seeds SQLite `settings` (posts folder, daily notes folder, daily note date format, default sort, default limit). Can optionally install a daily cron entry for unattended fetches; re-running and confirming again updates that entry in place rather than duplicating it.

### `drip source add --kind rss|youtube|reddit --url <url> --name <label>`

Registers a source under a fetchable label. `--url`'s meaning depends on `--kind`:

- `--kind rss`: a genuine RSS/Atom feed URL (e.g. `https://blog.rust-lang.org/feed.xml`).
- `--kind youtube`: a channel id (starts with `UC`) or a `https://www.youtube.com/channel/UC.../` URL. Handle-style URLs (`/@name`) are **not** supported — resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (channel's About page, or page source for `"channelId":"UC...`).
- `--kind reddit`: the **bare subreddit name** (e.g. `rust`), not a URL — `drip` builds the subreddit's own public RSS/Atom feed URL from it.

Reddit-only flags on `source add` (ignored for other kinds):

- `--sort <hot|top|new|rising|controversial>` (default `hot`)
- `--time <hour|day|week|month|year|all>` — only meaningful with `--sort top`/`controversial`
- `--search <term>` — free-text Reddit search within the subreddit; **not** a flair filter (flair isn't exposed by this feed)

These are baked into the feed URL at `source add` time, not at fetch time.

### `drip source list`

Lists saved sources.

### `drip source remove --name <label>`

Removes a saved source by label.

### `drip topic add|add-source|remove-source|remove|list`

Topics are named groups of saved sources, so a recurring set of labels can be fetched as one unit instead of typing every member label on every `drip fetch --source ...`.

- `drip topic add --name <name>` — create a new (empty) topic. Errors clearly if the name is already taken.
- `drip topic add-source --topic <name> --source <label>` — attach an already-saved source (by its `drip source add`/`drip source list` label) to a topic. Adding the same source to the same topic twice is a no-op, not an error. Errors clearly if either the topic or the source doesn't exist.
- `drip topic remove-source --topic <name> --source <label>` — detach a source from a topic. Not being a member is a no-op, not an error.
- `drip topic remove --name <name>` — delete a topic. Does **not** delete its member sources — only the topic and its membership rows.
- `drip topic list` — list every saved topic with its member sources' labels.

### `drip fetch --source <label>[,<label>...] --topic <name>[,<name>...] [flags]`

Fetches one or more saved sources (comma-separated, or repeat `--source`) and/or one or more saved topics (comma-separated, or repeat `--topic`) into one combined digest note, then appends the journal reference (unless suppressed).

Flags:

- `--sort <hot|top|new|rising|controversial>` — labels the digest note's frontmatter/header only. Falls back to the saved `default_sort` setting.
- `--time <hour|day|week|month|year|all>` — labels the digest note only.
- `-q`/`--query <term>` — labels the digest note only.
- `-n`/`--limit <n>` — caps how many items are taken **per source**, before dedup. Falls back to saved `default_limit`.
- `--tag <tag>[,<tag>...]` — adds real Obsidian tags to the digest note (repeat flag or comma-separate). Falls back to saved `default_tags`.
- `--topic <name>[,<name>...]` — each named topic (see `drip topic add`/`drip topic list`) is resolved into its member sources' labels and merged with any `--source` labels given in the same invocation. A source named by both `--source` and a `--topic` it belongs to is still fetched exactly once, not twice. An unknown topic name warns clearly (`no topic named '<name>' (run \`drip topic list\`)`) rather than aborting the whole fetch.
- `--folder <name>` — overrides the configured posts folder for this run only.
- `--no-journal` — skip appending a reference to the daily journal note.
- `--dry-run` — preview both writes (digest note + journal reference) without touching disk.
- `-v`/`--verbose` — diagnostic output (request URLs, rate-limit waits, loaded config/parsed args).

### `drip config show|edit|set <key> <value>`

- `show` — print current configuration (`config.toml` + settings).
- `edit` — open `config.toml` in `$EDITOR`.
- `set <key> <value>` — set one SQLite-backed setting. Valid keys: `posts_folder`, `daily_notes_folder`, `daily_note_format`, `default_sort`, `default_limit`, `default_tags`.

### `drip update [--check] [-y]`

Checks GitHub Releases for a newer tag than the running binary's version. `--check` reports only, without installing. `-y` skips the install confirmation prompt. Downloads and installs over the currently running binary, wherever it lives. **Linux x86_64 only** today (the only platform currently released).

## Gotchas

- **`fetch --sort`/`--time`/`-q`/`--query` are cosmetic only.** They label the digest note's frontmatter/header and never filter or search what gets fetched. Real Reddit sort/time-window/search must be set at `drip source add --kind reddit --sort/--time/--search` registration time instead.
- **`-n`/`--limit` on `fetch` is per-source, applied before dedup.** `drip fetch --source a,b -n 5` can write up to 10 items total (5 from each source), not 5 combined.
- **Dedup is per-source, not global.** Tracked via `UNIQUE(source_id, external_id)` in `seen_items`; a crosspost of the same post into two subreddits counts as two distinct items. An item already shown in a previous digest for a given source won't reappear. If a fetch turns up nothing new for all requested sources, no digest note is written.
- **Source identity is `(kind, name)`, never bare `name`.** Two sources of different kinds may legitimately share the same label string without colliding. `--source` lists passed to `fetch` are also deduplicated (order-preserving) before fetching.
- **No credentials of any kind are ever needed.** Every source kind (Reddit, RSS, YouTube) is fetched via a plain unauthenticated HTTP GET against a public feed URL — no API key, app registration, or OAuth flow anywhere in this tool.
- **`--tag` on `fetch` adds real Obsidian tags** to the digest note (not just a label), unlike `--sort`/`--time`/`--query`.
- **The digest filename only uses the topic's name when exactly one `--topic` is given and it resolves cleanly.** With zero topics it falls back to the joined source labels; with a `--topic` name that failed to resolve (e.g. a typo) it also falls back to the joined source labels; with more than one `--topic` it joins the topic names themselves (not their member source labels).
- **Removing a topic (`drip topic remove`) never deletes its member sources** — only the topic and its `topic_sources` membership rows. The sources themselves stay saved and fetchable by `--source`.

## Example workflow

```bash
# Register a Reddit source with a real sort/time/search baked in
drip source add --kind reddit --url rust --sort top --time week --search "async" --name rust-async-weekly

# Fetch it on its own
drip fetch --source rust-async-weekly --tag rust --dry-run

# Register an RSS source
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog

# Fetch both together in one combined digest
drip fetch --source rust-async-weekly,rust-blog -n 5 --tag rust

# Group them into a topic instead of typing both labels every time
drip topic add --name rust
drip topic add-source --topic rust --source rust-async-weekly
drip topic add-source --topic rust --source rust-blog

# Fetch the whole topic in one go -- digest filename/header is labeled "rust"
drip fetch --topic rust --tag rust
```
