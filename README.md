# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts and RSS/Atom feed entries from sources you choose, writes them as a "digest" markdown note into your Obsidian vault, and links that note from your daily journal note.

## Prerequisites

- **Rust**, via [rustup.rs](https://rustup.rs).
- **OpenSSL development headers** (e.g. `libssl-dev` + `pkg-config` on Debian/Ubuntu, `openssl-devel` on Fedora, or just Homebrew's OpenSSL on macOS) — `drip` links against the system's native TLS library rather than a bundled one (see "Why native TLS" below), so these need to be installed before `cargo build`/`cargo install` will succeed.

No Reddit API credentials, app registration, or API key of any kind is needed for any source `drip` supports — see "Usage" below.

## Install

### From a release binary

Prebuilt binaries for Linux x86_64 (glibc) are attached to each [GitHub release](https://github.com/beingfrankly/drip/releases):

```bash
curl -LO https://github.com/beingfrankly/drip/releases/latest/download/drip-vX.Y.Z-x86_64-linux-gnu.tar.gz
tar -xzf drip-vX.Y.Z-x86_64-linux-gnu.tar.gz
sudo mv drip /usr/local/bin/
```

Replace `vX.Y.Z` with the version you want (check the releases page for the exact filename of the latest tag).

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

`drip update` replaces whichever binary is currently running (wherever it lives — `/usr/local/bin/drip`, `~/.cargo/bin/drip`, etc.) with the latest release from GitHub. It's Linux x86_64 only for now (that's the only platform released today).

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

### Reddit subreddits

Register a subreddit as a source, then fetch it by label. This uses Reddit's own public RSS/Atom feed for the subreddit — no API key, app registration, or credentials of any kind needed:

```bash
drip source add --kind reddit --url rust --name rust-hot
drip fetch --source rust-hot
```

Pick a sort, time window, or restrict to posts matching a search term — these are baked into the feed URL at `source add` time, not at fetch time:

```bash
drip source add --kind reddit --url ObsidianMD --search tasks --name obsidian-tasks
drip source add --kind reddit --url rust --sort top --time week --name rust-weekly-top
```

`--search` is a free-text Reddit search within the subreddit, not a flair filter — flair isn't available through this feed. Since this goes through Reddit's public feed rather than a JSON API, these sources have no post score, comment count, or flair to filter on.

**Why native TLS:** Reddit's edge appears to fingerprint TLS clients — `reqwest`'s default `rustls` backend got a hard `403 Forbidden` fetching these feeds even from a fresh rate-limit window, while `curl` from the same machine succeeded. `drip` links against the system's native TLS library instead (see Prerequisites), which resolved it. If you ever see unexplained `403`s here (as opposed to ordinary `429` rate-limiting, which just needs a short wait), that's the symptom to look for.

### RSS feeds and YouTube channels

Register an RSS or Atom feed under a label, then fetch it by that label — on its own, or alongside other sources in one combined digest:

```bash
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog
drip fetch --source rust-blog
drip fetch --source rust-hot,rust-blog --dry-run
```

YouTube channels work the same way — `drip` fetches a channel's own Atom feed, so no YouTube API key is needed. Pass either the channel id (starts with `UC`) or its `https://www.youtube.com/channel/UC.../` URL — handle URLs like `/@name` aren't supported, since resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (e.g. via the channel's About page, or by viewing page source for `"channelId":"UC...`):

```bash
drip source add --kind youtube --url UC_x5XG1OV2P6uZZ5FSM9Ttw --name gfd
drip fetch --source gfd
```

`--source` accepts a comma-separated list (repeat the flag or comma-separate) to combine any mix of registered sources — Reddit, RSS, YouTube — into one digest.

### Managing sources

List or remove saved sources:

```bash
drip source list
drip source remove --name rust-blog
```

### Topics: grouping sources

A topic is a named group of already-saved sources — fetch the whole group with one `--topic` name instead of typing every member's `--source` label each time:

```bash
drip topic add --name rust
drip topic add-source --topic rust --source rust-hot
drip topic add-source --topic rust --source rust-blog

drip fetch --topic rust
```

`--topic` accepts a comma-separated list (repeat the flag or comma-separate), the same as `--source`, and both can be combined in one `drip fetch` — each named topic is resolved into its member sources' labels and merged with any `--source` labels given, with a source named by both fetched exactly once, not twice:

```bash
drip fetch --source rust-weekly-top --topic rust
```

When exactly one `--topic` is given and it resolves cleanly, the digest note's filename and header are labeled with the topic's name (e.g. `rust`) instead of joining every member source's label. With zero `--topic`s, or one that fails to resolve (e.g. a typo), it falls back to the existing joined-source-labels behavior; with more than one `--topic`, it labels the note with the topic names joined instead.

```bash
drip topic remove-source --topic rust --source rust-blog  # detach one source
drip topic remove --name rust                              # delete the topic (its sources are unaffected)
drip topic list                                             # see saved topics and their members
```

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

View or edit the config file directly:

```bash
drip config show
drip config edit
```

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
