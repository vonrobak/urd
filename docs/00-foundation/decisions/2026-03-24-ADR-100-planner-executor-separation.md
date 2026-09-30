---
type: ADR
title: Planner/Executor Separation
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-100: Planner/Executor Separation

> **TL;DR:** The planner is a pure function that decides what operations to run. The executor
> takes a plan and runs it. Neither crosses the boundary. This is the most important
> architectural property in Urd — it enables full unit testing of backup logic without
> touching the filesystem, and prevents the "did it decide wrong or execute wrong?" ambiguity
> that plagued the bash script.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-planner-surface-and-the-post-plan-stamp); amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-the-pure-planner-sanctioned-executor-paths-and-the-lock))
**Supersedes:** None (founding decision from project inception)

## Context

The bash script (`btrfs-snapshot-backup.sh`, 1710 lines) interleaved decision logic with
execution throughout. When a backup failed, diagnosing whether the *decision* was wrong
(e.g., wrong parent selected for incremental send) or the *execution* was wrong (e.g.,
btrfs command failed) required reading through tangled control flow. Testing required a
real filesystem with real btrfs commands.

Urd was designed from inception to separate these concerns completely.

## Decision

**The planner (`plan.rs`) is a pure function:**
`fn plan(config, filesystem_state, now, filters) -> BackupPlan`

- It reads config and filesystem state through the `FileSystemState` trait
- It produces a list of `PlannedOperation` variants (CreateSnapshot, SendIncremental,
  SendFull, DeleteSnapshot)
- It never calls btrfs, writes files, modifies state, or performs I/O
- It is fully unit-testable via `MockFileSystemState`

**The executor (`executor.rs`) takes a plan and runs it:**

- It executes each operation sequentially in plan order
- It never decides *what* to do — that is the planner's job
- It handles error isolation, cascading failure detection, crash recovery, and cleanup
- It writes pin files, records state in SQLite, and calls btrfs via `BtrfsOps`

**`urd plan` prints the plan. `urd backup --dry-run` prints it. `urd backup` executes it.**

Every variant in `PlannedOperation` carries `subvolume_name` so operations are
self-describing. Send variants carry `pin_on_success: Option<(PathBuf, SnapshotName)>` so
the send/pin dependency is structural, not implicit.

## Consequences

### Positive

- Backup logic is fully testable without a filesystem, sudo, or btrfs commands
- `urd plan` gives the user a preview of exactly what `urd backup` will do
- Bug diagnosis is unambiguous: plan bugs are in `plan.rs`, execution bugs are in
  `executor.rs`
- The `MockFileSystemState` + `MockBtrfs` combination enables comprehensive test coverage
  (216 tests at time of writing, none requiring real btrfs)

### Negative

- The planner cannot know exact snapshot sizes, so the executor must re-check space during
  external retention deletions (the planner proposes, the executor verifies)
- Some operations have implicit ordering dependencies (create before send, send before
  delete) that exist only in the plan emission order, not in a formal dependency graph

### Constraints

- New backup logic must go through the planner. No module may bypass the plan to execute
  btrfs operations directly.
- The `FileSystemState` trait is the planner's only window into the real world. Extending
  the planner's awareness requires extending this trait.
- The executor must not contain decision logic beyond error-handling decisions (skip
  dependent operations after failure, stop deletions when space is sufficient).

## Related

- Roadmap (`docs/96-project-supervisor/roadmap.md`) §Architecture: Key Design Principles
- Phase 1 journal (`docs/98-journals/2026-03-22-urd-phase01.md`) — original implementation
- Phase 2 journal (`docs/98-journals/2026-03-22-urd-phase02.md`) — Executor Contract
- ADR-101: BtrfsOps trait (the executor's interface to btrfs)

## Amendment 2026-09-04: planner surface and the post-plan stamp

The separation itself is unchanged. Three names in the Decision section have moved, and
one narrow post-`plan()` mutation is load-bearing enough to state as a rule rather than
leave to a code comment.

### Renamed surfaces

- **`plan.rs` → `plan/`.** The planner is a directory module: `src/plan/mod.rs` plus the
  `local`, `external`, `send`, and `transient` regions and the `fragment` accumulator
  every region pushes operations, deferrals, and events into. `plan::plan()` is still
  the single entry point; the regions are internal decomposition, not additional
  decision surfaces.
- **`FileSystemState` → `Observation`.** The planner's window into the world is
  `Observation` (`src/observation.rs`), which bundles two traits split along the ADR-102
  axis — `FilesystemQuery` (snapshot directories, pin files, mounts, free and total
  bytes) and `HistoryQuery` (send sizes, calibration, send/drive timestamps) — alongside
  a read-only `BtrfsRead` handle for generation counters (ADR-101). Extending the
  planner's awareness still means extending a trait on this boundary, never adding an
  I/O call inside the planner.
- **Signature.** `plan(config, now, filters, obs: &Observation, arming: &RunArming)`.
  `MockFileSystemState` remains the test double behind `Observation`.

### `RunArming`: the ADR-113 Layer-1 input

`RunArming` (`src/commands/storage_signals.rs`) is the storage posture the planner plans
against: the per-subvolume armed tier (`armed_tier_map`), the resolved per-pool tiers,
and the away-sheddable pin view (`away_shed`). It is resolved **once per run, pre-lock**
by `RunArming::resolve(&signals, &config, &fs_state)` — in `commands/backup.rs` and in
`commands/plan_cmd.rs`, so the preview and the run plan against the same posture — and is
read back everywhere else: the planner, the emergency re-plan, the executor (through the
plan's `lifecycles`), the post-execution assessment, and the armed-tier writeback.

Re-resolving mid-run is a bug, not an optimization. A clear-all frees space during the
run, so a second gather would see a higher free ratio and de-escalate the tier the plan
was built against — desyncing the effective send interval the planner timed against from
the one awareness judges staleness against, and surfacing a correctly-adapting subvolume
as a false AT RISK.

This does not weaken purity: `RunArming` is data, resolved by the impure command layer
and handed to the planner as an argument like `config` and `now`. `RunArming::default()`
is the all-`Roomy` identity — declared behavior, byte-identical plans — which is what
hand-built test plans pass.

### Post-plan stamps: what the command layer may still touch

`plan()` returning is not quite the end of plan construction. Between `plan()` and
execution, `commands/backup.rs` mutates the plan twice, and both mutations are bounded.

**Removal is allowed.** `apply_token_gating` drops `SendFull` / `SendIncremental`
operations targeting drives whose identity token failed (retention `Delete*` operations
are deliberately left alone — a clone's snapshots are redundant copies, and blocking
deletes would cause space exhaustion for no safety gain), and `filter_promise_retention`
drops retention deletions for promise-level subvolumes absent
`--confirm-retention-change` (ADR-107 fail-closed). Both strictly shrink the plan;
neither invents work the planner did not authorize.

**Exactly one field may be written: `token_verified`.** The planner emits
`PlannedOperation::SendFull { token_verified: false, .. }` (`src/plan/send.rs`) because
drive-token verification is an I/O question it cannot answer. `apply_token_gating` sets
it to `true` for drives whose token file exists and matches, so the executor's
chain-break gate may proceed on a known-good drive.

The rule: **`token_verified` may only widen permission — `false` → `true`, never the
reverse.** A stamp that could narrow permission would be the command layer deciding
*what to do*, which is the planner's job; a stamp that can only widen leaves the plan, at
worst, at the planner's own conservative default. No other field of any
`PlannedOperation` may be set after `plan()`. A second such stamp would need its own
amendment here and the same widening-only argument.

## Amendment 2026-09-30: the pure planner, sanctioned executor paths, and the lock

The separation is unchanged. This amendment brings three names from the 2026-09-04
amendment up to date, replaces the second post-plan removal it named, and states two
facts the original Constraints section left false: which executor paths run without a
plan, and when the run lock is taken.

### Planner surface

- **Planner output types** live in `src/plan/types.rs` and are re-exported from `plan`:
  `BackupPlan`, `PlannedOperation`, `PlannedSkip`,
  `PlannedLifecycle`, `DeleteKind`. A skip carries a typed `SkipReason`, one variant per
  reason shape with its data (the unmounted drive, the caught-up drive). Its `Display` is
  the reason prose and is an ADR-105 contract (`urd plan --json`'s `skipped[].reason`);
  consumers classify with a total `match`, and nothing parses the prose back.
- **`Observation`** lives in `src/observation/` (`mod.rs` holds the `FilesystemQuery` and
  `HistoryQuery` traits and the bundle). The production adapter, `RealFileSystemState`,
  is `observation/real.rs` — the I/O half of the read boundary, constructed by the
  command layer, the sentinel runner, and the executor, and no longer part of `plan/`.
  The send-size estimator the planner's space gate uses is `observation/estimate.rs`,
  pure over `HistoryQuery`.
- **`RunArming`** lives in `src/arming.rs`, not `commands/storage_signals.rs`. It is
  resolved by a pure function the command layer calls:
  `RunArming::resolve(&signals.pools, &config, &fs_state)` in `commands/backup/mod.rs`
  and `commands/plan_cmd.rs`. The command layer gathers the `PoolSignal`s
  (`storage_signals::gather`: `findmnt`, `statvfs`, SQLite); `arming.rs` only fans the
  resolved tiers out and composes the away-shed pin view. The once-per-run,
  never-re-resolve rule is unchanged.
- **`scripts/check-purity-boundary.sh`** now holds `plan/` (with the other pure modules,
  ADR-108) to the no-I/O, no-wall-clock rule in CI, so the Decision's "it never calls
  btrfs, writes files, … or performs I/O" is checked mechanically rather than by review.

### Post-plan stamps today

The removal `filter_promise_retention` performed is gone. It dropped every retention
deletion for every promise-level subvolume on any run without
`--confirm-retention-change`, whether or not retention had changed. Its replacement is
the retention-change gate (ADR-110's amendment of this date): `retention::decide_retention_gate`
decides, from the recorded retention shapes, which subvolumes' retention *tightened*;
`retention::apply_retention_gate` removes, for those subvolumes only, the retention
deletions the recorded (previous) retention would not have made.
Both are pure. The removal rule from the 2026-09-04 amendment still holds: the gate only
shrinks the plan.

The command layer's post-plan mutations are now:

| Mutation | Where | Kind |
|---|---|---|
| `apply_retention_gate` | `commands/backup/mod.rs`, re-applied to the emergency re-plan | Removal |
| `apply_token_gating` | `commands/backup/gating.rs`, under the lock | Removal, plus the `token_verified` widening stamp |

**Preview parity.** `urd plan` and `urd backup --dry-run` apply the same retention-gate
decision to the plan they print (`plan_cmd::gate_preview`, read-only: a preview never
records a shape), so "`urd plan` gives the user a preview of exactly what `urd backup`
will do" holds again for retention. Token gating is reported, not applied, in a preview:
`plan_cmd::populate_token_warnings` names a drive whose identity token is missing or
mismatched and says its sends are blocked, while the operation list still shows those
sends. The token probe reads the drive and the state DB, and `apply_token_gating` runs
after the lock is taken, which a preview never takes.

### Executor paths that run without a plan

"No module may bypass the plan to execute btrfs operations directly" is narrowed to
this: **every btrfs mutation outside a plan goes through a named `Executor` method that
re-checks pins at delete time**, except one interactive path.

- **`Executor::emergency_reclaim_pool`** (`src/executor/reclaim.rs`) — the pool reclaim
  after a mid-send watchdog abort (ADR-113 Layer 2, `commands/backup/mod.rs` and
  `commands/backup/watchdog.rs`) and after an idle eject (Layer 3,
  `sentinel_runner/eject.rs`). It sheds pins by design, so its re-check is the
  fail-closed ordering in `shed_and_delete_unpinned`: strict pin read, never-the-only-copy
  gate, drop the chosen pins, strict re-read, delete only what is now unpinned.
- **`Executor::delete_candidates`** (`src/executor/reclaim.rs`) — the single deletion
  loop behind the backup's emergency pre-flight (`commands/backup/preflight.rs`) and
  `urd emergency` (`commands/emergency.rs`). Both choose candidates through
  `commands::emergency::emergency_walk`; the loop runs the ADR-106 Layer-3 check
  (`chain::is_pinned_at_delete_time`) immediately before each delete.
- **`commands/init.rs`'s incomplete-snapshot cleanup** (`handle_incomplete_deletions`)
  still calls `BtrfsOps::delete_subvolume` directly. It offers each drive's newest
  destination snapshot that the pin does not name as a possible partial, and deletes it
  only after an explicit per-snapshot `y` at the terminal. It is the one remaining direct
  call: the operator is the decider, and the delete is on the destination, where the
  local pin re-check does not apply. Routing it through the executor with ADR-107's
  `Received UUID` proof is the open item.

These paths exist because their trigger is not "time for a backup": host survival
(ADR-113) and explicit operator action. They make no decision the planner could have
made at plan time.

### The run lock

`urd backup` takes an **exclusive**, non-blocking `flock` (`lock::acquire_lock`) on
`<state_db>.lock` **after** planning, arming, and the retention-gate decision, and before
the emergency pre-flight, the re-plan, token gating, and execution
(`commands/backup/mod.rs`). Everything before the lock is reads; everything after it can
delete. A held lock fails the run and names the holder from the JSON metadata written
after acquisition (PID, start time, trigger). `--dry-run` returns before the lock.

The sentinel's idle eject uses `lock::try_acquire_lock` with trigger `"sentinel-eject"`
(`sentinel_runner/eject.rs`): a held lock means a backup is running, and the eject
defers to that run's own watchdog rather than waiting or failing. `acquire_lock`'s branch
for a holder whose trigger is `"sentinel"` is unreachable, because nothing writes that
trigger since the sentinel stopped triggering backups (ADR-110's amendment of this date).

### Numbers

"216 tests at time of writing" in Consequences was a snapshot and is retired. The claim
that stands is qualitative: the planner, the executor (through `MockBtrfs`), and every
pure module are tested without root, btrfs, or a real pool.
