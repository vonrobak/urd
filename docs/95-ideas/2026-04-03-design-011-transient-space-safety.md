---
upi: "011"
status: proposed
date: 2026-04-03
---

# Design: Transient Space Safety (UPI 011)

> **TL;DR:** Fix the broken promise that transient subvolumes won't accumulate local
> snapshots. Three reinforcing changes: (1) skip local snapshot creation for transient
> subvolumes when no configured drive is mounted, (2) hard-cap transient subvolumes at
> 1 local snapshot, (3) detect and clear stale pin files that reference deleted snapshots.
> This is a production bug fix — five NVMe exhaustion incidents in ten days.

## Problem

`local_retention = "transient"` promises: "delete local after send, keep only chain
parent." But transient is a deletion policy, not a creation policy. The planner creates
local snapshots unconditionally for all subvolumes in `plan_local_snapshot()` — it has
no awareness of transient retention.

Three compounding failures:

1. **Creation is transient-unaware.** `plan_local_snapshot()` creates snapshots for
   transient subvolumes on every run that passes interval/space checks. Manual runs
   (`urd backup` without `--auto`) bypass interval checks entirely
   (`skip_intervals: !args.auto`), so every manual run creates a new snapshot.

2. **Unsent protection blocks transient cleanup.** When no configured drive is mounted,
   snapshots can't be sent. `plan_local_retention()` protects all snapshots newer than
   the oldest pin (or all snapshots if no pin exists). Transient cleanup never fires
   because there's nothing to clean up — everything is protected.

3. **Pin files reference deleted snapshots.** After manual deletion of accumulated
   snapshots, pin files (`.last-external-parent-{DRIVE}`) point to non-existent
   snapshots. The planner classifies this as `ChainBroken` and autonomous mode skips
   the send entirely (`FullSendPolicy::SkipAndNotify`). The stale pin persists across
   runs — the chain stays broken until manual `--force-full`.

**Result:** On the 118GB NVMe hosting both htpc-root and htpc-home, three manual
`urd backup` runs created three htpc-root snapshots. External drive wasn't mounted.
Unsent protection kept all three. NVMe approached exhaustion. User manually deleted
snapshots, breaking the incremental chain. Fifth incident in ten days.

## Proposed Design

Three changes that reinforce each other. Each is independently valuable but together
they make transient subvolumes genuinely safe on space-constrained volumes.

### Change 1: Drive-availability preflight for transient subvolumes

**Where:** `plan.rs`, in the main `plan()` loop, before the local operations block.

**Logic:** For transient subvolumes (`subvol.local_retention.is_transient()`), check
whether at least one configured drive is available before creating a local snapshot.
If no drive is available, skip the entire local operations block (no create, no
retention) and log a skip reason.

```
// Pseudocode — in plan() loop, before `if !filters.external_only`
if subvol.local_retention.is_transient() && !filters.external_only {
    let any_drive_available = config.drives.iter()
        .filter(|d| subvol.drives.as_ref().map_or(true, |allowed| allowed.contains(&d.label)))
        .any(|d| matches!(fs.drive_availability(d), DriveAvailability::Available));

    if !any_drive_available && !local_snaps.is_empty() {
        // At least one snapshot exists (chain parent) — no need to create more
        skipped.push((subvol.name.clone(), "transient: no configured drive mounted".into()));
        // Still run retention to clean up any excess snapshots
        plan_local_retention(config, subvol, &local_dir, &local_snaps, now, &pinned, fs, &mut operations);
        // Skip to external operations (which will also skip — drives aren't mounted)
        continue to external block;
    }
    // If no snapshots exist at all, allow creation of the first one
    // (needed for the very first backup before any drive has been plugged in)
}
```

**Why this module:** The planner decides what operations to run (ADR-100). Drive
availability is already queried through `fs.drive_availability()` in the external
operations block. Using it in the local block for transient subvolumes is a natural
extension — the planner is asking "is this snapshot useful?" not "is the drive ready
for I/O?"

**Edge case — first-ever snapshot:** If `local_snaps` is empty and no drive is
available, we still allow creation of one snapshot. This ensures the first backup
works even if the user hasn't plugged in the drive yet. Change 2 (cap of 1) prevents
accumulation from this edge case.

**Edge case — `--external-only` filter:** If the user runs `urd backup --external-only`,
the local operations block is already skipped. No interaction with this change.

**Edge case — `--force` or specific subvolume:** Even with `--force` or
`--subvolume htpc-root`, the preflight still applies. Force overrides *intervals*,
not *drive availability*. A forced snapshot on a transient subvolume with no drive
mounted is still useless.

### Change 2: Transient snapshot cap of 1

**Where:** `plan_local_snapshot()` in `plan.rs`.

**Logic:** After the space guard check, before interval logic, add a transient cap
check. If the subvolume is transient and at least one local snapshot already exists,
skip creation regardless of interval, force, or skip_intervals.

```
// In plan_local_snapshot(), after space guard, before interval check
if subvol.local_retention.is_transient() && !local_snaps.is_empty() {
    skipped.push((
        subvol.name.clone(),
        "transient: snapshot exists (cap of 1)".into(),
    ));
    return;
}
```

**Why this is defense in depth, not redundant:** Change 1 prevents creation when no
drive is available. Change 2 prevents creation when a snapshot already exists,
regardless of drive state. Together they handle:

- Drive mounted but previous snapshot not yet sent → cap prevents second
- Drive not mounted → preflight prevents creation AND cap prevents accumulation
- Race condition: drive unmounts between preflight check and snapshot creation →
  cap still prevents accumulation on next run
- Bug in preflight logic → cap still prevents accumulation

**Interaction with interval logic:** The cap check comes BEFORE the interval check.
For transient subvolumes, the interval is irrelevant when a snapshot already exists —
the only question is "was it sent?" The interval logic is preserved for the case where
`local_snaps` is empty (first snapshot timing).

**Parameter change:** `plan_local_snapshot()` needs access to `subvol.local_retention`.
It currently receives `subvol: &ResolvedSubvolume` so this is already available — no
signature change needed.

### Change 3: Pin file self-healing

**Where:** `executor.rs`, in the send execution path, before attempting incremental send.
Also `chain.rs` for a pin validation helper.

**Logic:** Before an incremental send, verify that the pin file's referenced snapshot
exists as a local subvolume. If it doesn't:

1. Log a warning: "chain parent {name} no longer exists locally — clearing stale pin"
2. Delete the stale pin file
3. Reclassify the send as `FullSendReason::ChainBroken` (already happens in planner,
   but the pin file now gets cleaned up so next run sees `NoPinFile` instead of
   repeating `ChainBroken`)

**New function in `chain.rs`:**

```rust
/// Validate that a pin file's referenced snapshot exists on the local filesystem.
/// Returns `Ok(true)` if valid, `Ok(false)` if stale (and removes the pin file).
pub fn validate_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
) -> Result<bool> {
    let Some(pin) = read_pin_file(local_snapshot_dir, drive_label)? else {
        return Ok(true); // No pin file — nothing to validate
    };
    let snap_path = local_snapshot_dir.join(pin.name.as_str());
    if snap_path.exists() {
        Ok(true)
    } else {
        // Stale pin — referenced snapshot was deleted
        let pin_path = match pin.source {
            PinSource::DriveSpecific => {
                local_snapshot_dir.join(format!(".last-external-parent-{drive_label}"))
            }
            PinSource::Legacy => {
                local_snapshot_dir.join(".last-external-parent")
            }
        };
        log::warn!(
            "Stale pin file {} references non-existent snapshot {} — removing",
            pin_path.display(),
            pin.name,
        );
        std::fs::remove_file(&pin_path).map_err(|e| {
            UrdError::Io(format!("failed to remove stale pin {}: {e}", pin_path.display()))
        })?;
        Ok(false)
    }
}
```

**Where to call it:** Two options considered:

- **Option A: In executor, before send.** The executor already re-reads pin files for
  transient cleanup (executor.rs:887). Adding a validation call before send is natural
  and keeps the "filesystem is truth" check close to the I/O.

- **Option B: In planner, when reading pins.** This would make the planner's
  `ChainBroken` classification also clear the pin. But the planner is a pure function
  (ADR-108) — it must not delete files.

**Decision: Option A.** The planner classifies; the executor heals. The planner already
correctly produces `ChainBroken` when the parent is missing. The executor's job is to
execute operations AND maintain filesystem invariants. Pin file cleanup is I/O — it
belongs in the executor.

**Call site:** In `execute_send()`, before the send attempt, after resolving the parent
path. If validation returns false (stale pin removed), the send should proceed as full
(the planner already classified it as full — this just cleans up the pin so the next
run doesn't see the same stale state).

**ADR-102 alignment:** "Filesystem is truth." If the filesystem says a snapshot doesn't
exist, the pin file is lying. Self-healing corrects the lie.

**ADR-107 alignment:** "Deletions fail closed." We only delete the *pin file* (metadata),
not a snapshot. And we only delete it when we can confirm the referenced snapshot
doesn't exist. This is fail-closed: we're refusing to trust a pin that points to nothing.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `plan.rs` | Drive-availability preflight for transient in `plan()` loop; transient cap of 1 in `plan_local_snapshot()` | Unit tests with `MockFileSystemState`: transient + no drives = no create; transient + 1 existing = no create; transient + 0 existing = create; transient + drive available = create; non-transient unaffected. ~8-10 new tests. |
| `chain.rs` | New `validate_pin_file()` function | Unit tests with `tempfile::TempDir`: valid pin → true; stale pin → false + file removed; no pin → true; legacy pin stale → removed. ~4-5 new tests. |
| `executor.rs` | Call `validate_pin_file()` before send; handle stale pin recovery | Integration-style tests with `MockBtrfs`: stale pin → full send proceeds; valid pin → incremental unchanged. ~3-4 new tests. |

**Modules NOT touched:** `types.rs`, `config.rs`, `retention.rs`, `awareness.rs`,
`voice.rs`, `output.rs`. No new types, no config changes, no output changes.

## Effort Estimate

**~0.5-1 session.** Calibrated against:
- UPI 004 (token gate): 1 module extended, 5 tests, half session → this is 3 modules,
  ~15-19 tests, slightly more
- UPI 008 (doctor pin-age): 2 modules, 6 tests, quarter session → this is more complex
  but same pattern (read state, validate, fix)

The changes are surgical — no new modules, no new types, no config changes. The bulk
of the work is tests.

## Sequencing

1. **Change 2 first (transient cap of 1).** Simplest change, immediately prevents
   accumulation. One check in `plan_local_snapshot()`, several tests. Can be verified
   immediately: run `urd plan` with transient subvolume + existing snapshot → skip.

2. **Change 1 second (drive preflight).** Depends on understanding the cap behavior to
   write correct edge-case tests (especially the "first snapshot" case). Slightly more
   complex because it queries drive availability in a new location.

3. **Change 3 last (pin self-healing).** Independent of changes 1 and 2 but less urgent
   (the planner already handles stale pins gracefully via `ChainBroken`). This change
   makes recovery automatic rather than requiring `--force-full`.

**Risk ordering rationale:** Change 2 has the highest leverage and lowest risk. If we
ship only change 2, accumulation is prevented. Changes 1 and 3 are defense in depth and
chain repair — important but not blocking.

## Architectural Gates

**None.** All changes operate within existing module boundaries and existing architectural
invariants:

- Planner remains pure (changes 1 and 2 add conditions, not I/O)
- Executor performs I/O (change 3 deletes a pin file, which is executor's domain)
- `BtrfsOps` is not touched
- No new on-disk contracts — pin file format unchanged, snapshot names unchanged
- No config changes — existing `local_retention = "transient"` gains correct behavior

## Rejected Alternatives

### "External-only mode" as a new config concept

Brainstorm idea #2. Instead of making transient smarter, introduce `mode = "external-only"`
that explicitly means "no local snapshots." Rejected for this design because:
- Adds config surface area for a problem solvable without it
- Requires a config migration (existing `local_retention = "transient"` would need
  updating)
- The config vocabulary change is better addressed in UPI 010 (config schema v1)
- This design fixes the bug; the vocabulary improvement is a separate concern

### Block manual `urd backup` for transient subvolumes

Brainstorm idea #8. Rejected because it punishes the user. `urd backup` should back up
everything — the system should make that safe, not restrict it. The preflight + cap
achieve safety without restricting the user's commands.

### Aggressive transient retention (protect only newest unsent)

Brainstorm idea #16. Modifying unsent protection for transient subvolumes to only protect
the newest snapshot. Rejected because:
- Changes the safety invariant of unsent protection (currently protects ALL unsent)
- The cap of 1 achieves the same result (can't accumulate) without weakening protection
- Riskier: if the logic has a bug, you could delete an unsent snapshot

### Snapshot budget per filesystem

Brainstorm idea #3. A general-purpose mechanism for limiting snapshots per volume.
Rejected as over-engineering for this problem:
- Adds config complexity (`max_snapshots` or `max_exclusive_bytes`)
- Requires filesystem-level awareness the planner doesn't currently have
- The transient cap of 1 solves the specific problem without generalizing

## Assumptions

1. **`fs.drive_availability()` is callable from the local operations block.** The
   planner already calls it in the external operations block (plan.rs:186). No
   technical barrier to calling it earlier — it reads mount state, not btrfs state.

2. **A transient subvolume with 0 local snapshots should be allowed to create one.**
   The first-ever snapshot must be created before any send can happen. Without this
   exception, a new transient subvolume could never start its chain.

3. **Pin file deletion is safe when the referenced snapshot doesn't exist.** The pin
   file is metadata about chain state. If the chain parent is gone, the pin is a lie.
   Deleting it causes the next send to be full, which is the correct recovery.

4. **The planner's `ChainBroken` classification already handles the send correctly.**
   Verified in plan.rs:593-595 — when pin exists but parent is missing locally or
   externally, the send is classified as `ChainBroken`. The executor gates this in
   auto mode via `FullSendPolicy::SkipAndNotify`. Change 3 clears the stale pin so
   the NEXT run sees `NoPinFile` instead, which is a cleaner state.

5. **The space guard (`min_free_bytes`) remains as a last-resort safety net.** This
   design doesn't remove or modify the space guard. It adds prevention above the
   threshold — the space guard catches anything this design misses.

## Open Questions

### Q1: Should the transient cap apply when `--force` is used?

**Option A (cap always applies):** Even `urd backup --subvolume htpc-root` (which sets
`force = true`) respects the cap. Rationale: force overrides intervals, not safety
invariants. The cap exists to prevent space exhaustion, which force doesn't change.

**Option B (force overrides cap):** `--force` or `--subvolume` bypasses the cap,
allowing a second transient snapshot. Rationale: the user explicitly asked for this
specific subvolume — honor the request.

**Recommendation: Option A.** Force already doesn't override the space guard (by
explicit design — see plan.rs:287 comment). The cap is the same class of safety check.
A forced transient snapshot that can't be sent is still useless and still dangerous.

### Q2: Should pin self-healing run for all subvolumes or only transient?

**Option A (all subvolumes):** Any subvolume can have stale pins from manual intervention.
Self-healing should be universal.

**Option B (transient only):** Transient subvolumes are the ones most likely to have
manual deletions (because they're the ones causing space problems). Limit blast radius.

**Recommendation: Option A.** The validation is cheap (one `path.exists()` check per
drive per subvolume) and the benefit applies universally. A graduated subvolume can
also have a stale pin if someone manually deletes a snapshot.

### Q3: What skip reason text should the preflight produce?

The skip reason appears in `urd plan` and `urd backup` output. Options:

- `"transient: no configured drive available"` — precise but jargon-heavy
- `"no drive for send — skipping transient snapshot"` — clearer intent
- `"htpc-root: drive WD-18TB1 not mounted (transient — skipping local snapshot)"` —
  names the specific drive

**Recommendation:** Include the drive name(s) so the user knows what to plug in. The
skip message is guidance, not just status.
