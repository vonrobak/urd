# Architectural Adversary Review: Post-Cutover Features Implementation

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Post-implementation review of Priorities 2, 3, 4 — failed send bytes, progress indication, `urd calibrate`
**Reviewer:** Claude (arch-adversary)
**Base commit:** `a60f031` (master, post-Phase 4)
**Journal:** `docs/98-journals/2026-03-23-post-cutover-features.md`

---

## Executive Summary

This is clean, disciplined work. Three independently-shippable features were built in priority order, each following the designs validated in the prior adversary review. The planner/executor separation — the most important architectural property of Urd — is preserved throughout. The most significant finding is a timing bug in the progress display that produces misleading rate calculations across multiple sends. The second finding is a correctness edge case in calibrate that silently records 0 bytes when `du` output is malformed. Neither is close to the catastrophic failure mode.

## What Kills You

**Catastrophic failure mode:** Silent data loss — retention deletes snapshots that haven't reached external drives.

**Distance from these changes:** Far. None of the three features touch retention logic, pin file handling, or snapshot deletion decisions. The closest path to harm:

1. **Failed send bytes → stale MAX → permanent skip (2 steps from reduced protection):** If a failed send records a large partial byte count, and the subvolume later shrinks, the MAX(successful, failed) logic could permanently skip sends to undersized drives. This is protection reduction, not data loss — local snapshots are unaffected. The `ORDER BY id DESC LIMIT 1` mitigates: only the *most recent* failure is compared, so a new successful send to the same drive/type overwrites it. But stale failed data for a *different* drive/type combination persists indefinitely. Distance: two conditions must hold simultaneously (subvolume shrinks AND no subsequent successful send of same type to same drive). Realistic but slow-onset.

2. **Calibration false-positive → send permanently blocked (1 step from reduced protection):** If calibrated size is stale-high, the planner skips sends. The 30-day staleness warning mitigates but doesn't force recalibration. After the first successful send, Tier 1 data takes over. Only the very first send to a new drive is at risk — and only if calibration data exists and is stale-high.

**Verdict:** Neither path reaches data loss. The worst outcome is a delayed external backup. The system's fail-open behavior (proceed without estimate) and Tier 1 override both work in the right direction.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Sound logic, good edge case handling; two minor bugs (progress timer, calibrate 0-byte) |
| Security | 5 | No new privilege paths; `du` runs without sudo; no path injection vectors |
| Architectural Excellence | 5 | Planner/executor separation rigorously preserved; progress stays outside trait |
| Systems Design | 4 | Good crash recovery (partial cleanup), good fail-open behavior; progress timer is slightly leaky |
| Rust Idioms | 4 | Good atomic usage, correct `Ordering`; struct variant is pragmatic |
| Code Quality | 4 | Well-tested new paths; documentation-driven development; minor test gap in calibrate |

## Design Tensions

### 1. `UrdError::Btrfs` struct variant vs. separate error type

**Trade-off:** Adding `bytes_transferred: Option<u64>` to every `Btrfs` error (including snapshot creation and delete, where it's always `None`) vs. creating a `BtrfsSend` variant just for sends.

**Evaluation:** The struct variant was the right call. The alternative (`BtrfsSend { msg, bytes_transferred }`) would have required the executor's error handler to match on *two* btrfs error variants, adding a code path that's easy to get wrong. The cost of `bytes_transferred: None` on 15 non-send errors is one `Option<u64>` field that the compiler guarantees is always `None` at those sites. This is a textbook trade of type precision for simplicity — and for a single-user backup tool (not a library), simplicity wins.

**Commendation:** The journal explicitly names this trade-off and the alternatives considered. That's exactly the kind of decision documentation that prevents future "why didn't we just..." discussions.

### 2. Global progress timer vs. per-operation timer

**Trade-off:** The progress display uses a single `Instant::now()` at thread creation and computes rate as `total_bytes / total_elapsed`. This gives stable rates but is semantically wrong when the executor runs multiple sends in one backup run — elapsed time includes non-send operations between sends, and the counter resets to 0 between sends while the timer doesn't.

**Evaluation:** This is a bug, not a trade-off (see Finding 1). The timer should reset when the counter resets.

### 3. MAX(successful, failed) in `last_send_size`

**Trade-off:** MAX gives the most conservative (largest) estimate, preventing wasted sends to undersized drives. But a stale failed estimate can persist indefinitely if no new send of the same type to the same drive occurs.

**Evaluation:** MAX is correct for the stated goal (prevent wasted multi-hour sends). The staleness risk is real but low-impact: it causes skipped sends, not data loss, and only for specific (subvolume, drive, send_type) triples. The right mitigation is not to change MAX, but to add a TTL or allow `urd calibrate` to clear stale failed estimates. This is a future enhancement, not a current bug.

### 4. `du -sb` vs. btrfs send stream size

**Trade-off:** `du -sb` can overestimate (reflinks) or underestimate (metadata overhead). The 1.2x margin handles underestimates. Overestimates could cause false-positive skips.

**Evaluation:** For this system's subvolumes (media files, home directories, containers), reflink usage is likely low. The overestimate risk is real but mitigated by Tier 1 always overriding after the first successful send. The calibration exists specifically for the first-send gap — once Tier 1 data exists, calibration is never consulted again. Acceptable for the problem scope.

## Findings

### Significant

#### Finding 1: Progress timer doesn't reset between sends — misleading rate on 2nd+ send

**What:** `progress_display_loop` in `backup.rs:376` creates `start = Instant::now()` once at thread spawn. The byte counter in `RealBtrfs::send_receive` resets to 0 at the start of each send (`counter.store(0, Ordering::Relaxed)` at `btrfs.rs:172`). But the `start` time is never reset.

**Consequence:** For the second send in a backup run, elapsed time includes the entire first send's duration plus any snapshot/retention operations between sends. If the first send took 10 minutes and transferred 1GB, and the second send has transferred 200MB in 30 seconds, the display shows: `200 MB @ 18.2 MB/s [10:30]` — where 10:30 is total wall time since the progress thread started, not time since this send started, and 18.2 MB/s is `200MB / 10.5 minutes`, a dramatic undercount of the actual rate.

**Why it matters:** The progress display exists to answer "is it working or hung?" A rate of 18 MB/s when the actual rate is 400 MB/s is misleading in exactly the wrong direction — it makes a fast send look slow. For a single-subvolume run this doesn't manifest; it only appears when multiple sends execute in one run (the common case: 7 subvolumes × 1-2 drives).

**Suggested fix:** Track a "send start" timestamp that resets when the counter transitions from 0 to non-zero. Something like:

```rust
if current > 0 && last_display_bytes == 0 {
    send_start = Instant::now(); // new send started
}
let elapsed = send_start.elapsed();
```

**Severity:** Significant — affects every multi-send backup run's UX.

#### Finding 2: `calibrate` silently stores 0 bytes on malformed `du` output

**What:** In `commands/calibrate.rs:75-79`:

```rust
let bytes: u64 = stdout
    .split_whitespace()
    .next()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);
```

If `du -sb` produces unexpected output (empty, or first token isn't a number), this silently stores `estimated_bytes = 0` in the database.

**Consequence:** A calibrated size of 0 bytes means the planner's Tier 3 check would compute `estimated = 0 * 1.2 = 0`, which is always ≤ available space, so the send proceeds. This is fail-open, which is the right direction — a 0-byte estimate won't block sends. But it's still semantically wrong: the database now claims the subvolume is 0 bytes, and `urd status` or any future display of calibration data would show "0 bytes" instead of an error.

**Suggested fix:** Treat 0 as an error:

```rust
let bytes: u64 = stdout
    .split_whitespace()
    .next()
    .and_then(|s| s.parse().ok())
    .filter(|&b| b > 0)
    .ok_or_else(|| anyhow::anyhow!("du -sb produced no usable output: {:?}", stdout.trim()))?;
```

Or at minimum, skip the upsert and count it as a failure instead of calibrating to 0.

**Severity:** Significant — wrong data in the database, even if the downstream effect is benign.

### Moderate

#### Finding 3: `calibration_age_days` returns 0 on parse failure — staleness warning never fires for corrupt data

**What:** In `plan.rs:502-507`:

```rust
fn calibration_age_days(measured_at: &str) -> i64 {
    let now = chrono::Local::now().naive_local();
    chrono::NaiveDateTime::parse_from_str(measured_at, "%Y-%m-%dT%H:%M:%S")
        .map(|ts| (now - ts).num_days())
        .unwrap_or(0)
}
```

If `measured_at` is corrupt or in an unexpected format, this returns 0 days — meaning "fresh." The staleness warning (>30 days) never fires.

**Consequence:** If the timestamp in the database is somehow malformed, calibration data appears perpetually fresh. Combined with a stale-high estimate, this could cause sends to be permanently skipped without warning.

**Suggested fix:** Return a large value (e.g., `i64::MAX` or `999`) on parse failure, so corrupt timestamps trigger the staleness warning rather than suppressing it.

**Severity:** Moderate — requires database corruption to trigger, but fails in the wrong direction.

#### Finding 4: Space estimation code is duplicated between Tier 1 and Tier 3

**What:** In `plan.rs:375-427`, the space check logic (compute estimated, get free, subtract min_free, compare) appears twice — once for Tier 1 and once for Tier 3. The two blocks are nearly identical except for the estimate source and skip message.

**Consequence:** If the space check logic needs to change (e.g., margin becomes configurable, or `min_free` semantics change), two sites must be updated. This isn't wrong, but it's one abstraction short of clean.

**Suggested fix:** Extract a helper: `fn check_space(estimated: u64, ext_dir: &Path, drive: &DriveConfig, fs: &dyn FileSystemState) -> Option<(u64, u64, u64)>` that returns `(estimated_with_margin, free, available)` or `None` if space check can't be done. The caller formats the skip message.

**Severity:** Moderate — maintenance friction, not a bug.

### Minor

#### Finding 5: `progress_display_loop` hard-codes 60 spaces for line clearing

**What:** `backup.rs:411`: `eprint!("\r{}\r", " ".repeat(60));`

If the progress line exceeds 60 characters (e.g., very large byte counts like `1.2 TB @ 245.3 MB/s [1:23:45]`), the trailing characters from the longest line won't be cleared.

**Suggested fix:** Track the max line length and clear to that length, or use ANSI escape `\x1b[2K` (erase entire line) which works on all modern terminals.

**Severity:** Minor — cosmetic.

#### Finding 6: `format_elapsed` could show `0:03` instead of `0:03` — edge case clarity

**What:** The format `{mins}:{secs:02}` produces `0:03` for 3 seconds. This is correct but some users might read it as "0 hours 3 minutes" instead of "0 minutes 3 seconds."

**Suggested fix:** Not urgent, but `0m03s` or `0:03s` would be unambiguous. Cargo uses `0.03s` for sub-minute. This is a preference, not a bug.

**Severity:** Minor — cosmetic.

### Commendations

#### Commendation 1: Progress counter stays outside `BtrfsOps` trait

The `bytes_counter: Arc<AtomicU64>` lives in `RealBtrfs` (the concrete type), not in `BtrfsOps` (the trait). `MockBtrfs` doesn't know about it. This is exactly right. The progress counter is a presentation concern that belongs in the I/O implementation, not in the behavioral contract. The executor reads it through the shared `Arc`, not through the trait. This preserves the architecture's most valuable property: you can test all backup logic through the mock without dealing with progress display.

#### Commendation 2: Partial bytes flow naturally through existing infrastructure

The decision to extend `UrdError::Btrfs` with `bytes_transferred: Option<u64>` means the executor's existing `match &e { UrdError::Btrfs { .. } => ... }` pattern extracts partial bytes with no new plumbing. The existing `record_operation` call writes them to SQLite with no schema change (the `bytes_transferred` column already exists). The planner reads them through `last_failed_send_size`. This is a textbook example of a well-designed data flow: one change (error struct variant) propagates through three layers without requiring structural changes at any of them.

#### Commendation 3: Tier 1 always overrides Tier 3

The calibration check is in the `else` branch of the Tier 1 check (`plan.rs:397`). This means ground truth (actual send bytes) always wins. There's no code path where stale calibration data can override a recent successful send. This is the right invariant and it's enforced by code structure, not by a runtime check.

#### Commendation 4: Journal's "Design Decisions That Warrant Adversary Scrutiny" section

The journal explicitly lists 7 design decisions the author is uncertain about, with specific alternatives and edge cases. This is unusually honest self-assessment. Every one of the 7 items raised is a genuine trade-off worth discussing. The quality of the journal's analysis means the adversary review can focus on implementation details rather than re-deriving the design space.

## The Simplicity Question

**What could be removed?** Very little. Each of the three features is minimal for what it does:

- Failed send bytes: ~80 lines, 4 tests. The struct variant is more verbose than a tuple variant but the compiler catches every site. No new types, no new modules.
- Progress display: ~70 lines, no new dependencies. The `AtomicU64` counter is the simplest possible shared state.
- Calibrate: ~120 lines, 1 new file. Uses existing `FileSystemState` trait, existing `StateDb` infrastructure.

**What's earning its keep?** The `FileSystemState` trait is the unsung hero. It enables testing all space estimation logic (Tier 1, Tier 3, fail-open) through `MockFileSystemState` without touching the filesystem or SQLite. The 6 plan tests for space estimation are fast, deterministic, and cover the critical decision paths.

**What isn't earning its keep?** The `method` column in `subvolume_sizes` stores `"du -sb"` for every row. Today there's only one measurement method. If a second method is never added, this is dead weight. But it's one TEXT column — the cost of removing it later exceeds the cost of carrying it.

## Priority Action Items

1. **Fix progress timer reset** (Finding 1) — Reset the rate timer when counter transitions from 0 to non-zero. Affects every multi-send backup's display. ~5 lines.

2. **Don't store 0-byte calibration** (Finding 2) — Treat `du` returning 0 or unparseable output as a failure. ~5 lines.

3. **Make `calibration_age_days` fail loud** (Finding 3) — Return a large value on parse failure, not 0. ~1 line change.

4. **Extract space check helper** (Finding 4) — Optional, reduces duplication in `plan_external_send`. ~20 lines refactor.

5. **Consider TTL on failed send estimates** (design tension) — Not urgent, but the MAX(successful, failed) approach means a stale failed estimate can linger. Consider clearing failed estimates when a successful calibration runs. Future work.

## Open Questions

1. **Has `du -sb` been tested on btrfs snapshots with xattrs and symlinks on this system?** The journal flags this but doesn't resolve it. Edge case: `du -sb` follows symlinks by default — if snapshots contain symlinks to large files outside the snapshot, the estimate could be wildly wrong. Check whether `-P` (don't follow symlinks) should be added.

2. **Should successful sends update `subvolume_sizes`?** After a successful full send, the exact pipe bytes are known. Storing them in `subvolume_sizes` would keep calibration data fresh without running `urd calibrate`. This would close the staleness gap for subvolumes that successfully send. The trade-off: pipe bytes ≠ `du -sb` bytes (different measurement), so mixing methods in one table could confuse future queries. Worth discussing.

3. **What happens if the progress thread panics?** The thread runs `progress_display_loop` which can't panic (no `unwrap`, no indexing). But if it did, `h.join()` in `backup.rs:98` would return `Err`, which `.ok()` swallows. The backup would complete but the progress line might not be cleared. Low risk, but worth noting.

---

**Metadata:**
- Commit SHA: `a60f031` (base; changes are unstaged)
- Test results: 144 passed, 0 failed, 0 ignored
- Clippy: clean (`-D warnings`)
- Files reviewed: `src/error.rs`, `src/btrfs.rs`, `src/executor.rs`, `src/state.rs`, `src/plan.rs`, `src/cli.rs`, `src/main.rs`, `src/commands/calibrate.rs`, `src/commands/backup.rs`, `src/commands/init.rs`
- Areas excluded: retention.rs, chain.rs, metrics.rs (not touched by these changes)
