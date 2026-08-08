//! The pure term-matching primitive underneath keyword-rule classification
//! (bd issue drip-ho5.1, spec'd by bd issue drip-98u.2).
//!
//! `drip-98u.2` measured three candidate match strategies against 27 live
//! r/ClaudeCode titles and rejected two of them: substring matching (`"ai"`
//! matches "d*ai*ly routine" and "ag*ai*nst" -- zero true positives) and
//! whole-word matching (`"hook"` misses "Claude Code hook**s**" entirely --
//! English plurals break it, and this domain's terms are pluralized in
//! practice). The chosen semantics -- a term matches when it starts at a
//! word boundary, with any suffix allowed -- is the only one of the three
//! that scored correctly on both cases in that table. No `regex` crate: the
//! whole contract reduces to "find the occurrence, check the preceding
//! character is non-alphanumeric (or start-of-string)", which doesn't need
//! one.
//!
//! This module owns only the matching primitive itself -- not rule storage
//! (`link_rules(link_id, role, term)`, per drip-98u.2's resolution). Multi-word
//! phrase matching falls out of the single-term primitive for free (a phrase
//! is just a term that happens to contain a space, matched as one contiguous
//! occurrence). An empty include list matches everything, and exclude terms
//! always win -- combined as `Any(includes) minus None(excludes)`, per
//! drip-98u.2's resolution, reusing the exact same matching primitive as
//! include rather than a second one; see `RuleSet::matches`'s doc comment
//! for the full contract.

/// A set of include/exclude terms for keyword-rule classification.
///
/// Storage semantics beyond matching itself (e.g. `link_rules(link_id, role,
/// term)`) are out of scope for this type -- see bd issue drip-98u.2 for the
/// full contract this implements. Matching combines include and exclude as
/// `Any(includes) minus None(excludes)`: an item matches if it satisfies the
/// (possibly-empty, i.e. unfiltered) include side and no exclude term hits;
/// any exclude hit rejects the item outright, regardless of the include
/// side. See `RuleSet::matches`'s doc comment for the full contract,
/// including the word-boundary/case-insensitive/phrase matching semantics
/// shared by both sides.
pub struct RuleSet {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl RuleSet {
    /// Whether `haystack` matches this rule set.
    ///
    /// Full contract (bd issue drip-98u.2's resolution): `Any(includes) minus
    /// None(excludes)` -- an item matches if it satisfies the include side
    /// *and* no exclude term hits, and an exclude hit rejects the item
    /// outright regardless of the include side. Exclude is checked first and
    /// unconditionally, precisely because an empty include list is treated
    /// as "matches everything" below it -- if the exclude check happened
    /// after that early return, a lone exclude term with no include terms
    /// configured would never get a chance to reject anything.
    ///
    /// The include side implements prefix-at-word-boundary matching (bd
    /// issue drip-ho5.1's first cycle): an include term matches if it occurs
    /// in `haystack` at a position preceded by either nothing (start of
    /// string) or a non-alphanumeric character, regardless of what follows
    /// it (so a plural suffix like "hooks" still matches "hook"). Matching
    /// is case-insensitive (bd issue drip-ho5.1's second cycle): both the
    /// term and the haystack are case-folded before the boundary check, so
    /// "AI" matches "ai engineering" and "SKILL.md" matches "skill.md file"
    /// -- while still rejecting a mid-word occurrence like "ai" inside
    /// "daily" or "against". Multi-word phrases fall out of this for free: a
    /// term containing a space (e.g. "agent loop") only matches when the
    /// whole phrase occurs contiguously, since it's searched for as a single
    /// string, and the boundary check still applies at its start. An empty
    /// include list matches everything (bd issue drip-98u.2's resolution,
    /// third cycle): a link with zero include terms is unfiltered, so every
    /// item from that source is treated as a match on the include side --
    /// this is what makes migration 0006's backfill behaviour-preserving for
    /// pre-existing sources that never had include terms configured.
    ///
    /// The exclude side (this cycle) reuses the exact same
    /// prefix-at-word-boundary, case-insensitive, phrase-capable matching
    /// primitive as include -- there is no separate substring-based exclude
    /// check, so an exclude term like "ban" rejects "a real ban" but not
    /// "urban planning", the same way an include term would.
    pub fn matches(&self, haystack: &str) -> bool {
        if self
            .exclude
            .iter()
            .any(|term| term_matches_at_word_boundary(haystack, term))
        {
            return false;
        }

        if self.include.is_empty() {
            return true;
        }

        self.include
            .iter()
            .any(|term| term_matches_at_word_boundary(haystack, term))
    }
}

/// `true` if `term` occurs anywhere in `haystack` at a position immediately
/// preceded by either the start of the string or a non-alphanumeric
/// character. Matching is case-insensitive: both `haystack` and `term` are
/// lowercased before searching, so all byte indices used below are computed
/// against the lowercased haystack, never against the original.
fn term_matches_at_word_boundary(haystack: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }

    let haystack = haystack.to_lowercase();
    let term = term.to_lowercase();

    let mut search_from = 0;
    while let Some(rel_idx) = haystack[search_from..].find(&term) {
        let idx = search_from + rel_idx;
        let boundary_ok = match haystack[..idx].chars().next_back() {
            None => true,
            Some(c) => !c.is_alphanumeric(),
        };
        if boundary_ok {
            return true;
        }
        // Advance past the first character of this (rejected) match so the
        // next search starts on a valid char boundary, then keep looking --
        // the same term can occur again later in the haystack.
        let advance = haystack[idx..].chars().next().map_or(1, |c| c.len_utf8());
        search_from = idx + advance;
    }

    false
}

/// Which of `terms` match `haystack` at a word boundary, in `terms`' own
/// order -- backs `drip topic test`'s "which terms fired" explain output
/// (bd issue drip-ho5.8). Reuses the exact same private matching primitive
/// `RuleSet::matches` itself calls, so this can never disagree with the real
/// matching decision.
pub fn matching_terms<'a>(terms: &'a [String], haystack: &str) -> Vec<&'a str> {
    terms
        .iter()
        .filter(|term| term_matches_at_word_boundary(haystack, term))
        .map(|term| term.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_term_matches_at_a_word_boundary_but_not_mid_word() {
        let rules = RuleSet {
            include: vec!["hook".into()],
            exclude: vec![],
        };

        assert!(
            rules.matches("Spent months ignoring Claude Code hooks"),
            "an include term should match when it starts at a word boundary, \
             even with a plural suffix"
        );

        assert!(
            !rules.matches("unhooked and adrift"),
            "an include term should not match mid-word, with no word boundary \
             before it"
        );
    }

    #[test]
    fn include_term_matches_case_insensitively_but_still_at_a_word_boundary() {
        let rules = RuleSet {
            include: vec!["ai".into()],
            exclude: vec![],
        };

        assert!(
            rules.matches("AI engineering"),
            "a lowercase term should match an uppercase occurrence in the \
             haystack"
        );

        assert!(
            !rules.matches("My daily routine with Claude"),
            "case-folding must not degrade into substring matching -- the \
             \"ai\" in \"daily\" is still mid-word"
        );

        assert!(
            !rules.matches("I sorted all 79 rules in my CLAUDE.md against Boris Cherny's advice"),
            "case-folding must not degrade into substring matching -- the \
             \"ai\" in \"against\" is still mid-word"
        );

        let rules = RuleSet {
            include: vec!["SKILL.md".into()],
            exclude: vec![],
        };

        assert!(
            rules.matches("writing your first skill.md file"),
            "an uppercase term should match a lowercase occurrence in the \
             haystack"
        );
    }

    #[test]
    fn include_term_supports_multi_word_phrases_as_a_single_contiguous_term() {
        let rules = RuleSet {
            include: vec!["agent loop".into()],
            exclude: vec![],
        };

        assert!(
            rules.matches("The agent loop, explained"),
            "a multi-word term should match when the whole phrase occurs \
             contiguously at a word boundary"
        );

        assert!(
            !rules.matches("agent in the loop"),
            "a multi-word term must match as one contiguous phrase, not as \
             its words scattered separately in the haystack"
        );

        assert!(
            rules.matches("multi-agent loops in practice"),
            "a hyphen is non-alphanumeric, so it still counts as a word \
             boundary immediately before the phrase -- \"multi-agent loops\" \
             contains \"agent loop\" starting right after the hyphen"
        );
    }

    #[test]
    fn exclude_terms_always_win_over_a_matching_include_or_an_empty_include() {
        let rules = RuleSet {
            include: vec!["skill".into()],
            exclude: vec!["pricing".into()],
        };

        assert!(
            rules.matches("Writing your first Skill"),
            "an include match with no exclude hit should still match"
        );

        assert!(
            !rules.matches("Skills and the new pricing tiers"),
            "an exclude term match should reject the item even though an \
             include term also matches"
        );

        let rules = RuleSet {
            include: vec![],
            exclude: vec!["megathread".into()],
        };

        assert!(
            rules.matches("Helpful things for running multiple sessions"),
            "an empty include list should still accept everything, absent an \
             exclude hit"
        );

        assert!(
            !rules.matches("Claude Model Performance Megathread - August 3"),
            "an exclude term must reject even when the include list is empty \
             -- empty-include-accepts-everything must not short-circuit past \
             the exclude check"
        );

        let rules = RuleSet {
            include: vec![],
            exclude: vec!["ban".into()],
        };

        assert!(
            !rules.matches("Your account has been suspended for a ban"),
            "an exclude term matching at a word boundary should reject"
        );

        assert!(
            rules.matches("urban planning with Claude"),
            "an exclude term must use the same word-boundary rule as \
             include -- \"ban\" mid-word inside \"urban\" must not trigger a \
             reject"
        );
    }

    #[test]
    fn empty_include_list_accepts_everything() {
        let rules = RuleSet {
            include: vec![],
            exclude: vec![],
        };

        assert!(
            rules.matches("literally anything at all"),
            "zero include terms means unfiltered -- every item should match"
        );

        assert!(
            rules.matches(""),
            "an empty haystack should also match when there are no include \
             terms to filter on"
        );
    }

    #[test]
    fn matching_terms_returns_only_the_terms_that_actually_fired() {
        let terms = vec!["hook".to_string(), "skill".to_string()];

        let fired = matching_terms(&terms, "Spent months ignoring Claude Code hooks");
        assert_eq!(fired, vec!["hook"]);

        let fired_none = matching_terms(&terms, "My game demo is on Steam");
        assert!(fired_none.is_empty());

        let fired_both = matching_terms(&terms, "a skill that wraps a hook for you");
        assert_eq!(fired_both, vec!["hook", "skill"]);
    }
}
