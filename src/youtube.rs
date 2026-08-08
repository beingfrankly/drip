//! Resolves whatever a user passes to `drip source add --kind youtube --url
//! <input>` into a YouTube channel's Atom feed URL.
//!
//! Design context: bd issue drip-15n.9.7. YouTube exposes a per-channel Atom
//! feed at `https://www.youtube.com/feeds/videos.xml?channel_id={id}`,
//! confirmed live (returns HTTP 200 with real, parseable entries) as of this
//! issue's investigation. Because that feed is a perfectly standard Atom
//! feed, fetching it needs no YouTube-specific client, no YouTube Data API,
//! and no OAuth/API key -- once [`channel_feed_url`] below has produced the
//! right URL, fetching is delegated entirely to [`crate::rss::fetch`], the
//! exact same function RSS sources use, since a channel's Atom feed is
//! indistinguishable in format from any other Atom feed.
//!
//! For a bare channel id, an already-built `feeds/videos.xml` URL, or a
//! `/channel/UC.../` URL, resolution is pure string logic -- no network.
//!
//! A `/@handle` URL (what YouTube shows in the address bar today, and the
//! form that motivated this whole module, bd issue drip-ho5.11) has no
//! channel id anywhere in the URL itself -- resolving one requires fetching
//! the handle's channel page and scraping its `channelId` out of the
//! markup. That's the one place this module does network I/O, and it's kept
//! to a thin wrapper ([`fetch_channel_id_from_handle_page`]) mirroring
//! `src/rss.rs`'s fetch-vs-parse split: the actual scraping is a separate
//! PURE function ([`channel_id_from_handle_page`]), unit-tested against a
//! fixture built from a real fetched page, while the network wrapper is
//! tested with `mockito` exactly as `src/rss.rs` does. `/c/{name}` and
//! `/user/{name}` custom-URL forms remain unsupported (out of scope here --
//! unlike a handle page, they don't reliably carry a channel id in their own
//! markup); callers with one get a clear error pointing them at the
//! channel's canonical `channel_id` instead.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::USER_AGENT;

/// Turn `input` (whatever the user passed to `drip source add --kind youtube
/// --url <input>`) into the canonical feed URL
/// `https://www.youtube.com/feeds/videos.xml?channel_id={id}`.
///
/// Accepts, in order of precedence:
/// 1. An already-constructed `feeds/videos.xml` URL (a power-user escape
///    hatch) -- returned verbatim (after trimming). No network.
/// 2. A `https://www.youtube.com/channel/{id}` URL, with or without
///    `http://`, with or without a `www.` prefix, with or without a trailing
///    slash or extra path segments after `{id}`. No network.
/// 3. A bare channel id -- starts with `"UC"` and is at least 10 characters.
///    No network.
/// 4. A handle -- a bare `@handle`, or a `youtube.com/@handle` URL, with or
///    without a scheme/`www.`/trailing path (bd issue drip-ho5.11, the exact
///    URL form YouTube shows in its own address bar today). Resolving this
///    one fetches the handle's channel page over the network and scrapes its
///    channel id out of the markup -- see the module doc comment for the
///    pure/network split.
///
/// Resolution happens here, at `drip source add` time, not at fetch time --
/// the stored `identifier` this feeds into is always the resolved feed URL,
/// so a saved source's actual fetches stay a plain unauthenticated GET with
/// no extra request per run.
///
/// Errors clearly on:
/// - A handle that fails to resolve (network error, handle not found, or no
///   channel id found on the fetched page) -- the error says resolution
///   failed and points at passing the `UC...` channel id directly instead.
/// - A `/c/{name}` or `/user/{name}` custom-URL form -- unlike a handle page,
///   these don't reliably carry a channel id in their own markup, so
///   they're out of scope here.
/// - Anything else that doesn't look like a channel id, `/channel/UC.../`
///   URL, or handle.
pub fn channel_feed_url(input: &str) -> Result<String> {
    let trimmed = input.trim();

    if trimmed.contains("feeds/videos.xml") {
        return Ok(trimmed.to_string());
    }

    if let Some(id) = extract_channel_id_from_url(trimmed) {
        return Ok(build_feed_url(&id));
    }

    if looks_like_bare_channel_id(trimmed) {
        return Ok(build_feed_url(trimmed));
    }

    if let Some(handle) = extract_handle(trimmed) {
        let page_url = handle_page_url(&handle);
        let channel_id = fetch_channel_id_from_handle_page(&page_url).with_context(|| {
            format!(
                "could not resolve YouTube handle '@{handle}' to a channel id; you can pass the \
                 channel's UC... channel id directly instead (find it via the channel's About \
                 page, or page source for `\"channelId\":\"UC...`)"
            )
        })?;
        return Ok(build_feed_url(&channel_id));
    }

    if trimmed.contains("youtube.com/c/") || trimmed.contains("youtube.com/user/") {
        bail!(
            "'{trimmed}' is a YouTube custom-URL link (/c/ or /user/), which can't be resolved \
             to a channel id without an extra HTTP request (out of scope for `drip source \
             add`). Find the channel's canonical channel id instead -- it starts with \"UC\" -- \
             by opening the channel and viewing page source for `\"channelId\":\"UC...`, or by \
             using the channel's own https://www.youtube.com/channel/UC.../ URL if you can find \
             one (many channels link this from their About page), and pass that instead."
        );
    }

    bail!(
        "'{trimmed}' doesn't look like a YouTube channel id (starts with \"UC\"), a \
         https://www.youtube.com/channel/UC.../ URL, or a @handle"
    );
}

/// Fetch `url` (a YouTube handle page, e.g.
/// `https://www.youtube.com/@mattpocockuk`) and extract its channel id via
/// [`channel_id_from_handle_page`]. Thin network wrapper mirroring
/// `src/rss.rs`'s `fetch` -- this function's only job is the HTTP GET; all
/// real logic lives in the pure parse function, which is what's
/// unit-tested against a fixture. This is what makes this function itself
/// testable with `mockito` the same way `src/rss.rs`'s tests are: `url` is
/// whatever the caller passes, so a test can point it at a mock server
/// instead of the real `youtube.com`.
fn fetch_channel_id_from_handle_page(url: &str) -> Result<String> {
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client for YouTube handle resolution")?;

    let resp = http
        .get(url)
        .header(USER_AGENT, "drip/0.1 (YouTube handle resolver)")
        .send()
        .with_context(|| format!("failed to fetch YouTube handle page at {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("failed to fetch YouTube handle page at {url}: HTTP {status}");
    }

    let body = resp
        .text()
        .with_context(|| format!("failed to read response body from {url}"))?;

    channel_id_from_handle_page(&body)
}

/// Extract the channel id from a fetched YouTube handle page's HTML body
/// (e.g. the response body of `GET https://www.youtube.com/@handle`). PURE,
/// no network -- the network fetch itself is [`fetch_channel_id_from_handle_page`]
/// below, which calls this. Confirmed live (bd issue drip-ho5.11) against
/// `https://www.youtube.com/@mattpocockuk`.
///
/// Looks first for the page's canonical link (`<link rel="canonical"
/// href="https://www.youtube.com/channel/UC.../">`), falling back to its RSS
/// autodiscovery link (`<link rel="alternate" type="application/rss+xml"
/// ... href="...?channel_id=UC...">`) if the canonical link is missing or
/// doesn't carry a `/channel/UC.../` href. Errors clearly if neither is
/// present -- e.g. YouTube changed its page markup, or the fetched body
/// wasn't really a channel page at all.
pub fn channel_id_from_handle_page(html: &str) -> Result<String> {
    if let Some(href) = extract_link_href(html, r#"rel="canonical""#) {
        if let Some(id) = extract_channel_id_from_url(&href) {
            return Ok(id);
        }
    }

    if let Some(href) = extract_link_href(html, r#"type="application/rss+xml""#) {
        if let Some(id) = extract_channel_id_query_param(&href) {
            return Ok(id);
        }
    }

    bail!(
        "the fetched YouTube page didn't contain a recognizable channel id (no canonical link \
         or RSS autodiscovery link with a channel id) -- YouTube's page markup may have changed"
    );
}

/// Find the `<link ...>` tag containing `attr_marker` (e.g. `r#"rel="canonical""#`)
/// and return its `href` attribute value, if present. `html` is searched for
/// the nearest enclosing `<link` .. `>` span around `attr_marker`'s first
/// occurrence, rather than assuming any particular attribute order.
fn extract_link_href(html: &str, attr_marker: &str) -> Option<String> {
    let marker_idx = html.find(attr_marker)?;
    let tag_start = html[..marker_idx].rfind("<link")?;
    let tag_end = html[marker_idx..].find('>')? + marker_idx;
    let tag = &html[tag_start..tag_end];

    let href_marker = "href=\"";
    let href_idx = tag.find(href_marker)? + href_marker.len();
    let rest = &tag[href_idx..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract a `channel_id=UC...` query parameter value from `href`, as found
/// on the `feeds/videos.xml?channel_id=UC...` RSS autodiscovery link.
fn extract_channel_id_query_param(href: &str) -> Option<String> {
    let marker = "channel_id=";
    let idx = href.find(marker)?;
    let after = &href[idx + marker.len()..];
    let id: String = after
        .chars()
        .take_while(|c| *c != '&' && *c != '"' && !c.is_whitespace())
        .collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// Build the canonical feed URL for channel id `id`.
fn build_feed_url(id: &str) -> String {
    format!("https://www.youtube.com/feeds/videos.xml?channel_id={id}")
}

/// `true` if `s` looks like a bare channel id: starts with `"UC"`, is at
/// least 10 characters, and has no `/` or whitespace in it (so a `/channel/
/// UC.../` URL -- handled separately by [`extract_channel_id_from_url`] --
/// never also matches here).
fn looks_like_bare_channel_id(s: &str) -> bool {
    s.starts_with("UC") && s.len() >= 10 && !s.contains('/') && !s.contains(char::is_whitespace)
}

/// Extract a handle (without its leading `@`) from `s`, if `s` is a handle
/// form: a bare `@handle`, or a `youtube.com/@handle` URL (with or without a
/// scheme, `www.` prefix, trailing slash, or extra path segments after the
/// handle, mirroring [`extract_channel_id_from_url`]'s tolerance). Returns
/// `None` for anything else, including a bare channel id or a `/channel/
/// UC.../` URL, so callers can try those forms first without this ever
/// stealing the match.
fn extract_handle(s: &str) -> Option<String> {
    let marker = "youtube.com/@";
    if let Some(idx) = s.find(marker) {
        let after = &s[idx + marker.len()..];
        let handle: String = after
            .chars()
            .take_while(|c| *c != '/' && *c != '?' && !c.is_whitespace())
            .collect();
        return if handle.is_empty() {
            None
        } else {
            Some(handle)
        };
    }

    let rest = s.strip_prefix('@')?;
    if !rest.is_empty() && !rest.contains('/') && !rest.contains(char::is_whitespace) {
        Some(rest.to_string())
    } else {
        None
    }
}

/// Build the handle page URL to fetch (e.g.
/// `https://www.youtube.com/@mattpocockuk`) for `handle` (without its
/// leading `@`, as returned by [`extract_handle`]).
fn handle_page_url(handle: &str) -> String {
    format!("https://www.youtube.com/@{handle}")
}

/// Extract `{id}` from a `youtube.com/channel/{id}` URL, tolerating a
/// missing scheme, a missing/present `www.` prefix, and a trailing slash or
/// extra path segments after `{id}` (e.g. `/channel/UC.../videos`). Returns
/// `None` if `s` doesn't contain a `youtube.com/channel/` segment at all.
fn extract_channel_id_from_url(s: &str) -> Option<String> {
    let marker = "youtube.com/channel/";
    let idx = s.find(marker)?;
    let after = &s[idx + marker.len()..];
    let id: String = after
        .chars()
        .take_while(|c| *c != '/' && *c != '?' && !c.is_whitespace())
        .collect();

    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_FEED_URL: &str =
        "https://www.youtube.com/feeds/videos.xml?channel_id=UC_x5XG1OV2P6uZZ5FSM9Ttw";

    /// A trimmed-down but verbatim excerpt of the real markup returned by
    /// `curl https://www.youtube.com/@mattpocockuk` (bd issue drip-ho5.11,
    /// checked live on 2026-08-08) -- the surrounding multi-megabyte page of
    /// inline JS is dropped, but the `<link rel="canonical">`, `<title>`,
    /// and RSS autodiscovery `<link rel="alternate" type="application/
    /// rss+xml">` tags, and their exact attribute order, are copied
    /// unmodified from the real response. The real channel id resolved:
    /// `UCswG6FSbgZjbWtdf_hMLaow` (Matt Pocock's channel).
    const HANDLE_PAGE_FIXTURE: &str = concat!(
        "<!doctype html><html><head>",
        r#"<link rel="canonical" href="https://www.youtube.com/channel/UCswG6FSbgZjbWtdf_hMLaow">"#,
        r#"<link rel="alternate" media="handheld" href="https://m.youtube.com/@mattpocockuk">"#,
        r#"<link rel="alternate" media="only screen and (max-width: 640px)" href="https://m.youtube.com/@mattpocockuk">"#,
        "<title>Matt Pocock - YouTube</title>",
        r#"<meta name="description" content="Become an AI Hero with tips, tricks and tutorials.">"#,
        r#"<link rel="alternate" type="application/rss+xml" title="RSS" href="https://www.youtube.com/feeds/videos.xml?channel_id=UCswG6FSbgZjbWtdf_hMLaow">"#,
        r#"<meta property="og:title" content="Matt Pocock">"#,
        "</head><body></body></html>"
    );

    const REAL_RESOLVED_CHANNEL_ID: &str = "UCswG6FSbgZjbWtdf_hMLaow";

    #[test]
    fn channel_id_from_handle_page_parses_the_real_fixture() {
        let id = channel_id_from_handle_page(HANDLE_PAGE_FIXTURE)
            .expect("the real handle page fixture should parse");
        assert_eq!(id, REAL_RESOLVED_CHANNEL_ID);
    }

    #[test]
    fn channel_id_from_handle_page_falls_back_to_the_rss_autodiscovery_link_without_a_canonical_link(
    ) {
        let html = concat!(
            "<html><head>",
            r#"<link rel="alternate" type="application/rss+xml" title="RSS" href="https://www.youtube.com/feeds/videos.xml?channel_id=UCswG6FSbgZjbWtdf_hMLaow">"#,
            "</head></html>"
        );
        let id = channel_id_from_handle_page(html)
            .expect("the RSS autodiscovery link alone should be enough to resolve");
        assert_eq!(id, REAL_RESOLVED_CHANNEL_ID);
    }

    #[test]
    fn channel_id_from_handle_page_errors_clearly_when_no_channel_id_is_present() {
        let err =
            channel_id_from_handle_page("<html><head><title>Nothing here</title></head></html>")
                .expect_err("a page with no channel id anywhere should error");
        let message = err.to_string();
        assert!(
            message.contains("channel id"),
            "error should mention 'channel id': {message}"
        );
    }

    #[test]
    fn bare_channel_id_resolves_to_the_feed_url() {
        let resolved =
            channel_feed_url("UC_x5XG1OV2P6uZZ5FSM9Ttw").expect("bare channel id should resolve");
        assert_eq!(resolved, EXPECTED_FEED_URL);
    }

    #[test]
    fn channel_url_resolves_to_the_same_feed_url_as_the_bare_id() {
        let resolved = channel_feed_url("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw")
            .expect("channel URL should resolve");
        assert_eq!(resolved, EXPECTED_FEED_URL);
    }

    #[test]
    fn channel_url_with_trailing_slash_resolves_correctly() {
        let resolved =
            channel_feed_url("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw/")
                .expect("channel URL with trailing slash should resolve");
        assert_eq!(resolved, EXPECTED_FEED_URL);
    }

    #[test]
    fn channel_url_with_extra_path_segment_resolves_correctly() {
        let resolved =
            channel_feed_url("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw/videos")
                .expect("channel URL with an extra path segment should resolve");
        assert_eq!(resolved, EXPECTED_FEED_URL);
    }

    #[test]
    fn channel_url_without_scheme_or_www_still_resolves() {
        let resolved = channel_feed_url("youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw")
            .expect("scheme-less, www-less channel URL should still resolve");
        assert_eq!(resolved, EXPECTED_FEED_URL);
    }

    #[test]
    fn an_already_constructed_feed_url_passes_through_unchanged() {
        let already = "  https://www.youtube.com/feeds/videos.xml?channel_id=UCabc123XYZ  ";
        let resolved =
            channel_feed_url(already).expect("an already-constructed feed URL should pass through");
        assert_eq!(
            resolved,
            "https://www.youtube.com/feeds/videos.xml?channel_id=UCabc123XYZ"
        );
    }

    #[test]
    fn custom_url_style_link_still_errors_with_a_clear_explanation() {
        // `/c/...` and `/user/...` custom URLs are a different, still-
        // unsupported form -- unlike `/@handle` (handled by
        // `extract_handle`/network resolution below), YouTube's `/c/`/`/user/`
        // vanity URLs don't reliably expose a channel id anywhere in their
        // own markup the way a handle page's canonical link does, so they're
        // intentionally left out of scope here.
        let err = channel_feed_url("https://www.youtube.com/c/SomeChannel")
            .expect_err("a /c/ custom URL should still error");

        let message = err.to_string();
        assert!(
            message.contains("channel id"),
            "error should mention 'channel id': {message}"
        );
    }

    #[test]
    fn extract_handle_recognizes_a_bare_at_handle() {
        assert_eq!(
            extract_handle("@mattpocockuk"),
            Some("mattpocockuk".to_string())
        );
    }

    #[test]
    fn extract_handle_recognizes_a_full_url_with_scheme_and_www() {
        assert_eq!(
            extract_handle("https://www.youtube.com/@mattpocockuk"),
            Some("mattpocockuk".to_string())
        );
    }

    #[test]
    fn extract_handle_recognizes_a_url_without_scheme_or_www() {
        assert_eq!(
            extract_handle("youtube.com/@mattpocockuk"),
            Some("mattpocockuk".to_string())
        );
    }

    #[test]
    fn extract_handle_tolerates_a_trailing_path_segment() {
        assert_eq!(
            extract_handle("https://www.youtube.com/@mattpocockuk/videos"),
            Some("mattpocockuk".to_string())
        );
    }

    #[test]
    fn extract_handle_returns_none_for_a_bare_channel_id() {
        assert_eq!(extract_handle("UC_x5XG1OV2P6uZZ5FSM9Ttw"), None);
    }

    #[test]
    fn extract_handle_returns_none_for_a_channel_url() {
        assert_eq!(
            extract_handle("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw"),
            None
        );
    }

    #[test]
    fn extract_handle_returns_none_for_unrelated_garbage() {
        assert_eq!(extract_handle("not a url or id"), None);
    }

    #[test]
    fn handle_page_url_builds_the_handle_page_url() {
        assert_eq!(
            handle_page_url("mattpocockuk"),
            "https://www.youtube.com/@mattpocockuk"
        );
    }

    #[test]
    fn fetch_channel_id_from_handle_page_resolves_via_mocked_network() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/@mattpocockuk")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(HANDLE_PAGE_FIXTURE)
            .create();

        let url = format!("{}/@mattpocockuk", server.url());
        let id = fetch_channel_id_from_handle_page(&url)
            .expect("a mocked 200 response with the real fixture body should resolve");
        assert_eq!(id, REAL_RESOLVED_CHANNEL_ID);
    }

    #[test]
    fn fetch_channel_id_from_handle_page_errors_actionably_on_a_404() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/@nonexistenthandle")
            .with_status(404)
            .with_body("not found")
            .create();

        let url = format!("{}/@nonexistenthandle", server.url());
        let err = fetch_channel_id_from_handle_page(&url)
            .expect_err("a 404 handle page should error, not silently resolve");

        let message = err.to_string();
        assert!(
            message.contains("404"),
            "error should mention the HTTP status: {message}"
        );
    }

    #[test]
    fn fetch_channel_id_from_handle_page_errors_actionably_when_the_page_has_no_channel_id() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/@somehandle")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><head><title>Nothing here</title></head></html>")
            .create();

        let url = format!("{}/@somehandle", server.url());
        let err = fetch_channel_id_from_handle_page(&url)
            .expect_err("a page with no channel id should error, not silently resolve");

        let message = err.to_string();
        assert!(
            message.contains("channel id"),
            "error should mention 'channel id': {message}"
        );
    }

    #[test]
    fn garbage_input_errors_clearly() {
        let err = channel_feed_url("not a url or id").expect_err("garbage input should error");

        let message = err.to_string();
        assert!(
            message.contains("channel id") || message.contains("channel/UC"),
            "error should explain what a valid input looks like: {message}"
        );
    }
}
