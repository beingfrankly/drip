//! Builds "digest" notes: markdown files summarizing a batch of fetched
//! items, grouped by source, written into the Obsidian vault.
//!
//! This module is split into a pure rendering half ([`render_digest_note`])
//! and a thin I/O half ([`write_digest_note`]) so the markdown/frontmatter
//! logic can be unit tested without touching the filesystem.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::item::Item;
use crate::types::{Sort, TimeFilter};

/// Characters that are unsafe (or at least unwelcome) in filenames across
/// the platforms an Obsidian vault might live on. Any of these, wherever
/// they show up in a computed filename, get replaced with `-`.
const UNSAFE_FILENAME_CHARS: [char; 9] = [':', '/', '\\', '*', '?', '"', '<', '>', '|'];

/// Roughly how many characters of an item's summary to show in the digest
/// excerpt before truncating.
const EXCERPT_CHAR_LIMIT: usize = 200;

/// Identifies one group of items in a [`DigestRun`]: which source kind it
/// came from (`"reddit"` today; `"rss"` and others later -- see bd issue
/// drip-15n.9.6) and the group's display name (a subreddit name, for
/// Reddit). Rendering picks source-kind-specific formatting (e.g. `## r/
/// {name}` for Reddit) based on `kind`.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceGroup {
    pub kind: String,
    pub name: String,
}

/// Everything needed to render (and name) a single digest note: which
/// sources were fetched and with what parameters, the tags to apply, the
/// items themselves (grouped by source, skipping any source whose fetch
/// failed), and when the run happened.
#[derive(Debug, Clone)]
pub struct DigestRun {
    pub sort: Sort,
    pub time: Option<TimeFilter>,
    pub query: Option<String>,
    /// User-supplied tags (e.g. from `--tag`), on top of the `reddit` /
    /// `reddit/{subreddit}` tags this module adds automatically for
    /// Reddit-origin groups.
    pub tags: Vec<String>,
    /// Fetched items, grouped by source, in the order they should appear in
    /// the note.
    pub items_by_source: Vec<(SourceGroup, Vec<Item>)>,
    /// The profile name used for this run, if any. Used as the digest's
    /// filename label instead of joining source names, when available.
    pub profile: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl DigestRun {
    /// The display name of each group in `items_by_source`, in fetch order
    /// (i.e. the ones that fetched successfully). For this phase every group
    /// is Reddit-origin, so these are subreddit names.
    pub fn source_labels(&self) -> Vec<String> {
        self.items_by_source
            .iter()
            .map(|(group, _)| group.name.clone())
            .collect()
    }

    /// The [`SourceGroup`] of each group in `items_by_source`, in fetch
    /// order, cloned. Unlike [`source_labels`](Self::source_labels) (bare
    /// names, used for `digest_filename`'s label), this keeps each group's
    /// `kind` -- needed so `journal::digest_bullet` can render `r/{name}`
    /// only for Reddit-origin groups (see bd issue drip-15n.9.6).
    pub fn source_groups(&self) -> Vec<SourceGroup> {
        self.items_by_source
            .iter()
            .map(|(group, _)| group.clone())
            .collect()
    }

    /// Total item count across all source groups.
    fn fetched_count(&self) -> usize {
        self.items_by_source
            .iter()
            .map(|(_, items)| items.len())
            .sum()
    }

    /// `reddit` + `reddit/{name}` for each distinct Reddit-origin group in
    /// `items_by_source`, plus any user-supplied tags, deduplicated while
    /// preserving first-seen order.
    fn all_tags(&self) -> Vec<String> {
        let mut tags = vec!["reddit".to_string()];
        for (group, _) in &self.items_by_source {
            if group.kind == "reddit" {
                tags.push(format!("reddit/{}", group.name));
            }
        }
        tags.extend(self.tags.iter().cloned());

        let mut seen = std::collections::HashSet::new();
        tags.into_iter()
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// The filename label: the profile name if one was used for this run,
    /// otherwise a comma-joined, trimmed list of source names.
    fn label(&self) -> String {
        match &self.profile {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => self
                .source_labels()
                .iter()
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// Replace any character in [`UNSAFE_FILENAME_CHARS`], anywhere in `name`,
/// with `-`. Applied to the whole computed filename (not just the label
/// portion), so timestamps or joins can never accidentally produce a
/// filesystem-hostile path segment.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if UNSAFE_FILENAME_CHARS.contains(&c) {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Compute the sanitized filename (including `.md` extension, excluding
/// any directory) for this digest run. Uses the local timezone for the
/// human-facing timestamp, matching what a person reading their vault
/// would expect.
pub fn digest_filename(run: &DigestRun) -> String {
    let local_ts = run.created_at.with_timezone(&Local).format("%Y-%m-%d %H%M");
    let raw = format!("{local_ts} - Reddit digest ({}).md", run.label());
    sanitize_filename(&raw)
}

/// Escape `[` and `]` in an item's title so it can't break `[title](url)`
/// markdown link syntax when embedded in one.
fn escape_title(title: &str) -> String {
    title.replace('[', "\\[").replace(']', "\\]")
}

/// Collapse a summary into a single-line excerpt: newlines become spaces,
/// and the result is truncated to roughly [`EXCERPT_CHAR_LIMIT`] characters
/// (on a `char` boundary, so this never panics on multi-byte UTF-8), with a
/// trailing `…` if truncation happened.
fn excerpt(summary: &str) -> String {
    let collapsed = summary.replace('\n', " ");
    let total_chars = collapsed.chars().count();
    if total_chars <= EXCERPT_CHAR_LIMIT {
        return collapsed;
    }
    let mut truncated: String = collapsed.chars().take(EXCERPT_CHAR_LIMIT).collect();
    truncated.push('…');
    truncated
}

/// Render one numbered item entry (without the trailing blank line). Every
/// field beyond `id`/`title`/`url` is rendered conditionally on whether
/// it's present, since only Reddit-origin items populate all of them today.
/// `source_kind` picks the author formatting: Reddit's `u/{name}` convention
/// only makes sense for Reddit usernames, not RSS/YouTube author names
/// (drip-01b).
fn render_item(index: usize, item: &Item, source_kind: &str) -> String {
    let nsfw = if item.nsfw { "⚠️ NSFW " } else { "" };
    let title = escape_title(&item.title);

    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(score) = item.score {
        meta_parts.push(format!("{score} pts"));
    }
    if let Some(num_comments) = item.num_comments {
        meta_parts.push(format!("{num_comments} comments"));
    }
    if let Some(author) = &item.author {
        if source_kind == "reddit" {
            // Reddit's own Atom/RSS feed's `<author><name>` field already
            // has the `/u/` prefix baked in (e.g. "/u/llogiq"), so strip any
            // pre-existing `/u/`/`u/` prefix before re-adding the canonical
            // one, to avoid a doubled `u//u/llogiq`. This is defensive --
            // kept in case any future `--kind reddit` source ever supplies a
            // bare username instead.
            let clean = author.trim_start_matches("/u/").trim_start_matches("u/");
            meta_parts.push(format!("u/{clean}"));
        } else {
            meta_parts.push(author.clone());
        }
    }
    if let Some(flair) = &item.flair {
        meta_parts.push(flair.clone());
    }
    let mut meta = meta_parts.join(" · ");

    if let Some(comments_url) = &item.comments_url {
        if comments_url != &item.url {
            meta.push_str(&format!(" · [external link]({})", item.url));
        }
    }

    let heading_link = item.comments_url.as_deref().unwrap_or(&item.url);
    // A sparse/malformed feed entry (via `feed-rs`) can have an empty title
    // and no url at all -- render that degenerately rather than as a dead
    // `**[]()**` markdown link: fall back to plain bold text when there's no
    // link to point at, and substitute a placeholder when the title itself
    // is (after trimming) empty, so the line is never just blank.
    let title_display = if title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        title
    };
    let heading = if heading_link.is_empty() {
        format!("**{title_display}**")
    } else {
        format!("**[{title_display}]({heading_link})**")
    };
    let mut lines = vec![format!("{index}. {nsfw}{heading} — {meta}")];

    if let Some(summary) = &item.summary {
        if !summary.is_empty() {
            lines.push(format!("   > {}", excerpt(summary)));
        }
    }

    lines.join("\n")
}

/// Pure rendering: given a `DigestRun`, produce the full markdown note text
/// (frontmatter + body). Does no I/O, which keeps it cheap to unit test.
pub fn render_digest_note(run: &DigestRun) -> String {
    let created_iso = run.created_at.format("%Y-%m-%dT%H:%M:%SZ");
    let tags = run.all_tags();
    let tags_block = tags
        .iter()
        .map(|t| format!("  - {t}"))
        .collect::<Vec<_>>()
        .join("\n");

    let subreddits_list = run.source_labels().join(", ");
    let time_filter_yaml = match run.time {
        Some(t) => t.as_str().to_string(),
        None => "null".to_string(),
    };
    let query_yaml = match &run.query {
        Some(q) => format!("\"{}\"", q.replace('"', "\\\"")),
        None => "null".to_string(),
    };

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("tags:\n");
    out.push_str(&tags_block);
    out.push('\n');
    out.push_str(&format!("createdOn: \"{created_iso}\"\n"));
    out.push_str(&format!("modifiedOn: \"{created_iso}\"\n"));
    out.push_str(&format!("subreddits: [{subreddits_list}]\n"));
    out.push_str(&format!("sort: {}\n", run.sort.as_str()));
    out.push_str(&format!("time_filter: {time_filter_yaml}\n"));
    out.push_str(&format!("query: {query_yaml}\n"));
    out.push_str(&format!("fetched_count: {}\n", run.fetched_count()));
    out.push_str("---\n\n");

    let local_ts = run
        .created_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M");
    out.push_str(&format!("# Reddit digest — {local_ts}\n\n"));

    let source_labels_display = run
        .items_by_source
        .iter()
        .map(|(group, _)| {
            if group.kind == "reddit" {
                format!("r/{}", group.name)
            } else {
                group.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sort_label = match run.time {
        Some(t) => format!("{} ({})", run.sort.as_str(), t.as_str()),
        None => run.sort.as_str().to_string(),
    };
    let query_label = run.query.as_deref().unwrap_or("—");
    out.push_str(&format!(
        "**Subreddits:** {source_labels_display} · **Sort:** {sort_label} · **Query:** {query_label}\n\n"
    ));

    for (group, items) in &run.items_by_source {
        let heading = if group.kind == "reddit" {
            format!("## r/{}", group.name)
        } else {
            format!("## {}", group.name)
        };
        out.push_str(&heading);
        out.push_str("\n\n");
        for (i, item) in items.iter().enumerate() {
            out.push_str(&render_item(i + 1, item, &group.kind));
            out.push_str("\n\n");
        }
    }

    // Trim any trailing blank lines added by the loop above, keep exactly
    // one trailing newline.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }

    out
}

/// Render `run` and write it to `{vault_path}/{posts_folder}/{filename}`,
/// creating the folder if it doesn't exist yet. Returns the full path
/// written.
pub fn write_digest_note(
    vault_path: &Path,
    posts_folder: &str,
    run: &DigestRun,
) -> Result<PathBuf> {
    let dir = vault_path.join(posts_folder);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create posts folder at {}", dir.display()))?;

    let filename = digest_filename(run);
    let path = dir.join(&filename);
    let content = render_digest_note(run);

    fs::write(&path, content)
        .with_context(|| format!("failed to write digest note at {}", path.display()))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_item(id: &str, title: &str) -> Item {
        Item {
            id: id.to_string(),
            title: title.to_string(),
            url: format!("https://reddit.com/r/rust/comments/{id}/post/"),
            comments_url: None,
            author: Some("someone".to_string()),
            published_at: None,
            summary: None,
            score: Some(42),
            num_comments: Some(5),
            flair: None,
            nsfw: false,
        }
    }

    fn sample_run(items_by_subreddit: Vec<(String, Vec<Item>)>) -> DigestRun {
        let items_by_source = items_by_subreddit
            .into_iter()
            .map(|(name, items)| {
                (
                    SourceGroup {
                        kind: "reddit".to_string(),
                        name,
                    },
                    items,
                )
            })
            .collect();
        DigestRun {
            sort: Sort::Top,
            time: Some(TimeFilter::Day),
            query: None,
            tags: vec![],
            items_by_source,
            profile: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 8, 14, 32, 10).unwrap(),
        }
    }

    #[test]
    fn demo_full_featured_rendering_sample() {
        let mut self_post = sample_item("abc123", "A [neat] discovery about lifetimes");
        self_post.summary = Some("I found something interesting today.\nHere's the gist of it, repeated a bit to push past two hundred characters so we can see the truncation ellipsis kick in for real: lorem ipsum dolor sit amet.".to_string());
        self_post.flair = Some("Discussion".to_string());

        let mut link_post = sample_item("def456", "Another title");
        link_post.comments_url =
            Some("https://reddit.com/r/rust/comments/def456/post/".to_string());
        link_post.url = "https://example.com/thing".to_string();
        link_post.score = Some(12);
        link_post.num_comments = Some(3);
        link_post.author = Some("someone".to_string());

        let mut nsfw_post = sample_item("ghi789", "A spicy post");
        nsfw_post.nsfw = true;
        nsfw_post.score = Some(5);

        let run = sample_run(vec![(
            "rust".to_string(),
            vec![self_post, link_post, nsfw_post],
        )]);
        let note = render_digest_note(&run);

        assert!(note.contains("A \\[neat\\] discovery about lifetimes"));
        assert!(note.contains("· Discussion"));
        assert!(note.contains("[external link](https://example.com/thing)"));
        assert!(note.contains("⚠️ NSFW **[A spicy post]"));
        // Exactly one blank line between the query summary line and the
        // first subreddit heading, and between post entries.
        assert!(!note.contains("\n\n\n"));
    }

    #[test]
    fn renders_basic_multi_subreddit_digest() {
        let run = sample_run(vec![
            (
                "rust".to_string(),
                vec![sample_item("abc123", "Some post title")],
            ),
            (
                "programming".to_string(),
                vec![sample_item("def456", "Another title")],
            ),
        ]);

        let note = render_digest_note(&run);

        assert!(note.starts_with("---\n"));
        assert!(note.contains("createdOn: \"2026-07-08T14:32:10Z\""));
        assert!(note.contains("modifiedOn: \"2026-07-08T14:32:10Z\""));
        assert!(note.contains("subreddits: [rust, programming]"));
        assert!(note.contains("sort: top"));
        assert!(note.contains("time_filter: day"));
        assert!(note.contains("fetched_count: 2"));
        assert!(note.contains("## r/rust"));
        assert!(note.contains("## r/programming"));
        assert!(
            note.contains("**[Some post title](https://reddit.com/r/rust/comments/abc123/post/)**")
        );
        assert!(note.contains("42 pts · 5 comments · u/someone"));
        assert!(note.contains("**Subreddits:** r/rust, r/programming"));
        assert!(note.contains("**Sort:** top (day)"));
        assert!(note.contains("**Query:** —"));
    }

    #[test]
    fn truncates_long_selftext_excerpt_without_raw_newlines() {
        let mut item = sample_item("abc123", "Long post");
        let long_text = "a".repeat(50) + "\nline two\n" + &"b".repeat(200);
        item.summary = Some(long_text);

        let run = sample_run(vec![("rust".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        assert!(
            note.contains('…'),
            "expected an ellipsis marking truncation:\n{note}"
        );
        // The excerpt line itself must not contain a raw newline (only the
        // note's own line breaks, which are between markdown elements, not
        // inside the excerpt text).
        let excerpt_line = note
            .lines()
            .find(|l| l.trim_start().starts_with('>'))
            .expect("expected a blockquote excerpt line");
        assert!(!excerpt_line.contains('\n'));
        assert!(excerpt_line.starts_with("   > "));
    }

    #[test]
    fn escapes_square_brackets_in_post_titles() {
        let item = sample_item("abc123", "Post with [brackets] in title");
        let run = sample_run(vec![("rust".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        assert!(note.contains("Post with \\[brackets\\] in title"));
        assert!(!note.contains("[Post with [brackets]"));
    }

    #[test]
    fn marks_nsfw_posts() {
        let mut item = sample_item("abc123", "NSFW post");
        item.nsfw = true;
        let run = sample_run(vec![("rust".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        assert!(note.contains("1. ⚠️ NSFW **[NSFW post]"));
    }

    #[test]
    fn reddit_author_gets_u_prefix_but_non_reddit_author_does_not() {
        let reddit_item = sample_item("abc123", "A reddit post");
        let mut rss_item = sample_item("def456", "An rss entry");
        rss_item.author = Some("Jane Blogger".to_string());

        let run = DigestRun {
            sort: Sort::Top,
            time: Some(TimeFilter::Day),
            query: None,
            tags: vec![],
            items_by_source: vec![
                (
                    SourceGroup {
                        kind: "reddit".to_string(),
                        name: "rust".to_string(),
                    },
                    vec![reddit_item],
                ),
                (
                    SourceGroup {
                        kind: "rss".to_string(),
                        name: "rust-blog".to_string(),
                    },
                    vec![rss_item],
                ),
            ],
            profile: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 8, 14, 32, 10).unwrap(),
        };
        let note = render_digest_note(&run);

        assert!(
            note.contains("u/someone"),
            "reddit author should keep the u/ prefix:\n{note}"
        );
        assert!(
            note.contains("Jane Blogger") && !note.contains("u/Jane Blogger"),
            "non-reddit author should render without the u/ prefix:\n{note}"
        );
    }

    #[test]
    fn reddit_author_with_pre_existing_u_prefix_from_the_rss_feed_is_not_doubled() {
        // Reddit's own Atom/RSS feed's `<author><name>` field already
        // includes the `/u/` prefix -- this must render as `u/llogiq`, not
        // `u//u/llogiq`.
        let mut item = sample_item("abc123", "A reddit-feed-sourced post");
        item.author = Some("/u/llogiq".to_string());

        let run = DigestRun {
            sort: Sort::Top,
            time: Some(TimeFilter::Day),
            query: None,
            tags: vec![],
            items_by_source: vec![(
                SourceGroup {
                    kind: "reddit".to_string(),
                    name: "rust".to_string(),
                },
                vec![item],
            )],
            profile: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 8, 14, 32, 10).unwrap(),
        };
        let note = render_digest_note(&run);

        assert!(note.contains("u/llogiq"), "expected u/llogiq in:\n{note}");
        assert!(
            !note.contains("u//u/llogiq")
                && !note.contains("u/u/llogiq")
                && !note.contains("u//u/"),
            "author prefix must not be doubled:\n{note}"
        );
    }

    #[test]
    fn query_and_time_filter_render_null_when_absent_and_value_when_present() {
        let run_without = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);
        let mut run_with = run_without.clone();
        run_with.query = Some("foo bar".to_string());

        let note_without = render_digest_note(&run_without);
        assert!(note_without.contains("time_filter: day"));
        assert!(note_without.contains("query: null"));
        assert!(note_without.contains("**Query:** —"));

        let mut run_no_time = run_without.clone();
        run_no_time.time = None;
        let note_no_time = render_digest_note(&run_no_time);
        assert!(note_no_time.contains("time_filter: null"));

        let note_with = render_digest_note(&run_with);
        assert!(note_with.contains("query: \"foo bar\""));
        assert!(note_with.contains("**Query:** foo bar"));
    }

    #[test]
    fn tags_are_deduped_and_include_per_subreddit_tags() {
        let mut run = sample_run(vec![
            ("rust".to_string(), vec![sample_item("a", "t")]),
            ("rust".to_string(), vec![sample_item("b", "t2")]),
        ]);
        run.tags = vec!["dev".to_string(), "reddit".to_string()];

        let tags = run.all_tags();
        assert_eq!(
            tags,
            vec![
                "reddit".to_string(),
                "reddit/rust".to_string(),
                "dev".to_string()
            ]
        );
    }

    #[test]
    fn filename_is_sanitized_and_uses_profile_label_when_present() {
        let mut run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);
        run.profile = Some("weekly: digest".to_string());

        let filename = digest_filename(&run);
        assert!(!filename.contains(':'));
        assert!(filename.contains("weekly- digest"));
        assert!(filename.ends_with(".md"));
    }

    #[test]
    fn filename_joins_subreddits_when_no_profile() {
        let run = sample_run(vec![
            ("rust".to_string(), vec![sample_item("a", "t")]),
            ("programming".to_string(), vec![sample_item("b", "t2")]),
        ]);

        let filename = digest_filename(&run);
        assert!(filename.contains("rust, programming"));
    }

    #[test]
    fn writes_note_to_expected_sanitized_path_under_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);

        let path = write_digest_note(dir.path(), "Resources/Reddit", &run)
            .expect("write_digest_note should succeed");

        assert!(path.exists());
        assert_eq!(path.parent().unwrap(), dir.path().join("Resources/Reddit"));

        let filename = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(filename, digest_filename(&run));

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, render_digest_note(&run));
    }

    #[test]
    fn renders_an_untitled_placeholder_instead_of_a_dead_link_for_empty_title_and_url() {
        let mut item = sample_item("empty1", "");
        item.url = String::new();
        item.summary = None;

        let run = sample_run(vec![("rss-feed".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        assert!(
            note.contains("(untitled)"),
            "expected the untitled placeholder in the rendered note:\n{note}"
        );
        assert!(
            !note.contains("[]("),
            "must not render a dead markdown link for an empty title/url item:\n{note}"
        );
    }

    #[test]
    fn write_digest_note_creates_missing_folders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);

        let nested_folder = "Deeply/Nested/Reddit";
        let path = write_digest_note(dir.path(), nested_folder, &run)
            .expect("write_digest_note should create missing folders");

        assert!(path.exists());
        assert!(dir.path().join(nested_folder).is_dir());
    }
}
