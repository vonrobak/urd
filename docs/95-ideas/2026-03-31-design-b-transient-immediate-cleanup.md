# Design: Transient Immediate Cleanup (Idea B)

> **TL;DR:** After a successful send to all configured drives, the executor deletes the
> previous pin parent as part of pin advancement. Transient subvolumes drop from two local
> snapshots to one immediately, instead of waiting for the next run's cleanup pass.

**Date:** 2026-03-31
**Status:** Reviewed
**Origin:** Idea B from [2026-03-30 brainstorm](2026-03-30-brainstorm-transient-workflow-and-redundancy-guidance.md), scored 9/10.
**Review:** [2026-03-31 design review](../99-reports/2026-03-31-design-b-review.md) -- approved with refinements.

## Problem

Transient snapshots linger locally until the next backup run. The sequence today:

```
Run N:   create snap-2  ->  send snap-2  ->  pin advances to snap-2  ->  delete snap-0
Run N+1: create snap-3  ->  send snap-3  ->  pin advances to snap-3  ->  delete snap-1
```

Between runs, *two* snapshots exist locally: the just-sent snapshot (new pin parent) and
the previous pin parent (now deletable, but the planner didn't know the send would succeed).
For htpc-root on a 118GB NVMe, even one extra root snapshot wastes meaningful space. Local
root snapshots have zero recovery value after send -- if the NVMe fails, you boot from
rescue and restore from external.

## Design

### Approach

Pin-and-delete in the executor's send-completion path. No new operation types. No planner
changes. The delete is a *consequence* of successful execution, not a planned operation.

> **ADR-100 note:** This is a timing optimization for an operation the planner would produce
> on the next run, not a new decision. The planner already plans transient deletes for all
> non-pinned snapshots. The executor is accelerating one such deletion into the current run
> because the send result proves the old parent is no longer needed. The executor does not
> make retention decisions -- it accelerates a deletion the planner has already endorsed by
> construction (transient mode deletes all non-pinned snapshots). This framing must not be
> generalized as a precedent for the executor making policy decisions.

### Sequence after this change

```
Run N:   create snap-2  ->  send snap-2 to all drives  ->  pin advances to snap-2
                                                        ->  delete snap-1 (old parent)
```

One local snapshot at all times: the current pin parent.

### Module changes

**executor.rs only.** The change lives in `execute_subvolume()`, after the operation loop
completes -- not inside `execute_send()`.

#### Information flow

1. During the operation loop, track per-subvolume state:
   - `old_pin_parents: HashMap<String, PathBuf>` -- for each drive, the pin parent *before*
     this run's send (read from pin file before the send writes the new one).
   - `sends_succeeded: HashSet<String>` -- drive labels where send succeeded.
   - `any_pin_failed: bool` -- already tracked as `pin_failures > 0`.

2. The planner already reads pin files to determine incremental parents. The parent path
   in `SendIncremental { parent, .. }` *is* the old pin parent. Capture it during the
   match arm. **Note:** `SendFull` operations do not carry an old parent. Full sends occur
   on chain reset (e.g., `ChainBroken` reason) -- the old parent was either missing locally
   or unreachable on the external drive. Immediate cleanup only applies to the incremental
   case. Any leftover unpinned snapshots after a full send are handled by the planner's
   normal transient cleanup in the same or next run.

3. Pass subvolume context to the executor using a `SubvolumeContext` struct rather than a
   bare `is_transient` boolean. The `execute()` method has access to the config and can
   build this struct when grouping operations by subvolume:

   ```rust
   struct SubvolumeContext {
       name: String,
       is_transient: bool,
       // Extensible: add fields as needed without changing signatures.
   }
   ```

   This replaces the `(subvol_name, ops)` tuple in `execute_subvolume()` with
   `(context, ops)`. The struct is constructed in `execute()` by looking up the subvolume's
   `local_retention.is_transient()` from the config.

4. After the operation loop, collect old pin parent paths. **Deduplicate by path** before
   attempting cleanup -- when multiple drives share the same old parent, the delete must
   only be attempted once.

5. After deduplication, if all conditions are met (see below), issue a delete for each
   unique old pin parent that is no longer pinned.

#### Trigger conditions (all must be true)

1. **Subvolume uses transient retention.** Checked via `context.is_transient`.

2. **All configured drives received a successful send this run.** Compare
   `sends_succeeded` against the set of drive labels that had planned sends for this
   subvolume. If drive A succeeded but drive B failed, do not delete -- the old parent
   may still be needed as an incremental base for drive B on the next run.

3. **No pin write failures.** If `pin_failures > 0`, the chain state is ambiguous. Do
   not delete anything.

4. **The old parent still exists on disk.** It may have already been deleted by a planned
   transient cleanup operation earlier in the same run.

5. **The old parent is not pinned by any drive.** Re-read pin files after all sends
   complete (they've been updated). If any pin still references the old parent, do not
   delete. This is the ADR-106 defense-in-depth re-check at the executor level.

#### Double-delete deduplication

The operation loop for a subvolume may contain both planned transient deletes (from the
planner) and the immediate post-send cleanup delete (from this new path). When the
immediate cleanup runs first and deletes the old parent, the planned delete for the same
snapshot must not fail noisily. Two layers handle this:

1. **Condition 4 above:** The immediate cleanup checks existence before deleting.
2. **Executor skip on planned deletes:** Before executing a planned `DeleteSnapshot`, the
   executor must check whether the target was already cleaned up by immediate post-send
   cleanup earlier in the same run. Track immediately-cleaned paths in a
   `already_cleaned: HashSet<PathBuf>` and skip planned deletes that target those paths.
   Log at debug level: "Skipping planned delete for {name}: already cleaned by
   post-send transient cleanup."

This prevents double-delete attempts and avoids relying on `btrfs subvolume delete`
returning a graceful error for non-existent subvolumes.

#### Collecting old pin parents

The `SendIncremental` variant already carries the `parent` path. When processing a
`SendIncremental` operation that succeeds, record `(drive_label -> parent path)` in the
tracking map. `SendFull` operations have no old parent to collect -- see the note in the
information flow section above.

This means the executor doesn't need to read pin files itself -- the planner already
resolved the parent, and the operation variant carries it.

#### The delete call

Use the existing `self.btrfs.delete_subvolume()` path, which is what `execute_delete()`
calls internally. Log at info level: "Transient cleanup: deleted old pin parent {name}".

Record the outcome in a `TransientCleanupOutcome` field on `SubvolumeResult` so the
cleanup is auditable in the same way as planned deletes.

### Failure handling

- **Delete fails:** Log warning, continue. The snapshot survives until the next run's
  normal transient cleanup. This is fail-open for the backup (it succeeded), fail-closed
  for the delete (snapshot preserved on error). Consistent with ADR-107.

- **Pin write fails:** No delete attempted. The `pin_failures > 0` guard prevents it.
  Next run will sort out the chain state.

- **Partial drive success:** No delete. The old parent is still needed as an incremental
  base for the drive that failed. Next run handles it.

### Crash safety

**Crash between pin advancement and cleanup delete is safe.** If the executor writes the
new pin file (advancing to snap-2) and then crashes before deleting the old parent
(snap-1), the next run recovers correctly:

1. The planner sees the new pin (snap-2), creates snap-3.
2. The planner plans transient deletes for everything not pinned.
3. snap-1 (the old parent from the crashed run) is not pinned by any drive.
4. The planner generates a `DeleteSnapshot` for snap-1.

This is graceful degradation to the current behavior -- snap-1 survives one extra run,
which is exactly what happens today without this feature. No data loss, no correctness
issue. The existing transient cleanup in the planner serves as the fallback.

### TOCTOU note

Safety of the pin re-read (condition 5) relies on the advisory lock (`lock.rs`) preventing
concurrent backup runs. The TOCTOU window between pin re-read and delete call is not
independently defended -- it is microseconds wide and the lock makes concurrent mutation
impossible in practice. If Urd ever moves to concurrent subvolume processing, this
assumption must be revisited.

## Multi-drive safety

This is the critical correctness constraint.

A subvolume might send to multiple drives (e.g., WD-18TB and WD-18TB1). Each drive has
its own pin file and its own incremental chain. The old pin parent for drive A might
differ from drive B if they were last sent at different times.

**Rule:** Only delete an old parent snapshot if *no* pin file references it after all
sends complete.

**Example:**
- Before run: pin-WD-18TB = snap-1, pin-WD-18TB1 = snap-1
- Send snap-2 to WD-18TB: succeeds, pin-WD-18TB advances to snap-2
- Send snap-2 to WD-18TB1: fails
- After run: pin-WD-18TB1 still = snap-1
- Result: snap-1 is still pinned. Do NOT delete.

**Example 2:**
- Before run: pin-WD-18TB = snap-1, pin-WD-18TB1 = snap-0
- Send snap-2 to both: both succeed
- After run: both pins = snap-2
- Old parents: snap-1 (from WD-18TB), snap-0 (from WD-18TB1)
- Neither is pinned. Delete both (after deduplicating by path).

The re-read of pin files after all sends is the safety mechanism. It costs a few
filesystem reads and provides a definitive answer.

## What this does NOT change

- **Planner:** No changes. Planned transient deletes still run as before for snapshots
  that predate the old pin parent.
- **Retention:** No changes. Still computes the same keep/delete lists.
- **Types:** No new operation variants.
- **Chain module:** No changes to pin file format or semantics.
- **Awareness:** No changes to promise state computation.
- **Voice/output:** No changes (the delete outcome gets recorded in the same
  `SubvolumeResult` as all other operations, plus the new `TransientCleanupOutcome`).

## Test strategy

1. **Unit: transient cleanup fires after all drives succeed.** Mock two drives, both
   sends succeed, verify old parent deleted.

2. **Unit: no cleanup when one drive fails.** Mock two drives, one send fails, verify
   old parent preserved.

3. **Unit: no cleanup when pin write fails.** Send succeeds but pin write fails, verify
   no delete attempted.

4. **Unit: no cleanup for non-transient subvolumes.** Same send pattern, graduated
   retention -- verify no post-send delete.

5. **Unit: divergent pin parents.** Two drives with different old parents, both sends
   succeed, verify both old parents deleted and neither current pin is touched.

6. **Unit: old parent already deleted.** Planned transient cleanup already deleted the
   old parent earlier in the run. Verify no error when post-send cleanup finds it gone.

7. **Unit: full send has no old parent to delete.** `SendFull` operation, verify no
   cleanup attempted (no incremental parent means no old pin parent).

8. **Unit: planned delete skipped when already cleaned.** Immediate cleanup deletes old
   parent, then a planned `DeleteSnapshot` for the same path is skipped via the
   `already_cleaned` set. Verify the planned delete is not attempted.

9. **Unit: crash recovery.** Simulate pin advancement without cleanup delete (executor
   crashes). Verify the next run's planner generates a `DeleteSnapshot` for the old parent.

## Alternatives rejected

**A1: Executor generates post-send delete operations dynamically.** Breaks the
planner/executor contract by having the executor create operations the planner didn't
plan. The pin-and-delete approach is simpler: it's a side effect of pin advancement,
not a new operation.

**A2: Conditional delete operations in the plan.** Adds a new `PlannedOperation` variant
with a condition field. Overengineered for one behavioral change. The condition
("did the send succeed?") is only knowable at execution time, making it awkward in
a plan data structure.

**A3: New `"transient-immediate"` retention mode.** Config-level distinction for what is
purely an executor timing optimization. The user shouldn't need to choose between
"transient that cleans up slowly" and "transient that cleans up fast." Fast is strictly
better.

**C: Ephemeral mode (delete everything, full sends only).** Trades bandwidth for disk
space. Appropriate for a different use case (very slow-changing subvolumes where
incrementality doesn't matter). Not a replacement for this change.

## Review findings incorporated

Changes made to address findings from the [arch-adversary review](../99-reports/2026-03-31-design-b-review.md):

1. **ADR-100 framing (Finding 1).** Added explicit note in the Approach section that this
   is a timing optimization, not a policy decision. Includes the recommended framing
   language and the `TransientCleanupOutcome` addition for auditability.

2. **TOCTOU documentation (Finding 2).** Added a TOCTOU note section documenting the
   advisory lock assumption and the conditions under which this would need revisiting.

3. **Crash safety (Finding 3).** Added a Crash safety section documenting the graceful
   degradation property -- crash between pin advancement and delete is safe because the
   next run's planner handles it.

4. **SubvolumeContext struct (Finding 4).** Updated information flow to use a
   `SubvolumeContext` struct instead of a bare `is_transient` bool. Includes struct
   definition and construction point.

5. **SendFull clarification (Finding 5).** Added note in the information flow section
   that full sends do not carry an old parent, so immediate cleanup only applies to the
   incremental case. Leftover snapshots after full sends are handled by planned transient
   cleanup.

6. **Double-delete deduplication (Finding 6).** Added a deduplication section: old parent
   paths are deduplicated before cleanup attempts, and the executor skips planned transient
   deletes for snapshots already cleaned up by immediate post-send cleanup via an
   `already_cleaned` set. Added test case 8 for this behavior.

7. **Crash recovery test (review recommendation).** Added test case 9 covering the crash
   recovery scenario.

## Ready for review

**Focus areas for arch-adversary:**

1. **ADR-100 compliance.** Is a post-send delete in the executor a violation of
   planner/executor separation, or is it legitimately a consequence of execution (like
   pin file writes)?

2. **ADR-106 defense-in-depth.** The re-read of pin files after sends is the safety
   layer. Is it sufficient? Are there race conditions (another process modifying pins)?

3. **Multi-drive edge cases.** The "all drives must succeed" rule plus the pin re-read
   should prevent premature deletion. Are there scenarios where this fails?

4. **Information flow.** The executor needs `is_transient` per subvolume. Currently
   `execute_subvolume()` only receives `(subvol_name, ops)`. What's the cleanest way
   to pass retention mode through?
