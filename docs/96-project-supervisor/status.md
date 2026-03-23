# Urd Project Status

> This is the project's living status tracker. When starting a new session, read this
> document first to understand where things stand, then follow links to relevant details.

## Current State

**Phase 4 code is complete. Post-cutover features (Priorities 2-4) are built and reviewed.
Operational cutover has not started.** The bash script (`btrfs-snapshot-backup.sh`) is still
the sole production backup system, running nightly at 02:00 via `btrfs-backup-daily.timer`.
Urd v0.1.0 is installed (`~/.cargo/bin/urd`) and has been tested manually on real subvolumes
(2026-03-23), but is not deployed on a schedule.

Post-Phase 4 work completed: pre-send space estimation (historical data-based), documentation
system (CONTRIBUTING.md), first real-world backup testing (subvol6-tmp, htpc-root,
subvol1-docs, htpc-home — all successful, chains now incremental for tested subvolumes),
failed-send bytes recording, live progress display during sends, and `urd calibrate` command.
All three post-cutover features passed adversary review (scores 4-5 across all dimensions)
and review-identified fixes have been applied.

## What to Build Next — Priority Order

The operational cutover is the gate — nothing else matters until Urd is running production
backups. All post-cutover code features (Priorities 2-4) are complete.

### Priority 1: Operational Cutover (Phase 4 ops)

**Why first:** All code improvements are valueless until Urd is the production backup system.
The code is tested and ready. This is purely operational — no code changes needed.

See [Step-by-step cutover checklist](#active-work--operational-cutover) below.

**Blocking question:** The parallel run requires 1-2 weeks of monitoring. Starting the
cutover now means Urd becomes sole backup system by mid-April 2026.

### Priority 2: Phase 5 — Sentinel daemon + udev rules

**Why deferred:** Requires Urd to be battle-tested as sole backup system first. The nightly
timer provides reliable scheduled backups. Drive-plug-triggered backups add convenience but
not safety.

**Scope:** `sentinel.rs` (stubbed), udev rules, `urd-sentinel.service`. Desktop notification
hooks.

### Completed (Priorities 2-4)

These features are built, adversary-reviewed, and ready to ship:

- **Failed send bytes** (P2) — `UrdError::Btrfs` carries `bytes_transferred: Option<u64>`.
  Failed sends record partial byte counts. Planner uses MAX(successful, failed) for estimation.
  System self-heals after one failed send. [Journal](../98-journals/2026-03-23-post-cutover-features.md)
- **Progress display** (P3) — `AtomicU64` counter in `RealBtrfs`, polled by display thread in
  `backup.rs`. Shows `bytes @ rate [elapsed]` on stderr when TTY. Counter stays outside
  `BtrfsOps` trait. [Journal](../98-journals/2026-03-23-post-cutover-features.md)
- **`urd calibrate`** (P4) — Measures snapshot sizes via `du -sb`, stores in `subvolume_sizes`
  table. Planner uses as Tier 3 fallback for first-ever full sends. Tier 1 always overrides.
  Staleness warning at 30 days. [Journal](../98-journals/2026-03-23-post-cutover-features.md)

Review fixes applied: progress timer reset between sends, reject 0-byte calibration, corrupt
timestamp staleness handling, space check deduplication, ANSI line clearing.
[Review](../99-reports/2026-03-23-post-cutover-features-review.md)

### Not Building (dropped per adversary review)

- **Tier 2 filesystem-level upper bound** — wrong in both directions for the actual data
  distribution (7 subvolumes from ~50GB to ~3TB). Average-based check would false-positive
  on small subvolumes and false-negative on large ones.
- **Tier 3 Option A opportunistic qgroup query** — quotas confirmed off. Speculative
  complexity for hypothetical future users. Can be added if quotas are ever enabled (see
  [qgroup guide](../98-journals/2026-03-23-space-estimation-and-testing.md#part-3-enabling-btrfs-quotas-qgroups-on-btrfs-pool)).

## Phase Checklist

- [x] **Phase 1** — Skeleton + Config + Plan (67 tests)
- [x] **Phase 1.5** — Hardening (unsent protection, path safety, pin-on-success)
- [x] **Phase 2** — Executor + State DB + Metrics + `urd backup`
- [x] **Phase 3** — CLI commands (`status`, `history`, `verify`) + systemd units
- [x] **Phase 3.5** — Hardening for cutover (adversary review fixes)
- [x] **Phase 4 code** — Cutover polish + space estimation + real-world testing
- [ ] **Phase 4 cutover** — Operational transition from bash to Urd (see below)
- [x] **Post-cutover features** — failed-send bytes, progress, calibrate (Priorities 2-4)
- [ ] **Phase 5** — Sentinel daemon + udev rules (deferred)
- [ ] Notifications (desktop + Discord, deferred until core is battle-tested)

## Active Work — Operational Cutover

These are the remaining steps to complete Phase 4. They are operational actions, not code.
See [Phase 4 journal](../98-journals/2026-03-22-urd-phase4.md) section "What Was NOT Built".

**Cross-repo ownership:** The bash backup units (`btrfs-backup-daily.*`) are owned by
`~/containers`. Modifying or disabling them is a `~/containers` operation. Urd's units
(`urd-backup.*`) are owned by this repo. See [deployment conventions](../../CONTRIBUTING.md#systemd-deployment)
for details.

### Step 1: Install Urd units (this repo)

- [ ] Install Urd systemd units: `cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/`
- [ ] Reload and enable: `systemctl --user daemon-reload && systemctl --user enable --now urd-backup.timer`

### Step 2: Parallel run (requires action in ~/containers repo)

- [ ] _(~/containers)_ Shift bash timer to 03:00: `systemctl --user edit btrfs-backup-daily.timer` → `OnCalendar=*-*-* 03:00:00`
- [ ] Verify both systems run nightly and produce equivalent results (compare Prometheus metrics, snapshot directories, pin files)
- [ ] Run parallel for at least 1 week, ideally 2

### Step 3: Cutover (requires action in ~/containers repo)

- [ ] _(~/containers)_ Disable bash timer: `systemctl --user disable --now btrfs-backup-daily.timer`
- [ ] Monitor Urd as sole system for 1 week
- [ ] Verify Grafana dashboard continuity (metrics names/labels must match)

### Step 4: Cleanup (cross-repo)

- [ ] _(~/containers)_ Archive bash script: `mv ~/containers/scripts/btrfs-snapshot-backup.sh ~/containers/scripts/archive/`
- [ ] _(~/containers)_ Update backup documentation to reference Urd as the backup system
- [ ] _(~/containers)_ Remove bash backup units from `~/containers/systemd/` and `~/.config/systemd/user/`
- [ ] _(this repo)_ Write ADR-021: migration decision record
- [ ] _(this repo)_ Clean up legacy `.last-external-parent` pin files (wait 30+ days after bash retirement)

## Recent Decisions

| Decision | Date | Reference |
|----------|------|-----------|
| Daily external sends for Tier 1/2 (RPO 7d → 1d) | 2026-03-21 | [ADR-020](../00-foundation/decisions/ADR-relating-to-bash-script/2026-03-21-ADR-020-daily-external-backups.md) |
| Pre-send space estimation using historical data | 2026-03-23 | [Journal](../98-journals/2026-03-23-space-estimation-and-testing.md) |
| Drop Tier 2 and qgroup option from size estimation | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) |
| Keep progress counter out of BtrfsOps trait | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) Finding 4 |
| Calibrate on snapshots, not live sources | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) Finding 5 |
| UrdError::Btrfs struct variant (not separate type) for partial bytes | 2026-03-23 | [Post-cutover journal](../98-journals/2026-03-23-post-cutover-features.md) |
| MAX(successful, failed) for send size estimation | 2026-03-23 | [Post-cutover journal](../98-journals/2026-03-23-post-cutover-features.md) |

## Key Documents

| Purpose | Document |
|---------|----------|
| Original roadmap & architecture | [roadmap.md](roadmap.md) |
| Latest journal entry | [2026-03-23 Post-implementation review fixes](../98-journals/2026-03-23-post-implementation-review-fixes.md) |
| Latest adversary review | [2026-03-23 Post-cutover features review](../99-reports/2026-03-23-post-cutover-features-review.md) |
| Post-cutover features journal | [2026-03-23 Post-cutover features](../98-journals/2026-03-23-post-cutover-features.md) |
| Progress & size estimation proposal | [2026-03-23 Proposal](../99-reports/2026-03-23-proposal-progress-and-size-estimation.md) |
| Code conventions & architecture | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |

## Known Issues & Tech Debt

- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- Space-skip visibility in plan output could be improved (`[SKIP:SPACE]` marker suggested)
- `du -sb` may follow symlinks in snapshots — consider `-P` flag (not yet tested on real snapshots with symlinks)
- Stale failed send estimates persist indefinitely for (subvolume, drive, send_type) triples with no subsequent sends — consider TTL or clearing on successful calibration
- Successful sends could update `subvolume_sizes` table to keep calibration fresh, but pipe bytes ≠ `du -sb` bytes (method mixing concern)
- `sentinel.rs` stubbed but not implemented (Phase 5)
- Per-drive pin protection for external retention — current all-drives-union is conservative but suboptimal for space
- Idea: [systemd unit drift check](../95-ideas/2026-03-23-systemd-unit-drift-check.md) in `urd verify`
