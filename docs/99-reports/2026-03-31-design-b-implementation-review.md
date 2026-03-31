# Implementation Review: Transient Immediate Cleanup (Feature 6-B)

**Reviewed:** Implementation diff (branch vs master) for feature 6-B
**Source files:** `src/executor.rs`, `src/commands/backup.rs`, `src/config.rs`, `src/heartbeat.rs`
**Design:** `docs/95-ideas/2026-03-31-design-b-transient-immediate-cleanup.md`
**Date:** 2026-03-31
**Reviewer:** arch-adversary
**Mode:** Implementation review (6 dimensions)

---

## Executive Summary

The implementation is well-structured and faithful to the design. The five-condition safety
gate is sound in concept, the `TransientCleanupOutcome` enum provides full auditability,
and the `SubvolumeContext` struct is clean. However, there is one finding that approaches
data loss territory: the pin safety check (condition 5) fails open on parse errors due to
a `let`-chain short-circuit, which inverts the ADR-107 principle for deletions. Two
additional findings affect correctness under multi-parent failure scenarios and design
compliance. The rest are minor improvements.

---

## Catastrophic Failure Checklist

| # | Risk | Assessment |
|---|------|------------|
| 1 | Silent data loss from deleting a pinned snapshot | **Medium.** Finding 1: parse failure in pin check causes the delete to proceed unchecked. Requires unusual path corruption to trigger, but violates fail-closed for deletions. |
| 2 | Premature deletion needed as incremental base | **Low.** The "all drives must succeed" gate and pin re-read are strong. No path to this failure found. |
| 3 | Path traversal to wrong subvolume | **None.** Delete targets come from `SendIncremental.parent`, resolved by planner from pin files. |
| 4 | Double-delete causing noisy failure | **Low.** Analysis shows planned transient deletes and immediate cleanup target disjoint snapshot sets (see Finding 5). Not a real risk. |
| 5 | Crash between pin advance and cleanup delete | **None.** Graceful degradation is real — next run's planner handles it. Verified by code tracing. |
| 6 | Config change creating stale `is_transient` | **None.** Context is built from live config at execution time. |

---

## Scores

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 7/10 | Pin check fails open on parse error (Finding 1); early return on first delete failure loses subsequent parents (Finding 3) |
| Security | 8/10 | No new privilege surface; TOCTOU documented and acceptable |
| Architectural compliance | 8/10 | ADR-100 framing is explicit and well-bounded; ADR-107 violated in pin check path |
| Robustness | 8/10 | Crash safety verified; failure modes degrade gracefully except Finding 3 |
| Test coverage | 7/10 | 8 tests cover the main paths; two important gaps (Findings 6, 7) |
| Simplicity | 9/10 | Clean implementation; `SubvolumeContext`, outcome enum, and doc comments are well-designed |

**Overall: 7.5/10** — Strong implementation with one finding that needs fixing before merge.

---

## Findings

### Finding 1: Pin safety check fails open on parse error (Severity: HIGH — Correctness/Safety)

**Location:** `executor.rs`, `attempt_transient_cleanup()`, lines 862-874.

**The problem.** The pin check uses a `let`-chain:

```rust
if let Some(snap_name_osstr) = parent_path.file_name()
    && let Ok(snap) = SnapshotName::parse(&snap_name_osstr.to_string_lossy())
    && current_pinned.contains(&snap)
{
    continue; // skip delete — still pinned
}
// Falls through to delete
```

If `file_name()` returns `None` or `SnapshotName::parse()` fails, the entire `if`
short-circuits and execution falls through to the delete call. This means: **if the
snapshot name cannot be parsed, the pin protection check is skipped entirely and the
delete proceeds.**

**Why it matters.** ADR-107 says deletions fail closed. The equivalent check in
`execute_delete()` (line 668-693) has the same structural pattern but is less dangerous
because the planner already validated those paths. Here, the path comes from
`SendIncremental.parent` which the planner also validated — so this requires an unusual
corruption scenario. But defense-in-depth means each layer must be independently sound.

**Fix.** Invert the logic: default to skipping the delete, and only proceed if the parse
succeeds AND the snapshot is NOT pinned:

```rust
let is_safe_to_delete = parent_path.file_name()
    .and_then(|name| SnapshotName::parse(&name.to_string_lossy()).ok())
    .map(|snap| !current_pinned.contains(&snap))
    .unwrap_or(false); // Can't verify → don't delete

if !is_safe_to_delete {
    log::warn!(
        "Transient cleanup: refusing to delete {} (still pinned or unparseable)",
        parent_path.display(),
    );
    continue;
}
```

**Action: Must fix.**

---

### Finding 2: The `resolved()` bypass is safe today but fragile (Severity: MEDIUM — Architectural)

**Location:** `executor.rs`, `execute()`, lines 193-203.

**The code.** The `is_transient` check reads the raw config field:

```rust
matches!(sv.local_retention, Some(crate::types::LocalRetentionConfig::Transient))
```

The design review asked about `sv.resolved().local_retention.is_transient()` vs this raw
field access. The `/simplify` pass chose the raw field.

**Analysis.** Today this is safe because:
1. `DerivedPolicy.local_retention` is always `ResolvedGraduatedRetention` (never transient).
   Named protection levels cannot derive transient retention.
2. A subvolume is transient only if its raw `local_retention` field is explicitly
   `Some(Transient)`. The `resolved()` path preserves this (line 163-165 in `config.rs`).

**However:** If a future named protection level ever derives transient retention (e.g., a
hypothetical "ephemeral" level), the raw field check would miss it because
`sv.local_retention` would be `None` (relying on the derived policy). The `resolved()`
path would catch it.

**Recommendation.** Add a comment documenting why the raw field access is used and under
what assumption it is equivalent to the resolved path:

```rust
// Raw field check: named protection levels never derive transient retention.
// If this changes, switch to sv.resolved(&config.defaults, freq).local_retention.is_transient().
```

**Action: Should fix (comment only).**

---

### Finding 3: Early return on first delete failure abandons remaining parents (Severity: MEDIUM — Correctness)

**Location:** `executor.rs`, `attempt_transient_cleanup()`, line 892.

**The problem.** When deleting multiple old parents (divergent pin parents from multiple
drives), if the first delete fails, the method returns `DeleteFailed` immediately without
attempting the remaining parents:

```rust
Err(e) => {
    return TransientCleanupOutcome::DeleteFailed { path, error };
}
```

With two divergent parents (snap-0 from DRIVE-A, snap-1 from DRIVE-B), if deleting snap-0
fails, snap-1 is never attempted. Both survive until the next run, but snap-1's survival
is unnecessary.

**Why it matters.** This is consistent with the executor's general pattern of continuing
past errors (ADR-100, invariant 4: "individual subvolume failures never abort the run").
The transient cleanup should follow the same philosophy: attempt all deletes, report
failures.

**Fix.** Continue through all parents, track failures, and report a summary outcome:

```rust
let mut failed: Option<(String, String)> = None;
for parent_path in existing_parents {
    // ... pin check ...
    match self.btrfs.delete_subvolume(parent_path) {
        Ok(()) => { deleted_count += 1; }
        Err(e) => {
            log::warn!("...");
            if failed.is_none() {
                failed = Some((path_str, e.to_string()));
            }
        }
    }
}
// Return Cleaned if any succeeded, DeleteFailed if all failed
```

**Action: Should fix.**

---

### Finding 4: ADR-100 compliance — the framing is sound (Severity: LOW — Architectural, positive)

**Assessment.** The design review flagged the ADR-100 tension. The implementation addresses
it well:

1. The doc comment on `attempt_transient_cleanup()` explicitly states this is a timing
   optimization, not a policy decision.
2. The `TransientCleanupOutcome` enum makes the delete fully auditable.
3. The cleanup runs AFTER the operation loop, not inside it — maintaining the planner's
   operation sequence.
4. The cleanup is scoped to transient subvolumes only, with no generalization path.

The framing is legitimate. Pin file writes are a consequence of execution; immediate
cleanup of the now-unpinned old parent is a consequence of pin advancement. The executor
does not decide WHAT to delete (the transient policy does), only WHEN (now vs next run).

**No action needed.**

---

### Finding 5: `already_cleaned` set from design is correctly omitted (Severity: LOW — Design compliance, positive)

**The design** specified an `already_cleaned: HashSet<PathBuf>` and test case 8 to prevent
double-delete between planned transient cleanup and immediate post-send cleanup.

**Analysis.** The implementation omits both. This is correct because:

1. The planner's operation ordering is create -> local retention deletes -> sends ->
   external retention deletes (verified at `plan.rs` line 144).
2. Planned transient deletes only target snapshots NOT in the protected set.
3. The old pin parent IS in the protected set (it is pinned at planning time).
4. Therefore, the planner never plans a transient delete for the old pin parent.
5. The immediate cleanup and planned deletes target disjoint snapshot sets.

The `already_cleaned` mechanism was designed for a scenario that cannot occur given the
planner's protection logic. Omitting it avoids unnecessary complexity.

**No action needed.** The design could be updated to note this analysis.

---

### Finding 6: Missing test — parse failure in pin check (Severity: MEDIUM — Test gap)

**Related to Finding 1.** There is no test for the case where the old parent path has an
unparseable snapshot name. Given that Finding 1 represents a fail-open violation, a test
should verify the corrected behavior (delete skipped when name is unparseable).

**Test sketch:**

```rust
#[test]
fn transient_cleanup_refuses_delete_when_name_unparseable() {
    // Create an old parent with a name that fails SnapshotName::parse()
    let old_parent = sv_dir.join("not-a-valid-snapshot-name");
    std::fs::create_dir(&old_parent).unwrap();
    // ... set up sends ...
    // Assert: TransientCleanupOutcome is NotApplicable or similar, NOT Cleaned
}
```

**Action: Must add (after Finding 1 fix).**

---

### Finding 7: Missing test — shutdown signal during cleanup (Severity: LOW — Test gap)

**The scenario.** The shutdown signal (`self.shutdown.load()`) is checked at the top of
each operation in the loop, but `attempt_transient_cleanup()` does not check it. If a
shutdown signal arrives after the operation loop but before the cleanup delete, the delete
proceeds.

**Assessment.** This is acceptable because:
1. The cleanup is fast (one `btrfs subvolume delete` call).
2. The delete is safe (the snapshot is verified unpinned).
3. Skipping it would just defer to the next run.

But it is a deviation from the executor's general pattern of checking shutdown before each
destructive operation. Consider adding a shutdown check before the delete call, or
documenting why it is intentionally omitted.

**Action: Consider (low priority).**

---

### Finding 8: `TransientCleanupOutcome::DeleteFailed` reports only one failure (Severity: LOW — Design)

**Related to Finding 3.** The `DeleteFailed` variant holds a single `path` and `error`.
With multiple divergent parents, only the first failure is reported. If Finding 3 is fixed
to continue past failures, the variant should either hold a `Vec` of failures or the
outcome should distinguish "partial cleanup" (some deleted, some failed) from "total
failure."

**Action: Should fix (with Finding 3).**

---

## Design Tensions

### Timing optimization vs. architectural purity

The core tension: the executor deletes a snapshot the planner did not plan. The
implementation handles this well through framing (doc comment), scoping (transient only),
and auditability (`TransientCleanupOutcome`). The key constraint that makes this acceptable
is that transient mode's policy is unconditional: delete all non-pinned snapshots. The
executor is not evaluating graduated retention windows or making nuanced keep/delete
decisions. It is checking a binary condition (pinned or not) that the planner would also
check.

The precedent risk is real but mitigated by the explicit framing. If a future change
proposes executor-driven deletions for graduated retention, the comment on
`attempt_transient_cleanup()` serves as a bright line.

### Defense-in-depth vs. simplicity

The pin re-read (condition 5) duplicates work the "all drives succeeded" check (condition
2) should make unnecessary. If all drives succeeded, pins have advanced, and the old parent
is unpinned by construction. The re-read exists purely as a defense-in-depth layer. This
is the right trade — the cost is a few filesystem reads, and the benefit is catching bugs
in the pin advancement logic that would otherwise cause silent data loss.

---

## The Simplicity Question

> If you could mass-delete code from this feature, what would go?

Nothing. The feature is already minimal: ~80 lines of cleanup logic, 5 conditions, one
enum, one struct. The `SubvolumeContext` could theoretically be avoided by passing
`is_transient: bool` directly, but the struct is better for extensibility and costs
nothing. The `TransientCleanupOutcome` enum has exactly the right number of variants.

---

## Action Items

| # | Finding | Priority | Action |
|---|---------|----------|--------|
| 1 | Pin check fails open on parse error | **Must** | Invert logic to fail-closed: unparseable name -> skip delete |
| 2 | Raw field `is_transient` check | **Should** | Add comment documenting equivalence assumption |
| 3 | Early return abandons remaining parents | **Should** | Continue through all parents, track failures |
| 6 | Missing test: unparseable name | **Must** | Add test after Finding 1 fix |
| 7 | No shutdown check in cleanup | **Consider** | Add check or document omission |
| 8 | `DeleteFailed` single-failure reporting | **Should** | Update variant if Finding 3 is fixed |

---

## Open Questions

1. **Should the transient cleanup outcome be recorded in SQLite?** Currently the cleanup
   is logged and returned in `SubvolumeResult`, but `record_operation()` is only called
   inside the operation loop. The cleanup delete happens after the loop. This means the
   SQLite history does not capture transient cleanup deletes. If the state DB is "history"
   (ADR-102), this is a gap. Low priority since SQLite failures never prevent backups,
   but worth noting for completeness.

2. **Should `execute_delete` be reused instead of a direct `btrfs.delete_subvolume()` call?**
   The cleanup bypasses `execute_delete()`'s pin protection check and space recovery
   tracking. The pin check is done separately (and is the subject of Finding 1).
   The space recovery skip is irrelevant for transient cleanup (transient subvolumes
   don't participate in space-pressure-gated deletion). But reusing `execute_delete()`
   would inherit future safety improvements automatically. Trade-off: it also inherits
   the space-recovery gate, which could incorrectly skip the cleanup delete.
