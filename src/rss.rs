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

use anyhow::{bail, Context, Result};
use feed_rs::model::Entry;
use reqwest::blocking::Client;
use reqwest::header::USER_AGENT;

use crate::item::Item;
use crate::vprintln;

/// Fetch and parse the RSS/Atom feed at `url`, returning its entries mapped
/// to [`Item`]s in feed order.
///
/// Surfaces a clear error for a non-2xx HTTP response and for a response
/// body that isn't parseable as RSS/Atom (e.g. an HTML error page, or
/// unrelated content at that URL) -- neither case panics.
pub fn fetch(url: &str, verbose: bool) -> Result<Vec<Item>> {
    // Without an explicit timeout, a slow or hanging feed URL (arbitrary,
    // user-supplied via `drip source add --url <url>`) would block `drip
    // fetch` indefinitely. 30 seconds is generous for a feed fetch while
    // still bounding the worst case.
    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client for RSS/Atom fetch")?;

    vprintln(verbose, format!("GET {url}"));

    let resp = http
        .get(url)
        .header(USER_AGENT, "drip/0.1 (RSS reader)")
        .send()
        .with_context(|| format!("failed to fetch feed at {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("failed to fetch feed at {url}: HTTP {status}");
    }

    let bytes = resp
        .bytes()
        .with_context(|| format!("failed to read response body from {url}"))?;

    let feed = feed_rs::parser::parse(bytes.as_ref())
        .with_context(|| format!("failed to parse feed at {url} as RSS/Atom"))?;

    Ok(feed.entries.into_iter().map(entry_to_item).collect())
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
        let items = fetch(&url, false).expect("fetch should succeed against the mock server");

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
        let items = fetch(&url, false).expect("fetch should succeed against the mock server");

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
        let err = fetch(&url, false).expect_err("a 404 response should produce an error");

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
        let err = fetch(&url, false).expect_err("malformed feed content should produce an error");

        assert!(
            err.to_string().contains("parse"),
            "error should mention parsing failed: {err}"
        );
    }
}
