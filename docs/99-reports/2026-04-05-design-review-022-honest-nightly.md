---
upi: "022"
date: 2026-04-05
---

# Arch-Adversary Design Review: The Honest Nightly (UPI 022)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** Implementation plan at `docs/97-plans/2026-04-05-plan-022-honest-nightly.md`, design at `docs/95-ideas/2026-04-05-design-022-honest-nightly.md`
**Mode:** Design review (plan before code)
**Commit:** ffcdfed (master, v0.11.0)

---

## Executive Summary

The plan is well-scoped and correctly identifies the root cause: transient retention
protects snapshots for absent drives indefinitely, recreating the exact accumulation
pattern that caused the catastrophic NVMe exhaustion. The fix is architecturally sound —
it adds drive-availability awareness to the protection-set computation without violating
ADR-106 (pin protection) or ADR-107 (fail-closed deletions). One finding requires a
code-level fix before shipping: the `mounted_pins` filter must include `TokenMissing`
drives, not just `Available`, to avoid silently unprotecting chain parents for first-use
drives.

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting a snapshot that is the only
remaining chain parent for a mounted drive, causing the next send to fail or (worse)
succeed with a full send the user doesn't expect on a space-constrained drive.

**Distance from this plan:** The critical path is Step 4's semantic inversion: transient
+ no mounted pins = protect nothing. If the `mounted_pins` set incorrectly excludes a
drive that CAN receive sends, a chain parent gets deleted while the drive is connected.
The next send would be a full send instead of incremental. Not silent data loss, but
unexpected space consumption and send time. **One filtering bug away from incorrect
retention.**

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Root cause correctly identified. Fix logic is sound. One filter gap (TokenMissing) must be addressed. |
| 2 | **Security** | 5 | No new privilege escalation paths. No new sudo calls. No path construction changes. |
| 3 | **Architectural Excellence** | 5 | Respects all module boundaries. Planner stays pure. No new types or modules needed. Graduated retention explicitly unchanged. |
| 4 | **Systems Design** | 4 | Production scenario well-analyzed. Executor interaction correctly scoped to "planner only" (Option A). One edge case (TokenMissing) missed. |

**Overall: 4.5 / 5** — Surgical, well-reasoned, correctly scoped. Fix the filter gap
and this ships clean.

## Design Tensions

### T1: Transient semantics inversion (protect-everything → protect-nothing)

**Trade-off:** The plan changes "no pins + send_enabled = protect all snapshots" to
"no *mounted* pins + transient = protect nothing." This trades a conservative default
(never delete what might matter) for a correct one (don't protect snapshots for drives
that can't receive them).

**Was this the right call?** Yes. The conservative default caused the exact problem —
indefinite accumulation on a 118GB NVMe. The inversion is scoped precisely: only
transient subvolumes, only when evaluating mounted-drive pins. Graduated retention
keeps the old behavior. The design doc's rationale is sound: if no mounted drive has a
pin, there's nothing to protect *for*, because absent drives will get full sends when
they return anyway.

**Residual risk:** A race where a drive mounts between planner and executor. The planner
sees no mounted pins, plans deletions, and the executor runs them. Then the drive mounts
and wants an incremental send, but the chain parent is gone. Mitigated by: (a) the
advisory lock prevents concurrent runs, (b) if it did happen, the result is a full send,
not data loss. Acceptable.

### T2: Planner-only vs planner+executor (Option A vs B)

**Trade-off:** The plan changes only the planner's protection-set computation, leaving
the executor's `attempt_transient_cleanup` unchanged. The executor still reads ALL drive
pin files (including absent drives) when deciding what to clean up.

**Was this the right call?** Yes, for this patch. The executor's cleanup is a "timing
optimization" (executor.rs:842) that accelerates deletions the planner would produce on
the next run. It has its own safety re-check (re-reads pin files, fail-closed on parse
errors). Changing it in the same patch adds risk for marginal benefit — the planner will
catch anything the executor misses on the next run. The executor change can follow if the
cleanup's ALL-drives pin read becomes a practical issue (unlikely for 1-3 drives).

### T3: `broken >= 2` broadens anomaly detection

**Trade-off:** The old logic required ALL chains to break (`intact == 0`). The new logic
fires when 2+ chains break regardless of survivors. This catches a scenario the old
logic missed (4→2 intact = 2 broke simultaneously) but also fires in scenarios the old
logic intentionally ignored.

**Was this the right call?** Yes. Two chains breaking simultaneously on the same drive IS
suspicious regardless of how many survive. The threshold of 2 prevents noise from single
chain breaks (normal operational events). The old `intact == 0` requirement was an
artifact of the original design, not a deliberate threshold.

## Findings

### S1 — Significant: `mounted_pins` filter excludes `TokenMissing` drives

**What:** Step 2 computes `mounted_pins` by filtering for
`DriveAvailability::Available` only:

```rust
.filter(|d| matches!(fs.drive_availability(d), DriveAvailability::Available))
```

**Why it matters:** `DriveAvailability::TokenMissing` drives proceed with sends
(plan.rs:243-246 falls through to `plan_external_send`). If a first-use drive has
`TokenMissing` status, sends succeed and a pin file is created. But that pin won't
be in `mounted_pins` because the filter excludes non-`Available` drives. For transient
subvolumes, this means the chain parent for a TokenMissing drive could be deleted while
the drive is connected — the next send would be a full send instead of incremental.

**Consequence:** Not silent data loss (the snapshot exists on the drive), but unexpected
full sends on drives that just received an incremental send. For large subvolumes this
could exhaust drive space or take hours.

**Suggested fix:** Change the filter to:

```rust
.filter(|d| matches!(
    fs.drive_availability(d),
    DriveAvailability::Available | DriveAvailability::TokenMissing
))
```

And apply the same filter to `any_drive_mounted`. Both computations must agree on which
drives are "usable for sends."

**Distance from catastrophic failure:** 2 steps — filter bug → chain parent deleted →
full send (not data loss, but operational disruption on space-constrained drives).

### S2 — Significant: Missing test for `TokenMissing` drive interaction

**What:** The test strategy (Step 4, tests 3-8) covers mounted/absent/no-pins scenarios
but none test a `TokenMissing` drive. Given S1, this is the exact edge case that needs
a test.

**Suggested fix:** Add test:
- `transient_token_missing_drive_pin_protected` — 1 drive with
  `DriveAvailability::TokenMissing`, pin exists. Assert: pin IS in `mounted_pins`,
  snapshot IS protected.

### M1 — Moderate: Step 3 `continue` suppresses per-drive skip entries

**What:** When no drives are mounted for a transient subvolume, the `continue` at Step 3
skips the external block entirely. This means the plan output won't contain individual
"drive X not mounted" skip entries — only the single "transient — no drives available
for send" entry.

**Why it matters:** Downstream consumers of plan output (voice.rs rendering, dry-run
display) may use per-drive skip entries for drive-specific advice. The aggregated skip
loses information about which specific drives are absent. For a user with 3 drives where
2 are intentionally absent, the message doesn't tell them which drives were checked.

**Suggested fix:** This is acceptable as-is for v0.11.1 — the single message is more
informative than three redundant "not mounted" entries. Note it for potential enhancement
if users request drive-specific detail in transient skip reasons.

### M2 — Moderate: Sentinel detection bug root cause is unclear

**What:** The design doc states the bug is `detect_simultaneous_chain_breaks` firing
"when prev_count=0 (vacuously true when 0 chains were intact previously)." But the
detection logic iterates `prev_intact` (a BTreeMap populated only when
`snap.chain_intact` is true). A drive with 0 intact chains in the previous state would
have no entry in `prev_intact` and would never be visited.

The actual log message ("all 0 chains broke on 2TB-backup simultaneously" with
`total_chains: 0`) would require `total > 0` to fail — but `total > 0` is an explicit
guard (line 819).

**Why it matters:** If the root cause is misidentified, the fix might not address the
real issue. The proposed `broken >= 2` logic is strictly better regardless (it eliminates
a class of false positives), but the plan should verify: (a) is the bug from a version
before the `total > 0` guard was added? (b) is it from a different code path? (c) was
the log message observed in a development build without the guard?

**Suggested fix:** Before implementing, reproduce the exact scenario with the current
code and verify the bug exists in the shipped v0.11.0 binary. If the `total > 0` guard
already prevents it, the `broken >= 2` change is still an improvement but should be
documented as a refinement, not a bug fix. Adjust the log message fix regardless (the
"all N chains" phrasing is misleading when not all broke).

### C1 — Commendation: Correct scoping of the semantic change

The plan explicitly preserves graduated retention behavior unchanged. The `effective_pinned`
dispatch — `if transient { mounted_pins } else { pinned }` — is the minimal intervention
that fixes transient without touching graduated. This is exactly right. Graduated
subvolumes have different failure modes (they keep many snapshots, space pressure is
gradual), and protecting absent-drive pins is the correct conservative default for them.
The plan names this explicitly in Risk Flag 1 and tests it in test 6
(`graduated_absent_drive_pins_still_protected`). This is disciplined.

### C2 — Commendation: Production-informed design

This plan was born from an actual nightly run (run #29), not hypothetical analysis. The
four fixes map directly to four observed symptoms, each with a concrete scenario. The
design doc includes the exact NVMe free space (26GB of 118GB), the exact drives involved,
and the exact log messages. This is how you design bug fixes — from the incident, not from
the spec.

### C3 — Commendation: UPI 011 subsumption is well-reasoned

Absorbing UPI 011's scope while simplifying it is the right call. UPI 011's "cap of 1"
violated ADR-106 (overriding pin protection). The plan correctly identifies that fixing
the root cause (absent-drive protection) makes the cap unnecessary. Taking UPI 011's
creation-skip guard (defense in depth) while dropping the cap shows good judgment about
which safety layers add value and which add risk.

## The Simplicity Question

**What could be removed?** Nothing. The plan is already minimal — 4 fixes, ~15 lines of
new logic, 2 additive struct fields, 1 string change, 1 config edit. There are no new
types, no new modules, no new abstractions. The `mounted_pins` set is computed inline
from existing data, not reified into a new type. This is the right level of machinery.

**What's earning its keep?** The `any_drive_mounted` boolean (Step 2) seems like it could
be derived from `mounted_pins.is_empty()` — but it can't. A drive can be mounted (sends
proceed) without having a pin file (no send has succeeded yet). So `any_drive_mounted`
answers "can any drive receive a send?" while `mounted_pins.is_empty()` answers "has any
mounted drive already received a send?" Both questions matter for different guards.

## For the Dev Team

Priority-ordered action items:

1. **Fix S1 (filter gap).** In Step 2, change the `mounted_pins` and `any_drive_mounted`
   filters to include `DriveAvailability::TokenMissing`:
   ```rust
   .filter(|d| matches!(
       fs.drive_availability(d),
       DriveAvailability::Available | DriveAvailability::TokenMissing
   ))
   ```
   Both computations. File: `src/plan.rs`, Step 2 of plan.

2. **Add S2 test.** Add test `transient_token_missing_drive_pin_protected` to Step 4's
   test suite. Use `drive_availability_overrides` with `DriveAvailability::TokenMissing`.

3. **Verify M2 (sentinel bug).** Before implementing Step 6, run the sentinel detection
   function with the exact scenario from run #29 to confirm the bug exists in the
   shipped code. If the `total > 0` guard already prevents it, document the change as
   a refinement.

4. **Proceed with all other steps as planned.** No changes needed to Steps 1, 3, 4
   (logic), 5, 7.

## Open Questions

1. **Was the "all 0 chains broke" log observed with v0.11.0 or an earlier build?** If
   the `total > 0` guard was added after the observation, the bug is already fixed in
   shipped code. The `broken >= 2` change is still worth shipping as a refinement.

2. **Should `any_drive_mounted` also consider `TokenMissing`?** The answer is yes (per
   S1), but confirm: are there scenarios where a `TokenMissing` drive should NOT trigger
   snapshot creation for transient subvolumes? (I believe the answer is no — if the drive
   can receive sends, creating a snapshot for it is correct.)
