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
use crate::types::{Sort, SourceKind, TimeFilter};

/// Characters that are unsafe (or at least unwelcome) in filenames across
/// the platforms an Obsidian vault might live on. Any of these, wherever
/// they show up in a computed filename, get replaced with `-`.
const UNSAFE_FILENAME_CHARS: [char; 9] = [':', '/', '\\', '*', '?', '"', '<', '>', '|'];

/// Identifies one group of items in a [`DigestRun`]: which source kind it
/// came from ([`SourceKind::Reddit`]/`Rss`/`Youtube`), the group's display
/// name (a subreddit name, for Reddit), and the topic it belongs to (bd issue
/// drip-38w.1: every source belongs to exactly one topic). Rendering picks
/// source-kind-specific formatting (e.g. `### r/{name}` for Reddit) based on
/// `kind`, and groups sources under a `## {topic}` heading based on `topic`
/// (bd issue drip-38w.3).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceGroup {
    pub kind: SourceKind,
    pub name: String,
    pub topic: String,
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
    /// the note. Rendering further groups these by [`SourceGroup::topic`]
    /// (bd issue drip-38w.3), but the per-source order within a topic, and
    /// the items within a source, are taken from this field's order as-is.
    pub items_by_source: Vec<(SourceGroup, Vec<Item>)>,
    /// The topic name used for this run (via `--topic`), if any. Used as
    /// the digest's filename label instead of joining source names, when
    /// available.
    pub topic: Option<String>,
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

    /// Distinct topics referenced by `items_by_source`'s groups, in
    /// first-seen order. Drives the `topics:` frontmatter key and the
    /// `## {topic}` body headings (bd issue drip-38w.3) -- drip is no longer
    /// Reddit-only, so topics (not subreddits) are the note's top-level
    /// grouping.
    pub fn topics(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.items_by_source
            .iter()
            .map(|(group, _)| group.topic.clone())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// The user-supplied tags (e.g. from `--tag`, plus `settings`'s
    /// `default_tags`), deduplicated while preserving first-seen order.
    /// Drip is no longer Reddit-only (bd issue drip-38w.3), so this no
    /// longer adds the `reddit`/`reddit/{name}` tags it used to -- a note
    /// pulling in RSS/YouTube sources shouldn't be tagged `reddit` at all.
    fn all_tags(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.tags
            .iter()
            .cloned()
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// The filename label: the topic name if one was used for this run,
    /// otherwise a comma-joined, trimmed list of source names.
    fn label(&self) -> String {
        match &self.topic {
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
    let raw = format!("{local_ts} - drip digest ({}).md", run.label());
    sanitize_filename(&raw)
}

/// Escape `[` and `]` in an item's title so it can't break `[title](url)`
/// markdown link syntax when embedded in one.
fn escape_title(title: &str) -> String {
    title.replace('[', "\\[").replace(']', "\\]")
}

/// Render one item as a single Obsidian checkbox-task line (no trailing
/// blank line, no numbering): `- [ ] {nsfw}{heading}{author_suffix}`.
/// `source_kind` picks the author formatting: Reddit's `u/{name}` convention
/// only makes sense for Reddit usernames, not RSS/YouTube author names
/// (drip-01b). Score/comment-count/flair/summary are no longer rendered
/// (bd issue drip-38w.3 replaced the old numbered-list-with-metadata format
/// with a flat checklist) -- the NSFW marker is the one piece of metadata
/// kept, since it's a content warning rather than decoration.
fn render_item(item: &Item, source_kind: SourceKind) -> String {
    let nsfw = if item.nsfw { "⚠️ NSFW " } else { "" };
    let title = escape_title(&item.title);

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

    let author_suffix = match &item.author {
        Some(author) if source_kind.is_reddit() => {
            // Reddit's own Atom/RSS feed's `<author><name>` field already
            // has the `/u/` prefix baked in (e.g. "/u/llogiq"), so strip any
            // pre-existing `/u/`/`u/` prefix before re-adding the canonical
            // one, to avoid a doubled `u//u/llogiq`. This is defensive --
            // kept in case any future `--kind reddit` source ever supplies a
            // bare username instead.
            let clean = author.trim_start_matches("/u/").trim_start_matches("u/");
            format!(" — u/{clean}")
        }
        Some(author) => format!(" — {author}"),
        None => String::new(),
    };

    format!("- [ ] {nsfw}{heading}{author_suffix}")
}

/// Pure rendering: given a `DigestRun`, produce the full markdown note text
/// (frontmatter + body). Does no I/O, which keeps it cheap to unit test.
pub fn render_digest_note(run: &DigestRun) -> String {
    let created_iso = run.created_at.format("%Y-%m-%dT%H:%M:%SZ");
    let tags = run.all_tags();
    // Empty tag set -> inline `tags: []` rather than a bare `tags:` key with
    // a blank line under it, which is malformed-looking YAML. Non-empty ->
    // the usual block-sequence form. In practice `default_tags` seeds at
    // least `drip`, so the empty case only arises if a user clears their tag
    // settings and passes no `--tag`.
    let tags_yaml = if tags.is_empty() {
        "tags: []\n".to_string()
    } else {
        let block = tags
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("tags:\n{block}\n")
    };

    let topics_list = run.topics().join(", ");
    let sources_list = run.source_labels().join(", ");
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
    out.push_str(&tags_yaml);
    out.push_str(&format!("createdOn: \"{created_iso}\"\n"));
    out.push_str(&format!("modifiedOn: \"{created_iso}\"\n"));
    out.push_str(&format!("topics: [{topics_list}]\n"));
    out.push_str(&format!("sources: [{sources_list}]\n"));
    out.push_str(&format!("sort: {}\n", run.sort.as_str()));
    out.push_str(&format!("time_filter: {time_filter_yaml}\n"));
    out.push_str(&format!("query: {query_yaml}\n"));
    out.push_str(&format!("fetched_count: {}\n", run.fetched_count()));
    out.push_str("---\n\n");

    let local_ts = run
        .created_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M");
    out.push_str(&format!("# drip digest — {local_ts}\n\n"));

    let source_labels_display = run
        .items_by_source
        .iter()
        .map(|(group, _)| group.kind.heading_prefix(&group.name))
        .collect::<Vec<_>>()
        .join(", ");
    let sort_label = match run.time {
        Some(t) => format!("{} ({})", run.sort.as_str(), t.as_str()),
        None => run.sort.as_str().to_string(),
    };
    let query_label = run.query.as_deref().unwrap_or("—");
    out.push_str(&format!(
        "**Sources:** {source_labels_display} · **Sort:** {sort_label} · **Query:** {query_label}\n\n"
    ));

    // Body grouping (bd issue drip-38w.3): distinct topics, in first-seen
    // order, each an H2; under each topic, its source groups (in their
    // existing `items_by_source` order) each an H3; under each source, its
    // items in feed order as flat checkbox lines. `topics()` already
    // computes the first-seen topic order -- for each topic, filter
    // `items_by_source` down to the groups belonging to it, which preserves
    // their relative order since the filter is a stable pass over the
    // original `Vec`.
    for topic in run.topics() {
        out.push_str(&format!("## {topic}\n\n"));
        for (group, items) in &run.items_by_source {
            if group.topic != topic {
                continue;
            }
            out.push_str(&format!("### {}\n\n", group.kind.heading_prefix(&group.name)));
            for item in items {
                out.push_str(&render_item(item, group.kind));
                out.push('\n');
            }
            out.push('\n');
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

    /// Build a `DigestRun` from `(name, items)` pairs, each a Reddit-origin
    /// group under a shared `"Programming"` topic -- the common case most
    /// tests below need. Use [`sample_run_with_topics`] instead when a test
    /// needs distinct topics or non-Reddit kinds.
    fn sample_run(items_by_subreddit: Vec<(String, Vec<Item>)>) -> DigestRun {
        sample_run_with_topics(
            items_by_subreddit
                .into_iter()
                .map(|(name, items)| ("Programming".to_string(), name, items))
                .collect(),
        )
    }

    /// Build a `DigestRun` from `(topic, name, items)` triples, each a
    /// Reddit-origin group -- for tests that need multiple distinct topics.
    fn sample_run_with_topics(items: Vec<(String, String, Vec<Item>)>) -> DigestRun {
        let items_by_source = items
            .into_iter()
            .map(|(topic, name, items)| {
                (
                    SourceGroup {
                        kind: SourceKind::Reddit,
                        name,
                        topic,
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
            topic: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 8, 14, 32, 10).unwrap(),
        }
    }

    #[test]
    fn demo_full_featured_rendering_sample() {
        let self_post = sample_item("abc123", "A [neat] discovery about lifetimes");

        let mut link_post = sample_item("def456", "Another title");
        link_post.comments_url =
            Some("https://reddit.com/r/rust/comments/def456/post/".to_string());
        link_post.url = "https://example.com/thing".to_string();
        link_post.author = Some("someone".to_string());

        let mut nsfw_post = sample_item("ghi789", "A spicy post");
        nsfw_post.nsfw = true;

        let run = sample_run(vec![(
            "rust".to_string(),
            vec![self_post, link_post, nsfw_post],
        )]);
        let note = render_digest_note(&run);

        assert!(note.contains("A \\[neat\\] discovery about lifetimes"));
        assert!(note.contains("⚠️ NSFW **[A spicy post]"));
        // Exactly one blank line between the query summary line and the
        // first topic heading, between the topic and source headings, and
        // between sections generally.
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
        assert!(note.contains("topics: [Programming]"));
        assert!(note.contains("sources: [rust, programming]"));
        assert!(note.contains("sort: top"));
        assert!(note.contains("time_filter: day"));
        assert!(note.contains("fetched_count: 2"));
        assert!(note.contains("## Programming"));
        assert!(note.contains("### r/rust"));
        assert!(note.contains("### r/programming"));
        assert!(
            note.contains("**[Some post title](https://reddit.com/r/rust/comments/abc123/post/)**")
        );
        assert!(note.contains("u/someone"));
        assert!(note.contains("**Sources:** r/rust, r/programming"));
        assert!(note.contains("**Sort:** top (day)"));
        assert!(note.contains("**Query:** —"));
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

        assert!(note.contains("- [ ] ⚠️ NSFW **[NSFW post]"));
    }

    #[test]
    fn checkbox_lines_start_with_exactly_dash_space_bracket_space_bracket_space() {
        let item = sample_item("abc123", "A post");
        let run = sample_run(vec![("rust".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        let item_line = note
            .lines()
            .find(|l| l.contains("A post"))
            .expect("expected a line containing the post title");
        assert!(
            item_line.starts_with("- [ ] "),
            "checkbox line must start with exactly '- [ ] ':\n{item_line}"
        );
    }

    #[test]
    fn item_with_no_author_renders_with_no_trailing_dash_suffix() {
        let mut item = sample_item("abc123", "Authorless post");
        item.author = None;
        let run = sample_run(vec![("rust".to_string(), vec![item])]);
        let note = render_digest_note(&run);

        let item_line = note
            .lines()
            .find(|l| l.contains("Authorless post"))
            .expect("expected a line containing the post title");
        assert_eq!(
            item_line,
            "- [ ] **[Authorless post](https://reddit.com/r/rust/comments/abc123/post/)**",
            "no author must not leave a trailing ' — ' suffix"
        );
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
                        kind: SourceKind::Reddit,
                        name: "rust".to_string(),
                        topic: "Programming".to_string(),
                    },
                    vec![reddit_item],
                ),
                (
                    SourceGroup {
                        kind: SourceKind::Rss,
                        name: "rust-blog".to_string(),
                        topic: "Programming".to_string(),
                    },
                    vec![rss_item],
                ),
            ],
            topic: None,
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
                    kind: SourceKind::Reddit,
                    name: "rust".to_string(),
                    topic: "Programming".to_string(),
                },
                vec![item],
            )],
            topic: None,
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
    fn tags_are_deduped_to_just_the_user_supplied_tags() {
        // bd issue drip-38w.3: drip is no longer Reddit-only, so tags are no
        // longer auto-populated with `reddit`/`reddit/{name}` -- only the
        // deduped user/default tags (e.g. `--tag`/`default_tags`) appear.
        let mut run = sample_run(vec![
            ("rust".to_string(), vec![sample_item("a", "t")]),
            ("rust".to_string(), vec![sample_item("b", "t2")]),
        ]);
        run.tags = vec!["dev".to_string(), "drip".to_string(), "dev".to_string()];

        let tags = run.all_tags();
        assert_eq!(tags, vec!["dev".to_string(), "drip".to_string()]);
    }

    #[test]
    fn rendered_note_tags_block_contains_only_user_tags() {
        let mut run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);
        run.tags = vec!["drip".to_string()];

        let note = render_digest_note(&run);

        assert!(note.contains("tags:\n  - drip\n"));
        // Check specifically the tags block (the note's title/URLs may
        // legitimately contain "reddit" elsewhere, e.g. reddit.com links).
        let tags_block = note
            .split("createdOn:")
            .next()
            .expect("expected a tags block before createdOn");
        assert!(
            !tags_block.contains("reddit"),
            "must not auto-tag the note `reddit`:\n{tags_block}"
        );
    }

    #[test]
    fn rendered_note_with_no_tags_uses_inline_empty_array() {
        // An empty tag set must render as `tags: []`, not a bare `tags:` key
        // with a blank line beneath it (malformed-looking frontmatter).
        let mut run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);
        run.tags = vec![];

        let note = render_digest_note(&run);

        assert!(note.contains("tags: []\n"), "expected inline empty tags array:\n{note}");
        assert!(!note.contains("tags:\n\n"), "must not emit a bare tags key with a blank line:\n{note}");
    }

    #[test]
    fn filename_is_sanitized_and_uses_topic_label_when_present() {
        let mut run = sample_run(vec![("rust".to_string(), vec![sample_item("a", "t")])]);
        run.topic = Some("weekly: digest".to_string());

        let filename = digest_filename(&run);
        assert!(!filename.contains(':'));
        assert!(filename.contains("weekly- digest"));
        assert!(filename.contains("drip digest"));
        assert!(filename.ends_with(".md"));
    }

    #[test]
    fn filename_joins_subreddits_when_no_topic() {
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

    #[test]
    fn two_sources_under_the_same_topic_render_under_one_topic_heading() {
        let run = sample_run_with_topics(vec![
            (
                "Programming".to_string(),
                "rust".to_string(),
                vec![sample_item("a", "Rust post")],
            ),
            (
                "Programming".to_string(),
                "golang".to_string(),
                vec![sample_item("b", "Go post")],
            ),
        ]);

        let note = render_digest_note(&run);

        assert_eq!(
            note.matches("## Programming").count(),
            1,
            "two sources under the same topic must share ONE topic heading:\n{note}"
        );
        assert!(note.contains("### r/rust"));
        assert!(note.contains("### r/golang"));
    }

    #[test]
    fn sources_under_different_topics_render_under_separate_headings_in_first_seen_order() {
        let run = sample_run_with_topics(vec![
            (
                "Claude".to_string(),
                "ClaudeCode".to_string(),
                vec![sample_item("a", "Claude post")],
            ),
            (
                "Rust".to_string(),
                "rust-hot".to_string(),
                vec![sample_item("b", "Rust post")],
            ),
        ]);

        let note = render_digest_note(&run);

        let h2_count = note.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(h2_count, 2, "expected exactly two H2 topic headings:\n{note}");
        let claude_idx = note.find("## Claude").expect("expected a Claude heading");
        let rust_idx = note.find("## Rust").expect("expected a Rust heading");
        assert!(
            claude_idx < rust_idx,
            "topics should appear in first-seen order:\n{note}"
        );
        assert!(note.contains("topics: [Claude, Rust]"));
    }

    #[test]
    fn renders_a_two_topic_three_source_sample() {
        // The exact scenario described in bd issue drip-38w.3's target
        // format: two topics, three sources, one item each.
        let run = sample_run_with_topics(vec![
            (
                "Claude".to_string(),
                "ClaudeCode".to_string(),
                vec![sample_item("a", "Anthropic ships MCP update")],
            ),
            (
                "Rust".to_string(),
                "rust-hot".to_string(),
                vec![sample_item("b", "Async traits stabilized")],
            ),
        ]);

        let note = render_digest_note(&run);

        assert!(note.contains("topics: [Claude, Rust]"));
        assert!(note.contains("sources: [ClaudeCode, rust-hot]"));
        assert!(note.contains("## Claude"));
        assert!(note.contains("### r/ClaudeCode"));
        assert!(note.contains("## Rust"));
        assert!(note.contains("### r/rust-hot"));
        assert!(!note.contains("\n\n\n"));
    }
}
