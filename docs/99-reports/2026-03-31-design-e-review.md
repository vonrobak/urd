# Design Review: Promise Redundancy Encoding (Design E)

**Date:** 2026-03-31
**Reviewed by:** Architectural Adversary
**Design document:** `docs/95-ideas/2026-03-31-design-e-promise-redundancy-encoding.md`
**Mode:** Design review (4 dimensions)
**Verdict:** Approve with findings. Architecturally sound, one critical concern and
several important items requiring resolution before implementation.

---

## Scores

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Logic is sound; boundary semantics and double-counting need clarification |
| Security | 9/10 | No new attack surface; fail-open preserved; no data-loss vectors |
| Architectural Excellence | 7/10 | Breaks a stated ADR-110 invariant; the break may be justified but needs acknowledgment |
| Systems Design | 8/10 | Good use of existing machinery; threshold choices need operational justification |

---

## Catastrophic Failure Checklist

| # | Failure mode | Risk | Assessment |
|---|-------------|------|------------|
| 1 | Silent data loss | None | Design is read-only — no deletion logic touched |
| 2 | Path traversal | None | No path changes |
| 3 | Pinned snapshot deletion | None | Retention untouched |
| 4 | Space exhaustion | None | No new snapshot creation logic |
| 5 | Config change orphaning snapshots | None | Offsite freshness is assessment-only |
| 6 | TOCTOU privilege boundary | None | No new privilege operations |

No catastrophic failure vectors identified. This design is purely observational — it
changes how Urd *reports* promise status, not how it *acts*. This is the right risk
profile for a first iteration.

---

## Critical Finding

### C1: Threading `protection_level` into awareness breaks ADR-110 Invariant 6

**ADR-110, Invariant 6 states:** *"The awareness model is unchanged. Promise levels affect
what intervals are configured, not how evaluation works."*

This design explicitly violates that invariant. Today, awareness is
`protection_level`-blind: it evaluates freshness against configured intervals, period.
The design threads `protection_level` into `assess()` so that resilient subvolumes get
an additional offsite freshness constraint that protected subvolumes do not.

This means two subvolumes with identical operational parameters (same intervals, same
drives, same send history) will receive different promise statuses based solely on their
`protection_level` label. Awareness is no longer evaluating operational reality — it is
evaluating operational reality *plus semantic intent*.

**This may be the right call.** The entire point of the design is that "resilient" should
mean something beyond "protected with more drives." But it is a deliberate architectural
change, not just an implementation of existing intent. The ADR-110 invariant was written
to keep awareness as a pure operational evaluator, and this design makes it a
policy-aware evaluator.

**Required action:** Either (a) update ADR-110 Invariant 6 to explicitly permit
protection-level-aware constraints in awareness, with a clear boundary for what awareness
may and may not do with that information, or (b) move the offsite freshness overlay
*outside* awareness — compute it as a post-processing step in the status command and
sentinel, keeping awareness itself protection-level-blind.

Option (b) would keep the `compute_offsite_freshness` function pure but place it in
a new module or in the command layer, composing it with the awareness output. This
preserves the invariant at the cost of splitting the "overall status" computation across
two locations.

Option (a) is simpler but creates precedent. If awareness knows about protection levels,
future features will want to add more level-specific logic there. Establish the boundary
now: awareness may apply *additive constraints* based on protection level (never relax
them), and only for dimensions that the level semantically claims (geographic redundancy
for resilient). Document this as a scoped exception.

---

## Important Findings

### I1: Double-counting risk with existing external assessment

The design says `overall = min(local_status, best_external_status,
offsite_freshness_status)`. But `best_external_status` already includes the offsite
drive's per-interval freshness assessment. Consider this scenario:

- Offsite drive WD-18TB1 (`role = "offsite"`, `send_interval = "24h"`)
- Last send: 25 days ago
- Per-interval assessment: `25d / 24h * 1.5 multiplier` = deeply UNPROTECTED
- Offsite freshness assessment: 25 days < 30 days = PROTECTED

The per-interval assessment already reports UNPROTECTED for the offsite drive. The
offsite freshness overlay reports PROTECTED. The overall status is dominated by the
per-interval result, so the offsite freshness overlay is invisible.

Conversely, consider:
- Primary drive WD-18TB (`role = "primary"`, `send_interval = "24h"`) — sent 2h ago, PROTECTED
- Offsite drive WD-18TB1 (`role = "offsite"`, `send_interval = "24h"`) — sent 35 days ago, UNPROTECTED per interval
- `best_external_status = max(PROTECTED, UNPROTECTED) = PROTECTED` (primary wins)
- `offsite_freshness_status = AT RISK` (35 days)
- `overall = min(local, PROTECTED, AT RISK) = AT RISK`

In this case the offsite freshness overlay is the only thing catching the degradation,
because `best_external_status` lets the primary drive mask the stale offsite. This is
the design's actual value — and it works correctly here.

**But:** The design should explicitly state that the offsite freshness check is
specifically needed *because* `best_external_status` uses `max()` across all drives,
which lets a healthy primary mask an absent offsite. This is the core justification.
Without it, a reader might think this is redundant with existing per-drive assessment.

### I2: 30-day threshold assumes monthly rotation but does not validate it

The design chooses 30 days because "generous enough for monthly drive rotation." But
consider a user who rotates offsite drives quarterly (every ~90 days). With the design's
thresholds:

- Days 1-30: PROTECTED
- Days 31-90: AT RISK (constant for 60 days)
- Days 91+: UNPROTECTED

This user sees AT RISK for two-thirds of every rotation cycle. They would need to switch
to `custom` to avoid the noise, even though their rotation cadence may be entirely
intentional for their threat model.

The 30-day threshold is defensible for the *word* "resilient" — if your offsite copy is
a month old, you have a month of data at risk from site disaster. But the design should
acknowledge that this threshold implicitly defines resilient as "monthly-or-better
offsite rotation." Users with longer rotation cycles must use custom. This is fine — but
say it explicitly so it is a conscious design choice, not an accidental side effect.

### I3: `compute_offsite_freshness` uses `last_send_age` which has a subtlety for unmounted drives

Looking at the current awareness code (line 278-279):

```rust
let last_send_time = fs.last_successful_send_time(&subvol.name, &drive.label);
let last_send_age = last_send_time.map(|t| clamp_age(now - t));
```

`last_successful_send_time` comes from the SQLite state database (history, not
filesystem truth — ADR-102). For an offsite drive that has been away for 40 days, this
returns the last recorded send time. The `last_send_age` is then 40 days.

This works correctly for the offsite freshness check — the age reflects how long since
the offsite copy was updated, regardless of whether the drive is currently mounted. Good.

However, the design's `compute_offsite_freshness` filters on `d.role == DriveRole::Offsite`,
but the current `DriveAssessment` struct does not carry `role`. The design adds it (section
"DriveAssessment gains `role`"). Verify that when building drive assessments in
`assess()`, the drive config's role is correctly plumbed. This is straightforward but
is the one data-flow change that could silently fail (a drive with `role = "offsite"` in
config but `role = Primary` in the assessment due to a default or missed plumbing).

### I4: Existing offsite advisory fires on `!mounted` condition only

The current advisory (awareness.rs lines 284-290) only fires when a drive is *not
mounted* and the last send is > 7 days old. A mounted offsite drive that simply has not
had a successful send in 35 days would not trigger the existing advisory, but *would*
trigger the new offsite freshness degradation.

This is correct behavior — the new system is more comprehensive. But the design should
clarify whether the existing 7-day advisory for unmounted drives should be kept,
removed, or subsumed. The design says "replaced by (or supplemented with)" — pick one.
Recommendation: keep the existing advisory for protected subvolumes (informational
only), replace it with the structured degradation reason for resilient subvolumes. Clean
separation.

---

## Minor Findings

### M1: ADR gate assessment is too conservative

The design says "no new ADR" because this "enforces what resilient already claims to
mean." But ADR-110 explicitly calls the taxonomy "provisional" and says the levels need
rework. This design is *defining* what resilient means more precisely than ADR-110 ever
did. Adding an ADR-110 addendum (not a new ADR) that documents the offsite freshness
contract and thresholds would be the right level of documentation. Future taxonomy
rework needs to know these thresholds exist.

### M2: Test 12 boundary condition needs clarification

The test strategy says "exactly 30 days is PROTECTED, 30 days + 1 minute is AT RISK."
But the code uses `days <= 30` where `days` is `age.num_days()`. `num_days()` truncates,
so 30 days and 23 hours would still return 30 and be PROTECTED. The actual boundary is
at exactly 31 days (31 * 86400 seconds). Decide whether the threshold should be in days
(integer comparison on `num_days()`) or in seconds (precise comparison). Days is simpler
and more forgiving. Seconds is more precise but less readable. The design should state
which it uses and test accordingly.

### M3: `PromiseStatus` ordering enables `min()` correctly

The design relies on `overall = overall.min(offsite_freshness)`. The existing enum
ordering is `Unprotected < AtRisk < Protected`, so `min()` yields the worst status. This
is correct and already used by `compute_overall_status`. No issue — just confirming the
design's assumption holds.

### M4: No interaction with notifications module

The design mentions that "offsite degradation may trigger notifications via existing
promise-change logic." This is correct — the sentinel tracks promise status transitions
and fires notifications on degradation. If the offsite freshness overlay causes a status
change from PROTECTED to AT RISK, the sentinel will detect the transition and notify.
No additional notification code is needed. Verify that the sentinel's comparison logic
uses the final `SubvolAssessment.status` (which it does — confirmed from awareness
integration).

---

## Focus Area Responses

### Is 30-day/90-day the right threshold?

Defensible but opinionated. 30 days aligns with monthly rotation, which is the most
common home-user pattern. 90 days as UNPROTECTED is generous — three months without
updating an offsite copy is genuinely concerning for data labeled "resilient." The risk
is users with intentionally longer cycles (see I2). Mitigated by custom being
first-class. The thresholds being non-configurable is the right call per ADR-110 opacity.

### Does threading protection_level into awareness break the pure-function contract?

No — it preserves functional purity (inputs in, outputs out, no I/O). It breaks the
*scope* contract: ADR-110 Invariant 6 says awareness is protection-level-blind. The
function is still pure; the concern is whether awareness should know about protection
levels at all. See C1.

### Is "advisory only" for resilient-without-offsite the right call?

Yes. Per ADR-109, structural validity (is the TOML well-formed?) is a hard gate;
achievability (can the world fulfill the promise?) is advisory. Missing an offsite drive
is an achievability gap, not a structural error. The user may be in transition — they
bought the drive but have not configured the mount point yet. Blocking backups would
violate fail-open (ADR-107).

However, the design could be stronger here: if no drive has `role = "offsite"` *and*
the subvolume has `protection_level = "resilient"`, should this be a structural error
(the config is internally inconsistent) rather than an achievability gap? The config
says "resilient" but the drive topology makes that structurally impossible. This is
different from "drive not mounted" (runtime) — it is "no drive exists with the right
role" (structural). Consider making this a hard gate in config validation, not just a
preflight advisory.

### How does offsite freshness interact with existing external assessment?

See I1. The interaction is sound but non-obvious. The key insight: `best_external_status`
uses `max()` so a healthy primary masks a stale offsite. The offsite freshness overlay
specifically catches this case. The design should make this reasoning explicit.

---

## Summary

This is a well-considered design that fills a genuine semantic gap. The core idea —
resilient should mean geographic redundancy, and the system should verify it — is
correct and overdue. The implementation plan is appropriately scoped, the test strategy
covers the important cases, and the rejected alternatives show good judgment.

The main concern is architectural: threading `protection_level` into awareness changes
the module's contract. This is not necessarily wrong, but it must be acknowledged and
bounded, not treated as "just enforcing existing intent." Update ADR-110 to document
the scoped exception.

The 30/90-day thresholds are reasonable defaults. The double-counting analysis (I1)
confirms the design adds genuine value that the existing per-drive assessment cannot
provide. The preflight check is correctly scoped as advisory.

Proceed to implementation after resolving C1 (ADR-110 invariant update) and clarifying
I1 (explicit rationale for why `max()` masking necessitates the overlay), I2 (state
the rotation cadence assumption), and I4 (decide on advisory replacement vs
supplementation).
