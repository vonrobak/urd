# Design: Transient Local Snapshots

> **TL;DR:** A subvolume workflow where local snapshots exist only long enough to be sent
> externally, then are deleted. Solves the htpc-root problem (118GB NVMe can't sustain
> local history) and opens a general "send-only" use case for large subvolumes on
> constrained volumes.

**Date:** 2026-03-27
**Status:** Idea — not yet designed for implementation
**Trigger:** Fourth NVMe exhaustion incident from htpc-root snapshots competing with
htpc-home for space on the 118GB system drive.

## The problem

Some subvolumes live on volumes too small for local snapshot history. htpc-root (`/`) is
the canonical example: the 118GB NVMe also hosts htpc-home, and every htpc-root snapshot's
CoW delta steals headroom from htpc-home snapshots that protect data changing daily.

The user doesn't need local htpc-root history — if root breaks, they reinstall. But they
*do* want the root filesystem sent to an external drive periodically, so configs and system
state are recoverable.

**Current options are insufficient:**
- `enabled = false` — disables everything, including external sends
- `protection_level = "guarded"` — local-only, no external sends (the opposite of what's
  needed)
- Minimal retention (`daily = 1`) — still keeps 1 snapshot permanently, still costs NVMe
  space, and that space grows as root changes

## The concept: transient local snapshots

A local snapshot that exists only as a transport vehicle for an external send.

```
create local snapshot  →  send to external drive  →  delete local snapshot
```

The snapshot's lifecycle is bound to the send operation, not to a retention policy.

## Key design questions

### 1. How to express this in config?

**Option A: New retention mode**
```toml
[[subvolumes]]
name = "htpc-root"
local_retention = "transient"    # Keep only until sent
send_enabled = true
drives = ["WD-18TB1"]
```

**Option B: Explicit flag**
```toml
[[subvolumes]]
name = "htpc-root"
transient_local = true           # Delete local after successful send
send_enabled = true
drives = ["WD-18TB1"]
```

**Option C: Retention count of zero**
```toml
[[subvolumes]]
name = "htpc-root"
local_retention = { daily = 0 }  # Keep nothing — implies transient
send_enabled = true
drives = ["WD-18TB1"]
```

Option A feels cleanest — `"transient"` communicates intent clearly and doesn't overload
existing retention semantics. Option C is tempting but `daily = 0` currently means
"unlimited" in the graduated retention system, so it would be a semantic inversion.

### 2. What happens when a send fails?

**Keep the local snapshot.** Fail closed — don't delete data that hasn't been successfully
sent anywhere. The snapshot persists until the next successful send, at which point it
becomes eligible for deletion.

This means transient snapshots can accumulate under repeated failures. The space guard
(`min_free_bytes`) still applies and will block new snapshot creation if the volume is
under pressure. The circuit breaker (in Sentinel active mode) prevents infinite retry
loops.

### 3. What about pin files?

Pin files point to the last successfully sent snapshot. In a transient workflow:

1. Create snapshot A, send to drive, pin A, delete A locally
2. Next cycle: create snapshot B, attempt incremental send with parent A
3. But A doesn't exist locally anymore — incremental send fails, falls back to full send

**This breaks incremental chains by design.** Every send becomes a full send.

**Is that acceptable?** For htpc-root, yes — root changes slowly, full sends are small
(a few GB). For larger subvolumes, the cost may be prohibitive.

**Alternative: keep the pinned snapshot.** Transient retention means "delete everything
except the pin." This preserves incremental chains at the cost of one persistent local
snapshot. The pin rotates on each successful send, so it's always the most recent sent
snapshot — bounded space cost.

This is probably the right default. The pin is small (one snapshot's CoW delta from the
current state), and incremental sends save far more space on the external drive than the
pin costs locally.

```
Transient retention rule:
  - Keep: the pinned snapshot (incremental chain parent)
  - Delete: everything else
  - Net local cost: 1 snapshot (the pin)
```

### 4. How does awareness assess a transient subvolume?

Current behavior: 0 local snapshots → UNPROTECTED. But a transient subvolume with recent
external sends is not unprotected — its data is on the external drive.

**Options:**
- **A. Awareness learns about transient mode.** If `local_retention = "transient"`, local
  status is assessed differently: the external send recency becomes the primary signal,
  and local snapshot absence is expected, not alarming.
- **B. Transient subvolumes skip local assessment.** Only external status matters.
- **C. Accept UNPROTECTED for local, let overall status be driven by external.**

Option A is correct but adds complexity to the awareness model. Option C may be sufficient
initially — the overall promise status already aggregates local + external, and "AT RISK"
from external recency is more informative than "UNPROTECTED" from local absence.

If the pinned-snapshot approach is used (keeping one local snapshot for incremental chains),
local assessment would show 1 snapshot and likely report PROTECTED or AT RISK depending on
age — which is actually reasonable.

### 5. Interaction with protection levels

Transient doesn't map cleanly to existing protection levels:
- **Guarded** = local only, no sends → opposite of transient
- **Protected** = local + 1 external drive → local retention expected
- **Resilient** = local + 2 external drives → local retention expected

Transient is a `custom` workflow. It should require `protection_level = "custom"` (or no
protection level). Named levels should not support transient mode — their whole point is
opaque, well-understood semantics.

### 6. Planner changes

The planner currently emits: CREATE → SEND → RETENTION DELETE (graduated).

For transient mode, the sequence becomes: CREATE → SEND → TRANSIENT DELETE (all except pin).

The transient delete is conceptually different from retention — it's not thinning based on
age windows, it's cleaning up after a successful transport operation. This suggests a new
`PlannedOperation` variant rather than overloading retention:

```rust
PlannedOperation::DeleteTransient {
    location: Location::Local,
    snapshot_dir: PathBuf,
    snapshot: SnapshotName,
    reason: String,  // "transient: sent to WD-18TB1"
}
```

Or it could be a regular `DeleteSnapshot` with a different reason field. The executor
doesn't care about the reason — it just deletes. The distinction matters for logging and
for `urd plan` output.

### 7. Executor changes

The executor needs to know whether to delete local snapshots after a successful send.
Currently, sends and local retention are independent operations. For transient mode:

- If send succeeds → mark local snapshot for deletion (except pin)
- If send fails → keep local snapshot

This is a conditional delete that depends on send outcome. The current executor processes
operations sequentially and doesn't have send-outcome-dependent branching. Two approaches:

**A. Planner emits conditional operations.** `DeleteIfSent { snapshot, drive }` — executor
checks send results before executing.

**B. Two-pass execution.** First pass: create + send. Second pass: transient cleanup based
on results. This is closer to how the executor already works (it tracks `failed_creates`).

**C. Post-execution cleanup.** After all operations complete, a cleanup pass deletes
transient snapshots that were successfully sent. Simple, but runs after retention, which
might try to delete the same snapshots.

Option B feels most natural given the existing executor structure.

## Scope and sequencing

This feature touches: config parsing, types, planner, executor, retention, awareness,
voice, and possibly the Sentinel. It's medium-sized and architecturally significant.

**Recommended sequence:**
1. Design review (arch-adversary) on this document
2. Implement config parsing (`local_retention = "transient"` or equivalent)
3. Implement planner transient delete logic
4. Implement executor conditional delete
5. Update awareness model for transient subvolumes
6. Update voice rendering
7. Implementation review

**Not in scope for initial implementation:**
- Sentinel active mode interaction (builds on Session 4)
- Multiple-drive transient sends (send to all configured drives before deleting)
- Config migration (new field, no migration needed — additive)

## Immediate workaround

Until this feature is built, htpc-root should be set to `enabled = false` in the
production config. This disables all operations including external sends. The root
filesystem is low-value for local snapshots and can be backed up manually or re-enabled
once transient mode exists.
