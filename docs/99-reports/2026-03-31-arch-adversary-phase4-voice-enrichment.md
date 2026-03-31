# Arch-Adversary Review: Phase 4 — Voice Enrichment

**Date:** 2026-03-31
**Reviewer:** arch-adversary
**Artifact:** `docs/95-ideas/2026-03-31-design-phase4-voice-enrichment.md`
**Type:** Design review (no code)

---

## 1. Executive Summary

This is a well-scoped, low-risk design that adds graduated urgency, next-action
suggestions, and transition-aware voice to Urd's output. All three features are
presentation-layer only (or nearly so), which limits blast radius. The one
architectural concern is 4c's `TransitionEvent` addition to `output.rs` / `BackupSummary`,
which introduces a computation step in `commands/backup.rs` that blurs the clean
"awareness is pure" boundary and creates a subtle coupling between backup execution
and awareness assessment ordering.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss from snapshot deletion.**

None of these features interact with retention, deletion, or the btrfs layer. They
are all read-only over existing structured output. Distance from catastrophic failure:
**3+ bugs away.** This is safe territory.

The only path to danger would be if transition detection (4c) somehow influenced
planner or executor decisions, but the design explicitly keeps it in the output layer.
No concern here.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4/5 | Voice/awareness threshold consistency is flagged but not fully resolved |
| Security | 5/5 | No filesystem writes, no privilege escalation, pure presentation |
| Architectural Excellence | 4/5 | 4a and 4b are pristine voice.rs additions; 4c introduces justified but non-trivial output.rs coupling |
| Systems Design | 4/5 | Solid integration points, but the pre/post awareness diff in 4c needs error handling design |

**Overall: 4.25/5** — Clean design with minor tensions to resolve before building.

---

## 4. Design Tensions

### Tension 1: Voice thresholds vs awareness thresholds (4a)

The design acknowledges this but doesn't fully resolve it. Voice has finer-grained
escalation tiers (observe/suggest/warn) than awareness (binary: `DRIVE_AWAY_DEGRADED_DAYS = 7`).
The risk: voice says "protection degrading" at day 14 (warn tier for primary) while
awareness still shows PROTECTED for an offsite drive with a 7-day interval (since
`EXTERNAL_AT_RISK_MULTIPLIER = 1.5` means AT_RISK only at 10.5 days for a weekly
interval). The design's invariant ("voice should never claim urgency the awareness
model doesn't support") is correct but needs a concrete enforcement mechanism, not
just a stated principle.

### Tension 2: SuggestionContext as a parallel type hierarchy (4b)

`SuggestionContext` extracts boolean flags from the structured output types. This is
pragmatic but creates a second representation of the same data. If `StatusOutput` or
`BackupSummary` gain new fields that should trigger suggestions, the developer must
remember to update `SuggestionContext` too. The coupling is loose but invisible.

### Tension 3: 4c crosses the "voice.rs is pure presentation" line (4c)

Adding `TransitionEvent` to `output.rs` and computing it in `commands/backup.rs` means
the backup command now has awareness-diffing logic. This is justified (transitions are
meaningful output data, not decoration), but it's the first time a voice feature has
required changes outside voice.rs. Worth being explicit that this is a conscious
boundary expansion, not a precedent for putting arbitrary computation in backup.rs.

---

## 5. Findings

### Significant

**S1: Voice escalation can contradict awareness state.**
The design proposes voice.rs thresholds independent of awareness.rs thresholds. For
an offsite drive with 14-day send interval, awareness considers it PROTECTED until
`14 * 1.5 = 21` days. But voice's warn tier for offsite kicks in at 45 days, and
the suggest tier at 21 days. These happen to align for offsite, but for primary drives,
voice warns at 14 days ("protection degrading") while awareness thresholds depend on
send_interval. A primary drive with a 30-day send interval would be PROTECTED at
day 14, while voice says "protection degrading." The contradiction erodes user trust.

**Recommendation:** Voice escalation thresholds should be derived from awareness
thresholds or from send interval, not hardcoded per role. Either:
(a) `escalated_drive_text` takes the send_interval and computes tiers as fractions
of the awareness thresholds, or
(b) voice reads the awareness PromiseStatus and only escalates text *within* the
current awareness state (e.g., "consider connecting" only when AT_RISK, "protection
degrading" only when UNPROTECTED).

**S2: Pre/post awareness diff has no error handling design.**
If the post-backup `assess()` call fails (e.g., filesystem state read error), what
happens? The design doesn't specify. Options: (a) skip transitions silently (loses
information), (b) emit a warning (noisy), (c) treat missing post-state as "no
transitions" (correct default). The design should pick (c) explicitly.

**Recommendation:** Specify that transition detection is best-effort. If post-backup
assessment fails, `transitions` is empty and no voice lines appear. Log at debug level.

### Moderate

**M1: `SuggestionContext::Doctor { all_clear: true }` emits text, violating the
"silence when healthy" principle.**
The design shows `Doctor { all_clear: true }` returning `Some("All clear. No action needed.")`.
But the stated invariant is "healthy states produce `None` -- no 'everything is fine!' noise."
This is a direct contradiction within the design.

**Recommendation:** `Doctor { all_clear: true }` should return `None`. The doctor
command's own output already communicates the all-clear state. The suggestion line
should only appear when there's an actual *next* action to suggest.

**M2: `SuggestionContext` references `urd doctor` and `urd calibrate` which don't exist yet.**
The design proposes suggestions like "Run `urd doctor` to diagnose" and "Run `urd calibrate`
to measure actual snapshot sizes." Neither command exists in the current codebase.
Building 4b before those commands creates dead suggestions.

**Recommendation:** Either (a) build 4b after `urd doctor` exists, or (b) gate those
specific suggestions behind a feature check / omit them until the commands land.
Suggesting a command that doesn't exist is worse than silence.

**M3: 4c `TransitionEvent` is not `Clone` or `PartialEq` in the design.**
The design shows the enum but doesn't specify derives. For testing (asserting on
specific transitions) and for potential daemon serialization, `TransitionEvent` needs
at minimum `Clone`, `PartialEq`, `Eq`, and `Serialize`. The project convention requires
`Debug` on all types plus `Clone`, `PartialEq`, `Eq` where sensible -- all are sensible
here.

**Recommendation:** Specify `#[derive(Debug, Clone, PartialEq, Eq, Serialize)]` on
`TransitionEvent`.

### Minor

**m1: Rendering order of 4b + 4c not specified in the design doc.**
The "Ready for Review" section mentions it as a concern but doesn't resolve it. When
both transitions and suggestions appear, the order matters for visual coherence.

**Recommendation:** Transitions first (what happened), then suggestion (what to do next).
Add this to the design doc as an invariant.

**m2: `escalated_thread_text` references "thread health" but the current codebase
uses `ChainHealth` in data structures.**
The vocabulary decision ("thread" replaces "chain" in user-facing text) is documented,
but `escalated_thread_text()` operates on chain health data. The function name uses
the new vocabulary while its inputs use the old. This is consistent with the naming
decision but worth a comment in the code for clarity.

**Recommendation:** Add a brief doc comment: `/// Escalated text for chain health
(user-facing: "thread").`

### Commendation

**C1: Strong adherence to the "voice.rs is pure" principle.**
Features 4a and 4b are entirely within voice.rs with no state changes, no I/O, and
clear `None`-means-silence semantics. This is exactly how presentation-layer features
should be designed. The project has internalized the pure-function module pattern
(ADR-108) deeply enough that it shapes new feature design by default.

**C2: Honest self-assessment in "Ready for Review" section.**
The design identifies its own weakest points (threshold consistency, over-suggestion,
diff cost, transition accuracy, rendering order) and asks the reviewer to focus there.
This is a sign of mature design practice.

**C3: Minimal blast radius for maximum UX value.**
Three features, approximately 35 new tests, changes to 2-3 files. The ratio of user
value to code surface area is excellent. The brainstorm scoring (9, 9, 8) reflects
genuine user desire, not engineering enthusiasm.

---

## 6. The Simplicity Question

**Is this design as simple as it could be?**

4a and 4b: Yes. Pure functions with clear inputs and outputs. No simpler design exists
that achieves the same goal.

4c: Almost. The `TransitionEvent` enum and pre/post diff are the minimal mechanism for
detecting state changes. An alternative would be to infer transitions from operation
results (e.g., "a full send to a new drive = first send"), but this would be fragile
and couple voice to executor internals. The awareness diff approach is more robust.
The one simplification opportunity: instead of a full `assess()` before backup, capture
only the specific data needed for transition detection (promise statuses per subvolume
per drive). This avoids redundant computation without losing accuracy.

---

## 7. For the Dev Team

**Priority order (do these before or during build):**

1. **Resolve voice/awareness threshold consistency (S1).** This is the design's one
   real flaw. Pick option (b): voice reads the awareness PromiseStatus and calibrates
   its text to the current state, not to independent thresholds. This makes the
   escalation *complement* the awareness model rather than *compete* with it.

2. **Remove the Doctor all-clear suggestion (M1).** Simple fix, but it's a contradiction
   that would ship if not caught.

3. **Gate suggestions on command existence (M2).** Don't suggest `urd doctor` until
   it exists.

4. **Specify error handling for post-backup assessment failure (S2).** One sentence
   in the design doc: "If post-backup assessment fails, transitions is empty."

5. **Add derives to TransitionEvent (M3).** Mechanical but easy to forget.

6. **Document rendering order as an invariant (m1).** Transitions, then suggestions.

---

## 8. Open Questions

1. **Should voice escalation tiers be configurable?** The design hardcodes them. For
   an offsite drive that rotates monthly, the 45-day warn threshold may be too tight.
   For a primary drive that syncs daily, 14-day warn may be too loose. If thresholds
   are always derived from awareness/interval, this question dissolves. If hardcoded,
   it will surface as a user request eventually.

2. **Should `TransitionEvent::AllSealed` include which subvolumes transitioned?** The
   current design makes it a global event ("all threads hold"), but if only one of
   nine subvolumes actually changed state, the user might want to know which one. On
   the other hand, the brevity is valuable. Worth a conscious decision.

3. **Does the pre-backup `assess()` call need filesystem access that might not be
   available?** If the backup is running via systemd timer (nightly, no terminal),
   the assessment reads filesystem state and SQLite. This should work, but if any
   assessment path requires an interactive resource (e.g., terminal width for rendering),
   the pre-backup call would need to be assessment-only, not render-ready. The design
   implies this (it calls `assess()` not `render()`), but confirm that `assess()` is
   truly I/O-minimal.

4. **4c says "status and other query commands never have mythic voice lines" -- does
   `urd plan` count as a query or an event?** Plan is a dry-run preview. If a future
   plan output shows "this would restore a thread," is that a transition voice line or
   a factual preview? The boundary should be defined now, not discovered later.
