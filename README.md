# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts and RSS/Atom feed entries from sources you choose, groups them by topic, writes them as a "digest" markdown note into your Obsidian vault, and links that note from your daily journal note.

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

Every source belongs to exactly one topic, so a topic named `rust` is assumed to already exist (`drip topic add --name rust`) before any of the `drip source add` examples below — see "Topics: grouping sources" for the full picture.

### Reddit subreddits

Register a subreddit as a source, then fetch it by label. This uses Reddit's own public RSS/Atom feed for the subreddit — no API key, app registration, or credentials of any kind needed:

```bash
drip source add --kind reddit --url rust --name rust-hot --topic rust
drip fetch --source rust-hot
```

Pick a sort, time window, or restrict to posts matching a search term — these are baked into the feed URL at `source add` time, not at fetch time:

```bash
drip source add --kind reddit --url ObsidianMD --search tasks --name obsidian-tasks --topic rust
drip source add --kind reddit --url rust --sort top --time week --name rust-weekly-top --topic rust
```

`--search` is a free-text Reddit search within the subreddit, not a flair filter — flair isn't available through this feed. Since this goes through Reddit's public feed rather than a JSON API, these sources have no post score, comment count, or flair to filter on.

**Why native TLS:** Reddit's edge appears to fingerprint TLS clients — `reqwest`'s default `rustls` backend got a hard `403 Forbidden` fetching these feeds even from a fresh rate-limit window, while `curl` from the same machine succeeded. `drip` links against the system's native TLS library instead (see Prerequisites), which resolved it. If you ever see unexplained `403`s here (as opposed to ordinary `429` rate-limiting, which just needs a short wait), that's the symptom to look for.

### RSS feeds and YouTube channels

Register an RSS or Atom feed under a label, then fetch it by that label — on its own, or alongside other sources in one combined digest:

```bash
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog --topic rust
drip fetch --source rust-blog
drip fetch --source rust-hot,rust-blog --dry-run
```

YouTube channels work the same way — `drip` fetches a channel's own Atom feed, so no YouTube API key is needed. Pass either the channel id (starts with `UC`) or its `https://www.youtube.com/channel/UC.../` URL — handle URLs like `/@name` aren't supported, since resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (e.g. via the channel's About page, or by viewing page source for `"channelId":"UC...`):

```bash
drip source add --kind youtube --url UC_x5XG1OV2P6uZZ5FSM9Ttw --name gfd --topic rust
drip fetch --source gfd
```

`--source` accepts a comma-separated list (repeat the flag or comma-separate) to combine any mix of registered sources — Reddit, RSS, YouTube — into one digest.

### Topics: grouping sources

**Every source belongs to exactly one topic.** `drip source add` requires an existing `--topic` and does not auto-create one — create the topic first (as in "Usage" above), then register sources into it, then fetch the whole group with one `--topic` name instead of typing every member's `--source` label each time:

```bash
drip topic add --name rust
drip source add --kind reddit --url rust --name rust-hot --topic rust
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog --topic rust

drip fetch --topic rust
```

`--topic` accepts a comma-separated list (repeat the flag or comma-separate), the same as `--source`, and both can be combined in one `drip fetch` — each named topic is resolved into its member sources' labels and merged with any `--source` labels given, with a source named by both fetched exactly once, not twice:

```bash
drip fetch --source rust-weekly-top --topic rust
```

When exactly one `--topic` is given and it resolves cleanly, the digest note's filename and header are labeled with the topic's name (e.g. `rust`) instead of joining every member source's label. With zero `--topic`s, or one that fails to resolve (e.g. a typo), it falls back to the existing joined-source-labels behavior; with more than one `--topic`, it labels the note with the topic names joined instead.

There's no separate "attach"/"detach" operation — since a source always has exactly one topic, reassigning it to a different (existing) topic is `drip source move`:

```bash
drip topic add --name programming
drip source move --name rust-blog --topic programming   # reassign it to another (existing) topic
drip topic list                                           # see saved topics and their members
```

`drip topic remove --name <name>` deletes a topic, but **refuses while it still owns any sources**, telling you to move them first:

```
topic 'rust' still has 1 source(s); move them to another topic first (e.g. `drip source move --name <label> --topic <other>`) before removing it
```

An empty topic can always be removed; removing a topic never deletes the sources that were in it (move them elsewhere first, then the topic they're left in can be removed).

### Managing sources

List or remove saved sources — `drip source list` now shows each source's topic:

```bash
drip source list
# - rust-hot (topic: rust, kind: reddit, url: rust)
# - rust-blog (topic: rust, kind: rss, url: https://blog.rust-lang.org/feed.xml)

drip source remove --name rust-blog
```

### Fetching every saved source

`--all` fetches every saved source (see `drip source list`) into one combined digest, without needing to enumerate `--source`/`--topic`:

```bash
drip fetch --all
```

It merges/dedups with any `--source`/`--topic` also given in the same invocation — a source selected more than one way is still fetched exactly once. Because a topic is just a named group of already-saved sources, `--all` inherently covers everything any topic references, so it never needs to iterate topics itself. With no sources saved at all, it prints a clear message and does nothing. This makes it a good fit for a stable unattended cron/systemd command that shouldn't need updating every time a new source is registered.

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

`-n`/`--limit` (default: the saved `default_limit` setting) does have a real effect: it caps how many items are taken from each source's fetched feed, per source, before dedup — `drip fetch --source rust-hot,rust-blog -n 5` can still write up to 10 items total (5 from each), not 5 combined.

Note: `drip fetch` remembers what it's already shown you, per source — an item that appeared in a previous digest won't be included again. If a fetch turns up nothing new, `drip` says so and skips writing a digest note.

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

Each fetch writes one markdown note into your vault's posts folder (`Resources/drip` by default, the `posts_folder` setting), grouped **topic → source → item**:

- **Frontmatter:** `tags:` (only your `--tag`/`default_tags` tags, e.g. `drip` — renders as `tags: []` when empty), `createdOn`, `modifiedOn`, `topics: [...]` (every distinct topic referenced by this run, in first-seen order), `sources: [...]` (the source labels fetched), `sort`, `time_filter`, `query`, `fetched_count`.
- **Body:** an `# drip digest — <local timestamp>` heading, then a `**Sources:** ... · **Sort:** ... · **Query:** ...` summary line, then for each topic an `## <topic>` heading; under it, each source gets its own `### r/<subreddit>` (Reddit) or `### <label>` (RSS/YouTube) heading; under that, each item is a single Obsidian checkbox task:

  ```markdown
  - [ ] **[Async traits stabilized](https://example.com/post)** — u/someone
  ```

  (`— <author>` without the `u/` prefix for RSS/YouTube items; a leading `⚠️ NSFW ` marker on NSFW Reddit posts.) There's no score, comment count, flair, or summary excerpt — and no LLM-generated summary — by design.

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
