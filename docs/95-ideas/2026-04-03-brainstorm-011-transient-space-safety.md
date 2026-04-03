# Brainstorm 011: Transient Subvolume Space Safety

**Status:** raw
**Date:** 2026-04-03
**Trigger:** Fifth NVMe space exhaustion incident. htpc-root configured as transient
but `urd backup` (manual) created 3 local snapshots that nearly filled the 118GB NVMe.
Pin files now reference deleted snapshots (user manually deleted to recover).

## Problem Statement

Transient retention is a deletion policy, not a creation policy. The planner creates
local snapshots unconditionally for all subvolumes. The create-then-delete architecture
has a fundamental gap: if external drives aren't mounted, unsent protection keeps all
local snapshots alive indefinitely. Manual runs (`urd backup`) bypass interval checks
entirely (`skip_intervals: !args.auto`), making accumulation even faster.

**Five incidents in 10 days.** Space guard (`min_free_bytes`) prevents the absolute
worst outcome but doesn't prevent accumulation up to the threshold. The 118GB NVMe
cannot sustain any htpc-root local snapshot history alongside htpc-home.

**Secondary damage:** Pin files (`.last-external-parent-*`) now reference snapshots
the user manually deleted. Next incremental send will either fail or fall back to
full send, wasting time and external drive space.

## Ideas

### 1. Transient-aware creation gate in `plan_local_snapshot()`

Make `plan_local_snapshot()` aware of transient retention. If `local_retention` is
`Transient`, only create a snapshot when:
- An external send is actually possible (at least one configured drive is mounted), OR
- No local snapshot exists at all (first-ever snapshot)

This turns transient from "create aggressively, delete aggressively" into "create only
when useful." The planner already has access to drive availability through `fs`.

**Modules touched:** `plan.rs` (add transient check to `plan_local_snapshot`)

### 2. External-only mode as first-class config

Instead of transient being a retention hack, introduce `mode = "external-only"` at the
subvolume level. External-only subvolumes:
- Never create local snapshots except as transient intermediaries for sends
- Create a local snapshot, send it, delete it — all in one executor pass
- If no drive is mounted, skip entirely (no local accumulation)

This makes the intent explicit in config rather than inferring it from retention policy.

**Modules touched:** `types.rs` (new mode enum), `plan.rs`, `executor.rs`, `config.rs`

### 3. Snapshot budget per filesystem

Instead of `min_free_bytes` (reactive threshold), give each filesystem a snapshot
budget: maximum number of snapshots OR maximum total exclusive data. The planner
refuses to create when the budget is exhausted.

This prevents gradual accumulation above the space guard threshold. Could be
per-subvolume or per-snapshot-root (all subvolumes on the same filesystem share
a budget).

**Modules touched:** `plan.rs`, `types.rs`, `config.rs`

### 4. Transient snapshot cap: hard limit of 1

For transient subvolumes, enforce a hard cap of 1 local snapshot at any time. The
planner refuses to create a second snapshot if one already exists (regardless of
`skip_intervals`, `force`, or send state). This is simpler than idea #1 — no need
to check drive availability — and directly prevents accumulation.

The one surviving snapshot is either the chain parent (pinned) or the latest unsent.
Either way, the space cost is bounded to exactly one snapshot's worth.

**Modules touched:** `plan.rs` (add count check to `plan_local_snapshot` for transient)

### 5. Atomic send-and-delete in executor

For transient subvolumes, restructure the executor to treat snapshot creation, send,
and local deletion as a single atomic unit of work:
1. Create snapshot
2. Send to all available drives
3. Update pin files
4. Delete the snapshot (and old pin parent) immediately

If no drives are available, step 1 is skipped entirely. No window for accumulation.

**Modules touched:** `executor.rs` (new transient execution path)

### 6. Drive-availability preflight for transient subvolumes

Add a preflight check: before planning any operations for a transient subvolume,
verify at least one configured drive is mounted. If none are mounted, skip the entire
subvolume (no local snapshot, no retention, nothing). Log it as a skip reason.

This is similar to idea #1 but happens at the subvolume level, before
`plan_local_snapshot()` is even called.

**Modules touched:** `plan.rs` (add preflight before the local operations block)

### 7. Pin file recovery / orphan detection

Address the secondary damage: when a pin file references a snapshot that no longer
exists on the local filesystem, detect this condition and handle it gracefully:
- Log a warning: "chain parent deleted, next send will be full"
- Clear the stale pin file (forces full send, which is correct)
- Or: scan external drive for the most recent snapshot and derive new pin from that

This doesn't prevent the space problem but prevents cascading damage from manual
recovery actions.

**Modules touched:** `chain.rs` (pin validation), `plan.rs` (stale pin handling),
potentially `executor.rs` (pre-send pin check)

### 8. `urd backup --auto` awareness for transient

Make `--auto` the only mode that creates snapshots for transient subvolumes. Manual
`urd backup` (without `--auto`) would skip transient subvolumes entirely, with a
message: "htpc-root is transient — use `urd backup --auto` or wait for the scheduled
run." This prevents the most dangerous scenario: repeated manual runs creating
snapshots that can't be sent.

**Modules touched:** `plan.rs` or `commands/backup.rs` (filter transient from manual)

### 9. Volume-aware snapshot scheduling

The planner currently treats each subvolume independently. Introduce volume awareness:
subvolumes that share a filesystem (same snapshot root or same mount point) should
coordinate. When htpc-root and htpc-home share the NVMe, the planner can:
- Prioritize htpc-home snapshots (higher data-change rate)
- Refuse htpc-root snapshots when htpc-home needs the space
- Apply the `min_free_bytes` budget considering all subvolumes on the volume

**Modules touched:** `plan.rs` (volume grouping), `config.rs` (volume detection),
`types.rs` (volume type)

### 10. Sentinel drive-gated transient backup

Instead of the nightly timer creating transient snapshots blindly, have the Sentinel
trigger transient backups only when the target drive is detected. Sentinel already
knows about drive plug/unplug events. When WD-18TB1 is plugged in:
1. Sentinel triggers backup for htpc-root
2. Snapshot created, sent, cleaned up
3. Drive removed — no orphan snapshots

This aligns transient backup timing with drive availability rather than a fixed schedule.

**Modules touched:** `sentinel.rs` (new event→action mapping), `sentinel_runner.rs`

### 11. Filesystem pressure notifications

Extend Sentinel's monitoring to track filesystem free space on snapshot roots. When
NVMe free space drops below a configurable threshold (higher than `min_free_bytes`),
emit an early warning notification before the space guard kicks in. The Discord alert
saved the day this time — make it part of Urd's own notification system.

**Modules touched:** `sentinel.rs` (new event type), `notify.rs` (space alert template)

### 12. `local_snapshots = false` — explicit opt-out

Instead of inferring "no local snapshots" from `local_retention = "transient"`, add
an explicit `local_snapshots = false` config field. When false:
- Planner never creates local snapshots
- Send operations use a temporary snapshot (created, sent, deleted atomically)
- No retention logic needed — nothing to retain

This is the clearest expression of intent: "I do not want local snapshots for this
subvolume, period."

**Modules touched:** `types.rs`, `config.rs`, `plan.rs`, `executor.rs`

### 13. Retroactive space reclaim command

`urd reclaim` — emergency command that identifies and deletes snapshots to recover
space, prioritizing:
1. Transient subvolume snapshots (should be zero anyway)
2. Snapshots beyond retention policy
3. Oldest snapshots on the most constrained filesystem

This doesn't prevent the problem but gives the user a safer alternative to manually
running `sudo btrfs subvolume delete`.

**Modules touched:** new command in `commands/`, uses `plan.rs` and `retention.rs`

### 14. Pin file self-healing with filesystem truth

Strengthen ADR-102 (filesystem is truth): before any incremental send, verify the
pin file's referenced snapshot actually exists as a local subvolume. If it doesn't:
- Check if the snapshot exists on the external drive
- If yes: the next send must be full (no local parent), but pin the new snapshot
- If no: full send from scratch, log the chain break
- Either way: remove the stale pin file

This makes pin files eventually consistent with filesystem reality, regardless of
manual interventions.

**Modules touched:** `executor.rs` (pre-send validation), `chain.rs` (pin healing)

### 15. Two-phase transient: snapshot-in-tmpfs

Wild idea: for transient subvolumes, don't create the snapshot on the source
filesystem at all. Use `btrfs send` piped directly to `btrfs receive` on the external
drive, without materializing the read-only snapshot on the source. BTRFS might not
support this directly, but the conceptual direction is: minimize source filesystem
impact for subvolumes that don't need local history.

**Feasibility:** Unknown. BTRFS requires a snapshot to exist for `send`. But the
snapshot could be created in a separate BTRFS filesystem mounted as tmpfs-backed
loopback? Probably too exotic.

### 16. Aggressive transient retention in same planning pass

Currently, `plan_local_snapshot()` creates and `plan_local_retention()` deletes in
the same plan — but unsent protection prevents deletion of anything newer than the
oldest pin. For transient subvolumes, modify unsent protection: instead of protecting
ALL unsent snapshots, protect only the NEWEST one. This caps accumulation at
pin + 1 newest, regardless of how many manual runs happened.

**Modules touched:** `plan.rs` (transient-specific unsent protection logic)

## Handoff to Architecture

1. **Idea #4 (transient cap of 1)** — Simplest fix with highest leverage: directly
   prevents accumulation without checking drive state or restructuring execution.

2. **Idea #1 (transient-aware creation gate)** — More principled: don't create what
   you can't send. Addresses root cause but requires planner to reason about drive
   availability, which is currently an executor concern.

3. **Idea #14 (pin file self-healing)** — Addresses the secondary damage from this
   incident and all future manual interventions. Independent of the space fix.

4. **Idea #6 (drive-availability preflight)** — Clean integration point: skip
   transient subvolumes entirely when drives aren't mounted. Pairs well with #4.

5. **Idea #10 (Sentinel drive-gated backup)** — Long-term ideal: transient backups
   triggered by drive presence, not schedules. Requires Sentinel to trigger backups,
   which is a larger architectural step.
