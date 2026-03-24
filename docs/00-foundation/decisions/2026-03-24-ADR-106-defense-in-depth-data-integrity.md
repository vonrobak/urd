# ADR-106: Defense-in-Depth for Data Integrity

> **TL;DR:** Retention and deletion logic uses multiple independent layers of protection so
> that silent data loss requires multiple simultaneous failures. Pinned snapshots are guarded
> by the planner (exclusion), executor (re-check), and unsent snapshot protection (blanket
> guard). Each layer is independently testable and independently sufficient.

**Date:** 2026-03-22 (identified in Phase 1 hardening; formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** None (crystallized across Phase 1–5 adversary reviews)

## Context

For a backup tool on a BTRFS pool with no RAID, silent data loss is the catastrophic
failure mode. The most direct path to it: retention deletes the last copy of a snapshot
that hasn't been sent to an external drive. If this happens silently (no error, no log),
the user discovers the gap only when they need to restore — which is too late.

The Phase 1 adversary review identified this risk. The Phase 1 hardening review introduced
unsent snapshot protection. Every subsequent review validated and extended the layered
approach.

## Decision

### Layer 1: Unsent snapshot protection (planner)

When `send_enabled` is true for a subvolume, the planner protects snapshots that may not
yet have been sent to all drives:

- If pins exist: protect all snapshots newer than the oldest pin parent
- If no pins exist (first run): protect everything — no snapshot is deleted until at
  least one successful send establishes a pin
- If `send_enabled` is false: no protection (normal retention applies)

This is a blanket guard. It does not check whether each individual snapshot has been sent
— it protects the entire window between the oldest pin and now.

### Layer 2: Pin-based exclusion (planner)

The planner's retention logic excludes all pinned snapshot names from the deletion candidate
set. A snapshot that is any drive's current pin parent is never proposed for deletion,
regardless of its age or retention window.

### Layer 3: Pin re-check (executor)

Before executing any `DeleteSnapshot` operation, the executor re-reads pin files and
verifies the target is not currently pinned. This catches any TOCTOU race between planning
and execution (e.g., another process wrote a pin file between plan and execute).

### Each layer is independently sufficient

If Layer 1 fails (bug in unsent protection), Layer 2 still excludes pinned snapshots.
If Layer 2 fails (bug in retention exclusion), Layer 3 catches it at execution time.
If Layer 3 fails (bug in re-check), Layers 1 and 2 have already prevented the snapshot
from being in the deletion list.

Silent data loss requires all three layers to fail simultaneously on the same snapshot.

## Consequences

### Positive

- The most direct path to catastrophic failure requires three independent bugs
- Each layer is unit-testable in isolation (planner tests for Layer 1–2, executor tests
  for Layer 3)
- The layers compose without knowing about each other — no coupling between them

### Negative

- Redundant checks add code complexity (the executor's pin re-check is "unnecessary" if
  the planner works correctly)
- The defense-in-depth philosophy means some code exists solely for safety, not for
  correctness under normal operation — this can look like dead code to someone unfamiliar
  with the rationale

### Constraints

- New deletion paths (e.g., `urd prune`, manual cleanup commands) must implement at
  minimum Layer 2 (pin exclusion) and Layer 3 (pre-deletion pin check). Layer 1 applies
  only to automated retention.
- Pin file writes must be atomic (temp + rename). A corrupted pin file could cause both
  Layer 2 and Layer 3 to miss the protection.

## Related

- ADR-100: Planner/executor separation (the layers map to the planner/executor boundary)
- ADR-104: Graduated retention (the retention logic where these layers operate)
- [Phase 1 hardening review](../../99-reports/2026-03-22-phase1-hardening-review.md) —
  unsent snapshot protection introduced
- [Phase 3.5 adversary review](../../99-reports/2026-03-22-arch-adversary-phase35.md) —
  "three layers of protection" articulated
