//! A normalized item shape shared across source kinds (Reddit today; RSS and
//! others later -- see bd issue drip-15n.9.6). Digest rendering and dedup
//! logic operate on this type rather than on any single source's own
//! (richer) type, so those two modules don't need to know anything about
//! Reddit specifically.
//!
//! [`crate::reddit::Post`] stays exactly as-is (Reddit-specific, produced by
//! [`crate::reddit::RedditClient`]); `impl From<Post> for Item` below is the
//! one-way conversion applied at the point of consumption.

use chrono::{DateTime, Utc};

use crate::reddit::Post;

/// A single fetched item, normalized across source kinds. Fields that only
/// make sense for some source kinds (Reddit's score/comment count/flair/
/// NSFW flag) are `Option`/default-`false` so non-Reddit sources can simply
/// leave them unset.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub url: String,
    /// A separate "discussion" link, when the source has one that differs
    /// from `url` (e.g. a Reddit link post's comments page). `None` when
    /// there's no such distinct link (e.g. a Reddit self post, where the
    /// comments page and the content are the same page).
    pub comments_url: Option<String>,
    pub author: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    /// Reddit only, for now.
    pub score: Option<i64>,
    /// Reddit only, for now.
    pub num_comments: Option<i64>,
    /// Reddit only, for now.
    pub flair: Option<String>,
    /// Reddit only, for now; defaults to `false` for source kinds that have
    /// no equivalent concept.
    pub nsfw: bool,
}

impl From<Post> for Item {
    fn from(post: Post) -> Self {
        let comments_url = if post.permalink != post.url {
            Some(post.permalink)
        } else {
            None
        };

        Item {
            id: post.id,
            title: post.title,
            url: post.url,
            comments_url,
            author: Some(post.author),
            published_at: DateTime::from_timestamp(post.created_utc as i64, 0),
            summary: post.selftext,
            score: Some(post.score),
            num_comments: Some(post.num_comments),
            flair: post.link_flair_text,
            nsfw: post.over_18,
        }
    }
}
