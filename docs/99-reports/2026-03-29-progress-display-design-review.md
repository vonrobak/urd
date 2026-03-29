# Architectural Review: Progress Display & Plan Output Designs

**Project:** Urd
**Date:** 2026-03-29
**Scope:** Five design proposals in `docs/95-ideas/2026-03-29-design-*.md`
**Review type:** Design review
**Commit:** `a123b70` (v0.4.0)
**Real data source:** Run #15 — 6 full sends to WD-18TB, 31,601s (8h46m), 4.6 TB total

## Executive Summary

Five well-scoped UX improvements that address real pain observed during an 8-hour manual
backup. None are near the catastrophic failure mode. The designs respect module boundaries
and keep I/O out of pure modules. Two significant findings: the size estimation design has
a data accuracy gap that real run data exposes, and the skip classification approach creates
a silent coupling that will break. Both are fixable without changing the overall approach.

## What Kills You

**Catastrophic failure mode:** Silent data loss via incorrect retention or path construction.

**Distance:** These five designs are far from it. They modify the presentation layer (voice.rs),
output types (output.rs), and runtime coordination (backup.rs progress thread). No design
touches retention logic, pin file management, btrfs command construction, or executor
decision-making. The closest any design gets is Feature P1 adding a field to the Executor
struct — but it's an `Option<Arc<Mutex<>>>` for display purposes, checked nowhere in the
execution logic. **These are safe to build.**

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Designs are sound; one data accuracy gap in size estimation (S1) |
| Security | 5 | No privilege boundary interaction. Display-only changes. |
| Architecture | 4 | Clean module boundaries. One coupling concern (S2). One premature complexity flag (M1). |
| Systems Design | 4 | Real-data-informed. One environment assumption to verify (M2). |

## Design Tensions

### 1. Structured data vs. string parsing (D1)

The design classifies skip reasons by parsing free-text strings from plan.rs at the
output boundary. The alternative — structured skip types from the planner — would touch
the core `BackupPlan` type.

**Verdict: the string-parsing approach is the right call for now.** The planner is pure
(ADR-108) and its skip reasons are an internal contract between plan.rs and the output
layer. Making them structured is architecturally cleaner but premature — it changes a
core type for a presentation concern. The fragility is real (see S2) but manageable with
a completeness test.

### 2. FileSystemState in the output boundary (D2)

`build_plan_output()` is currently a pure transform. Adding `&dyn FileSystemState` makes
it depend on database state. The design correctly notes this is acceptable because
`plan_cmd.rs` is a wiring layer (commands/), not a pure module.

**Verdict: correct.** The alternative — threading sizes through `PlannedOperation` — would
contaminate the planner with display concerns. The wiring layer is the right place for
this join.

### 3. Mutex vs. channel for progress context (P1)

The design chose `Arc<Mutex<ProgressContext>>` over channels. For one write per send
(minutes apart) and one read per 250ms poll, mutex contention is non-existent.

**Verdict: correct.** A channel would be cleaner for event-driven communication but adds
complexity to the poll loop. The mutex piggybacks on the existing design.

### 4. Global average rate vs. rolling window (P3)

The design uses total_bytes/total_elapsed for ETA calculation, noting that a rolling
window (brainstorm idea P7) was rejected as out-of-scope.

**Verdict: correct for v1.** Real data from run #15 shows the transfer rates were
remarkably stable: htpc-home at 140 MB/s, subvol3-opptak at 154 MB/s, subvol5-music
at 138 MB/s. The global average would have been accurate for this run. A rolling window
can be added later if users report ETA instability.

## Findings

### S1 — Significant: Size estimation accuracy gap between drives

**What:** Feature D2 proposes using `last_send_size(subvol, drive, send_type)` as
primary estimate, falling back to `calibrated_size`. But `last_send_size` queries
by drive label. When the user switches from WD-18TB1 to WD-18TB (as happened today),
there is **no history for the new drive label.** The first dry-run against WD-18TB
would show calibrated sizes only (available for 2 of 8 subvolumes) or nothing.

**Real data proves this:**

| Subvolume | Calibrated | Run 14 (WD-18TB1) | Run 15 (WD-18TB) | Available for D2? |
|-----------|-----------|-------------------|------------------|-------------------|
| htpc-home | — | 53.2 GB | 42.3 GB | No (no cal, wrong drive) |
| subvol3-opptak | 3.1 TB | — | 3.4 TB | Yes (calibrated, 1.2% off) |
| subvol2-pics | — | 47.6 GB | 47.6 GB | No |
| subvol1-docs | — | 12.7 GB | 12.7 GB | No |
| subvol7-containers | — | 13.8 GB | 13.9 GB | No |
| subvol5-music | 1.0 TB | — | 1.1 TB | Yes (calibrated, 0.2% off) |

After run 15, WD-18TB has history and future lookups will work. But on the first
encounter with any new drive label, 4 of 6 subvolumes would show no estimate.

Furthermore, htpc-home shows a **20% difference** between run 14 (53.2 GB, sent
the 20260327 snapshot) and run 15 (42.3 GB, sent the 20260329 snapshot). This is
because different snapshots were sent — the data changed between them. Historical
send sizes are accurate only when the data hasn't changed significantly.

**Suggested fix:** The design's priority should be: (1) same-drive history, (2)
any-drive history for same send_type, (3) calibrated size. Adding a cross-drive
fallback — "no WD-18TB history, but WD-18TB1 sent this subvolume as full at 47.6 GB" —
covers the drive-swap scenario. This is a one-line change to the lookup: if
`last_send_size(subvol, drive, type)` returns None, try
`last_send_size_any_drive(subvol, type)`.

**Consequence if unfixed:** 4 of 6 subvolumes show no size estimate on first use of
a new drive. The feature's value proposition (informed decisions before long runs)
is undermined exactly when the user needs it most — drive swaps and first sends.

### S2 — Significant: Skip classification is a silent coupling that will break

**What:** Feature D1 classifies skip reasons by parsing free-text strings. There are
**13 distinct skip patterns** in plan.rs (verified by grep):

1. `"disabled"`
2. `"drive {} not mounted"`
3. `"drive {} UUID mismatch (expected {}, found {})"`
4. `"drive {} UUID check failed: {}"`
5. `"drive {} token mismatch (expected {}, found {}) — possible drive swap"`
6. `"send disabled"`
7. `"local filesystem low on space ({} free, {} required)"`
8. `"snapshot already exists"`
9. `"interval not elapsed (next in ~{})"`
10. `"send to {} not due (next in ~{})"`
11. `"no local snapshots to send"`
12. `"{snap} already on {drive}"`
13. `"send to {} skipped: estimated ~{} exceeds {} available (free: {}, min_free: {})"`
14. `"send to {} skipped: calibrated size ~{} exceeds {} available{}"`

The D1 design's enum lists 9 categories but the actual plan.rs has 14 patterns
(including two space-exceeded variants and the UUID check failed/token mismatch patterns).
The "local filesystem low on space" pattern is missing from the classification entirely.

**Suggested fix:** Add a compile-time or test-time completeness check. One approach:
a test in plan_cmd.rs that collects all known reason prefixes from plan.rs tests
and asserts each classifies to a non-Other category. Alternatively, since this will
break silently, add `#[cfg(test)]` test that greps plan.rs source for `skipped.push`
patterns. Crude but effective.

Also update the enum to include `LocalSpaceLow` (already in the proposed enum but
missing from the classification function description) and ensure `UuidCheckFailed`
is handled (pattern 4 differs from pattern 3).

**Consequence if unfixed:** New skip reasons added to plan.rs in future features (like
HSD-B chain-break detection) will silently fall to `Other`, producing ugly mixed output
where some reasons are collapsed and others aren't.

### M1 — Moderate: SkipCategory enum is over-specified for current needs

**What:** The D1 design proposes 9+ enum variants. Looking at actual dry-run output from
today's run, the 20 skip entries break down as:

| Category | Count | From output |
|----------|-------|-------------|
| Interval not elapsed | 7 | All 7 active subvolumes |
| Drive not mounted | 9 | WD-18TB1 (6), 2TB-backup (3) |
| Disabled | 1 | htpc-root |
| Send disabled | 2 | subvol4-multimedia, subvol6-tmp |
| Send interval not elapsed | 1 | subvol4-multimedia |

Five categories cover this real-world case. UUID mismatch, token mismatch, space exceeded,
already on drive — these are valid but rare. Start with fewer categories and add as needed.

**Suggested fix:** Start with 5 categories: `DriveNotMounted`, `IntervalNotElapsed`,
`Disabled`, `SpaceExceeded`, `Other`. Merge `SendDisabled` into `Disabled` (the user
doesn't care about the distinction in a summary). Merge `SendIntervalNotElapsed` into
`IntervalNotElapsed`. UUID/token mismatch are rare enough for `Other`. This halves the
enum and the classification function while covering 100% of the observed output.

### M2 — Moderate: Progress thread completion detection relies on counter reset timing

**What:** Feature P1 detects send completion when the byte counter transitions from
non-zero to zero. This happens when `btrfs.rs:191` resets the counter at the START
of the **next** send, not at the end of the current send. Real timing from run #15:

- htpc-home: 300.6s
- subvol3-opptak: 21,955.9s (6h6m)
- Gap between them: the executor does snapshot creation (~1s), then enters
  `send_receive()` which resets the counter.

During the ~1s gap between sends (snapshot creation time), the counter stays at the
previous send's final value. The progress thread would show the old send's total as
a stale line for ~1s before detecting the reset.

More importantly: between the last send (subvol5-music) and the shutdown signal,
there's executor cleanup, metrics writing, heartbeat, and notification dispatch.
The final send's completion line would only print during shutdown cleanup — which
could be seconds later.

**Suggested fix:** The design already notes this ("Last-send handling: Must be handled
in shutdown cleanup"). Make it explicit in the state machine: when shutdown is signaled
and `last_display_bytes > 0`, print the completion line immediately with the last known
context. This is correct as designed; just ensure it's not overlooked during implementation.

### M3 — Moderate: ETA meaningless for the 6-hour subvolume

**What:** Feature P3 shows ETA based on current transfer rate. For subvol3-opptak
(3.4 TB, 6h6m in run #15), the ETA would be useful. But the denominator depends on
the estimate source. Calibrated: 3.1 TB. Actual: 3.4 TB. That's a 9% gap, which at
154 MB/s translates to ~33 minutes of ETA error.

For the first 10% of the transfer, the ETA would show "~5h30m remaining." The actual
was 6h6m. By midpoint, the ETA would be more accurate as the rate stabilizes.

**Suggested fix:** This is acceptable for v1. The `~` prefix on estimates communicates
uncertainty. Consider adding a note in the display when the estimate source is calibrated
vs. historical: `~5h30m left (est)` vs `~5h30m left` — the "(est)" qualifier signals
lower confidence.

### Minor — Commendation: The backup summary already collapses some skips

Looking at the actual backup completion output, voice.rs already does partial collapsing
for the post-execution summary:

```
  Drives not mounted: WD-18TB1, 2TB-backup
    → 9 send(s) skipped (htpc-home, subvol3-opptak, ...)
```

This is good evidence that the collapsing approach in D1 is the right direction. The
dry-run plan output should match the quality of the backup summary output. The
implementation can reference `render_backup_summary` in voice.rs for the grouping pattern
that already works.

### Minor — Commendation: Pure formatting functions for testability (P1)

The P1 design explicitly calls out extracting `format_progress_line` and
`format_completion_line` as pure functions. This is the right instinct — the progress
thread's state machine is hard to test, but the formatting is trivially testable.
This pattern (complex state machine + pure formatters) should be the template for any
future interactive display work.

### Minor — Commendation: Recommended implementation order

D5+D1 → D2 → P1+P3 sequences risk correctly. Plan output changes (pure rendering, no
threading) go first, building confidence. Progress display changes (threading, shared
state) go last when the team is warmed up. D2 before P3 means the size lookup logic is
established before P3 needs it.

## Also Noted

- D5 heading style uses `===` for ASCII compatibility — good, but verify it doesn't
  clash with `colored::Colorize` on terminals that don't support bold.
- D2 summary "6 sends (~623 GB estimated for 4 of 6)" is slightly wordy; consider
  "6 sends (~623 GB est.)" with a footnote only when coverage is partial.
- P1 `send_type: &'static str` — consider the `SendType` enum from executor.rs instead
  of a raw string. It already has Full/Incremental/NoSend variants.
- The backup summary output already shows duration per subvolume — D2's estimated
  duration (brainstorm D4) would be a natural companion to the size estimates. Parked
  correctly for now.
- Feature D5 "Skips-only" case (no operations, all skips) — this is the normal nightly
  run when no drives are mounted. Worth a dedicated test verifying the output is clean.

## The Simplicity Question

**What could be removed?** The SkipCategory enum (D1) can be halved from 9+ to 5
variants without losing any real-world coverage. The `SizeEstimates` HashMap (P3) could
be replaced by a closure or a method on the executor if the map feels like unnecessary
indirection.

**What's earning its keep?** The `Arc<Mutex<ProgressContext>>` (P1) is the minimum
viable mechanism for sharing operation context with the progress thread. The
`estimated_bytes: Option<u64>` fields (D2) add exactly one concept to two structs.
The structural headings (D5) are zero-machinery — pure rendering.

**Overall:** These designs are appropriately sized. No feature introduces a new module,
trait, or architectural pattern. They extend existing patterns (atomic shared state,
structured output types, voice rendering) with minimal additions.

## For the Dev Team

Priority-ordered action items:

1. **D2 — Add cross-drive fallback for size estimation.** In the size lookup logic,
   when `last_send_size(subvol, drive, type)` returns None, query
   `last_send_size_any_drive(subvol, type)`. This requires a new `StateDb` method
   (one SQL query without the `drive_label` filter). Without this, 4/6 subvolumes
   show no estimate on first use of a new drive — undermining the feature's value.
   **Files:** `src/state.rs` (new method), `src/commands/plan_cmd.rs` (fallback chain).

2. **D1 — Reduce SkipCategory to 5 variants; add completeness test.** Start with
   `DriveNotMounted`, `IntervalNotElapsed`, `Disabled`, `SpaceExceeded`, `Other`.
   Add a test that exercises all 14 plan.rs skip patterns against the classifier.
   **Files:** `src/output.rs` (enum), `src/commands/plan_cmd.rs` (classifier + test).

3. **P1 — Use `SendType` enum instead of `&'static str`.** The executor already has
   `SendType { Full, Incremental, NoSend }`. Use it in `ProgressContext` instead of
   a raw string. **Files:** `src/commands/backup.rs` (ProgressContext definition).

4. **P1 — Ensure last-send completion line in shutdown path.** Add explicit check in
   progress thread shutdown: if `last_display_bytes > 0`, print completion line before
   clearing. Write a test for this edge case. **Files:** `src/commands/backup.rs`.

5. **D2/P3 — Verify calibrated_size measures send-pipe bytes.** The P3 design assumes
   calibrated sizes are apples-to-apples with the byte counter. Check that `urd calibrate`
   measures the btrfs send stream size, not `du -sb` on the subvolume. (The sqlite data
   shows `method = "du -sb"` for calibrated sizes — this is filesystem size, NOT send
   stream size. The difference may be significant. Verify and document.)
   **Files:** `src/commands/calibrate.rs` or equivalent.

## Open Questions

1. **Calibration method mismatch.** The `subvolume_sizes` table shows `method = "du -sb"`.
   But btrfs send stream size ≠ `du -sb` size. For subvol3-opptak: calibrated = 3.1 TB
   (`du -sb`), actual send = 3.4 TB (run 15). For subvol5-music: calibrated = 1.0 TB,
   actual send = 1.1 TB. The send stream is ~10% larger than filesystem size due to btrfs
   metadata overhead. **This affects both D2 (plan estimates) and P3 (progress ETA).** The
   designs should document this systematic underestimate, or `urd calibrate` should use a
   dry-run send instead of `du -sb`.

2. **WD-18TB1 drive entry consolidation.** After the drive config is consolidated (known
   issue in status.md), will historical WD-18TB1 send data still be queryable? If the
   drive entry is removed from config, `last_send_size("htpc-home", "WD-18TB1", "send_full")`
   will never be called. The cross-drive fallback (S1 fix) would surface this data.

3. **Incremental size display value.** Run data shows incremental sends range from 123
   bytes (no-change subvolumes) to 1.5 GB (active home directory). Showing "last: 123 B"
   for subvol2-pics is technically correct but misleading — it implies tiny transfers are
   normal. Consider a threshold: only show incremental estimates above 1 MB.
