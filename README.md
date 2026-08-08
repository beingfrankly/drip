# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts and RSS/Atom feed entries from sources you choose, keyword-classifies each item into a two-level tree of topics, writes them as a "digest" markdown note into your Obsidian vault, and links that note from your daily journal note.

## Prerequisites

- **Rust**, via [rustup.rs](https://rustup.rs).
- **OpenSSL development headers** (e.g. `libssl-dev` + `pkg-config` on Debian/Ubuntu, `openssl-devel` on Fedora, or just Homebrew's OpenSSL on macOS) — `drip` links against the system's native TLS library rather than a bundled one (see "Why native TLS" below), so these need to be installed before `cargo build`/`cargo install` will succeed.

No Reddit API credentials, app registration, or API key of any kind is needed for any source `drip` supports — see "Usage" below.

## Install

### From a release binary

Each [GitHub release](https://github.com/beingfrankly/drip/releases) ships prebuilt binaries for Linux (x86_64/glibc), macOS (x86_64 and Apple Silicon), and Windows (x86_64), plus a shell installer that picks the right one for your platform:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/beingfrankly/drip/releases/latest/download/drip-installer.sh | sh
```

Or grab the archive for your platform straight from the releases page — e.g. `drip-x86_64-unknown-linux-gnu.tar.xz` (Linux), `drip-aarch64-apple-darwin.tar.xz` (Apple Silicon), `drip-x86_64-pc-windows-msvc.zip` (Windows) — unpack it, and move the `drip` binary onto your `PATH`:

```bash
curl -LO https://github.com/beingfrankly/drip/releases/latest/download/drip-x86_64-unknown-linux-gnu.tar.xz
tar -xf drip-x86_64-unknown-linux-gnu.tar.xz
sudo mv drip-x86_64-unknown-linux-gnu/drip /usr/local/bin/
```

### From source

```bash
cargo install --path .
```

## Update

### From a release binary

Repeat the download steps above with the new version's filename — it overwrites the binary already at `/usr/local/bin/drip`.

Or let `drip` do it for you:

```bash
drip update --check  # see if a newer version is available, without installing it
drip update          # download and install it in place (asks for confirmation first; -y skips that)
```

`drip update` replaces whichever binary is currently running (wherever it lives — `/usr/local/bin/drip`, `~/.cargo/bin/drip`, etc.) with the latest release from GitHub. It works on every platform drip publishes binaries for — Linux (x86_64), macOS (x86_64 and Apple Silicon), and Windows (x86_64) — downloading and unpacking the matching release archive in place. On any other platform it reports that no prebuilt binary is available and points you at `cargo install` / the releases page instead.

### From source

```bash
git pull
cargo install --path . --force
```

## First-time setup

```bash
drip init
```

This walks you through your Obsidian vault path and a few defaults (posts folder, daily notes folder, daily note date format, default sort, default limit), then optionally sets up a daily cron entry for unattended fetches (see "Running unattended" below).

## Usage

Topics are a **two-level tree**: main topics and their sub-topics. A source always links into a **leaf sub-topic**, never a main topic directly, so a leaf sub-topic must already exist (`drip topic add --name <main>` then `drip topic add --name <sub> --parent <main>`) before any of the `drip source add` examples below — see "Topics: a two-level tree with keyword rules" for the full picture, including how one source can link into more than one sub-topic with its own keyword rules per link.

### Reddit subreddits

Create a main topic and a leaf sub-topic under it, register a subreddit as a source linked into that sub-topic, then fetch it by label. This uses Reddit's own public RSS/Atom feed for the subreddit — no API key, app registration, or credentials of any kind needed:

```bash
drip topic add --name rust
drip topic add --name "rust news" --parent rust

drip source add --kind reddit --url rust --name rust-hot --topic "rust news"
drip fetch --source rust-hot
```

`drip source add --topic` errors clearly rather than guessing if the name doesn't exist yet, or names a main topic instead of a leaf sub-topic:

```
no topic named '<name>'; create it first with `drip topic add --name <name>`
'<name>' is a main topic; sources link to sub-topics only -- create one with `drip topic add --name <sub-topic> --parent <name>`
```

Pick a sort, time window, or restrict to posts matching a search term — these are baked into the feed URL at `source add` time, not at fetch time:

```bash
drip source add --kind reddit --url ObsidianMD --search tasks --name obsidian-tasks --topic "rust news"
drip source add --kind reddit --url rust --sort top --time week --name rust-weekly-top --topic "rust news"
```

`--search` is a free-text Reddit search within the subreddit, not a flair filter — flair isn't available through this feed. Since this goes through Reddit's public feed rather than a JSON API, these sources have no post score, comment count, or flair to filter on.

**Search-scoped sources are a retrieval layer, keyword rules are a routing layer — keep both, don't collapse one into the other.** Measured against a live feed at full depth (`hot/.rss?limit=100`): 88% of a search-scoped source's results (22 of 25) were absent from the same subreddit's broad `hot` feed at its maximum depth. Keyword rules can only route what retrieval already fetched, so folding a search-scoped source like `obsidian-tasks` into one broadly-fetched `r/ObsidianMD` source and relying on a `--match tasks` rule instead would permanently lose that 88% — no rule, however well written, can recover content a broad fetch never retrieved. Register a search-scoped source per topic you actually want reach into, and use `drip source link --match`/`--exclude` on top of it only to refine what that source already retrieves.

**Why native TLS:** Reddit's edge appears to fingerprint TLS clients — `reqwest`'s default `rustls` backend got a hard `403 Forbidden` fetching these feeds even from a fresh rate-limit window, while `curl` from the same machine succeeded. `drip` links against the system's native TLS library instead (see Prerequisites), which resolved it. If you ever see unexplained `403`s here (as opposed to ordinary `429` rate-limiting, which just needs a short wait), that's the symptom to look for.

### RSS feeds and YouTube channels

Register an RSS or Atom feed under a label, linked into a leaf sub-topic, then fetch it by that label — on its own, or alongside other sources in one combined digest:

```bash
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog --topic "rust news"
drip fetch --source rust-blog
drip fetch --source rust-hot,rust-blog --dry-run
```

YouTube channels work the same way — `drip` fetches a channel's own Atom feed, so no YouTube API key is needed. Pass either the channel id (starts with `UC`) or its `https://www.youtube.com/channel/UC.../` URL — handle URLs like `/@name` aren't supported, since resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (e.g. via the channel's About page, or by viewing page source for `"channelId":"UC...`):

```bash
drip source add --kind youtube --url UC_x5XG1OV2P6uZZ5FSM9Ttw --name gfd --topic "rust news"
drip fetch --source gfd
```

`--source` accepts a comma-separated list (repeat the flag or comma-separate) to combine any mix of registered sources — Reddit, RSS, YouTube — into one digest.

### Topics: a two-level tree with keyword rules

Sources no longer belong to exactly one topic. Instead, `drip source add --topic <sub-topic>` creates one **link** into a leaf sub-topic, and `drip source link`/`drip source unlink` add or remove further links from there — a source can feed several sub-topics at once, each with its own keyword rules deciding which of that source's items land there.

Here's a worked example: one noisy, broadly-scoped subreddit (`r/ClaudeCode`) split into two sub-topics of one main topic, using keyword rules to route:

```bash
# Two-level tree: one main topic, two leaf sub-topics under it
drip topic add --name "AI engineering"
drip topic add --name "claude code hooks" --parent "AI engineering"
drip topic add --name "claude code skills" --parent "AI engineering"

# Register the source once, linked into the first sub-topic with a keyword
# rule -- `source add --topic` creates exactly one link
drip source add --kind reddit --url ClaudeCode --name cc-feed --topic "claude code hooks"
drip source link --name cc-feed --topic "claude code hooks" --match "hook" --match-body

# Link the SAME source into the second sub-topic too, with its own rule --
# one source, two links, two independent rule sets
drip source link --name cc-feed --topic "claude code skills" --match "skill,SKILL.md" --match-body

# Sanity-check the rules offline, no network, before spending a real fetch
drip topic test --title "Spent months ignoring Claude Code hooks"
drip topic test --title "Writing your first SKILL.md file"

# Fetch the whole main topic in one go -- expands to every leaf sub-topic
# beneath it, classifying the one fetch of cc-feed into both as they match
drip fetch --topic "AI engineering"
```

An item matching both sub-topics' rules (e.g. a post about a skill that wraps a hook) renders under **both** `### claude code hooks` and `### claude code skills` — there's no precedence between links, this is treated as useful signal that the two rule sets overlap, not a bug. An item matching neither is silently dropped (not written anywhere) rather than falling back to a catch-all section — `-v` lists each dropped item's title so you can see whether your rules need widening.

`--topic` (on both `drip fetch` and `drip source add`/`link`) accepts a comma-separated list (repeat the flag or comma-separate) the same as `--source`, and both can be combined in one `drip fetch`:

```bash
drip fetch --source rust-weekly-top --topic "AI engineering"
```

Manage the tree itself with `drip topic add|rename|reparent|remove|list|test`:

```bash
drip topic rename --name "claude code hooks" --to "hooks & workflow"   # future-notes-only; warns if today's note already has the old heading
drip topic reparent --name "claude code skills" --parent "developer tools"  # move a sub-topic under a different main
drip topic list                                                        # see the two-level tree and each sub-topic's linked sources
```

`drip topic remove --name <name>` refuses while the topic still has any descendant — a main topic while it still has sub-topics, a sub-topic while it still has direct source links:

```
topic 'AI engineering' still has 2 sub-topic(s); remove them first
topic 'claude code hooks' still has 1 source(s); unlink them first (e.g. `drip source unlink --name <label> --topic <name>`) before removing it
```

An empty topic can always be removed; removing a topic never deletes the sources that were linked to it. There's no wholesale "move a source to a different topic" command any more (`drip source move` was removed) — since a source can have several links, reassigning it is `drip source unlink` the old sub-topic plus `drip source link` the new one.

### Managing sources

List, link, unlink, or remove saved sources — `drip source list` shows each source's source-level excludes (if any) and every sub-topic link it has, with that link's rules:

```bash
drip source list
# - cc-feed (kind: reddit, url: ClaudeCode)
#     -> claude code hooks (AI engineering): match=hook match-body
#     -> claude code skills (AI engineering): match=skill,SKILL.md match-body

drip source link --name cc-feed --topic "claude code hooks" --exclude "megathread"  # REPLACES that link's rules wholesale
drip source unlink --name cc-feed --topic "claude code skills"                       # drop just that one link
drip source remove --name rust-blog                                                  # remove the source entirely (cascades its links)
```

`--exclude` on `drip source add` sets a **source-level**, title-only pre-filter that runs before any sub-topic's rules at all — useful for noise (a recurring megathread, pricing chatter) that should never reach any sub-topic this source feeds, regardless of link-level rules.

### Fetching every saved source

`--all` fetches every saved source (see `drip source list`) into one combined digest, without needing to enumerate `--source`/`--topic`:

```bash
drip fetch --all
```

It merges/dedups with any `--source`/`--topic` also given in the same invocation — a source selected more than one way is still fetched exactly once. `--all` classifies each source against every sub-topic it's linked into (not scoped down to one `--topic`'s rules, as a `--topic`-only fetch is), so it never needs to iterate topics itself. With no sources saved at all, it prints a clear message and does nothing. This makes it a good fit for a stable unattended cron/systemd command that shouldn't need updating every time a new source is registered.

### Fetch options

Tag the resulting note and preview without writing anything to the vault or journal:

```bash
drip fetch --source rust-hot --tag rust --dry-run
```

Add `-v`/`--verbose` to see diagnostic output (request URLs, rate-limit waits, the loaded config and parsed args):

```bash
drip fetch --source rust-hot --dry-run -v
```

Note: `--sort`/`--time`/`-q`/`--query` on `drip fetch` only label the digest note's own frontmatter and header — they don't filter or search what actually gets fetched. For Reddit sources, control sort/time window/search at `drip source add --kind reddit` time (see above).

`-n`/`--limit` (default: the saved `default_limit` setting) caps how many items are **WRITTEN** per source, applied AFTER dedup and keyword-rule classification, not before — the per-source pipeline is `fetch → dedup → classify → truncate`, so this is "at most N items routed from this source," never "take the first N raw fetched items and see what routes." Truncating the raw feed first can leave zero routable items if a source's noisiest posts happen to sort first — at the default limit against a real noisy feed this measured as a difference between 0 routed items and 6. `drip fetch --source rust-hot,rust-blog -n 5` can still write up to 10 distinct items total (5 from each source, not 5 combined) — and because a single item can multi-match into two sub-topics, one source can still render as more than 5 checkbox lines.

Note: `drip fetch` remembers what it's already written to a digest, per source — an item that appeared in an *earlier* written digest won't be included again (an item is recorded as seen only when it's actually written to a digest, never on a `--dry-run`, and never for an item dropped by zero-match or by the source-level exclude pre-filter), so each item shows up in exactly one digest. If a fetch turns up nothing new, `drip` says so and leaves the day's note unchanged.

When a fetch includes multiple Reddit sources, `drip` paces the requests to dodge Reddit's per-IP (global) HTTP 429 rate-limiting: it spaces reddit requests `reddit_request_delay_secs` apart (default `10`, widening after each 429 it sees), retries a rate-limited request up to `reddit_retry_max` times (default `4`, honoring a `Retry-After` header then falling back to exponential backoff with base `reddit_retry_base_secs`, default `5`), and runs one **final retry pass** over any source still limited after a short cooldown. Anything still limited after that is skipped for the run and picked up next time (dedup avoids duplicates). RSS/YouTube feeds are never throttled. Tune the pacing without a rebuild:

```bash
drip config set reddit_request_delay_secs 15   # more space between reddit requests
drip config set reddit_retry_max 5
drip config set reddit_retry_base_secs 6
```

View or edit the config file directly:

```bash
drip config show
drip config edit
```

## Digest format

Each fetch writes one markdown note into your vault's posts folder (`Resources/drip` by default, the `posts_folder` setting), named `<YYYY-MM-DD> - Daily digest.md` (local ISO date only — no time, no topic/source label), grouped **main topic → sub-topic → item** — items are keyword-classified into sub-topics, not grouped by which source fetched them (a single source can feed several sub-topics at once). Because the name carries no time, it's one note per calendar day, and a second fetch the same day **appends/merges** into that day's existing note rather than overwriting it: genuinely-new items are inserted under the right `## <main topic>`/`### <sub-topic>` headings, while items already in the note — including any you've ticked (`- [x]`) or lines you've hand-edited — are left untouched. A run that turns up nothing new leaves the note byte-for-byte unchanged. Dedup suppresses an item only once it has appeared in an *earlier* written digest, so each item shows up in exactly one digest (the first day it appears).

- **Frontmatter:** `tags:` (only your `--tag`/`default_tags` tags, e.g. `drip` — renders as `tags: []` when empty), `createdOn`, `modifiedOn`, `topics: [...]` (every distinct **main** topic referenced by this run, in first-seen order), `subtopics: [...]` (every distinct sub-topic referenced, in first-seen order), `sources: [...]` (every source that fetched successfully this run, deduped — including one that contributed zero items, since this list means "what this run looked at," not "where these items came from"), `sort`, `time_filter`, `query`, `fetched_count`.
- **Body:** an `# <YYYY-MM-DD> - Daily digest` heading, then a `**Sources:** ... · **Sort:** ... · **Query:** ...` summary line (bare source labels, no `r/` prefix — a Reddit source's label isn't necessarily its real subreddit name, e.g. a search-scoped label like `cc-hooks`), then for each main topic an `## <main topic>` heading; under it, each sub-topic with at least one routed item gets its own `### <sub-topic>` heading (a sub-topic with zero routed items this run is simply omitted); under that, each item is a single, title-only Obsidian checkbox task:

  ```markdown
  - [ ] **[Async traits stabilized](https://example.com/post)**
  ```

  (a leading `⚠️ NSFW ` marker on NSFW Reddit posts.) **No source heading, no author suffix**, and no score, comment count, flair, or summary excerpt — and no LLM-generated summary — by design. An item matching two sub-topics' keyword rules renders once under each: classification has no precedence, and a duplicate line is treated as useful signal that two rule sets overlap, not a bug to dedupe away.

`fetched_count` in the frontmatter counts rendered checkbox **lines**, not distinct items — it can legitimately diverge from the journal bullet's "N posts" count (which counts distinct items), since an item that multi-matches into two sub-topics adds 2 to `fetched_count` but only 1 to the post count. This is expected under keyword-rule overlap, not a bug.

The checkbox format is deliberate: these are plain Obsidian tasks, which this setup surfaces elsewhere (an Obsidian Base, and the Taskforge iOS app) so you can tick an item off once you've clipped it into somewhere permanent, or just mark it "simple done" if it turned out not to be interesting — independently of `drip` itself, which only ever writes the note once and never touches it again.

## Running unattended (cron / systemd timer)

`drip fetch` has no interactive prompts, so it's safe to run from cron or a systemd user timer.

### cron

`drip init` can set this up for you: its final step optionally installs a daily cron entry (asking which saved source labels to fetch, and what time to run), so you don't need to edit your crontab by hand. Re-running `drip init` and answering "y" again updates that entry in place rather than duplicating it. If you decline the prompt, or you're setting up a headless/non-interactive install where `drip init` itself isn't run interactively, fall back to editing your crontab manually:

```bash
# Daily digest at 8am
0 8 * * * /path/to/drip fetch --source rust-hot,rust-blog >> ~/.local/log/drip.log 2>&1
```

### systemd user timer

`~/.config/systemd/user/drip-fetch.service`:

```ini
[Unit]
Description=drip fetch

[Service]
Type=oneshot
ExecStart=/path/to/drip fetch --source rust-hot,rust-blog
```

`~/.config/systemd/user/drip-fetch.timer`:

```ini
[Unit]
Description=Run drip fetch daily

[Timer]
OnCalendar=*-*-* 08:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

Enable with:

```bash
systemctl --user enable --now drip-fetch.timer
```

## Using with Claude Code

This repo ships a Claude Code skill at `.claude/skills/drip/SKILL.md` that teaches Claude Code the full `drip` command surface — subcommands, flags, and gotchas like `--sort`/`--time`/`-q` on `drip fetch` being cosmetic-only — so an agent can operate `drip` correctly on request. Claude Code picks it up automatically for repos it's working in, no setup needed.
