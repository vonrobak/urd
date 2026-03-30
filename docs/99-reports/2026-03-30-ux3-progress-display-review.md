# Arch-Adversary Review: UX-3 Rich Progress Display

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review — `src/commands/backup.rs` and `src/executor.rs` diff
**Commit base:** `3d1a7aa` (master)
**Mode:** Implementation review

---

## Executive Summary

Clean presentation-layer feature with good separation of concerns. The pure formatting
functions are well-extracted and testable. Two significant findings identified: a race
window where the completion line can report slightly stale bytes (accepted for v1), and
a mutex poisoning recovery gap (fixed). No proximity to catastrophic failure — this is
all display code.

## What Kills You

**Catastrophic failure mode:** silent data loss (deleting snapshots that shouldn't be deleted).

**Distance:** Far. Pure presentation. `ProgressContext` and `SizeEstimates` are read-only
display data — they influence no backup decisions, no retention logic, no btrfs commands.
A poisoned mutex cannot affect backup correctness.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Sound logic; completion line has a cosmetic timing issue (S1, accepted) |
| 2 | Security | 5 | No new trust boundaries, no path construction, no privilege escalation |
| 3 | Architectural Excellence | 4 | Good separation: pure formatters, mutex only at transitions |
| 4 | Systems Design | 4 | Handles TTY/non-TTY, shutdown, last-send edge case |
| 5 | Rust Idioms | 4 | Clean Option/match usage, appropriate pub(crate) visibility |
| 6 | Code Quality | 4 | 19 well-targeted tests, readable state machine |

## Design Tensions

1. **Mutex vs Channel.** Mutex piggybacks on existing poll loop, avoids `select!` complexity.
   Lock held for microseconds, contention negligible. Right call.

2. **`pub(crate)` on ProgressContext.** Minimum viable visibility for executor import. Correct.

3. **Size estimate duplication with plan_cmd.rs.** Both implement three-tier fallback by
   calling the same `FileSystemState` methods. Intentional — serves different consumers.
   Three similar lines is better than a premature abstraction.

## Findings

### S1 (Significant): Completion line reports slightly stale bytes — ACCEPTED

The completing state prints `last_display_bytes` which may lag the true total by up to
one poll interval (250ms). At USB3 speeds this is ~45 MB discrepancy. Backup summary
(from `OperationOutcome.bytes_transferred`) is authoritative and correct.

**Resolution:** Accepted for v1. The `~` prefix communicates approximation. Fixing would
add a second mutex lock per send for a cosmetic issue.

### S2 (Significant): send_index and skipped sends — NOT A BUG

Initial concern: `send_index` increments unconditionally, causing gaps when sends are skipped.

**Analysis:** The progress context update is positioned *after* all early returns (cascading
failure, mkdir failure, crash-recovery skip, cleanup failure). Only sends that actually
execute `send_receive()` get a progress index. The crash-recovery "already pinned" path
returns `OpResult::Success` without incrementing — correct, since no visual transfer occurs.

**Resolution:** No code change needed. The placement is correct as implemented.

### M1 (Moderate): Mutex poisoning recovery — FIXED

Changed progress thread's context read from `if let Ok(ctx) = context.lock()` to
`context.lock().unwrap_or_else(|e| e.into_inner())`. Recovers data even from a poisoned
mutex — the data isn't corrupt, only the thread that held it panicked.

### C1 (Commendation): Pure formatting functions

`format_progress_line`, `format_completion_line`, `compute_eta` — pure functions, no I/O,
clear contracts, 15 tests covering all display variants. Right pattern for code that runs
during live multi-TB transfers.

### C2 (Commendation): Shutdown state handles last-send edge case

Every send gets a completion line, including the final one whose counter never resets.

### C3 (Commendation): Option wrapper means zero test impact

All existing executor tests pass `None` — the feature is invisible to tests that don't need it.

## The Simplicity Question

Nothing to remove. Every struct field is used, every function is called, every test covers
a distinct case. The 5-branch match in `format_progress_line` is clear and serves different
display logic — collapsing would add nesting that's harder to read.

## For the Dev Team

1. **M1 — Fixed.** Mutex recovery in progress thread now uses `unwrap_or_else`.
2. **S1 — Accepted.** Completion line byte discrepancy bounded by poll interval. Cosmetic.
3. **S2 — Not a bug.** Progress context update is already after all early returns.

## Open Questions

1. Completion line elapsed time includes poll gap (~250ms). Noticeable for short incrementals
   (8% error on a 3s transfer). Acceptable for v1.

2. Multi-drive sends for same subvolume show separate `[N/total]` entries. Correct behavior
   since each is a distinct transfer operation.
