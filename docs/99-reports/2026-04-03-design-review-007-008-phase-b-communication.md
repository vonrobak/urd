---
upi: "007+008"
date: 2026-04-03
---

# Architectural Adversary Review: Phase B Implementation Plan (UPI 007 + 008)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Implementation plan `docs/97-plans/2026-04-03-plan-007-008-phase-b-communication.md`
**Mode:** Design review (pre-implementation plan)
**Designs:** UPI 007 (Safety Gate Communication), UPI 008 (Doctor Pin-Age Correlation)

---

## Executive Summary

UPI 007 is well-traced and will work as planned. The plan correctly identifies the single
change site in the executor and lets the compiler enforce exhaustive handling. UPI 008
has a premise error: the plan adds a `drive_mounted: bool` parameter to
`collect_stale_pin_check()`, but the verify loop already short-circuits at line 87-98
when a drive is unmounted — the function is never called for absent drives. The real
problem (stale pin on a recently *reconnected* drive) requires a different fix.

---

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be
deleted.

**Distance:** This plan is far from the catastrophic failure. Both UPI 007 and 008 are
communication-layer changes. The `Deferred` variant doesn't touch retention, pin
protection, or deletion logic. The closest proximity is through the transient cleanup
path (a deferred send changes `sends_succeeded` vs `planned_send_drives`), but this
correctly results in `SkippedPartialSends` — which is the safe default.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 3 | 007 is correct. 008 has a premise error — the drive-unmounted path never reaches the function being modified. |
| 2 | **Security** | 5 | No sudo paths, no filesystem mutations, no new trust boundaries. |
| 3 | **Architectural Excellence** | 4 | 007 follows the data cleanly through the pipeline. 008 Step 9 (UUID suppression) is well-placed. |
| 4 | **Systems Design** | 3 | 008 misidentifies the runtime scenario. The plan also doesn't address what metrics/heartbeat consumers see for deferred runs. |

**Overall: 3.75/5** — Strong plan with one significant premise error in 008 that needs
correction before building.

---

## Design Tensions

### 1. `success: bool` vs. a tri-state on SubvolumeSummary

The plan adds `deferred: Vec<DeferredInfo>` alongside the existing `success: bool`. This
creates a compound check: voice.rs must check `sv.success && !sv.deferred.is_empty()` to
distinguish OK from DEFERRED. The alternative — replacing `success: bool` with a
`SubvolStatus` enum (as the design doc proposed) — would make the state unambiguous.

The plan's choice is the right trade-off *for now*. Changing `success: bool` to an enum
would touch every consumer (metrics, heartbeat, notifications, test helpers), expanding
the blast radius significantly for a patch-tier change. The compound check in voice.rs
is localized. If a third state ever appears, refactor then.

### 2. Deferred info via `error` field vs. dedicated field

The plan reuses `OperationOutcome.error: Option<String>` to carry the deferred suggestion
text, noting it's already used for informational messages on `Skipped` ops. This is a
pragmatic choice — changing the field name or adding a separate `deferred_info` field to
`OperationOutcome` would cascade to SQLite recording, metrics, and every test that
constructs outcomes. But it means "error" doesn't mean "error" — it means "additional
context for non-success results." Document this in a code comment at the field level.

### 3. Accuracy vs. simplicity in pin-age diagnostics (008)

The design doc's scenario — drive absent for 8 days, pin stale — suggests a simple mount
check. But the actual code path reveals a harder problem: the drive is mounted when doctor
runs. The staleness is historical, not current. A proper fix would need to correlate pin
age with drive reconnection time (e.g., from sentinel events or mount timestamps). That's
over-engineered for a 0.25-session patch. The simpler fix (acknowledged in "Rejected
Alternatives") is to just change the message to be less accusatory.

---

## Findings

### F1: UPI 008 premise error — `collect_stale_pin_check()` never runs for unmounted drives [Significant]

**What:** The plan's Step 7 adds `drive_mounted: bool` to `collect_stale_pin_check()` and
changes the message when `!drive_mounted`. But `verify.rs:87-98` does an early `continue`
for unmounted drives — `collect_stale_pin_check()` at line 190 is unreachable when the
drive is not mounted.

**Why it matters:** The parameter would compile and pass tests, but the `drive_mounted: false`
branch would be dead code. The actual problem scenario — drive mounted NOW but pin stale from
prior absence — would remain unfixed.

**Suggested fix:** Two options:

**(A) Remove the early continue for unmounted drives (broader fix).** Let the verify loop
  proceed into the pin-age check even for unmounted drives. This enables the
  `drive_mounted: false` message path. Pros: all existing verify checks that are purely
  local (pin file existence, staleness) can still run without the drive. Cons: checks 3
  (pin exists on external) and 4 (orphan detection) need the drive mounted. Those would
  need individual guards rather than the blanket `continue`.

**(B) Change the message without a mount-state parameter (simpler).** When the pin is stale,
  check whether the *pin age exceeds a "recently reconnected" grace period* or simply
  soften the message unconditionally from "sends may be failing" to "last successful send
  was N days ago." The current message's problem is the accusation ("may be failing"), not
  the data. A neutral message is always correct: whether the drive was absent or sends are
  actually failing, "last successful send was N days ago" is true either way.

  Recommended: Option B. It's a one-line message change, no signature change, no dead code.
  The per-subvolume `drives` scoping (shipped in v0.8.1) means `urd status` already shows
  whether sends are failing. Doctor doesn't need to speculate.

### F2: Deferred subvolume with mixed operations renders ambiguously [Moderate]

**What:** The plan says voice.rs will check `!sv.deferred.is_empty() && sv.success` to
render DEFERRED. But a subvolume can have BOTH successful ops (snapshot create, incremental
send to drive A) AND deferred ops (chain-break full send to drive B). In that case:
`sv.success = true`, `sv.deferred` is non-empty, AND `sv.sends` is non-empty.

**Why it matters:** The rendering would show "DEFERRED" as the status label, which hides
the successful sends. The user needs to see that *some* drives got new data and one was
deferred.

**Suggested fix:** Render the mixed case explicitly. If a subvolume has both sends and
deferred items, show "OK" status with the send info, then add the deferred items below
it (same indentation as errors). The deferred info is *additional context*, not a
replacement for the success line. E.g.:

```
  OK     htpc-root  [30.7s]  (incremental → WD-18TB, 1.6GB)
    DEFERRED  full send to 2TB-backup gated — requires opt-in
    → Run `urd backup --force-full --subvolume htpc-root` when ready
```

### F3: `test_backup_summary()` helper needs deferred field — 30+ test callsites [Moderate]

**What:** The plan says "Update `test_backup_summary()` helper to optionally include
deferred data." But `SubvolumeSummary` gains a new required field `deferred: Vec<DeferredInfo>`.
Every construction of `SubvolumeSummary` — including the `test_backup_summary()` helper and
every test that constructs one directly — needs `deferred: vec![]` added.

**Why it matters:** Not a correctness issue, but the plan undercounts the mechanical work.
There are 10+ direct `SubvolumeSummary` constructions in voice.rs tests and several in
backup.rs tests. Missing this leads to "why am I still fixing compile errors" frustration
during the build phase.

**Suggested fix:** Acknowledge in the plan as mechanical churn. Alternatively, consider
`#[serde(default)]` on the field and providing a `Default` impl (but `SubvolumeSummary`
doesn't derive Default, and adding it for one field is overkill — just note the work).

### F4: Transient cleanup correctly handles deferred — but plan should document why [Minor]

**What:** A deferred `SendFull` adds the drive to `planned_send_drives` (line 305) but
not to `sends_succeeded`. The `attempt_transient_cleanup` check at line 858
(`sends_succeeded != planned_send_drives`) will return `SkippedPartialSends`. This is
correct (don't clean up old parents if not all drives have the new snapshot).

**Why it matters:** The plan doesn't mention transient cleanup at all. A future reader
verifying the plan would have to trace this themselves. Explicitly noting "deferred sends
correctly trigger SkippedPartialSends in transient cleanup — no change needed" prevents
a future developer from "fixing" this.

**Suggested fix:** Add a note to Step 2.

### C1: Compiler-enforced exhaustive match is the right strategy [Commendation]

The plan correctly identifies that adding a variant to `OpResult` will cause exhaustive
match failures at compile time, letting the compiler find every site that needs updating.
This is the right approach — it turns a category of runtime bugs into compile-time errors.
The plan's risk flag #2 correctly predicts there's only one `match` site (line 967)
because other sites use equality comparisons. Good code archaeology.

### C2: "Do NOT change the `result` string logic" insight [Commendation]

Step 4 correctly identifies that `build_backup_summary` already uses `result.overall.as_str()`
and that the overall result is now naturally "success" for deferred-only runs. This avoids
a tempting but unnecessary change — a less careful plan would have added explicit deferred
logic to the summary result computation.

---

## The Simplicity Question

**What could be removed?** The `deferred_count` field on `BackupSummary` (Step 3). It's
derivable from `subvolumes.iter().map(|sv| sv.deferred.len()).sum()` and adds a field that
must be kept in sync. Voice.rs can compute it inline. One fewer field to maintain.

**What's earning its keep?** `DeferredInfo` as a struct (not just a String) is justified.
It separates the reason from the suggestion, which the rendering needs to format differently.
The `OpResult::Deferred` variant earns its keep by giving the compiler exhaustive match
enforcement.

---

## For the Dev Team

Priority-ordered action items:

1. **[Significant] Fix 008 premise — use Option B.** In `collect_stale_pin_check()`,
   change the stale-pin message from `"sends may be failing"` to `"last successful send
   was {N} day(s) ago"`. No parameter change needed. Remove Steps 7 and 8 from the plan.
   The neutral message is correct whether the drive was absent, sends are failing, or the
   config was recently changed. Still add a test that verifies the new message wording.

2. **[Moderate] Handle mixed success+deferred rendering in Step 5.** Add a rendering case
   for subvolumes with both successful sends and deferred ops. Show "OK" with send info,
   then list deferred items below. Only use "DEFERRED" as the status when the subvolume
   has *no* successful sends — i.e., the only send operations were deferred.

3. **[Moderate] Account for test construction churn in Step 3.** Note that adding
   `deferred: Vec<DeferredInfo>` to `SubvolumeSummary` will require `deferred: vec![]`
   in ~15 test callsites across voice.rs and backup.rs. This is mechanical but takes time.

4. **[Minor] Document transient cleanup non-interaction in Step 2.** Add: "Deferred sends
   add to `planned_send_drives` but not `sends_succeeded`, which correctly triggers
   `SkippedPartialSends` in transient cleanup. No change needed."

5. **[Minor] Consider dropping `deferred_count` from `BackupSummary`.** Derive it in
   voice.rs from `data.subvolumes` instead. One fewer field to keep in sync.

---

## Open Questions

1. **What did the v0.8.0 test session actually observe for T1.4?** The design doc says
   2TB-backup was absent for 8 days and the stale-pin warning fired. Given the current
   code's early `continue` for unmounted drives, either: (a) the drive was re-mounted
   before running doctor, or (b) the code was different at v0.8.0. If (a), the "drive not
   connected" message path would never fire. Confirming the actual scenario determines
   whether Option A or B is the right fix.

2. **Should `urd failures` show deferred operations?** The `recent_failures()` SQL query
   uses `WHERE result = 'failure'`. Deferred operations will be invisible in `urd failures`.
   Is this correct? A user might want to see "what was deferred" alongside "what failed."
   Not blocking — just a UX decision to make explicitly.
