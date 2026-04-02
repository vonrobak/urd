---
upi: "004, 005"
date: 2026-04-03
---

# Architectural Adversary Review: Phase A Implementation Plan (UPI 004 + 005)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Implementation plan at `docs/97-plans/2026-04-03-plan-004-005-phase-a.md`
**Mode:** Design review (plan, no code yet)
**Design docs:** UPI 004 (TokenMissing safety gate), UPI 005 (Status truth)

---

## Executive Summary

The plan is sound and well-sequenced. Both UPIs address real correctness issues surfaced by
testing. The main risk is a contradictory approach in Step 7 (assess scoping) where the plan
proposes two different strategies and self-corrects partway through without cleaning up — a
builder following the document linearly will hit confusion. One significant finding: the plan
misses that `build_plan_output()` is called from `backup.rs` too, not just `plan_cmd.rs`.

## What Kills You

**Catastrophic failure mode: sending backups to an impostor drive.** UPI 004 is precisely about
closing this gap. The plan's approach — returning `TokenExpectedButMissing` when SQLite has a
stored token but the drive has no token file — is the right fix at the right layer. Distance
from catastrophe: one missing `if` check. The plan closes this correctly.

**Secondary: false promise states.** UPI 005 fixes `assess()` reporting false degradation. This
is a trust problem, not a data safety problem — false "protected" would be dangerous, but false
"degraded" is merely annoying. No proximity to data loss.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Both fixes target real bugs with correct logic. Step 7 has an unresolved approach conflict. |
| 2 | **Security** | 4 | Token gate correctly fails-closed for identity uncertainty; fail-open for SQLite errors (ADR-107). |
| 3 | **Architectural Excellence** | 4 | Respects pure/impure boundaries, adds variant to existing enum, uses established patterns. |
| 4 | **Systems Design** | 3 | Missing a call site for `build_plan_output`, and Step 7's dual approach needs resolution. |

## Design Tensions

**1. Planner purity vs. token verification completeness.**

The planner calls `drive_availability()` which checks mount + UUID only — it never returns
token variants. The `TokenMismatch` and `TokenMissing` match arms in `plan.rs:186-226` are
only exercisable via `MockFileSystemState`. Real token enforcement happens in `backup.rs` as
a post-plan filter.

The plan adds `TokenExpectedButMissing` to the planner match (Step 2). This is architecturally
correct — the planner must be exhaustive on the enum, and mocks exercise the path. But the plan
should make explicit that **production safety comes from Step 3 (backup.rs), not Step 2
(plan.rs)**. A builder reading Step 2 in isolation might believe the planner handles it in
production.

**Trade-off resolved correctly.** The alternative — making the planner call `verify_drive_token()`
directly — would violate the pure-function contract (ADR-100/108). The two-layer approach
(planner handles it for testability, command handles it for production) is the right architecture.

**2. Display completeness vs. scoping correctness (Step 7).**

The grill-me decision (005-Q1) says: iterate all drives, build full vectors for display, filter
before passing to health computation. The plan proposes: iterate only effective drives, making
all vectors scoped. These are different designs with different display behavior.

Under the plan's approach, a subvolume scoped to `["WD-18TB"]` won't show any 2TB-backup column
in the status table — even if historical snapshots exist on 2TB-backup from before scoping was
configured. The plan self-corrects to accept this, calling it "correct behavior."

This is the right call for a patch. Historical data on unscoped drives is an edge case that
affects display, not safety. The simpler approach (iterate only scoped drives) eliminates an
entire category of filtering bugs. But the grill-me decision should be formally updated to
match — contradictory resolved decisions create confusion in future sessions.

## Findings

### Significant

**S1: `build_plan_output()` is called from `backup.rs`, not just `plan_cmd.rs`.**

`backup.rs:77` and `backup.rs:132` both call `build_plan_output()`. Adding `warnings: Vec<String>`
to `PlanOutput` means the function (or struct construction) must be updated. The plan only
mentions changes in `plan_cmd.rs`.

**Consequence:** Compile error during build, or — if the builder adds a default — backup dry-run
output silently omits token warnings that were shown in `urd plan`.

**Fix:** Step 4 should note that `build_plan_output()` is shared. Simplest approach: set
`warnings` on the output struct after construction rather than adding a parameter. Backup command
already handles token issues via its own filter, so `backup.rs` callers pass empty warnings.

### Moderate

**M1: Step 7 contains two contradictory approaches that aren't cleaned up.**

Lines of the plan describe building `scoped_drive_assessments` and `scoped_chain_health` as
filtered subsets passed to `compute_overall_status()` and `compute_health()`. Then the plan
self-corrects: "Wait — after re-reading: the iteration now only covers `effective_drives`..."
Both approaches remain in the document.

**Consequence:** A builder reading linearly will implement the filter-after-build approach
(unnecessary complexity). Or worse, implement both — filtering an already-scoped vector.

**Fix:** Remove the clone-and-filter discussion. The final approach is: replace
`for drive in &config.drives` with `for drive in &effective_drives`. The vectors built in the
loop are already scoped. Pass them directly to `compute_overall_status()` and `compute_health()`.
No signature changes, no separate scoped vectors, no cloning.

**M2: Retention deletes still execute on a token-suspicious drive.**

The `backup.rs` post-plan filter (Step 3) only removes `SendFull` and `SendIncremental`
operations. `DeleteSnapshot` operations planned for the suspicious drive remain. The planner
plans retention deletes in `plan_external_retention()` which runs in the same iteration as
sends — but only the planner's match arm (Step 2) can skip both. In production, the planner
doesn't see the token issue, so it plans both sends AND deletes. The backup filter removes
sends but leaves deletes.

**Consequence:** Retention deletes execute on an unverified (possibly cloned) drive. This is
likely harmless — deleting redundant copies from a clone doesn't affect the real drive. But
it's architecturally inconsistent: "this drive is suspicious, block writes but allow deletes."

**Fix:** Accept this as a conscious trade-off and document it. Blocking deletes on a suspicious
drive has no safety benefit (the snapshots are copies) and could cause space exhaustion on a
drive that might be legitimately reformatted. Add a brief comment in `backup.rs` near the
filter explaining why only sends are blocked.

### Minor

**m1: Existing `all_known_skip_reasons_classify_correctly` test (output.rs:1286) also uses
`"send disabled"` → `Disabled`.** The plan mentions updating it but only in the Risk Flags
section, not in Step 5's test list. Include it explicitly in Step 5 to prevent a missed test
update.

**m2: `skip_tag()` in voice.rs is an exhaustive match (no wildcard).** Adding `LocalOnly`
without adding a match arm causes a compile error. The plan covers this in Step 6, but the
exhaustive nature isn't called out — worth noting for the builder since this is a common source
of "why won't it compile" confusion when adding enum variants.

### Commendation

**C1: The two-layer token verification respects architectural boundaries perfectly.** Planner
stays pure (ADR-100/108), command layer handles I/O-dependent verification, tests exercise
both paths via mocks. This is exactly how the planner/executor separation should work.

**C2: Risk Flag 1 (existing test will break) is an excellent catch.** The test
`verify_drive_token_no_file_on_drive` sets up the exact scenario the fix targets — SQLite has
a token, drive doesn't. Identifying that this test must change from asserting `TokenMissing`
to asserting `TokenExpectedButMissing` shows the planner read the code, not just the design.

**C3: Step sequencing (dependencies flow forward) is clean.** Each step only touches what was
established in prior steps. No circular dependencies, no "go back and update Step 2 after
Step 5." The two UPIs are cleanly independent despite sharing `output.rs`.

## The Simplicity Question

The plan is appropriately scoped. Nothing here is speculative — every change addresses a
concrete bug or display issue surfaced by testing. No new modules, no new abstractions, no
new traits. The `TokenExpectedButMissing` variant is an enum extension; the `LocalOnly` variant
is an enum extension; the assess scoping is a filter addition. This is how patch-tier work
should look.

**One thing to consider deleting:** The `PlanOutput.warnings` field (Step 4) adds a new
communication channel. Is it necessary? The planner already generates skip reasons for token
issues (Step 2), which appear in the plan's skip section. Adding a separate warning block
means token issues appear in *two* places: skip section ("drive D1 token expected but missing")
AND warning block ("Drive D1 is mounted but missing its identity token...").

Counter-argument: the skip reason is per-subvolume, buried in a list. The warning is drive-level,
prominent, and actionable. A user with 8 subvolumes sending to a suspicious drive would see 8
skip lines but only 1 warning. The warning is the right UX. Keep it.

But — in production, the planner won't generate skip reasons for token issues (it doesn't see
them). Only `plan_cmd.rs` post-plan check generates warnings. So there's no duplication in
production, only in tests. The warning field earns its keep.

## For the Dev Team

Priority order for fixes before building:

1. **Step 4: Note shared call site.** Add to Step 4: "`build_plan_output()` is also called
   from `backup.rs:77` and `backup.rs:132`. Set `warnings` on the struct after construction
   (field default: empty vec) so backup callers don't need changes. `backup.rs` handles token
   issues via its own filter, so it passes no warnings."

2. **Step 7: Remove contradictory text.** Delete the clone-and-filter discussion. The approach
   is: compute `effective_drives` from `subvol.drives`, iterate `effective_drives` instead of
   `config.drives`. Vectors are automatically scoped. No signature changes. No `scoped_*`
   intermediaries. Pass `drive_assessments` and `chain_health_entries` directly — they already
   contain only scoped entries.

3. **Step 3: Document retention-delete trade-off.** Add a comment in `backup.rs` near the
   send filter explaining: "Only sends are blocked for token-suspicious drives. Retention
   deletes proceed — deleting redundant copies from a clone is harmless and prevents space
   exhaustion."

4. **Step 5: Include `all_known_skip_reasons_classify_correctly` in test list.** Move from
   Risk Flags to Step 5's test section. This test will fail when `"send disabled"` changes
   from `Disabled` to `LocalOnly`.

## Open Questions

1. **Should the grill-me resolution 005-Q1 be formally updated?** The plan departs from it
   (iterate scoped drives vs. iterate all + filter). If Q1 is referenced in future sessions,
   the stale decision could cause confusion.

2. **Does `urd backup --dry-run` need token warnings?** Currently `backup.rs:77` calls
   `build_plan_output()` for dry-run. If warnings are populated in `plan_cmd.rs` but not in
   `backup.rs`, a dry-run won't show them. Should dry-run also run the post-plan token check?
   Or is the dry-run's purpose purely "what would the planner do" without command-layer enrichment?
