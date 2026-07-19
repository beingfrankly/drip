//! Minimal RSS/Atom fetch client: a plain HTTP GET plus a single
//! `feed_rs::parser::parse` call, mapped into the normalized [`Item`] shape
//! shared with Reddit (see `src/item.rs`).
//!
//! Design context: bd issue drip-15n.9.6. There's no token or session state
//! to cache here, so a fresh `reqwest::blocking::Client` per call is fine --
//! no client struct needed.
//! `feed-rs` was chosen (per the issue's own recommendation) because it
//! handles both RSS 2.0 and Atom via the same `parse` call, so this module
//! doesn't need to know or care which format a given feed uses.

use std::time::Duration;

use anyhow::anyhow;
use feed_rs::model::Entry;
use reqwest::blocking::Client;
use reqwest::header::USER_AGENT;

use crate::item::Item;
use crate::vprintln;

/// Hard cap on any single backoff wait (also clamps a large Retry-After).
const RETRY_MAX_DELAY: Duration = Duration::from_secs(120);

/// The outcome of a single [`fetch`] call. Reddit rate-limits per-IP
/// GLOBALLY across its `.rss` endpoints (bd issue drip-6xz, follow-up to
/// drip-hja), so a source that's still 429-ing after exhausting its own
/// inline retries needs to be distinguishable from any other failure --
/// `handle_fetch`'s coordinator (`src/main.rs`) uses that distinction to
/// widen spacing for the rest of the run and to queue the source for a
/// final retry pass, rather than dropping it for the run the way a genuine
/// error is.
#[derive(Debug)]
pub enum FetchOutcome {
    /// The feed was fetched and parsed successfully.
    Fetched(Vec<Item>),
    /// Still HTTP 429 after exhausting `max_retries` inline retries.
    RateLimited,
    /// Any other failure: transport error, a non-429 non-2xx status, or a
    /// response body that didn't parse as RSS/Atom.
    Failed(anyhow::Error),
}

/// Compute how long to wait before retrying after a 429. Honors an explicit
/// Retry-After (integer seconds) when present; otherwise exponential backoff
/// `base * 2^attempt` (attempt 0-indexed). Both are clamped to RETRY_MAX_DELAY.
/// No jitter: drip is a single sequential client, so thundering-herd jitter
/// buys nothing and would need a rand dependency.
fn retry_delay(attempt: u32, retry_after_secs: Option<u64>, base: Duration) -> Duration {
    match retry_after_secs {
        Some(secs) => Duration::from_secs(secs).min(RETRY_MAX_DELAY),
        None => {
            // Clamp the exponent so `2u32.pow(..)` can never overflow --
            // anything beyond a handful of doublings is already far past
            // RETRY_MAX_DELAY anyway.
            let exponent = attempt.min(16);
            base.saturating_mul(2u32.saturating_pow(exponent))
                .min(RETRY_MAX_DELAY)
        }
    }
}

/// Fetch and parse the RSS/Atom feed at `url`, returning a [`FetchOutcome`]
/// rather than a plain `Result` (bd issue drip-6xz) -- the caller
/// (`handle_fetch`'s coordinator in `src/main.rs`) needs to distinguish
/// "still rate-limited after retrying" from any other failure, since only
/// the former gets a final retry pass at the end of the run.
///
/// A 429 (rate-limited) response is retried up to `max_retries` times,
/// honoring a `Retry-After` header when the server sends one and falling
/// back to exponential backoff (base `base`) otherwise (bd issue drip-hja)
/// -- see [`retry_delay`] for the mechanics. `max_retries`/`base` come from
/// the caller's resolved `settings.reddit_retry_max`/
/// `reddit_retry_base_secs` (drip-6xz made these configurable rather than
/// fixed consts, since a single hardcoded policy proved too tight against
/// reddit's global per-IP limit).
///
/// A transport-level send error (DNS, connection refused, timeout, ...)
/// never retries -- unlike a 429, there's no reason to expect a second
/// attempt moments later to succeed.
pub fn fetch(url: &str, verbose: bool, max_retries: u32, base: Duration) -> FetchOutcome {
    // Without an explicit timeout, a slow or hanging feed URL (arbitrary,
    // user-supplied via `drip source add --url <url>`) would block `drip
    // fetch` indefinitely. 30 seconds is generous for a feed fetch while
    // still bounding the worst case.
    let http = match Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(http) => http,
        Err(err) => {
            return FetchOutcome::Failed(
                anyhow::Error::new(err)
                    .context("failed to build HTTP client for RSS/Atom fetch"),
            )
        }
    };

    for attempt in 0..=max_retries {
        vprintln(verbose, format!("GET {url}"));

        let resp = match http
            .get(url)
            .header(USER_AGENT, "drip/0.1 (RSS reader)")
            .send()
        {
            Ok(resp) => resp,
            Err(err) => {
                return FetchOutcome::Failed(
                    anyhow::Error::new(err).context(format!("failed to fetch feed at {url}")),
                )
            }
        };

        let status = resp.status();

        if status.as_u16() == 429 {
            if attempt < max_retries {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let delay = retry_delay(attempt, retry_after, base);
                vprintln(
                    verbose,
                    format!(
                        "{url}: HTTP 429; retrying in {}s (attempt {}/{})",
                        delay.as_secs(),
                        attempt + 1,
                        max_retries
                    ),
                );
                std::thread::sleep(delay);
                continue;
            }
            return FetchOutcome::RateLimited;
        }

        if !status.is_success() {
            return FetchOutcome::Failed(anyhow!("failed to fetch feed at {url}: HTTP {status}"));
        }

        let bytes = match resp.bytes() {
            Ok(bytes) => bytes,
            Err(err) => {
                return FetchOutcome::Failed(
                    anyhow::Error::new(err)
                        .context(format!("failed to read response body from {url}")),
                )
            }
        };

        return match feed_rs::parser::parse(bytes.as_ref()) {
            Ok(feed) => FetchOutcome::Fetched(feed.entries.into_iter().map(entry_to_item).collect()),
            Err(err) => FetchOutcome::Failed(
                anyhow::Error::new(err).context(format!("failed to parse feed at {url} as RSS/Atom")),
            ),
        };
    }

    // Unreachable: the loop above always either returns before falling off
    // the end (the `attempt == max_retries` 429 branch returns
    // `RateLimited`).
    unreachable!("fetch retry loop should always return")
}

/// Map a single `feed_rs::model::Entry` to our normalized [`Item`] shape.
/// See bd issue drip-15n.9.6's design note for the field mapping rationale
/// (in short: RSS/Atom entries have no separate comments page, and no
/// score/comment-count/flair/NSFW concept, so those fields are always
/// `None`/`false`).
fn entry_to_item(entry: Entry) -> Item {
    Item {
        id: entry.id,
        title: entry.title.map(|t| t.content).unwrap_or_default(),
        url: entry
            .links
            .first()
            .map(|l| l.href.clone())
            .unwrap_or_default(),
        comments_url: None,
        author: entry.authors.first().map(|p| p.name.clone()),
        published_at: entry.published.or(entry.updated),
        summary: entry.summary.map(|s| s.content),
        score: None,
        num_comments: None,
        flair: None,
        nsfw: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal RSS 2.0 fixture: one `<item>` with title/link/guid/
    /// pubDate/description populated.
    const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Example RSS Feed</title>
    <link>https://example.com/</link>
    <description>An example feed</description>
    <item>
      <title>First RSS post</title>
      <link>https://example.com/posts/first</link>
      <guid>https://example.com/posts/first</guid>
      <pubDate>Mon, 06 Jul 2026 12:00:00 GMT</pubDate>
      <description>A short summary of the first post.</description>
    </item>
  </channel>
</rss>"#;

    /// A minimal Atom fixture: one `<entry>` with title/link/id/updated/
    /// summary populated -- proves `feed-rs`'s format-agnostic handling
    /// works, not just RSS.
    const ATOM_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Atom Feed</title>
  <id>https://example.com/</id>
  <updated>2026-07-06T12:00:00Z</updated>
  <entry>
    <title>First Atom post</title>
    <link href="https://example.com/posts/atom-first"/>
    <id>https://example.com/posts/atom-first</id>
    <updated>2026-07-06T12:00:00Z</updated>
    <summary>A short summary of the first Atom post.</summary>
  </entry>
</feed>"#;

    #[test]
    fn fetch_parses_a_minimal_rss_2_0_fixture() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_header("content-type", "application/rss+xml")
            .with_body(RSS_FIXTURE)
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_millis(0));

        let items = match outcome {
            FetchOutcome::Fetched(items) => items,
            other => panic!("expected Fetched, got a different outcome: {other:?}"),
        };
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "https://example.com/posts/first");
        assert_eq!(item.title, "First RSS post");
        assert_eq!(item.url, "https://example.com/posts/first");
        assert_eq!(
            item.summary,
            Some("A short summary of the first post.".to_string())
        );
        assert!(item.published_at.is_some());
    }

    #[test]
    fn fetch_parses_a_minimal_atom_fixture() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/feed.atom")
            .with_status(200)
            .with_header("content-type", "application/atom+xml")
            .with_body(ATOM_FIXTURE)
            .create();

        let url = format!("{}/feed.atom", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_millis(0));

        let items = match outcome {
            FetchOutcome::Fetched(items) => items,
            other => panic!("expected Fetched, got a different outcome: {other:?}"),
        };
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "https://example.com/posts/atom-first");
        assert_eq!(item.title, "First Atom post");
        assert_eq!(item.url, "https://example.com/posts/atom-first");
        assert_eq!(
            item.summary,
            Some("A short summary of the first Atom post.".to_string())
        );
        assert!(item.published_at.is_some());
    }

    #[test]
    fn fetch_errors_clearly_on_non_2xx_status() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/feed.xml")
            .with_status(404)
            .with_body("not found")
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_millis(0));

        let err = match outcome {
            FetchOutcome::Failed(err) => err,
            other => panic!("expected Failed, got a different outcome: {other:?}"),
        };
        assert!(
            err.to_string().contains("404"),
            "error should mention the HTTP status: {err}"
        );
    }

    #[test]
    fn fetch_errors_clearly_on_malformed_non_feed_body() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body>this is not a feed</body></html>")
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_millis(0));

        let err = match outcome {
            FetchOutcome::Failed(err) => err,
            other => panic!("expected Failed, got a different outcome: {other:?}"),
        };
        assert!(
            err.to_string().contains("parse"),
            "error should mention parsing failed: {err}"
        );
    }

    #[test]
    fn retry_delay_honors_an_explicit_retry_after() {
        assert_eq!(
            retry_delay(0, Some(7), Duration::from_secs(3)),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn retry_delay_clamps_a_huge_retry_after_to_the_max_delay() {
        assert_eq!(
            retry_delay(0, Some(10_000), Duration::from_secs(3)),
            RETRY_MAX_DELAY
        );
    }

    #[test]
    fn retry_delay_backs_off_exponentially_with_no_retry_after_header() {
        let base = Duration::from_secs(2);
        assert_eq!(retry_delay(0, None, base), Duration::from_secs(2));
        assert_eq!(retry_delay(1, None, base), Duration::from_secs(4));
        assert_eq!(retry_delay(2, None, base), Duration::from_secs(8));
    }

    #[test]
    fn retry_delay_clamps_a_huge_attempt_without_panicking() {
        assert_eq!(
            retry_delay(u32::MAX, None, Duration::from_secs(3)),
            RETRY_MAX_DELAY
        );
    }

    #[test]
    fn fetch_succeeds_after_a_single_429() {
        let mut server = mockito::Server::new();
        let rate_limited = server
            .mock("GET", "/feed.xml")
            .with_status(429)
            .create();
        let ok = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_header("content-type", "application/rss+xml")
            .with_body(RSS_FIXTURE)
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_millis(0));

        let items = match outcome {
            FetchOutcome::Fetched(items) => items,
            other => panic!(
                "fetch should succeed after retrying past the 429, got: {other:?}"
            ),
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "First RSS post");
        rate_limited.assert();
        ok.assert();
    }

    #[test]
    fn fetch_reports_rate_limited_after_exhausting_retries_on_persistent_429() {
        let mut server = mockito::Server::new();
        let max_retries = 2;
        let rate_limited = server
            .mock("GET", "/feed.xml")
            .with_status(429)
            .expect(max_retries as usize + 1)
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, max_retries, Duration::from_millis(0));

        assert!(
            matches!(outcome, FetchOutcome::RateLimited),
            "persistent 429s should eventually report RateLimited, got: {outcome:?}"
        );
        rate_limited.assert();
    }

    #[test]
    fn fetch_honors_a_zero_second_retry_after() {
        let mut server = mockito::Server::new();
        let rate_limited = server
            .mock("GET", "/feed.xml")
            .with_status(429)
            .with_header("retry-after", "0")
            .create();
        let ok = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_header("content-type", "application/rss+xml")
            .with_body(RSS_FIXTURE)
            .create();

        let url = format!("{}/feed.xml", server.url());
        let outcome = fetch(&url, false, 3, Duration::from_secs(60));

        let items = match outcome {
            FetchOutcome::Fetched(items) => items,
            other => panic!(
                "a Retry-After: 0 header should be honored instead of the exponential base, got: {other:?}"
            ),
        };
        assert_eq!(items.len(), 1);
        rate_limited.assert();
        ok.assert();
    }
}
