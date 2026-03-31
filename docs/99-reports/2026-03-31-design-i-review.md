# Design Review: Redundancy Recommendations (Idea I)

**Design doc:** `docs/95-ideas/2026-03-31-design-i-redundancy-recommendations.md`
**Reviewer:** Architectural adversary
**Date:** 2026-03-31
**Verdict:** Approve with findings. Three issues need resolution before implementation.

---

## Scores

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Advisory taxonomy is sound; Ord derivation has a subtle ordering issue; stringly-typed migration path is underspecified |
| Security | 9/10 | No security concerns — advisory-only system with no enforcement or state mutation |
| Architectural Excellence | 7/10 | Right modules, right boundaries, but introduces a parallel advisory system without a migration plan for the existing one |
| Systems Design | 8/10 | Spindle boundary is well-drawn; notification chattiness for drive cycling patterns needs a suppression mechanism |

**Overall: 8/10** — A well-scoped, well-motivated design that fills a real gap. The
findings below are tractable and don't challenge the fundamental approach.

---

## Findings

### S1. RedundancyAdvisoryKind Ord derivation produces wrong ordering [Severity: Medium]

**The claim:** "The `Ord` derivation orders variants worst-first (NoOffsiteProtection >
OffsiteDriveStale > SinglePointOfFailure > TransientNoLocalRecovery)."

**The problem:** Rust's `derive(Ord)` on enums orders by discriminant — i.e., by
declaration order, top to bottom. The enum is declared as:

```rust
pub enum RedundancyAdvisoryKind {
    TransientNoLocalRecovery,   // discriminant 0
    SinglePointOfFailure,       // discriminant 1
    OffsiteDriveStale,          // discriminant 2
    NoOffsiteProtection,        // discriminant 3
}
```

This means `TransientNoLocalRecovery < SinglePointOfFailure < ... < NoOffsiteProtection`.
So `max()` returns `NoOffsiteProtection` (worst), which is correct for the stated goal.
But the comment says "worst-first," implying the Ord should sort worst items to the front
(lowest), like `PromiseStatus` does (where `Unprotected < AtRisk < Protected` so `min()`
yields worst). The design uses `max()` instead of `min()`, which works, but this is
inconsistent with every other ordered enum in the codebase (`PromiseStatus`,
`OperationalHealth`, `ChainHealth`) where `min()` yields worst.

**Recommendation:** Either:
- Reverse the declaration order so `min()` yields worst (consistent with the rest of the
  codebase), or
- Document the intentional `max()` convention explicitly and add a unit test that verifies
  `NoOffsiteProtection` is the maximum variant.

The inconsistency will cause a bug the first time someone writes
`advisories.iter().map(|a| a.kind).min()` expecting worst-first, following the pattern
from awareness.rs.

---

### S2. Parallel advisory systems without migration path [Severity: Medium]

**The existing system:** `SubvolAssessment.advisories: Vec<String>` — stringly-typed,
rendered by `voice.rs` via `render_advisories()`, serialized into `StatusAssessment.advisories:
Vec<String>` in `output.rs`.

**The proposed system:** `Vec<RedundancyAdvisory>` — structured, typed, with its own
rendering path and sentinel integration.

**The design acknowledges this** (Open Question 2) and recommends "keep existing for now,
add structured alongside." This is pragmatic for shipping, but the review report should
name the costs:

1. **Two rendering paths in voice.rs.** The existing `render_advisories()` iterates
   `assessment.advisories` (Vec<String>). The new redundancy advisories get their own
   "REDUNDANCY" section. Two different visual treatments for the same conceptual thing
   (advisory guidance). The user sees "NOTE" lines mixed with a separate "REDUNDANCY"
   block.

2. **Overlap with existing offsite cycling advisory.** awareness.rs already emits
   `"offsite drive {} last sent {} days ago -- consider cycling"` at 7 days for unmounted
   drives. The new `OffsiteDriveStale` advisory fires at 30 days for offsite-role drives.
   Between 7 and 30 days, the user sees the old stringly-typed advisory. After 30 days,
   they see both. This is confusing, not complementary.

3. **StatusAssessment serialization diverges.** JSON consumers (daemon mode, `--json` flag)
   see `advisories: ["offsite drive..."]` for the old system and a separate top-level
   `redundancy_advisories: [...]` for the new one. Two advisory arrays in the same output.

**Recommendation:** The design should commit to one of:
- **(a) Migrate the existing offsite cycling advisory** into `RedundancyAdvisory` as part of
  this work. The 7-day "consider cycling" advisory becomes `OffsiteDriveStale` with a lower
  severity or a separate `OffsiteCyclingReminder` kind. This removes the overlap.
- **(b) Defer the structured system** and add the four advisory types as stringly-typed
  entries in the existing `Vec<String>`, with a comment marking them for future structuring.
  Ships faster, no parallel systems.
- **(c) Accept the debt** but add a `// TODO: migrate Vec<String> advisories to
  RedundancyAdvisory` comment in awareness.rs and output.rs, and document the planned
  migration in the design doc (not just "revisit later").

Option (a) is cleanest. The clock-skew advisory and "send_enabled but no drives configured"
advisory can remain stringly-typed since they are operational, not redundancy-related.

---

### S3. Notification chattiness for intentional drive absence [Severity: Medium]

**The scenario:** A user rotates an offsite drive monthly. The drive is intentionally absent
for 25-29 days at a time. Around day 30, `OffsiteDriveStale` appears. On day 31, the user
connects the drive, runs a backup, and the advisory resolves. Next month, same cycle.

**The problem:** `RedundancyAdvisoryChanged` fires on every appearance/resolution. For a
monthly drive rotation, this means two notifications per month, every month, forever. The
user learns to ignore them. Notification fatigue is the #1 killer of monitoring systems.

**The design says:** "at most once per gap appearance/resolution" and "sentinel deduplicates
by tracking the set of active `(kind, subvolume)` pairs." This prevents within-cycle
duplicates but not cross-cycle repetition.

**Recommendation:** Add a cooldown or repeat-suppression mechanism:
- After a `RedundancyGapResolved` notification, suppress the same `(kind, subvolume)` pair
  from firing `RedundancyGapDetected` again for N days (e.g., 7 days). This handles the
  "drive just came back and will leave again soon" pattern.
- Or: only notify on the *first* appearance of an advisory kind for a subvolume. Subsequent
  appearances after resolution are silent unless the advisory has been absent for longer
  than the threshold (30 days). This treats the pattern as "known and managed."
- Or: make redundancy advisory notifications configurable per-kind (some users want them,
  some don't). This conflicts with opaque-level principles but may be necessary for
  advisory-level notifications that are inherently opinion-driven.

The existing `PromiseDegraded`/`PromiseRecovered` notifications don't have this problem
because promise transitions are rare events, not cyclic patterns.

---

### S4. Overlap between I's "no-offsite-protection" and E's preflight check [Severity: Low]

**Design I:** `NoOffsiteProtection` advisory when a resilient subvolume has no offsite drive.
**Design E:** `resilient-without-offsite` preflight check when a resilient subvolume has no
offsite drive.

These detect the same condition in different modules with different consequences:
- E's check fires at config validation time (preflight) — once, at startup.
- I's advisory fires at assessment time (awareness) — every tick, in status output, in
  sentinel state.

**Assessment:** This is intentional and correct overlap, not redundancy. E catches it early
(before any backup runs); I surfaces it persistently (in ongoing monitoring). A user who
ignores E's preflight warning still sees I's advisory in `urd status`. Different layers,
different audiences, same underlying fact.

**The design should say this explicitly.** The relationship table mentions "E enforces, I
advises, they compose" but doesn't address this specific overlapping detection. One sentence
in the "Relationship to other ideas" section would prevent a future developer from
consolidating them and losing the persistent-surfacing property.

---

### S5. Sentinel state file schema bump needs version gate [Severity: Low]

**The design:** Adds `advisory_summary: Option<AdvisorySummary>` to `SentinelStateFile`,
bumps schema to v3. Uses `serde(default, skip_serializing_if)` for backward compatibility.

**The concern:** The current `SentinelStateFile` has `schema_version: u32` but the codebase
does not appear to validate it on read. A v2 reader (older Urd) reading a v3 file will
silently ignore `advisory_summary` — correct. A v3 reader reading a v2 file will get
`advisory_summary: None` — also correct.

But Spindle (a separate binary) will read the state file. If Spindle is built against v3
and reads a v2 file, it needs to handle `advisory_summary: None` gracefully. The design
says "Spindle should be aware that the field may be absent on v2 state files" but doesn't
specify what Spindle should do — show no badge? Show a "?" badge?

**Recommendation:** Define the Spindle behavior for missing `advisory_summary`: "absent
means unknown, not zero advisories." Spindle should show no badge (not a zero-count badge)
when the field is absent. This prevents a v2 Urd from appearing advisory-free when it
simply doesn't compute advisories yet.

---

### S6. Sentinel advisory diff detection timing [Severity: Low]

**The question:** "What happens when advisories change between runs — does the sentinel
detect the transition correctly?"

**Analysis:** The design says the sentinel tracks the previous advisory set and diffs on
each tick. The sentinel currently diffs `promise_states` between ticks for
`PromiseDegraded`/`PromiseRecovered` notifications (via `compute_notifications()`). The
new advisory diff would follow the same pattern but with a different data structure.

**Potential issue:** Advisories depend on assessment state, which depends on filesystem
state. If the sentinel tick doesn't re-assess (e.g., it reads a cached heartbeat), the
advisory set won't change even if the underlying condition resolved. The design should
clarify: does `compute_redundancy_advisories()` run on every sentinel tick, or only after
a backup run updates the heartbeat?

If it runs every tick (using fresh filesystem state), advisories resolve promptly when a
drive is connected. If it only runs on heartbeat changes, there's a delay equal to the
backup interval.

**Recommendation:** State explicitly that `compute_redundancy_advisories()` runs on every
sentinel tick using fresh assessment data (not cached heartbeat). This is consistent with
the sentinel's existing behavior of re-assessing on each tick.

---

## Positive observations

1. **Advisory-only, not enforcement.** The clear separation between I (advice) and E
   (enforcement) is exactly right. Advice that blocks operations is no longer advice.

2. **Spindle boundary is well-drawn.** Keeping advisory detail out of the state file and
   using counts + worst-kind is the right abstraction boundary. It avoids rendering logic
   in the state file and keeps Spindle's contract minimal.

3. **Informational tier excluded from counts.** `TransientNoLocalRecovery` not contributing
   to badge counts prevents false urgency for a deliberate configuration choice.

4. **Two-line rendering pattern.** Observation + suggestion is the right voice for advisory
   content. It respects the user's intelligence while providing actionable guidance.

5. **Test strategy is thorough.** 15 tests covering all four advisory types, ordering,
   count exclusion, and sentinel diff detection. The "guarded subvolumes excluded" test
   (item 7) is particularly important — it prevents advisory creep into configurations
   that explicitly chose minimal protection.

---

## Summary of required actions

| # | Finding | Severity | Action |
|---|---------|----------|--------|
| S1 | Ord derivation inconsistent with codebase convention | Medium | Reverse enum declaration order so `min()` yields worst, or document and test `max()` convention |
| S2 | Parallel advisory systems | Medium | Choose migration strategy (a), (b), or (c); don't ship without a decision |
| S3 | Notification chattiness for drive cycling | Medium | Add cooldown or repeat-suppression for cyclic advisory patterns |
| S4 | Overlap with E's preflight check | Low | Add explicit note in relationship table |
| S5 | Schema bump without Spindle guidance | Low | Define Spindle behavior for missing advisory_summary |
| S6 | Advisory diff timing in sentinel | Low | Clarify whether advisories recompute on every tick or on heartbeat change |
