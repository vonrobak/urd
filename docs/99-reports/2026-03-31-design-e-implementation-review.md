# Architectural Review: 6-E Promise Redundancy Encoding (Implementation)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-31
**Scope:** Implementation of Design 6-E. Files: awareness.rs, preflight.rs, output.rs,
voice.rs, commands/status.rs, commands/backup.rs, sentinel_runner.rs, heartbeat.rs,
sentinel.rs, ADR-110 addendum.
**Reviewer:** Architectural adversary
**Commit:** uncommitted working tree (post-simplify pass)
**Mode:** Implementation review

---

## Executive Summary

Solid implementation of a well-designed feature. The overlay pattern preserves the ADR-110
Invariant 6 contract cleanly, and the one-directional degradation guard (`offsite_freshness <
assessment.status`) is correct and tested. One significant finding: the design doc specifies
that the 7-day "consider cycling" advisory should be **replaced** for resilient subvolumes,
but the implementation **adds alongside** it, producing duplicate advisories. The remaining
findings are moderate or minor.

## What Kills You

**Catastrophic failure mode: silent data loss.** This feature is purely observational — it
changes promise *reporting*, not backup *operations*. No snapshot creation, deletion, or
send logic is touched. Distance from catastrophic failure: **far**. The overlay can only
degrade status (never improve), and status degradation has no operational side effects — it's
presentation-layer information that flows into heartbeat, notifications, and voice rendering.

The closest this gets to danger is notification fatigue: if the overlay generates spurious
or duplicate advisories, the user learns to ignore warnings, and a real warning gets lost.
The duplicate advisory finding (S1) is relevant through this lens.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Core logic is right. One-way degradation guard is sound. Duplicate advisory is a gap, not a bug. |
| 2 | Security | 5 | No new privilege boundaries, no new paths to btrfs. Purely observational. |
| 3 | Architectural Excellence | 5 | Overlay pattern preserves awareness purity beautifully. Post-processing is the right call. |
| 4 | Systems Design | 4 | Notification flow is correctly wired. Sentinel state persistence captures overlaid values. Advisory duplication is the gap. |
| 5 | Rust Idioms | 4 | Clean iterator chains, proper Ord derivation, DriveRole on output types (after simplify). |
| 6 | Code Quality | 4 | Good test coverage (14 new tests), clear naming, concise implementation. Two test gaps. |

## Design Tensions

### 1. Awareness purity vs. caller convenience

The overlay must be called separately at every call site (status, backup ×2, sentinel) because
awareness is protection-level-blind. This distributes a one-line call across four locations.
The alternative — threading protection_level into assess() — was correctly rejected per ADR-110
Invariant 6. The four call sites are a small price for preserving the invariant. **Right call.**

### 2. Fixed thresholds vs. configurability

The 30/90-day thresholds are hardcoded constants, not user-configurable. This enforces the
opacity principle (ADR-110) but means quarterly rotators must use custom. The design doc
explicitly acknowledges this trade-off and chose correctly — a configurable threshold on a
named level undermines what "named level" means. **Right call.**

### 3. Advisory as string vs. structured data

The overlay pushes a plain string advisory. The design doc's Open Question #1 asks whether
advisories should carry structured data. For this feature, strings are adequate — voice.rs
renders them directly and doesn't need to branch on advisory type. But the duplicate advisory
issue (S1) would be easier to fix with structured advisories. **Acceptable for now, tension
will resurface with feature 6-I.**

## Findings

### S1: Duplicate advisory for resilient subvolumes (Significant)

**What:** The design doc (section "awareness.rs — 7-day advisory replaced for resilient
subvolumes") says the existing "consider cycling" advisory is **replaced** by the structured
offsite freshness system for resilient subvolumes. The implementation does not replace it —
it adds alongside it.

**Consequence:** A resilient subvolume with an unmounted offsite drive last sent 35 days ago
will show:
```
NOTE sv1: offsite drive WD-18TB1 last sent 35 days ago — consider cycling
NOTE sv1: offsite copy stale — resilient promise degraded
```

The first advisory is generated unconditionally in `assess()` (line 284-293) for any unmounted
drive with send age > 7 days, regardless of protection level. The overlay then adds its own
advisory without removing the old one.

This is not a correctness bug — both statements are true. But duplicate advisories for the
same condition train the user to skim warnings, which is the notification fatigue path to
missing a real problem.

**Fix:** The overlay should filter out the legacy "consider cycling" advisory for resilient
subvolumes when it applies its own degradation advisory. Either:
- (a) In `overlay_offsite_freshness()`, remove advisories matching the "consider cycling"
  pattern before pushing the new one.
- (b) In `assess()`, skip the 7-day advisory for drives whose role is `Offsite` (awareness
  now has `DriveRole` in scope). This is simpler but slightly breaks the protection-level-blind
  contract — though the advisory is generated per-drive, not per-protection-level, so filtering
  by drive role preserves Invariant 6.

Option (b) is cleaner. The existing advisory comment even says "Advisory for stale offsite
drives" but the code doesn't actually filter by `DriveRole::Offsite` — it fires for all
unmounted drives.

### S2: 7-day advisory fires for all unmounted drives, not just offsite (Significant)

**What:** The comment on line 284 says "Advisory for stale offsite drives" but the code has
no `drive.role` filter. A primary drive that's been unmounted for 8 days also gets "consider
cycling," which is misleading — you don't "cycle" a primary drive.

**Consequence:** The advisory text says "consider cycling" which implies an offsite rotation
workflow. For a primary drive that's simply disconnected, this is confusing guidance. With the
`DriveRole` now available on `DriveAssessment`, this can be scoped correctly.

**Fix:** Add `&& drive.role == DriveRole::Offsite` to the condition at line 284. This scopes
the advisory to drives where "cycling" makes semantic sense, and naturally eliminates the
duplicate for resilient subvolumes when combined with the overlay's own advisory.

### M1: Two test gaps in overlay coverage (Moderate)

**What:** Two boundary conditions are untested:

1. **Already-Unprotected resilient subvolume.** If `assessment.status == Unprotected` before
   the overlay runs (e.g., local snapshots are stale), the overlay should NOT add its advisory
   (because `offsite_freshness < Unprotected` is false for any value). This is correct behavior
   but implicit — a test documents the invariant that the overlay doesn't pile on when things
   are already worst-case.

2. **Equality boundary (offsite_freshness == assessment.status).** If both are `AtRisk`, the
   overlay should not update. The `<` comparison handles this, but an explicit test would catch
   a future regression to `<=`.

**Fix:** Add two tests:
```rust
#[test]
fn overlay_already_unprotected_no_redundant_advisory() { ... }

#[test]
fn overlay_equal_status_no_change() { ... }
```

### M2: `InitDriveStatus.role` still stringly-typed (Moderate)

**What:** The simplify pass correctly converted `StatusDriveAssessment.role` and `DriveInfo.role`
from `String` to `DriveRole`, but `InitDriveStatus` in output.rs (line 556) still uses
`role: String`. This is pre-existing but now inconsistent with the two sibling structs.

**Fix:** Convert `InitDriveStatus.role` to `DriveRole` for consistency. This is a one-file
change with one construction site.

### C1: Post-processing overlay is the right architecture (Commendation)

The decision to implement offsite freshness as a post-processing overlay rather than threading
protection_level into `assess()` is the key architectural choice in this feature. It preserves
ADR-110 Invariant 6 (awareness is protection-level-blind) while still producing correct final
assessments. The call sites are explicit — you can grep for `overlay_offsite_freshness` and
find every place where protection-level-aware assessment happens. This is the right trade-off
between purity and practicality.

### C2: One-way degradation guard (Commendation)

The `if offsite_freshness < assessment.status` guard in the overlay is exactly right. Combined
with the `PromiseStatus` Ord derivation (worst-to-best: Unprotected < AtRisk < Protected),
this guarantees the overlay can only make things worse, never better. The invariant is
structural — enforced by the type system's Ord implementation — not behavioral. This means
it can't be broken by future code changes to the overlay body.

### C3: Preflight drives-in-scope unification (Commendation)

The simplify pass hoisted `drives_in_scope` before both the drive-count check and the
resilient-without-offsite check, eliminating a duplicated match expression. This is a small
change that removes a maintenance trap — if drive scoping rules change (e.g., ADR-111 drive
groups), there's now one place to update instead of two.

## Also Noted

- `overlay_offsite_freshness` calls `config.resolved_subvolumes()` redundantly (assess() already
  called it). Acceptable — architectural separation justifies the cost, and N is 3-5.
- The advisory string "offsite copy stale — resilient promise degraded" is hardcoded in the
  overlay. If structured advisories are adopted (design Open Question #1), this becomes an enum.
- No test for a subvolume that exists in assessments but was removed from config between
  `assess()` and the overlay. The code handles it correctly (skips via `None`), but it's an
  untested edge case.

## The Simplicity Question

**What could be removed?** Nothing. The feature is minimal:
- One pure function (overlay) + one helper (compute_offsite_freshness)
- One preflight check
- One plumbing addition (DriveRole on DriveAssessment)
- Four one-line call site additions

The overlay is 22 lines of production code. The helper is 20. The preflight check is 8.
Total new production logic: ~50 lines. This is proportional to the feature's scope.

**What's earning its keep?** The post-processing pattern. It looks like overhead (four call
sites instead of one integrated check), but it buys testability and architectural clarity.
The overlay tests don't need a `MockFileSystemState` — they construct assessments directly
and verify mutations. That's the payoff of purity.

## For the Dev Team

Priority-ordered action items:

1. **Fix duplicate advisory (S1 + S2).** In `assess()` at line 284, add
   `&& drive.role == DriveRole::Offsite` to scope the 7-day "consider cycling" advisory to
   actual offsite drives. This fixes both the misleading message for primary drives (S2)
   and eliminates the duplicate for resilient subvolumes (S1) since the overlay provides
   its own degradation advisory for that case. One condition change, one test to verify
   primary drives no longer get the "consider cycling" advisory.

2. **Add two test cases (M1).** Add `overlay_already_unprotected_no_redundant_advisory` and
   `overlay_equal_status_no_change` to the overlay test suite. Each is ~10 lines using the
   existing test helpers.

3. **Convert `InitDriveStatus.role` to `DriveRole` (M2).** In output.rs line 556, change
   `pub role: String` to `pub role: DriveRole`. Update the one construction site in
   commands/init.rs and the test in voice.rs. Consistency with the two sibling structs.

## Open Questions

1. **Should the overlay advisory include the drive label and age?** Currently it says "offsite
   copy stale — resilient promise degraded" without naming which drive or how stale. The legacy
   advisory includes both. Adding specifics would make the overlay advisory self-sufficient and
   strengthen the case for removing the legacy one.

2. **Should there be an integration test for the full assess → overlay → notification flow?**
   The unit tests verify each piece, but no test verifies that a resilient subvolume with a
   stale offsite drive produces the correct notification through the sentinel pipeline. This
   is low risk (the wiring is trivial) but would catch a regression in call site ordering.
