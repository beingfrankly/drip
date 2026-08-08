//! Builds "digest" notes: markdown files summarizing a batch of fetched
//! items, grouped by `(main topic, sub-topic)` (bd issue drip-98u.5/.6/
//! drip-ho5.6/.7), and written into the Obsidian vault.
//!
//! This module is split into a pure rendering half ([`render_digest_note`])
//! and a thin I/O half ([`write_digest_note`]) so the markdown/frontmatter
//! logic can be unit tested without touching the filesystem.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::classify::Section;
use crate::item::Item;
use crate::types::{Sort, SourceKind, TimeFilter};

/// Characters that are unsafe (or at least unwelcome) in filenames across
/// the platforms an Obsidian vault might live on. Any of these, wherever
/// they show up in a computed filename, get replaced with `-`.
const UNSAFE_FILENAME_CHARS: [char; 9] = [':', '/', '\\', '*', '?', '"', '<', '>', '|'];

/// Identifies one successfully-fetched source in a [`DigestRun`]: which
/// source kind it came from ([`SourceKind::Reddit`]/`Rss`/`Youtube`) and its
/// display label. Deliberately carries no topic (bd issue drip-98u.5): one
/// source can now route items into several different `(main topic,
/// sub-topic)` sections at once (via `topic_links`, see `src/classify.rs`),
/// so a single "this group's topic" field is no longer representable -- the
/// items themselves carry their own [`Section`] in `DigestRun::items_by_subtopic`
/// instead. `kind` is kept (rather than dropping this down to a bare
/// `String` label) purely so future kind-specific rendering has somewhere to
/// hang off of, even though bd issue drip-98u.12 removed the one rendering
/// decision (`r/{name}` vs bare `{name}`) that used to depend on it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceGroup {
    pub kind: SourceKind,
    pub name: String,
}

/// Everything needed to render (and name) a single digest note: which
/// sources were fetched and with what parameters, the tags to apply, the
/// classified items, and when the run happened.
#[derive(Debug, Clone)]
pub struct DigestRun {
    pub sort: Sort,
    pub time: Option<TimeFilter>,
    pub query: Option<String>,
    /// User-supplied tags (e.g. from `--tag`), on top of the `reddit` /
    /// `reddit/{subreddit}` tags this module adds automatically for
    /// Reddit-origin groups.
    pub tags: Vec<String>,
    /// Classified items, grouped into `(main topic, sub-topic)` sections (bd
    /// issue drip-ho5.6's pipeline classifies every fetched item via
    /// `classify::classify_items` before this run is built), in first-seen
    /// section order. Rendering groups these into `## {main_topic}` (H2)
    /// then `### {sub_topic}` (H3) -- no source heading, no author suffix
    /// (the pre-charting note-shape decision + bd issue drip-98u.6). The
    /// item order *within* a section is feed order, concatenated across
    /// every source that routed into it. An entry with an empty item `Vec`
    /// is never rendered (bd issue drip-98u.6, point 2: "empty sub-topics
    /// are omitted") -- callers should avoid constructing one, but rendering
    /// skips it defensively either way.
    pub items_by_subtopic: Vec<(Section, Vec<Item>)>,
    /// Every source that fetched successfully this run (bd issue
    /// drip-98u.5), independent of `items_by_subtopic` -- a source whose
    /// items were all dropped/excluded/already-seen still belongs here, so
    /// `sources:`/`**Sources:**` means "what this run looked at", not "where
    /// these items came from". Not required to be pre-deduped by the caller;
    /// [`DigestRun::source_groups`]/[`DigestRun::source_labels`] dedupe on
    /// read.
    pub sources: Vec<SourceGroup>,
    pub created_at: DateTime<Utc>,
}

impl DigestRun {
    /// Every source in [`Self::sources`], deduped by `(kind, name)` while
    /// preserving first-seen order.
    pub fn source_groups(&self) -> Vec<SourceGroup> {
        let mut seen: HashSet<(SourceKind, String)> = HashSet::new();
        self.sources
            .iter()
            .filter(|group| seen.insert((group.kind, group.name.clone())))
            .cloned()
            .collect()
    }

    /// The display label of each deduped source (see [`Self::source_groups`]),
    /// in first-seen order -- drives the `sources:` frontmatter key and the
    /// `**Sources:**` line, both of which want the bare label with no `r/`
    /// prefix (bd issue drip-98u.12).
    pub fn source_labels(&self) -> Vec<String> {
        self.source_groups()
            .into_iter()
            .map(|group| group.name)
            .collect()
    }

    /// Total rendered checkbox-line count across every section -- counts
    /// LINES, not distinct items (bd issue drip-98u.6's resolution,
    /// deliberately diverging from `fetch_runs.post_count`'s distinct-item
    /// count, drip-98u.5): an item that multi-matches into two sub-topics
    /// renders twice and is counted twice here.
    fn fetched_count(&self) -> usize {
        self.items_by_subtopic
            .iter()
            .map(|(_, items)| items.len())
            .sum()
    }

    /// Distinct main topics referenced by `items_by_subtopic`, in
    /// first-seen order. Drives the `topics:` frontmatter key and the `##
    /// {main_topic}` body headings.
    pub fn topics(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.items_by_subtopic
            .iter()
            .map(|(section, _)| section.main_topic.clone())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// Distinct sub-topics referenced by `items_by_subtopic`, in first-seen
    /// order -- drives the new `subtopics:` frontmatter key (bd issue
    /// drip-98u.6). Two different main topics sharing a same-named sub-topic
    /// each contribute their own entry here (this is a flat list of names,
    /// not scoped per main topic); the body rendering itself never confuses
    /// them, since it always re-checks `section.main_topic` alongside
    /// `section.sub_topic`.
    pub fn subtopics(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.items_by_subtopic
            .iter()
            .map(|(section, _)| section.sub_topic.clone())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// The user-supplied tags (e.g. from `--tag`, plus `settings`'s
    /// `default_tags`), deduplicated while preserving first-seen order.
    /// Drip is no longer Reddit-only (bd issue drip-38w.3), so this no
    /// longer adds the `reddit`/`reddit/{name}` tags it used to -- a note
    /// pulling in RSS/YouTube sources shouldn't be tagged `reddit` at all.
    fn all_tags(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.tags
            .iter()
            .cloned()
            .filter(|t| seen.insert(t.clone()))
            .collect()
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
/// any directory) for this digest run: the local-timezone ISO date plus a
/// "Daily digest" suffix -- no time-of-day, no topic/source parenthetical.
pub fn digest_filename(run: &DigestRun) -> String {
    let date = run.created_at.with_timezone(&Local).format("%Y-%m-%d");
    sanitize_filename(&format!("{date} - Daily digest.md"))
}

/// Escape `[` and `]` in an item's title so it can't break `[title](url)`
/// markdown link syntax when embedded in one.
fn escape_title(title: &str) -> String {
    title.replace('[', "\\[").replace(']', "\\]")
}

/// Render one item as a single Obsidian checkbox-task line (no trailing
/// blank line, no numbering): `- [ ] {nsfw}{heading}`. Score/comment-count/
/// flair/summary/author are not rendered (the pre-charting note-shape
/// decision, bd issue drip-98u.6: title-only linked checkbox lines, no
/// source headings, no author suffix) -- the NSFW marker is the one piece of
/// metadata kept, since it's a content warning rather than decoration.
fn render_item(item: &Item) -> String {
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

    format!("- [ ] {nsfw}{heading}")
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
    let subtopics_list = run.subtopics().join(", ");
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
    out.push_str(&format!("subtopics: [{subtopics_list}]\n"));
    out.push_str(&format!("sources: [{sources_list}]\n"));
    out.push_str(&format!("sort: {}\n", run.sort.as_str()));
    out.push_str(&format!("time_filter: {time_filter_yaml}\n"));
    out.push_str(&format!("query: {query_yaml}\n"));
    out.push_str(&format!("fetched_count: {}\n", run.fetched_count()));
    out.push_str("---\n\n");

    let local_date = run.created_at.with_timezone(&Local).format("%Y-%m-%d");
    out.push_str(&format!("# {local_date} - Daily digest\n\n"));

    let sort_label = match run.time {
        Some(t) => format!("{} ({})", run.sort.as_str(), t.as_str()),
        None => run.sort.as_str().to_string(),
    };
    let query_label = run.query.as_deref().unwrap_or("—");
    out.push_str(&format!(
        "**Sources:** {sources_list} · **Sort:** {sort_label} · **Query:** {query_label}\n\n"
    ));

    // Body grouping (bd issue drip-98u.5/.6): distinct main topics, in
    // first-seen order, each an H2; under each main topic, its sub-topics
    // (in `items_by_subtopic`'s existing order) each an H3; under each
    // sub-topic, its items in feed order as flat checkbox lines. Empty
    // sections are skipped (drip-98u.6, point 2) rather than rendering a
    // heading with nothing under it.
    for topic in run.topics() {
        out.push_str(&format!("## {topic}\n\n"));
        for (section, items) in &run.items_by_subtopic {
            if section.main_topic != topic || items.is_empty() {
                continue;
            }
            out.push_str(&format!("### {}\n\n", section.sub_topic));
            for item in items {
                out.push_str(&render_item(item));
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

    // If a note for this day already exists, MERGE the run's new items into
    // it rather than overwriting -- so a second same-day run accumulates
    // items and never clobbers the first run's (or the user's hand-edits),
    // bd issue drip-47u. The first run of a day renders a fresh note.
    let content = match fs::read_to_string(&path) {
        Ok(existing) => merge_digest_note(&existing, run),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => render_digest_note(run),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to read existing digest note at {}", path.display())
            })
        }
    };

    fs::write(&path, content)
        .with_context(|| format!("failed to write digest note at {}", path.display()))?;

    Ok(path)
}

/// Compute what [`write_digest_note`] WOULD write for `run` right now,
/// without touching disk: the merge of `run` into the existing same-day note
/// if one exists, else a fresh render. Backs `drip fetch --dry-run`'s preview
/// so it reflects the append/merge behavior (bd issue drip-47u).
pub fn preview_digest_note(
    vault_path: &Path,
    posts_folder: &str,
    run: &DigestRun,
) -> Result<String> {
    let path = vault_path.join(posts_folder).join(digest_filename(run));
    match fs::read_to_string(&path) {
        Ok(existing) => Ok(merge_digest_note(&existing, run)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(render_digest_note(run)),
        Err(err) => Err(err)
            .with_context(|| format!("failed to read existing digest note at {}", path.display())),
    }
}

/// True if `line` (ignoring leading whitespace) is a rendered checkbox task
/// line -- `- [ ] ...`, or the user-ticked `- [x] ...`/`- [X] ...`. Used by
/// the merge logic to find where a sub-topic's item lines end and to guard
/// against re-inserting an item already in the note (bd issue drip-47u).
fn is_item_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- [ ] ") || t.starts_with("- [x] ") || t.starts_with("- [X] ")
}

/// The comparable "body" of a checkbox task line: the text after the
/// `- [ ] `/`- [x] `/`- [X] ` marker. Lets a freshly-rendered `- [ ]` item
/// and the same item the user has since ticked (`- [x]`) compare equal, so
/// merging never duplicates an item just because its checkbox state changed.
/// Non-item lines are returned unchanged.
fn checkbox_body(line: &str) -> &str {
    let t = line.trim_start();
    for marker in ["- [ ] ", "- [x] ", "- [X] "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return rest;
        }
    }
    line
}

/// Insert `item_lines` into `lines` under the `## {main topic}` / `### {sub
/// topic}` headings given, creating either heading (and its section) if
/// absent, and return how many lines were actually inserted. Mirrors
/// `journal::insert_reddit_bullet`'s targeted, parse-free line surgery: it
/// only ever inserts new item lines and (when needed) new headings, and never
/// rewrites, reorders, or drops an existing line -- so ticked checkboxes and
/// manual edits between runs survive untouched (bd issue drip-47u). Items
/// whose `checkbox_body` already appears in the target sub-topic subsection
/// are skipped, keeping a re-run from duplicating a line.
///
/// Re-parameterized from `(topic, source)` to `(main topic, sub-topic)` (bd
/// issue drip-98u.6/drip-ho5.7) -- depth is unchanged, so the two properties
/// this relies on carry over unchanged too: the `### {sub_topic}` search
/// below is scoped to within its `## {main_topic}`'s own line range (so a
/// same-named sub-topic under a different main topic can never collide), and
/// the "already present" `HashSet` a few lines down is built fresh per
/// subsection (so the same item rendered under two different sub-topics is
/// never mistaken for a duplicate of itself). Both confirmed by test, not
/// merely inherited by assumption -- see
/// `merge_does_not_collide_two_identically_named_subtopics_under_different_main_topics`
/// and `merge_does_not_treat_the_same_item_rendered_under_two_subtopics_as_a_collision`
/// below.
fn insert_item_lines(
    lines: &mut Vec<String>,
    topic_heading: &str,
    subtopic_heading: &str,
    item_lines: &[String],
) -> usize {
    // 1. Locate the `## {main topic}` section, or append a fresh one at EOF.
    let topic_idx = lines.iter().position(|l| l == topic_heading);
    let (topic_body_start, topic_end) = match topic_idx {
        Some(idx) => {
            let next_h2 = lines[idx + 1..]
                .iter()
                .position(|l| l.starts_with("## "))
                .map(|off| idx + 1 + off);
            (idx + 1, next_h2.unwrap_or(lines.len()))
        }
        None => {
            if lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            lines.push(topic_heading.to_string());
            lines.push(String::new());
            lines.push(subtopic_heading.to_string());
            lines.push(String::new());
            let mut inserted = 0;
            for il in item_lines {
                lines.push(il.clone());
                inserted += 1;
            }
            return inserted;
        }
    };

    // 2. Within the topic section, locate the `### {sub topic}` subsection,
    //    or splice a fresh one in at the topic section's end.
    let subtopic_idx = lines[topic_body_start..topic_end]
        .iter()
        .position(|l| l == subtopic_heading)
        .map(|off| topic_body_start + off);
    let (sub_start, sub_end) = match subtopic_idx {
        Some(idx) => {
            let next = lines[idx + 1..topic_end]
                .iter()
                .position(|l| l.starts_with("### ") || l.starts_with("## "))
                .map(|off| idx + 1 + off);
            (idx + 1, next.unwrap_or(topic_end))
        }
        None => {
            let insert_at = topic_end;
            let mut block: Vec<String> = Vec::new();
            if insert_at > 0 && !lines[insert_at - 1].is_empty() {
                block.push(String::new());
            }
            block.push(subtopic_heading.to_string());
            block.push(String::new());
            let mut inserted = 0;
            for il in item_lines {
                block.push(il.clone());
                inserted += 1;
            }
            // Keep a blank line between the new subsection and whatever
            // section follows it (e.g. the next `## {main topic}`).
            if insert_at < lines.len() {
                block.push(String::new());
            }
            for (k, line) in block.into_iter().enumerate() {
                lines.insert(insert_at + k, line);
            }
            return inserted;
        }
    };

    // 3. Insert into the existing subsection, after its last item line (or
    //    right after the heading's blank line if it has none yet), skipping
    //    items already present by `checkbox_body`. This `HashSet` is scoped
    //    to `sub_start..sub_end` -- i.e. rebuilt fresh for every subsection
    //    -- which is what makes the same item under a second, different
    //    sub-topic heading never look like an already-present duplicate.
    let existing_bodies: std::collections::HashSet<String> = lines[sub_start..sub_end]
        .iter()
        .filter(|l| is_item_line(l))
        .map(|l| checkbox_body(l).to_string())
        .collect();

    let last_item = (sub_start..sub_end)
        .rev()
        .find(|&i| is_item_line(&lines[i]));
    let mut insert_at = match last_item {
        Some(i) => i + 1,
        None => {
            if sub_start < sub_end && lines[sub_start].is_empty() {
                sub_start + 1
            } else {
                sub_start
            }
        }
    };

    let mut inserted = 0;
    for il in item_lines {
        if existing_bodies.contains(checkbox_body(il)) {
            continue;
        }
        lines.insert(insert_at, il.clone());
        insert_at += 1;
        inserted += 1;
    }
    inserted
}

/// Merge `run`'s items into an already-existing same-day digest note's text,
/// returning the updated note. Unlike [`render_digest_note`] (which produces
/// a fresh note from scratch), this preserves the existing note verbatim --
/// including ticked checkboxes (`- [x]`) and any manual edits the user made
/// between runs -- and only *inserts* genuinely-new item lines under the
/// right `## {main topic}` / `### {sub topic}` headings, creating those
/// headings as needed (bd issue drip-47u). It also bumps `modifiedOn`, grows
/// `fetched_count` by the number of lines inserted, and extends the
/// `topics:`/`subtopics:`/`sources:` frontmatter lists to cover any
/// newly-introduced main topic, sub-topic, or source. If nothing new is
/// inserted, the note is returned byte-for-byte unchanged.
pub fn merge_digest_note(existing: &str, run: &DigestRun) -> String {
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();

    let mut total_inserted = 0usize;
    for (section, items) in &run.items_by_subtopic {
        if items.is_empty() {
            continue;
        }
        let topic_heading = format!("## {}", section.main_topic);
        let subtopic_heading = format!("### {}", section.sub_topic);
        let item_lines: Vec<String> = items.iter().map(render_item).collect();
        total_inserted +=
            insert_item_lines(&mut lines, &topic_heading, &subtopic_heading, &item_lines);
    }

    // Nothing genuinely new -> leave the note exactly as it was (no
    // frontmatter churn, no rewritten trailing newline).
    if total_inserted == 0 {
        return existing.to_string();
    }

    // Targeted frontmatter updates -- single-line rewrites only, in the
    // spirit of `journal::bump_modified_on`.
    let modified_iso = run.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    for line in lines.iter_mut() {
        if line.starts_with("modifiedOn: \"") && line.ends_with('"') {
            *line = format!("modifiedOn: \"{modified_iso}\"");
            break;
        }
    }
    for line in lines.iter_mut() {
        if let Some(rest) = line.strip_prefix("fetched_count: ") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                *line = format!("fetched_count: {}", n + total_inserted);
            }
            break;
        }
    }
    extend_inline_list(&mut lines, "topics: [", &run.topics());
    extend_inline_list(&mut lines, "subtopics: [", &run.subtopics());
    extend_inline_list(&mut lines, "sources: [", &run.source_labels());

    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Extend an inline frontmatter list line (`topics: [a, b]` / `sources: [...]`)
/// with any of `additions` not already present, preserving order and the
/// existing entries. A no-op if the line isn't found. Used by
/// [`merge_digest_note`] so a main topic/sub-topic/source first introduced by
/// a later same-day run still shows up in the note's frontmatter.
fn extend_inline_list(lines: &mut [String], prefix: &str, additions: &[String]) {
    for line in lines.iter_mut() {
        if let Some(rest) = line.strip_prefix(prefix) {
            let inner = rest.trim_end().strip_suffix(']').unwrap_or(rest);
            let mut current: Vec<String> = if inner.trim().is_empty() {
                Vec::new()
            } else {
                inner.split(", ").map(|s| s.to_string()).collect()
            };
            for add in additions {
                if !current.iter().any(|c| c == add) {
                    current.push(add.clone());
                }
            }
            *line = format!("{prefix}{}]", current.join(", "));
            break;
        }
    }
}

/// Whether TODAY's already-written digest note (if one exists) contains a
/// section heading -- H2 `## {name}` or H3 `### {name}` -- for `name` (bd
/// issue drip-ho5.8): backs `drip topic rename`/the reparent verb's
/// "warn, don't rewrite" behavior (per drip-98u.7's resolution). Headings
/// are located by exact full-line equality, mirroring `insert_item_lines`'s
/// own matching -- a same-day rename means the next fetch cannot find the
/// old heading and appends a fresh one alongside it instead, which is
/// confusing but not destructive (and gone by tomorrow), so this function
/// only reports the fact; it never rewrites the note itself. Returns
/// `Ok(false)` (not an error) when there's no note for today yet.
pub fn todays_note_has_heading_for(
    vault_path: &Path,
    posts_folder: &str,
    name: &str,
) -> Result<bool> {
    let filename = sanitize_filename(&format!(
        "{} - Daily digest.md",
        Local::now().format("%Y-%m-%d")
    ));
    let path = vault_path.join(posts_folder).join(filename);

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to read today's digest note at {}", path.display())
            })
        }
    };

    let h2 = format!("## {name}");
    let h3 = format!("### {name}");
    Ok(content.lines().any(|l| l == h2 || l == h3))
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

    fn section(main_topic: &str, sub_topic: &str) -> Section {
        Section {
            main_topic: main_topic.to_string(),
            sub_topic: sub_topic.to_string(),
        }
    }

    /// Build a `DigestRun` from `(main_topic, sub_topic, items)` triples.
    /// One `SourceGroup` is synthesized per triple, labeled after `sub_topic`
    /// -- most tests below don't care about the source/sub-topic decoupling
    /// bd issue drip-98u.5 introduced (a source can feed several sub-topics,
    /// or a sub-topic can be fed by several sources), so reusing the
    /// sub-topic name as a stand-in source label keeps most existing
    /// assertions about `sources:`/`**Sources:**` meaningful without a
    /// third parameter. Tests that DO exercise that decoupling build a
    /// `DigestRun` by hand instead (e.g.
    /// `sources_frontmatter_and_source_labels_dedupe_and_include_zero_contribution_sources`,
    /// the two "CONFIRM BY TEST" merge tests near the bottom of this module).
    fn sample_run_with_topics(entries: Vec<(&str, &str, Vec<Item>)>) -> DigestRun {
        let items_by_subtopic = entries
            .iter()
            .map(|(main_topic, sub_topic, items)| (section(main_topic, sub_topic), items.clone()))
            .collect();
        let sources = entries
            .iter()
            .map(|(_, sub_topic, _)| SourceGroup {
                kind: SourceKind::Reddit,
                name: sub_topic.to_string(),
            })
            .collect();
        DigestRun {
            sort: Sort::Top,
            time: Some(TimeFilter::Day),
            query: None,
            tags: vec![],
            items_by_subtopic,
            sources,
            created_at: Utc.with_ymd_and_hms(2026, 7, 8, 14, 32, 10).unwrap(),
        }
    }

    /// Build a `DigestRun` from `(sub_topic, items)` pairs, each under a
    /// shared `"Programming"` main topic -- the common case most tests below
    /// need. Use [`sample_run_with_topics`] instead when a test needs
    /// distinct main topics.
    fn sample_run(entries: Vec<(&str, Vec<Item>)>) -> DigestRun {
        sample_run_with_topics(
            entries
                .into_iter()
                .map(|(sub_topic, items)| ("Programming", sub_topic, items))
                .collect(),
        )
    }

    #[test]
    fn demo_full_featured_rendering_sample() {
        let self_post = sample_item("abc123", "A [neat] discovery about lifetimes");

        let mut link_post = sample_item("def456", "Another title");
        link_post.comments_url =
            Some("https://reddit.com/r/rust/comments/def456/post/".to_string());
        link_post.url = "https://example.com/thing".to_string();

        let mut nsfw_post = sample_item("ghi789", "A spicy post");
        nsfw_post.nsfw = true;

        let run = sample_run(vec![("rust", vec![self_post, link_post, nsfw_post])]);
        let note = render_digest_note(&run);

        assert!(note.contains("A \\[neat\\] discovery about lifetimes"));
        assert!(note.contains("⚠️ NSFW **[A spicy post]"));
        // Exactly one blank line between the query summary line and the
        // first topic heading, between the topic and sub-topic headings, and
        // between sections generally.
        assert!(!note.contains("\n\n\n"));
    }

    #[test]
    fn renders_basic_multi_subtopic_digest() {
        let run = sample_run(vec![
            ("rust", vec![sample_item("abc123", "Some post title")]),
            ("programming", vec![sample_item("def456", "Another title")]),
        ]);

        let note = render_digest_note(&run);

        assert!(note.starts_with("---\n"));
        assert!(note.contains("createdOn: \"2026-07-08T14:32:10Z\""));
        assert!(note.contains("modifiedOn: \"2026-07-08T14:32:10Z\""));
        assert!(note.contains("topics: [Programming]"));
        assert!(note.contains("subtopics: [rust, programming]"));
        assert!(note.contains("sources: [rust, programming]"));
        assert!(note.contains("sort: top"));
        assert!(note.contains("time_filter: day"));
        assert!(note.contains("fetched_count: 2"));
        assert!(note.contains("# 2026-07-08 - Daily digest\n"));
        assert!(note.contains("## Programming"));
        assert!(note.contains("### rust"));
        assert!(note.contains("### programming"));
        assert!(
            note.contains("**[Some post title](https://reddit.com/r/rust/comments/abc123/post/)**")
        );
        assert!(note.contains("**Sources:** rust, programming"));
        assert!(note.contains("**Sort:** top (day)"));
        assert!(note.contains("**Query:** —"));
    }

    #[test]
    fn h1_heading_is_iso_date_plus_daily_digest_with_no_time_of_day() {
        let run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
        let note = render_digest_note(&run);

        assert!(
            note.contains("# 2026-07-08 - Daily digest\n"),
            "expected the new date-only H1 heading:\n{note}"
        );
        let h1_line = note
            .lines()
            .find(|l| l.starts_with("# "))
            .expect("expected an H1 heading line");
        assert!(
            !h1_line.contains("1432") && !h1_line.contains("14:32"),
            "H1 heading must not include a time-of-day:\n{h1_line}"
        );
    }

    #[test]
    fn escapes_square_brackets_in_post_titles() {
        let item = sample_item("abc123", "Post with [brackets] in title");
        let run = sample_run(vec![("rust", vec![item])]);
        let note = render_digest_note(&run);

        assert!(note.contains("Post with \\[brackets\\] in title"));
        assert!(!note.contains("[Post with [brackets]"));
    }

    #[test]
    fn marks_nsfw_posts() {
        let mut item = sample_item("abc123", "NSFW post");
        item.nsfw = true;
        let run = sample_run(vec![("rust", vec![item])]);
        let note = render_digest_note(&run);

        assert!(note.contains("- [ ] ⚠️ NSFW **[NSFW post]"));
    }

    #[test]
    fn checkbox_lines_start_with_exactly_dash_space_bracket_space_bracket_space() {
        let item = sample_item("abc123", "A post");
        let run = sample_run(vec![("rust", vec![item])]);
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
        let run = sample_run(vec![("rust", vec![item])]);
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
        // ADAPTED (bd issue drip-98u.6, the pre-charting note-shape
        // decision): `render_item`'s author branch is gone entirely -- no
        // item's author, reddit or otherwise, is ever rendered anymore. This
        // test's original premise (reddit gets `u/`, others don't) is
        // genuinely obsolete; it now pins the stronger, simpler successor
        // property: author never appears, regardless of source kind.
        let reddit_item = sample_item("abc123", "A reddit post");
        let mut rss_item = sample_item("def456", "An rss entry");
        rss_item.author = Some("Jane Blogger".to_string());

        let run = sample_run(vec![
            ("rust", vec![reddit_item]),
            ("rust-blog", vec![rss_item]),
        ]);
        let note = render_digest_note(&run);

        assert!(
            !note.contains("u/someone") && !note.contains(" — someone"),
            "no author suffix should ever render, reddit or not:\n{note}"
        );
        assert!(
            !note.contains("Jane Blogger"),
            "no author suffix should ever render, reddit or not:\n{note}"
        );
    }

    #[test]
    fn reddit_author_with_pre_existing_u_prefix_from_the_rss_feed_is_not_doubled() {
        // ADAPTED (bd issue drip-98u.6): author rendering is gone entirely,
        // so the original doubled-`/u/` hazard this test guarded against no
        // longer exists -- there's no author-formatting code path left to
        // double anything. Kept as a regression guard on the stronger
        // successor property: even a pre-existing malformed `/u/`-prefixed
        // author string must never leak into the rendered note at all.
        let mut item = sample_item("abc123", "A reddit-feed-sourced post");
        item.author = Some("/u/llogiq".to_string());

        let run = sample_run(vec![("rust", vec![item])]);
        let note = render_digest_note(&run);

        assert!(
            !note.contains("llogiq"),
            "author text must never appear in the rendered note at all:\n{note}"
        );
    }

    #[test]
    fn query_and_time_filter_render_null_when_absent_and_value_when_present() {
        let run_without = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
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
            ("rust", vec![sample_item("a", "t")]),
            ("rust", vec![sample_item("b", "t2")]),
        ]);
        run.tags = vec!["dev".to_string(), "drip".to_string(), "dev".to_string()];

        let tags = run.all_tags();
        assert_eq!(tags, vec!["dev".to_string(), "drip".to_string()]);
    }

    #[test]
    fn rendered_note_tags_block_contains_only_user_tags() {
        let mut run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
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
        let mut run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
        run.tags = vec![];

        let note = render_digest_note(&run);

        assert!(
            note.contains("tags: []\n"),
            "expected inline empty tags array:\n{note}"
        );
        assert!(
            !note.contains("tags:\n\n"),
            "must not emit a bare tags key with a blank line:\n{note}"
        );
    }

    #[test]
    fn filename_is_iso_date_plus_daily_digest() {
        let run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);

        let filename = digest_filename(&run);
        assert_eq!(filename, "2026-07-08 - Daily digest.md");
        assert!(!filename.contains('('));
        assert!(!filename.contains(')'));
        assert!(
            !filename.contains("1432"),
            "filename must not include a time-of-day:\n{filename}"
        );
        assert!(filename.ends_with(".md"));
    }

    #[test]
    fn filename_is_identical_regardless_of_sources_or_topics() {
        // The filename is now purely a function of the run's date -- unlike
        // the old topic/source-label parenthetical, differing sources or
        // topics must not change it.
        let single_source_run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
        let multi_source_run = sample_run(vec![
            ("rust", vec![sample_item("a", "t")]),
            ("programming", vec![sample_item("b", "t2")]),
        ]);

        assert_eq!(
            digest_filename(&single_source_run),
            digest_filename(&multi_source_run)
        );
        assert_eq!(
            digest_filename(&single_source_run),
            "2026-07-08 - Daily digest.md"
        );
    }

    #[test]
    fn writes_note_to_expected_sanitized_path_under_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);

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

        let run = sample_run(vec![("rss-feed", vec![item])]);
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
        let run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);

        let nested_folder = "Deeply/Nested/Reddit";
        let path = write_digest_note(dir.path(), nested_folder, &run)
            .expect("write_digest_note should create missing folders");

        assert!(path.exists());
        assert!(dir.path().join(nested_folder).is_dir());
    }

    #[test]
    fn two_subtopics_under_the_same_main_topic_render_under_one_topic_heading() {
        let run = sample_run_with_topics(vec![
            ("Programming", "rust", vec![sample_item("a", "Rust post")]),
            ("Programming", "golang", vec![sample_item("b", "Go post")]),
        ]);

        let note = render_digest_note(&run);

        assert_eq!(
            note.matches("## Programming").count(),
            1,
            "two sub-topics under the same main topic must share ONE H2:\n{note}"
        );
        assert!(note.contains("### rust"));
        assert!(note.contains("### golang"));
    }

    #[test]
    fn subtopics_under_different_main_topics_render_under_separate_headings_in_first_seen_order() {
        let run = sample_run_with_topics(vec![
            (
                "Claude",
                "ClaudeCode",
                vec![sample_item("a", "Claude post")],
            ),
            ("Rust", "rust-hot", vec![sample_item("b", "Rust post")]),
        ]);

        let note = render_digest_note(&run);

        let h2_count = note.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(
            h2_count, 2,
            "expected exactly two H2 main-topic headings:\n{note}"
        );
        let claude_idx = note.find("## Claude").expect("expected a Claude heading");
        let rust_idx = note.find("## Rust").expect("expected a Rust heading");
        assert!(
            claude_idx < rust_idx,
            "main topics should appear in first-seen order:\n{note}"
        );
        assert!(note.contains("topics: [Claude, Rust]"));
    }

    #[test]
    fn renders_a_two_topic_two_subtopic_sample() {
        // The exact scenario described in bd issue drip-38w.3's target
        // format (updated for the H2 main / H3 sub-topic shape, bd issue
        // drip-98u.6): two main topics, one sub-topic each, one item each.
        let run = sample_run_with_topics(vec![
            (
                "Claude",
                "ClaudeCode",
                vec![sample_item("a", "Anthropic ships MCP update")],
            ),
            (
                "Rust",
                "rust-hot",
                vec![sample_item("b", "Async traits stabilized")],
            ),
        ]);

        let note = render_digest_note(&run);

        assert!(note.contains("topics: [Claude, Rust]"));
        assert!(note.contains("subtopics: [ClaudeCode, rust-hot]"));
        assert!(note.contains("sources: [ClaudeCode, rust-hot]"));
        assert!(note.contains("## Claude"));
        assert!(note.contains("### ClaudeCode"));
        assert!(note.contains("## Rust"));
        assert!(note.contains("### rust-hot"));
        assert!(!note.contains("\n\n\n"));
    }

    #[test]
    fn source_kind_no_longer_adds_an_r_slash_prefix_anywhere_in_the_rendered_note() {
        // bd issue drip-98u.12: `SourceKind::heading_prefix`'s `r/`
        // convention is gone entirely -- a Reddit-kind source renders as its
        // bare label everywhere a source name appears (frontmatter
        // `sources:`, the `**Sources:**` line; source subheadings no longer
        // exist at all, bd issue drip-98u.6). Uses an item whose own URL
        // doesn't happen to contain the literal text "r/rust" -- unlike
        // `sample_item`'s default `reddit.com/r/rust/...` URL, which would
        // make a whole-note substring check trip on legitimate link text
        // rather than an actual rendered `r/` prefix (see
        // `rendered_note_tags_block_contains_only_user_tags`'s comment on
        // the same pitfall).
        let mut item = sample_item("a", "t");
        item.url = "https://example.com/a".to_string();
        let mut run = sample_run(vec![("rust", vec![item])]);
        run.sources = vec![SourceGroup {
            kind: SourceKind::Reddit,
            name: "rust".to_string(),
        }];

        let note = render_digest_note(&run);

        assert!(
            !note.contains("r/rust"),
            "no r/ prefix should ever be rendered:\n{note}"
        );
        assert!(note.contains("**Sources:** rust"));
        assert!(note.contains("sources: [rust]"));
    }

    #[test]
    fn sources_frontmatter_and_source_labels_dedupe_and_include_zero_contribution_sources() {
        // bd issue drip-98u.5: `sources:` lists every source that fetched
        // successfully THIS run, including one that contributed zero items
        // -- and `DigestRun::sources` is not required to be pre-deduped by
        // the caller, so `source_labels`/`source_groups` must dedupe on read.
        let mut run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
        run.sources.push(SourceGroup {
            kind: SourceKind::Rss,
            name: "quiet-source".to_string(),
        });
        // A duplicate of the already-listed "rust" source (same kind+name)
        // must collapse to a single entry, not appear twice.
        run.sources.push(SourceGroup {
            kind: SourceKind::Reddit,
            name: "rust".to_string(),
        });

        assert_eq!(
            run.source_labels(),
            vec!["rust".to_string(), "quiet-source".to_string()],
            "duplicates must collapse, first-seen order preserved"
        );

        let note = render_digest_note(&run);
        assert!(note.contains("sources: [rust, quiet-source]"));
        assert!(!note.contains("quiet-source, quiet-source"));
        assert!(note.contains("**Sources:** rust, quiet-source"));
    }

    #[test]
    fn frontmatter_carries_a_separate_subtopics_key_alongside_topics() {
        let run = sample_run_with_topics(vec![
            ("AI engineering", "hooks", vec![sample_item("a", "t")]),
            ("AI engineering", "skills", vec![sample_item("b", "t2")]),
        ]);
        let note = render_digest_note(&run);

        assert!(note.contains("topics: [AI engineering]"));
        assert!(note.contains("subtopics: [hooks, skills]"));
    }

    #[test]
    fn empty_subtopic_sections_are_omitted_from_the_rendered_body() {
        // bd issue drip-98u.6, point 2: only sub-topics with at least one
        // routed item get an H3.
        let mut run = sample_run(vec![("rust", vec![sample_item("a", "t")])]);
        run.items_by_subtopic
            .push((section("Programming", "empty-one"), vec![]));

        let note = render_digest_note(&run);

        assert!(
            !note.contains("### empty-one"),
            "an empty sub-topic must not get its own heading:\n{note}"
        );
        assert!(note.contains("### rust"));
    }

    #[test]
    fn fetched_count_counts_rendered_lines_not_distinct_items_under_multi_match() {
        // bd issue drip-98u.6, point 4: `fetched_count` deliberately diverges
        // from `fetch_runs.post_count` (which counts distinct items,
        // drip-98u.5) -- it counts rendered LINES, so a multi-matched item
        // counts once per sub-topic it lands in.
        let item = sample_item("a", "Multi-matched item");
        let run = sample_run_with_topics(vec![
            ("AI engineering", "hooks", vec![item.clone()]),
            ("AI engineering", "skills", vec![item]),
        ]);

        let note = render_digest_note(&run);

        assert!(
            note.contains("fetched_count: 2"),
            "fetched_count should count the two rendered lines, not the one \
             distinct item:\n{note}"
        );
    }

    #[test]
    fn the_same_item_rendered_under_two_subtopics_appears_in_both_with_no_precedence() {
        // bd issue drip-98u.3's "no precedence" multi-match decision,
        // rendered.
        let item = sample_item("a", "A skill that wraps a hook");
        let run = sample_run_with_topics(vec![
            ("AI engineering", "hooks", vec![item.clone()]),
            ("AI engineering", "skills", vec![item]),
        ]);

        let note = render_digest_note(&run);

        assert_eq!(
            note.matches("A skill that wraps a hook").count(),
            2,
            "the same item should render once under each matching sub-topic:\n{note}"
        );
        assert!(note.contains("### hooks") && note.contains("### skills"));
    }

    // -- merge (append into an existing same-day note) tests: bd issue drip-47u --

    #[test]
    fn merge_is_a_byte_for_byte_no_op_when_the_run_has_nothing_new() {
        let run = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run);

        let merged = merge_digest_note(&existing, &run);

        assert_eq!(
            merged, existing,
            "merging a run whose items are all already present must not change the note"
        );
    }

    #[test]
    fn merge_appends_a_new_item_under_the_existing_topic_and_subtopic() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run1);

        let run2 = sample_run(vec![("rust", vec![sample_item("b", "Post B")])]);
        let merged = merge_digest_note(&existing, &run2);

        assert!(
            merged.contains("Post A"),
            "existing item must survive:\n{merged}"
        );
        assert!(
            merged.contains("Post B"),
            "new item must be appended:\n{merged}"
        );
        assert_eq!(merged.matches("## Programming").count(), 1);
        assert_eq!(merged.matches("### rust").count(), 1);
        assert!(merged.find("Post A").unwrap() < merged.find("Post B").unwrap());
        assert!(!merged.contains("\n\n\n"));
    }

    #[test]
    fn merge_preserves_a_ticked_checkbox_and_a_manual_edit() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let rendered = render_digest_note(&run1);
        let edited = rendered.replace("- [ ] **[Post A]", "- [x] **[Post A]")
            + "\nMy manual note under the digest.\n";

        let run2 = sample_run(vec![("rust", vec![sample_item("b", "Post B")])]);
        let merged = merge_digest_note(&edited, &run2);

        assert!(
            merged.contains("- [x] **[Post A]"),
            "ticked checkbox must not be reset to - [ ]:\n{merged}"
        );
        assert!(
            merged.contains("My manual note under the digest."),
            "manual edit must survive the merge:\n{merged}"
        );
        assert!(
            merged.contains("Post B"),
            "new item still appended:\n{merged}"
        );
    }

    #[test]
    fn merge_does_not_duplicate_an_item_even_after_it_was_ticked() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let rendered = render_digest_note(&run1);
        let edited = rendered.replace("- [ ] **[Post A]", "- [x] **[Post A]");

        let merged = merge_digest_note(&edited, &run1);

        assert_eq!(
            merged.matches("Post A").count(),
            1,
            "item must not be duplicated after being ticked:\n{merged}"
        );
        assert!(merged.contains("- [x] **[Post A]"));
    }

    #[test]
    fn merge_adds_a_new_subtopic_subsection_under_an_existing_topic() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run1);

        let run2 = sample_run(vec![("golang", vec![sample_item("b", "Post B")])]);
        let merged = merge_digest_note(&existing, &run2);

        assert_eq!(
            merged.matches("## Programming").count(),
            1,
            "one topic heading:\n{merged}"
        );
        assert!(merged.contains("### rust"));
        assert!(merged.contains("### golang"));
        assert!(merged.contains("Post A") && merged.contains("Post B"));
        assert!(!merged.contains("\n\n\n"));
    }

    #[test]
    fn merge_adds_a_brand_new_topic_section() {
        let run1 = sample_run_with_topics(vec![(
            "Programming",
            "rust",
            vec![sample_item("a", "Post A")],
        )]);
        let existing = render_digest_note(&run1);

        let run2 = sample_run_with_topics(vec![(
            "News",
            "worldnews",
            vec![sample_item("b", "Post B")],
        )]);
        let merged = merge_digest_note(&existing, &run2);

        assert!(merged.contains("## Programming"));
        assert!(merged.contains("## News"));
        assert!(merged.find("## Programming").unwrap() < merged.find("## News").unwrap());
        assert!(merged.contains("Post A") && merged.contains("Post B"));
        assert!(
            merged.contains("topics: [Programming, News]"),
            "frontmatter topics extended:\n{merged}"
        );
        assert!(!merged.contains("\n\n\n"));
    }

    #[test]
    fn merge_bumps_fetched_count_and_modified_on() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run1);
        assert!(existing.contains("fetched_count: 1"));

        let mut run2 = sample_run(vec![("rust", vec![sample_item("b", "Post B")])]);
        run2.created_at = Utc.with_ymd_and_hms(2026, 7, 8, 18, 0, 0).unwrap();
        let merged = merge_digest_note(&existing, &run2);

        assert!(
            merged.contains("fetched_count: 2"),
            "count should grow by inserted lines:\n{merged}"
        );
        assert!(
            merged.contains("modifiedOn: \"2026-07-08T18:00:00Z\""),
            "modifiedOn should bump:\n{merged}"
        );
        assert!(
            merged.contains("createdOn: \"2026-07-08T14:32:10Z\""),
            "createdOn must not change:\n{merged}"
        );
    }

    #[test]
    fn merge_extends_the_sources_frontmatter_list_for_a_new_source() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run1);
        assert!(existing.contains("sources: [rust]"));

        let run2 = sample_run(vec![("golang", vec![sample_item("b", "Post B")])]);
        let merged = merge_digest_note(&existing, &run2);

        assert!(
            merged.contains("sources: [rust, golang]"),
            "sources list should extend:\n{merged}"
        );
    }

    #[test]
    fn merge_extends_the_subtopics_frontmatter_list_for_a_new_subtopic() {
        let run1 = sample_run(vec![("rust", vec![sample_item("a", "Post A")])]);
        let existing = render_digest_note(&run1);
        assert!(existing.contains("subtopics: [rust]"));

        let run2 = sample_run(vec![("golang", vec![sample_item("b", "Post B")])]);
        let merged = merge_digest_note(&existing, &run2);

        assert!(
            merged.contains("subtopics: [rust, golang]"),
            "subtopics list should extend:\n{merged}"
        );
    }

    // -- CONFIRM BY TEST, NOT DESIGN (bd issue drip-98u.6/drip-ho5.7) --

    #[test]
    fn merge_does_not_collide_two_identically_named_subtopics_under_different_main_topics() {
        // The `### {sub_topic}` search inside `insert_item_lines` is scoped
        // to within its own `## {main_topic}`'s line range, so a sub-topic
        // named "general" under "Claude" must not be confused with a
        // same-named "general" sub-topic under "Rust" (digest.rs's own
        // doc comment on `insert_item_lines`, confirmed here rather than
        // merely assumed).
        let run1 = sample_run_with_topics(vec![(
            "Claude",
            "general",
            vec![sample_item("a", "Claude post")],
        )]);
        let existing = render_digest_note(&run1);

        let run2 = sample_run_with_topics(vec![(
            "Rust",
            "general",
            vec![sample_item("b", "Rust post")],
        )]);
        let merged = merge_digest_note(&existing, &run2);

        assert_eq!(
            merged.matches("### general").count(),
            2,
            "two distinct H3s, one per main topic:\n{merged}"
        );
        assert!(merged.contains("## Claude") && merged.contains("## Rust"));

        let claude_idx = merged.find("## Claude").unwrap();
        let rust_idx = merged.find("## Rust").unwrap();
        let claude_post_idx = merged.find("Claude post").unwrap();
        let rust_post_idx = merged.find("Rust post").unwrap();
        assert!(
            claude_idx < claude_post_idx && claude_post_idx < rust_idx,
            "Claude post must be nested under the Claude section, not spliced \
             into Rust's:\n{merged}"
        );
        assert!(
            rust_idx < rust_post_idx,
            "Rust post must be nested under the Rust section:\n{merged}"
        );
    }

    #[test]
    fn merge_does_not_treat_the_same_item_rendered_under_two_subtopics_as_a_collision() {
        // The "already present" `HashSet` `insert_item_lines` builds is
        // scoped per subsection (rebuilt fresh for every `### {sub_topic}`),
        // so the same item's second copy under a different sub-topic must
        // not be skipped as a false duplicate.
        let item = sample_item("a", "A skill that wraps a hook");

        let run_hooks_only =
            sample_run_with_topics(vec![("AI engineering", "hooks", vec![item.clone()])]);
        let existing = render_digest_note(&run_hooks_only);

        let run_skills_only =
            sample_run_with_topics(vec![("AI engineering", "skills", vec![item])]);
        let merged = merge_digest_note(&existing, &run_skills_only);

        assert_eq!(
            merged.matches("A skill that wraps a hook").count(),
            2,
            "merging the item's second sub-topic must not be skipped as a \
             false duplicate:\n{merged}"
        );
        assert!(merged.contains("### hooks") && merged.contains("### skills"));
    }

    // -- bd issue drip-ho5.8: `todays_note_has_heading_for` backs `drip
    // topic rename`/the reparent verb's "warn, don't rewrite" behavior.

    #[test]
    fn todays_note_has_heading_for_returns_false_when_no_note_exists_yet() {
        let dir = tempfile::tempdir().expect("tempdir");

        let found = todays_note_has_heading_for(dir.path(), "Resources/drip", "Claude")
            .expect("should succeed even with no note yet");
        assert!(!found);
    }

    /// Like [`sample_run_with_topics`], but with `created_at` set to right
    /// now -- required for `todays_note_has_heading_for` tests, since
    /// `sample_run_with_topics`'s own fixed 2026-07-08 timestamp would write
    /// under a filename that is never "today" once this test runs on any
    /// other date.
    fn sample_run_with_topics_today(entries: Vec<(&str, &str, Vec<Item>)>) -> DigestRun {
        DigestRun {
            created_at: Utc::now(),
            ..sample_run_with_topics(entries)
        }
    }

    #[test]
    fn todays_note_has_heading_for_finds_an_h2_main_topic_heading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run =
            sample_run_with_topics_today(vec![("Claude", "cc hooks", vec![sample_item("a", "x")])]);
        write_digest_note(dir.path(), "Resources/drip", &run).expect("write should succeed");

        let found = todays_note_has_heading_for(dir.path(), "Resources/drip", "Claude")
            .expect("should succeed");
        assert!(found, "an H2 heading for the main topic should be found");
    }

    #[test]
    fn todays_note_has_heading_for_finds_an_h3_sub_topic_heading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run =
            sample_run_with_topics_today(vec![("Claude", "cc hooks", vec![sample_item("a", "x")])]);
        write_digest_note(dir.path(), "Resources/drip", &run).expect("write should succeed");

        let found = todays_note_has_heading_for(dir.path(), "Resources/drip", "cc hooks")
            .expect("should succeed");
        assert!(found, "an H3 heading for the sub-topic should be found");
    }

    #[test]
    fn todays_note_has_heading_for_returns_false_for_a_name_not_in_the_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run =
            sample_run_with_topics_today(vec![("Claude", "cc hooks", vec![sample_item("a", "x")])]);
        write_digest_note(dir.path(), "Resources/drip", &run).expect("write should succeed");

        let found = todays_note_has_heading_for(dir.path(), "Resources/drip", "Rust")
            .expect("should succeed");
        assert!(!found);
    }
}
