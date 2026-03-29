# Architectural Review: UX-1 Plan Output Improvements (D5 + D1)

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review of uncommitted changes in `src/output.rs`, `src/voice.rs`, `src/commands/plan_cmd.rs`, `src/commands/backup.rs`
**Base commit:** `3b6301c`
**Reviewer:** arch-adversary

---

## Executive summary

Clean, well-scoped presentation-layer change. The implementation adds a `SkipCategory` enum
to classify skip reasons for grouped rendering in `urd plan` output and enriched JSON for
daemon consumers. No proximity to catastrophic failure modes — this touches only the
rendering pipeline, not the planner, executor, or btrfs layer. Two findings worth addressing:
a silent coupling between classification and rendering that will break on new skip patterns,
and duplicated duration formatting logic.

## What kills you

**Catastrophic failure mode:** Silent data loss via incorrect snapshot deletion.

**Distance from this change:** Not reachable. This change operates entirely in the
presentation layer (output.rs types, voice.rs rendering). It cannot influence which
snapshots are created, sent, retained, or deleted. The planner's skip reasons remain
free-text strings in plan.rs — this change only classifies them after the fact for display.
No backup decisions depend on `SkipCategory`.

**Verdict:** Safe. No catastrophic failure checklist items triggered.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | All 14 patterns classified correctly with completeness test. One edge case in rendering (S1). |
| 2 | Security | 5 | No security surface — pure presentation, no privileged operations. |
| 3 | Architectural Excellence | 4 | Clean separation: enum in output.rs, rendering in voice.rs, classification at boundary. |
| 4 | Systems Design | 4 | JSON enrichment is additive/non-breaking. Duration parsing handles all `format_duration_short` outputs. |
| 5 | Rust Idioms | 4 | Good use of iterators, `#[must_use]`, serde rename. Minor: could use `IndexMap` but array iteration is fine. |
| 6 | Code Quality | 4 | 22 new tests, clear function decomposition, good doc comments. |

## Design tensions

### 1. Stringly-typed classification vs structured skip reasons

**Trade-off:** `from_reason()` classifies free-text reason strings rather than plan.rs
producing structured `(SkipCategory, String)` tuples directly.

**Why this was chosen:** Avoids touching plan.rs, keeps the output boundary as the
classification point, and plan.rs remains a pure planner that doesn't know about presentation
concerns.

**Verdict:** Right call for now. The coupling is one-directional (output.rs reads plan.rs
patterns) and the completeness test catches drift. If plan.rs grows past ~20 skip patterns,
consider making the planner emit structured skip types — but that's a larger refactor that
should wait for ADR-111 config migration when plan.rs will change anyway.

### 2. Coexistence of two grouping approaches

**Trade-off:** `render_skipped_block` (backup summary) still uses ad-hoc string pattern
matching for "drive not mounted" grouping. The new `render_plan_skipped_grouped` uses the
`SkipCategory` enum. Both coexist.

**Why:** Different rendering requirements (backup summary shows send counts, plan output
shows subvolume counts per drive). Unifying would require refactoring backup summary
rendering — out of scope.

**Verdict:** Acceptable. The journal correctly flagged this as future work. The backup
summary could adopt `SkipCategory` later, but the current ad-hoc approach works and
touching it would expand scope for no functional gain.

## Findings

### S1 — Significant: `render_drive_not_mounted_group` re-parses reason strings after classification

**What:** `render_drive_not_mounted_group` (voice.rs:868-888) extracts drive labels by
parsing the reason string again with `strip_prefix("drive ").strip_suffix(" not mounted")`.
If `from_reason()` classifies a reason as `DriveNotMounted` but the reason format changes
slightly (e.g., "drive WD-18TB is not mounted"), the category would still match but label
extraction would fail, falling back to `"unknown"`.

**Consequence:** The output would show `Not mounted: unknown (6 subvolumes)` — confusing
but not dangerous. The user loses the drive label information that makes the grouped output
useful.

**Why this matters:** The category classifier and the label extractor parse the same string
with different patterns but no shared logic. They can drift independently.

**Suggested fix:** Extract the drive label during classification and store it. Either:
- (a) Add a `drive_label()` method on `SkippedSubvolume` that extracts the label (single
  source of truth for the parsing logic), or
- (b) Accept the current approach but add a test in voice.rs that verifies label extraction
  succeeds for a `DriveNotMounted`-classified reason — this catches drift without adding
  a new type.

Option (b) is simpler and sufficient.

### M1 — Moderate: Duplicated duration formatting logic

**What:** `render_interval_group` (voice.rs:898-906) re-implements `format_duration_short`
from plan.rs (plan.rs:596-604) to convert minutes back to a human string. The two
implementations must stay in sync.

**Consequence:** If `format_duration_short` changes (e.g., adds hours+minutes for >1 day
durations like "1d2h"), the rendering would produce a different format than what the planner
emits, creating visual inconsistency.

**Suggested fix:** Either:
- (a) Make `format_duration_short` public in plan.rs and call it from voice.rs, or
- (b) Move the formatting function to a shared location (types.rs or output.rs).

Option (a) is one line: `pub fn format_duration_short`.

### M2 — Moderate: Completeness test is pattern-level, not source-level

**What:** The `classify_all_14_patterns` test (output.rs) hardcodes 14 representative
reason strings. When someone adds a 15th skip reason in plan.rs, the test still passes
with 14 assertions — there's no mechanism to detect the gap.

**Consequence:** New skip patterns silently fall to `Other` until someone notices the
plan output doesn't group them. Not dangerous (Other renders correctly), but defeats the
purpose of the completeness test.

**Suggested fix:** Add a comment in plan.rs at each `skipped.push()` call site referencing
the completeness test: `// NOTE: new patterns must be added to output::tests::classify_all_14_patterns`.
Alternatively, grep for `skipped.push` in plan.rs from the test and assert the count matches,
though that's fragile.

### Minor — "1 subvolumes" grammar

**What:** `render_drive_not_mounted_group` always uses "subvolumes" (plural), even when
count is 1: `"2TB-backup (1 subvolumes)"`. The test
`plan_grouped_drive_not_mounted` even asserts this incorrect grammar.

**Suggested fix:** `if count == 1 { "subvolume" } else { "subvolumes" }`.

### Commendation — Duration parsing with cross-unit test

The `parse_duration_to_minutes` function and the cross-unit comparison test
(`parse_duration_cross_unit_comparison`) directly address the bug caught in the simplify
pass. The test name and doc comment explain *why* this test exists — it's not obvious that
"9d" vs "2h30m" comparison is a real failure mode. This is exactly the kind of test that
prevents regression: it encodes a discovered bug, not a specification.

### Commendation — Classification at the output boundary

Placing `from_reason()` on `SkipCategory` in output.rs — rather than in plan.rs or voice.rs —
respects the module boundaries cleanly. Plan.rs stays a pure planner (doesn't know about
presentation). Voice.rs stays a pure renderer (doesn't parse business logic). Output.rs is the
translation layer between them, which is where classification belongs. This is the right module
boundary.

## The simplicity question

**What's earning its keep:** The `SkipCategory` enum justifies itself — it collapses 20+ lines
into ~4 and enriches JSON output. The 5 variants match real-world skip distributions well. The
category renderers are short, single-purpose functions.

**What could be simpler:** The duration re-formatting in `render_interval_group` duplicates
plan.rs logic (see M1). One public function eliminates the duplication.

**What could be removed:** Nothing. The implementation is minimal for the design requirements.

## For the Dev Team

Priority order:

1. **[Minor] Fix "1 subvolumes" grammar** — `src/voice.rs:render_drive_not_mounted_group`.
   Use `if count == 1 { "subvolume" } else { "subvolumes" }`. Update the test assertion too.

2. **[S1] Add label extraction test** — `src/voice.rs` tests. Add a test that constructs a
   `DriveNotMounted` skip, renders it via `render_plan_interactive`, and asserts the actual
   drive label (not "unknown") appears in the output. This catches drift between the
   classifier and the label extractor.

3. **[M1] Deduplicate duration formatting** — Make `format_duration_short` in
   `src/plan.rs:596` public. Import and call it from `render_interval_group` in voice.rs
   instead of reimplementing.

4. **[M2] Add skip pattern breadcrumb** — Add a comment at each `skipped.push()` site in
   `src/plan.rs` noting the completeness test in output.rs.

## Open questions

- **Backup summary unification:** The journal and design review both noted that
  `render_skipped_block` could adopt `SkipCategory`. Is this planned for UX-2 or deferred
  further? Having two grouping approaches is fine short-term but creates a maintenance
  surface if someone changes skip reason wording.

- **Sentinel daemon consumption:** The JSON `category` field is documented as non-breaking
  and additive. Does the sentinel or any downstream consumer currently parse skip reasons
  from JSON? If so, they could switch to using the `category` field instead.
