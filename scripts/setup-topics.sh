#!/usr/bin/env bash
# Reproducible drip topic/source/rule configuration.
#
# Every command here is a DECLARATIVE UPSERT (bd issue drip-98u.8): `--match`
# and `--exclude` REPLACE a link's term lists wholesale, so re-running this
# whole script is idempotent and produces identical state. Editing rules never
# touches a source's dedup ledger (`seen_items`).
#
# This file IS the config. Edit it, re-run it, commit it.
#
# Usage:  DRIP=/path/to/drip ./scripts/setup-topics.sh
#         (defaults to the installed `drip` on PATH)

set -euo pipefail
DRIP="${DRIP:-drip}"

echo "== 1. topic tree =="
# The migrated tree is `Claude` -> `Claude (general)` and `Rust` -> `Rust
# (general)`. Rename rather than build alongside, so there are no duplicate
# topics. Rename is future-notes-only: already-written digests keep their old
# `## Claude` headings, which is intended.
$DRIP topic rename --name "Claude" --to "AI engineering"    || true
$DRIP topic rename --name "Rust (general)" --to "rust general" || true

for sub in "loop engineering" "context engineering" "claude code skills" \
           "hooks and automation" "spec-driven development"; do
  $DRIP topic add --name "$sub" --parent "AI engineering" || true
done

echo
echo "== 2. source-level exclude pre-filters (BROAD sources only) =="
# Title-only, applied before any routing. Only the broad `hot` feeds need
# these -- the search-scoped sources are already narrow.
#
# Terms chosen from real observed noise in these subreddits: pricing/limit
# complaints, account drama, and complaint megathreads. Prefix-at-word-boundary
# matching means `ban` matches "banned" but NOT "urban".
#
# NOTE: re-running `source add` for a search-scoped source would REBUILD its
# feed URL from --sort/--search and could silently change it, so only the two
# plain-`hot` sources are re-added here. The rest are configured via `link`.
NOISE="pricing,megathread,suspend,unsubscrib,refund,cancel,downgrade,rant,lawsuit"

$DRIP source add --kind reddit --url anthropic --name Anthropic \
  --topic "context engineering" --exclude "$NOISE"
$DRIP source add --kind reddit --url claudeskills --name ClaudeSkills \
  --topic "claude code skills" --exclude "$NOISE"

echo
echo "== 3. links + keyword rules =="
# MEASURED CORRECTION (2026-08-08): search-scoped sources are NOT narrow
# enough to leave ruleless. Reddit's search matches body text and related
# terms, so a `--search hooks` feed returns posts that merely mention hooks in
# passing. Measured on a real dry run with ruleless links:
#
#   claude code skills   11/11 on-topic   (had keyword rules)
#   context engineering   6/12            (ruleless)
#   loop engineering      4/11            (ruleless)
#   hooks and automation  1/10            (ruleless)
#
# Search buys REACH, not precision (bd issue drip-98u.9 measured the reach:
# 88% of search results are unreachable from `hot`). Both layers are needed:
# search to retrieve, rules to route. So every link below carries rules.
#
# Title-only (no --match-body) on purpose: the search already matched the
# body, so matching the body again just re-admits the loose hits.
$DRIP source link --name cc-hooks --topic "hooks and automation" \
  --match "hook,pretooluse,posttooluse,settings.json,guardrail,automat,notification"
$DRIP source link --name cc-spec --topic "spec-driven development" \
  --match "spec,plan,prd,design doc,requirement,tdd,test-driven"
$DRIP source link --name ClaudeCode --topic "loop engineering" \
  --match "agent,subagent,orchestrat,harness,loop,parallel,pipeline,worktree,swarm"
$DRIP source link --name ClaudeAI --topic "context engineering" \
  --match "mcp,context,memory,token,claude.md,retrieval,knowledge,compact"

# rust-hot stays ruleless deliberately: "rust general" is a catch-all for a
# single-subject subreddit, not a routed topic.
$DRIP source link --name rust-hot --topic "rust general"

# -- broad: keyword-routed, and matched against the body too, since Reddit
#    self-post titles are frequently opaque (measured: 7 of 8 on-topic hits
#    matched only in the body -- bd issue drip-98u.2)
$DRIP source link --name Anthropic --topic "loop engineering" \
  --match "agent,subagent,orchestrat,harness,autonom,loop" --match-body
$DRIP source link --name Anthropic --topic "context engineering" \
  --match "context,memory,token,compact,claude.md,mcp,retrieval" --match-body

$DRIP source link --name ClaudeSkills --topic "claude code skills" \
  --match "skill,plugin,progressive disclosure" --match-body

echo
echo "== 4. YouTube =="
$DRIP source add --kind youtube --url "https://www.youtube.com/@mattpocockuk" \
  --name mattpocockuk --topic "loop engineering"
# Descriptive titles, and the body is a sponsor-link blurb -> title-only.
$DRIP source link --name mattpocockuk --topic "loop engineering" \
  --match "agent,loop,harness,orchestrat,autonom,subagent"
$DRIP source link --name mattpocockuk --topic "claude code skills" \
  --match "skill,plugin"
$DRIP source link --name mattpocockuk --topic "context engineering" \
  --match "context,memory,token,claude.md,mcp"

echo
echo "== 5. retire the migration's placeholder sub-topic =="
# Every source has been relinked above, so `Claude (general)` is now empty.
# `topic remove` refuses while any link remains, so this is safe: if it
# errors, something above did not relink.
for s in Anthropic ClaudeAI ClaudeCode ClaudeSkills cc-hooks cc-spec; do
  $DRIP source unlink --name "$s" --topic "Claude (general)" 2>/dev/null || true
done
$DRIP topic remove --name "Claude (general)" || true

echo
echo "== result =="
$DRIP topic list
echo
$DRIP source list
