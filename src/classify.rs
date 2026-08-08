//! Per-item classification: routes a fetched `Item` into zero or more
//! `(main topic, sub-topic)` sections, per bd issue drip-98u.3's resolution
//! (spec'd by bd issue drip-ho5.5). This module owns the classification
//! *decision* as a pure function over already-loaded rule data -- DB loading
//! of `topic_links`/`link_rules`/`source_excludes` (`migrations/0006_topic_tree.sql`)
//! is a separate concern layered on top, so the decision itself stays
//! testable without a database.
//!
//! The contract, in order (drip-98u.3):
//!
//! 1. **Source-level exclude is a title-only PRE-FILTER.** It runs first and
//!    rejects the item outright, before any candidate routing -- title only,
//!    never the body, even when a candidate's `match_body` is set.
//! 2. **Candidates are only the REQUESTED sub-topics' rules**, not every
//!    sub-topic the source happens to link to (drip-98u.3's "candidate set"
//!    decision) -- the caller is responsible for narrowing `candidates` to
//!    what a given `drip fetch --topic`/`--source` run actually asked for.
//! 3. **Multi-match fans out**: an item matching two candidate sub-topics
//!    appears in BOTH. No precedence, no priority, no tiebreak.
//! 4. **Zero-match drops** the item -- and per the "recorded seen iff written
//!    to a digest" invariant, a dropped item must NOT be recorded seen by the
//!    caller. This module only reports the drop; it does not touch
//!    `seen_items` itself.
//! 5. **Empty include list accepts everything** -- already implemented by
//!    `RuleSet::matches` (`src/rules.rs`), reused here unchanged.
//! 6. The haystack per candidate link is the **title**, or **title + "\n" +
//!    body** when that link's `match_body` is set. The `"\n"` separator
//!    exists specifically so a multi-word phrase term can never match across
//!    the title/body join.
//!
//! Source-level excludes are reused as a `RuleSet` with an empty include list
//! and the source's exclude terms -- `RuleSet::matches` already implements
//! exactly "reject if any exclude term hits, else accept everything" for
//! that shape, so there is no second matching primitive to write or keep in
//! sync with `src/rules.rs`.

use std::collections::{HashMap, HashSet};

use crate::item::Item;
use crate::rules::RuleSet;

/// A `(main topic, sub-topic)` pair -- the unit an item is routed into. Named
/// fields rather than a bare tuple so call sites read as `section.sub_topic`
/// rather than `section.1`, and so it can be used as a `HashMap` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Section {
    pub main_topic: String,
    pub sub_topic: String,
}

/// One requested sub-topic's routing rules for a single source -- the
/// candidate set the caller narrows down per drip-98u.3's "candidates are
/// only the requested sub-topics' rules" decision. Carries `match_body`
/// per-link (per `migrations/0006_topic_tree.sql`'s `topic_links.match_body`),
/// since whether to also search an item's body is a per-(source, sub-topic)
/// choice, not a global one (bd issue drip-98u.2).
pub struct Candidate {
    pub section: Section,
    pub rules: RuleSet,
    pub match_body: bool,
}

/// The outcome of classifying a single item against one source's source-level
/// excludes and candidate sub-topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemOutcome {
    /// Rejected by the source-level, title-only exclude pre-filter before any
    /// candidate routing ran.
    Excluded,
    /// Matched at least one candidate; carries every matching section (fans
    /// out on a multi-match, per drip-98u.3's "no precedence" decision).
    Routed(Vec<Section>),
    /// Passed the pre-filter but matched zero candidates.
    Dropped,
}

/// Classify a single item against `source_excludes` (title-only pre-filter)
/// and `candidates` (the requested sub-topics' rules). Pure: no I/O, no
/// database -- callers load `source_excludes`/`candidates` from
/// `source_excludes`/`topic_links`/`link_rules` and pass the loaded data in.
pub fn classify_item(
    item: &Item,
    source_excludes: &[String],
    candidates: &[Candidate],
) -> ItemOutcome {
    // Source-level exclude: title-only, runs first, unconditional -- reuses
    // `RuleSet::matches` with an empty include list (accepts everything)
    // and the source's exclude terms, since that already implements exactly
    // "reject on any exclude hit, else accept" (src/rules.rs).
    let source_filter = RuleSet {
        include: vec![],
        exclude: source_excludes.to_vec(),
    };
    if !source_filter.matches(&item.title) {
        return ItemOutcome::Excluded;
    }

    let matched_sections: Vec<Section> = candidates
        .iter()
        .filter(|candidate| {
            let haystack = haystack_for_link(item, candidate.match_body);
            candidate.rules.matches(&haystack)
        })
        .map(|candidate| candidate.section.clone())
        .collect();

    if matched_sections.is_empty() {
        ItemOutcome::Dropped
    } else {
        ItemOutcome::Routed(matched_sections)
    }
}

/// The composed haystack for one candidate link (drip-98u.3, point 6): the
/// title alone, or title + `"\n"` + the item's cleaned body when
/// `match_body` is set. The newline separator is deliberate -- without it, a
/// multi-word phrase term could match spuriously across the title/body join
/// (e.g. title ending in "agent" + body starting with "loop" would otherwise
/// read as the contiguous phrase "agent loop").
fn haystack_for_link(item: &Item, match_body: bool) -> String {
    if !match_body {
        return item.title.clone();
    }

    let body = item
        .summary
        .as_deref()
        .map(plain_text_body)
        .unwrap_or_default();
    format!("{}\n{}", item.title, body)
}

/// Recover readable plain text from a source's raw body field (`Item.summary`).
///
/// Reddit's Atom `<content type="html">` is double-HTML-escaped (bd issue
/// drip-98u.2): the underlying HTML is escaped once for embedding as HTML
/// text, then escaped again for embedding that already-escaped string inside
/// the XML document. `feed-rs`'s XML parser only reverses the outer,
/// XML-level layer of escaping while parsing the document, so what lands in
/// `Item.summary` is still HTML-escaped once (e.g. `&lt;div
/// class=&quot;md&quot;&gt;`) -- one more unescape pass is needed to get real
/// HTML tags, which are then stripped. Unescaping is applied twice
/// unconditionally (`unescape(unescape(x))`, matching drip-98u.2's
/// resolution) rather than adaptively, since a second pass over
/// already-plain text is a no-op: there are no entities left for it to
/// touch.
///
/// Tags are stripped wholesale -- this deliberately also drops attribute
/// names/values (e.g. `class="md"`) along with the tag itself, not just the
/// angle brackets. That matters: leaving `class="md"` behind as visible text
/// would make a rule term like `md` match every single Reddit self post,
/// which is the documented footgun this function exists to avoid.
pub fn plain_text_body(raw: &str) -> String {
    let unescaped_once = unescape_html_entities(raw);
    let unescaped_twice = unescape_html_entities(&unescaped_once);
    strip_tags(&unescaped_twice)
}

/// Decode one pass of named/numeric HTML entities (`&amp;`, `&lt;`, `&gt;`,
/// `&quot;`, `&apos;`, and numeric character references like `&#39;`/
/// `&#x27;`). A single pass only ever decodes entities literally present in
/// `input` -- it never re-scans text it has just produced, which is what
/// makes calling this twice equivalent to genuinely reversing two layers of
/// escaping rather than collapsing them into one merged decode.
fn unescape_html_entities(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '&' {
            // Look ahead for a closing ';' within a short window -- long
            // enough for any real entity name/numeric reference, short
            // enough that a stray '&' followed by unrelated prose doesn't
            // scan the whole rest of the string looking for a ';'.
            let window_end = (i + 1 + 12).min(chars.len());
            if let Some(semi_offset) = chars[i + 1..window_end].iter().position(|&c| c == ';') {
                let entity_end = i + 1 + semi_offset;
                let entity: String = chars[i + 1..entity_end].iter().collect();
                if let Some(decoded) = decode_entity(&entity) {
                    out.push(decoded);
                    i = entity_end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Decode a single entity's inner text (without the surrounding `&`/`;`), or
/// `None` if it isn't a recognized entity -- in which case the caller leaves
/// the original text untouched rather than guessing.
fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

/// Strip everything between (and including) `<` and `>` -- tag names,
/// attribute names, and attribute values all disappear along with the tag,
/// deliberately (see `plain_text_body`'s doc comment for why that matters).
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;

    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    out
}

/// The aggregate result of classifying a batch of items: per-section
/// buckets, the "routed set" of item ids that landed in at least one
/// section, and every dropped/excluded item (so a caller can both report a
/// count and, at `-v`, list titles -- bd issue drip-98u.3's "REPORT the
/// count (-v lists titles)"). See this module's doc comment header for why
/// dropped (zero-match) and excluded (source-level pre-filter) are kept as
/// two separate lists rather than one combined "not routed" bucket: they
/// have different remediation stories for a human reading a report -- a
/// zero-match count says "your rules are too narrow", an excluded count says
/// "your pre-filter is working as intended" -- and bd issue drip-98u.3
/// treats them as related-but-distinct decisions (points 3 and 4 of its
/// resolution).
pub struct ClassifyResult {
    pub sections: HashMap<Section, Vec<Item>>,
    /// Ids of items that landed in at least one section. A `HashSet` because
    /// a multi-match item is only "routed" once, even though it appears
    /// twice in `sections` -- this is what a caller checks before calling
    /// `dedup::record_seen` (per the "recorded seen iff written to a digest"
    /// invariant), and recording an id once vs. twice must not differ.
    pub routed_ids: HashSet<String>,
    pub dropped: Vec<Item>,
    pub excluded: Vec<Item>,
}

impl ClassifyResult {
    pub fn dropped_count(&self) -> usize {
        self.dropped.len()
    }

    pub fn excluded_count(&self) -> usize {
        self.excluded.len()
    }
}

/// Classify a batch of items, aggregating each one's `classify_item` outcome
/// into `ClassifyResult`'s per-section buckets, routed set, and
/// dropped/excluded lists.
pub fn classify_items(
    items: Vec<Item>,
    source_excludes: &[String],
    candidates: &[Candidate],
) -> ClassifyResult {
    let mut result = ClassifyResult {
        sections: HashMap::new(),
        routed_ids: HashSet::new(),
        dropped: Vec::new(),
        excluded: Vec::new(),
    };

    for item in items {
        match classify_item(&item, source_excludes, candidates) {
            ItemOutcome::Excluded => result.excluded.push(item),
            ItemOutcome::Dropped => result.dropped.push(item),
            ItemOutcome::Routed(sections) => {
                result.routed_ids.insert(item.id.clone());
                for section in sections {
                    result
                        .sections
                        .entry(section)
                        .or_default()
                        .push(item.clone());
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(id: &str, title: &str) -> Item {
        Item {
            id: id.to_string(),
            title: title.to_string(),
            url: format!("https://reddit.com/r/claudecode/comments/{id}/post/"),
            comments_url: None,
            author: None,
            published_at: None,
            summary: None,
            score: None,
            num_comments: None,
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

    // Cycle E: an item matching zero candidates is dropped -- excluded from
    // the routed set, absent from every section bucket, and counted (via
    // the batch aggregator, since "the routed set"/"dropped count" are
    // properties of a whole classified batch, not one item's outcome).
    #[test]
    fn zero_match_item_is_dropped_excluded_from_routed_set_and_counted() {
        let matching = sample_item("abc123", "Spent months ignoring Claude Code hooks");
        let noise = sample_item("def456", "My game demo is on Steam");

        let candidates = vec![Candidate {
            section: section("AI engineering", "hooks"),
            rules: RuleSet {
                include: vec!["hook".to_string()],
                exclude: vec![],
            },
            match_body: false,
        }];

        let result = classify_items(vec![matching.clone(), noise.clone()], &[], &candidates);

        assert_eq!(
            result.dropped_count(),
            1,
            "exactly one of the two items should be reported as dropped"
        );
        assert_eq!(
            result.dropped,
            vec![noise.clone()],
            "the dropped list should contain the zero-match item itself, so \
             a caller can report its title at -v"
        );
        assert!(
            !result.routed_ids.contains(&noise.id),
            "a dropped item's id must not appear in the routed set"
        );
        assert!(
            result.routed_ids.contains(&matching.id),
            "the matching item's id should appear in the routed set"
        );
        for items in result.sections.values() {
            assert!(
                !items.contains(&noise),
                "a dropped item must not appear in any section bucket"
            );
        }
    }

    // Cycle C: a non-excluded item matching exactly one candidate's rules
    // routes into that one section.
    #[test]
    fn item_matching_one_candidates_rules_routes_into_that_section() {
        let item = sample_item("abc123", "Spent months ignoring Claude Code hooks");

        let candidates = vec![
            Candidate {
                section: section("AI engineering", "hooks"),
                rules: RuleSet {
                    include: vec!["hook".to_string()],
                    exclude: vec![],
                },
                match_body: false,
            },
            Candidate {
                section: section("AI engineering", "skills"),
                rules: RuleSet {
                    include: vec!["skill".to_string()],
                    exclude: vec![],
                },
                match_body: false,
            },
        ];

        let outcome = classify_item(&item, &[], &candidates);

        assert_eq!(
            outcome,
            ItemOutcome::Routed(vec![section("AI engineering", "hooks")]),
            "an item matching only the \"hooks\" candidate's rules should \
             route into that one section, not the \"skills\" one"
        );
    }

    // Cycle D: an item matching TWO candidates' rules fans out into BOTH
    // sections -- no precedence, no first-match-wins.
    #[test]
    fn item_matching_two_candidates_rules_fans_out_into_both_sections() {
        let item = sample_item("abc123", "A skill that wraps a hook for you automatically");

        let candidates = vec![
            Candidate {
                section: section("AI engineering", "hooks"),
                rules: RuleSet {
                    include: vec!["hook".to_string()],
                    exclude: vec![],
                },
                match_body: false,
            },
            Candidate {
                section: section("AI engineering", "skills"),
                rules: RuleSet {
                    include: vec!["skill".to_string()],
                    exclude: vec![],
                },
                match_body: false,
            },
        ];

        let outcome = classify_item(&item, &[], &candidates);

        match outcome {
            ItemOutcome::Routed(sections) => {
                assert_eq!(
                    sections.len(),
                    2,
                    "an item matching two candidates should carry exactly two sections, got {sections:?}"
                );
                assert!(sections.contains(&section("AI engineering", "hooks")));
                assert!(sections.contains(&section("AI engineering", "skills")));
            }
            other => panic!("expected Routed(_) with both sections, got {other:?}"),
        }
    }

    // Cycle B: Reddit's Atom body is double-HTML-escaped and needs
    // tag-stripping on top -- a rule term like "md" must not spuriously
    // match every self post just because `class="md"` is common markup.
    #[test]
    fn plain_text_body_strips_tags_and_double_unescapes_reddit_html() {
        let raw = r#"&lt;!-- SC_OFF --&gt;&lt;div class=&quot;md&quot;&gt;&lt;p&gt;I added a &lt;a href="x"&gt;hook&lt;/a&gt; to settings.json&lt;/p&gt;&lt;/div&gt;"#;

        let text = plain_text_body(raw);

        assert!(
            text.contains("hook"),
            "the word \"hook\" should be findable in the cleaned text, got: {text:?}"
        );
        for noise in ["div", "class", "md", "href"] {
            assert!(
                !text.contains(noise),
                "HTML tag/attribute noise {noise:?} should not survive tag-stripping, got: {text:?}"
            );
        }
    }

    // Cycle A: source-level exclude is a title-only pre-filter that runs
    // first and rejects outright, never consulting the body -- even when a
    // candidate that would otherwise match has `match_body` set.
    #[test]
    fn source_level_exclude_rejects_on_title_and_never_consults_body() {
        let mut item = sample_item("abc123", "Claude Model Performance Megathread - August 3");
        item.summary = Some("This body is all about hooks and skills".to_string());

        let source_excludes = vec!["megathread".to_string()];
        let candidates = vec![Candidate {
            section: section("AI engineering", "hooks"),
            rules: RuleSet {
                include: vec!["hook".to_string()],
                exclude: vec![],
            },
            match_body: true,
        }];

        let outcome = classify_item(&item, &source_excludes, &candidates);

        assert_eq!(
            outcome,
            ItemOutcome::Excluded,
            "a source-level exclude hit on the title should reject the item \
             before any candidate routing runs, even though the body would \
             otherwise match a candidate with match_body set"
        );
    }
}
