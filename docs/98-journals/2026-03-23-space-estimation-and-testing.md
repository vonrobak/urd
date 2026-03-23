# Session Journal: Space Estimation & First Real-World Testing

**Date:** 2026-03-23
**Base commit:** `068f63c` (Phase 4 complete)

## Part 1: What Was Done

### Space estimation feature (+315 / -21 lines, 7 files, 8 new tests)

The planner had no pre-send space checking — sends to undersized drives would run for hours and fail. Built a historical-data-based estimation system:

- **`state.rs`** — New `last_successful_send_size(subvol, drive, send_type)` query. Returns most recent `bytes_transferred` from a successful send of the same type. 5 unit tests.
- **`plan.rs`** — Extended `FileSystemState` trait with `last_send_size()`. Changed `RealFileSystemState` from unit struct to `RealFileSystemState<'a>` carrying `Option<&'a StateDb>`. Added space check in `plan_external_send`: queries history, applies 1.2x margin, compares against `free - min_free`. Skips with descriptive message if it won't fit. First-ever sends (no history) proceed. 3 unit tests.
- **`commands/backup.rs`** — Moved `StateDb::open` before planning (was after). Same instance shared with executor. Eliminated duplicate open.
- **`commands/plan_cmd.rs`** — Opens `StateDb` so `urd plan` shows space-based skips.
- **`commands/{status,verify,init}.rs`** — Updated to `RealFileSystemState { state: None }`.

Arch-adversary review: `docs/99-reports/2026-03-23-arch-adversary-space-estimation.md`. Score 4-5/5 across dimensions. No critical findings. Two moderate items: pipe bytes vs. on-disk size mismatch (1.2x margin handles common case), and space-skip visibility in plan output.

### First real-world backup testing

All commands run from the installed binary (`cargo install --path .`).

| Step | Command | Result |
|------|---------|--------|
| Init | `urd init` | All checks passed. Cleaned 5 partial snapshots on 2TB-backup from interrupted bash transfers. |
| Plan | `urd plan` | 8 snapshots, 7 sends planned. All sends are full (no incremental chain from bash-era pins). |
| subvol6-tmp | `urd backup --subvolume subvol6-tmp` | 0.9s. Local snapshot only (`send_enabled=false`). |
| htpc-root | `urd backup --subvolume htpc-root` | 0.5s first run (snapshot only). 553s second run (full send to WD-18TB1 + 2TB-backup). |
| subvol1-docs | `urd backup --subvolume subvol1-docs` | 196s. Full send to 2TB-backup. Chain now incremental. |
| htpc-home | `urd backup --subvolume htpc-home` | 652s. Full send to 2TB-backup (~11 min for full home dir). |

Post-test `urd plan` confirms subvol1-docs and htpc-home are now incremental for subsequent sends.

### Proposal and adversary review of next features

Wrote proposal: `docs/99-reports/2026-03-23-proposal-progress-and-size-estimation.md`. Covers progress indication during sends and proactive size estimation for first-ever sends.

Adversary review of that proposal: `docs/99-reports/2026-03-23-arch-adversary-proposal-review.md`. Key findings:

1. **Record `bytes_transferred` from failed sends** — Highest-value change not in original proposal. The copy thread already counts bytes before the pipe breaks. Recording this makes the system self-healing after one failure.
2. **Drop Tier 2** (filesystem-level average) — Wrong in both directions for the actual data distribution (7 subvolumes from ~50GB to ~3TB, average lies).
3. **Drop qgroup option from proposal** — Quotas confirmed off via `btrfs subvolume show` output (`Quota group: n/a`). However, enabling quotas retroactively was discussed as potentially superior to `du -s` caching. See Part 3.
4. **Keep progress callback out of `BtrfsOps` trait** — Use `AtomicU64` counter in `RealBtrfs`, polled by executor. Trait stays clean.
5. **Calibrate on snapshots, not live sources** — `du -s` should measure the newest snapshot, not the live subvolume.

### Real-world btrfs command output collected

Ran `btrfs filesystem usage` on all three filesystems (NVMe root, btrfs-pool, WD-18TB1) and `btrfs subvolume show` on all pool subvolumes. Key data:

- btrfs-pool: 10.87TiB used, 3.66TiB free, 4 devices, data single, metadata RAID1
- NVMe root (/home): 77.64GiB used, 39.40GiB free
- WD-18TB1: 13.84TiB used, 2.49TiB free, LUKS-encrypted
- All subvolumes: `Quota group: n/a` — quotas not enabled

---

## Part 2: Hand-Off for Next Session

### Context

The next session should enter planning mode and build two features:

1. **Progress indication during sends**
2. **Proactive size estimation for first-ever sends**

### Required reading

- `CLAUDE.md` — project conventions, module responsibilities
- `docs/PLAN.md` — architecture
- `docs/99-reports/2026-03-23-arch-adversary-proposal-review.md` — the reviewed and revised design direction
- `docs/99-reports/2026-03-23-proposal-progress-and-size-estimation.md` — original proposal (superseded by adversary review findings on several points)

### Revised design (post-adversary-review)

**Progress counter:**
- Replace `std::io::copy` in `btrfs.rs:142-145` with a chunked copy loop updating an `AtomicU64` byte counter
- Do NOT add a progress callback to the `BtrfsOps` trait — keep the trait unchanged
- The executor creates the counter, constructs `RealBtrfs` with it (or passes it per-send), and polls it from a display thread
- Only display progress when stdout is a TTY (`std::io::IsTerminal`)
- Use `\r` to overwrite the line. Show: bytes transferred, rate, elapsed. When estimated total is known, show percentage and ETA
- `MockBtrfs` is untouched

**Size estimation — three changes, in priority order:**

1. **Record `bytes_transferred` from failed sends.** In `btrfs.rs`, capture the partial byte count from the copy thread even when send/receive fails. Return it alongside the error. In `executor.rs`, record it in SQLite. In the planner, query failed sends with non-null `bytes_transferred` as a lower bound (a failed send that transferred 1.1TB proves the subvolume is at least 1.1TB). This is the highest-value change — it makes the system self-heal after one failure.

2. **`urd calibrate` command.** New command that runs `du -s` on the newest local snapshot for each subvolume. Stores results in a new `subvolume_sizes` SQLite table (`subvolume TEXT PRIMARY KEY, estimated_bytes INTEGER, measured_at TEXT, method TEXT`). Schema migration via `CREATE TABLE IF NOT EXISTS`. The planner queries this as a fallback when Tier 1 (historical send data, including from failures) has no data. Tier 1 always takes priority over calibration.

3. **If qgroups are enabled (see Part 3):** `urd calibrate` can optionally use `btrfs qgroup show -reF` instead of `du -s`. Instant, always-current, no TTL needed. The `method` column in `subvolume_sizes` distinguishes "qgroup" from "du". If qgroups are enabled, calibration is essentially free and could run automatically at the start of every `urd backup`.

**What NOT to build:**
- Tier 2 (filesystem-level average check) — dropped per adversary review
- Tier 3 Option A as a separate feature — fold into `urd calibrate` if quotas happen to be enabled

### Key files to modify

| File | Change |
|------|--------|
| `src/btrfs.rs` | Chunked copy with `AtomicU64` counter; return partial bytes on failure |
| `src/executor.rs` | Poll counter for progress display; record partial bytes from failed sends |
| `src/state.rs` | New `subvolume_sizes` table; query method for calibrated sizes |
| `src/plan.rs` | Extend `FileSystemState` with `calibrated_size()`; query failed sends in `last_send_size` |
| `src/commands/calibrate.rs` | New command: `du -s` on newest snapshots, or qgroup if available |
| `src/cli.rs` | Add `Calibrate` subcommand |

### Testing strategy

- Unit tests with `MockBtrfs` for failed-send byte recording
- Unit tests for calibration-based space estimation in planner
- Manual test: run `urd calibrate`, verify sizes in `urd status` or similar, then `urd plan` shows space-based skips for oversized subvolumes on 2TB-backup

---

## Part 3: Enabling BTRFS Quotas (qgroups) on btrfs-pool

Quotas give instant per-subvolume size data, eliminating the need for slow `du -s` walks. On kernel 6.18 with modern btrfs-progs, the performance overhead is acceptable.

### Prerequisites

- Kernel 6.7+ for simple quotas (`squota` mode) — you have 6.18, confirmed
- btrfs-progs 6.7+ for `--simple` flag — check with `btrfs --version`
- The filesystem must be mounted read-write

### Step-by-step

**1. Check btrfs-progs version:**
```bash
btrfs --version
```
Need 6.7+ for `--simple`. If older, use `btrfs quota enable /mnt/btrfs-pool` without the flag (works but uses the older, slower accounting mode).

**2. Enable quotas in simple mode:**
```bash
sudo btrfs quota enable --simple /mnt/btrfs-pool
```
If `--simple` is not supported by your btrfs-progs version, use:
```bash
sudo btrfs quota enable /mnt/btrfs-pool
```
Simple quotas (squota) track accounting at the extent level rather than the subvolume tree level. This is significantly faster for snapshot creation and deletion — the exact operations Urd performs frequently.

**3. Wait for the initial accounting scan.**

The scan runs in the background. There is no progress indicator. On a 10.87TiB pool with ~80 snapshots, expect 1-6 hours depending on extent count and disk load. During the scan, qgroup sizes report as 0.

Check if the scan is complete:
```bash
sudo btrfs qgroup show -reF /mnt/btrfs-pool
```
When the `rfer` (referenced) column shows non-zero values for your subvolumes, the scan is done. Example expected output:
```
Qgroupid    Referenced    Exclusive
--------    ----------    ---------
0/256       195.23GiB     12.45GiB    (subvol1-docs)
0/257       847.91GiB    102.33GiB    (subvol2-pics)
0/258         2.89TiB    456.78GiB    (subvol3-opptak)
...
```

The `Referenced` column is what Urd needs — it's the total data reachable from the subvolume, which closely approximates the full send stream size.

**4. Verify with a known subvolume:**
```bash
# Compare qgroup referenced size against a known send size
sudo btrfs qgroup show -reF /mnt/btrfs-pool | grep docs
# Then check: urd history --subvolume subvol1-docs --last 1
# The bytes_transferred from the full send should be close to the referenced size
```

**5. Benchmark snapshot performance (optional but recommended):**
```bash
# Time a snapshot create + delete cycle with quotas enabled
time sudo btrfs subvolume snapshot -r /mnt/btrfs-pool/subvol6-tmp /mnt/btrfs-pool/.snapshots/subvol6-tmp/quota-test
time sudo btrfs subvolume delete /mnt/btrfs-pool/.snapshots/subvol6-tmp/quota-test
```
On kernel 6.18 with simple quotas, snapshot create should be <1 second. If it's noticeably slower (>5 seconds), the overhead may be too high for the 15-minute htpc-home snapshot interval. In that case, consider enabling quotas only on btrfs-pool (not on the NVMe root filesystem where htpc-home lives).

**6. Add sudoers entry for Urd:**
```bash
# Add to /etc/sudoers.d/btrfs-backup:
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs qgroup show /mnt/btrfs-pool
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs qgroup show /mnt
```
Note: `btrfs qgroup show` requires the filesystem mount point, not a subvolume path. `/mnt/btrfs-pool` may not work if the pool is mounted at `/mnt` — test which path works.

**7. If performance is unacceptable, disable:**
```bash
sudo btrfs quota disable /mnt/btrfs-pool
```
This removes all qgroup data. Re-enabling requires another full scan.

### What about the NVMe root filesystem?

htpc-home and htpc-root live on the NVMe (`/`). These are small enough that `du -s` is fast (77GB, <30 seconds). Enabling quotas on the root filesystem is higher-risk (affects all system writes) for lower reward. Recommendation: leave quotas off on NVMe, use `du -s` calibration for htpc-home and htpc-root.

### Impact on Urd when fully operational

When Urd runs at full cadence (15-minute snapshots for htpc-home, hourly for priority 1), the btrfs-pool will accumulate ~200-400 local snapshots across all subvolumes at steady state (governed by retention). Each snapshot create/delete updates qgroup accounting. With simple quotas on kernel 6.18, this should add <100ms per operation — negligible against the I/O time of the snapshot itself.

The real benefit: `urd calibrate` becomes instant (one `btrfs qgroup show` call vs. 7 `du -s` walks), and calibration data is always current. No TTL, no staleness, no silent skips from outdated sizes.
