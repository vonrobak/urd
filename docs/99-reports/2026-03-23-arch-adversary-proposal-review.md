# Architectural Adversary Review: Progress & Size Estimation Proposal

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Proposal review — `docs/99-reports/2026-03-23-proposal-progress-and-size-estimation.md` + conversation context including real `btrfs` command output from the production system
**Reviewer:** Claude (arch-adversary)
**Base commit:** `068f63c` (master, post-space-estimation feature)

---

## Executive Summary

The proposal identifies the right problems and the right general direction, but Tier 2 (filesystem-level upper bound) should be dropped — it adds code for a check that's wrong more often than it's right. The progress counter is well-designed. The `urd calibrate` approach is sound but the proposal underestimates a subtle correctness issue with `du -s` vs. btrfs send stream size. The proposal also misses a simpler alternative that solves 80% of the problem with 20% of the machinery.

## What Kills You

**Catastrophic failure mode:** Silent data loss — retention deletes snapshots that haven't reached all external drives.

**Distance from this proposal:** Far. Neither progress indication nor size estimation affects retention, pin files, or deletion logic. The worst outcome of a bad size estimate is a skipped send (backup delayed) or a failed send (wasted I/O, executor cleans up). Neither path reaches data loss.

That said, there's a subtler risk: **false positives in size estimation could permanently block sends.** If the calibrated size is stale-high (subvolume shrank after data cleanup), the planner would skip sends indefinitely until recalibration. This means a subvolume silently stops getting backed up externally — not data loss, but reduced protection. This is the closest the proposal gets to the catastrophic failure mode and it warrants explicit mitigation.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | Tier 2 logic is flawed; `du -s` vs. send stream size mismatch unaddressed |
| Security | 4 | No new privilege escalation paths; sudoers additions are read-only |
| Architectural Excellence | 4 | Progress counter design preserves existing seams; calibrate is well-separated |
| Systems Design | 3 | Stale calibration data could silently block sends; no recalibration trigger |
| Rust Idioms | 4 | Progress callback design is idiomatic; `AtomicU64` is the right primitive |
| Code Quality | 4 | Proposal is clear and well-structured; good analysis of btrfs command limitations |

## Design Tensions

### 1. Estimation accuracy vs. implementation complexity

**Trade-off:** The proposal offers three tiers of increasing accuracy and cost. This is the right structure. But the middle tier (filesystem upper bound) occupies an awkward position — it's too inaccurate to prevent wasted sends on the only system that matters (the btrfs-pool with 7 subvolumes varying from ~50GB to ~3TB), yet adds code and a concept to explain.

**Verdict:** Drop Tier 2. Go from Tier 1 (already done) directly to Tier 3 (`urd calibrate`). The gap between "no history" and "calibrated size" is exactly one `urd calibrate` run — the user can do this before the first backup. Simpler is better.

### 2. Proactive estimation vs. reactive learning

**Trade-off:** The proposal leans heavily on proactive measurement (calibrate before first send). The alternative is reactive: let the first send attempt proceed, observe the `bytes_transferred` result (even from a failed send), and use that for future planning.

This deserves more attention. Today, the executor records `bytes_transferred: None` for failed sends. But the copy thread *does* count bytes before the pipe breaks. If we recorded the partial byte count from failed sends, the system would learn "subvol5-music sent 1.1TB before failing on the 1.3TB-free drive" — and that's enough to skip it next time. No calibration needed.

**Verdict:** This is a Finding (see below). It doesn't eliminate the need for calibration (you still want to prevent the first wasted attempt), but it means the system self-heals after one failure. That significantly reduces the urgency of `urd calibrate`.

### 3. Progress callback in BtrfsOps trait vs. outside it

**Trade-off:** The proposal adds a progress callback parameter to `send_receive` in the `BtrfsOps` trait. This means `MockBtrfs` must handle it too. An alternative is to keep `BtrfsOps::send_receive` unchanged and have `RealBtrfs` expose the `AtomicU64` byte counter separately — the executor reads it from outside.

**Verdict:** The callback approach is cleaner. The `AtomicU64` approach would require the executor to spawn a polling thread and deal with lifecycle (when to stop polling). The callback inverts the control: the copy loop pushes updates, the executor consumes them. This is the right direction. But the trait change should use a concrete type, not `dyn Fn` — see Finding below.

### 4. `du -s` vs. actual send stream size

**Trade-off:** The proposal assumes `du -s` gives a close estimate of full send size. This is approximately true but has a specific failure mode worth naming.

`du -s` reports the sum of file sizes (or block usage with `--apparent-size`). A btrfs send stream includes:
- All file data
- All metadata (xattrs, permissions, directory structure)
- Inline extents (small files stored in metadata)

But it does NOT include:
- Extents shared via reflinks (these appear once in the stream, but `du -s` counts them per-file)

For subvolumes with heavy reflink use (CoW copies, deduplication), `du -s` can significantly **overestimate** the send stream size. This matters for subvol3-opptak if recordings share extents, or for subvol7-containers if container images use reflinks.

Conversely, metadata overhead means the send stream can be slightly **larger** than `du -s` reports for subvolumes with millions of small files.

**Verdict:** The 1.2x margin handles the underestimate case. The overestimate case (reflinks) could cause false-positive skips. This should be documented and the margin should be tunable.

## Findings

### Significant

#### Finding 1: Record `bytes_transferred` from failed sends

**What:** Today, `executor.rs` records `bytes_transferred: None` for failed sends (lines 420-424). But `btrfs.rs` returns `Err(UrdError::Btrfs(...))` without the byte count, even though the copy thread has already counted bytes before the pipe broke.

**Consequence:** After a failed send to an undersized drive, the system has no data to prevent the same wasted attempt next run. The user must either run `urd calibrate` or wait for a successful send to a different drive (which may never happen for that subvolume if it's too large for all configured drives except the unmounted one).

**Suggested fix:** In `btrfs.rs`, when the send fails, capture the byte count from the copy thread and include it in the error or return it alongside the error. Then in the executor, record it as a failed operation with `bytes_transferred` populated.

One approach:

```rust
// In btrfs.rs, change the error return to also carry partial byte count:
struct SendFailure {
    error: UrdError,
    bytes_transferred: Option<u64>,
}
```

Or simpler: change `send_receive` to always return `SendResult` alongside `Result`, e.g., `Result<SendResult, (UrdError, Option<u64>)>`.

Then in the planner's `last_send_size` query, consider also querying failed sends with non-null `bytes_transferred` as a lower bound. If a failed send transferred 1.1TB before dying, the full subvolume is at least 1.1TB — enough to skip a drive with 1.3TB free given the 1.2x margin.

**Priority:** High. This makes the system self-healing after one failed attempt, which is the exact scenario the proposal is trying to prevent. It's also cheaper than `urd calibrate` and requires no user action.

#### Finding 2: Stale calibration can silently block sends

**What:** The `subvolume_sizes` table stores `estimated_bytes` with a `measured_at` timestamp, and the proposal mentions a TTL. But the proposal doesn't specify what happens when a calibration entry exists but is stale, or when a subvolume shrinks after data deletion.

**Consequence:** If `subvol3-opptak` is calibrated at 3TB, then the user deletes 1.5TB of old recordings, the calibrated size blocks sends to the 2TB-backup drive even though the subvolume now fits. The planner sees "estimated ~3.6TB (with 1.2x margin) > 1.3TB free" and skips indefinitely. The user gets no indication that the skip reason is stale data.

**Suggested fix:** Two mitigations:

1. **Age-based staleness warning.** If calibration data is older than N days (configurable, default 30), include "calibrated N days ago — run `urd calibrate` to refresh" in the skip message. This makes stale data visible.

2. **Successful-send-overrides-calibration.** If Tier 1 (historical send data) has a value, always prefer it over Tier 3 (calibration). `bytes_transferred` from a real send is ground truth. Calibration is only the fallback for first-ever sends.

3. **Auto-recalibrate on first send to new drive.** When the planner would use calibration data to skip a send, and there's no Tier 1 history for that (subvolume, drive, send_type) triple, flag it as "estimated from calibration — may be stale" rather than silently skipping.

### Moderate

#### Finding 3: Tier 2 (filesystem upper bound) is not worth building

**What:** The proposal recommends Tier 2: "If the filesystem total used exceeds destination free x number of subvolumes on that root, log a warning."

**Why it fails:** The btrfs-pool has 10.87TiB used across 7 subvolumes. The average is ~1.55TB. But the actual distribution is wildly uneven — subvol1-docs is likely ~200GB while subvol5-music is ~1TB+. An average-based check would:
- **False-positive skip** subvol1-docs on 2TB-backup (average 1.55TB > 1.3TB free, but actual ~200GB fits easily)
- **False-negative allow** subvol5-music on 2TB-backup (if the average happened to be below free space due to many small subvolumes, the 1TB+ music would still fail)

The "per-root-group" refinement acknowledges this weakness but doesn't fix it — the distribution is the problem, not the averaging method.

**Suggested fix:** Drop Tier 2 entirely. The implementation cost is low but it adds a concept that's wrong more often than right, and wrong in both directions. Go directly from Tier 1 (history) to Tier 3 (calibration). The gap is exactly one `urd calibrate` run.

#### Finding 4: Progress callback should not change the `BtrfsOps` trait signature

**What:** The proposal adds `progress: Option<&dyn Fn(SendProgress)>` to `send_receive` in the `BtrfsOps` trait. This forces `MockBtrfs` to accept a parameter it will never use, and adds a `dyn Fn` trait object to an interface that's currently simple and clean.

**Consequence:** Every test that calls `send_receive` must now pass `None` as the progress parameter, or the trait becomes harder to mock. The progress callback is a presentation concern — it doesn't affect the send's correctness, and the mock doesn't need to know about it.

**Suggested fix:** Keep `BtrfsOps::send_receive` unchanged. Instead, add a separate `send_receive_with_progress` method only on `RealBtrfs` (not on the trait). The executor can downcast or use a different code path for real vs. mock. Or simpler: have `RealBtrfs` accept an `Arc<AtomicU64>` byte counter in its constructor, which the copy loop updates. The executor creates the counter, passes it to `RealBtrfs`, and polls it for progress display. The trait stays clean.

Actually, the cleanest approach: the executor already knows whether it has a TTY. It can construct a `RealBtrfs` that has a byte counter, spawn a progress display thread that polls the counter, and let the existing `send_receive` trait method work unchanged. The counter is an implementation detail of `RealBtrfs`, not a contract on `BtrfsOps`.

#### Finding 5: `du -s` measures the source subvolume, not the snapshot

**What:** The proposal says "Run `du -s --apparent-size <source_path>` to measure the live subvolume." But sends operate on *snapshots*, not live subvolumes. A snapshot is a point-in-time freeze — its size may differ from the live subvolume if data was added or deleted between the snapshot and the calibration.

**Consequence:** For subvolumes with high churn (htpc-home, subvol7-containers), the difference could be significant. A `du -s` on `/home` right now gives 77.64GB, but the snapshot taken 15 minutes ago might be 77.5GB or 77.8GB. For static subvolumes (music, multimedia), the difference is negligible.

**Suggested fix:** Run `du -s` on the most recent snapshot, not the live source. The snapshot directory is known from the config (`root/subvol_name/latest_snapshot`). This gives the exact size that would be sent. For the `urd calibrate` command, iterate subvolumes, find their newest local snapshot, and `du -s` that.

Caveat: if the snapshot is a read-only btrfs subvolume (which it is), `du -s` still works and gives the correct size. No sudo needed since the files are readable by the user.

### Minor

#### Finding 6: Progress display format should handle very large transfers

**What:** The proposal shows `12.4GB / ~45.2GB`. For subvol3-opptak at ~3TB, this would show `1234.5GB / ~3000.0GB`. The `ByteSize` Display impl handles TiB, so it would actually be `1.2TB / ~3.0TB`, which is fine. But the elapsed time and ETA for a multi-hour transfer should use a clear format.

**Suggested fix:** For transfers over 1 hour, show `[1:23:45 / ~4:30:00]` instead of `[83:45 / ~270:00]`. Use `HH:MM:SS` format. The proposal's `[2:18 / ~7:26]` example is ambiguous — is that hours:minutes or minutes:seconds?

### Commendations

#### Commendation 1: TTY detection for progress output

The proposal correctly identifies that progress output must be suppressed when stdout is not a TTY (systemd journal, piped output, cron). `std::io::stdout().is_terminal()` is the right API, and it was called out explicitly. This is the kind of systems-awareness that separates a tool that works in testing from one that works in production.

#### Commendation 2: Honest assessment of btrfs command limitations

The "Available btrfs commands and what they tell us" section is excellent. Each command is evaluated with a clear verdict, backed by what actually happens on the production system (now confirmed with real output). The proposal doesn't pretend `btrfs subvolume show` gives useful size data — it names the limitation and moves on. This kind of honest assessment prevents wasted implementation effort.

#### Commendation 3: Fail-open as default for missing data

The existing Tier 1 implementation and the proposal's overall philosophy of "if we don't know, let it try" is correct for a backup tool. The only danger is silent permanent skipping from stale calibration (Finding 2), which is the mirror failure of fail-open and needs explicit mitigation.

## The Simplicity Question

**What should be removed?**

- **Tier 2 (filesystem upper bound).** Adds code and a concept for a check that's wrong in both directions for the actual data distribution. Drop it.
- **Tier 3 Option A (opportunistic qgroup).** Quotas are confirmed off. This option exists only for hypothetical future users who might enable quotas. It's speculative complexity. If a future user wants qgroup support, they can add it then. Don't build it now.

**What should be added that isn't in the proposal?**

- **Record partial byte counts from failed sends.** This is the single highest-value change not in the proposal. It makes the system self-healing.

**What's earning its keep?**

- **Progress counter.** Every send benefits. High value, low complexity.
- **`urd calibrate` with cached sizes.** Solves the first-send problem definitively. Worth the new table and command.
- **Tier 1 (historical send data).** Already built, already working. Foundation for everything else.

## Priority Action Items

1. **Record `bytes_transferred` from failed sends** — Highest value-to-effort ratio. Makes the system learn from failures without user intervention. Requires changes to `btrfs.rs` error path and `executor.rs` recording logic.

2. **Build the progress counter** — Highest UX impact. Keep it outside the `BtrfsOps` trait. Use an `AtomicU64` counter in `RealBtrfs` that the executor polls for display.

3. **Build `urd calibrate`** — Runs `du -s` on the newest snapshot (not the live source) for each subvolume. Stores results in `subvolume_sizes` table. Planner queries this as Tier 3 fallback when Tier 1 has no data.

4. **Add staleness warning to calibration-based skips** — When a skip is based on calibration data older than 30 days, include the age in the skip message.

5. **Drop Tier 2 and Tier 3 Option A** — Simplify the plan. Don't build what doesn't earn its keep.

6. **Ensure Tier 1 always overrides Tier 3** — Historical send data is ground truth. Calibration is a fallback for first-ever sends only.

## Open Questions

1. **How fast is `du -s` on your largest snapshots?** The proposal says "minutes for large subvolumes." For subvol3-opptak (~3TB of recordings), this could be 5-15 minutes depending on file count and disk cache state. Is this acceptable for a manual `urd calibrate` run? Consider running it with `--one-file-system` to avoid crossing mount points, and with `ionice -c3` to avoid disrupting other I/O.

2. **Does the copy thread in `btrfs.rs` actually have the byte count when the send fails?** The `copy_thread.join().unwrap_or(Ok(0)).ok()` on line 158 — if the pipe breaks due to a full disk, `io::copy` returns the number of bytes successfully copied before the error. This should be the partial count. Worth verifying with a test on the actual system.

3. **Should `urd calibrate` run automatically as part of `urd init`?** For the initial setup case, the user runs `urd init` and then discovers their first send will fail because there's no history. If `urd init` offered to calibrate sizes (with a "this may take several minutes" warning), the first `urd backup` would already have size data.
