# drip

`drip` is a Rust CLI that fetches hot/trending Reddit posts and RSS/Atom feed entries from sources you choose, writes them as a "digest" markdown note into your Obsidian vault, and links that note from your daily journal note.

## Prerequisites

- **Rust**, via [rustup.rs](https://rustup.rs).
- **OpenSSL development headers** (e.g. `libssl-dev` + `pkg-config` on Debian/Ubuntu, `openssl-devel` on Fedora, or just Homebrew's OpenSSL on macOS) — `drip` links against the system's native TLS library rather than a bundled one (see "Why native TLS" below), so these need to be installed before `cargo build`/`cargo install` will succeed.
- **A Reddit "script" app** (optional — only needed for `-s/--subreddit`, the OAuth-based fetch path; skip this if you're only using `--kind reddit` sources, see below), to get an API client id and secret:
  1. Go to https://www.reddit.com/prefs/apps.
  2. Click "create app" (or "create another app").
  3. Choose type **script**.
  4. Give it any name, and any redirect URI (e.g. `http://localhost:8080`) — this app never does a user login flow, so the redirect URI is never actually used.
  5. After creating it, note the client id (the string under the app name) and the client secret.

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

### From source

```bash
git pull
cargo install --path . --force
```

## First-time setup

```bash
drip init
```

This walks you through entering your Reddit client id/secret, your Obsidian vault path, and a few defaults (posts folder, daily notes folder, daily note date format, default sort, default limit).

Credentials are saved to the OS keyring first. If no keyring backend is available (e.g. a headless Linux box with no Secret Service/keychain provider), `drip init` will tell you so, and you can instead export `DRIP_REDDIT_CLIENT_ID` / `DRIP_REDDIT_CLIENT_SECRET` as environment variables — `drip` checks the keyring first, then falls back to these two variables.

## Usage

Fetch hot posts from a subreddit:

```bash
drip fetch -s rust
```

Fetch from multiple subreddits (repeat the flag or comma-separate), with a specific sort and time window:

```bash
drip fetch -s rust,programming --sort top --time week
```

Search within a subreddit, tag the resulting note, and cap the post count:

```bash
drip fetch -s rust -q "async" -n 5 --tag async --tag rust
```

Preview without writing anything to the vault or journal:

```bash
drip fetch -s rust --dry-run
```

Filter out low-scoring or NSFW posts:

```bash
drip fetch -s aww --min-score 100 --no-nsfw
```

NSFW content is included by default; pass `--no-nsfw` to exclude it. `--min-score`/`--no-nsfw` only have an effect on Reddit posts fetched via the OAuth API today — RSS/YouTube items, and subreddits registered as `--kind reddit` sources (see "Subreddits without Reddit API credentials" below), have no score or NSFW concept, so they always pass both filters.

Note: `drip fetch` remembers what it's already shown you, per subreddit — a post that appeared in a previous digest won't be included again, even if it's still hot. (A post filtered out by `--min-score` or `--no-nsfw` is the one exception: it's never marked as seen, so it can still show up later if its score rises above the threshold or you drop `--no-nsfw`.) If a fetch turns up nothing new, `drip` says so and skips writing a digest note.

Add `-v`/`--verbose` to any `fetch` to see diagnostic output (Reddit request URLs, token requests, rate-limit waits, the loaded config and parsed args):

```bash
drip fetch -s rust --dry-run -v
```

Save a set of flags as a named profile, then fetch with it — `--profile` loads that profile's subreddits/sort/time/query/limit/tags for you:

```bash
drip profile add --name weekly-rust -s rust,programming --sort top --time week --tag rust
drip fetch --profile weekly-rust
```

Note: if you pass `--profile` together with `-s/--subreddit`, the profile name is used only as a label for the digest note/filename — your explicit flags win and the profile's saved flags are not applied. This is intentional: use `--profile` alone to load a saved profile's flags, or pass flags directly (optionally alongside `--profile` just for labeling).

List or remove saved profiles:

```bash
drip profile list
drip profile remove --name weekly-rust
```

### RSS feeds and YouTube channels

Register an RSS or Atom feed under a label, then fetch it by that label — on its own, or alongside a subreddit in one combined digest:

```bash
drip source add --kind rss --url https://blog.rust-lang.org/feed.xml --name rust-blog
drip fetch --source rust-blog
drip fetch -s rust --source rust-blog --dry-run
```

YouTube channels work the same way — `drip` fetches a channel's own Atom feed, so no YouTube API key is needed. Pass either the channel id (starts with `UC`) or its `https://www.youtube.com/channel/UC.../` URL — handle URLs like `/@name` aren't supported, since resolving those to a channel id needs an extra request; find the canonical channel id/URL instead (e.g. via the channel's About page, or by viewing page source for `"channelId":"UC...`):

```bash
drip source add --kind youtube --url UC_x5XG1OV2P6uZZ5FSM9Ttw --name gfd
drip fetch --source gfd
```

`--source` accepts a comma-separated list, same as `-s/--subreddit`. RSS/YouTube items don't have a Reddit-style score or NSFW flag, so `--min-score`/`--no-nsfw` never filter them out. A `--source`-only fetch (no `-s`/`--profile`) never needs Reddit credentials configured.

### Subreddits without Reddit API credentials

If you don't have (or can't get) Reddit OAuth credentials, you can register a subreddit as a source instead, using Reddit's own public RSS feed for it — no API key needed:

```bash
drip source add --kind reddit --url rust --name rust-hot
drip fetch --source rust-hot
```

Pick a sort, time window, or restrict to posts matching a search term:

```bash
drip source add --kind reddit --url ObsidianMD --search tasks --name obsidian-tasks
drip source add --kind reddit --url rust --sort top --time week --name rust-weekly-top
```

`--search` is a free-text Reddit search within the subreddit, not a flair filter — flair isn't available through this feed. Since this goes through Reddit's public feed rather than its JSON API, these sources have no post score/comment count/flair, and `--min-score`/`--no-nsfw` never filter them (same limitation as RSS/YouTube sources).

**Why native TLS:** Reddit's edge appears to fingerprint TLS clients — `reqwest`'s default `rustls` backend got a hard `403 Forbidden` fetching these feeds even from a fresh rate-limit window, while `curl` from the same machine succeeded. `drip` links against the system's native TLS library instead (see Prerequisites), which resolved it. If you ever see unexplained `403`s here (as opposed to ordinary `429` rate-limiting, which just needs a short wait), that's the symptom to look for.

List or remove saved sources:

```bash
drip source list
drip source remove --name rust-blog
```

View or edit the config file directly:

```bash
drip config show
drip config edit
```

## Running unattended (cron / systemd timer)

`drip fetch` has no interactive prompts once credentials are configured (via `drip init`'s OS keyring, or the `DRIP_REDDIT_CLIENT_ID`/`DRIP_REDDIT_CLIENT_SECRET` environment variables), so it's safe to run from cron or a systemd user timer.

### cron

`drip init` can set this up for you: its final step optionally installs a daily cron entry (asking what to fetch — a saved profile, or subreddits/source labels directly — and what time to run), so you don't need to edit your crontab by hand. Re-running `drip init` and answering "y" again updates that entry in place rather than duplicating it. If you decline the prompt, or you're setting up a headless/non-interactive install where `drip init` itself isn't run interactively, fall back to editing your crontab manually:

```bash
# Daily digest at 8am, using a saved profile
0 8 * * * /path/to/drip fetch --profile weekly-rust >> ~/.local/log/drip.log 2>&1
```

If your Reddit credentials aren't in the OS keyring (e.g. a headless server with no Secret Service/keychain provider), set them via cron's own environment instead:

```bash
0 8 * * * DRIP_REDDIT_CLIENT_ID=... DRIP_REDDIT_CLIENT_SECRET=... /path/to/drip fetch --profile weekly-rust >> ~/.local/log/drip.log 2>&1
```

### systemd user timer

`~/.config/systemd/user/drip-fetch.service`:

```ini
[Unit]
Description=drip fetch (weekly-rust profile)

[Service]
Type=oneshot
ExecStart=/path/to/drip fetch --profile weekly-rust
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
