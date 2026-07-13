//! Minimal Reddit API client: OAuth2 "client credentials" auth, plus
//! listing/search fetching mapped into a clean `Post` type.
//!
//! Credentials are supplied by the caller (see [`crate::credentials`], which
//! tries the OS keyring first and falls back to
//! `DRIP_REDDIT_CLIENT_ID` / `DRIP_REDDIT_CLIENT_SECRET`) and passed into
//! [`RedditClient::new`].

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::types::{Sort, TimeFilter};
use crate::vprintln;

const DEFAULT_TOKEN_BASE: &str = "https://www.reddit.com";
const DEFAULT_API_BASE: &str = "https://oauth.reddit.com";

/// Refresh the cached token this many seconds before Reddit says it will
/// actually expire, so we never send a request with a token that's about to
/// die mid-flight.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// A cleaned-up Reddit post, independent of the raw listing JSON shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    pub id: String,
    pub title: String,
    pub author: String,
    pub subreddit: String,
    /// Full `https://reddit.com/...` URL to the comments page.
    pub permalink: String,
    /// The link target Reddit gives you; identical to `permalink` for self
    /// posts.
    pub url: String,
    pub is_self: bool,
    /// `None` if the post has no self text (or isn't a self post).
    pub selftext: Option<String>,
    pub score: i64,
    pub upvote_ratio: f64,
    pub num_comments: i64,
    /// Raw epoch seconds, as given by Reddit. Converting this to a proper
    /// timestamp type is the vault writer's job (drip-15n.3), not this
    /// module's.
    pub created_utc: f64,
    pub link_flair_text: Option<String>,
    pub over_18: bool,
}

impl From<RawPost> for Post {
    fn from(raw: RawPost) -> Self {
        let selftext = if raw.selftext.trim().is_empty() {
            None
        } else {
            Some(raw.selftext)
        };

        Post {
            id: raw.id,
            title: raw.title,
            author: raw.author,
            subreddit: raw.subreddit,
            permalink: format!("https://reddit.com{}", raw.permalink),
            url: raw.url,
            is_self: raw.is_self,
            selftext,
            score: raw.score,
            upvote_ratio: raw.upvote_ratio,
            num_comments: raw.num_comments,
            created_utc: raw.created_utc,
            link_flair_text: raw.link_flair_text,
            over_18: raw.over_18,
        }
    }
}

/// Raw shape of a single post's `data` object inside a Reddit listing.
/// Reddit's real-world responses are full of fields that are technically
/// optional or occasionally null/missing, so nearly everything here defaults
/// rather than failing deserialization.
#[derive(Debug, Default, Deserialize)]
struct RawPost {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    subreddit: String,
    #[serde(default)]
    permalink: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    is_self: bool,
    #[serde(default)]
    selftext: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    upvote_ratio: f64,
    #[serde(default)]
    num_comments: i64,
    #[serde(default)]
    created_utc: f64,
    #[serde(default)]
    link_flair_text: Option<String>,
    #[serde(default)]
    over_18: bool,
}

/// A single `{ "kind": "t3", "data": {...} }` entry in a listing's
/// `children` array. We only care about `data`; `kind` is intentionally
/// ignored (the wrapper always contains post data for the endpoints this
/// client calls).
#[derive(Debug, Default, Deserialize)]
struct RedditThing {
    #[serde(default)]
    data: RawPost,
}

#[derive(Debug, Default, Deserialize)]
struct RedditListingData {
    #[serde(default)]
    children: Vec<RedditThing>,
}

#[derive(Debug, Deserialize)]
struct RedditListingResponse {
    data: RedditListingData,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: String,
    expires_in: u64,
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// A Reddit API client authenticated via OAuth2 client-credentials grant.
pub struct RedditClient {
    http: Client,
    client_id: String,
    client_secret: String,
    user_agent: String,
    token: Option<CachedToken>,
    /// Base URL (no trailing path) for the token endpoint host, e.g.
    /// `https://www.reddit.com`. Overridable in tests to point at a mock
    /// server.
    token_base: String,
    /// Base URL (no trailing path) for the authenticated API host, e.g.
    /// `https://oauth.reddit.com`. Overridable in tests.
    api_base: String,
}

impl RedditClient {
    /// Build a client for the given app credentials.
    pub fn new(client_id: String, client_secret: String) -> Self {
        RedditClient {
            http: Client::new(),
            client_id,
            client_secret,
            // Reddit's API rules require a descriptive User-Agent
            // identifying the app and its author, e.g.
            // "appname/version (by /u/username)". There's no real Reddit
            // username to embed here, so this is a generic placeholder --
            // personalize it via config once that lands (drip-15n.5+).
            user_agent: "drip/0.1 (by /u/drip-cli-user)".to_string(),
            token: None,
            token_base: DEFAULT_TOKEN_BASE.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Point this client at test doubles instead of the real Reddit hosts.
    #[cfg(test)]
    fn with_base_urls(
        mut self,
        token_base: impl Into<String>,
        api_base: impl Into<String>,
    ) -> Self {
        self.token_base = token_base.into();
        self.api_base = api_base.into();
        self
    }

    /// Return a valid bearer token, fetching (or refreshing) one from Reddit
    /// if the cached token is missing or about to expire.
    pub fn ensure_token(&mut self, verbose: bool) -> Result<&str> {
        let needs_refresh = match &self.token {
            Some(cached) => Instant::now() >= cached.expires_at,
            None => true,
        };

        if needs_refresh {
            let url = format!("{}/api/v1/access_token", self.token_base);
            vprintln(verbose, "requesting Reddit access token");
            let resp = self
                .http
                .post(&url)
                .basic_auth(&self.client_id, Some(&self.client_secret))
                .header(USER_AGENT, self.user_agent.as_str())
                .form(&[("grant_type", "client_credentials")])
                .send()
                .context("failed to reach Reddit's access_token endpoint")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().unwrap_or_default();
                bail!(
                    "Reddit rejected the access token request (HTTP {status}): {}",
                    truncate(&body, 200)
                );
            }

            let parsed: AccessTokenResponse = resp
                .json()
                .context("failed to parse Reddit's access_token response as JSON")?;

            let ttl = Duration::from_secs(parsed.expires_in).saturating_sub(TOKEN_REFRESH_MARGIN);
            self.token = Some(CachedToken {
                access_token: parsed.access_token,
                expires_at: Instant::now() + ttl,
            });
        }

        Ok(&self
            .token
            .as_ref()
            .expect("token was just set above")
            .access_token)
    }

    /// Fetch a subreddit's listing (hot/top/new/rising/controversial).
    pub fn fetch_listing(
        &mut self,
        subreddit: &str,
        sort: Sort,
        time: Option<TimeFilter>,
        limit: u32,
        verbose: bool,
    ) -> Result<Vec<Post>> {
        let token = self.ensure_token(verbose)?.to_string();
        let url = format!("{}/r/{subreddit}/{}", self.api_base, sort.as_str());

        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if matches!(sort, Sort::Top | Sort::Controversial) {
            if let Some(t) = time {
                query.push(("t", t.as_str().to_string()));
            }
        }

        vprintln(verbose, format!("GET {url}?{}", format_query(&query)));

        let resp = self
            .http
            .get(&url)
            .header(USER_AGENT, self.user_agent.as_str())
            .header(AUTHORIZATION, format!("bearer {token}"))
            .query(&query)
            .send()
            .with_context(|| format!("failed to fetch r/{subreddit} listing"))?;

        sleep_if_rate_limited(&resp, verbose);

        let status = resp.status();
        if !status.is_success() {
            return Err(listing_status_error(subreddit, status));
        }

        let parsed: RedditListingResponse = resp
            .json()
            .with_context(|| format!("failed to parse r/{subreddit} listing JSON"))?;

        Ok(parsed
            .data
            .children
            .into_iter()
            .map(|t| t.data.into())
            .collect())
    }

    /// Search within a subreddit.
    pub fn search(
        &mut self,
        subreddit: &str,
        query: &str,
        sort: Sort,
        time: Option<TimeFilter>,
        limit: u32,
        verbose: bool,
    ) -> Result<Vec<Post>> {
        let token = self.ensure_token(verbose)?.to_string();
        let url = format!("{}/r/{subreddit}/search", self.api_base);

        let mut params: Vec<(&str, String)> = vec![
            ("q", query.to_string()),
            ("restrict_sr", "1".to_string()),
            ("sort", sort.as_str().to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(t) = time {
            params.push(("t", t.as_str().to_string()));
        }

        vprintln(verbose, format!("GET {url}?{}", format_query(&params)));

        let resp = self
            .http
            .get(&url)
            .header(USER_AGENT, self.user_agent.as_str())
            .header(AUTHORIZATION, format!("bearer {token}"))
            .query(&params)
            .send()
            .with_context(|| format!("failed to search r/{subreddit}"))?;

        sleep_if_rate_limited(&resp, verbose);

        let status = resp.status();
        if !status.is_success() {
            return Err(listing_status_error(subreddit, status));
        }

        let parsed: RedditListingResponse = resp
            .json()
            .with_context(|| format!("failed to parse r/{subreddit} search JSON"))?;

        Ok(parsed
            .data
            .children
            .into_iter()
            .map(|t| t.data.into())
            .collect())
    }

    /// Fetch (or search) each subreddit in turn, returning a per-subreddit
    /// result so one private/banned/missing subreddit doesn't take down the
    /// whole batch.
    pub fn fetch_many(
        &mut self,
        subreddits: &[String],
        query: Option<&str>,
        sort: Sort,
        time: Option<TimeFilter>,
        limit: u32,
        verbose: bool,
    ) -> Vec<(String, Result<Vec<Post>>)> {
        subreddits
            .iter()
            .map(|sub| {
                let result = match query {
                    Some(q) => self.search(sub, q, sort, time, limit, verbose),
                    None => self.fetch_listing(sub, sort, time, limit, verbose),
                };
                (sub.clone(), result)
            })
            .collect()
    }
}

/// Render a query param list as a `k=v&k=v` string for verbose-only request
/// logging. Not used for the actual request (that goes through
/// `RequestBuilder::query`, which handles escaping); this is diagnostic
/// output only, so it's fine that it isn't percent-encoded.
fn format_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Best-effort, non-blocking-framework rate limit courtesy: if Reddit tells
/// us we're nearly out of requests, sleep the remaining reset window before
/// the caller's next request goes out.
fn sleep_if_rate_limited(resp: &Response, verbose: bool) {
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok());
    let reset_secs = resp
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    if let (Some(remaining), Some(reset_secs)) = (remaining, reset_secs) {
        if remaining < 2.0 && reset_secs > 0 {
            vprintln(verbose, format!("rate limit low, sleeping {reset_secs}s"));
            thread::sleep(Duration::from_secs(reset_secs));
        }
    }
}

fn listing_status_error(subreddit: &str, status: StatusCode) -> anyhow::Error {
    match status.as_u16() {
        403 => anyhow!("r/{subreddit}: private or banned (403)"),
        404 => anyhow!("r/{subreddit}: not found (404)"),
        other => anyhow!("r/{subreddit}: HTTP {other}"),
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but realistic Reddit listing JSON fixture: two posts, one
    /// of which is missing `link_flair_text` entirely (as real listings
    /// sometimes do) to exercise the `#[serde(default)]` handling.
    const LISTING_FIXTURE: &str = r#"{
        "kind": "Listing",
        "data": {
            "children": [
                {
                    "kind": "t3",
                    "data": {
                        "id": "abc123",
                        "title": "First post",
                        "author": "someone",
                        "subreddit": "rust",
                        "permalink": "/r/rust/comments/abc123/first_post/",
                        "url": "https://reddit.com/r/rust/comments/abc123/first_post/",
                        "is_self": true,
                        "selftext": "Hello world",
                        "score": 42,
                        "upvote_ratio": 0.95,
                        "num_comments": 3,
                        "created_utc": 1700000000.0,
                        "link_flair_text": "Discussion",
                        "over_18": false
                    }
                },
                {
                    "kind": "t3",
                    "data": {
                        "id": "def456",
                        "title": "Second post",
                        "author": "someone_else",
                        "subreddit": "rust",
                        "permalink": "/r/rust/comments/def456/second_post/",
                        "url": "https://example.com/some-link",
                        "is_self": false,
                        "selftext": "",
                        "score": 7,
                        "upvote_ratio": 0.66,
                        "num_comments": 0,
                        "created_utc": 1700000100.0,
                        "over_18": false
                    }
                }
            ]
        }
    }"#;

    fn token_response_body() -> String {
        serde_json::json!({
            "access_token": "test-token-123",
            "token_type": "bearer",
            "expires_in": 3600,
            "scope": "*"
        })
        .to_string()
    }

    fn test_client(server: &mockito::Server) -> RedditClient {
        RedditClient::new("client-id".to_string(), "client-secret".to_string())
            .with_base_urls(server.url(), server.url())
    }

    #[test]
    fn fetches_token_then_listing_and_parses_posts() {
        let mut server = mockito::Server::new();

        let _token_mock = server
            .mock("POST", "/api/v1/access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(token_response_body())
            .create();

        let _listing_mock = server
            .mock("GET", mockito::Matcher::Regex(r"^/r/rust/hot".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(LISTING_FIXTURE)
            .create();

        let mut client = test_client(&server);
        let posts = client
            .fetch_listing("rust", Sort::Hot, None, 10, false)
            .expect("fetch_listing should succeed against the mock server");

        assert_eq!(posts.len(), 2);

        let first = &posts[0];
        assert_eq!(first.id, "abc123");
        assert_eq!(first.title, "First post");
        assert_eq!(
            first.permalink,
            "https://reddit.com/r/rust/comments/abc123/first_post/"
        );
        assert_eq!(first.selftext, Some("Hello world".to_string()));
        assert_eq!(first.score, 42);
        assert_eq!(first.link_flair_text, Some("Discussion".to_string()));

        // Second post has no `link_flair_text` field at all in the fixture
        // and empty selftext -- both should come through as `None` rather
        // than failing deserialization.
        let second = &posts[1];
        assert_eq!(second.id, "def456");
        assert_eq!(second.link_flair_text, None);
        assert_eq!(second.selftext, None);
        assert!(!second.is_self);
    }

    #[test]
    fn fetch_many_isolates_a_forbidden_subreddit_from_the_rest() {
        let mut server = mockito::Server::new();

        let _token_mock = server
            .mock("POST", "/api/v1/access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(token_response_body())
            .create();

        let _forbidden_mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/r/private_sub/hot".to_string()),
            )
            .with_status(403)
            .with_body("private")
            .create();

        let _ok_mock = server
            .mock("GET", mockito::Matcher::Regex(r"^/r/rust/hot".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(LISTING_FIXTURE)
            .create();

        let mut client = test_client(&server);
        let subreddits = vec!["private_sub".to_string(), "rust".to_string()];
        let results = client.fetch_many(&subreddits, None, Sort::Hot, None, 10, false);

        assert_eq!(results.len(), 2);

        let (name, result) = &results[0];
        assert_eq!(name, "private_sub");
        let err = result.as_ref().expect_err("private_sub should fail");
        assert!(err.to_string().contains("403"));

        let (name, result) = &results[1];
        assert_eq!(name, "rust");
        let posts = result.as_ref().expect("rust should still succeed");
        assert_eq!(posts.len(), 2);
    }

    #[test]
    fn malformed_optional_fields_do_not_fail_deserialization() {
        let mut server = mockito::Server::new();

        let _token_mock = server
            .mock("POST", "/api/v1/access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(token_response_body())
            .create();

        // Missing `link_flair_text`, `over_18`, `upvote_ratio`, and even
        // `selftext` entirely -- all of these should default rather than
        // erroring.
        let sparse_fixture = r#"{
            "kind": "Listing",
            "data": {
                "children": [
                    {
                        "kind": "t3",
                        "data": {
                            "id": "sparse1",
                            "title": "Sparse post",
                            "subreddit": "rust",
                            "permalink": "/r/rust/comments/sparse1/",
                            "score": 1
                        }
                    }
                ]
            }
        }"#;

        let _listing_mock = server
            .mock("GET", mockito::Matcher::Regex(r"^/r/rust/hot".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(sparse_fixture)
            .create();

        let mut client = test_client(&server);
        let posts = client
            .fetch_listing("rust", Sort::Hot, None, 10, false)
            .expect("sparse fixture should still deserialize");

        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].id, "sparse1");
        assert_eq!(posts[0].link_flair_text, None);
        assert_eq!(posts[0].selftext, None);
        assert!(!posts[0].over_18);
        assert_eq!(posts[0].upvote_ratio, 0.0);
    }

    #[test]
    fn ensure_token_surfaces_a_clean_error_on_http_failure() {
        let mut server = mockito::Server::new();

        let _token_mock = server
            .mock("POST", "/api/v1/access_token")
            .with_status(401)
            .with_body("{\"message\": \"Unauthorized\", \"error\": 401}")
            .create();

        let mut client = test_client(&server);
        let err = client
            .ensure_token(false)
            .expect_err("bad credentials should error, not panic");
        assert!(err.to_string().contains("401"));
    }
}
