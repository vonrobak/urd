# Arch-Adversary Review: Phase 4a+4b Implementation

**Project:** Urd
**Date:** 2026-04-01
**Scope:** Phase 4a (Staleness Escalation) + Phase 4b (Next-Action Suggestions) in `src/voice.rs`
**Reviewer:** arch-adversary
**Commit context:** Uncommitted changes on master

---

## 1. Executive Summary

Clean, well-scoped implementation that stays entirely within `voice.rs` as promised.
The S1 finding from the design review (voice contradicting awareness) is properly
resolved by deriving escalation text from the awareness model's own status strings
rather than independent hardcoded thresholds. All review findings (S1, M1, M2, m1)
are addressed. No architectural damage.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss from snapshot deletion.**

This change is **4+ bugs away** from catastrophic failure. The implementation:
- Adds no I/O, no filesystem operations, no btrfs calls
- Touches only `voice.rs` (pure presentation layer)
- Reads existing structured output types, writes only to string buffers
- Cannot influence retention, planning, or execution decisions

There is no path from this code to data loss. This is the safest category of change
in the Urd codebase.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 5/5 | Escalation calibrated to awareness status; edge cases covered; tests thorough |
| Security | 5/5 | No filesystem writes, no privilege escalation, pure string formatting |
| Architectural Excellence | 5/5 | Entirely within voice.rs; no module boundary violations; purity preserved |
| Systems Design | 4/5 | String-matching on status values is a minor fragility (see M1) |
| UX Quality | 4/5 | Silence-when-healthy upheld; one question on PROTECTED drives showing staleness text (see M2) |
| Testing | 5/5 | 28 new tests covering units, integration, edge cases; 681 total, all passing |

**Overall: 4.7/5** -- Exemplary presentation-layer feature implementation.

---

## 4. Design Tensions

### Tension 1: String-matching vs typed enums for status

`status_severity()` matches on `"UNPROTECTED"`, `"AT RISK"`, `"PROTECTED"` as raw
strings. These values originate in `awareness.rs` and flow through `StatusAssessment`
as `String` fields. If awareness ever changes these string values, the voice layer
silently degrades to the default case rather than failing loudly. This is
fail-safe (PROTECTED drives get mild text, not alarming text), but it's invisible
coupling. The existing codebase already has this pattern throughout voice.rs (matching
on `a.status == "UNPROTECTED"` etc.), so this is consistent, not novel debt.

### Tension 2: Escalated text for PROTECTED disconnected drives

When a drive is disconnected but all subvolumes are still PROTECTED, the escalation
function returns text like `"WD-18TB away -- 3d"`. This replaces the previous
role-based fallback (`"away"` for offsite, `"disconnected"` for primary). The new
behavior always shows age, which adds information. But it also means a PROTECTED
primary drive that's been disconnected for 1 day now shows age data where before
it would just say "disconnected". This is arguably better UX (more informative),
but it changes the existing behavior for a case that wasn't broken.

### Tension 3: `urd calibrate` suggestion for space-skipped plans

The suggestion `"Run urd calibrate to review retention"` is reworded from the
design's `"measure actual snapshot sizes"` -- the implementation focuses on
retention review. Both commands exist, so the M2 finding from the design review
is fully resolved. The rewording is arguably clearer about what the user should
actually do.

---

## 5. Findings

### Moderate

**M1: Status string fragility is consistent but worth documenting.**

`status_severity()` matches `"UNPROTECTED"`, `"AT RISK"`, and falls through to 0
for anything else. These strings are the canonical awareness model outputs, but
they're not defined as constants anywhere -- they're string literals in both
`awareness.rs` and `voice.rs`. If a future change to awareness modified the
string (e.g., `"AT_RISK"` vs `"AT RISK"`), voice would silently treat it as
PROTECTED.

The fallback behavior is **fail-safe** (unknown status gets gentle text, not
alarming text), which is correct per ADR-107 (fail-open for reads). But it's
invisible degradation.

**Recommendation:** Not blocking. Consider adding status string constants in
`output.rs` that both `awareness.rs` and `voice.rs` reference. This is existing
debt, not new debt from this PR.

**M2: PROTECTED drives always show escalated text when age data exists.**

`escalated_staleness_text` returns `Some(...)` for any `worst_status` when
`max_age_secs` is `Some(...)`, including PROTECTED. This means a primary drive
disconnected for 1 hour still gets `"TestDrive away -- 1h"` instead of the
previous plain `"disconnected"`. The existing test
(`unmounted_primary_drive_shows_disconnected`) was updated to expect the new
behavior.

This is defensible -- showing age is strictly more informative. But it eliminates
the role-based distinction between "away" (offsite) and "disconnected" (primary)
for drives that are PROTECTED. The offsite/primary role distinction in the
fallback path (lines 425-430) is now only reached when there's no age data at
all, which is a rare edge case (no sends ever completed to this drive).

**Recommendation:** Acceptable as-is. The age information is more useful than
the role-based label. The fallback still handles the no-data edge case correctly.

### Minor

**m1: `aggregate_drive_staleness` returns borrowed `&str` with lifetime tied to
input, but default is `&'static str`.**

The function signature returns `(&'a str, Option<i64>)` where `'a` is the
lifetime of the assessments slice. The default return value `"PROTECTED"` is
`&'static str`, which coerces correctly to `&'a str`. This compiles and is safe,
but the mixed provenance (sometimes from the input data, sometimes a static
string) could surprise a future reader.

**Recommendation:** No action needed. A comment would be gold-plating. The
compiler enforces correctness here.

**m2: `SuggestionContext::Doctor` has no fields, unlike the design's
`Doctor { all_clear: bool }`.**

The design proposed a boolean field. The implementation uses a unit variant and
always returns `None`. This is the correct resolution of the M1 design finding
("Doctor all-clear violates silence-when-healthy"). By making it a unit variant,
the code makes it impossible to accidentally add a Doctor suggestion without
changing the enum definition. Good decision.

**m3: `append_suggestion` adds a blank line before the suggestion (`writeln!(out).ok()`).**

This creates consistent vertical spacing, but the blank line appears even when
the output above already ends with a newline. In practice this means a double
blank line before suggestions in some commands. This is a cosmetic nit.

**Recommendation:** Verify the visual output of `urd status` and `urd plan` with
real data to confirm spacing looks right.

### Commendation

**C1: S1 design finding resolved elegantly.**

The design review's most significant finding was that voice thresholds could
contradict awareness status. The implementation eliminates this entirely by
reading the awareness status directly from `StatusDriveAssessment.status` and
calibrating voice text to it. There are no independent voice thresholds at all.
The three-tier text (PROTECTED/AT RISK/UNPROTECTED) maps 1:1 to awareness
states. This is simpler and more correct than either option proposed in the
design review.

**C2: Test coverage is thorough and well-structured.**

28 new tests covering: `status_severity` ordering, `aggregate_drive_staleness`
(single subvol, worst-across-subvols, max-age, no-match), `escalated_staleness_text`
(all three tiers plus None), `suggest_next_action` (every variant, both healthy
and unhealthy), and integration tests (status with degraded data, default status
healthy/unhealthy, doctor no-suggestion). The no-match test for
`aggregate_drive_staleness` exercises the "PROTECTED"/None default path.

**C3: Module purity preserved perfectly.**

Every new function is pure: inputs in, string out. `SuggestionContext` is private
to `voice.rs`. No imports from I/O modules. No state. No side effects. This is
textbook ADR-108 adherence.

**C4: Design review findings addressed systematically.**

- S1 (voice/awareness contradiction): Resolved by deriving from awareness status
- M1 (Doctor all-clear noise): Resolved by making Doctor always return None
- M2 (nonexistent commands): Both `urd doctor` and `urd calibrate` exist in the codebase
- m1 (rendering order): Not applicable for 4a+4b alone (4c not implemented)

---

## 6. The Simplicity Question

**Is this implementation as simple as it could be?**

Yes. The implementation is simpler than the design in two ways:

1. **Staleness escalation** uses the awareness model's status directly instead
   of introducing independent voice thresholds. This eliminates an entire
   category of consistency bugs.

2. **Doctor suggestion** uses a unit variant instead of a boolean field, making
   the "always None" behavior structural rather than conditional.

The only complexity is `aggregate_drive_staleness`, which iterates all assessments
to find the worst status for a drive. This is O(subvolumes * drives_per_subvolume),
which for the typical 9-subvolume case is negligible.

---

## 7. For the Dev Team

**No blocking items.** Prioritized improvements for consideration:

1. **Consider status string constants (M1).** Not urgent, but would prevent
   silent degradation if awareness status strings ever change. This is existing
   debt across voice.rs, not specific to this PR.

2. **Verify visual spacing (m3).** Run `urd status` and `urd plan` with real
   data and confirm the blank line before suggestions looks intentional, not
   like a rendering bug.

3. **Ship it.** The implementation is clean, well-tested, architecturally sound,
   and resolves all design review findings. No reason to hold.

---

## 8. Open Questions

1. **Should PROTECTED disconnected drives show age at all?** The current
   implementation always shows age when available, regardless of status. An
   alternative would be to return `None` for PROTECTED drives (keeping the
   old "away"/"disconnected" text) and only show escalated text for AT RISK
   and UNPROTECTED. This is a UX judgment call, not a correctness issue.

2. **When should status string constants be introduced?** This is a small
   refactor that would benefit the entire voice.rs module, not just this
   feature. It could be a standalone cleanup PR or folded into the next
   voice.rs change.
