---
upi: "014"
status: proposed
date: 2026-04-03
---

# Design: Skip Unchanged Subvolumes (UPI 014)

> **TL;DR:** Before creating a snapshot, compare the source subvolume's BTRFS generation
> against the generation recorded in the most recent local snapshot. If equal, nothing has
> changed — skip snapshot creation silently and say why. A `--force-snapshot` flag provides
> an escape hatch for forensic use cases. No config field; this is the right default behavior.

## Problem

Urd creates a new snapshot whenever the snapshot interval elapses, regardless of whether
the subvolume has changed since the last snapshot. For subvolumes with infrequent writes —
docs, photos, music, config archives — this produces long runs of identical snapshots that
waste space and pollute retention.

**Concrete impact:** A docs subvolume snapshotted nightly with `daily = 30` accumulates 30
snapshots. If the user hasn't touched the directory in two weeks, 14 of those 30 are bit-for-bit
identical to the one before them. The retention window "30 most recent snapshots" silently
becomes "coverage of the last 16 active days." Users who trust the retention semantics get
less history than expected.

**BTRFS already knows.** `btrfs subvolume show <path>` returns a `Generation: N` counter that
increments on every write or metadata change to the subvolume. A snapshot records its
generation at the moment of creation. If the source generation equals the latest snapshot's
generation, the subvolume has not been modified.

## Design

### Default behavior — no config required

Generation comparison is Urd's default snapshot decision. No `snapshot_on_change` field,
no opt-in. Smart behavior is the baseline; the CLI provides an escape hatch.

Rationale: A config field forces users to reason about "do I want change-detection for this
subvolume?" That is not a question users should carry. Urd should know. The only exception
is deliberate override for forensic or compliance use — and that is a CLI concern, not a
config concern.

### Mechanism

`btrfs subvolume show <path>` already runs for other inspection tasks. The `Generation`
line:

```
Generation:             12847
```

A snapshot's generation is the source's generation at the moment the snapshot was taken.
Urd stores snapshot names (which encode timestamps, not generations), so the latest
snapshot's generation is obtained by calling `btrfs subvolume show` on the snapshot
directory itself, not by parsing the name.

**Decision point in `plan_local_snapshot()`:**

1. Interval check passes (or is bypassed via `force` / `skip_intervals`).
2. Fetch source generation via `fs.subvolume_generation(&subvol.source)`.
3. Fetch latest-snapshot generation via `fs.subvolume_generation(&latest_snap_path)`.
4. If equal and not forced: emit skip reason, return.
5. If not equal (or no prior snapshot): proceed to `CreateSnapshot`.

If either generation call fails (btrfs error, path missing), proceed as if changed.
Fail open — missing data is not a reason to skip a snapshot.

### CLI override: `--force-snapshot`

```
urd backup --force-snapshot
urd plan --force-snapshot
```

Forces snapshot creation for all subvolumes regardless of generation comparison. Does
NOT override the space guard (`min_free_bytes`) — a forced snapshot on a full filesystem
is still catastrophic. Consistent with existing `force` semantics in the planner.

Use cases:
- Forensic capture: "snapshot everything right now, even if unchanged, to establish a
  known-good baseline."
- Debugging: verify that generation detection is working correctly by forcing creation
  and checking the new generation.
- Compliance: some environments require time-correlated snapshots for audit trails.

`--force-snapshot` maps to `PlanFilters::force_snapshot: bool`. The existing `force` flag
(per-subvolume via `--subvolume`) is separate and orthogonal.

### Skip reason string

```
unchanged — no changes since last snapshot (21h ago)
```

The time elapsed since the last snapshot provides orientation: "21h ago" tells the user
the subvolume was active enough to snapshot recently and is now idle. Consistent with the
existing `interval not elapsed (next in ~14h)` pattern — elapsed time as context.

### Display in plan output

Unchanged subvolumes emit a distinct `SkipCategory::Unchanged` classification. The voice
layer renders them separately from interval skips, space skips, and drive skips:

```
  [UNCHANGED]  docs       — no changes since last snapshot (21h ago)
  [UNCHANGED]  music      — no changes since last snapshot (3d ago)
  [SKIP]       home       — interval not elapsed (next in ~6h)
```

Unchanged is positive information — "Urd checked, nothing to do, all is well." It should
read as confident, not apologetic.

### Retention semantics clarification

Generation-skip changes what `daily = 30` means in practice. Previously: "the 30 most
recent time-correlated snapshots (one per day)." After this change: "the 30 most recent
snapshots, each representing a distinct change point." On an active subvolume the behavior
is identical. On a quiet subvolume, the retention window now covers more time — 30
snapshots span more calendar days because idle days produce no new snapshots.

This is strictly better: retention now represents 30 actual change points rather than
30 calendar slots, many of which may be duplicates. Users get more history for the same
retention budget on quiet subvolumes. No retention config changes are needed; the semantics
naturally improve.

External sends: if no new local snapshot is created because the subvolume is unchanged,
the most recent existing snapshot is still eligible for external send if the send interval
has elapsed. The "skip local snapshot" decision is independent of the "send external"
decision. Sends continue to protect existing snapshots against the external send interval.

### Edge cases

**No prior snapshots:** No generation to compare against. Create the first snapshot
unconditionally. Correct — the subvolume exists and we have no record of it.

**Generation fetch fails:** Proceed as if changed. Fail open: missing generation data is
not a safe reason to skip a snapshot. Log at `warn!` level.

**Metadata-only changes (chmod, xattr, rename):** BTRFS increments generation on all
mutations, including metadata-only changes. These DO trigger snapshots. This is correct —
metadata changes are real changes; a permissions audit trail is valid data.

**Subvolume path missing at snapshot time:** Already handled upstream (space guard, path
checks). Generation fetch failing for a missing path falls into the "fail open" case.

**Clock skew with future snapshot:** Existing warning logic in `plan_local_snapshot()` is
unchanged. Generation check is independent of timestamp.

**`skip_intervals = true` (manual `urd backup`):** Generation check still applies unless
`--force-snapshot` is set. Manual runs are still subject to "nothing changed" logic —
skipping an unchanged subvolume on a manual run is correct and saves a pointless operation.

## Architecture

### `FileSystemState` trait extension

Add one method to `src/plan.rs`:

```rust
/// Get the BTRFS generation number for a subvolume or snapshot directory.
/// Returns Err if the path is not a BTRFS subvolume or btrfs is unavailable.
fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64>;
```

`SystemFileSystemState` implements this by calling `btrfs subvolume show <path>` and
parsing the `Generation:` line. The call goes through `BtrfsOps` — no direct subprocess
spawning in `plan.rs`.

`MockFileSystemState` gains:

```rust
pub generations: HashMap<PathBuf, u64>,
```

Tests set generations directly:

```rust
mock.generations.insert(subvol_source.clone(), 1000);
mock.generations.insert(latest_snap_path.clone(), 1000); // equal → skip
```

### `BtrfsOps` trait extension

Add to `src/btrfs.rs`:

```rust
fn subvolume_generation(&self, path: &Path) -> Result<u64>;
```

`RealBtrfsOps` implements this via `sudo btrfs subvolume show <path>`, parses stdout for
`Generation:` field. The error type maps through `UrdError` as normal.

### Planner changes (`src/plan.rs`)

`plan_local_snapshot()` gains:

1. After the interval check passes (before `CreateSnapshot`):
   - Retrieve the latest snapshot path from `local_snaps`.
   - If one exists, call `fs.subvolume_generation()` on both source and latest snapshot.
   - On success: compare; if equal and not `force_snapshot`, push skip reason and return.
   - On any error: log warning, proceed to create.

2. The existing `force` parameter is unchanged (`--subvolume` targeting).
3. New `force_snapshot: bool` parameter is added to `plan_local_snapshot()` and threaded
   through `PlanFilters`.

### `PlanFilters` extension

```rust
pub struct PlanFilters {
    // ... existing fields ...
    /// When true, bypass generation comparison — create snapshot even if unchanged.
    /// Does NOT override min_free_bytes space guard.
    pub force_snapshot: bool,
}
```

### Output and voice changes

`SkipCategory` gains a new variant:

```rust
Unchanged,
```

`SkipCategory::from_reason()` gains a match arm:

```rust
r if r.starts_with("unchanged") => SkipCategory::Unchanged,
```

`voice.rs` renders `Unchanged` skips with `[UNCHANGED]` tag, visually grouped or
interleaved with other per-subvolume output. Distinct from `[SKIP]` to communicate the
positive meaning: "checked, nothing to do."

### CLI wiring

`src/commands/backup.rs` and `src/commands/plan_cmd.rs` gain `--force-snapshot` flag,
mapped to `PlanFilters::force_snapshot`.

## Module Map

| Module | Changes | Test count |
|--------|---------|------------|
| `src/btrfs.rs` | Add `subvolume_generation()` to `BtrfsOps` trait + `RealBtrfsOps` impl | ~2 unit tests: parse valid output, handle missing field |
| `src/plan.rs` | `FileSystemState::subvolume_generation()`; generation comparison in `plan_local_snapshot()`; `PlanFilters::force_snapshot`; `MockFileSystemState::generations` | ~8 tests (see below) |
| `src/output.rs` | `SkipCategory::Unchanged` variant + `from_reason()` arm + `classify_all_N_patterns` update | ~2 tests: classify unchanged, exhaust all patterns |
| `src/voice.rs` | Render `[UNCHANGED]` for `SkipCategory::Unchanged` | ~1 test: voice renders correctly |
| `src/commands/backup.rs` | `--force-snapshot` flag → `PlanFilters::force_snapshot` | ~0 new tests (integration path) |
| `src/commands/plan_cmd.rs` | Same flag threading | ~0 new tests |

**Total: ~13 new tests.**

### Key planner tests (plan.rs)

1. `skip_when_generation_equal` — source gen == snapshot gen → skip, reason contains "unchanged"
2. `create_when_generation_different` — source gen > snapshot gen → `CreateSnapshot`
3. `create_when_no_prior_snapshot` — no snapshots exist → `CreateSnapshot` (no generation to compare)
4. `create_when_generation_fetch_fails` — `subvolume_generation` returns `Err` → proceed, log warn
5. `force_snapshot_overrides_generation_check` — `force_snapshot: true` → `CreateSnapshot` even if equal
6. `force_subvolume_overrides_generation_check` — existing `force` flag also overrides (consistent)
7. `skip_intervals_still_checks_generation` — `skip_intervals: true` + equal gen → skip (manual run, unchanged)
8. `sends_proceed_when_snapshot_skipped` — generation skip on local does not block external send

## Effort Estimate

**~0.5 session.** Calibrated against UUID fingerprinting (1 new module, 10 tests, 1 session):
this feature adds one trait method (no new module), ~13 tests, and touches 6 files with
small, localized changes. The parsing of `btrfs subvolume show` output is trivial — the
format is stable and well-tested. The planner change is a 10-line addition after the
existing interval check. Output and voice are additive. No migration, no config changes,
no ADR gate.

Risk factors: `btrfs subvolume show` on a snapshot directory must return the generation
at the time the snapshot was taken, not the current time. This is correct BTRFS behavior
(snapshots are read-only; their generation is fixed at creation). Integration test should
verify this assumption before relying on it. Mark as `#[ignore]` per convention.

## Sequencing

Independent of all active UPIs (010, 010-a, 011, 012, 013). No shared files with 010/010-a
beyond `config.rs` (which this design does not touch). Can ship in any order.

**Recommended:** After 011 (transient space safety) ships. UPI 011 is a production bug fix
with five incidents behind it; 014 is a quality-of-life improvement. Fix first, polish second.

## Rejected Alternatives

### Config field `snapshot_on_change = true`

Would require users to reason about which subvolumes benefit from change detection. Rejected
because Urd should be smart by default. Config fields for "should Urd be smart?" add
cognitive overhead without proportional control.

### Store generation in SQLite state

Would allow generation comparison without calling `btrfs subvolume show` on each plan run.
Rejected because the filesystem is truth (ADR-102). Querying BTRFS directly is the correct
source of generation data. SQLite state is history, not ground truth — generation is a
live filesystem property.

### Compare snapshot content hashes instead of generation

Overkill and wrong direction. Generation is a cheap, reliable BTRFS primitive designed
for exactly this purpose. Content hashing would be orders of magnitude slower, require
reading snapshot data, and duplicate work BTRFS already does.

### Skip only in autonomous mode

Would preserve the current behavior in manual `urd backup` runs. Rejected because the
skip is correct regardless of invocation mode — if the subvolume hasn't changed, creating
a snapshot wastes space whether the run was manual or automated. `--force-snapshot` handles
the cases where the user genuinely wants a snapshot despite no changes.

## Assumptions

1. **`btrfs subvolume show` on a read-only snapshot returns the generation at snapshot
   creation time.** This is standard BTRFS behavior. The integration test should verify it.

2. **The `Generation:` field is stable across BTRFS kernel versions in the project's
   target range.** This field has been present since early BTRFS days and is not subject
   to churn. No special handling needed for older kernels.

3. **Generation comparison is sufficient for correctness.** Generation increments on every
   write and every metadata mutation. It cannot be reset without destroying the subvolume.
   There is no false-negative risk (generation equal, content changed) — only the theoretical
   false-positive (generation changed, content identical), which is harmless (creates an
   unnecessary snapshot rather than skipping a necessary one).
