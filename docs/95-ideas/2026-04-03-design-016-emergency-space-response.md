---
upi: "016"
status: proposed
date: 2026-04-03
---

# Design: Guided Emergency Space Recovery (UPI 016)

> **TL;DR:** Two-mode emergency space response. `urd emergency` is a guided interactive
> command that assesses snapshot congestion, previews aggressive thinning, and executes
> with confirmation. The invisible worker gains an automatic emergency retention path that
> runs before the nightly backup when space is critically low, freeing space and notifying
> the user. Both modes share a new pure function in `retention.rs` and reuse the existing
> delete path in the executor.

## Problem

Urd caused a catastrophic storage failure from snapshot congestion during development.
The current space guard in `plan.rs` detects `min_free > 0 && free_bytes < min_free` and
enters `space_pressure` mode, which tightens the hourly retention window. This is
mitigation, not recovery: it limits future accumulation but doesn't free space that is
already consumed. The system can reach a state where:

1. The snapshot root is critically full.
2. Every nightly run tries to create a new snapshot, hits the space guard, skips it.
3. No retention runs because there's nothing to delete beyond what's already kept.
4. Space never recovers without manual `btrfs subvolume delete` commands.

The user faces a silent, stuck system. There is no guided path out.

## Current State

| Location | What it does | What it doesn't do |
|----------|--------------|--------------------|
| `plan.rs:414` | Detects `space_pressure` per subvolume | Doesn't free space, just tightens future retention |
| `retention.rs:28-33` | `graduated_retention()` accepts `space_pressure` flag | `space_pressure` only thins hourly window, not a deep cut |
| `preflight.rs` | Config consistency checks | No space trend analysis or early warning |
| `sentinel.rs` | Pure state machine, drive/tick events | No space-critical event or emergency retention action |
| `commands/doctor.rs` | Runtime checks | No "approaching threshold" projection |

There is no command the user can run to recover, and no automatic mechanism for the daemon
to self-heal before a backup run fails.

## Design

### Two modes

**Mode 1: `urd emergency` — the guided interactive command.**
The user runs this when they notice space is low, Sentinel notified them, or a backup run
reported a problem. It is a deliberate, confirmed action — not automatic.

**Mode 2: Automatic emergency in the invisible worker.**
When the nightly timer (or Sentinel on drive mount) detects space is critically low before
attempting a backup, it runs emergency retention first, logs what it freed, and then
proceeds with the normal backup plan. Notifies the user via the standard notification path.

Both modes share a single pure function in `retention.rs`: `emergency_retention()`.
Neither adds new deletion infrastructure — they produce a list of snapshots to delete, and
the existing executor delete path handles it.

### `urd emergency` — the interaction

```
$ urd emergency

Urd sees a crisis.

~/.snapshots/home — 1.8 GB free (threshold: 10 GB)
  47 snapshots across 2 subvolumes
  Oldest: 2026-01-15  Newest: 2026-04-03
  Chain parents pinned: 3

~/.snapshots/root — 22 GB free (OK)

This will delete 39 snapshots from home, freeing approximately 8.2 GB.
Your newest snapshot and all chain parents will be preserved.
9 snapshots will remain.

Proceed? [y/N]
```

If the user confirms:

```
Deleting snapshots from home...
  [████████████████████] 39/39

Freed 8.2 GB. 9 snapshots remain in home.
~/.snapshots/home now has 10.0 GB free.

The nightly timer will resume normal operation.
```

If there is no crisis:

```
$ urd emergency

No crisis detected. All snapshot roots are within their free-space thresholds.
```

### Emergency retention semantics

`emergency_retention()` returns the minimal keep set: the **latest snapshot per subvolume**
plus all **pinned snapshots** (chain parents). Everything else is marked for deletion.

This is intentionally more aggressive than `space_pressure` mode. `space_pressure` only
thins the hourly window — it still retains weeks and months of history. Emergency retention
is a one-time deep cut to recover from a stuck state, not a routine retention policy.

```
Keep:
  - The single newest snapshot per subvolume (the freshest history)
  - All pin-file-referenced snapshots (send chain integrity)

Delete:
  - Everything else
```

### ADR-106 defense-in-depth

Emergency retention applies the same three-layer pin protection as all other retention:

1. `emergency_retention()` (pure) builds the keep set with pin-file-referenced names.
2. The planner's unsent-snapshot protection is not bypassed — `urd emergency` calls the
   retention function directly, not through the planner. The function takes explicit
   `pinned: &HashSet<SnapshotName>` and `latest: &SnapshotName` as mandatory arguments.
   Callers cannot pass an empty pinned set unless they explicitly queried and found none.
3. The executor re-checks pin files before every delete (existing behavior, unchanged).

ADR-107 (fail-closed deletions) is preserved: if pin file reads fail, the snapshot is kept.
The `emergency_retention()` function must treat a read error on the pinned set as "keep
everything pinned-looking" rather than "nothing is pinned."

### Automatic emergency in the invisible worker

The nightly timer calls `plan()`, which already checks space. The change: before calling
`plan()`, check if any snapshot root is *critically* below threshold (below 50% of
`min_free_bytes`). If so, run emergency retention on that root first.

The 50% threshold is a conservative trip point: if the user configured 10 GB as the
minimum, critical means less than 5 GB free. This gives a meaningful buffer between
"normal space pressure" (tightened hourly window) and "emergency" (deep cut).

```
Automatic emergency path (nightly timer / Sentinel on drive mount):

1. For each snapshot root, check free bytes vs min_free_bytes.
2. If free_bytes < min_free_bytes * 0.5 (critical threshold):
   a. Compute emergency_retention() for all subvolumes on that root.
   b. Execute deletions via the existing executor delete path.
   c. Log: "Emergency retention freed X GB before backup."
   d. Emit SentinelAction::NotifyEmergencyRetention { root, freed_bytes }.
3. Proceed with normal plan() + execute().
```

If emergency retention fails to free enough space (snapshot root still critical after
deletion), the backup for affected subvolumes is skipped with a clear reason recorded:
`"emergency retention insufficient — manual intervention needed"`. This is consistent with
ADR-109: runtime space conditions skip per-unit and report rather than aborting.

### Sentinel integration

The Sentinel (pure state machine in `sentinel.rs`) gains:

- A new `SentinelAction::NotifyEmergencyRetention { root: PathBuf, freed_bytes: u64 }`.
- The runner (`sentinel_runner.rs`) detects critical space during the `AssessmentTick`
  by calling the same check used by the nightly timer. If critical, it emits the
  emergency action before the normal `Assess`.

Sentinel does not trigger emergency retention autonomously between backup runs — it
notifies the user and lets the next scheduled backup run execute it. The user should not
be surprised by silent deletion of 39 snapshots at 2 AM because they plugged in a drive.
Automatic emergency retention runs only as part of the scheduled backup workflow, not on
arbitrary Sentinel ticks.

### `urd doctor` integration

Doctor gains a space trend warning: if `free_bytes < min_free_bytes * 2.0` (approaching
threshold), emit a warning:

```
WARN snapshot root ~/.snapshots/home: 5.1 GB free, threshold 10 GB
     Space pressure active. Emergency retention may trigger on next backup.
     Run `urd emergency` to recover space now.
```

This is a pure check — `preflight.rs` or `commands/doctor.rs` calls
`drives::filesystem_free_bytes()` and compares. No new abstraction needed.

## Architecture

### New pure function: `retention::emergency_retention()`

```rust
/// Compute the minimal keep set for emergency space recovery.
///
/// Keeps: the single newest snapshot per subvolume, plus all pinned snapshots.
/// Returns all other snapshots as candidates for deletion.
///
/// Safety invariants (ADR-106, ADR-107):
/// - `latest` must be the actual newest snapshot — caller must sort and verify.
/// - `pinned` must be the result of a pin-file read — caller must not pass empty
///   when pin files are unreadable (treat read failure as keep-all-pinned).
/// - If `pinned` read failed, caller should pass a HashSet containing all
///   snapshots with names that look like they could be chain parents.
#[must_use]
pub fn emergency_retention(
    snapshots: &[SnapshotName],
    latest: &SnapshotName,
    pinned: &HashSet<SnapshotName>,
) -> RetentionResult
```

This function is independent of `now`, `config`, and `space_pressure` — it has no time
windows, no configuration inputs, and no space checks. It is purely structural: keep the
ends, keep the pins, delete the middle.

### New command: `src/commands/emergency.rs`

The handler:
1. Loads config (same path as all other commands).
2. For each snapshot root, calls `drives::filesystem_free_bytes()` to get current space.
3. For each snapshot root below threshold, calls `plan::FileSystemState::pinned_snapshots()`
   and `plan::FileSystemState::local_snapshots()` to enumerate candidates.
4. Calls `retention::emergency_retention()` with real data.
5. Prints the assessment and preview (see interaction above).
6. Prompts for confirmation. On `n` or Ctrl-C, exits cleanly.
7. Executes deletions through the existing executor delete path (not `plan()` — direct
   delete calls to `BtrfsOps::delete_snapshot()`).
8. Reports freed space.

The command does not call `plan()`. It bypasses the planner and calls the executor's
delete primitive directly. This is appropriate because emergency is not a backup run — it
is a maintenance action on existing snapshots. The planner's interval logic and operation
ordering are irrelevant here.

### Executor reuse

`executor.rs` already has a `DeleteSnapshot` operation path. `urd emergency` will reuse
the same deletion logic, with the same pin re-check. No new executor surface needed — the
emergency command constructs `PlannedOperation::DeleteSnapshot` items from the
`emergency_retention()` result and passes them to the executor's existing delete path.

Alternatively, the command can call `BtrfsOps::delete_snapshot()` directly (as `urd get`
does for individual file restoration). Both approaches are valid. The executor path is
preferred because it preserves the pin re-check invariant (ADR-106 layer 3) without
duplicating it.

### Nightly timer / backup command integration

`src/commands/backup.rs` (which drives the nightly timer) gains a pre-flight space check:

```rust
// Before calling plan():
for each snapshot root in config:
    let free = real_fs.filesystem_free_bytes(&root)?;
    let min_free = subvols_on_root.min_free_bytes.max();
    if free < min_free / 2 {
        run_emergency_retention(&root, &config, &btrfs, &state)?;
    }
```

`run_emergency_retention()` is a free function in `commands/backup.rs` (or a shared helper
in `commands/mod.rs`) that encapsulates: enumerate snapshots → call
`retention::emergency_retention()` → execute deletes → log → notify.

### Module map

| Module | Changes | ADR gate |
|--------|---------|----------|
| `retention.rs` | Add `emergency_retention()` pure function | None — additive |
| `commands/emergency.rs` | New command handler | None — new command |
| `commands/backup.rs` | Pre-flight critical space check, call emergency retention | None — internal flow change |
| `sentinel.rs` | Add `SentinelAction::NotifyEmergencyRetention` variant | None — additive |
| `sentinel_runner.rs` | Emit notification when backup auto-emergency runs | None — additive |
| `commands/doctor.rs` or `preflight.rs` | Space trend warning at 2x threshold | None — advisory only |
| `main.rs` or CLI entry | Register `urd emergency` subcommand | None |

**Modules NOT touched:** `plan.rs`, `config.rs`, `types.rs`, `awareness.rs`, `chain.rs`,
`state.rs`, `btrfs.rs` — no changes to backup logic, config schema, or data contracts.

## Safety constraints (explicit)

1. **Never delete pinned snapshots.** `emergency_retention()` signature forces callers
   to pass pin data. It never treats an absent pin set as "no pins."

2. **Never delete the latest snapshot per subvolume.** `emergency_retention()` requires
   `latest: &SnapshotName` as a mandatory argument. It is always kept.

3. **Deletions fail closed (ADR-107).** If a pin file read fails during the emergency
   command, the affected snapshot is kept. The function errs toward over-keeping.

4. **No silent automatic deletion outside backup runs.** Sentinel does not trigger
   emergency retention on arbitrary ticks. Only the scheduled backup workflow runs it
   automatically.

5. **Preview before action.** `urd emergency` always shows what will be deleted and
   requires explicit `y` confirmation. There is no `--force` flag that bypasses the
   prompt in v1.

6. **Executor pin re-check (ADR-106 layer 3).** Whether called from `urd emergency` or
   the nightly timer, the executor re-checks pin files immediately before each delete.

## Experience design

### The framing matters

This command exists because Urd can get into a stuck state. The interaction must not make
the user feel like they did something wrong. The framing:

- Name it `emergency`, not `cleanup` or `gc`. It is a signal that something unusual is
  happening, and Urd is offering a way out.
- Lead with the diagnosis: show the space numbers, then the snapshot counts, then the
  proposed solution. The user should understand the problem before they see the solution.
- Be precise about what will be kept: "Your newest snapshot and all chain parents will be
  preserved." The user needs to trust that the delete is not destroying everything.
- Report the outcome in terms that answer "is my data safe?": not "39 operations
  completed" but "Freed 8.2 GB. 9 snapshots remain."

### What happens when there's no crisis

`urd emergency` with no crisis should not feel like a dead end. It should reassure:

```
No crisis detected. All snapshot roots are within their free-space thresholds.
  ~/.snapshots/home  — 12.4 GB free (threshold: 10 GB)  OK
  ~/.snapshots/root  — 22 GB free (threshold: 5 GB)     OK
```

### Error paths

- **Permission denied on snapshot directory:** "Cannot read snapshot directory — verify
  Urd's btrfs sudoers configuration."
- **btrfs delete fails:** Report which snapshots could not be deleted, continue with
  others. Log the error. Consistent with ADR-109 (isolate failures, don't abort).
- **Space still critical after emergency retention:** "Emergency retention freed 2.1 GB,
  but the snapshot root is still below threshold (3.9 GB free, need 10 GB). Only pinned
  and latest snapshots remain — manual intervention is needed." Then print the surviving
  snapshots so the user knows what is there.

## ADR gates

No new ADRs are required. This design operates within existing constraints:

- ADR-100: The planner is bypassed for the `urd emergency` command (it is a maintenance
  action, not a backup run). The backup command's pre-flight emergency path uses the
  planner normally after emergency retention completes.
- ADR-106: Defense-in-depth pin protection is preserved by design. The `emergency_retention()`
  function signature enforces it structurally.
- ADR-107: Fail-closed deletions — emergency retention errs toward keeping.
- ADR-108: `emergency_retention()` is a pure function. The command handler does I/O.
- ADR-109: Runtime space failures skip per-unit and report. Emergency mode follows the
  same contract for post-emergency space still insufficient.

The only judgment call: bypassing `plan()` in `urd emergency` is an intentional deviation
from the normal config → plan → execute flow. The rationale is that emergency is not a
backup run — it is a maintenance action on snapshot history. The planner knows nothing
about "thin to minimum" semantics; `emergency_retention()` does. This is not a violation
of ADR-100 because ADR-100's invariant is "the planner never modifies anything," and
emergency retention is not modifying the planner.

## Sequencing

This feature is independent of all open UPIs. It does not depend on UPI 010-a, 011, or
012. It can be built at any time.

**Recommended:** After the v0.10.0 test session. If the test session reveals space issues,
this becomes urgent. If it reveals nothing, build it as part of the next feature batch.

Internally, build in this order:
1. `retention::emergency_retention()` + tests (pure, no dependencies)
2. `urd emergency` command (uses retention + executor)
3. Backup pre-flight emergency path (uses same retention + executor)
4. Sentinel notification action (additive, low risk)
5. Doctor space trend warning (advisory, last)

## Effort estimate

**~1 session.** Breakdown:
- `retention::emergency_retention()` + ~6 unit tests: 0.15 session
- `urd emergency` command (assessment, preview, confirm, execute, report): 0.35 session
- Backup pre-flight emergency path + integration: 0.25 session
- Sentinel action + runner wiring: 0.1 session
- Doctor space trend warning: 0.05 session
- Test coverage pass: 0.1 session

The mechanism is simple. The experience needs care — the interaction design must feel
deliberate and trustworthy, not alarming. Plan for one revision of the output format after
seeing it in a terminal.

## Rejected alternatives

### `urd backup --emergency-thin`
A flag on the backup command rather than a separate command. Rejected because:
- The interactive confirmation doesn't belong inside a backup run.
- The user invoking the crisis path is in a different mental state than the user running
  a backup. A separate command makes the intent explicit and the surface discoverable.

### Auto-emergency on every space_pressure detection (not just critical)
Running emergency retention whenever `free_bytes < min_free_bytes` would fire too easily.
Space pressure is a normal operating condition — the hourly thinning handles it. Emergency
retention is a deep cut that destroys months of history. The 50% threshold gates it to
genuine crises.

### Configurable emergency retention policy
Allowing users to configure `emergency_keep_count = 5` (keep more than just latest + pins).
Rejected: the value of emergency retention is its simplicity and predictability. A
configurable keep count adds surface and decision load. The invariant "keep the latest and
the chain parents" is correct for all realistic scenarios — the chain parents are exactly
what's needed for the next send, and the latest is the freshest local history.

### Keep multiple recents per subvolume (e.g., last 3)
More conservative than "latest only." Rejected: if the user is in an emergency, they have
very little space. Keeping 3 instead of 1 triples the minimum space needed. The pin-file
chain parents (which can number 2-4 per drive) already provide a meaningful floor. The
goal is recovery, not a smaller graduated retention.

## Assumptions

1. The executor's `DeleteSnapshot` path (or direct `BtrfsOps::delete_snapshot()`) is
   sufficient for the emergency command without modification.
2. `drives::filesystem_free_bytes()` is accurate enough for the 50% critical threshold
   decision. No new space measurement primitive is needed.
3. The Sentinel's existing `AssessmentTick` cycle is frequent enough to detect critical
   space within a reasonable window. No new Sentinel event type is needed.
4. A single `y` confirmation is sufficient UX for the emergency command. The preview is
   explicit enough that the user understands the scope before confirming.
