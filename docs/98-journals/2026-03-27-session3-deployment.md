# Session Journal: Notification Dispatcher, Voice Migration, and Deployment (Session 3)

> **TL;DR:** Built notification dispatcher (5a), completed init voice migration (8/8 commands),
> tuned config with protection promises, deployed v0.3.2026-03-27 to production. First real
> backup with protection promises runs tonight at 04:00. 318 tests, clippy clean. PR #26 open.

**Date:** 2026-03-27
**Base commit:** `c77b147` (session 2, PR #25 merged)
**End commit:** `bd75efc` (pushed, PR #26 open)
**Branch:** `feat/protection-promises-types`
**PR:** https://github.com/vonrobak/urd/pull/26
**Version:** 0.3.2026-03-27 (installed via `cargo install --path .`)

## What was done

### 1. Operational config tuning

Updated `config/urd.toml.example` and deployed to `~/.config/urd/urd.toml`:

- `run_frequency = "daily"` explicit (was commented out, defaulted silently)
- Defaults aligned to 1d/1d (was 1h/4h — meaningless with daily timer)
- All 9 subvolumes assigned protection levels:
  - **Resilient** (2 drives): htpc-home, opptak, pics
  - **Protected** (any drive): docs, containers, music
  - **Guarded** (local only): htpc-root, multimedia, tmp
- Resilient subvolumes pinned to `drives = ["WD-18TB", "WD-18TB1"]` (opptak exceeds
  2TB-backup capacity; htpc-home and pics are irreplaceable)
- Organized by promise level instead of priority number
- htpc-root: changed from weekly send to guarded (no send). Root is low-value on this
  containerized system. Existing external snapshots remain as archival safety net.

**Behavioral changes from previous config:**
- htpc-home no longer sends to 2TB-backup (restricted to 18TB drives)
- htpc-root no longer sends externally (guarded)
- Snapshot intervals for htpc-home, opptak, containers, docs now derived as 1d (was 1h–6h,
  but timer ran daily so effective frequency was already 1d)
- No new snapshots will be created for subvolumes whose 1d interval hasn't elapsed

### 2. Notification dispatcher (`src/notify.rs`)

New module implementing Sentinel component 5a. Key elements:

- **`compute_notifications()`** — pure function comparing previous/current heartbeat.
  Detects: PromiseDegraded, PromiseRecovered, BackupFailures, AllUnprotected. BackupOverdue
  variant defined for Sentinel (5b), not computed by backup command.
- **Urgency** — Info (recoveries), Warning (degradations), Critical (all unprotected, all
  failed). Configurable minimum threshold filters what gets dispatched.
- **4 channels** — Desktop (notify-send), Webhook (curl subprocess), Command (subprocess
  with URD_NOTIFICATION_* env vars), Log (log::info/warn/error).
- **Config** — `[notifications]` section in urd.toml. Optional, defaults to disabled.
  No channels configured in production yet (validate the compute logic first).
- **Crash recovery** — `notifications_dispatched` boolean on Heartbeat. Written false at
  heartbeat creation, set true after dispatch. Next run (or Sentinel) can retry if false.
- **Mythic voice** in notification text: "The thread of {name} has frayed" (degradation),
  "Every thread in the well has snapped" (all unprotected).
- **Integration** — backup.rs reads old heartbeat before writing new one (comparison window),
  computes notifications, dispatches, marks dispatched. Both the empty-run and execution paths
  are wired up.
- **18 tests** covering state transitions, urgency ordering, config parsing, filtering.

Design decision: webhook via `curl` subprocess, not `ureq` crate. Keeps Urd dependency-light.
Acceptable for v1 where notifications are non-critical.

### 3. Init voice migration

Last command migrated to voice layer (8/8 complete):

- `InitOutput` struct with 8 sections in `output.rs`
- `render_init()` in `voice.rs` (interactive + daemon modes)
- Interactive deletion prompt for incomplete snapshots stays in the command (I/O belongs
  in commands, rendering in voice — correct separation)
- 2 tests (interactive content, daemon JSON validity)

### 4. Version bump and deployment

- Cargo.toml: 0.2.2026-03-24 -> 0.3.2026-03-27
- Release build with LTO + strip
- `cargo install --path .` replaces binary at `~/.cargo/bin/urd`
- systemd timer at 04:00 will use the new binary

## Expected behavior over the coming days

### Tonight's backup (2026-03-28 04:00)

This is the first real run with protection promises. Expected behavior:

**Snapshots created:**
- All subvolumes where 24h has elapsed since last snapshot (most of them — last run was
  2026-03-26 04:04). htpc-root creates a new snapshot (1w override, previous snapshots
  cleaned by tight retention).
- Guarded subvolumes (multimedia, tmp) create local snapshots, no sends.

**Sends:**
- WD-18TB1 (mounted): incremental sends for htpc-home, opptak, pics, docs, containers,
  music. All have existing pin files pointing to 20260325/20260326 snapshots.
- 2TB-backup (mounted): incremental sends for docs, containers. Music skipped (calibrated
  ~1.3TB exceeds available ~1.0TB). htpc-home, opptak, pics excluded by `drives` restriction.
- WD-18TB (not mounted): all sends skipped with "not mounted" reason.

**Retention:**
- `--confirm-retention-change` NOT set (default). Retention deletions for all promise-level
  subvolumes will be SKIPPED. This is the fail-closed gate from ADR-107. Log will show:
  "Skipped N retention deletion(s) for promise-level subvolumes."
- This is intentional for the first run — observe before allowing deletions.

**Heartbeat:**
- Written with `notifications_dispatched: false`, then updated to `true` after dispatch.
- First heartbeat with the new field. Previous heartbeat (if it exists) won't have the
  field — `#[serde(default = "default_dispatched")]` handles this (defaults to true,
  meaning "don't re-send for old heartbeats").

**Notifications:**
- `[notifications]` not configured in production config. `dispatch()` returns immediately
  when `enabled = false` (the default). No notifications will fire.
- The `compute_notifications()` function still runs — any state changes are computed but
  silently discarded. This validates the compute path without side effects.

**Status after run:**
- Local status should be PROTECTED for all subvolumes (fresh daily snapshots).
- External status depends on send success. WD-18TB1 should move from AT RISK to PROTECTED.
  2TB-backup should be PROTECTED for docs/containers.
- htpc-root should move from UNPROTECTED to PROTECTED (new local snapshot created).

### Days 2-3 (2026-03-29 — 2026-03-30)

**Steady state:**
- Each 04:00 run creates one snapshot per subvolume (daily interval), sends to mounted drives.
- Retention deletions continue to be skipped (no `--confirm-retention-change`).
- Snapshot counts will grow by 1/day per subvolume. Watch NVMe space on htpc-home
  (12 snapshots currently, 10GB min_free_bytes guard active).

**What to watch for:**
- Awareness model should report PROTECTED across the board (local + external for
  protected/resilient subvolumes) assuming WD-18TB1 stays mounted.
- If WD-18TB is mounted, resilient subvolumes should get sends to both 18TB drives.
- Preflight warnings about htpc-root and multimedia weakening overrides will appear in
  every log — these are expected and intentional.

### Day 4+ (2026-03-31 onward)

**Retention pressure:**
- Without `--confirm-retention-change`, no retention deletions happen for promise-level
  subvolumes. Snapshot counts will grow indefinitely. For most subvolumes on btrfs-pool
  (50GB min_free_bytes), this is fine for weeks. For htpc-home on NVMe (10GB guard), the
  space guard will eventually skip snapshot creation.
- **Action needed:** Once confident the config is correct, run
  `urd backup --confirm-retention-change` manually to verify retention behavior, then
  consider adding the flag to the systemd unit's ExecStart.

## Verification tests for next session

### Test 1: First-run heartbeat has new field

```bash
# After tonight's backup:
cat ~/.local/share/urd/heartbeat.json | python3 -m json.tool
# Verify:
# - "notifications_dispatched": true
# - All subvolumes have promise_status
# - run_result is "success" or "partial"
```

### Test 2: Promise states in status output

```bash
urd status
# Verify:
# - PROMISE column visible (at least one subvolume has protection_level)
# - htpc-home, opptak, pics show "resilient"
# - docs, containers, music show "protected"
# - htpc-root, multimedia, tmp show "guarded"
# - htpc-root should be PROTECTED (was UNPROTECTED before tonight's run)
```

### Test 3: Retention skip log

```bash
journalctl --user -u urd-backup.service --since "4 hours ago" | grep -i "retention"
# Verify:
# - "Skipped N retention deletion(s) for promise-level subvolumes"
# - This confirms the fail-closed gate is working
```

### Test 4: Drive filtering

```bash
journalctl --user -u urd-backup.service --since "4 hours ago" | grep -i "skip\|send"
# Verify:
# - htpc-home, opptak, pics NOT sent to 2TB-backup (drives restriction)
# - docs, containers sent to both WD-18TB1 and 2TB-backup
# - WD-18TB shows "not mounted" (if still unmounted)
```

### Test 5: Preflight warnings expected

```bash
journalctl --user -u urd-backup.service --since "4 hours ago" | grep "preflight"
# Verify exactly these warnings (intentional overrides):
# - htpc-root: snapshot_interval is longer than guarded baseline
# - htpc-root: local_retention is tighter than guarded baseline
# - subvol4-multimedia: snapshot_interval is longer than guarded baseline
```

### Test 6: Dry-run unchanged

```bash
urd backup --dry-run
# Verify:
# - Plan shows only daily-interval operations (not sub-hourly)
# - Guarded subvolumes show "send disabled"
# - Resilient subvolumes only target WD-18TB and WD-18TB1
```

### Test 7: Notification compute path (no side effects)

```bash
# Run with RUST_LOG=debug to see notification compute:
RUST_LOG=debug urd backup --dry-run 2>&1 | grep -i notif
# No notifications should fire (config not enabled)
# But the compute path runs — check no errors
```

### Test 8: Config parse roundtrip

```bash
# Verify the real config parses without errors:
urd status
urd backup --dry-run
# Both should work without parse errors or panics
```

### Test 9: Enable notifications (optional, manual)

If ready to test notifications end-to-end:

```toml
# Add to ~/.config/urd/urd.toml:
[notifications]
enabled = true
min_urgency = "info"

[[notifications.channels]]
type = "log"
```

Then run `urd backup` and check logs for `[notification]` lines. The log channel is
side-effect-free and verifies the full dispatch path.

### Test 10: Confirm retention (when ready)

```bash
# Preview what retention would delete:
urd backup --dry-run
# Look for DeleteSnapshot operations in the plan

# If comfortable, run with retention enabled:
urd backup --confirm-retention-change
# Verify snapshot counts decrease appropriately
```

## Key files modified this session

| File | Changes |
|------|---------|
| `src/notify.rs` | **New.** Notification types, compute, dispatch, config, 18 tests |
| `src/heartbeat.rs` | `notifications_dispatched` field, `mark_dispatched()` |
| `src/config.rs` | `notifications: NotificationConfig` on Config |
| `src/commands/backup.rs` | `dispatch_notifications()`, heartbeat read reordering |
| `src/commands/init.rs` | Full rewrite: data collection + voice rendering |
| `src/output.rs` | `InitOutput` and 7 related types |
| `src/voice.rs` | `render_init()` interactive/daemon, 2 tests |
| `config/urd.toml.example` | Protection promises, notifications section, aligned intervals |
| `~/.config/urd/urd.toml` | Production config with protection promises deployed |
| `Cargo.toml` | Version 0.3.2026-03-27 |
| `docs/96-project-supervisor/status.md` | Session 3 updates |

## Key documents

- **Session 1 journal:** `docs/98-journals/2026-03-27-protection-promises-session1.md`
- **Session 2 journal:** `docs/98-journals/2026-03-27-protection-promises-session2.md`
- **Sentinel design:** `docs/95-ideas/2026-03-26-design-sentinel.md`
- **ADR-110:** `docs/00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md`
- **Project status:** `docs/96-project-supervisor/status.md`
