---
upi: "023"
date: 2026-04-05
---

# Adversary Review: UPI 023 — The Honest Diagnostic

**Project:** Urd  
**Date:** 2026-04-05  
**Scope:** Implementation plan at `.claude/plans/compiled-dazzling-flute.md`, design doc at
`docs/95-ideas/2026-04-05-design-023-honest-diagnostic.md`  
**Mode:** Design review (plan, pre-implementation)  
**Commit:** b1db859 (master)

---

## Executive Summary

A well-scoped presentation-layer fix with one subtle verdict-interaction bug that needs
resolving before implementation. The trust gap fix (Degraded verdict) is the right call
and the highest-value change. The findings-first verify and doctor threads rewrite are
clean presentation work that respects module boundaries. One finding (Significant) about
the `--thorough` interaction with `degraded_count` needs a design decision.

## What Kills You

**Catastrophic failure mode for Urd:** Silent data loss — deleting snapshots that shouldn't
be deleted.

**Distance from this plan:** Very far. This is purely presentation-layer work. No code in
this plan touches retention, pin files, btrfs commands, or the executor. The plan explicitly
changes no behavior — only how existing data is rendered. The closest proximity is indirect:
if the degraded verdict gave false reassurance ("Data is safe") when data *isn't* safe, a
user might not act. But the plan only applies "Data is safe" to Protected subvolumes, which
by definition have data on at least one external drive. The promise model is correct here.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | One verdict-interaction edge case (F1); otherwise solid |
| 2 | Security | 5 | No security surface — presentation only, no sudo, no paths |
| 3 | Architectural Excellence | 4 | Respects module boundaries (R4, R6); clean output.rs/voice.rs separation |
| 4 | Systems Design | 4 | Handles daemon mode correctly; JSON backward-compatible |

## Design Tensions

### T1: Verdict precedence hierarchy (errors > warnings > degraded > healthy)

The plan places `Degraded` below `Warnings` in the verdict hierarchy. This means that
when `--thorough` is used and verify finds any warnings (including `drive-mounted` warnings),
those warnings mask the degraded verdict. This is a conscious trade-off: warnings and errors
represent actionable problems, while degraded is informational. The hierarchy is correct in
principle — but the interaction with `drive-mounted` warnings specifically is problematic
(see F1).

### T2: Classification in voice.rs vs. output.rs (R4 vs. R6)

The plan correctly puts expected-condition classification in voice.rs (rendering judgment)
while putting suggestions in output.rs (domain knowledge). This is the right split — the
same check might be an expected condition in one context but not another, while the
suggestion for a chain break is always "run backup."

## Findings

### F1: `--thorough` drive-mounted warnings mask degraded verdict [Significant]

**What:** When `urd doctor --thorough` runs, verify's `warn_count` (which includes
`drive-mounted` warnings for absent drives) gets added to doctor's `warn_count` at
line 238. The verdict cascade is `errors > warnings > degraded > healthy`. So with
2 absent drives and 2 degraded subvolumes, the user gets `"14 warnings."` instead of
`"2 subvolumes degraded."` The warnings are the *same absent drives* causing the
degradation — the verdict double-counts the condition.

**Consequence:** The exact scenario that motivated this design (user runs doctor,
gets a non-answer) partially recurs with `--thorough`. Plain `urd doctor` works
correctly (no verify, no drive-mounted warnings). But `--thorough` users get a
warning count that obscures the degraded message.

**Suggested fix:** Two options:

**Option A (simple):** When computing the verdict, subtract verify's drive-mounted
warning count from `warn_count` before the cascade. This prevents expected conditions
from masking degraded status:
```rust
// After verify count accumulation
let verify_expected_warns = verify_output.as_ref().map_or(0, |v| {
    v.subvolumes.iter().flat_map(|sv| &sv.drives)
        .flat_map(|d| &d.checks)
        .filter(|c| c.name == "drive-mounted" && c.status == "warn")
        .count()
});
let effective_warn_count = warn_count - verify_expected_warns;
```
Then use `effective_warn_count` in the verdict cascade.

**Option B (defer):** Accept this as a known limitation. Plain `urd doctor` (which R2
directs users toward) works correctly. Document that `--thorough` may show a higher
warning count that includes expected conditions from verify.

**Recommendation:** Option B for this implementation. The fix in Option A puts
classification logic in doctor.rs (the same `drive-mounted` string matching), which
muddies the module boundary. The `--thorough` interaction is a real but secondary issue
— the primary trust gap (plain doctor) is fully resolved. Revisit if users report
confusion.

### F2: Verify summary line counts could drift from VerifyOutput totals [Moderate]

**What:** The findings-first renderer in Step 3e computes its own counts by iterating
checks (findings vs. expected conditions), then renders `"N subvolumes verified, M
checks OK."` using `data.ok_count`. But `data.ok_count` was computed in verify.rs and
includes all OK checks regardless of classification. The summary line uses this total
alongside a separately-computed absent-drive count.

**Consequence:** No actual bug — `ok_count` is correct regardless of classification.
But the two counting paths (verify.rs totals vs. voice.rs classification) create a
maintenance coupling. If a new expected-condition type is added (not just drive-mounted),
the summary's arithmetic could become confusing: "34 checks OK, 2 drives skipped" when
the total checks were 37 (34 OK + 2 drive-mounted + 1 fail) — the user might wonder
where the missing check went.

**Suggested fix:** Note this in a code comment where the summary line is rendered:
`// ok_count is the verify.rs total; it doesn't include drive-mounted warnings.`
No code change needed — just documentation of the relationship.

### F3: Commendation — R6 (suggestion field on VerifyCheck)

The grill-me session's pivot from string-matching ("Chain broken" in detail text) to a
structured `suggestion` field is the right call. It respects the module boundary table:
verify.rs knows the domain (what's broken, what fixes it), voice.rs knows the
presentation (how to render it). This is exactly how output.rs is supposed to work —
structured data in, rendering decisions out. The original design's approach would have
embedded verify's domain vocabulary into voice.rs as a stringly-typed contract.

### F4: Commendation — Sequencing

Shipping the trust gap fix (Step 1) first means the highest-value change lands with the
smallest diff. If implementation runs long or gets interrupted, the most important fix
is already in. The dependency ordering (Step 2 needs Step 1's Degraded variant, Step 4
reuses Step 3's classification pattern) is correct and well-reasoned.

### F5: Commendation — "What NOT to Change" section

Explicitly listing what stays unchanged (R2: status advice text, daemon paths, no new
modules) prevents scope creep during implementation. This is particularly valuable for
a presentation-layer change where it's tempting to "fix one more thing while we're here."

## The Simplicity Question

**What could be removed?** Nothing. The plan is already lean — four focused changes in
five files, no new modules, no new abstractions beyond a `pluralize()` helper. The
rejected alternatives (interactive doctor, generic compression utility, streaming output)
were all correctly rejected for adding machinery this doesn't need.

**What's earning its keep?** The `suggestion` field on VerifyCheck (R6) earns its keep
immediately — it eliminates string matching and enables the doctor Threads section to
render suggestions without domain knowledge. The `--detail` flag earns its keep by
preserving the current verbose output for debugging without cluttering the default view.

## For the Dev Team

Priority-ordered action items:

1. **Decide on F1 (--thorough masking).** Before implementing Step 1, decide whether to
   accept Option B (document the limitation) or implement Option A (subtract expected
   warnings). Recommendation: Option B — ship the primary fix, revisit if it matters in
   practice. Add a code comment at the verdict block noting the known interaction.

2. **Implement as sequenced.** Steps 1→2→3→4. No reordering needed. Each step has a
   clean checkpoint.

3. **Step 3 mechanical updates.** The `suggestion: None` addition to ~16 VerifyCheck sites
   is tedious but the compiler catches all of them. Don't try to be clever — just add the
   field and move on.

4. **Step 3f test decision.** The existing `verify_interactive_shows_checks` test asserts
   on `"OK"` text which won't appear in findings-first mode. The plan offers two options
   (switch to `detail: true` or rewrite assertions). Recommend: switch to `detail: true` —
   the test's intent is "all check statuses render," which is what detail mode tests.

## Open Questions

1. **F1 decision:** Accept the `--thorough` masking as a known limitation (Option B), or
   fix it now (Option A)? This is the only finding that needs a decision before implementation.
