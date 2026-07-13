//! Resolves whatever a user passes to `drip source add --kind youtube --url
//! <input>` into a YouTube channel's Atom feed URL. No network I/O lives
//! here -- this module is pure string logic.
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
//! What this module *doesn't* do: resolve a `/@handle`, `/c/{name}`, or
//! `/user/{name}` URL to a channel id. YouTube doesn't expose that mapping
//! in the URL itself -- doing so would require an extra HTTP round-trip
//! (fetching the handle page and scraping its `channelId`), which is out of
//! scope here. Callers with a handle URL get a clear error pointing them at
//! how to find the canonical `channel_id` themselves.

use anyhow::{bail, Result};

/// Turn `input` (whatever the user passed to `drip source add --kind youtube
/// --url <input>`) into the canonical feed URL
/// `https://www.youtube.com/feeds/videos.xml?channel_id={id}`.
///
/// Accepts, in order of precedence:
/// 1. An already-constructed `feeds/videos.xml` URL (a power-user escape
///    hatch) -- returned verbatim (after trimming).
/// 2. A `https://www.youtube.com/channel/{id}` URL, with or without
///    `http://`, with or without a `www.` prefix, with or without a trailing
///    slash or extra path segments after `{id}`.
/// 3. A bare channel id -- starts with `"UC"` and is at least 10 characters.
///
/// Errors clearly on:
/// - A handle/custom-URL form (`youtube.com/@...`, `youtube.com/c/...`,
///   `youtube.com/user/...`) -- these can't be resolved to a channel id
///   without an extra HTTP round-trip, which is out of scope here.
/// - Anything else that doesn't look like a channel id or `/channel/UC.../`
///   URL.
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

    if trimmed.contains("youtube.com/@")
        || trimmed.contains("youtube.com/c/")
        || trimmed.contains("youtube.com/user/")
    {
        bail!(
            "'{trimmed}' is a YouTube handle/custom-URL link, which can't be resolved to a \
             channel id without an extra HTTP request (out of scope for `drip source add`). \
             Find the channel's canonical channel id instead -- it starts with \"UC\" -- by \
             opening the channel and viewing page source for `\"channelId\":\"UC...`, or by \
             using the channel's own https://www.youtube.com/channel/UC.../ URL if you can find \
             one (many channels link this from their About page), and pass that instead."
        );
    }

    bail!(
        "'{trimmed}' doesn't look like a YouTube channel id (starts with \"UC\") or a \
         https://www.youtube.com/channel/UC.../ URL"
    );
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
    fn handle_style_url_errors_with_a_clear_explanation() {
        let err = channel_feed_url("https://www.youtube.com/@SomeHandle")
            .expect_err("a handle-style URL should error");

        let message = err.to_string();
        assert!(
            message.contains("channel id"),
            "error should mention 'channel id': {message}"
        );
        assert!(
            message.contains("extra HTTP request") || message.contains("round-trip"),
            "error should explain the handle-resolution limitation, not just say 'invalid': \
             {message}"
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
