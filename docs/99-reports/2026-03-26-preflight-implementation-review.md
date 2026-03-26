# Implementation Review: Pre-flight Checks Module

**Project:** Urd
**Date:** 2026-03-26
**Scope:** `src/preflight.rs` (new), integration changes in `commands/backup.rs`,
`commands/init.rs`, `commands/verify.rs`, `main.rs`
**Reviewer:** Arch-adversary
**Mode:** Implementation review (6 dimensions)

## Executive Summary

Clean, well-scoped module that follows the project's established patterns. The core retention
model is technically sound but its warning message overstates the risk — the three-layer pin
protection system means the scenario described ("pinned parent may be deleted") can't actually
happen through normal operation. The implementation is solid; the messaging needs calibration.

## What Kills You

**Catastrophic failure mode: silent data loss via retention deleting pinned snapshots.**

**Distance from this code: three layers away.** The preflight module is read-only — it examines
config and produces warnings. It never touches filesystem state, never modifies pins, never
interacts with retention. It cannot cause data loss. The question is whether its *warnings* are
accurate, because a misleading warning can cause a user to make a wrong operational decision
(e.g., changing retention policy based on a false alarm, inadvertently reducing protection).

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 3 | Retention model math is right, but the warning describes a scenario the pin system prevents. See Finding 1. |
| **Security** | 5 | Pure function, no I/O, no privilege escalation, no trust boundary changes. |
| **Architectural Excellence** | 5 | Clean module boundary. Pure function of config. Follows ADR-108. Does not contaminate existing modules. Integration is minimal and non-intrusive. |
| **Systems Design** | 4 | Runs at the right time in each command. Warnings merge naturally into existing output. One edge case in backup dry-run path. |
| **Rust Idioms** | 4 | Clean, idiomatic code. `#[must_use]` on public function. Minor: single-variant enum is unusual (see Finding 3). |
| **Code Quality** | 4 | Good test coverage, clear naming, well-documented. Tests cover the motivating scenario and edge cases. |

## Design Tensions

### Tension 1: Warning accuracy vs. warning usefulness

The retention/send compatibility model correctly computes the guaranteed survival floor. But
the warning message says "pinned parent may be deleted before next send, forcing a full send."
In reality, the pinned parent is protected by three independent layers: the planner's unsent
snapshot protection, retention's `is_pinned` check, and the executor's re-check. The pinned
parent will NOT be deleted by retention.

The warning is still useful — it detects a config inconsistency where the retention windows
are tighter than the send interval, which is a smell. But the *consequence* described is wrong.
The actual consequence is subtler: if the user manually deletes the pin file, or if a future
code change weakens pin protection, this config would be vulnerable. It's a defense-in-depth
signal, not an active threat.

**Resolution:** The math should stay. The message should be reframed.

### Tension 2: Preflight in backup vs. log-only

The design calls for preflight warnings to be logged via `log::warn!` AND included in the
`BackupSummary.warnings`. This means the same warning appears in two places: the log output
during the run, and the structured summary at the end. For an autonomous nightly run viewed
in journal output, this is fine — redundancy is cheap. For an interactive run, the user sees
it twice. The current approach is correct: the log line serves daemon mode, the summary serves
interactive mode. The duplication is the cost of serving both.

## Findings

### Finding 1: Warning message overstates risk (Significant)

**What:** The retention/send compatibility warning says: "pinned parent may be deleted before
next send, forcing a full send."

**Why it matters:** The three-layer pin protection system (planner unsent protection in
`plan_local_retention` lines 326-348, retention's `is_pinned` guard in `graduated_retention`
line 74/88/99/108/116/121, and executor re-check) means the pinned parent snapshot cannot be
deleted by automated retention. The pin file records the last successfully sent snapshot. That
snapshot name is always in the `pinned` set. Retention always checks `is_pinned` before
deleting. This is the system working as designed.

The status.md note about htpc-root says "retention policy deletes pinned snapshot before the
next send interval can use it as incremental parent." If this actually happened, it would
represent a bug in the pin protection system, not a config incompatibility. More likely, the
chain break was caused by a different mechanism (legacy pin migration, manual deletion, or a
period before pin protection was fully implemented).

**Consequence:** A user reading this warning might take unnecessary action (changing retention
policy, adding workarounds) based on a threat that the system already handles. The warning
reduces trust in the pin protection system.

**Suggested fix:** Reframe the message to describe what the config inconsistency actually
means:

```
"{name}: retention window ({survival_display}) is shorter than send interval ({interval_display}).
 Snapshots are currently protected by pin files, but this config depends on pin protection
 rather than retention to keep incremental parents alive."
```

Or more concisely: flag it as a config hygiene issue without claiming the pinned parent will
be deleted. The warning is valuable as "your retention and send intervals are misaligned" —
just don't claim a consequence that the existing safety system prevents.

### Finding 2: Backup dry-run path skips preflight warnings in summary (Moderate)

**What:** In `backup.rs` line 67-70, the dry-run path returns early after printing the plan,
before `build_backup_summary` is called. Preflight warnings are logged via `log::warn!`
(line 63-64), but they never appear in the plan output.

**Consequence:** `urd backup --dry-run` logs preflight warnings to stderr (if log level
allows), but they're not visible in the plan display. An interactive user running `--dry-run`
to preview their backup may miss config warnings.

**Suggested fix:** Either (a) print preflight warnings before the plan output in the dry-run
path, or (b) accept this as a known limitation since `urd plan` also doesn't show them. If
(b), consider that `urd verify` is the intended surface for config warnings, and `plan` is
for operational preview only.

### Finding 3: Single-variant enum `Severity` (Minor)

**What:** `Severity` has only one variant: `Warning`. The `Error` variant was removed during
implementation to avoid a dead-code warning.

**Consequence:** An enum with one variant is semantically equivalent to a unit struct — it
carries no information. Every check produces `Severity::Warning`. The `severity` field on
`PreflightCheck` always has the same value.

**Suggested fix:** Two options:
- (a) Remove the `severity` field entirely. All preflight checks are warnings. If `Error`
  severity is needed later (when init's I/O checks migrate), add it then.
- (b) Keep the enum as-is, anticipating that Error will be added when more checks arrive.
  This is the "leave room for growth" argument — reasonable given the design doc plans
  for future checks.

Option (a) is simpler. Option (b) is fine if it doesn't trigger clippy.

### Finding 4: `send-without-drives` emits one warning per subvolume (Minor)

**What:** If the config has 5 subvolumes with `send_enabled = true` and no drives, the user
gets 5 identical warnings (differing only in subvolume name). The drives configuration is
global, not per-subvolume.

**Consequence:** Noisy output when the underlying issue is a single config-level fact: "no
drives configured." Five warnings for one problem.

**Suggested fix:** Check `config.drives.is_empty()` once at the top of `preflight_checks`,
outside the per-subvolume loop, and emit a single warning. The per-subvolume loop doesn't
add information for this particular check.

### Commendation: Pure function boundary

The decision to accept only `&Config` (not `&dyn FileSystemState`) is the right call. It
was explicitly reviewed in the design review (Finding 2) and the implementation honors that
decision. The module cannot regress into I/O-dependent code without changing its public
signature. This is the kind of constraint that prevents architectural drift — not through
documentation, but through the type system.

### Commendation: Integration minimalism

The integration into backup, init, and verify is minimal: 3-5 lines in each command. The
preflight module has zero coupling to command internals. Adding it to `build_backup_summary`
required only a new parameter, not a structural change. This is well-calibrated — the feature
fits into the existing architecture without reshaping it.

## The Simplicity Question

**What could be removed?** The `severity` field (Finding 3) — it's always `Warning`. The
per-subvolume `send-without-drives` check (Finding 4) — it should be a single global check.

**What's earning its keep?** The retention/send compatibility model. The `hourly + daily * 24`
guaranteed survival floor is the right abstraction. It correctly handles the design review's
test cases. The model is sound even though the warning message needs reframing.

**Overall:** This is a well-sized module. 115 lines of production code, 245 lines of tests.
Two checks. Nothing speculative.

## For the Dev Team

Priority order:

1. **Reframe the retention/send warning message** (Finding 1, `preflight.rs` line 82-89).
   Remove the claim that "pinned parent may be deleted." Replace with a message that describes
   the config inconsistency without claiming a consequence that pin protection prevents. The
   math stays; the framing changes.

2. **Move `send-without-drives` check outside the per-subvolume loop** (Finding 4,
   `preflight.rs` line 96-114). Check `config.drives.is_empty()` once. Emit one warning
   listing which subvolumes have `send_enabled`, or just state "no drives configured but
   send_enabled subvolumes exist."

3. **Decide on dry-run preflight visibility** (Finding 2, `backup.rs` line 67-70). Either
   add a preflight print before the plan output in the dry-run path, or accept that `urd
   verify` is the surface for config warnings. Both are reasonable.

4. **Optionally simplify Severity** (Finding 3, `preflight.rs` line 24-27). Remove the
   field if it's not earning its keep. Minor.

## Open Questions

1. **What actually caused the htpc-root chain break?** The status.md note says retention
   deleted the pinned parent, but the pin protection system should prevent this. Was it during
   the legacy pin migration period? Was pin protection not yet implemented? Understanding the
   root cause would clarify whether the preflight warning is detecting a real operational risk
   or a theoretical one. If the chain break happened before the three-layer defense was in
   place, the warning is purely forward-looking ("if pin protection ever weakens, this config
   is vulnerable").

2. **Should preflight checks run on `urd plan`?** Currently `plan_cmd.rs` doesn't call
   preflight. The `plan` command shows what operations would happen — config consistency
   warnings would be natural there too. But the design may intentionally keep `plan` focused
   on operational preview.
