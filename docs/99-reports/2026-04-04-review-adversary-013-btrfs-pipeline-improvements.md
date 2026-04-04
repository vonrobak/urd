---
upi: "013"
date: 2026-04-04
---

# Architectural Adversary Review: UPI 013 — BTRFS Pipeline Improvements

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-04
**Scope:** Design review of `docs/95-ideas/2026-04-03-design-013-btrfs-pipeline-improvements.md`
**Mode:** Design review (4 dimensions)
**Base commit:** `9974c4c`
**Reviewer:** arch-adversary

---

## Executive Summary

A clean, well-scoped design with one significant misunderstanding of the executor's
execution model. 013-a (compressed sends) is straightforward and sound. 013-b (sync
after delete) has the right instinct but proposes inserting the sync "after the deletion
loop" — the executor has no deletion loop. Deletes are individual `PlannedOperation`
items interleaved with creates and sends, each processed in `execute_subvolume`'s single
`for op in ops` pass. The design needs to reconcile its sync placement with this reality.

## What Kills You

**Catastrophic failure mode:** Silent data loss through deleting snapshots that shouldn't
be deleted, or space exhaustion causing failed sends with partial snapshots.

**Distance from this design:** Far. Neither 013-a nor 013-b introduces new deletion logic
or modifies retention decisions. 013-a adds a flag to an existing send command. 013-b adds
a sync that, at worst, is a no-op. Neither change moves closer to the catastrophic failure
mode. This is a low-risk design.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | 013-a is sound. 013-b has the right semantics but the "deletion loop" placement doesn't exist as described — needs adjustment. |
| 2 | **Security** | 5 | No new trust boundaries, no new paths to sudo, no new user input. Both changes follow existing `Command::arg(&Path)` patterns. |
| 3 | **Architectural Excellence** | 4 | Clean separation: probe outside `BtrfsOps`, sync inside it. `SystemBtrfs` as a startup-only struct is the right call. |
| 4 | **Systems Design** | 3 | The sync placement assumes an execution model the code doesn't have. The `--help` probe has a minor portability concern. |

## Design Tensions

### 1. Sync granularity vs. executor structure

The design wants to sync once per "retention batch" — the right instinct for efficiency.
But the executor processes operations as a flat list: create, create, delete, send, delete,
delete. There is no "retention batch" boundary visible to the executor. The design must
choose between:

- **(A) Sync per-delete inside `execute_delete`.** Simple, correct, but calls sync N times
  for N deletes. Each sync flushes all pending commits, so the 2nd through Nth are near-instant
  if the first completed. Overhead is N subprocess spawns, not N flushes.
- **(B) Accumulate "needs sync" state and sync before the first space check.** More complex,
  requires the executor to track whether any deletes happened since the last sync. Better
  efficiency but adds state tracking.
- **(C) Sync once at the end of `execute_subvolume`, after all operations.** Simple, but
  doesn't help the space check within `execute_delete` — the check runs before the sync
  would happen.

**Recommendation:** Option A is the pragmatic choice. The subprocess overhead of redundant
syncs is negligible compared to the actual btrfs commit time. The first sync in a batch does
the real work; subsequent syncs return quickly. This avoids any executor-level state tracking
and keeps the change contained within `execute_delete`.

### 2. Probe mechanism: `--help` parsing vs. version comparison

The design probes `btrfs send --help` for the `--compressed-data` string. This is pragmatic
and avoids maintaining a version-to-feature mapping. The trade-off: it depends on the help
text being stable across btrfs-progs releases. The alternative (parsing `btrfs version` and
comparing against 5.18) is more fragile — distribution patching, backport variations, and
version string format differences make this worse. The design made the right call.

## Findings

### S1 — Significant: The "deletion loop" doesn't exist in the executor

**What:** The design states: "sync_subvolumes is called once per retention batch, not once
per deleted snapshot. The executor already groups retention deletions by subvolume; the sync
follows all deletions for a given snapshot root." This is inaccurate.

**Reality:** The executor (`executor.rs:245-404`) processes all operations for a subvolume
in a single `for op in ops` loop. The planner emits operations in load-bearing order:
`create → local retention deletes → sends → external retention deletes` (plan.rs:148-150).
There is no "retention batch" boundary — each `PlannedOperation::DeleteSnapshot` is handled
individually by `execute_delete`, which includes its own per-delete space check
(executor.rs:746-773).

**Consequence:** The design's proposed insertion point ("after the deletion loop") doesn't
map to a real code location. The implementation will need to either:
1. Add sync inside `execute_delete` (per-delete, option A above), or
2. Restructure the executor to batch deletes (high-risk refactor for minimal gain).

**Suggested fix:** Revise the design to place `sync_subvolumes` inside `execute_delete`,
called after a successful `delete_subvolume` and before the space check. This is the
minimal change that achieves correct space accounting.

### S2 — Significant: `--help` probe runs under `sudo` unnecessarily

**What:** The design's probe calls `Command::new("sudo").arg(btrfs_path).arg("send").arg("--help")`.
The `btrfs send --help` command doesn't require root privileges — it just prints usage
text and exits. Running it under sudo means:

1. If the sudoers configuration requires a password (TTY prompt), the probe blocks or fails
   at startup — before any backup logic runs.
2. On systems with `sudo` rate-limiting or logging, this adds an unnecessary privileged
   invocation.
3. The probe happens at process startup, potentially before the user expects any sudo
   interaction.

**Consequence:** On misconfigured or partially-configured systems, the probe could cause
a startup hang or confusing error before Urd has done anything useful. The fail-open
`unwrap_or(false)` handles the error case, but a hung process waiting for a password
prompt is not a graceful failure.

**Suggested fix:** Run `btrfs send --help` without `sudo`. The help text is identical
regardless of privileges. If `btrfs` isn't in PATH, the probe returns false — correct
behavior. Keep sudo exclusively for actual btrfs operations as ADR-101 intends.

### M1 — Moderate: Probe output assumption — stdout vs. stderr

**What:** The design combines stdout and stderr: `let combined = [o.stdout, o.stderr].concat()`.
This is defensive, but the comment "exits non-zero on older btrfs-progs" deserves
verification. On modern btrfs-progs (5.x+), `btrfs send --help` exits 0 and prints to
stdout. On very old versions, behavior varies. The design handles this correctly by
combining both streams, but the `unwrap_or(false)` on the outer `map` already handles
complete failure.

**Suggested fix:** This is fine as-is. Just noting that the combined-stream approach is
belt-and-suspenders — the code is correct, the comment could be more precise about which
versions exhibit non-zero exit.

### M2 — Moderate: `MockBtrfsCall::SendReceive` needs `compressed_data` field

**What:** The design correctly notes that `MockBtrfsCall::SendReceive` should gain a
`compressed_data: bool` field. However, the `BtrfsOps::send_receive` trait signature
doesn't change — `compressed_data` is an internal implementation detail of `RealBtrfs`,
not a trait parameter. The mock has no way to know whether the real implementation would
have injected the flag.

**Consequence:** The mock can't actually verify flag injection through the trait interface.
Tests can only verify that `RealBtrfs` has the `supports_compressed_data` field set
correctly. The flag injection itself is only testable via integration tests or by
inspecting the `Command` — which is the existing pattern for all btrfs.rs tests.

**Suggested fix:** Drop `compressed_data` from `MockBtrfsCall::SendReceive`. The mock
tests should verify the probe logic (unit tests on `SystemBtrfs::probe` with crafted
help text). The actual flag injection in `send_receive` is a 3-line conditional that
follows the existing pattern — the risk of it being wrong is minimal and not worth
complicating the mock interface.

### C1 — Commendation: `SystemBtrfs` as a separate startup-only struct

The decision to separate capability probing (`SystemBtrfs::probe`) from operation
execution (`BtrfsOps` trait / `RealBtrfs`) is well-reasoned. The rejected alternatives
(probe in constructor, probe in trait) are correctly identified as violations of existing
conventions. This keeps the trait clean as an operation contract and puts I/O where
callers can see it.

### C2 — Commendation: Sync failure semantics

The fail-open treatment of sync failure is exactly right. A failed sync leaves behavior
identical to today's code (no sync at all). The ADR-107 analysis is precise: "not worse
than today" is the correct bar for a fail-open decision. The log line at `warn!` level
ensures visibility without blocking the run.

### C3 — Commendation: No config knob for `--compressed-data`

The rejection of a config flag is well-argued. Protocol v2 compressed sends are strictly
better — there's no user-facing trade-off. Auto-detection via probe is the right UX:
the user never needs to know this feature exists, and it activates automatically when
the system supports it. This is "invisible worker" behavior at its best.

## Also Noted

- The `info!` vs `debug!` open question (OQ2): `debug!` is better. The flag's presence is
  a system capability, not per-transfer context. Log at `info!` once during probe, `debug!`
  per send.
- Assumption 4 (sudoers covers `btrfs subvolume sync`): correct — the typical grant is
  `NOPASSWD: /usr/bin/btrfs *`, which covers all subcommands.
- The design says 013-b should be built first — agree, it's simpler and purely additive.

## The Simplicity Question

Nothing to cut. Both sub-items are minimal changes to existing patterns. The `SystemBtrfs`
struct is the only new type, and it earns its keep by keeping the probe out of `RealBtrfs`.
The design's scope discipline is good — it resists adding anything that isn't directly
needed.

If anything, the mock changes could be *simpler* than proposed (see M2) — dropping the
`compressed_data` field from `MockBtrfsCall` removes complexity without losing meaningful
test coverage.

## For the Dev Team

Priority-ordered action items for `/post-review`:

1. **Revise 013-b sync placement.** The executor has no "deletion loop" to place the sync
   after. Place `sync_subvolumes` inside `execute_delete`, after the successful
   `delete_subvolume` call and before the space recheck (executor.rs:744-773). This makes
   each delete self-contained: delete → sync → check space. Redundant syncs after the first
   are near-instant (btrfs commits are already flushed).

2. **Remove `sudo` from the `--help` probe.** Run `btrfs send --help` directly, not through
   sudo. Help text doesn't require privileges and running it unprivileged avoids startup
   hangs on misconfigured sudoers.

3. **Drop `compressed_data` from `MockBtrfsCall::SendReceive`.** The trait interface doesn't
   expose the flag — the mock can't meaningfully verify it. Test the probe logic directly
   (crafted help text → `supports_compressed_data` bool) instead.

4. **Log probe result at `info!`, per-send flag at `debug!`.** Log "compressed data
   pass-through available" once at startup (info), "using --compressed-data" per send
   (debug).

## Open Questions

1. **Is the per-delete sync overhead acceptable?** On a typical nightly run, retention
   deletes 1-5 snapshots per subvolume. That's 1-5 sync calls, with the first doing real
   work and the rest returning near-instantly. For the emergency space-recovery path
   (potentially dozens of deletes), the overhead is higher but still bounded by real commit
   time. Monitor after first production run. If the overhead is measurable, batch-sync can
   be added later as an optimization — the per-delete approach is correct by construction.

2. **Does `btrfs send --help` work without `sudo` on all target systems?** Almost certainly
   yes — the help subcommand is client-side text rendering, not a privileged operation. But
   worth a quick manual check on the production host before committing to the no-sudo probe.
