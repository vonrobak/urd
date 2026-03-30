# Architectural Review: Transient Snapshots Implementation

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review of transient snapshots feature (`local_retention = "transient"`)
**Base commit:** `b0d0117` (v0.4.3)
**Files reviewed:** `src/types.rs`, `src/config.rs`, `src/plan.rs`, `src/preflight.rs`, `config/urd.toml.example`
**Mode:** Implementation review (6 dimensions)

---

## Executive Summary

Clean, disciplined implementation that correctly reuses the existing unsent-snapshot
protection and three-layer pin defense. The core insight — that transient retention
is just "delete everything not in the protected set" — is exactly right and leverages
existing planner infrastructure. Two findings matter: one significant (awareness reports
UNPROTECTED when there are 0 local snapshots between send cycles, which will be the
normal transient state), one moderate (missing test for the multi-drive pin interaction
that is the most subtle correctness property of this feature).

---

## What Kills You

The catastrophic failure mode is **deleting the last local copy of a snapshot before it
reaches any external drive**. This is silent data loss — the user thinks they're protected,
but the transient cleanup deleted data that was never sent.

Distance from catastrophe: **three independent layers prevent this.** (1) The planner's
unsent-snapshot protection expands the protected set to include everything newer than the
oldest pin. (2) Transient retention only deletes snapshots outside the protected set. (3)
The executor's defense-in-depth re-reads pin files from disk before every delete. All three
layers are reused from existing code, not newly written. The implementation is well-distanced
from catastrophe.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Core deletion logic is correct. Unsent protection correctly inherited. One awareness gap (S1). |
| 2 | **Security** | 5 | No new trust boundaries, no new sudo paths, no path construction changes. |
| 3 | **Architectural Excellence** | 5 | Leverages existing infrastructure perfectly. No new abstractions that don't earn their keep. Type system enforces transient-not-in-defaults at parse time. |
| 4 | **Systems Design** | 4 | Correct in steady state. Awareness model interaction (S1) needs attention for operational clarity. |
| 5 | **Rust Idioms** | 5 | Custom serde visitor is the right approach (better errors than `#[serde(untagged)]`). `as_graduated()` convenience method is clean. |
| 6 | **Code Quality** | 4 | Clear, readable code. Tests cover the critical paths. One coverage gap for multi-drive (M1). |

---

## Design Tensions

### 1. Reuse existing protection vs. transient-specific logic

**Tension:** The transient path reuses the same unsent-snapshot protection logic as
graduated retention (lines 348-374 of plan.rs). This means transient behavior is
coupled to graduated retention's protection semantics — if those semantics change,
transient changes too.

**Resolution:** Correct call. The unsent-snapshot protection logic is load-bearing
for data safety in both modes. Having one implementation means one place to get right
(or wrong). The alternative — duplicating the protection logic for a transient-specific
path — would be strictly worse: same semantics, two implementations, inevitable
divergence. The coupling here is *desirable* coupling.

### 2. Awareness model ignorance vs. transient-aware assessment

**Tension:** The implementation explicitly chose not to modify `awareness.rs`. With
the pinned-snapshot approach, transient subvolumes will usually have exactly 1 local
snapshot, and existing assessment works. But between the send-delete-create cycle
(after transient deletes old snapshots and before a new one is created), there may be
moments with 0 local snapshots, which awareness reports as UNPROTECTED.

**Resolution:** This is the one tension that wasn't resolved correctly. See S1.

---

## Findings

### S1 — Significant: Awareness reports UNPROTECTED for transient subvolumes at rest

`awareness.rs:369` — when `snapshots.len() == 0`, `assess_local()` returns
`PromiseStatus::Unprotected`. For a transient subvolume after a successful send cycle
(create → send → pin advances → transient deletes old snapshots), the normal state is
exactly 1 local snapshot (the pinned one). But there are realistic windows where the
count is 0:

1. **Between runs when pin is the only snapshot:** If the pinned snapshot is also the
   only local snapshot, and the next `plan()` call creates a new one and deletes the
   old pin (after the send advances), there's a window during execution where 0 local
   snapshots exist (old pin deleted, new one not yet pinned).

2. **First run before any send:** No pins, `send_enabled = true` → unsent protection
   keeps everything. Count will be > 0. This case is fine.

3. **After send to all drives + transient cleanup, before next create:** The pin points
   to the latest sent snapshot, which is still local. Count = 1. This case is fine.

On closer inspection, the pinned snapshot is always protected by the transient logic
and should remain. The real problem is narrower: **`urd status` shows UNPROTECTED for
the local axis** whenever the pinned snapshot ages past the assessment threshold (which
it will, since transient subvolumes don't create new snapshots between send intervals).
A transient subvolume with a 1-day send interval will show its local snapshot aging to
24h, crossing the UNPROTECTED threshold at ~5× the *snapshot* interval — which may be
1 hour. The local snapshot is 24h old, the threshold is 5h → local = UNPROTECTED. But
for transient, this is *expected* — the data is on the external drive.

**Consequence:** Alert fatigue. `urd status` and sentinel health notifications will
show transient subvolumes as locally UNPROTECTED most of the time, even when external
copies are recent. The user learns to ignore the local status for these subvolumes,
which reduces their trust in the overall status display.

**Suggested fix:** Not necessarily now, but flag for the next session. Two options:
(a) Make awareness aware of transient mode — skip local assessment or report
`PromiseStatus::Protected` when the mode is transient and external status is good.
(b) Add an advisory note like "local_retention = transient: local status reflects
external send recency" to suppress the misleading signal. Either way, the overall
`SubvolAssessment.status` should weight external assessment more heavily for transient
subvolumes.

### M1 — Moderate: No test for multi-drive transient interaction

The most subtle correctness property of transient mode is the multi-drive case: with
2 drives (D1 and D2), the pinned set is the *union* of both drives' pins. If D1's
pin is at snapshot A and D2's pin is at snapshot B (older), unsent protection keeps
everything from B onward. Transient then deletes nothing older than B.

This behavior is *correct* (it falls out of the existing unsent-protection logic), but
there's no test that exercises it. The `transient_config()` test helper has only one
drive. A multi-drive test would verify the interaction that matters most for real-world
use (the design doc envisions `drives = ["WD-18TB1"]` but nothing prevents multi-drive
transient configs).

**Suggested fix:** Add a test with 2 drives where pins are at different snapshots.
Verify that all snapshots between the older pin and the newer pin are protected (unsent
to the drive with the older pin).

### M2 — Moderate: `urd plan` output doesn't distinguish transient from graduated deletes visually

The reason string `"transient: not pinned"` is the only signal that a delete is
transient. In `urd plan` output, transient deletes appear as:

```
DELETE  /snap/sv1/20260320-1000-one (transient: not pinned)
```

For a user scanning plan output, the distinction between "retention expired this
snapshot" and "transient mode is cleaning up" is important — it tells them whether
this is normal aging or active cleanup. The reason string carries the signal, but it's
buried in parentheses alongside graduated retention reasons like "(weekly window
expired)".

**Suggested fix:** This is fine for now — the reason string is there. If transient
mode becomes commonly used, consider adding a section header in `urd plan` output like
"Transient cleanup (sv1): 3 snapshots" to make it scannable. Low priority.

### C1 — Commendation: Type system enforcement of transient-not-in-defaults

`DefaultsConfig.local_retention` stays as `GraduatedRetention`, not the new enum.
This means `local_retention = "transient"` in the `[defaults]` section is a parse
error at config load time — not a preflight warning, not a runtime check, a type error.
The user gets an error immediately, not after their first backup run. This is the right
level of enforcement for a setting that is semantically nonsensical as a default.

### C2 — Commendation: Zero new abstractions

The implementation adds exactly two types (`LocalRetentionConfig` for serde,
`LocalRetentionPolicy` for resolved config) and touches no module that doesn't need
touching. No new traits, no new helper modules, no new operation variants. The transient
delete path in the planner is 7 lines. The existing unsent-snapshot protection, pin
exclusion, and executor defense-in-depth all apply unchanged. This is a feature that
was designed to slot into the existing architecture, and it does.

### C3 — Commendation: Custom serde visitor with clear errors

The custom `Deserialize` implementation on `LocalRetentionConfig` produces error
messages like `unknown local_retention mode "bogus": expected "transient" or a
retention table`. Compare to `#[serde(untagged)]` which would produce `data did not
match any variant`. For a config file the user edits by hand, error quality matters —
a confusing parse error at 04:00 when the backup fails is the difference between a
5-minute fix and a frustrated hour.

---

## The Simplicity Question

**What could be removed:** Nothing. This is already minimal. Two enums, a serde
visitor, a match branch in the planner, two preflight checks. Every piece earns its
keep.

**What's earning its keep:**
- `as_graduated()` convenience method — eliminates match boilerplate at every consumer
- Custom serde visitor — better error messages than the alternative
- Preflight checks for transient+no-send and transient+named-level — catches the two
  config mistakes a user is most likely to make

---

## For the Dev Team

Priority-ordered action items:

1. **Add multi-drive transient test** (plan.rs). Create a test with 2 drives, pins
   at different snapshots. Verify unsent protection keeps snapshots between the two
   pins. This exercises the most important interaction.
   - File: `src/plan.rs`, tests section
   - Why: the multi-drive case is the subtle correctness property; existing tests
     only use 1 drive

2. **Flag awareness interaction for next session.** Transient subvolumes will report
   local status as UNPROTECTED when the pinned snapshot ages past the assessment
   threshold. This is expected for transient but confusing for users. Options: (a)
   teach awareness about transient mode, (b) add an advisory note. Not urgent — the
   external assessment still shows PROTECTED when sends are recent, and the overall
   promise status aggregates both. But it will cause noise in `urd status`.
   - File: `src/awareness.rs` (future work)
   - Why: prevents alert fatigue and confusion in status output

---

## Open Questions

1. **Should `urd status` show a "transient" indicator in the LOCAL column?** Something
   like "1 pinned" instead of "1 (12h)" to signal that the low count and old age are
   expected. This is a voice.rs concern, not an awareness concern.

2. **What happens when a transient subvolume's only drive is unmounted for weeks?**
   The unsent protection keeps all local snapshots (correct, since nothing has been
   sent). But local snapshots accumulate without transient cleanup running. The space
   guard (`min_free_bytes`) prevents exhaustion, but the user may be surprised that
   "transient" doesn't mean "always 0-1 local snapshots." Worth documenting.

3. **Should transient mode interact with the sentinel's health notifications?** A
   transient subvolume with `OperationalHealth::Blocked` (no drives mounted) is in a
   state where local snapshots will accumulate. Should the sentinel flag this
   specifically for transient subvolumes? Probably not yet — the existing Blocked
   notification covers it.
