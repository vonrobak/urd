---
status: raw
date: 2026-04-02
source: v0.8.0 test report + Steve Jobs review "the-drive-knows-who-it-is"
---

# Brainstorm: v0.8.0 Test Findings and Steve Review Follow-up

Source material:
- `docs/99-reports/2026-04-02-testing-urd-v0.8.0.md` — 30 tests, all drive configs
- `docs/99-reports/2026-04-02-steve-jobs-000-the-drive-knows-who-it-is.md` — product review

---

## Theme 1: Drive Identity — "The drive knows who it is"

The test exposed that Urd's drive token system exists but doesn't protect against the
most dangerous scenario: a cloned drive mounting at the original's path with no token
file. `TokenMissing` is treated as benign (fail-open), so Urd proceeds, writes a new
token under the wrong label, and overwrites the real drive's SQLite record.

### Idea 1A: TokenMissing → gate when SQLite has a token

The simplest targeted fix. In `verify_drive_token()` (drives.rs:282), change behavior:
- If drive has no token file AND SQLite has a stored token for this label → return a new
  `DriveAvailability::TokenExpectedButMissing` variant instead of `TokenMissing`.
- Backup command treats this as a hard stop: "This drive should have a token but doesn't.
  If this is a new or reformatted drive, run `urd drives adopt <label>` to reset."
- Only `TokenMissing` with no SQLite record remains benign (genuine first-time drive).

This preserves fail-open for new drives while catching swaps and clones.

### Idea 1B: Token verification at Sentinel drive detection

Move token checks from backup-time to mount-detection-time. When Sentinel sees a drive
mount (`sentinel_runner.rs:113`), immediately verify the token. If mismatched or
suspiciously absent, emit a `DriveIdentityAlert` action instead of `LogDriveChange`.

Benefit: the user learns about the problem when the drive connects, not when they try
to back up. "I just plugged in WD-18TB but Urd says it doesn't recognize it" is much
better than "I ran backup and it planned 1.3TB of full sends to... wait, is this the
right drive?"

### Idea 1C: `urd drives` subcommand for drive identity management

A dedicated surface for the drive relationship:
- `urd drives` — list configured drives, their status, token state, UUID
- `urd drives identify <label>` — show what Urd knows about a specific drive
- `urd drives adopt <label>` — reset token for a drive (e.g., after clone/replace)
- `urd drives forget <label>` — remove a drive's token from SQLite

This gives users a way to resolve identity issues instead of editing SQLite or deleting
token files manually. The `adopt` command addresses the test scenario: when WD-18TB1
gets its own UUID via `btrfstune -u`, the user runs `urd drives adopt WD-18TB1` to
initialize its token.

### Idea 1D: LUKS UUID as secondary fingerprint

The test showed both drives are LUKS-encrypted with different LUKS UUIDs (`luks-9a0e...`
for 2TB-backup, `luks-b6b3...` for WD-18TB). Even when BTRFS UUIDs match (cloned drives),
LUKS UUIDs differ. `findmnt -n -o UUID` already returns the inner filesystem UUID, but
the LUKS container UUID is available from `/dev/mapper/luks-*` names or `blkid`.

Could add optional `luks_uuid` to DriveConfig, or auto-discover it. Gives Urd a unique
fingerprint even for cloned drives without requiring `btrfstune -u`.

### Idea 1E: Drive-label filesystem check

When `drive_availability()` finds a drive mounted, also check the BTRFS filesystem label
(via `btrfs filesystem label <path>` or `blkid`). If the filesystem label doesn't match
the configured drive label, warn. This would catch the WD-18TB1-at-WD-18TB-path scenario
directly — but only works if the cloned drive's label has been changed. Doesn't help
when labels are also cloned. Marginal value on its own.

### Idea 1F: Snapshot-content fingerprinting

Instead of (or in addition to) tokens, Urd could fingerprint a drive by the set of
snapshots it contains. When a drive mounts, compare the snapshot inventory against what
Urd expects for that label. A drive with snapshots from 2026-03-27 when Urd expects
2026-04-02 is clearly not the drive it claims to be.

More complex than token checking but provides evidence-based identity verification.
Could be an advisory layer: "Drive at WD-18TB path has snapshots up to 2026-03-27 but
expected up to 2026-04-02 — may be a different drive."

---

## Theme 2: Drive Lifecycle Events — "Close the loop"

Steve's core observation: Urd creates anxiety ("protection degrading") but never resolves
it visibly. The Sentinel sees everything but says nothing.

### Idea 2A: Drive reconnection notifications

When Sentinel detects a drive transition from absent to mounted, emit a desktop
notification via `notify-send` (or the existing notification infrastructure in
`notify.rs`). Message template:

"WD-18TB reconnected after 10 days. Run `urd backup` to restore full protection."

Or if the Sentinel is in active mode (future): "WD-18TB reconnected after 10 days.
Starting catch-up backup automatically."

The notification is the "click" — it closes the anxiety loop from "protection degrading."

### Idea 2B: Drive status transitions in `urd status`

Instead of just showing current state, surface recent transitions:

```
Drives: WD-18TB connected (4.2TB free)
Drives: 2TB-backup reconnected 5m ago — run backup to catch up
Drives: WD-18TB1 absent 10d — protection degrading
```

The "reconnected" line replaces the generic "connected" when the drive was recently
absent. Adds temporal context. Requires Sentinel to track transition timestamps (already
has `mounted_drives` BTreeSet, needs timestamp per entry).

### Idea 2C: Sentinel event log

A persistent event log that Sentinel writes to (SQLite or file):
- DriveConnected { label, timestamp }
- DriveDisconnected { label, timestamp }
- BackupCompleted { run_id, timestamp }
- PromiseStateChanged { subvol, old, new, timestamp }

`urd sentinel log` or `urd events` would show the timeline. This makes Sentinel's
observations visible without requiring the user to dig through journald.

### Idea 2D: "Protection restored" as a composite event

Don't just notify on drive connection — notify when protection is actually restored.
Drive connection is necessary but not sufficient; the backup has to succeed too.
Track the full cycle:

1. Drive connects → "2TB-backup connected — backup needed"
2. Backup runs → "2TB-backup caught up — all threads intact"

This is richer than just drive events but requires Sentinel to correlate across backup
runs. Could be implemented as a "pending resolution" that clears after a successful
backup to that drive.

### Idea 2E: Drive absence milestones

Instead of the generic "absent 10d — protection degrading," surface escalating milestones:

- 1 day: (no mention — normal for offsite rotation)
- 3 days: "WD-18TB1 away 3 days" (informational)
- 7 days: "WD-18TB1 away 7 days — consider connecting for catch-up"
- 14 days: "WD-18TB1 away 14 days — protection degrading"
- 30 days: "WD-18TB1 away 30 days — offsite copy stale"

Thresholds could be configurable per drive (offsite drives tolerate longer absence).
Matches the `DriveRole` concept — primary vs offsite drives have different absence
tolerances.

---

## Theme 3: Status Clarity — "Pick one"

The "All sealed. 3 degraded." contradiction, the assess() scoping bug, and the
misleading `[OFF] Disabled` label all erode trust in the status display.

### Idea 3A: Fix assess() scoping (the bug)

This is the known correctness bug. In `awareness.rs`, `assess()` iterates over all
`config.drives` for every send-enabled subvolume. It should respect `subvol.drives`
scoping — only assess against drives the subvolume is configured to send to.

The fix exists in `compute_redundancy_advisories()` (lines 813-820) which already
does this filtering. Apply the same pattern to the main assessment loop.

Patch tier. Should be the next thing built.

### Idea 3B: Single-sentence status verdict

Replace the two-axis summary line with a single verdict that answers "what should I do?":

- All healthy: "All sealed — nothing needs attention."
- Degraded with action: "All sealed — htpc-root needs a full send to 2TB-backup.
  Run `urd backup --force-full` when ready."
- Degraded from absence: "All sealed — connect WD-18TB1 for full protection."
- Exposed: "3 subvolumes exposed — run `urd backup` now."

The summary line becomes an action recommendation, not a state description. The table
below provides the detail for those who want it.

### Idea 3C: Replace `[OFF] Disabled` with `[LOCAL]`

Targeted vocabulary fix. In `output.rs`, add `SkipCategory::LocalOnly` alongside
`Disabled`. In `plan.rs`, when a subvolume has `send_enabled = false` but is still
snapshotted locally (i.e., it has a `protection_level` and `snapshot_interval`),
classify as `LocalOnly` instead of `Disabled`.

Voice renders `[LOCAL]` or `[LOCAL ONLY]` instead of `[OFF] Disabled`.

### Idea 3D: Omit local-only subvolumes from skip section entirely

More aggressive than 3C. If a subvolume is doing exactly what it's configured to do
(local snapshots, no sends), it's not "skipped" — it's complete. Only show it in the
plan if there's an operation (snapshot create/delete). Don't list it as skipped at all.

This reduces noise in the plan output. The status table already shows these subvolumes
with `—` in drive columns, which is sufficient.

### Idea 3E: Health explanations in status table

Add a hover/detail layer: when health is not "healthy," show why in the THREAD column
(already partially done — "broken — full send (pin missing locally)"). Extend this to
degraded-from-absence: "degraded — WD-18TB1 absent" so the user knows which drive is
causing the degradation without having to cross-reference the drive lines.

Already partially implemented. Ensure every non-healthy state has a THREAD explanation.

---

## Theme 4: Doctor Self-Consistency

### Idea 4A: Suppress UUID suggestion when UUID is shared

In `doctor.rs`, before suggesting "Add uuid = X to drive Y", check if that UUID is
already configured on another drive. If so, suppress the suggestion and instead note:
"WD-18TB shares UUID with WD-18TB1 (cloned drives). Run `btrfstune -u` on one drive
to separate identities, or configure LUKS UUID as an alternative."

### Idea 4B: Correlate pin-age warnings with drive absence

Doctor warns "Pin file is 8 day(s) old — sends may be failing" but the pin is old
because the drive was absent for 8 days, not because sends failed. Doctor should check
drive mount state and absence duration before attributing old pins to send failures:
- If drive absent: "Pin file is 8 days old — expected, drive has been absent"
- If drive present: "Pin file is 8 days old — sends may be failing"

### Idea 4C: Doctor drive identity check

Add a doctor check that runs `verify_drive_token()` for all mounted drives and reports
mismatches or suspicious absences. Currently doctor checks threads but not drive identity.
This would have caught the WD-18TB1 scenario in T2.7.

---

## Theme 5: Plan / Dry-Run Alignment

### Idea 5A: Align `urd plan` and `urd backup --dry-run`

The test found that `urd plan` shows retention deletions that `backup --dry-run` omits.
This is confusing — both claim to show "what will happen." Options:
- Make `backup --dry-run` show the full plan including deletions
- Add a footer to `backup --dry-run`: "Retention deletions also pending — run `urd plan`
  for full view"
- Document the distinction in help text

### Idea 5B: Unify plan and dry-run into one command

Remove `--dry-run` from `urd backup` entirely. `urd plan` is the preview; `urd backup`
is the execution. This eliminates the confusing dual-preview and makes the CLI cleaner.
The `urd plan` output already ends with "Run `urd backup` to execute this plan."

Breaking change (minor: removes a flag, doesn't change behavior).

---

## Theme 6: Rescue Path — "The 2am scenario"

### Idea 6A: Guided `urd get` without arguments

When `urd get` is invoked without arguments, show a human-written guide instead of
cargo-clap usage:

```
Restore a file from a snapshot:
  urd get ~/documents/important.txt --at yesterday
  urd get ~/photos/vacation.jpg --at 2026-03-15
  urd get ~/code/project/ --at "3 days ago"    (directory restore — coming soon)

Need help finding what's available?
  urd get ~/documents/important.txt --list      (show all versions)
```

This transforms the error state into a teaching moment. The rescue path should be the
most welcoming surface in the entire tool.

### Idea 6B: `urd get --list` to show available versions

Before restoring, let the user see what's available. `urd get <path> --list` would scan
local snapshots and show a timeline:

```
Versions of ~/documents/important.txt:
  2026-04-02 19:25  (2 hours ago)   3.2KB
  2026-04-02 04:00  (17 hours ago)  3.1KB
  2026-04-01 04:00  (yesterday)     3.1KB
  2026-03-31 04:00  (2 days ago)    2.9KB
```

Reduces the "archaeology" feeling Steve warned about. The user sees what they can get
back and picks the right version.

### Idea 6C: "Partial" backup result relabeling

When a backup is "partial" because a safety gate fired (not because something broke),
the label should reflect that. Instead of "partial," use "complete (1 deferred)" or
"success — htpc-root full send deferred (safety gate)." The word "partial" implies
failure; a safety gate working correctly is success.

---

## Theme 7: Uncomfortable Ideas

### Idea 7A: Auto-backup on drive connect (Sentinel active mode)

When a drive reconnects after absence, Sentinel automatically triggers a backup to that
drive. No user intervention needed. The anxiety loop closes itself.

This is the "set and forget" extreme. Requires very high confidence in the safety
mechanisms (token verification, space checking, chain-break gating). The circuit breaker
already exists. But the trust cost of an autonomous backup to the wrong drive (F2.3
scenario) is catastrophic. This idea is only safe AFTER Idea 1A/1B makes drive identity
bulletproof.

### Idea 7B: Drive migration wizard

When a drive dies or is replaced, the user needs to: update config, reset tokens, deal
with broken chains, possibly do full sends. Today this is manual. A `urd drives replace
<old-label> <new-label>` command could guide through the process:
1. Verify new drive is mounted
2. Write fresh token
3. Plan initial full sends
4. Update pin files
5. Report estimated time/space

This is the "drive replacement workflow" from the roadmap's deferred list, but framed as
a guided experience rather than a series of manual steps.

### Idea 7C: Cross-drive restore awareness

`urd get` currently only restores from local snapshots. But when local snapshots are thin
(htpc-root has only 2-3), the external drives have more history. `urd get` could:
1. Check local snapshots first
2. If no match, report which external drives have older versions
3. Guide: "This file isn't in local snapshots. It exists on WD-18TB from 2026-03-29. Mount WD-18TB and run `urd get --from WD-18TB ...`"

This is the "directory restore" horizon item extended to cross-drive awareness.

---

## Handoff to Architecture

1. **Idea 1A (TokenMissing gate when SQLite has a token)** — Smallest fix with the
   highest safety impact. Closes the exact gap that let the cloned drive through.
   Patch-tier change in `verify_drive_token()`.

2. **Idea 3A (assess() scoping fix)** — Known correctness bug, already on the roadmap.
   The test confirmed it causes real false degradation. Fix pattern already exists in
   `compute_redundancy_advisories()`.

3. **Idea 2A (Drive reconnection notifications)** — Closes Steve's "anxiety loop." The
   Sentinel already detects drive transitions; it just needs to emit a notification via
   the existing `notify.rs` infrastructure.

4. **Idea 3C (Replace [OFF] Disabled with [LOCAL])** — Five-minute vocabulary fix that
   immediately stops mischaracterizing the user's config choices.

5. **Idea 1C (`urd drives` subcommand)** — Provides a proper user surface for drive
   identity management. Without it, token and identity issues require manual SQLite/file
   editing. Needed before drive identity verification (1A) can have useful error guidance.
