//! Links a freshly-written digest note into the day's daily journal note.
//!
//! This module deliberately avoids any YAML/markdown parsing when editing
//! an existing daily note: it only ever does targeted line-based reads and
//! replacements, so a note the user has hand-edited (comments, unusual
//! spacing, extra sections) survives untouched apart from the one bullet
//! we add and the one `modifiedOn` line we bump.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Local, Utc};

use crate::digest::SourceGroup;

/// The exact level-2 heading this module looks for (and creates) in a
/// daily note.
const REDDIT_HEADING: &str = "## Reddit";

/// Compute the path of today's daily note, based on `vault_path`,
/// `daily_notes_folder`, and `daily_note_format` (all three now sourced
/// from the `settings` table -- see [`crate::settings`] -- rather than a
/// whole `Config`, matching the explicit-params convention already
/// established by [`crate::digest::write_digest_note`]). Uses the local
/// calendar date (matching how digest filenames are already dated), since a
/// person's daily note is keyed to their local "today", not UTC's.
///
/// Pure path computation — does no I/O, so it's safe to call from a
/// `--dry-run` preview.
pub fn daily_note_path(
    vault_path: &Path,
    daily_notes_folder: &str,
    daily_note_format: &str,
) -> PathBuf {
    let today = Local::now().format(daily_note_format).to_string();
    vault_path
        .join(daily_notes_folder)
        .join(format!("{today}.md"))
}

/// Ensure today's daily note exists, creating it (with the vault's minimal
/// frontmatter + title-heading shape) if it's missing. Returns the note's
/// path either way. Never modifies an already-existing note — creation is
/// the only thing this function does.
pub fn ensure_daily_note(
    vault_path: &Path,
    daily_notes_folder: &str,
    daily_note_format: &str,
) -> Result<PathBuf> {
    let path = daily_note_path(vault_path, daily_notes_folder, daily_note_format);
    if path.exists() {
        return Ok(path);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create daily notes folder at {}",
                parent.display()
            )
        })?;
    }

    let today = Local::now().format(daily_note_format).to_string();
    let now_iso = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let content = format!(
        "---\ntags:\n  - type/daily\ncreatedOn: \"{now_iso}\"\nmodifiedOn: \"{now_iso}\"\n---\n\n# {today}\n"
    );

    fs::write(&path, content)
        .with_context(|| format!("failed to create daily note at {}", path.display()))?;

    Ok(path)
}

/// Build the markdown bullet referencing a digest note, in the exact format
/// [`append_digest_reference`] inserts. Exposed separately (and kept pure)
/// so callers — e.g. a `--dry-run` preview — can show exactly what would be
/// written without touching any file.
///
/// Named source-kind-neutrally (rather than `reddit_bullet`) since a digest
/// can include non-Reddit source groups too (see bd issue drip-15n.9.6):
/// each group's label is rendered via [`SourceKind::heading_prefix`], the
/// same centralized decision point `src/digest.rs` uses for its own
/// heading/`**Subreddits:**` rendering (bd issue drip-p6v.4).
pub fn digest_bullet(
    digest_note_basename: &str,
    groups: &[SourceGroup],
    post_count: usize,
) -> String {
    let source_labels = groups
        .iter()
        .map(|group| group.kind.heading_prefix(&group.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("- [[{digest_note_basename}]] — {post_count} posts from {source_labels}")
}

/// Insert a bullet referencing a freshly-written digest note into the
/// `## Reddit` section of the daily note at `note_path` (creating the
/// section at the end of the file if it doesn't exist yet), and bump the
/// note's `modifiedOn` frontmatter timestamp.
///
/// `digest_note_basename` should be the digest file's name *without* the
/// `.md` extension (Obsidian wikilinks resolve by basename) — callers
/// typically derive this from `PathBuf::file_stem()`.
///
/// Idempotent in the sense that calling this twice never duplicates or
/// corrupts the `## Reddit` heading — it adds two distinct bullets under
/// it — and never disturbs any other content in the file.
pub fn append_digest_reference(
    note_path: &Path,
    digest_note_basename: &str,
    groups: &[SourceGroup],
    post_count: usize,
) -> Result<()> {
    let content = fs::read_to_string(note_path)
        .with_context(|| format!("failed to read daily note at {}", note_path.display()))?;

    let bullet = digest_bullet(digest_note_basename, groups, post_count);
    let with_bullet = insert_reddit_bullet(&content, &bullet);
    let updated = bump_modified_on(&with_bullet);

    fs::write(note_path, updated)
        .with_context(|| format!("failed to update daily note at {}", note_path.display()))?;

    Ok(())
}

/// Insert `bullet` under the `## Reddit` heading in `content`, creating the
/// heading at the end of the file if it's missing. Pure string logic (no
/// I/O), which keeps it cheap to unit test directly.
fn insert_reddit_bullet(content: &str, bullet: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    match lines.iter().position(|l| l == REDDIT_HEADING) {
        Some(heading_idx) => {
            // The section runs until the next level-2 heading, or EOF if
            // this is the last section.
            let next_heading_idx = lines[heading_idx + 1..]
                .iter()
                .position(|l| l.starts_with("## "))
                .map(|offset| heading_idx + 1 + offset);
            let section_end = next_heading_idx.unwrap_or(lines.len());

            // Insert right after the last existing bullet in the section,
            // or at the section's end if there are no bullets yet.
            let last_bullet_idx = (heading_idx + 1..section_end)
                .rev()
                .find(|&i| lines[i].starts_with("- "));
            let insert_at = last_bullet_idx.map_or(section_end, |idx| idx + 1);

            lines.insert(insert_at, bullet.to_string());
        }
        None => {
            // No `## Reddit` heading yet: add one at the end of the file,
            // with exactly one blank line separating it from whatever
            // precedes it.
            if !lines.is_empty() && !lines.last().unwrap().is_empty() {
                lines.push(String::new());
            }
            lines.push(REDDIT_HEADING.to_string());
            lines.push(String::new());
            lines.push(bullet.to_string());
        }
    }

    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Replace the `modifiedOn: "..."` frontmatter line, if one is present,
/// with the current UTC timestamp — a targeted single-line replace that
/// never parses or reorders the rest of the file. If no `modifiedOn` line
/// exists (e.g. a hand-crafted daily note that lacks one), the file is left
/// unchanged: we deliberately don't invent a new frontmatter key in an
/// unexpected spot.
fn bump_modified_on(content: &str) -> String {
    let now_iso = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    for line in lines.iter_mut() {
        if line.starts_with("modifiedOn: \"") && line.ends_with('"') {
            *line = format!("modifiedOn: \"{now_iso}\"");
            break;
        }
    }

    let mut result = lines.join("\n");
    result.push('\n');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SourceKind;

    const DAILY_NOTES_FOLDER: &str = "Journal/Daily notes";
    const DAILY_NOTE_FORMAT: &str = "%Y-%m-%d";

    fn reddit_group(name: &str) -> SourceGroup {
        SourceGroup {
            kind: SourceKind::Reddit,
            name: name.to_string(),
            topic: "Programming".to_string(),
        }
    }

    #[test]
    fn ensure_daily_note_creates_expected_shape_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");

        let path = ensure_daily_note(dir.path(), DAILY_NOTES_FOLDER, DAILY_NOTE_FORMAT)
            .expect("should create daily note");

        assert!(path.exists());
        assert_eq!(
            path.parent().unwrap(),
            dir.path().join("Journal/Daily notes")
        );

        let today = Local::now().format(DAILY_NOTE_FORMAT).to_string();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{today}.md")
        );

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("tags:\n  - type/daily\n"));
        assert!(content.contains("createdOn: \""));
        assert!(content.contains("modifiedOn: \""));
        assert!(content.contains(&format!("# {today}\n")));
    }

    #[test]
    fn ensure_daily_note_does_not_modify_existing_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = daily_note_path(dir.path(), DAILY_NOTES_FOLDER, DAILY_NOTE_FORMAT);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let existing = "---\ntags:\n  - type/daily\ncreatedOn: \"2020-01-01T00:00:00Z\"\nmodifiedOn: \"2020-01-01T00:00:00Z\"\n---\n\n# custom content\n\nsome hand-written notes\n";
        fs::write(&path, existing).unwrap();

        let returned = ensure_daily_note(dir.path(), DAILY_NOTES_FOLDER, DAILY_NOTE_FORMAT)
            .expect("should return existing path");
        assert_eq!(returned, path);

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, existing);
    }

    #[test]
    fn append_creates_reddit_heading_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        fs::write(
            &path,
            "---\ntags:\n  - type/daily\ncreatedOn: \"2026-07-08T00:00:00Z\"\nmodifiedOn: \"2026-07-08T00:00:00Z\"\n---\n\n# 2026-07-08\n",
        )
        .unwrap();

        append_digest_reference(
            &path,
            "2026-07-08 0900 - Reddit digest (rust)",
            &[reddit_group("rust")],
            3,
        )
        .expect("append should succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(
            "## Reddit\n\n- [[2026-07-08 0900 - Reddit digest (rust)]] — 3 posts from r/rust\n"
        ));
        // Only one heading, no duplicates.
        assert_eq!(content.matches("## Reddit").count(), 1);
    }

    #[test]
    fn append_adds_second_bullet_under_existing_heading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        fs::write(
            &path,
            "---\ntags:\n  - type/daily\ncreatedOn: \"2026-07-08T00:00:00Z\"\nmodifiedOn: \"2026-07-08T00:00:00Z\"\n---\n\n# 2026-07-08\n\n## Reddit\n\n- [[old digest]] — 5 posts from r/rust\n",
        )
        .unwrap();

        append_digest_reference(&path, "new digest", &[reddit_group("programming")], 2)
            .expect("append should succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("## Reddit").count(), 1);

        let old_pos = content.find("- [[old digest]]").unwrap();
        let new_pos = content.find("- [[new digest]]").unwrap();
        assert!(old_pos < new_pos, "new bullet should come after old bullet");

        // Both bullets sit directly under the single heading, with no
        // stray content between them.
        let reddit_idx = content.find("## Reddit").unwrap();
        let section = &content[reddit_idx..];
        assert!(section.contains("- [[old digest]] — 5 posts from r/rust\n- [[new digest]] — 2 posts from r/programming\n"));
    }

    #[test]
    fn append_inserts_into_reddit_section_without_disturbing_later_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        let before = "---\ntags:\n  - type/daily\ncreatedOn: \"2026-07-08T00:00:00Z\"\nmodifiedOn: \"2026-07-08T00:00:00Z\"\n---\n\n# 2026-07-08\n\n## Reddit\n\n- [[old digest]] — 5 posts from r/rust\n\n## Log\n\nDid some stuff today.\n";
        fs::write(&path, before).unwrap();

        append_digest_reference(&path, "new digest", &[reddit_group("programming")], 2)
            .expect("append should succeed");

        let after = fs::read_to_string(&path).unwrap();

        // Exactly one Reddit heading, one Log heading.
        assert_eq!(after.matches("## Reddit").count(), 1);
        assert_eq!(after.matches("## Log").count(), 1);

        // The new bullet lands in the Reddit section, before ## Log.
        let reddit_idx = after.find("## Reddit").unwrap();
        let log_idx = after.find("## Log").unwrap();
        let new_bullet_idx = after.find("- [[new digest]]").unwrap();
        assert!(reddit_idx < new_bullet_idx && new_bullet_idx < log_idx);

        // The Log section's content is completely untouched.
        assert!(after.contains("## Log\n\nDid some stuff today.\n"));

        println!("--- before ---\n{before}\n--- after ---\n{after}");
    }

    #[test]
    fn append_updates_modified_on_and_preserves_everything_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        let before = "---\ntags:\n  - type/daily\ncreatedOn: \"2026-07-08T00:00:00Z\"\nmodifiedOn: \"2026-07-08T00:00:00Z\"\n---\n\n# 2026-07-08\n";
        fs::write(&path, before).unwrap();

        append_digest_reference(&path, "new digest", &[reddit_group("rust")], 1)
            .expect("append should succeed");

        let after = fs::read_to_string(&path).unwrap();

        assert!(!after.contains("modifiedOn: \"2026-07-08T00:00:00Z\""));
        assert!(after.contains("createdOn: \"2026-07-08T00:00:00Z\""));
        assert!(after.contains("tags:\n  - type/daily"));

        // Every line except the modifiedOn line and the newly-inserted
        // Reddit section should be byte-for-byte unchanged.
        for line in before.lines() {
            if line.starts_with("modifiedOn: \"") {
                continue;
            }
            assert!(after.contains(line), "missing untouched line: {line}");
        }
    }

    #[test]
    fn append_twice_produces_two_distinct_bullets_not_a_duplicate_heading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        fs::write(
            &path,
            "---\ntags:\n  - type/daily\ncreatedOn: \"2026-07-08T00:00:00Z\"\nmodifiedOn: \"2026-07-08T00:00:00Z\"\n---\n\n# 2026-07-08\n",
        )
        .unwrap();

        append_digest_reference(&path, "digest one", &[reddit_group("rust")], 3).unwrap();
        append_digest_reference(&path, "digest two", &[reddit_group("programming")], 4).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("## Reddit").count(), 1);
        assert!(content.contains("- [[digest one]] — 3 posts from r/rust"));
        assert!(content.contains("- [[digest two]] — 4 posts from r/programming"));

        let first_pos = content.find("- [[digest one]]").unwrap();
        let second_pos = content.find("- [[digest two]]").unwrap();
        assert!(first_pos < second_pos);
    }

    #[test]
    fn digest_bullet_renders_non_reddit_groups_without_the_r_prefix() {
        let rss_group = SourceGroup {
            kind: SourceKind::Rss,
            name: "rust-blog".to_string(),
            topic: "Programming".to_string(),
        };

        let bullet = digest_bullet("digest", &[rss_group], 2);

        assert!(bullet.contains("rust-blog"));
        assert!(
            !bullet.contains("r/rust-blog"),
            "non-reddit groups must not get the r/ prefix: {bullet}"
        );
    }
}
