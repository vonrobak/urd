# ADR-100: Planner/Executor Separation

> **TL;DR:** The planner is a pure function that decides what operations to run. The executor
> takes a plan and runs it. Neither crosses the boundary. This is the most important
> architectural property in Urd — it enables full unit testing of backup logic without
> touching the filesystem, and prevents the "did it decide wrong or execute wrong?" ambiguity
> that plagued the bash script.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
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

- [Roadmap](../../96-project-supervisor/roadmap.md) §Architecture: Key Design Principles
- [Phase 1 journal](../../98-journals/2026-03-22-urd-phase01.md) — original implementation
- [Phase 2 journal](../../98-journals/2026-03-22-urd-phase02.md) — Executor Contract
- ADR-101: BtrfsOps trait (the executor's interface to btrfs)
