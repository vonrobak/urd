---
upi: "022"
status: proposed
date: 2026-04-05
---

# Design: The Honest Nightly (UPI 022)

> **TL;DR:** Four surgical fixes to make v0.11.0's nightly run honest: (1) transient
> cleanup stops protecting snapshots for absent drives, (2) sentinel suppresses vacuous
> "0 chains broke" warnings, (3) "send disabled" becomes "local only", (4) config
> scopes htpc-root to its actual drive. Ships as v0.11.1. No new modules, no config
> schema changes, no new user concepts.

## Problem

The first v0.11.0 nightly (run #29, 2026-04-05) revealed four issues that undermine
user trust, one of which is a data safety risk:

**1. Transient snapshots accumulate for absent drives (CRITICAL).** htpc-root has
`local_snapshots = false` but 2 local snapshots exist on a 118GB NVMe with 26GB free.
Root cause: `plan_local_retention()` protects snapshots as "unsent" for ALL configured
drives, including absent ones. WD-18TB1 (offsite) and 2TB-backup (test) are absent
indefinitely — their sends never complete — so unsent protection never releases. This
is the exact accumulation pattern that caused the catastrophic NVMe exhaustion.

**2. Sentinel logs "all 0 chains broke" warnings.** `detect_simultaneous_chain_breaks()`
fires when a drive transitions from mounted to unmounted, producing "Drive anomaly:
all 0 chains broke on 2TB-backup simultaneously". The `total_chains` field is 0 — a
vacuously true anomaly that teaches the user to ignore sentinel warnings.

**3. "send disabled" text for local-only subvolumes.** subvol4-multimedia and subvol6-tmp
show `"send disabled"` in skip reasons. They're local-only by design, not disabled.
The text implies something is broken.

**4. htpc-root assessed against all drives.** htpc-root has no explicit `drives` config,
so `awareness.rs` evaluates it against all 3 drives. With WD-18TB1 and 2TB-backup
absent, htpc-root is permanently "degraded" — a state the user can't fix without
connecting drives they don't intend to connect.

## Relationship to UPI 011

UPI 011 ("Transient Space Safety") was designed on 2026-04-03 and proposes three
changes: (1) drive-availability preflight for transient snapshot creation, (2) transient
cap of 1, (3) pin file self-healing. It was reviewed by Steve but never implemented.

This design **subsumes UPI 011's scope** with a simpler approach informed by production
data. The key insight from the first nightly: the problem is not that snapshots are
created — it's that they're protected indefinitely for absent drives. UPI 011's
Changes 1 and 2 are defense-in-depth that become unnecessary when the root cause (Change
3-equivalent: absent-drive protection) is fixed directly.

**What we take from UPI 011:** The core insight that transient retention must be
drive-availability-aware. UPI 011's Change 1 (skip creation when no drive mounted) is
a good secondary guard that we include. UPI 011's Change 3 (pin self-healing) is
valuable but orthogonal — deferred to a separate patch.

**What we simplify:** UPI 011's Change 2 (cap of 1) is dropped. With the
absent-drive fix, the snapshot count naturally stays at 1-2 during the send window
and drops to 1 after cleanup. A hard cap that overrides pin protection (violating
ADR-106) is unnecessary when the protection logic itself is correct.

## Proposed Design

### Fix 1: Transient retention ignores absent drives

**Where:** `plan_local_retention()` in `plan.rs`, lines 426-448 — the protected set
construction.

**What changes:** When building the unsent-protection set for a transient subvolume,
only consider pins for drives that are currently mounted. Absent drives can't receive
sends, so protecting snapshots for them is pointless — and on space-constrained
filesystems, dangerous.

```rust
// In plan_local_retention(), the protected-set construction block.
// Current code (lines 426-448):
let protected = if subvol.send_enabled {
    let oldest_pin = pinned.iter().min();
    let mut expanded = pinned.clone();
    // ... expands to all snapshots newer than oldest pin
    expanded
} else {
    pinned.clone()
};

// Changed: add `mounted_drives` parameter, filter for transient
let protected = if subvol.send_enabled {
    let effective_pinned = if subvol.local_retention.is_transient() {
        // For transient subvolumes, only protect pins for mounted drives.
        // Absent drives will get full sends when they return — protecting
        // snapshots for them indefinitely risks space exhaustion.
        pinned.iter()
            .filter(|snap| {
                // A pin is "mounted" if its drive is currently available.
                // Pin files are named .last-external-parent-{DRIVE_LABEL}.
                // We need to check if the drive that created this pin is mounted.
                // This requires the mounted_drives set as input.
                mounted_pin_snapshots.contains(snap)
            })
            .cloned()
            .collect::<HashSet<_>>()
    } else {
        pinned.clone()
    };
    let oldest_pin = effective_pinned.iter().min();
    let mut expanded = effective_pinned.clone();
    match oldest_pin {
        Some(oldest) => {
            for snap in local_snaps {
                if snap > oldest {
                    expanded.insert(snap.clone());
                }
            }
        }
        None if subvol.local_retention.is_transient() => {
            // Transient + no mounted pins = nothing to protect.
            // All snapshots eligible for deletion.
        }
        None => {
            // Non-transient: no pins at all — protect everything.
            for snap in local_snaps {
                expanded.insert(snap.clone());
            }
        }
    }
    expanded
} else {
    pinned.clone()
};
```

**Concrete implementation:** The `plan_local_retention` function currently receives
`pinned: &HashSet<SnapshotName>` — the set of all pinned snapshots from all drives.
We need to split this into mounted and unmounted pins.

**New parameter:** `mounted_pins: &HashSet<SnapshotName>` — pins from mounted drives only.

**How to compute it:** In the caller (`plan()` main loop), we already call
`chain::find_pinned_snapshots(local_dir, &drive_labels)` which reads all pin files.
We also already know which drives are mounted (from `fs.drive_availability()`). Filter
the pinned set to only drives where `DriveAvailability::Available`.

```rust
// In the plan() main loop, after building `pinned`:
let mounted_pins: HashSet<SnapshotName> = config.drives.iter()
    .filter(|d| {
        subvol.drives.as_ref().map_or(true, |allowed| allowed.contains(&d.label))
    })
    .filter(|d| matches!(fs.drive_availability(d), DriveAvailability::Available))
    .filter_map(|d| chain::read_pin_file(&local_dir, &d.label).ok().flatten())
    .map(|pin| pin.name)
    .collect();
```

Then pass `mounted_pins` to `plan_local_retention()`. For non-transient subvolumes,
the function uses `pinned` (all pins) as before. For transient, it uses `mounted_pins`.

**Risk:** If a drive reconnects between runs, the old chain parent may have been deleted.
The next send will be a full send. For offsite drives that return monthly, a full send
is expected anyway. For the primary drive (WD-18TB), it's always mounted, so its pins
are always in the mounted set. Acceptable tradeoff.

**Test strategy:** Unit tests with `MockFileSystemState`:
- Transient + 1 mounted drive + 1 absent drive: only mounted drive's pin protected
- Transient + no mounted drives: no pins protected, all old snapshots eligible for delete
- Non-transient + absent drives: ALL pins protected (unchanged behavior)
- Transient + all drives mounted: same as current behavior (all pins protected)
- Edge: transient + no pins at all + no mounted drives: no snapshots protected

Also include UPI 011's Change 1 (skip snapshot creation for transient when no drives
mounted):

**Where:** `plan()` main loop, before calling `plan_local_snapshot()`.

```rust
if subvol.local_retention.is_transient()
    && !local_snaps.is_empty()
    && !any_drive_mounted
{
    skipped.push((
        subvol.name.clone(),
        "no drive available for send — snapshot deferred".to_string(),
    ));
    // Still run retention to clean up excess snapshots
    plan_local_retention(...);
    continue; // skip to next subvolume
}
```

This prevents creating new snapshots when no drive can receive them, while still
running retention to clean up any accumulated snapshots from before the fix.

~8-10 new tests.

### Fix 2: Sentinel guards on chain delta

**Where:** `detect_simultaneous_chain_breaks()` in `sentinel.rs`, line 819.

**What changes:** Add a guard that the broken count is meaningful.

Current guard:
```rust
if prev_count >= 2 && intact == 0 && total > 0 {
```

Changed:
```rust
let broken = prev_count.saturating_sub(intact);
if broken >= 2 && total > 0 {
```

This computes the actual number of chains that broke. If 0 chains broke (prev_count == 0
or intact == prev_count), no anomaly. If 1 chain broke, no anomaly (could be a normal
chain break). If 2+ chains broke simultaneously, anomaly — likely a drive swap or
mass corruption.

Also fix the log message to report the correct count. In `sentinel_runner.rs` line 369:

Current:
```rust
"Drive anomaly: all {} chains broke on {} simultaneously",
anomaly.total_chains,
```

Changed:
```rust
"Drive anomaly: {} chains broke on {} simultaneously",
anomaly.broken_count,
```

**Struct change:** Add `broken_count: usize` to `DriveAnomaly` (sentinel.rs). The
`total_chains` field is still useful for the notification body.

**Test strategy:** Unit tests for `detect_simultaneous_chain_breaks`:
- Previous 0, current 0 → no anomaly
- Previous 3, current 0, total > 0 → anomaly (broken = 3)
- Previous 3, current 3 → no anomaly (broken = 0)
- Previous 1, current 0 → no anomaly (broken = 1, below threshold)
- Previous 2, current 0, total 0 → no anomaly (drive disconnected)

~4-5 new tests.

### Fix 3: "send disabled" → "local only"

**Where:** `plan.rs`, line 265.

**What changes:** One string:

```rust
// Before:
skipped.push((subvol.name.clone(), "send disabled".to_string()));
// After:
skipped.push((subvol.name.clone(), "local only".to_string()));
```

The `SkipCategory` in `output.rs` already uses `LocalOnly` as the enum variant. The
reason text now matches the category.

**Test strategy:** Update any existing test that asserts on "send disabled" string.
Grep for the string to find affected tests. ~1-2 test updates.

### Fix 4: Config — scope htpc-root drives

**Where:** `~/.config/urd/urd.toml`, htpc-root section.

**What changes:** Add explicit drive scoping:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
priority = 3
snapshot_interval = "1d"
send_interval = "1d"
local_snapshots = false
drives = ["WD-18TB"]
external_retention = { daily = 30, monthly = 0, weekly = 26 }
```

This is a config edit, not a code change. Effects:

1. `awareness.rs` only assesses htpc-root against WD-18TB → health becomes "healthy"
   when WD-18TB is connected (no more false "degraded" from absent WD-18TB1/2TB-backup).

2. `plan_local_retention()` only reads WD-18TB's pin file → the mounted-pins set
   always contains WD-18TB's pin (it's always connected) → transient cleanup works
   correctly even without Fix 1. Fix 1 is still needed for the general case.

3. htpc-root stops sending to WD-18TB1 and 2TB-backup. This is the correct intent —
   htpc-root needs one reliable external copy, not copies on every drive.

**No code change. No test change. No migration.**

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `plan.rs` | Filter pinned set for transient subvolumes by drive mount state; skip transient snapshot creation when no drives mounted | ~8-10 unit tests with MockFileSystemState |
| `sentinel.rs` | Guard on chain delta instead of absolute count; add `broken_count` to `DriveAnomaly` | ~4-5 unit tests |
| `sentinel_runner.rs` | Update log message to use `broken_count` | Covered by sentinel.rs tests |
| `plan.rs` (line 265) | Change "send disabled" to "local only" | ~1-2 test updates |
| Config file | Add `drives = ["WD-18TB"]` to htpc-root | Manual verification via `urd status` |

**Modules NOT touched:** `types.rs`, `config.rs`, `retention.rs`, `awareness.rs`,
`executor.rs`, `voice.rs`, `output.rs`, `chain.rs`, `btrfs.rs`.

## Effort Estimate

**~0.5 session.** Calibrated against:
- UPI 004 (token gate): 1 module, 5 tests, half session
- UPI 005 (status truth): 2 modules, 12 tests, half session

This is 2 modules (plan.rs, sentinel.rs) + 1 string change + 1 config edit, with
~13-17 new tests. The changes are surgical — no new types, no new modules, no config
schema changes.

## Sequencing

1. **Fix 4 first (config edit).** Zero risk. Immediate effect: htpc-root becomes
   "healthy" in status, reduces drive scope so subsequent code changes have cleaner
   test conditions. Do this before writing code.

2. **Fix 1 second (transient absent-drive fix).** Highest code complexity and highest
   data safety impact. The protection-set filtering is the critical logic that must be
   right. Write tests first, then implement.

3. **Fix 3 third (string change).** Trivial. One line. Do it while the plan.rs file is
   open from Fix 1.

4. **Fix 2 last (sentinel guard).** Independent of the other fixes. Lower risk — the
   sentinel warning is annoying but not dangerous.

**Risk ordering rationale:** Fix 4 de-risks Fix 1 by scoping htpc-root to one drive.
Fix 1 is then testable against a simpler scenario (1 mounted drive vs 3 with 2 absent).
Fixes 3 and 2 are independent cleanup.

## Architectural Gates

**None.** All changes operate within existing module boundaries:

- Planner remains pure (Fix 1 adds conditions, not I/O)
- Sentinel remains pure (Fix 2 modifies detection logic, no I/O)
- No new on-disk contracts
- No config schema changes (Fix 4 uses existing `drives` field)
- ADR-106 (pin protection) is respected — we're not overriding pins, we're
  correctly scoping which pins are relevant for transient retention
- ADR-107 (fail-closed deletions) is respected — graduated subvolumes still
  protect all pins; only transient subvolumes scope to mounted drives

## Rejected Alternatives

### Transient cap of 1 (UPI 011 Change 2)

Hard-caps transient subvolumes at 1 local snapshot regardless of pin state. Rejected
because:
- Overrides pin protection (violates ADR-106) — could delete a snapshot that's the
  chain parent for a mounted drive mid-send
- Unnecessary when the root cause (absent-drive protection) is fixed
- The emergency pre-flight (UPI 016) already exists as a space-based safety net

### Rename `local_snapshots = false` to `local_retention = "minimal"`

Adds config vocabulary to explain an implementation detail. Rejected because:
- `local_snapshots = false` is the user's language ("don't store snapshots on my drive")
- With Fix 1, the behavior matches the intent: 0-1 transient snapshots, cleaned up
  after send
- Renaming a config field requires migration, docs updates, and teaches users a new
  concept for no behavioral benefit

### Rotation interval for drives (Idea 5d from brainstorm)

Adds `rotation_interval = "30d"` to drive config for expected-absence thresholds.
Rejected because:
- Introduces a new config concept for a problem solvable with one config line (Fix 4)
- Adds complexity to awareness.rs for a single-user edge case
- Can be designed later if real users need it post-v1.0

### Role-based health weighting (Idea 5b from brainstorm)

Uses drive roles to weight health computation. Rejected because:
- Same reasoning as rotation intervals — overengineered for the current problem
- The `drives` field already provides explicit scoping
- Role semantics are still evolving (role field was added recently)

### Unify plan and dry-run (Idea 6d from brainstorm)

Merge `urd backup --dry-run` and `urd plan` into one code path. Rejected for this patch
because:
- Different user intents ("what's due?" vs "rehearse this action")
- Larger refactor than the other fixes
- The divergence is a UX concern, not a safety concern
- Can be addressed with a one-sentence footer in dry-run output (deferred)

## Assumptions

1. **WD-18TB is always connected when nightly runs.** Fix 4 scopes htpc-root to only
   WD-18TB. If WD-18TB is disconnected during a nightly, htpc-root will skip sends
   (correct) and skip snapshot creation (correct — no drive to send to). The existing
   "no backup drives connected" blocked state handles this.

2. **Full sends to returning drives are acceptable.** Fix 1 allows old chain parents
   to be deleted even if an absent drive still has that parent as its latest. When the
   drive returns, it gets a full send. For an offsite drive returning monthly, a full
   send is expected. For 2TB-backup (test drive), full sends are fine.

3. **The `DriveAnomaly` struct can be extended.** Adding `broken_count: usize` to
   `DriveAnomaly` is additive — no existing consumers break.

4. **Existing tests don't assert on "send disabled" text broadly.** A grep will confirm.
   If tests exist, they need updating as part of Fix 3.

## Open Questions

### Q1: Should Fix 1's absent-drive scoping apply to the executor's transient cleanup too?

**Option A (planner only):** Only change `plan_local_retention()`. The executor's
`attempt_transient_cleanup` continues to require all planned sends to succeed.

**Option B (planner + executor):** Also change `attempt_transient_cleanup` to consider
only mounted drives when evaluating "all sends succeeded."

**Recommendation: Option A for this patch.** The executor's cleanup is a bonus
optimization — the planner is the authority for retention. With Fix 1 in the planner,
the next run will delete any snapshots the executor didn't clean up. The executor
change can follow if needed.

### Q2: Should Fix 1 also skip snapshot creation for transient when no drives are mounted?

**Option A (retention-only fix):** Only change the protection set calculation. Snapshots
are still created on interval; they just get cleaned up faster.

**Option B (retention + creation skip):** Also skip `plan_local_snapshot()` for transient
subvolumes when no drives are mounted (UPI 011's Change 1).

**Recommendation: Option B.** Creating a snapshot that can't be sent is pointless for
a transient subvolume. It just creates work for the retention cleanup to undo. Include
the creation skip as a secondary guard. Already incorporated in the design above.

### Q3: Should the sentinel `broken >= 2` threshold be configurable?

**Option A (hardcoded threshold):** Always require 2+ chains to break simultaneously.

**Option B (configurable):** Add a threshold to sentinel config.

**Recommendation: Option A.** The threshold of 2 is the minimum for "this is suspicious."
1 chain breaking is normal (pin file deleted, manual intervention). 2+ simultaneous
is a drive-level event. No need for configuration.
