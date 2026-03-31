# Design Review: Transient Immediate Cleanup (Design B)

**Reviewed:** `docs/95-ideas/2026-03-31-design-b-transient-immediate-cleanup.md`
**Date:** 2026-03-31
**Reviewer:** arch-adversary
**Mode:** Design review (4 dimensions)

---

## Scores

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Sound core logic; two edge cases need attention |
| Security | 9/10 | Conservative multi-drive safety; minor TOCTOU surface |
| Architectural Excellence | 7/10 | ADR-100 tension is real but manageable with framing |
| Systems Design | 8/10 | Good failure mode analysis; crash window identified |

**Overall: 8/10** -- Well-reasoned design with strong safety properties. The findings below are refinements, not blockers.

---

## Catastrophic Failure Checklist

| # | Risk | Assessment |
|---|------|------------|
| 1 | Silent data loss | **Low.** Five-condition guard prevents premature deletion. Re-read of pin files is a strong final gate. |
| 2 | Path traversal to wrong subvolume | **None.** Delete target comes from `SendIncremental.parent`, which the planner resolved from pin files. No user-controlled path construction in the new code path. |
| 3 | Pinned snapshot deletion | **Low.** Condition 5 (re-read pins) is the ADR-106 defense-in-depth layer. See Finding 2 for a narrow TOCTOU surface. |
| 4 | Space exhaustion causing backup failure | **Positive.** This design reduces the risk by reclaiming space faster. |
| 5 | Config change silently orphaning snapshots | **None.** Cleanup is tied to pin file state, not config. Changing retention mode between runs does not affect pin files. |
| 6 | TOCTOU between privilege boundaries | **Narrow.** See Finding 2. |

---

## Findings

### Finding 1: ADR-100 tension is real, not a false alarm (Severity: Medium -- Architectural)

**The concern.** The design says "no planner changes" and frames the delete as a "consequence of execution, like pin file writes." This comparison is not quite right. Pin file writes record the outcome of an operation (a bookkeeping side effect). Deleting a snapshot is a destructive filesystem mutation -- it is an operation in its own right, and the planner is supposed to be the sole authority on what operations run.

**Why it matters.** If the executor can decide to delete snapshots based on runtime state, you have opened a precedent. Today it is transient cleanup. Tomorrow it could be "the executor noticed the drive is full, so it deleted an old snapshot to make room." The line between "consequence of execution" and "executor making decisions" gets blurry.

**Why it is still acceptable.** The delete is strictly narrower than what the planner would have planned on the next run anyway. It is a timing optimization, not a policy decision. The planner already plans transient deletes; the executor is just doing one of them sooner. The conditions (all drives succeeded, no pin failures, not currently pinned) are equivalent to what the planner would check.

**Recommendation.** Acknowledge this in an implementation comment: "This delete is a timing optimization for an operation the planner would produce on the next run. The executor does not make retention decisions -- it accelerates a deletion the planner has already endorsed by construction (transient mode deletes all non-pinned snapshots)." This framing prevents the precedent from being generalized.

Also consider: have `execute_subvolume` return a `TransientCleanupOutcome` in `SubvolumeResult` so the caller can see what happened. This keeps the delete auditable in the same way planned deletes are.

### Finding 2: TOCTOU gap between pin re-read and delete (Severity: Low)

**The scenario.** Between the moment the executor re-reads pin files (condition 5) and the moment it calls `delete_subvolume()`, another process could write a new pin file referencing the snapshot about to be deleted. In practice, Urd uses an advisory lock (`lock.rs`) so concurrent runs are prevented. The Sentinel daemon does not run backups concurrently.

**However:** If a user manually runs `urd backup` in one terminal while a timer-triggered run is in the post-send cleanup phase, and the lock is shared (not exclusive), this gap could matter. The window is very small (microseconds between re-read and delete call), and the advisory lock should prevent this in practice.

**Recommendation.** This is acceptable for now. Document the assumption: "Safety relies on the advisory lock preventing concurrent backup runs. The TOCTOU window between pin re-read and delete is not independently defended." If Urd ever moves to concurrent subvolume processing, this assumption must be revisited.

### Finding 3: Crash between pin advancement and cleanup delete (Severity: Medium -- Correctness)

**The scenario.** The executor successfully sends, writes the new pin file, then crashes (power loss, OOM kill, SIGKILL) before the cleanup delete executes. On the next run:

1. The planner sees the new pin (snap-2), creates snap-3.
2. The planner plans transient deletes for everything not pinned.
3. snap-1 (the old parent) is not pinned by any drive.
4. The planner generates a `DeleteSnapshot` for snap-1.

**Assessment.** This is handled correctly. The existing transient cleanup in the planner serves as the fallback. The only cost is that snap-1 survives one extra run, which is the current behavior anyway. No data loss, no correctness issue. The design doc should note this explicitly as a "graceful degradation" property.

### Finding 4: Information flow -- passing `is_transient` (Severity: Low -- Design)

The design identifies that `execute_subvolume()` currently receives `(subvol_name, ops)` and needs `is_transient`. The design proposes adding a bool parameter.

**Recommendation.** Instead of a bare `bool`, pass a small struct or add the information to the operation grouping. The executor already groups operations by subvolume name (line 163). Consider either:

(a) A `SubvolumeContext { name: String, is_transient: bool }` passed alongside ops, or
(b) Derive `is_transient` from the presence of transient-reason deletes in the ops list (`ops.iter().any(|op| matches!(op, PlannedOperation::DeleteSnapshot { reason, .. } if reason.starts_with("transient:")))`)

Option (b) is clever but fragile -- it couples to the reason string. Option (a) is cleaner and more explicit. The `execute()` method already has access to the config; it can look up the subvolume's retention mode when building the groups.

### Finding 5: `SendFull` with old parent (Severity: Low -- Correctness)

The design correctly identifies that `SendFull` has no old parent to delete. But there is a subtle case: a `SendFull` with `reason: ChainBroken` means a pin file existed but the parent was missing. The old pin still references a snapshot name. After the full send succeeds and the pin advances, that old pin parent snapshot might still exist locally (it was just unreachable on the external drive, not locally deleted).

**Scenario:**
1. pin-WD-18TB = snap-1 (exists locally, missing on external drive)
2. Planner generates `SendFull` for snap-2 (chain broken)
3. Send succeeds, pin advances to snap-2
4. snap-1 is now unpinned and exists locally

The current planner would generate a transient delete for snap-1 in the same run (it is not pinned). So this is already handled. But the post-send cleanup in the executor would not catch it because `SendFull` does not carry a `parent` field. This is fine -- the planned delete handles it. Just worth noting that immediate cleanup only applies to the incremental case.

### Finding 6: Double-delete race with planned transient cleanup (Severity: Low)

The design's condition 4 ("old parent still exists on disk") handles the case where a planned `DeleteSnapshot` already removed the old parent earlier in the operation loop. Good.

**Edge case to verify:** The operation ordering within a subvolume is create -> send -> delete (by convention from the planner). If sends are ordered before deletes, the post-send cleanup would run first, then the planned delete would try to delete an already-gone snapshot. The planned delete path in `execute_delete` should handle `NotFound` gracefully.

**Verification from source:** `execute_delete` (not shown in full but implied by the delete call at line 296) should already handle missing snapshots since btrfs delete of a non-existent subvolume returns an error that the executor logs and continues from. Confirm this during implementation.

---

## Multi-Drive Safety Assessment

The multi-drive logic is sound. The two-layer defense is:

1. **"All configured drives must succeed"** -- prevents deleting the old parent when it is still needed as an incremental base for a failed drive.
2. **Pin re-read after all sends** -- catches any case where the above rule is insufficient (e.g., divergent pin parents across drives).

**Verified edge cases from the design doc:**
- Partial drive success: correctly preserves old parent. The `sends_succeeded` check catches this.
- Divergent pin parents: correctly deletes both old parents only when both are unpinned. The re-read catches this.
- Same old parent for multiple drives: delete is attempted once (deduplicate by path). The "still exists on disk" check makes a second attempt a no-op.

**One edge case not covered in the design:** Three drives where two succeed and one fails, but the two successful drives had different old parents. The "all drives must succeed" rule blocks cleanup entirely, which is correct but conservative. The old parent for the two successful drives could theoretically be cleaned up if it is unpinned. However, the conservative approach is right -- it avoids complexity and the snapshot survives one more run at most.

---

## Recommendations Summary

| # | Action | Priority |
|---|--------|----------|
| 1 | Add implementation comment framing the delete as a timing optimization, not a policy decision. Add `TransientCleanupOutcome` to `SubvolumeResult` for auditability. | Must |
| 2 | Document the advisory lock assumption for TOCTOU safety. | Should |
| 3 | Note crash-between-pin-and-delete as a graceful degradation property in the design doc. | Should |
| 4 | Use `SubvolumeContext` struct rather than bare `is_transient: bool` for the information flow. | Should |
| 5 | Verify `execute_delete` handles already-deleted snapshots gracefully (double-delete from planned + immediate cleanup). | Must |
| 6 | Deduplicate old parent paths before attempting cleanup (when multiple drives share the same old parent). | Must |

---

## Verdict

**Approved with refinements.** The design is well-reasoned and the safety properties are strong. The ADR-100 tension is real but acceptable when properly framed as a timing optimization. The multi-drive safety logic is correct. The main implementation risks are the double-delete race (Finding 6, easily handled) and the information flow design (Finding 4, straightforward).

The test strategy covers all the important cases. Add one more: **crash recovery test** -- verify that if the executor crashes after pin advancement but before cleanup delete, the next run's planner correctly generates the delete.
