# Design: P3 — ETA and Size Denominator for Full Sends

**Status:** proposed
**Date:** 2026-03-29
**Source:** [Progress and Plan Display Brainstorm](2026-03-29-progress-and-plan-display.md)

## Summary

During active transfers, show estimated total size and time remaining for full sends.
Extends Feature P1's `ProgressContext` with `estimated_bytes: Option<u64>`. Incremental
sends (unpredictable sizes) omit the denominator and ETA.

Target:
```
  [3/6] subvol2-pics: 23.1 GB / ~47.6 GB @ 178.3 MB/s  [2:10, ~2:17 left]
```

## Module Changes

### `src/commands/backup.rs` — extend ProgressContext + ETA

Extend `ProgressContext` (from Feature P1):
```rust
struct ProgressContext {
    subvolume_name: String,
    drive_label: String,
    send_type: &'static str,
    send_index: u32,
    total_sends: u32,
    estimated_bytes: Option<u64>,  // NEW
}
```

Pre-compute size estimates before execution:
```rust
type SizeEstimates = HashMap<(String, String), Option<u64>>;
```
Built by iterating `backup_plan.operations`, querying `fs_state` for each send (same
logic as Feature D2).

Update `progress_display_loop` format logic:
- **With estimate:** `[3/6] subvol: 23.1 GB / ~47.6 GB @ 178.3 MB/s [2:10, ~2:17 left]`
- **Without estimate:** `[3/6] subvol: 23.1 GB @ 178.3 MB/s [2:10]`
- **Exceeded estimate:** `[3/6] subvol: 50.1 GB (est ~47.6 GB) @ 178.3 MB/s [4:30]`

ETA suppression rules:
- First 5 seconds (rate unreliable during ramp-up)
- Current bytes > estimated bytes (estimate too low)
- Zero transfer rate

Add pure function: `compute_eta(current: u64, estimated: u64, elapsed: Duration) -> Option<Duration>`

### `src/executor.rs` — pass size estimates

Accept `SizeEstimates` reference (or store in Executor). In `execute_send()`, look up
`(subvol_name, drive_label)` and write `estimated_bytes` to `ProgressContext`.

### Shared helper for size lookup

If Feature D2 is also implemented, extract the size lookup logic (history-first, calibrated
fallback) into a shared function in `plan_cmd.rs` or a new `size_estimation` helper.
Both features use identical logic.

## Data Flow

1. `backup.rs::run()` iterates plan operations, queries `fs_state` for each send's size.
2. Builds `SizeEstimates` map.
3. Passes to executor alongside `ProgressContext`.
4. Executor writes `estimated_bytes` to context before each send.
5. Progress thread reads on send-start transition, computes ETA during display.

## Test Strategy

- **ETA calculation:** Pure `compute_eta` function — normal, exceeded, early phase,
  zero rate, exact completion. ~6 tests.
- **Display formatting:** Extended `format_progress_line` — with estimate, without,
  exceeded, ETA suppression. ~5 tests.
- **Size map construction:** Mixed full/incremental, mock FileSystemState. ~3 tests.

**Estimated: 14-16 tests (including P1 tests that handle estimate field).**

## Effort Estimate

Incremental on Feature P1. If implemented same session as P1: +30%. If P1 is done:
**half session.**

## Dependencies

**Hard dependency on Feature P1.** Extends `ProgressContext` which P1 creates.

**Soft dependency on Feature D2.** Same size lookup logic. Extract to shared helper if
both implemented.

## Risks

**Estimate wildly wrong:** Once current > estimated, switch to "exceeded" mode and drop
ETA. Already specified in format logic.

## Alternatives Rejected

- **Live size query during transfer:** Query btrfs during active send/receive pipe.
  Adds I/O during performance-sensitive operation. Too complex.
- **Rolling window rate:** More responsive ETA but adds complexity. Global average
  sufficient for v1.

## Ready for Review

Focus on:
1. **Estimate accuracy:** Calibrated size measures send-pipe bytes (from `urd calibrate`
   doing full send to /dev/null). History measures actual transfer. Apples-to-apples? Verify.
2. **ETA stability:** Global average rate means ETA oscillates with variable USB speeds.
   Acceptable for v1?
3. **Threading size map:** Executor receives read-only lookup data for display — not
   decision-making. Does not violate executor contract.
