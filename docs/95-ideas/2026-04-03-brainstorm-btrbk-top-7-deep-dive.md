---
status: raw
date: 2026-04-03
---

# Brainstorm: Deep Dive on btrbk Top 7 Candidates

> **Context:** Steve's review of the btrbk competitive analysis identified 7 candidates
> worth deeper exploration. This brainstorm expands each into concrete approaches,
> variant ideas, and architectural hooks. The 7 are:
>
> 1. Change preview in `urd get` (btrbk diff)
> 2. Automatic garbled backup cleanup (btrbk clean, invisible)
> 3. `--compressed-data` on sends (protocol v2)
> 4. Emergency guided response (btrbk --wipe, reframed)
> 5. Skip unchanged subvolumes (btrbk onchange, default behavior)
> 6. `btrfs subvolume sync` after deletions (btrbk commit_delete)
> 7. Thread lineage visualization (btrbk origin)

## 1. Change Preview in `urd get`

### 1a. Inline diff before restore

Before copying the file, show a one-line summary: "This file was modified 2 days ago
(+3.2KB)." The user sees what they're getting without asking. Uses `btrfs subvolume
find-new <snapshot> <generation>` to check if the target file's inode appears in the
change set between the selected snapshot and the previous one.

Architectural hook: `commands/get.rs:71-80` already selects the snapshot. After selection,
look up the previous snapshot in the sorted list, get its generation via
`btrfs subvolume show`, then `btrfs subvolume find-new <selected> <prev_gen>` and check
if the relative path appears in the output. Lightweight — one extra btrfs call.

### 1b. `urd diff` as a standalone command

`urd diff ~/.config/urd/urd.toml` — show all versions of a file across snapshots with
timestamps and sizes. Not a restore, just discovery. "When did this file last change?
What did it look like a week ago?"

This extends `urd get` from "I know what I want" to "help me find what I need." The
mechanism: iterate snapshots in reverse chronological order, stat the file in each, show
entries where the file's mtime or size differs from the next-newer snapshot.

### 1c. Directory change summary for future `urd get --dir`

When `urd get` supports directory restore (roadmap horizon), the change preview becomes
essential. "This directory has 47 files. 3 were modified, 1 was deleted since the
previous snapshot. Restore all?" Uses `btrfs subvolume find-new` filtered to the
directory prefix.

### 1d. Side-by-side content diff for text files

For text files under a size threshold (e.g., 64KB), show an actual content diff between
the snapshot version and the live version. `urd get ~/.bashrc --at yesterday --diff` would
show unified diff output. The snapshot file is read-only and accessible at its path —
the diff is just `diff <snapshot-path> <live-path>`.

### 1e. Change preview in the voice layer

The mythic voice could frame the preview: "This .bashrc was altered two days past. The
changes are small." Or more practically: render the metadata through `voice.rs` so the
output matches Urd's tone. The `GetOutput` struct in `output.rs` would gain
`change_summary: Option<ChangeSummary>` with fields for file count, bytes changed,
time of change.

### 1f. Interactive snapshot browser

The ambitious version: `urd browse <path>` opens a TUI showing the file's history across
all snapshots with timestamps, sizes, and a visual timeline. Navigate with arrow keys,
press Enter to restore. This is the Time Machine visual experience in a terminal.

Way beyond current scope but worth noting as the north star for this feature family.

## 2. Automatic Garbled Backup Cleanup

### 2a. Pre-send detection in executor (Steve's recommendation)

The executor at `executor.rs:528-552` already detects and cleans up partial snapshots
from prior runs when the destination path exists but isn't pinned. This exists today.
The gap: it only checks when it's about to send to that exact path. Orphaned partials
from a different snapshot name (e.g., crashed mid-rename) would be missed.

Expand: before any send to a drive, scan the target snapshot directory for subvolumes
without `received_uuid` set. The check is `btrfs subvolume show <path>` — if
`Received UUID` is `-`, it's garbled. Delete and log.

### 2b. Doctor integration for audit

`urd doctor --thorough` already verifies thread health. Add a check: scan target
snapshot directories for subvolumes with missing `received_uuid`. Report them as
"N garbled snapshots found on WD-18TB from interrupted sends." The doctor doesn't
auto-delete — it reports and suggests `urd backup` (which will auto-clean per 2a).

### 2c. Sentinel detection

The sentinel could detect garbled snapshots during its periodic assessment. When a drive
is connected and the sentinel runs its tick, scan for garbled subvolumes. Notify:
"WD-18TB has 2 incomplete snapshots from an interrupted backup. They'll be cleaned up on
the next backup run." This bridges the gap between "invisible cleanup" and "the user knows
what happened."

### 2d. Garbled snapshot as a health signal

If garbled snapshots exist, the drive's health status in `awareness.rs` could include
a `garbled_count` field. The status display shows "WD-18TB connected (4.2TB free, 1
incomplete)." This is informational — it doesn't affect promise states but it tells the
user something happened.

### 2e. Graceful handling when the garbled snapshot IS the send target

Edge case: the crashed run was sending snapshot X to drive D. On the next run, the planner
wants to send snapshot X to drive D. The executor finds a garbled X on D. Today, it
deletes and resends. But what if the garbled snapshot is mostly complete (99% transferred)?
There's no way to resume a partial `btrfs receive` — it must restart from scratch. This
is inherent to btrfs. Document it clearly when it happens: "Cleaned up incomplete
20260403-2025-htpc-home on WD-18TB (prior run interrupted). Resending."

### 2f. Pre-send scan adds latency — is it worth it?

Scanning a drive's snapshot directory for garbled subvolumes requires `btrfs subvolume show`
on each candidate. For drives with hundreds of snapshots, this could add seconds to the
pre-send phase. Optimization: only scan when the last run had a failure (check heartbeat).
Or: scan asynchronously during the snapshot creation phase (sends haven't started yet).

## 3. `--compressed-data` on Sends

### 3a. Version detection and unconditional enable

`btrfs send --compressed-data` requires kernel 5.18+ and btrfs-progs 5.18+. Detection:
run `btrfs send --help` and check if `--compressed-data` appears in the output. If yes,
add it to every send command in `btrfs.rs`. No config toggle — it's strictly better when
supported.

Store the detection result in `BtrfsOps` at construction time (one-time probe). The
`send_receive` method checks `self.supports_compressed_data` before adding the flag.

### 3b. Also `--proto 2` for protocol v2

`--compressed-data` implicitly requires protocol v2. Some btrfs versions need explicit
`--proto 2` alongside `--compressed-data`. Check btrbk's handling: they use
`send_protocol` config. Urd could auto-detect: if `--compressed-data` is supported,
also pass `--proto 2` unless the version auto-selects it.

### 3c. Measure the actual improvement

Before shipping, measure send times with and without the flag on the production
subvolumes. If htpc-home (compressed zstd) sends are measurably faster, it's a clear win.
If the improvement is negligible (e.g., incremental sends are already tiny), document
that and ship anyway — it's free correctness.

### 3d. Log the protocol version

When `--compressed-data` is active, log it: "Send protocol: v2 (compressed pass-through)."
This helps debug send failures that might be protocol-related and tells the user their
system is using the optimal path.

## 4. Emergency Guided Response

### 4a. `urd emergency` — the guided panic

Full-screen (or near-full) interactive experience:
1. Assess: "Your snapshot root at ~/.snapshots has 2.1GB free (threshold: 10GB). 47
   snapshots across 2 subvolumes."
2. Explain: "Urd can free space by removing older snapshots while keeping your most recent
   backup safe."
3. Preview: "This will delete 38 snapshots, freeing approximately 8.3GB. Your newest
   snapshot for each subvolume will be preserved."
4. Confirm: "Proceed? [y/N]"
5. Execute: delete with progress output.
6. Report: "Freed 8.3GB. 9 snapshots remain. Next backup will run normally."

### 4b. Automatic emergency in the invisible worker

The nightly timer detects space critically low (below 50% of threshold? Below 2GB
absolute?). Instead of just skipping snapshots, it runs emergency retention first:
thin aggressively to `keep = latest`, then proceed with normal backup. Log it:
"Emergency retention freed 6GB before backup." Sentinel sends a notification.

This is the invisible worker version — no user command needed. The user wakes up to
a notification: "Urd freed space to keep backups running. Consider adding storage."

### 4c. `urd doctor` space pressure warning

Before space becomes critical, `urd doctor` warns: "Snapshot root at ~/.snapshots is
at 83% of min_free_bytes threshold. At current growth rate, space pressure in ~4 days."
This is the early warning that prevents the emergency.

Uses: current free space, min_free_bytes threshold, snapshot growth rate from state.db
history. Pure computation in doctor, no new data collection needed.

### 4d. Tiered aggression

Not all emergencies are equal. Three tiers:
- **Yellow:** approaching threshold → thin more aggressively (reduce daily from 30 to 7)
- **Orange:** at threshold → thin to minimum viable (daily = 3, weekly = 1)
- **Red:** below threshold → keep only latest per subvolume

The executor already has space-skip logic. Tiered retention would replace "skip everything"
with "thin first, then proceed." More data survives.

### 4e. The encounter teaches emergency preparedness

During guided setup (6-H), when Urd detects a small snapshot root, it could mention:
"If this volume runs low on space, Urd will automatically thin snapshots to keep backups
running. You can also run `urd emergency` manually." Plant the seed before the crisis.

### 4f. Emergency mode preserves external chain parents

Critical safety constraint: emergency deletion must never delete a pinned snapshot
(the chain parent needed for incremental sends). The existing defense-in-depth layers
(ADR-106) handle this. But emergency mode should explicitly verify and report: "Preserved
2 pinned snapshots needed for incremental chains."

## 5. Skip Unchanged Subvolumes

### 5a. Generation number comparison (Steve's recommendation)

`btrfs subvolume show <source>` returns `Generation: N`. `btrfs subvolume show <latest-snap>`
also returns `Generation: N`. If they match, nothing changed. Skip snapshot creation.

Architectural hook: `plan.rs` is where snapshot creation decisions happen. The
`FileSystemState` trait would gain `subvolume_generation(&self, path: &Path) -> Result<u64>`.
The planner compares source generation vs. latest snapshot generation. If equal, the
snapshot creation op is replaced with a `SkipReason::Unchanged` entry.

### 5b. Default behavior, not config

Steve is right: don't add `snapshot_on_change`. Just make it default. If the generation
hasn't changed, there's nothing to snapshot. The only reason to force a snapshot of
unchanged data would be forensic (proving the data *existed* at that time, not just
that it didn't change). That's an edge case that doesn't justify a config field.

If someone specifically needs forensic snapshots, they can override with
`--force-snapshot` on the CLI. No config pollution.

### 5c. Display skipped-unchanged in plan and backup

`urd plan` should show: `[UNCHANGED] docs — no changes since last snapshot (21h ago)`.
`urd backup` summary should mention: "Skipped 3 unchanged subvolumes."

This is a trust-building moment — Urd shows intelligence. "I checked, nothing changed,
so I didn't waste your space." The voice matters here.

### 5d. Unchanged affects retention counting

If snapshots are only created when data changes, retention thinning becomes more
interesting. A subvolume that changes once a week would accumulate ~52 yearly snapshots
instead of 365. The retention schedule (daily = 30) now means "30 most recent change
points" not "30 days." This is arguably better — the user gets 30 meaningful snapshots
instead of 30 copies of the same data.

Document this implication clearly. It changes the mental model of retention.

### 5e. Generation number requires filesystem access

The generation comparison requires reading `btrfs subvolume show` for both the source and
the latest snapshot. This is two additional btrfs subprocess calls per subvolume during
planning. For 9 subvolumes, that's 18 calls. Each is fast (~10ms), but it's nonzero.

In the planner's context, these calls happen before any mutations — they're read-only
filesystem queries through the `FileSystemState` trait. Safe and parallelizable.

### 5f. Edge case: metadata-only changes

`btrfs subvolume show` generation number increments on any metadata change (chmod, chown,
xattr), not just data writes. This means a `chmod` on a single file would trigger a new
snapshot. This is correct — the filesystem state *did* change — but might surprise users
who expect "unchanged" to mean "no file content changed."

Accept this. Metadata changes are real changes that should be backed up.

## 6. `btrfs subvolume sync` After Deletions

### 6a. Unconditional sync after retention deletions

After the executor deletes snapshots (retention thinning), call `btrfs subvolume sync`
on the filesystem. This waits for the btrfs transaction to commit, ensuring deleted
snapshots actually free their space before the next operation.

The call: `sudo btrfs subvolume sync <path>` where path is the snapshot root. Add it
to `BtrfsOps` trait as `fn sync_deletions(&self, path: &Path) -> Result<()>`.

### 6b. Only sync on space-constrained volumes

For the NVMe snapshot root (10GB threshold), sync matters — space freed by deletions is
needed before new snapshots. For the btrfs-pool root (50GB threshold with terabytes free),
sync is overhead with no benefit.

Heuristic: sync when free space is below 2x the min_free_bytes threshold. This targets
the volumes that actually benefit without penalizing unconstrained ones.

### 6c. Measure the actual timing

`btrfs subvolume sync` can take seconds to minutes depending on the number of deleted
subvolumes and filesystem activity. Measure on the production system: delete 5 snapshots,
time the sync. If it's <1 second, make it unconditional. If it's >5 seconds, make it
conditional on space pressure.

### 6d. Log the sync

"Waiting for deletion commit on ~/.snapshots (5 snapshots deleted)..." → "Commit complete,
8.3GB freed." This gives the user visibility into what was a black box: "I deleted
snapshots but `df` didn't change." Now they know why.

### 6e. Sync before space check, not after

The executor's space check (`min_free_bytes`) runs before snapshot creation. If deletions
happened but haven't committed, the space check sees stale free space and might skip the
snapshot. Sync *between* deletion and space check to get an accurate reading.

Order: delete old snapshots → sync → check free space → create new snapshot. Today it's:
delete → check → create (and the check may see pre-deletion free space).

## 7. Thread Lineage Visualization

### 7a. `urd thread <subvolume>` standalone command

Show the incremental chain (thread) for a subvolume across local and external snapshots:

```
htpc-home thread:
  Local (18 snapshots, newest 21h ago):
    20260403-2025-htpc-home ← pin for WD-18TB
    20260402-2220-htpc-home ← pin for WD-18TB1
    20260402-2215-htpc-home
    ...

  WD-18TB (6 snapshots, newest 21h ago):
    20260402-2220-htpc-home ← latest
    20260402-0400-htpc-home
    ...
    thread: unbroken (parent: 20260402-2220-htpc-home)

  WD-18TB1 (absent 11d):
    last seen: 20260323-0400-htpc-home
    thread: stale (11d since last send)
```

Uses existing data: `pin_file` reads for chain parents, `read_snapshot_dir` for listing,
drive assessment for status. No new btrfs calls needed.

### 7b. Enrich `urd doctor --thorough` instead of new command

Steve's suggestion: don't add a command, enrich the diagnostic. `urd doctor --thorough`
already checks thread health. Add the lineage visualization as a section in the thorough
output: "Thread details:" followed by the chain visualization per subvolume.

This keeps the command surface small and puts the information where power users already
look.

### 7c. Visualization in `urd verify`

`urd verify` currently checks chain health. Extend it to show the chain when a problem
is found: "Thread broken for htpc-home on WD-18TB. Chain:" followed by the lineage
showing exactly where the break is. Only show the visualization when there's something
to diagnose — don't clutter healthy output.

### 7d. JSON output for Spindle

The thread data should be available in daemon/JSON output mode for Spindle. The tray
icon could show a thread visualization popup: "Click to see htpc-home's backup chain."
The data structure is the same; the rendering differs.

### 7e. Cross-drive thread comparison

For subvolumes backed up to multiple drives, show the thread comparison: "WD-18TB has
snapshots through 20260403, WD-18TB1 is 11 days behind (last: 20260323)." This makes
drive cycling urgency concrete: the user sees exactly how far behind the offsite drive is.

### 7f. Thread break diagnosis

When a thread is broken, show *why*: "Parent snapshot 20260402-2220-htpc-home was deleted
by retention before the external send completed." Or: "Pin file missing — possible manual
deletion." This transforms the thread from a status indicator to a diagnostic tool.

The data for this exists in the state.db (send history, retention actions) but would need
correlation logic. More complex than the basic visualization but valuable for the
"why did my chain break?" question.

## Handoff to Architecture

1. **#5a+5b: Skip unchanged subvolumes by default** — Highest product impact per line of
   code. The generation comparison is cheap, the behavior change is universally correct,
   and it demonstrates intelligence. Needs `FileSystemState` trait extension and planner
   change. Every user benefits immediately.

2. **#1a: Inline change preview in `urd get`** — Highest trust-building impact. The file
   exists in `commands/get.rs:71-94`, the mechanism is one `btrfs subvolume find-new` call.
   Small scope, large UX payoff. Makes restore less scary.

3. **#3a: `--compressed-data` on sends** — Quickest win, zero UX surface, measurable
   performance improvement. One-time probe in `BtrfsOps`, one flag in `send_receive`.
   Ship as a patch.

4. **#6e: Sync deletions before space check** — Correctness fix for space-constrained
   volumes. Prevents the "deleted snapshots but space check still fails" scenario. Small
   change in executor ordering with `BtrfsOps::sync_deletions`.

5. **#4a+4b: Emergency response (guided + automatic)** — Needs `/design` before building.
   The mechanism is trivial (aggressive retention), but the experience (guided crisis,
   automatic prevention, sentinel notification) needs careful design. Directly addresses
   the catastrophic failure memory.
