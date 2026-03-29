# Design: D2+D3 — Estimated Send Sizes in Plan and Summary

**Status:** proposed
**Date:** 2026-03-29
**Source:** [Progress and Plan Display Brainstorm](2026-03-29-progress-and-plan-display.md)

## Summary

Surface estimated send sizes in the plan display and summary line. Each send operation
shows an approximate size (`~53 GB` for full sends, `last: 5.5 MB` for incrementals).
The summary aggregates into a total. The data already exists in SQLite — this feature
threads it through the output layer.

Target:
```
  htpc-home:
    [SEND] 20260329-0404-htpc-home -> WD-18TB (full, ~53 GB) + pin

  Summary: 6 sends (~623 GB total), 0 snapshots, 0 deletions, 20 skipped
```

## Module Changes

### `src/output.rs` — two new fields

- `PlanOperationEntry.estimated_bytes: Option<u64>`
- `PlanSummaryOutput.estimated_total_bytes: Option<u64>`

Both `Option` for cases where no estimate is available (first-ever send, no calibration).

### `src/commands/plan_cmd.rs` — size lookup

Expand signature: `build_plan_output(plan: &BackupPlan, fs_state: &dyn FileSystemState)`.
Both callers (`plan_cmd::run` and `backup::run`) already have `fs_state` in scope.

In `build_operation_entry`, query for each send using a three-tier fallback chain
(arch-adversary S1 — cross-drive fallback for drive swap scenarios):

- **SendFull:**
  1. `last_send_size(subvol, drive, "send_full")` — same drive history (most accurate)
  2. `last_send_size_any_drive(subvol, "send_full")` — cross-drive history (covers drive swaps)
  3. `calibrated_size(subvol)` — `du -sb` measurement (full sends only)
- **SendIncremental:**
  1. `last_send_size(subvol, drive, "send_incremental")` — same drive
  2. `last_send_size_any_drive(subvol, "send_incremental")` — cross-drive
  3. No calibrated fallback (calibration measures full subvolume, not incremental delta)

Label same-drive history as `~`, cross-drive as `~` (same confidence), calibrated as `~`
with `(est)` qualifier. Incrementals use `last:` prefix since sizes vary widely by delta.

The `last_send_size_any_drive()` infrastructure is already implemented in `state.rs` and
the `FileSystemState` trait in `plan.rs`.

Summary: sum all non-None `estimated_bytes`. If some sends lack estimates, render
qualified: `6 sends (~623 GB estimated for 4 of 6)`.

### `src/voice.rs` — render sizes

In `render_plan_interactive()`, append size to detail:
- Full with estimate: `(full, ~53 GB)`
- Incremental with history: `(incremental, parent: snap, last: 5.5 MB)`
- No estimate: unchanged

Summary line: `6 sends (~623 GB total)` or qualified variant.

### `src/commands/backup.rs` — dry-run path

Update to pass `&fs_state` to `build_plan_output()`.

## Data Flow

1. `plan_cmd::run()` / `backup::run()` both have `fs_state: RealFileSystemState`.
2. `build_plan_output(plan, &fs_state)` queries size using the three-tier fallback chain
   for each send operation.
3. Populates `PlanOperationEntry.estimated_bytes`.
4. Aggregates to `PlanSummaryOutput.estimated_total_bytes`.
5. `voice.rs` renders using `ByteSize` display wrapper.

## Test Strategy

- **Size lookup:** Mock `FileSystemState` returning known sizes. Cases: full with same-drive
  history, full with cross-drive fallback only, full with calibrated only, full with all
  three (same-drive wins), incremental with history, incremental with cross-drive fallback,
  send with no data. ~8 tests. Cross-drive fallback infrastructure already tested in
  `state.rs`.
- **Summary aggregation:** Partial data, all-None, all-present. ~3 tests.
- **Rendering:** Output includes `~53 GB`, `last: 5.5 MB`, no annotation for None. ~6 tests.
- **JSON serialization:** `estimated_bytes` as integer or null. ~1 test.

**Estimated: 16-18 tests.**

## Effort Estimate

Four modules changed (output.rs trivially, plan_cmd.rs moderately, voice.rs moderately,
backup.rs trivially). `FileSystemState` already in scope at both call sites. **One session.**

## Dependencies

None for implementation. Feature 4 (ETA) reuses the same size lookup logic — extract
to shared helper if both are implemented.

## Known Limitations

**Calibration size gap (arch-adversary open question 1):** `urd calibrate` uses `du -sb`
which measures filesystem apparent size, not btrfs send stream size. The send stream is
~10% larger due to btrfs metadata overhead. Real data from run #15:

| Subvolume | Calibrated (`du -sb`) | Actual send | Gap |
|-----------|----------------------|-------------|-----|
| subvol3-opptak | 3.1 TB | 3.4 TB | +9.7% |
| subvol5-music | 1.0 TB | 1.1 TB | +10% |

This is acceptable for v1: all sizes use `~` prefix communicating approximation, and
the existing 1.2x safety margin in space checks (plan.rs) partially compensates. A
future improvement would be to calibrate via `btrfs send --dry-run` (measures actual
stream size) instead of `du -sb`, but this is more complex and not needed now.

## Risks

**Stale calibration:** A calibrated size from months ago may be inaccurate. The `~`
prefix communicates approximation. Not worth adding staleness warnings in plan display.

## Alternatives Rejected

- **Adding estimated_bytes to PlannedOperation:** Architecturally cleaner but modifies
  a core enum used everywhere. The plan_cmd boundary approach avoids churn.

## Ready for Review

Focus on:
1. **History vs calibrated priority:** Design prefers history (last actual send_full) over
   calibrated (subvolume measurement). Rationale: history measures actual transfer bytes
   including btrfs overhead. Calibrated measures the subvolume. Verify this is correct.
2. **FileSystemState in plan_cmd:** Makes `build_plan_output` depend on DB state. Acceptable
   since plan_cmd is a wiring layer (commands/), not a pure module.
3. **Incremental size labeling:** "last: 5.5 MB" may mislead if workload changed. Consider
   omitting for incrementals entirely.
