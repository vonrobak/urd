# Architectural Adversary Review: Awareness Model Design

> **TL;DR:** The awareness model plan is architecturally sound — pure function following the
> planner pattern, correct trait placement, disciplined deferral of promise levels. Three
> findings that matter: (1) external freshness needs tighter thresholds than local freshness
> (same multipliers hide stale offsite copies), (2) assessment errors should be captured per
> subvolume rather than silently treated as UNPROTECTED, (3) overall status should reflect
> the best connected drive, not worst across all drives including disconnected offsite.

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Design review of the awareness model plan (Priority 3a, Phase 5)
**Base commit:** `f2314ca`
**Reviewer:** Claude (arch-adversary)

---

## What Kills You

**Catastrophic failure mode:** The awareness model tells the user their data is PROTECTED
when it isn't. The user trusts this assessment, doesn't connect their external drive, and a
disk failure destroys the only copy. This is one false-positive away from data loss — the
awareness model is a **safety-critical assessment tool**, and false confidence is worse than
no assessment at all.

**Distance:** Moderate. The multiplier thresholds and the external freshness logic are the
primary attack surface. If the thresholds are too generous, the model lies. If the external
assessment silently degrades when drives are unmounted for weeks, the user gets PROTECTED
when they should see AT_RISK.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Sound thresholds, but one significant gap in external freshness assessment |
| Security | 5 | Pure function, no I/O, no privilege — attack surface is zero |
| Architectural Excellence | 4 | Follows established patterns well; one finding on trait placement |
| Systems Design | 3 | External staleness model needs more thought; first-run bootstrapping unclear |
| Rust Idioms | 4 | Follows project conventions; `chrono::Duration` in public API is a minor concern |
| Code Quality | 4 | Test plan is thorough; struct design supports future extension |

---

## Design Tensions

### 1. FileSystemState Extension vs. Separate Trait

**Trade-off:** The plan adds `last_successful_send_time()` to the existing `FileSystemState`
trait rather than creating a new `BackupHistory` trait.

**Why this was probably chosen:** Consistency with the planner pattern. One trait, one mock,
one test fixture pattern. Reduces the number of abstractions.

**Evaluation: This is the right call.** `FileSystemState` already includes `last_send_size()`
which queries the StateDb — the precedent is set. Adding `last_successful_send_time()` follows
the established pattern. Creating a separate `BackupHistory` trait would mean the awareness
model takes two trait objects, the mock setup doubles in complexity, and every consumer must
thread two references. The marginal purity of a second trait doesn't pay for the complexity.

The trait name `FileSystemState` increasingly describes something broader than filesystem
state — it's becoming "all external state needed by pure functions." If more methods are added
in the future (drive connection history, heartbeat read), consider renaming to `SystemState`
or similar. But don't do it now — that's a rename, not a design change.

### 2. Same Multipliers for Local and External — NEEDS CHANGE

**Trade-off:** The plan uses the same 2x / 5x thresholds for both local snapshot freshness
and external send freshness.

**Evaluation: This is wrong for external sends, and it matters.** Local snapshots and external
sends operate on fundamentally different timescales and failure modes:

- **Local snapshots** with `snapshot_interval = 1h`: 2x = 2h grace. Reasonable — a systemd
  timer that fires late by an hour is a non-event.
- **External sends** with `send_interval = 24h`: 2x = 48h grace. This means a daily send
  can be two full days late before it even registers as AT_RISK.

But more critically: **external sends are gated by drive availability.** A drive that's
unmounted for a week isn't "the timer missed" — it's "the physical medium isn't here."

**Concrete scenario:** User configures `send_interval = 24h`. Their external drive disconnects
Monday morning. Tuesday evening (36h later): still PROTECTED. Wednesday evening (60h):
AT_RISK. Friday evening (108h): finally UNPROTECTED. Meanwhile, the user's only copies are
on the local disk for almost a week.

**Fix:** Use asymmetric multipliers: local 2x/5x, external 1.5x/3x.

### 3. Deriving Thresholds from Intervals vs. Explicit Promise Levels

**Trade-off:** Using configured intervals as the threshold base rather than waiting for
promise levels.

**Evaluation: Exactly right.** This ships value now, leaves the right extension points, and
doesn't commit to policy decisions that need an ADR. The awareness model gets richer inputs
when promise levels arrive — it doesn't need a different architecture.

### 4. Chain Health as Informational Only

**Trade-off:** Chain health doesn't affect `PromiseStatus` — a broken chain means the next
send is full (space risk), not that data is at risk.

**Evaluation: Correct.** Keeps the safety signal clean. A broken chain is a cost concern,
not a safety concern.

---

## Findings

### Significant: External Freshness Needs Tighter Multipliers

**Severity: Significant** — one user-trust violation away from the catastrophic failure mode.

The same 2x / 5x multipliers applied to external sends with 24h intervals create a
false-comfort window of 2-5 days where the user's offsite copy is stale but the model says
AT_RISK instead of UNPROTECTED. See Design Tension #2 for concrete scenario.

**Fix:** Use asymmetric multipliers: local 2x/5x, external 1.5x/3x.

### Significant: Overall Status Should Reflect Connected Drives, Not All Drives

**Severity: Significant** — policy question with direct UX impact.

If a user has two drives (primary + offsite), the offsite may only connect monthly. Taking
`min()` across all drives would make the overall status nearly permanently AT_RISK for
any user with an offsite rotation — exactly the kind of noise that makes users ignore the
signal.

**Fix:** Overall external status should be `max()` across drives that have *ever* been sent
to (best connected drive), not `min()`. The offsite drive's staleness should surface as a
separate advisory ("offsite drive WD-18TB last connected 12 days ago — consider cycling").
This advisory is informational, not part of the PromiseStatus computation.

When promise levels arrive (Priority 4), "resilient" (2+ copies) will need `min()` semantics
to enforce multi-copy guarantees. But that's promise-level logic, not default behavior.

### Significant: First-Run Bootstrapping Produces Misleading State

**Severity: Significant** — not close to data loss, but close to user-trust violation.

After the first-ever `urd backup`, the model says PROTECTED because `last_send_age <
send_interval × multiplier`. But there's only *one* external copy, ever. The assessment
is technically correct but semantically misleading.

**Fix:** Document this as a known limitation. Optionally add a note in the assessment when
there's minimal history. Not blocking for v1.

### Moderate: Assessment Errors Should Be Captured, Not Swallowed

**Severity: Moderate** — avoids misrepresenting unknown state as UNPROTECTED.

If `FileSystemState::local_snapshots()` fails for a subvolume, silently treating it as
"no snapshots" returns UNPROTECTED when the real state is *unknown*. UNPROTECTED tells the
user "your data isn't safe." Unknown tells the user "I can't check."

**Fix:** Add `errors: Vec<String>` to `SubvolAssessment`. When a filesystem query fails,
capture the error and still return a best-effort assessment. This matches the project's
philosophy of "individual subvolume failures must NOT abort the entire run."

### Moderate: Consider `std::time::Duration` for Age Fields

**Severity: Moderate** — API ergonomics, not a bug.

`chrono::Duration` is signed and can represent negative durations. Ages can't be negative.
`std::time::Duration` is unsigned and a better semantic fit. However, chrono is already
pervasive in the codebase, so consistency may outweigh the purity argument.

**Fix:** Use `std::time::Duration` for age fields in assessment structs if easy to convert.
Otherwise document the convention. Minor point — don't let this block the implementation.

### Commendation: Pure Function Design

The decision to make the awareness model a pure function following the planner's pattern is
exactly right. Every test is deterministic, the model can be used anywhere without coupling,
and there's no state to corrupt. This mirrors the planner's greatest architectural strength.

### Commendation: Deferred Promise Levels

Not building promise levels into the awareness model and instead deriving thresholds from
existing config is a disciplined choice. It ships value now, leaves the right extension
points, and doesn't commit to policy decisions that need an ADR.

### Commendation: Separation of Chain Health from Promise Status

Chain health as informational-only keeps the safety signal clean. A broken chain is a cost
concern, not a safety concern.

---

## The Simplicity Question

**What could be removed?** The plan is already lean — one module, one function, four types.
Nothing to cut.

**What's earning its keep?** `SubvolAssessment` with sub-assessments gives the presentation
layer the granularity it needs. `DriveAssessment` per drive gives multi-drive visibility.
`LocalAssessment` as parallel to `DriveAssessment` makes the code regular. All earn their
keep.

**What should stay simple?**
- Don't add methods to the assessment types (Display, colored output). The presentation layer
  owns rendering. Assessment types are data, not UI.
- Don't add serialization yet. The heartbeat (Priority 3b) will define its own format.

---

## Priority Action Items

1. **Use asymmetric multipliers** — local 2x/5x, external 1.5x/3x (Significant)
2. **Overall status = max() across connected drives** — offsite staleness as advisory, not
   status (Significant)
3. **Add `errors: Vec<String>` to SubvolAssessment** — capture assessment failures per
   subvolume (Moderate)
4. **Document first-run bootstrapping limitation** — known gap, not blocking (Significant,
   documentation only)
5. **Consider `std::time::Duration` for age fields** — cleaner semantics (Moderate)
6. **Handle `send_enabled = true` with no drives configured** — treat as UNPROTECTED with
   a warning in errors (edge case)

---

## Resolved Questions

1. **`send_enabled = true` but zero drives configured:** Treat as an assessment error — add
   a warning to `errors` and set external status to UNPROTECTED. This is a config smell, not
   an awareness model bug.

2. **Overall status across mounted vs. unmounted drives:** Overall status reflects the *best*
   connected drive (max across drives with send history). Offsite drive staleness surfaces as
   a separate advisory for the presentation layer, not as part of PromiseStatus. When promise
   levels with multi-copy requirements arrive (Priority 4), they override with min() semantics.
