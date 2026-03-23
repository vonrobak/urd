# Proposal: Send Progress Indication & Proactive Size Estimation

**Date:** 2026-03-23
**Status:** Proposal
**Context:** During first real-world testing, two UX gaps emerged:
1. Long-running sends show only a blinking cursor — no indication of progress or what's happening
2. Full sends to undersized drives (e.g., 1TB+ music to 2TB-backup) attempt and fail after hours, wasting time and I/O

---

## Problem 1: Silent Progress

### What happens today

When `urd backup` runs a send, the executor calls `btrfs.send_receive()` which pipes `btrfs send` stdout to `btrfs receive` stdin via `std::io::copy` in a background thread. The copy thread counts bytes but only reports the total after completion. During the transfer — which can take minutes to hours — the terminal shows nothing.

An experienced user recognizes a blinking cursor as "working." A new user might think Urd has hung.

### Proposed solution: Live transfer counter

Replace the atomic `std::io::copy` in `btrfs.rs` with a chunked copy loop that reports progress periodically.

#### What to show

```
  htpc-home -> WD-18TB1 (incremental)  47.3MB / ? @ 156.2MB/s  [0:03]
  subvol3-opptak -> 2TB-backup (full)  12.4GB / ~45.2GB @ 89.1MB/s  [2:18 / ~7:26]
```

Two modes:
- **Unknown total** (always true for incremental, true for first full send): Show bytes transferred, transfer rate, elapsed time. No percentage bar.
- **Known estimate** (full send with historical `bytes_transferred` or btrfs-reported size): Show bytes transferred, estimated total, transfer rate, elapsed time, estimated remaining.

#### Implementation

**`btrfs.rs`** — Add a progress callback to `send_receive`:

```rust
pub struct SendProgress {
    pub bytes_transferred: u64,
    pub elapsed: Duration,
    pub rate_bytes_per_sec: f64,
}

fn send_receive(
    &self,
    snapshot: &Path,
    parent: Option<&Path>,
    dest: &Path,
    progress: Option<&dyn Fn(SendProgress)>,
) -> Result<SendResult>
```

The copy thread changes from `std::io::copy` to a loop reading 256KB chunks, updating an `AtomicU64` byte counter. A separate reporting thread (or the main thread via periodic check) calls the progress callback every 1-2 seconds.

**`executor.rs`** — Pass a closure that formats and prints the progress line using `\r` (carriage return) to overwrite the current line. When the send completes, print the final line with a newline.

**`BtrfsOps` trait** — The trait method signature gets the optional callback. `MockBtrfs` ignores it.

**Terminal detection** — Only show progress when stdout is a TTY (`std::io::stdout().is_terminal()` on Rust 1.70+). When piped or running under systemd (no TTY), skip progress output to avoid polluting logs.

#### Complexity: Low-Medium

~50 lines in `btrfs.rs` (chunked copy + atomic counter), ~30 lines in `executor.rs` (progress formatting), minor trait change. No architectural impact.

#### Risks

- **Performance**: 256KB chunk reads add negligible overhead vs. the `std::io::copy` 8KB default. The `io::copy` implementation already reads in chunks internally.
- **Thread safety**: The `AtomicU64` byte counter is lock-free. The reporting thread only reads it.
- **Systemd compatibility**: No TTY detection avoids spurious output in journal logs.

---

## Problem 2: Proactive Size Estimation for First Sends

### What happens today

The space estimation implemented today queries SQLite for historical `bytes_transferred`. This works well after the first successful send — but for first-ever full sends, there's no history. The planner allows the send to proceed (fail-open), which is correct for safety but means a 1TB music subvolume will attempt to send to a 2TB drive, run for hours, and fail when the drive fills up.

### The challenge

BTRFS doesn't make it easy to know "how big will this send stream be?" before running the send. The send stream size depends on:
- **Full send**: All data in the snapshot — roughly equals the subvolume's data usage
- **Incremental send**: Only the delta since the parent — unpredictable without running the diff

For full sends (the problematic case), we need to estimate the source subvolume's data size and compare it against destination free space.

### Available btrfs commands and what they tell us

#### `sudo btrfs subvolume show <path>`

Shows subvolume metadata. Example output:
```
/mnt/btrfs-pool/subvol5-music
        Name:                   subvol5-music
        UUID:                   a1b2c3d4-...
        ...
        Exclusive:              0.00B
```

The **Exclusive** field shows data unique to this subvolume. However, this field is only populated when **quotas (qgroups) are enabled**. Without quotas, it reports `0.00B` or `none`.

**Verdict:** Unreliable without quotas. Enabling quotas has a performance cost (BTRFS tracks per-subvolume usage on every write). Not recommended to require it.

#### `sudo btrfs filesystem usage <path>`

Shows space breakdown for the entire filesystem. Example:
```
Overall:
    Device size:            18.19TiB
    Device allocated:       12.73TiB
    Used:                   12.47TiB
    Free (estimated):        5.72TiB
    ...
Data,single: Size:12.70TiB, Used:12.45TiB
Metadata,DUP: Size:15.00GiB, Used:10.27GiB
```

This tells us how much data is on the **entire filesystem**, not per-subvolume. Useful for destination free space (more accurate than `statvfs` because it accounts for BTRFS metadata overhead), but not for estimating a single subvolume's size.

**Verdict:** Useful for accurate destination free space. Not useful for per-subvolume source size.

#### `sudo btrfs filesystem show <path>`

Shows device-level info (device sizes, used). Similar to `filesystem usage` but less detailed. Already in sudoers.

**Verdict:** Redundant with `statvfs` for free space. No per-subvolume info.

#### `sudo btrfs filesystem du <path>` (or `du -s <snapshot>`)

Walks the directory tree and sums file sizes. Gives the actual data size of a snapshot directory.

**Verdict:** Accurate but **very slow** for large subvolumes. Running `du -s` on a 1TB music collection could take minutes itself. Acceptable as a one-time calibration, not as a per-run check.

#### `sudo btrfs qgroup show <path>`

If quotas are enabled, shows per-subvolume data usage (referenced and exclusive bytes). This is the ideal data source.

**Verdict:** Accurate and fast — but requires quotas to be enabled. Could be used opportunistically.

### Proposed solution: Layered estimation

A three-tier approach, each tier more accurate but more expensive:

#### Tier 1: Historical data (already implemented)

Query `bytes_transferred` from SQLite for the last successful send of the same type. Apply 1.2x safety margin.

- **When it works:** After the first successful send
- **Cost:** One SQLite query, negligible
- **Accuracy:** Good for incrementals, good-enough for full sends of stable subvolumes

#### Tier 2: Filesystem-level upper bound (new, cheap)

For full sends where Tier 1 has no data: compare the **source filesystem's total used space** against the **destination's free space**. If the entire source filesystem is larger than the destination's free space, no individual subvolume can possibly fit.

This is a coarse check but catches the obvious cases: "this 8TB pool can't possibly fit on a 2TB drive."

Implementation:
- Call `filesystem_free_bytes()` on the source subvolume's path (already available via `statvfs`)
- Call `filesystem_free_bytes()` on the destination drive
- If source filesystem total used > destination free, skip

More precisely: get the source filesystem's *used* bytes. This requires either:
- `statvfs`: `total_blocks - available_blocks` gives used space (but this is filesystem-wide)
- `btrfs filesystem usage`: more accurate, accounts for metadata

This catches: "The btrfs-pool has 12TB of data, 2TB-backup has 1.3TB free — even the smallest subvolume probably won't trigger a false skip, but the 1TB+ subvolumes definitely won't fit."

The weakness: a filesystem with 12TB used might have nine subvolumes of varying sizes. The 50GB docs subvolume fits fine on 2TB-backup even though the pool total is 12TB. So this tier only catches the "destination is clearly too small for anything from this pool" case.

**Refinement — per-root-group check:**
Since `local_snapshots.roots` groups subvolumes by filesystem, we could track which subvolumes share a filesystem and compare counts. If a root has 7 subvolumes and the filesystem uses 12TB, the average is ~1.7TB per subvolume. If the destination has 1.3TB free, flag a warning. This is still coarse but better than nothing.

- **Cost:** One additional `statvfs` call per source root, per plan run
- **Accuracy:** Low — only catches the "obviously impossible" cases
- **Sudoers:** No new entries needed (`statvfs` is unprivileged)

#### Tier 3: Source subvolume size estimation (new, moderate cost)

For full sends where Tier 1 has no data and Tier 2 didn't skip: estimate the specific subvolume's data size.

**Option A: Opportunistic qgroup query**

Try `sudo btrfs qgroup show -reF <path>`. If quotas are enabled, this returns per-subvolume "referenced" bytes in one fast query. If quotas are disabled, the command fails — fall back gracefully.

```rust
fn try_qgroup_size(&self, subvol_path: &Path) -> Option<u64> {
    // Run: sudo btrfs qgroup show -reF <filesystem_path>
    // Parse the output for the subvolume's qgroup entry
    // Return referenced bytes, or None if quotas disabled/parse fails
}
```

- **Cost:** One subprocess call, fast if quotas are enabled
- **Accuracy:** Exact subvolume data size
- **Sudoers:** Needs new entry: `btrfs qgroup show /mnt/btrfs-pool` (read-only, safe)
- **Failure mode:** If quotas are disabled, returns None — Tier 2 or fail-open applies

**Option B: `du -s` on source (not snapshot)**

Run `du -s --apparent-size <source_path>` to measure the live subvolume. This gives the current data size, which is a close estimate of what a full send of a recent snapshot would be.

- **Cost:** High — walks entire directory tree. Minutes for large subvolumes.
- **Accuracy:** Very accurate upper bound
- **Sudoers:** No new entries needed (read-only traversal as user)
- **Failure mode:** Slow. Unacceptable as a per-run check.

**Option C: Cached `du` with TTL**

Run `du -s` once, store the result in SQLite with a timestamp. Reuse the cached value for subsequent plans until it expires (e.g., 7-day TTL for priority 3 subvolumes, 1-day for priority 1). A background job or `urd calibrate` command could refresh stale entries.

```sql
CREATE TABLE subvolume_sizes (
    subvolume TEXT PRIMARY KEY,
    estimated_bytes INTEGER NOT NULL,
    measured_at TEXT NOT NULL,
    method TEXT NOT NULL  -- "qgroup", "du", "send_history"
);
```

- **Cost:** One-time expensive measurement, then free lookups
- **Accuracy:** Good, degrades as subvolume grows between measurements
- **Complexity:** New table, new command or background logic, TTL management

### Recommendation

**Phase 1 (implement now):**
- Tier 1 is already done
- Add Tier 2 as a cheap sanity check — one `statvfs` call on the source path's filesystem. If the filesystem total used exceeds destination free × number of subvolumes on that root, log a warning. This is ~10 lines of code.

**Phase 2 (implement after cutover):**
- Add Option A (opportunistic qgroup query). Try it; if quotas aren't enabled, fall back. If they are, you get exact sizes for free.
- Add a `urd calibrate` command that runs `du -s` on each source subvolume and stores results in SQLite. The user runs this once manually; the planner uses cached sizes as Tier 3 fallback.

**Phase 3 (if needed):**
- If qgroups aren't enabled and `urd calibrate` is too manual, consider enabling qgroups on the btrfs-pool and measuring the performance impact. Modern BTRFS (kernel 6.x) has significantly improved qgroup performance.

### Sudoers changes needed

For Tier 3 Option A (qgroup query):
```
patriark ALL=(root) NOPASSWD: /usr/sbin/btrfs qgroup show /mnt/btrfs-pool
patriark ALL=(root) NOPASSWD: /usr/sbin/btrfs qgroup show /
```

For `btrfs filesystem usage` (if used for accurate destination free space):
```
patriark ALL=(root) NOPASSWD: /usr/sbin/btrfs filesystem usage /run/media/patriark/*
patriark ALL=(root) NOPASSWD: /usr/sbin/btrfs filesystem usage /mnt/btrfs-pool
```

---

## Implementation Priority

| Feature | Effort | Value | Priority |
|---------|--------|-------|----------|
| Progress counter during sends | Low-Medium | High (UX) | Do first |
| Tier 2 filesystem upper bound | Low | Medium | Do with progress |
| Tier 3a opportunistic qgroup | Medium | High (accuracy) | After cutover |
| `urd calibrate` command | Medium | Medium | After cutover |

The progress counter is the highest-value change — it transforms the user experience for every send, not just the edge case of too-large subvolumes. The Tier 2 check is cheap enough to bundle with it. Tier 3 can wait until Urd is the sole backup system and there's real-world data on how often the first-send estimation gap matters.
