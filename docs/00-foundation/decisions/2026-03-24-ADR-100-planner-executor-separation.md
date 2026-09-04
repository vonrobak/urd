---
type: ADR
title: Planner/Executor Separation
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-04T15:03:12+02:00'
---
# ADR-100: Planner/Executor Separation

> **TL;DR:** The planner is a pure function that decides what operations to run. The executor
> takes a plan and runs it. Neither crosses the boundary. This is the most important
> architectural property in Urd — it enables full unit testing of backup logic without
> touching the filesystem, and prevents the "did it decide wrong or execute wrong?" ambiguity
> that plagued the bash script.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-planner-surface-and-the-post-plan-stamp))
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
