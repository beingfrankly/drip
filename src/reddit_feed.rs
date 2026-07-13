//! Builds a subreddit's public RSS/Atom feed URL (hot/new/top/rising/
//! controversial listing, or a free-text search within the subreddit). No
//! network I/O lives here -- this module is pure string/URL construction,
//! the same shape as [`crate::youtube::channel_feed_url`].
//!
//! Design context: bd issue drip-khu. Reddit's OAuth2 `client_credentials`
//! grant (see [`crate::reddit::RedditClient`]) stopped being self-service as
//! of 2026 -- new tokens now require manual "Responsible Builder Policy"
//! approval, which isn't a fit for `drip`'s "just works with an app id/
//! secret" model. We looked at Reddit's Devvit platform as an alternative
//! and ruled it out: it's an in-platform app framework (apps hosted BY
//! Reddit, scoped to communities that install them), not usable as a
//! generic personal-tool API. Instead, we confirmed LIVE that Reddit's old
//! unauthenticated per-subreddit RSS/Atom endpoints still work:
//!
//! - `https://www.reddit.com/r/{sub}/{sort}/.rss` -- a plain listing feed,
//!   for `sort` in hot/new/top/rising/controversial, with `top`/
//!   `controversial` additionally taking a `?t={time}` window (mirroring
//!   `crate::reddit::RedditClient::fetch_listing`'s OAuth equivalent
//!   exactly).
//! - `https://www.reddit.com/r/{sub}/search/.rss?q={query}&restrict_sr=1&sort={sort}`
//!   -- a genuine free-text Reddit search scoped to the subreddit (`restrict_sr=1`),
//!   confirmed real via a differential test (different search terms produce
//!   different result sets), not just silently ignored by the endpoint.
//!
//! Both endpoints returned real, rate-limited-but-not-hard-blocked responses
//! during that investigation -- Reddit sends real `x-ratelimit-*` headers on
//! them, same courtesy signal the OAuth API sends, just enforced per-IP
//! instead of per-token.
//!
//! Because these are just RSS/Atom feeds like any other, fetching one needs
//! no Reddit-specific client at all: once [`subreddit_feed_url`] below has
//! produced the right URL, fetching is delegated entirely to
//! [`crate::rss::fetch`], the exact same function RSS/YouTube sources use.
//!
//! What this module *doesn't* do: expose per-post flair as a filter. Flair
//! isn't present in the feed's entry XML at all (only a subreddit-level
//! `<category>`, not a per-post one) -- so `search`'s free-text query is the
//! supported way to narrow results (e.g. `--search tasks` for Obsidian-
//! related posts), and there is intentionally no flair-specific filtering
//! anywhere in this module.

use anyhow::{bail, Context, Result};
use reqwest::Url;

use crate::types::{Sort, TimeFilter};

/// Build the RSS/Atom feed URL for `subreddit`.
///
/// - `search: None` -- a plain listing feed:
///   `https://www.reddit.com/r/{subreddit}/{sort}/.rss`, with `?t={time}`
///   appended only when `sort` is [`Sort::Top`] or [`Sort::Controversial`]
///   *and* `time` is `Some` (any other sort silently drops a given `time`,
///   matching `RedditClient::fetch_listing`'s behavior exactly).
/// - `search: Some(query)` -- a subreddit-scoped search feed:
///   `https://www.reddit.com/r/{subreddit}/search/.rss?q={query}&restrict_sr=1&sort={sort}`,
///   with `t={time}` appended when `sort` is [`Sort::Top`] and `time` is
///   `Some`. Reddit's search endpoint only accepts `hot`/`top`/`new` as a
///   sort value -- [`Sort::Rising`] or [`Sort::Controversial`] here produce a
///   clear error rather than a URL Reddit would reject at request time.
///
/// Errors clearly if `subreddit` is empty (or all whitespace) after
/// trimming.
///
/// Uses [`Url::parse`] + [`url::UrlQuery`]'s `query_pairs_mut` (via
/// `reqwest`'s re-export of the `url` crate) rather than manual string
/// formatting, so the search term is correctly percent-encoded -- a query
/// containing a space, `&`, `#`, etc. round-trips correctly.
pub fn subreddit_feed_url(
    subreddit: &str,
    sort: Sort,
    time: Option<TimeFilter>,
    search: Option<&str>,
) -> Result<String> {
    let subreddit = subreddit.trim();
    if subreddit.is_empty() {
        bail!("subreddit name must not be empty");
    }

    match search {
        None => {
            let base = format!(
                "https://www.reddit.com/r/{subreddit}/{}/.rss",
                sort.as_str()
            );
            let mut url = Url::parse(&base)
                .with_context(|| format!("failed to build a valid URL from '{base}'"))?;

            if matches!(sort, Sort::Top | Sort::Controversial) {
                if let Some(t) = time {
                    url.query_pairs_mut().append_pair("t", t.as_str());
                }
            }

            Ok(url.to_string())
        }
        Some(query) => {
            if !matches!(sort, Sort::Hot | Sort::Top | Sort::New) {
                bail!(
                    "--search only supports sort in {{hot, top, new}} (Reddit's search endpoint \
                     doesn't accept '{}'); drop --search or pick one of those sorts",
                    sort.as_str()
                );
            }

            let base = format!("https://www.reddit.com/r/{subreddit}/search/.rss");
            let mut url = Url::parse(&base)
                .with_context(|| format!("failed to build a valid URL from '{base}'"))?;

            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("q", query);
                pairs.append_pair("restrict_sr", "1");
                pairs.append_pair("sort", sort.as_str());
                if sort == Sort::Top {
                    if let Some(t) = time {
                        pairs.append_pair("t", t.as_str());
                    }
                }
            }

            Ok(url.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_hot_has_no_query_string_at_all() {
        let url =
            subreddit_feed_url("rust", Sort::Hot, None, None).expect("plain hot should resolve");
        assert_eq!(url, "https://www.reddit.com/r/rust/hot/.rss");
    }

    #[test]
    fn top_with_time_window_appends_t_param() {
        let url = subreddit_feed_url("rust", Sort::Top, Some(TimeFilter::Week), None)
            .expect("top+time should resolve");
        assert_eq!(url, "https://www.reddit.com/r/rust/top/.rss?t=week");
    }

    #[test]
    fn hot_with_a_time_window_given_anyway_drops_it() {
        let url = subreddit_feed_url("rust", Sort::Hot, Some(TimeFilter::Week), None)
            .expect("hot+time should still resolve, just without t=");
        assert!(
            !url.contains("t="),
            "hot doesn't take a time window; t= must not appear: {url}"
        );
    }

    #[test]
    fn search_includes_q_restrict_sr_and_sort() {
        let url = subreddit_feed_url("rust", Sort::Hot, None, Some("tasks"))
            .expect("search should resolve");
        assert!(url.contains("q=tasks"), "{url}");
        assert!(url.contains("restrict_sr=1"), "{url}");
        assert!(url.contains("sort=hot"), "{url}");
    }

    #[test]
    fn search_with_a_multi_word_term_round_trips_semantically() {
        let url = subreddit_feed_url("ObsidianMD", Sort::Hot, None, Some("obsidian tasks"))
            .expect("multi-word search should resolve");

        let parsed = Url::parse(&url).expect("resulting URL should itself be parseable");
        let q = parsed
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .expect("q param should be present");

        assert_eq!(q, "obsidian tasks");
    }

    #[test]
    fn search_plus_top_plus_time_includes_t_alongside_search_params() {
        let url = subreddit_feed_url("rust", Sort::Top, Some(TimeFilter::Month), Some("macros"))
            .expect("search+top+time should resolve");

        assert!(url.contains("q=macros"), "{url}");
        assert!(url.contains("sort=top"), "{url}");
        assert!(url.contains("t=month"), "{url}");
    }

    #[test]
    fn search_with_rising_errors_with_valid_sorts_listed() {
        let err = subreddit_feed_url("rust", Sort::Rising, None, Some("tasks"))
            .expect_err("search + rising should error");
        let message = err.to_string();
        assert!(message.contains("hot"), "{message}");
        assert!(message.contains("top"), "{message}");
        assert!(message.contains("new"), "{message}");
    }

    #[test]
    fn search_with_controversial_errors_with_valid_sorts_listed() {
        let err = subreddit_feed_url("rust", Sort::Controversial, None, Some("tasks"))
            .expect_err("search + controversial should error");
        let message = err.to_string();
        assert!(message.contains("hot"), "{message}");
        assert!(message.contains("top"), "{message}");
        assert!(message.contains("new"), "{message}");
    }

    #[test]
    fn empty_subreddit_name_errors_clearly() {
        let err = subreddit_feed_url("", Sort::Hot, None, None)
            .expect_err("empty subreddit name should error");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn whitespace_only_subreddit_name_errors_clearly() {
        let err = subreddit_feed_url("   ", Sort::Hot, None, None)
            .expect_err("whitespace-only subreddit name should error");
        assert!(err.to_string().contains("empty"));
    }
}
