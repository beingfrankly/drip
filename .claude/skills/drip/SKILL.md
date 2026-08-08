---
name: drip
description: Use when the user wants to fetch Reddit/RSS/YouTube content into an Obsidian vault digest with the `drip` CLI, manage `drip source` entries or `drip topic` groups of sources, configure `drip` settings, or troubleshoot drip's fetch/dedup/update behavior.
---

# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts (via Reddit's own public, unauthenticated RSS/Atom feed — no API key or OAuth) and RSS/Atom feed entries (including YouTube channel feeds) from sources you register, normalizes everything into a shared `Item` type, writes them as one markdown "digest" note per fetch run into an Obsidian vault, and appends a reference bullet to that day's daily journal note.

## Command reference

### `drip init`

Interactive first-run wizard. Sets `vault_path` in `config.toml` and seeds SQLite `settings` (posts folder, daily notes folder, daily note date format, default sort, default limit). Can optionally install a daily cron entry for unattended fetches; re-running and confirming again updates that entry in place rather than duplicating it.

### `drip source add --kind rss|youtube|reddit --url <url> --name <label> --topic <sub-topic> [--exclude <term>[,<term>...]]`

Registers a source under a fetchable label and creates exactly one **ruleless** (accept-everything) link into `--topic`. `--topic` is required, must **already exist**, and must be a **leaf sub-topic** — `drip source add` does NOT auto-create it, and rejects a bare main topic:

```
no topic named '<name>'; create it first with `drip topic add --name <name>`
'<name>' is a main topic; sources link to sub-topics only -- create one with `drip topic add --name <sub-topic> --parent <name>`
```

So the order is always: `drip topic add --name <main>` → `drip topic add --name <sub> --parent <main>` → `drip source add ... --topic <sub>`. Use `drip source link` afterwards to add keyword rules or link the source into further sub-topics.

`--exclude <term>[,<term>...]` sets the source's **title-only** exclude terms — a pre-filter applied before any sub-topic's rules run at all. **Declarative/replacing**: re-running `source add` with a different `--exclude` list replaces it wholesale; omitting the flag clears it.

`--url`'s meaning depends on `--kind`:

- `--kind rss`: a genuine RSS/Atom feed URL (e.g. `https://blog.rust-lang.org/feed.xml`).
- `--kind youtube`: a channel id (starts with `UC`), a `https://www.youtube.com/channel/UC.../` URL, or a `@handle` / `https://www.youtube.com/@handle` URL (bd issue drip-ho5.11) — the form YouTube shows in its own address bar today. A `@handle` resolves via a one-time network fetch of the handle's channel page at `source add` time (scraping its channel id out of the page markup); the other forms resolve with no network. Either way, the *stored* source is always the resolved `feeds/videos.xml?channel_id=UC...` URL, so ordinary fetches never make an extra request. `/c/{name}` and `/user/{name}` custom-URL forms remain unsupported — find the channel's canonical channel id/URL instead (channel's About page, or page source for `"channelId":"UC...`). If handle resolution fails (network error, handle not found, or no channel id found on the page), the error explains that and tells you to pass the `UC...` channel id directly instead.
- `--kind reddit`: the **bare subreddit name** (e.g. `rust`), not a URL — `drip` builds the subreddit's own public RSS/Atom feed URL from it.

Reddit-only flags on `source add` (ignored for other kinds):

- `--sort <hot|top|new|rising|controversial>` (default `hot`)
- `--time <hour|day|week|month|year|all>` — only meaningful with `--sort top`/`controversial`
- `--search <term>` — free-text Reddit search within the subreddit; **not** a flair filter (flair isn't exposed by this feed)

These are baked into the feed URL at `source add` time, not at fetch time.

### `drip source link --name <label> --topic <sub-topic> [--match <term>[,<term>...]] [--exclude <term>[,<term>...]] [--match-body]`

Declaratively (re)configures the link between an already-saved source and a (leaf) sub-topic — creating it if it doesn't exist yet. **`--match`/`--exclude` REPLACE that link's entire include/exclude term lists wholesale**, not append: re-running the exact same command is idempotent and produces identical state, which is what makes a shell script the reproducible way to author a source's rule set. Omitting `--match`/`--exclude` clears that side. `--match-body` also matches an item's body/summary text for this link, not just its title.

`--topic` must already exist and be a leaf sub-topic — linking directly to a main topic is rejected, same error as `source add`. **Never touches `seen_items`**: editing a link's rules is a plain row-level change against `link_rules`, not a remove-and-re-add of the source, so tuning rules never resets that source's dedup ledger.

A source can link into more than one sub-topic at once — each link has its own rules and its own `--match-body` setting.

### `drip source unlink --name <label> --topic <sub-topic>`

Removes the link (and its rules) between a saved source and a sub-topic. A source with no remaining links still exists — it just routes nowhere until linked again. Unlinking a never-linked pair is a harmless no-op.

### `drip source list`

Lists saved sources, each with its source-level excludes (if any) and every sub-topic link it has, with that link's rules:

```
- rust-hot (kind: reddit, url: rust)
    -> releases (Rust): match=1.0,1.1,1.2
    -> general (Rust): ruleless
```

### `drip source remove --name <label>`

Removes a saved source by label (and cascades its links).

### `drip topic add|rename|reparent|remove|list|test`

Topics are a **two-level tree**: main topics and their sub-topics (migration `0006_topic_tree.sql`). Sources link into **leaf sub-topics only**, never a main topic directly, via `drip source add --topic`/`drip source link --topic`; each link carries its own keyword rules that route a fetched item into that sub-topic. `--topic <main>` on `drip fetch` expands to every sub-topic beneath it (a main topic owns no sources directly).

- `drip topic add --name <name> [--parent <main>]` — create a new main topic, or (with `--parent`) a sub-topic under an already-existing main topic. **Exactly two levels are enforced in app code**: naming a sub-topic as `--parent` (i.e. one that itself already has a parent) is rejected. Errors clearly if the name is already taken.
- `drip topic rename --name <old> --to <new>` — renames a topic. **Future-notes-only**: updates the DB but never rewrites an already-written digest note. If TODAY's digest note already has a section under the old name, this **warns** (the rename still succeeds) rather than rewriting the note, since headings are matched by exact text and a same-day fetch would otherwise add a second, differently-named section alongside the existing one.
- `drip topic reparent --name <sub-topic> --parent <new-main>` — moves a sub-topic under a different main topic. Same future-notes-only warning as `rename` if today's note already has a section for it under its previous main. Rejects moving a main topic (nothing to reparent) or reparenting under something that isn't itself a main topic.
- `drip topic remove --name <name>` — delete a topic. **Refuses while it has any descendant**:
  - a main topic that still has sub-topics:
    ```
    topic '<name>' still has N sub-topic(s); remove them first
    ```
  - a topic (main or sub) that still has directly-linked sources:
    ```
    topic '<name>' still has N source(s); unlink them first (e.g. `drip source unlink --name <label> --topic <name>`) before removing it
    ```

  Removing an empty topic still works. Removing an unknown topic name is still benign (prints `no topic named '<name>'`, not an error).
- `drip topic list` — lists every saved topic as a two-level tree: each main topic, followed immediately by its own sub-topics (indented two spaces), each with its directly-linked sources' labels.
- `drip topic test --title "..."` — **offline, no-network** explain surface: classifies a synthetic item (title only) against every saved source's sub-topic links, printing which links match, which terms fired, and where the item would land, e.g.:
  ```
  cc-feed -> cc hooks  MATCH  (hook)
  would route to: Claude > cc hooks
  ```
  Answers "why did nothing land in this sub-topic?" without spending a real fetch — pairs with `-v`'s dropped-item titles (see Gotchas below), which covers "what did my rules miss" against live data.

**Topic name rules** (enforced on `add`/`rename`): `,` `[` `]` `{` `}`, newlines, and other control characters are rejected anywhere in the name; a leading YAML sigil (`&` `*` `!` `%` `@` `` ` `` `#`) is rejected; leading/trailing whitespace is trimmed, and empty-after-trimming is rejected; `/` is reserved (illegal now, kept free for future path addressing). Everything else is legal — `C++`, `Node.js`, `.NET`, `AI & ML` all work. The comma rule matters twice over: the digest note's frontmatter is unquoted YAML (`topics: [...]`), and `--topic` on `drip fetch` uses `value_delimiter = ','`, so a comma in a name would silently split it into two.

### `drip fetch --source <label>[,<label>...] --topic <name>[,<name>...] --all [flags]`

Fetches one or more saved sources (comma-separated, or repeat `--source`) and/or one or more saved topics (comma-separated, or repeat `--topic`) and/or every saved source (`--all`) into one combined digest note, then appends the journal reference (unless suppressed).

Flags:

- `--sort <hot|top|new|rising|controversial>` — labels the digest note's frontmatter/header only. Falls back to the saved `default_sort` setting.
- `--time <hour|day|week|month|year|all>` — labels the digest note only.
- `-q`/`--query <term>` — labels the digest note only.
- `-n`/`--limit <n>` — caps how many items are **written per source**, applied AFTER dedup and keyword-rule classification, not before (see the Gotchas entry below). Falls back to saved `default_limit`.
- `--tag <tag>[,<tag>...]` — adds real Obsidian tags to the digest note (repeat flag or comma-separate). Falls back to saved `default_tags`.
- `--topic <name>[,<name>...]` — each named topic (see `drip topic add`/`drip topic list`) is resolved into its member sources' labels and merged with any `--source` labels given in the same invocation. A source named by both `--source` and a `--topic` it belongs to is still fetched exactly once, not twice. An unknown topic name warns clearly (`no topic named '<name>' (run \`drip topic list\`)`) rather than aborting the whole fetch.
- `--all` — fetch every saved source (see `drip source list`), regardless of `--source`/`--topic` selection. Merges/dedups with any `--source`/`--topic` also given, so a source selected more than one way is still fetched exactly once. Since a topic is just a named group of already-saved sources, `--all` inherently covers everything any topic references — it does not need to iterate topics separately. With no saved sources at all, prints a clear message to stderr and writes nothing (`drip fetch: --all given but no sources are saved yet (run \`drip source add\` first)`). Useful for a stable unattended cron command that doesn't need to enumerate labels.
- `--folder <name>` — overrides the configured posts folder for this run only.
- `--no-journal` — skip appending a reference to the daily journal note.
- `--dry-run` — preview both writes (digest note + journal reference) without touching disk; when a note for the day already exists, the preview shows the append/merge result, not a fresh note.
- `-v`/`--verbose` — diagnostic output (request URLs, rate-limit waits, loaded config/parsed args).

### `drip config show|edit|set <key> <value>`

- `show` — print current configuration (`config.toml` + settings).
- `edit` — open `config.toml` in `$EDITOR`.
- `set <key> <value>` — set one SQLite-backed setting. Valid keys: `posts_folder`, `daily_notes_folder`, `daily_note_format`, `default_sort`, `default_limit`, `default_tags`, `reddit_request_delay_secs`, `reddit_retry_max`, `reddit_retry_base_secs`.
  - `reddit_request_delay_secs` (default `10`), `reddit_retry_max` (default `4`), `reddit_retry_base_secs` (default `5`) tune how `drip fetch` paces reddit requests to avoid HTTP 429 rate-limiting — see the reddit-throttling gotcha below.

### `drip update [--check] [-y]`

Checks GitHub Releases for a newer tag than the running binary's version. `--check` reports only, without installing. `-y` skips the install confirmation prompt. Downloads and installs over the currently running binary, wherever it lives. Works on every platform drip publishes prebuilt binaries for — **Linux x86_64, macOS (x86_64 and Apple Silicon), and Windows x86_64** (the cargo-dist release targets); on any other platform it reports that no prebuilt binary is available and points at `cargo install`/the releases page instead.

## Digest format

Every fetch writes one markdown note into `Resources/drip` (the `posts_folder` setting), grouped by `(main topic, sub-topic)` — items are keyword-classified into sub-topics (bd issue drip-98u, epic drip-ho5), not grouped by which source fetched them:

- **Frontmatter:** `tags:` (only the user/`default_tags` tags, e.g. `drip` — `tags: []` if empty), `createdOn`, `modifiedOn`, `topics: [...]` (distinct **main** topics referenced, first-seen order), `subtopics: [...]` (distinct sub-topics referenced, first-seen order), `sources: [...]` (every source that fetched successfully this run, deduped — **including one that contributed zero items**, since this list means "what this run looked at", not "where these items came from"), `sort`, `time_filter`, `query`, `fetched_count`.
- **Filename / title:** `<YYYY-MM-DD> - Daily digest.md` (local ISO date only — no time, no topic/source label). One note per calendar day.
- **Body:** an H1 `# <YYYY-MM-DD> - Daily digest`, then a `**Sources:** ... · **Sort:** ... · **Query:** ...` summary line (bare source labels, no `r/` prefix — a `--kind reddit` source's label isn't necessarily its real subreddit name, e.g. a search-scoped label like `cc-hooks`), then for each main topic an H2 `## <main topic>`, under it each sub-topic with at least one routed item an H3 `### <sub-topic>` (a sub-topic with zero routed items this run is omitted entirely — not rendered as an empty heading), under it each item as a title-only Obsidian checkbox task: `- [ ] **[<title>](<url>)**`, with a leading `⚠️ NSFW ` marker on NSFW items. **No source subheading, no author suffix, no score/comment count/flair/summary excerpt** — and no LLM summaries, by design.
- **An item can appear under two different H3s.** Classification has no precedence: an item matching two sub-topics' keyword rules renders once under each (bd issue drip-98u.3) — this is treated as useful signal that the two rulesets overlap, not a bug to dedupe away.
- **`fetched_count` counts rendered checkbox *lines*, not distinct items.** It deliberately diverges from the journal bullet's "N posts" count (which counts distinct items) — an item that multi-matches into two sub-topics adds 2 to `fetched_count` but only 1 to the journal bullet's post count.

The checkbox items are the point: they're plain Obsidian tasks, surfaced elsewhere via an Obsidian Base and the Taskforge iOS app, so the user can tick each one off as they clip it (processed) or decide it's not interesting ("simple done") — independent of this skill or `drip` itself.

## Gotchas

- **`fetch --sort`/`--time`/`-q`/`--query` are cosmetic only.** They label the digest note's frontmatter/header and never filter or search what gets fetched. Real Reddit sort/time-window/search must be set at `drip source add --kind reddit --sort/--time/--search` registration time instead.
- **`-n`/`--limit` on `fetch` is per-source, applied AFTER dedup and classification, not before.** The per-source pipeline is `fetch → dedup → classify → truncate` (bd issue drip-98u.4) — the cap is "at most N items **routed** from this source", not "take the first N raw fetched items and see what routes". This matters: truncating the raw feed first can leave zero routable items if a source's noisiest posts happen to sort first. `drip fetch --source a,b -n 5` can still write up to 10 distinct items total (5 from each source, not 5 combined) — and because the same distinct item can multi-match into two sub-topics, it can render as more than 5 checkbox lines for that one source.
- **Items are keyword-classified into sub-topics, and an item matching nothing is dropped (not written anywhere).** Classification is driven by `topic_links`/`link_rules` rows on each source — an empty rule set matches everything (the behavior every pre-existing source got automatically when the two-level topic tree first landed), while a source with real keyword rules configured will silently drop any item matching none of them. Dropped/excluded counts are reported in `fetch`'s normal output; `-v` additionally lists each dropped item's title. A dropped/excluded item is **not** recorded as seen, so it stays eligible for a future run (e.g. after a rule is widened) as long as it's still in the feed window. `drip fetch --topic <name>` further restricts classification to only `<name>`'s own rules, even if the fetched source is also linked into other sub-topics — a direct `drip fetch --source <label>` (no topic given) classifies against every rule the source is linked to.
- **Dedup is per-source, not global.** Tracked via `UNIQUE(source_id, external_id)` in `seen_items`; a crosspost of the same post into two subreddits counts as two distinct items. An item already written to an *earlier* digest for a given source won't reappear — an item is recorded as seen only when it's actually written to a digest (never on a `--dry-run`), so each item appears in exactly one digest. If a fetch turns up nothing new for all requested sources, the day's note is left unchanged (and none is written if there isn't one yet).
- **Source identity is `(kind, name)`, never bare `name`.** Two sources of different kinds may legitimately share the same label string without colliding. `--source` lists passed to `fetch` are also deduplicated (order-preserving) before fetching.
- **No credentials of any kind are ever needed.** Every source kind (Reddit, RSS, YouTube) is fetched via a plain unauthenticated HTTP GET against a public feed URL — no API key, app registration, or OAuth flow anywhere in this tool.
- **`--tag` on `fetch` adds real Obsidian tags** to the digest note (not just a label), unlike `--sort`/`--time`/`--query`.
- **The digest note is one-per-day, named `<YYYY-MM-DD> - Daily digest.md`** (local ISO date, no time, no topic/source label). A second `drip fetch` on the same calendar day **appends/merges** into that day's existing note (bd issue drip-47u): genuinely-new items are inserted under the right `## <main topic>` / `### <sub-topic>` headings, while existing lines — including ticked checkboxes (`- [x]`) and manual edits — are preserved untouched. A re-run with nothing new leaves the note byte-for-byte unchanged. `--dry-run` previews the merge, not a fresh note.
- **A source can link into more than one sub-topic at once — links are many-to-many, not exclusive.** `drip source add --topic <sub>` creates one link; `drip source link`/`drip source unlink` add/remove further links (each with its own rules). To "move" a source, `unlink` the old sub-topic and `link` the new one — there's no single wholesale-move command (`drip source move` was removed, bd issue drip-ho5.8). `drip topic remove` refuses while it still has any descendant — a sub-topic while it still has direct source links, a main topic while it still has sub-topics; unlink/remove them first. An empty topic can always be removed, and doing so never deletes the sources that were linked to it.
- **Reddit fetches are throttled + retried to dodge HTTP 429 (per-IP, global).** `drip fetch` spaces reddit requests `reddit_request_delay_secs` apart (default 10s), widening after each 429 it sees ("pressure"), retries a 429 up to `reddit_retry_max` times (default 4, honoring `Retry-After` then exponential backoff with base `reddit_retry_base_secs`, default 5s), and runs a **final retry pass** over any source still rate-limited after a longer cooldown. Anything still 429 after that is skipped for the run and picked up next run (dedup avoids dupes). RSS/YouTube feeds are never throttled. Tune via `drip config set reddit_request_delay_secs <n>` etc. if you fetch many reddit sources at once.

## Example workflow

```bash
# Create the main topic, then a leaf sub-topic under it -- `drip source add`
# requires an existing LEAF sub-topic, never a bare main topic
drip topic add --name rust
drip topic add --name "rust news" --parent rust

# Register a Reddit source with a real sort/time/search baked in, linked into
# that sub-topic with one ruleless (accept-everything) link
drip source add --kind reddit --url rust --sort top --time week --search "async" --name rust-async-weekly --topic "rust news"

# Fetch it on its own
drip fetch --source rust-async-weekly --tag rust --dry-run

# Register an RSS source into the same sub-topic
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog --topic "rust news"

# Fetch both together in one combined digest
drip fetch --source rust-async-weekly,rust-blog -n 5 --tag rust

# Add real keyword rules to a link -- declarative: re-running with a
# different --match list REPLACES it wholesale, which is what makes this
# script the reproducible config
drip topic add --name releases --parent rust
drip source link --name rust-blog --topic releases --match "1.0,1.1,1.2"

# Sanity-check a rule offline, no network, before spending a real fetch
drip topic test --title "Rust 1.80 released"

# Reassign a source to a different sub-topic: unlink the old one, link the new
drip topic add --name programming
drip topic add --name "programming news" --parent programming
drip source unlink --name rust-blog --topic "rust news"
drip source link --name rust-blog --topic "programming news"

# Fetch the whole main topic in one go -- expands to every sub-topic beneath it
drip fetch --topic rust --tag rust

# Fetch every saved source in one combined digest -- e.g. for a stable
# unattended cron command that doesn't need to enumerate labels
drip fetch --all --tag digest
```
