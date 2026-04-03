# Urd v0.8.0 Comprehensive Test Plan

> **Purpose:** Systematic verification of v0.8.0 behavior across all drive configurations,
> with special attention to the cloned-UUID WD-18TB/WD-18TB1 swap scenario and 2TB-backup
> interaction. Results feed directly into patch prioritization.
>
> **How to use:** Run each test, paste the terminal output under the test, and note
> PASS/FAIL/OBSERVATION. Tests are ordered to minimize drive swapping.

---

## Phase 0: Baseline (WD-18TB connected, others absent)

Current state. No drive changes needed.

> **Before starting:** Resolve the subvol4-multimedia config warning (T0.0 below).
> Every subsequent test should run against a clean config.

### T0.0 — Resolve multimedia config warning

The config has `snapshot_interval = "1w"` with `protection_level = "guarded"`.
Guarded requires 1d interval. Options:
- a) Change the interval to 1d
- b) Change the protection level to custom
- c) Accept the warning (it's advisory)

**Decision and action taken:**

- snapshot_interval changed from 1w to 1d (matches guarded requirement)
- WD-18TB UUID: attempted to add `647693ed-...` but Urd rejects duplicate UUIDs
  across drives (`Configuration error: duplicate drive uuid`). Reverted — UUID
  verification is impossible for cloned drives until `btrfstune -u` is run on
  WD-18TB1. Drive tokens remain the only identity layer for these drives.
- **Finding F0.0:** Urd has no way to express "these drives share a UUID intentionally."
  Doctor advises adding the UUID but config rejects it. Contradictory guidance.
  Options: (a) downgrade to warning, (b) add a `clone_of` or `skip_uuid_check` field,
  (c) accept as known limitation until btrfstune resolves it.

### T0.1 — Verify v0.8.0 installed

```
urd --version
```

Expected: `urd 0.8.0`

**Result:** PASS. `urd 0.8.0` confirmed.

### T0.2 — Status baseline

```
urd status
```

Record: exposure/health for each subvolume, drive status lines, pinned snapshot count.
This is the reference point for all later comparisons.

**Result:** PASS. Baseline recorded:

- All 9 subvolumes sealed. 3 degraded (htpc-home, subvol2-pics, htpc-root).
- Degradation cause: 2TB-backup absent 10d. These subvolumes send to all drives
  (no explicit `drives = [...]`), so absent 2TB-backup triggers degradation.
- WD-18TB connected (4.2TB free). WD-18TB1 absent 10d. 2TB-backup absent 10d.
- Last backup: run #22, 2026-04-02T19:25:16 (success, 1m 52s).
- Pinned snapshots: 21 across subvolumes.
- All threads unbroken.

### T0.3 — Manual backup (v0.8.0 feature: immediate mode)

```
urd backup --dry-run
```

Expected: Pre-action briefing shown. All subvolumes planned (no interval gating).
Verify this differs from `urd backup --auto --dry-run` which should skip recently-backed-up subvolumes.

Also check: In the skipped section, do guarded subvolumes (subvol4-multimedia, subvol6-tmp)
show as `[OFF] Disabled`? If so, note this — they're local-only by design, not disabled.
This is a vocabulary issue worth tracking.

```
urd backup --auto --dry-run
```

Expected: Should show empty plan or skip most subvolumes (backed up ~1h ago).
Note the mode-aware empty-plan message if nothing is due.

**Result:** PASS.

- `urd backup --dry-run`: 7 sends (~7.0GB), 9 snapshots, 0 deletions, 13 skipped.
  All subvolumes planned — manual mode correctly bypasses intervals.
- `urd backup --auto --dry-run`: "No operations planned." 16 subvolumes skipped
  with `[WAIT] Interval not elapsed` (next in ~6h25m). Auto mode correctly gates.
- **Observation F0.3:** `[OFF] Disabled` label shown for subvol4-multimedia and
  subvol6-tmp. These are local-only (no drives configured), not "disabled." The
  label is misleading — they are actively snapshotted locally, just not sent anywhere.
  Vocabulary issue: should say `[LOCAL]` or similar.

### T0.4 — Plan: manual vs auto views

```
urd plan
urd plan --auto
```

Expected: `plan` shows everything (manual view). `plan --auto` respects intervals.

Also check: Same `[OFF] Disabled` label question as T0.3 — do guarded subvolumes
show accurate skip reasons in the plan output?

**Result:** PASS.

- `urd plan`: 7 sends (~7.0GB), 9 snapshots, 2 deletions (subvol6-tmp graduated
  thinning), 13 skipped. Shows full picture including retention deletions.
- `urd plan --auto`: Only 2 deletions for subvol6-tmp. 16 subvolumes `[WAIT]`.
- Same `[OFF] Disabled` label for subvol4-multimedia, subvol6-tmp.
- **Observation:** `urd plan` shows 2 deletions that `urd backup --dry-run` did not.
  This is because `backup --dry-run` excludes deletions in its view. The plan
  command is the authoritative view of what will happen.

### T0.5 — Lock trigger string

Run a manual backup and immediately check the lock file:

```
urd backup --dry-run &
cat ~/.local/share/urd/urd.lock 2>/dev/null
```

Expected: Lock metadata shows trigger = "manual" (not "timer").

**Result:** PASS. Lock file from run #22 shows `"trigger":"manual"`. Lock persists
after run completes (stale lock with valid metadata).

### T0.6 — History and verify

```
urd history
urd verify
```

Expected: Run #22 visible in history. Verify checks thread integrity.

**Result:** PASS.

- History: 10 runs visible (#13–#22), all `full` mode, all `success`. Run #22 at
  2026-04-02T19:25:16, 1m 52s.
- Verify: 35 OK, 14 warnings, 0 failures. All WD-18TB threads intact (pin exists
  locally and on drive, no orphans, pin age OK). WD-18TB1 and 2TB-backup skipped
  (drive not mounted).

### T0.7 — Doctor thorough

```
urd doctor --thorough
```

Expected: Thread checks run in addition to standard checks.
Note any new warnings beyond the known ones (multimedia interval, WD-18TB UUID).

**Result:** PASS. 15 warnings, 0 issues.

- Config: 9 subvolumes, 3 drives.
- WD-18TB UUID warning persists (contradicts F0.0 — can't add due to clone).
- Sentinel running (PID 15296).
- All 9 sealed.
- 14 thread warnings: 7 for WD-18TB1 (not mounted), 7 for 2TB-backup (not mounted).
- No unexpected warnings.

### T0.8 — Sentinel awareness of manual backup

```
urd sentinel status
```

Expected: Sentinel should have noticed the recent manual backup.
Check assessment timestamp — did it update after run #22?

**Result:** PASS. Sentinel running since 14m (PID 15296). Assessment at
2026-04-02T21:21:35 (tick: 15m — all promises held). Connected: WD-18TB.
Assessment reflects current state.

### T0.9 — File restore from local snapshot

Test `urd get` — Urd's restore path. Pick a known file and restore from yesterday:

```
urd get <path-to-a-known-file> --at yesterday
```

Expected: File content printed to stdout (or use `-o /tmp/restored` to write to file).
Verify the content matches the current file.

Note: `urd get` restores from local snapshots only. External drive restore and directory
restore are not yet implemented (horizon items). This test verifies the core restore path works.

**Result:** NOT TESTED. Only ran `urd get` without arguments, which correctly showed
usage error requiring `--at <AT>` and `<PATH>`. Restore path not exercised due to
session time constraints.

---

## Phase 1: Connect 2TB-backup

Plug in the 2TB-backup drive. Wait for it to mount at `/run/media/patriark/2TB-backup`.

### T1.0 — Pre-test: confirm mount

```
findmnt /run/media/patriark/2TB-backup
findmnt -n -o UUID /run/media/patriark/2TB-backup
```

Expected: UUID should be `973d284e-475b-4eac-8c56-3e3d1cb6a8ed` (matches config).

**Result:** PASS. Drive mounted via LUKS at `/run/media/patriark/2TB-backup`.
UUID `973d284e-475b-4eac-8c56-3e3d1cb6a8ed` confirmed — matches config.

### T1.1 — Sentinel drive detection and user experience

```
# Wait ~30s for sentinel tick, then:
urd sentinel status
journalctl --user -u urd-sentinel -n 20 --no-pager
```

Expected: Sentinel detects 2TB-backup connection. Check if DriveConnected event fires.

**UX observation:** When the drive connected, what did you actually notice as a user?
Was there a desktop notification? Did you have to run a command to find out Urd saw it?
Record the experience — this tells us whether drive reconnection is invisible or guided.

**Result:** PASS (functional). Sentinel detected 2TB-backup:

- `urd sentinel status`: Connected: 2TB-backup, WD-18TB.
- Assessment updated to 2026-04-02T21:40:31 (all promises held).

**UX finding F1.1:** Drive reconnection was completely silent. No desktop notification,
no terminal feedback — the user had to run `urd sentinel status` to discover Urd had
noticed the drive. For a drive flagged as "absent 10d — protection degrading," the
return should feel like relief, not require investigation. This is the invisible-worker
pattern working too well — the reconnection moment deserves user-facing acknowledgment.

### T1.2 — Status with 2TB-backup connected

```
urd status
```

Key questions:
- Does 2TB-backup appear as a column in the status table?
- Which subvolumes show send data for 2TB-backup? (Only those without explicit `drives = [...]` should send to it via defaults — that's subvol1-docs, subvol7-containers, subvol5-music, htpc-root.)
- Does the "degraded" count change? (The assess() scoping bug may affect this.)
- Does the "absent" drive warning for 2TB-backup disappear?

**Result:** PASS.

- 2TB-backup appears as a column in status table.
- Subvolumes with 2TB-backup data: htpc-home (4, 8d), subvol2-pics (2, 8d),
  subvol1-docs (5, 5d), subvol7-containers (6, 5d), htpc-root (1, 10d).
- subvol3-opptak and subvol5-music show `—` for 2TB-backup (no snapshots on drive).
- Degraded count dropped from 3 to 1. htpc-home and subvol2-pics became healthy.
  htpc-root still degraded (chain broken — pin missing locally).
- 2TB-backup drive line: "connected (1.1TB free)" — absent warning gone.
- **Observation:** htpc-home and subvol2-pics have explicit `drives` in config
  (resilient level) that should NOT include 2TB-backup, yet they show data on
  2TB-backup from historical sends. Status correctly reflects what's on disk
  even if current config wouldn't send there. Thread shows "unbroken" for these.

### T1.3 — Plan with 2TB-backup

```
urd plan
```

Key questions:
- Does urd plan sends to 2TB-backup?
- Which subvolumes? (Expected: subvol1-docs, subvol7-containers, subvol5-music, htpc-root — the ones without explicit `drives` scoping.)
- Are the resilient subvolumes (htpc-home, subvol3-opptak, subvol2-pics) correctly excluded from 2TB-backup sends?
- What does the skipped section say?
- Do guarded subvolumes still show `[OFF] Disabled`? (Same vocabulary check as T0.3.)

**Result:** PASS.

- Sends planned to 2TB-backup: subvol1-docs (incremental, parent 20260327, ~123B),
  subvol7-containers (incremental, parent 20260327, ~889.4MB),
  htpc-root (full — chain broken, ~31.8GB).
- subvol5-music: `[SPACE] send to 2TB-backup skipped: estimated ~1.3TB exceeds
  1.0TB available (free: 1.1TB, min_free: 100.0GB)`. Space-aware skip — correct.
- Resilient subvolumes (htpc-home, subvol3-opptak, subvol2-pics) correctly NOT
  planned for 2TB-backup sends. Drive scoping works.
- `[OFF] Disabled` still shown for subvol4-multimedia, subvol6-tmp.
- Footer: "Run `urd calibrate` to review retention, then `urd backup`." — new
  calibrate suggestion in v0.8.0.

### T1.4 — Doctor with 2TB-backup

```
urd doctor --thorough
```

Key questions:
- Does the 2TB-backup UUID check pass?
- Any new warnings or issues?
- Thread status for 2TB-backup subvolumes?

**Result:** PASS (1 issue, 15 warnings).

- 2TB-backup UUID: not explicitly checked by doctor (no "no UUID configured" warning
  for 2TB-backup, so UUID is configured and accepted).
- 1 issue: `htpc-root/2TB-backup: Pinned snapshot missing locally: 20260323-0123-htpc-root
  — Chain broken — next send will be full`. Confirmed chain break.
- New warnings with 2TB-backup connected:
  - Pin file age warnings for htpc-home (8d), subvol2-pics (8d), subvol1-docs (5d),
    subvol7-containers (5d), htpc-root (10d) — all exceed 2-day threshold.
  - subvol3-opptak/2TB-backup: "Pinned snapshot not on this drive: 20260324-opptak
    (legacy pin — run urd backup to establish drive-specific chain)". Legacy pin
    format detected — good diagnostic.

### T1.5 — Drive token state

```
ls -la /run/media/patriark/2TB-backup/.snapshots/.urd-drive-token 2>/dev/null
cat /run/media/patriark/2TB-backup/.snapshots/.urd-drive-token 2>/dev/null
```

Does 2TB-backup have an existing drive token? Record it for comparison.

**Result:** No drive token found on 2TB-backup. Both `ls` and `cat` returned empty.
This is expected for a drive that hasn't been used with v0.8.0's token system yet.
Token will be created on first successful send.

### T1.6 — Backup with 2TB-backup connected

```
urd backup --dry-run
```

If the dry-run looks safe:

```
urd backup
```

Key questions:
- Does the pre-action briefing mention 2TB-backup?
- Which subvolumes send to 2TB-backup?
- Are sends incremental or full? (If snapshots already exist on 2TB-backup, they should be incremental if the chain is intact.)
- Any token issues?

**Result:** PARTIAL (run #23, 284.4s).

- Pre-action briefing: "Backing up everything to 2TB-backup and WD-18TB."
  "WD-18TB1 is away — copies will update when it returns."
- Successful sends to 2TB-backup: subvol1-docs (incremental, 19.2MB),
  subvol7-containers (incremental, 3.7GB in 3:03).
- **FAILED: htpc-root** — `send_full: chain-break full send gated — run
  urd backup --force-full --subvolume htpc-root to proceed`. The chain-break
  full send safety gate correctly prevented an unconfirmed 31.8GB full send.
  This is good safety behavior — full sends from chain breaks require explicit opt-in.
- subvol5-music: skipped for space (same as plan).
- Drive token created for 2TB-backup: `b54d8e10-2b17-4c2a-8e8a-80ac6013e909`
  (recorded in SQLite at 2026-04-02T21:47:54).
- All WD-18TB sends succeeded (incremental).

### T1.7 — Post-backup status

```
urd status
```

Compare with T0.2 baseline. Did promise states improve for any subvolumes?

**Result:** PASS.

- Still 1 degraded (htpc-root — chain broken on 2TB-backup).
- subvol1-docs: 2TB-backup now shows 6 (7m) — up from 5 (5d). Chain refreshed.
- subvol7-containers: 2TB-backup now shows 7 (7m) — up from 6 (5d). Chain refreshed.
- htpc-home, subvol2-pics: 2TB-backup counts unchanged (4, 8d) — no new sends planned
  (correct, they have explicit drive scoping excluding 2TB-backup).
- Last backup: run #23, partial, 4m 45s.

### T1.8 — Existing snapshots on 2TB-backup

```
ls /run/media/patriark/2TB-backup/.snapshots/
```

For each subvolume directory that exists:

```
ls /run/media/patriark/2TB-backup/.snapshots/<subvol>/
```

Record what's already on the drive. This tells us what urd has to work with for incremental chains.

**Result:** 5 subvolume directories on 2TB-backup:

- `htpc-home/`: 4 snapshots (20260323 through 20260325-0046)
- `htpc-root/`: 1 snapshot (20260323-0123-htpc-root) — the broken chain parent
- `subvol1-docs/`: 6 snapshots (20260322 through 20260402-1925) — chain intact
- `subvol2-pics/`: 2 snapshots (20260322, 20260325-0046)
- `subvol7-containers/`: 7 snapshots (20251201 through 20260402-1925) — chain intact

No directories for: subvol3-opptak, subvol5-music, subvol4-multimedia, subvol6-tmp.

### T1.9 — Disconnect 2TB-backup

Safely unmount/eject the drive.

```
urd status
urd sentinel status
```

Confirm urd correctly reflects the disconnection.

**Result:** PASS.

- Status: back to "3 degraded — 2TB-backup away for 8 days." 2TB-backup column
  gone from table. Drive line: "2TB-backup absent 10d — protection degrading."
- Sentinel: Connected: WD-18TB only. Assessment updated to 2026-04-02T21:56:41.

---

## Phase 2: WD-18TB / WD-18TB1 swap

This is the critical test. The drives share a BTRFS UUID from cloning.
Currently WD-18TB is mounted. We will unmount it and mount WD-18TB1 in its place.

### Important context

| Property | WD-18TB | WD-18TB1 |
|----------|---------|----------|
| Config label | WD-18TB | WD-18TB1 |
| Config UUID | (none) | 647693ed-490e-4c09-8816-189ba2baf03f |
| Config mount_path | /run/media/patriark/WD-18TB | /run/media/patriark/WD-18TB1 |
| Config role | primary | offsite |
| Actual BTRFS UUID | 647693ed... (same) | 647693ed... (same) |
| Has drive token? | Check below | Check below |

### T2.0 — Pre-swap: record WD-18TB token and state

```
cat /run/media/patriark/WD-18TB/.snapshots/.urd-drive-token 2>/dev/null
sqlite3 ~/.local/share/urd/urd.db "SELECT * FROM drive_tokens;"
```

Record both the on-disk token and the SQLite-stored tokens for all drives.

**Result:** Recorded.

- On-disk token: `token=e5b824a1-207d-499e-9dd7-e41e5cb742cf` (written 2026-03-29T14:03:03,
  label: WD-18TB).
- SQLite tokens:
  - WD-18TB: `e5b824a1-...`, created 2026-03-29T14:03:03, last seen 2026-04-02T21:47:05
  - 2TB-backup: `b54d8e10-...`, created 2026-04-02T21:47:54, last seen 2026-04-02T21:47:54

### T2.1 — Pre-swap: record WD-18TB snapshot inventory

```
for dir in /run/media/patriark/WD-18TB/.snapshots/*/; do
  echo "=== $(basename $dir) ==="
  ls "$dir" | tail -5
  echo "($(ls "$dir" | wc -l) total)"
done
```

**Result:** WD-18TB inventory:

| Subvolume | Count | Most recent |
|-----------|-------|-------------|
| htpc-home | 5 | 20260402-1925-htpc-home |
| htpc-root | 5 | 20260402-1925-htpc-root |
| subvol1-docs | 5 | 20260402-1925-docs |
| subvol2-pics | 5 | 20260402-1925-pics |
| subvol3-opptak | 5 | 20260402-1925-opptak |
| subvol4-multimedia | 1 | 20251213-multimedia |
| subvol5-music | 5 | 20260402-1925-music |
| subvol6-tmp | 0 | — |
| subvol7-containers | 5 | 20260402-1925-containers |

### T2.2 — Unmount WD-18TB

Safely unmount/eject WD-18TB.

```
urd status
urd sentinel status
```

Expected: All send-enabled subvolumes lose their external copy.
Promise states should degrade (no external drives connected).

**Result:** PASS.

- Status: "7 blocked — no backup drives connected." All send-enabled subvolumes
  show `blocked` health. subvol4-multimedia and subvol6-tmp remain `healthy` (local-only).
- **New feature observed:** REDUNDANCY warning appeared:
  "htpc-root lives only on external drives while local copies are transient.
  Recovery requires a connected drive." — Good proactive guidance.
- Sentinel: Connected: none.
- All drive lines show absent.

### T2.3 — Mount WD-18TB1

Connect WD-18TB1. It should mount at `/run/media/patriark/WD-18TB1`.

```
findmnt /run/media/patriark/WD-18TB1
findmnt -n -o UUID /run/media/patriark/WD-18TB1
```

**Critical question:** Does it mount as WD-18TB1 (its GNOME Disks label) or WD-18TB?
If the filesystem label is identical, GNOME may mount it at an unexpected path.

**UX observation:** Same as T1.1 — what do you experience as a user when this drive
connects? Notification? Silence? This is the offsite drive returning — the moment
should feel like relief, not require investigation.

**Result:** CRITICAL FINDING F2.3.

- `findmnt /run/media/patriark/WD-18TB1` — **EMPTY.** Drive did NOT mount at WD-18TB1.
- `findmnt /run/media/patriark/WD-18TB` — **MOUNTED HERE.** WD-18TB1 mounted at
  WD-18TB's mount path. UUID confirmed: `647693ed-490e-4c09-8816-189ba2baf03f`.
- **Root cause:** The cloned drives share the BTRFS filesystem label "WD-18TB".
  GNOME/udisks2 uses the filesystem label for the mount path, so WD-18TB1 mounts
  at `/run/media/patriark/WD-18TB` — identical to where WD-18TB normally mounts.
- **Impact on Urd:** Urd identifies drives by `mount_path`. With WD-18TB1 mounted
  at WD-18TB's path, Urd believes WD-18TB is connected. It cannot distinguish the
  drives. All subsequent operations would target the wrong drive identity.
- No drive token on WD-18TB1 (`cat` returned empty), but Urd does not gate on
  token absence — it proceeds with the mount path identity.

### T2.4 — Status with WD-18TB1

```
urd status
```

Key questions:
- Does WD-18TB1 appear as connected?
- Is WD-18TB correctly shown as absent?
- Do promise states for resilient subvolumes (htpc-home, opptak, pics) reflect WD-18TB1 as available?
- UUID check: WD-18TB1 has UUID configured. Since the actual UUID matches, it should pass.

**Result:** CRITICAL — Urd misidentifies the drive.

- Status shows "WD-18TB connected (2.7TB free)" — but this is actually WD-18TB1.
- WD-18TB1 shown as "absent 10d."
- "1 blocked, 6 degraded — chain broken on WD-18TB — next send will be full."
- All 7 send-enabled subvolumes show broken threads: "broken — full send (pin
  missing on drive)". This is because WD-18TB1 has old snapshots (up to 20260327)
  but the pins point to 20260402-1925-* snapshots that only exist on the real WD-18TB.
- subvol3-opptak: `blocked` (not just degraded) — opptak has 6 snapshots on the
  real WD-18TB but the pin parent doesn't exist on WD-18TB1.

### T2.5 — WD-18TB1 drive token check

```
cat /run/media/patriark/WD-18TB1/.snapshots/.urd-drive-token 2>/dev/null
```

Key question: Does WD-18TB1 have a drive token? If this was cloned from WD-18TB,
it might have WD-18TB's token (a mismatch for the WD-18TB1 label in SQLite).

**Result:** No token. `cat` returned empty (path doesn't exist — drive is mounted
at WD-18TB's path, not WD-18TB1's path). Token check at the actual mount path
(`/run/media/patriark/WD-18TB/.snapshots/.urd-drive-token`) was not performed,
but based on T2.4 behavior, Urd accepted the drive without token verification.

### T2.6 — Plan with WD-18TB1

```
urd plan
```

Key questions:
- Does urd plan sends to WD-18TB1?
- Are sends incremental? (WD-18TB1 has old snapshots from before the clone divergence — the incremental parent may or may not exist.)
- Does urd correctly identify available parents on this drive?
- Any token mismatch warnings in the plan?

**Result:** DANGEROUS — Urd plans massive full sends to the wrong drive.

- Urd plans sends to "WD-18TB" (actually WD-18TB1). All are full sends (chain broken):
  - htpc-home: ~42.3GB full
  - subvol2-pics: ~47.6GB full
  - subvol1-docs: ~12.7GB full
  - subvol7-containers: ~13.9GB full
  - subvol5-music: ~1.1TB full
  - htpc-root: ~32.9GB full
  - **Total: ~1.3TB**
- subvol3-opptak: `[SPACE] skipped: estimated ~4.1TB exceeds 2.2TB available`.
- No token mismatch warnings — Urd does not check tokens during planning.
- 5 retention deletions also planned (local subvol6-tmp, subvol7-containers,
  subvol4-multimedia).
- **If executed:** Would write 1.3TB to WD-18TB1 under WD-18TB's identity, update
  all pin files for "WD-18TB", and break the real WD-18TB's incremental chains
  when it returns. Catastrophic for the primary backup drive's state.

### T2.7 — Doctor with WD-18TB1

```
urd doctor --thorough
```

Key questions:
- UUID check for WD-18TB1 — should pass (UUID matches config).
- Thread integrity — are chains intact or broken on WD-18TB1?
- Any warnings about token mismatches?

**Result:** 7 issues. Doctor correctly identifies all chains as broken but
misattributes them to WD-18TB:

- 7 `✗` issues: "Pinned snapshot missing from drive: 20260402-1925-*" for all
  send-enabled subvolumes on "WD-18TB".
- No token mismatch warning — doctor does not check drive tokens.
- WD-18TB UUID warning persists ("no UUID configured").
- Sentinel running normally (uptime 43m).

### T2.8 — Backup to WD-18TB1 (dry-run first)

```
urd backup --dry-run
```

**Read the dry-run carefully before proceeding.** Key risks:
- If sends are full (not incremental), they may be very large.
- If the token is mismatched, sends should be blocked.
- If the chain parent doesn't exist on WD-18TB1, urd should attempt a full send or skip.

If the dry-run looks safe and reasonable:

```
urd backup
```

Record: which subvolumes sent, incremental vs full, any errors.

**Result:** DRY-RUN ONLY — user correctly identified the danger and did NOT
execute a full backup.

- Dry-run identical to T2.6 plan: 6 full sends totaling ~1.3TB to "WD-18TB"
  (actually WD-18TB1).
- **Decision:** User recognized that executing would corrupt WD-18TB's pin state
  and elected not to proceed. This was the correct call — the chain-break full
  send gate would have blocked individual sends, but the aggregate damage from
  updating pins under the wrong drive identity would have been severe.

### T2.9 — WD-18TB1 snapshot inventory

```
for dir in /run/media/patriark/WD-18TB1/.snapshots/*/; do
  echo "=== $(basename $dir) ==="
  ls "$dir" | tail -5
  echo "($(ls "$dir" | wc -l) total)"
done
```

Compare with T2.1 (WD-18TB inventory). How have the drives diverged?

**Result:** WD-18TB1 mounted at WD-18TB's path, so inventory was taken from
`/run/media/patriark/WD-18TB/.snapshots/*/`:

| Subvolume | WD-18TB1 (actual) | WD-18TB (T2.1) | Divergence |
|-----------|-------------------|-----------------|------------|
| htpc-home | 7 (up to 20260327) | 5 (up to 20260402) | WD-18TB1 has older+more legacy snapshots |
| htpc-root | 1 (20260323) | 5 (up to 20260402) | WD-18TB1 far behind |
| subvol1-docs | 7 (up to 20260327) | 5 (up to 20260402) | Similar pattern |
| subvol2-pics | 4 (up to 20260327) | 5 (up to 20260402) | Similar |
| subvol3-opptak | 6 (up to 20260327) | 5 (up to 20260402) | WD-18TB1 has more legacy |
| subvol4-multimedia | 1 (20250422) | 1 (20251213) | Different single snapshot |
| subvol5-music | 5 (up to 20260327) | 5 (up to 20260402) | Same count, different recency |
| subvol6-tmp | 0 | 0 | — |
| subvol7-containers | 8 (up to 20260327) | 5 (up to 20260402) | WD-18TB1 has more legacy snapshots |

Drives diverged on ~2026-03-27. WD-18TB1 has not received sends since then (it went
offsite). WD-18TB has been receiving daily sends and has retention-thinned older snapshots.
WD-18TB1 retains snapshots that WD-18TB has since deleted.

### T2.10 — Swap back: unmount WD-18TB1, mount WD-18TB

Restore original state: reconnect WD-18TB at its mount path.

```
urd status
urd sentinel status
```

Confirm everything returns to the T0.2 baseline.

**Result:** PASS — clean recovery.

- WD-18TB token confirmed: `e5b824a1-207d-499e-9dd7-e41e5cb742cf`.
- Status: "3 degraded — 2TB-backup away for 8 days." All threads unbroken.
  Identical to T0.2 baseline (modulo snapshot counts from run #23).
- Sentinel: Connected: WD-18TB. Assessment updated.
- **No damage from Phase 2** — user avoided executing backup against wrong drive.

---

## Phase 3: Edge cases and known issues

### T3.1 — Double backup (empty plan message)

With WD-18TB connected, run backup twice in quick succession:

```
urd backup
# immediately after:
urd backup
```

Expected: Second run should show mode-aware message explaining nothing needs backing up.
(Manual mode skips interval gating, so this tests whether the planner recognizes
"just ran 10 seconds ago" differently from interval checks.)

Actually — since manual mode ignores intervals, the second backup should also proceed.
Record what actually happens. This reveals whether manual mode truly bypasses all gating
or just interval gating.

**Result:** PASS — both backups executed.

- Run #24: 279.6s. Full backup, all incremental. htpc-home 6.7GB, subvol7-containers
  602.2MB, htpc-root 3.5GB. All successful.
- Run #25: 22.3s. All incremental, much smaller deltas: htpc-home 7.6MB, subvol7-containers
  143.0MB, htpc-root 72.4MB. Others ~125B each.
- Manual mode correctly bypasses all interval gating. Second backup proceeds immediately.
  Delta is small because almost nothing changed between runs — correct behavior.
- No "nothing to do" message — manual mode always creates snapshots and sends.

### T3.2 — Concurrent backup (lock contention)

In two terminals:

```
# Terminal 1:
urd backup

# Terminal 2 (while T1 is running):
urd backup
```

Expected: Second invocation should see the lock and refuse to run.

**Result:** SKIPPED — time constraints.

### T3.3 — assess() scoping observation

Look at htpc-root in `urd status`. It has `send_enabled = true` but no `drives = [...]`,
meaning it sends to all configured drives. When 2TB-backup and WD-18TB1 are absent,
the assess() model may falsely degrade htpc-root's health.

Compare htpc-root's health across:
- T0.2 (WD-18TB only) — was it "degraded"?
- T1.2 (WD-18TB + 2TB-backup) — did it improve?
- T2.4 (WD-18TB1 only) — what changed?

This documents the severity of the assess() scoping bug for patching.

**Result:** assess() scoping confirmed as problematic.

| Phase | Drives connected | htpc-root health | Reason |
|-------|-----------------|------------------|--------|
| T0.2 | WD-18TB | degraded | 2TB-backup absent (sends to all drives) |
| T1.2 | WD-18TB + 2TB-backup | degraded | chain broken on 2TB-backup (legitimate) |
| T2.4 | WD-18TB1 (at WD-18TB path) | degraded | pin missing on drive (legitimate — wrong drive) |

htpc-root was degraded in every phase, but for different reasons. The T0.2 degradation
is the assess() scoping issue: htpc-root sends to all drives including 2TB-backup, so
when 2TB-backup is absent, health degrades even though WD-18TB has a complete, recent copy.

The scoping issue is more clearly visible on htpc-home and subvol2-pics:

| Phase | htpc-home | subvol2-pics | Note |
|-------|-----------|-------------|------|
| T0.2 | degraded | degraded | 2TB-backup absent — false degradation |
| T1.2 | healthy | healthy | 2TB-backup connected — clears false degradation |

These subvolumes have explicit `drives` config (resilient level) that does NOT include
2TB-backup. Yet they were assessed as degraded when 2TB-backup was absent. Connecting
2TB-backup "fixed" the false degradation. This confirms assess() evaluates against all
configured drives rather than the subvolume's configured drive scope.

---

## Summary

| Test | Status | Notes |
|------|--------|-------|
| T0.0 | DONE | Config: multimedia interval → 1d. F0.0: UUID contradiction for cloned drives |
| T0.1 | PASS | v0.8.0 confirmed |
| T0.2 | PASS | Baseline recorded: 3 degraded, 21 pins, all unbroken |
| T0.3 | PASS | Manual vs auto mode works. F0.3: `[OFF] Disabled` label misleading for local-only subvols |
| T0.4 | PASS | Plan vs plan --auto works. Plan shows deletions that backup --dry-run hides |
| T0.5 | PASS | Lock trigger = "manual" |
| T0.6 | PASS | History and verify clean |
| T0.7 | PASS | Doctor thorough: 15 warnings, 0 issues (baseline) |
| T0.8 | PASS | Sentinel aware of recent backup |
| T0.9 | NOT TESTED | `urd get` not exercised with actual file |
| T1.0 | PASS | 2TB-backup UUID confirmed |
| T1.1 | PASS | Sentinel detected drive. F1.1: silent reconnection — no user notification |
| T1.2 | PASS | 2TB-backup column appeared, degraded 3→1 |
| T1.3 | PASS | Drive scoping correct. subvol5-music space-gated |
| T1.4 | PASS | 1 issue (htpc-root chain break), legacy pin detected |
| T1.5 | PASS | No token on 2TB-backup (expected, pre-first-send) |
| T1.6 | PARTIAL | htpc-root full send correctly gated. Token created on first send |
| T1.7 | PASS | docs and containers chains refreshed on 2TB-backup |
| T1.8 | PASS | 2TB-backup inventory recorded |
| T1.9 | PASS | Disconnect reflected correctly |
| T2.0 | PASS | WD-18TB token and SQLite state recorded |
| T2.1 | PASS | WD-18TB inventory recorded |
| T2.2 | PASS | All blocked. Redundancy warning for htpc-root — good |
| T2.3 | **CRITICAL** | F2.3: WD-18TB1 mounted at WD-18TB's path — drive identity crisis |
| T2.4 | **CRITICAL** | Urd misidentifies drive. All chains "broken" |
| T2.5 | OBSERVATION | No token on WD-18TB1. Urd doesn't check tokens |
| T2.6 | **DANGEROUS** | Plan: 1.3TB full sends to wrong drive under wrong identity |
| T2.7 | OBSERVATION | Doctor sees 7 broken chains, no token warning |
| T2.8 | SAFE (user abort) | Dry-run only. User recognized danger |
| T2.9 | PASS | Drive divergence documented (split ~2026-03-27) |
| T2.10 | PASS | Clean recovery to baseline. No damage |
| T3.1 | PASS | Double backup works. Second run fast (small delta) |
| T3.2 | SKIPPED | |
| T3.3 | CONFIRMED | assess() scoping bug: false degradation from unconfigured absent drives |

## Findings for patching

### Severity: Critical (data integrity risk)

**F2.3 — Cloned drive identity crisis.** WD-18TB1 mounts at WD-18TB's path because
they share a BTRFS filesystem label. Urd identifies drives solely by `mount_path` and
cannot distinguish them. If a backup runs against the wrong drive:
- Pin files update under WD-18TB's label, breaking the real WD-18TB's chains on return
- ~1.3TB of full sends fill the offsite drive under the wrong identity
- No safety gate fires — tokens aren't checked, UUIDs aren't verified at mount time

**Immediate mitigation:** Run `btrfstune -u` on WD-18TB1 when it returns from offsite
to give it a unique UUID. Then rename the BTRFS label so GNOME mounts it at a different
path. This is a manual, one-time fix outside Urd.

**Urd-side fix options (pick one or layer):**
1. **Drive token verification at mount detection.** If the on-disk token doesn't match
   the SQLite-stored token for that label, refuse to send and warn loudly.
2. **UUID verification at backup time.** Compare actual filesystem UUID against config.
   Requires solving F0.0 first (can't configure same UUID on two drives).
3. **Fingerprinting beyond mount path.** Use drive serial, LUKS UUID, or partition UUID
   as secondary identity signal.

### Severity: Moderate (UX / correctness)

**F0.3 — `[OFF] Disabled` label for local-only subvolumes.** subvol4-multimedia and
subvol6-tmp show as "Disabled" in skip reasons, but they are actively snapshotted
locally. Should say `[LOCAL] Local only` or similar to distinguish from truly disabled
subvolumes.

**F1.1 — Silent drive reconnection.** When an absent drive (flagged "protection degrading")
reconnects, the user gets no notification. The Sentinel detects it but doesn't surface
the event. For drives that have been absent for days, reconnection should produce a
desktop notification: "2TB-backup connected — protection restored."

**F0.0 — UUID contradiction for cloned drives.** Doctor suggests adding UUID for WD-18TB,
but config rejects duplicate UUIDs across drives. Contradictory guidance. Options:
suppress the doctor suggestion when the UUID is already configured on another drive,
or add a `shared_uuid` / `clone_of` field.

**T3.3 — assess() scoping bug.** Subvolumes are assessed against all configured drives,
not just their configured drive scope. Absent drives that a subvolume doesn't send to
can cause false degradation. htpc-home and subvol2-pics were falsely degraded when
2TB-backup was absent despite having explicit `drives` config excluding 2TB-backup.

### Severity: Low (observations)

**T0.4 — Plan vs backup dry-run divergence.** `urd plan` shows retention deletions
that `urd backup --dry-run` omits. Both are "plan" views but show different operations.
Consider aligning them or documenting the difference.

**T1.4 — Legacy pin detection.** subvol3-opptak/2TB-backup had a legacy-format pin
(20260324-opptak without timestamp). Doctor correctly identified it. Not urgent but
indicates pre-v0.8.0 state that will self-heal on next send.

**T2.2 — Redundancy warning for htpc-root.** When all drives disconnected, Urd showed
"htpc-root lives only on external drives while local copies are transient. Recovery
requires a connected drive." This is good proactive guidance — keep this pattern.
