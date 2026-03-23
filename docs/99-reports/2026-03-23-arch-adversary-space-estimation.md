# Architectural Adversary Review: Pre-Send Space Estimation

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Space estimation feature — 7 files, +315 / -21 lines
**Base commit:** `068f63c` (master)
**Reviewer:** Claude (arch-adversary)

---

## Executive Summary

A clean, well-scoped feature that prevents wasted I/O by skipping sends that won't fit. The implementation respects the planner/executor separation, fails open (no history = proceed), and uses the existing architectural seams correctly. Two moderate findings, no critical issues.

## What Kills You

**Catastrophic failure mode:** Silent data loss — retention deletes snapshots that haven't been sent to all external drives.

**Distance from this change:** Far. The space estimation feature only *skips* sends; it never triggers deletions. A false positive (incorrectly skipping a send that would have fit) delays external backup but does not delete data. A false negative (allowing a send that won't fit) wastes I/O but the executor's crash recovery handles the partial. Neither path reaches the catastrophic failure mode.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Logic is sound; one edge case worth noting (see Finding 1) |
| Security | 5 | No new trust boundaries; no new paths to privileged operations |
| Architectural Excellence | 5 | Extends the existing trait seam perfectly; planner stays pure |
| Systems Design | 4 | Fails open correctly; one observability gap (see Finding 2) |
| Rust Idioms | 5 | Clean trait extension, proper lifetime handling, no unnecessary complexity |
| Code Quality | 4 | Tests cover the three main paths; good test coverage proportional to risk |

## Design Tensions

### 1. Accuracy vs. simplicity in estimation

**Trade-off:** Using last send size × 1.2 instead of a statistical model (average, median, weighted).

**Why this is right:** A single data point with a margin is dead simple, predictable, and easy to reason about. Subvolumes like `subvol3-opptak` (recordings) grow monotonically — yesterday's full send is a lower bound for today's. Subvolumes that churn (home, containers) have incremental sends that are small and unlikely to hit space limits. The 20% margin is conservative enough. A weighted average across multiple historical sends would be more "accurate" but also more complex, harder to explain in skip messages, and not meaningfully better for the real failure mode (200GB subvolume vs 1.2TB drive).

### 2. Fail-open vs. fail-closed on missing history

**Trade-off:** No history → allow the send, even to a tiny drive.

**Why this is right:** The alternative (fail-closed: block sends without history) would prevent Urd from ever completing its first send to a new drive, creating a chicken-and-egg problem. The first send *should* attempt — if it fails, the executor cleans up, and the next run has history to use. This is the correct bootstrap behavior.

### 3. Per-drive vs. cross-drive estimation

**Trade-off:** The query filters by `(subvolume, drive_label, send_type)` rather than using any-drive history as a fallback.

**Why this is right for incrementals** — incremental sizes vary by how long since last send, which is per-drive. **Arguable for full sends** — a full send of the same snapshot is the same size regardless of destination. If you've never done a full send to 2TB-backup but you have done one to WD-18TB, the WD-18TB history would be a valid estimate. This is a minor gap that only matters during the very first full send to a new drive, so it's acceptable.

## Findings

### Moderate

#### Finding 1: `bytes_transferred` is pipe bytes, not on-disk usage

**What:** The `bytes_transferred` value comes from `std::io::copy` between `btrfs send` stdout and `btrfs receive` stdin (`btrfs.rs:142-145`). This is the *send stream* size, not the resulting on-disk size at the destination. For full sends, the destination snapshot's on-disk size depends on BTRFS metadata overhead, compression settings, and CoW reflinks — it can be larger or smaller than the stream.

**Consequence:** The 1.2x margin handles the common case (on-disk is slightly larger than stream), but for heavily-compressed source data or subvolumes with many reflinks, the ratio could exceed 1.2x. For a 200GB subvolume, a 1.5x ratio means 300GB on disk vs. 240GB estimated — the send proceeds, fills the drive, and the executor cleans up the partial.

**Why this is moderate, not significant:** The executor already handles failed sends gracefully (partial cleanup, no data loss). The worst case is a wasted transfer, which is the exact problem this feature was built to prevent — but for an edge case with unusual compression ratios. The 1.2x margin is a reasonable starting point.

**Suggested fix:** No code change needed now. After accumulating real-world data from production runs, compare `bytes_transferred` against actual on-disk usage (via `btrfs subvolume show` if quotas are enabled, or `du -s` on the destination). If the ratio consistently exceeds 1.2x, bump the margin. Consider making the margin configurable in `urd.toml` as a future refinement.

#### Finding 2: Space-skip not visible in `urd plan` output format

**What:** When a send is skipped due to space estimation, the skip message goes into `backup_plan.skipped` as `(subvol_name, reason)`. The `plan_cmd.rs` renderer shows these as `[SKIP]` lines. However, the message format is dense:

```
[SKIP] send to 2TB-backup skipped: estimated ~240.0GB exceeds 100.0GB available (free: 200.0GB, min_free: 100.0GB)
```

This is informative but may not stand out from other skip reasons (interval not elapsed, drive not mounted, already sent). An operator glancing at `urd plan` might not realize a subvolume *will never* reach a drive due to size constraints vs. one that will reach it next run.

**Consequence:** Reduced operational visibility. Not a correctness issue — the skip reason is there if you read it.

**Suggested fix:** Consider prefixing with a distinct marker like `[SKIP:SPACE]` or using a different color for space-based skips. Low priority.

### Commendations

#### Commendation 1: Planner purity preserved

The most important architectural property of Urd is planner/executor separation. This feature needed database access in the planner — a natural temptation to break the purity constraint. Instead, the implementation extends `FileSystemState` with `last_send_size()`, pipes `StateDb` through `RealFileSystemState` via a reference, and keeps `MockFileSystemState` trivially mockable. The planner still takes `&dyn FileSystemState` and never knows about SQLite. This is the correct way to add state-dependent logic to a pure function.

#### Commendation 2: Backward-compatible trait extension

Adding `last_send_size` to `FileSystemState` with `MockFileSystemState::new()` initializing `send_sizes` as empty means `None` is returned for all existing tests. Every existing test passes without modification — no behavioral change. This is clean trait evolution.

#### Commendation 3: StateDb consolidation in backup.rs

Moving `StateDb::open` before planning and sharing the same instance with the executor eliminates a duplicate open. This is a net simplification — fewer system calls, fewer error paths, and the `Option<StateDb>` → `Option<&StateDb>` pattern handles both planning and execution cleanly.

#### Commendation 4: Fail-open safety

`unwrap_or(u64::MAX)` for filesystem queries on non-existent directories, `saturating_sub` for the min_free calculation, and `None` → proceed for missing history. Every edge case fails toward allowing the send, not blocking it. For a backup tool, "try and clean up" is strictly better than "refuse to back up" when the estimation is uncertain.

## The Simplicity Question

**What could be removed?** Nothing. This is 151 new lines in `plan.rs` (including 80 lines of tests) and 145 in `state.rs` (including 100 lines of tests). The production code is ~45 lines in `plan.rs` and ~30 lines in `state.rs`. For the value it provides (preventing multi-hour wasted transfers), this is well-proportioned.

**What's earning its keep?** The `send_type` parameter in the query. At first glance, you might think "just query the last send, regardless of type." But full sends are orders of magnitude larger than incrementals. If you used incremental history to estimate a full send, you'd always allow it. If you used full send history to estimate an incremental, you'd always block it. The type-specific query is load-bearing.

## Priority Action Items

1. **No critical or significant items.** Ship as-is.
2. (Optional) After production data accumulates, validate the 1.2x margin against actual on-disk usage.
3. (Optional) Consider a distinct skip marker for space-based skips in `urd plan` output.

## Open Questions

1. **What is the actual pipe-to-disk ratio for your subvolumes?** If BTRFS compression is enabled on the destination, the on-disk size could be significantly smaller than the pipe bytes, making the 1.2x margin overly conservative (false positives — skipping sends that would fit). If compression is disabled, the ratio is typically close to 1.0x, making 1.2x well-calibrated.

2. **Should the margin be configurable?** A `space_estimation_margin = 1.2` field in `[defaults]` would let operators tune it. Not needed now, but worth considering if the fixed 1.2x causes friction.
