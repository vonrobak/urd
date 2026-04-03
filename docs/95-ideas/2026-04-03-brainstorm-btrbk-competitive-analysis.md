---
status: raw
date: 2026-04-03
---

# Brainstorm: Learnings from btrbk

> **Context:** btrbk is the established BTRFS backup tool (~7K lines Perl, v0.33.0-dev).
> Urd occupies the same space but with a different philosophy: promises over operations,
> invisible worker, mythic voice. This brainstorm asks: what has btrbk solved that Urd
> should learn from? What has btrbk gotten wrong that Urd should avoid? Where are the
> quick wins?

## Ideas

### 1. `btrbk clean` — garbled backup recovery command

btrbk has `clean` to delete incomplete/garbled backups (subvolumes that were partially
received — missing `received_uuid`). Urd has no equivalent. If a send is interrupted
(network drop, power loss, SIGKILL), the partial subvolume sits on the target with no
way to automatically detect and clean it up. The executor cleans up on failure within
a run, but orphaned partials from previous crashed runs persist.

An `urd clean` or `urd doctor --clean` that detects subvolumes without `received_uuid`
on target drives would close this gap. The detection is a `btrfs subvolume show` check
— `received_uuid` is either set (complete) or `-` (garbled).

### 2. `btrbk resume` — complete interrupted sends without re-snapshotting

btrbk separates snapshot creation from backup transfer. `resume` skips creation and
just transfers missing backups from existing snapshots. Urd's `urd backup` always
creates fresh snapshots first. If a run created snapshots but sends failed (drive
disconnected mid-run), the next run creates new snapshots and tries to send those —
the old unsent snapshots become overhead.

Urd could benefit from a `--resume` flag or implicit resume behavior: detect existing
unsent snapshots and send them before creating new ones. This would reduce snapshot
accumulation after failures. The planner already knows about existing snapshots — the
logic is about send ordering, not new infrastructure.

### 3. `btrbk archive` — copy backups between drives with rescheduling

btrbk can copy backups from one target to another, applying different retention. This
enables a workflow: primary drive gets dense retention, archive drive gets sparse
retention. Urd's send model is always source→target. Drive-to-drive copies would require
a new operation type.

This is a horizon item but interesting for the offsite cycling use case: when the offsite
drive returns, copy the latest from the primary drive instead of sending from source.
Would be significantly faster for large subvolumes since the primary drive is always
available.

### 4. Stream compression for sends

btrbk supports `stream_compress` (gzip, lz4, zstd, etc.) in the send/receive pipeline.
Urd pipes `btrfs send | sudo btrfs receive` directly. For local sends, compression adds
overhead with no benefit (same disk bus). But for future SSH remote targets, stream
compression would be essential.

More immediately relevant: `btrfs send --compressed-data` (protocol v2) passes
already-compressed extents without decompressing. Urd could pass `--compressed-data`
to the send command when the btrfs version supports it. This is a one-line change in
`btrfs.rs` that could measurably reduce send times for compressed filesystems.

### 5. Rate limiting

btrbk supports `rate_limit` to throttle send throughput (via mbuffer). Urd has no
throttling. For USB drives with high initial burst but thermal throttling, or for
keeping the system responsive during large sends, a rate limit could prevent disk
contention.

However: this is feature bloat for Urd's current scope. The nightly timer runs at 04:00
when the system is idle. Rate limiting solves a problem Urd doesn't have yet. Park unless
SSH targets arrive.

### 6. `snapshot_create = onchange` — skip snapshots when nothing changed

btrbk can skip snapshot creation when the source hasn't changed since the last snapshot
(using btrfs `generation` numbers). Urd always creates a snapshot if the interval has
elapsed. For subvolumes with infrequent writes (docs, pics), this could eliminate
redundant snapshots that are byte-identical to the previous one.

The mechanism: compare the source subvolume's `generation` against the latest snapshot's
`otime` generation. If equal, skip. btrfs exposes this via `btrfs subvolume show`.
Could be a boolean config field: `snapshot_on_change = true`.

This directly serves north star #2 (reduce attention on backups) — fewer identical
snapshots means less clutter in `urd status` and less retention churn.

### 7. Yearly retention tier

btrbk supports yearly retention (`<N>y`). Urd's `GraduatedRetention` has hourly, daily,
weekly, monthly — no yearly. For long-running systems with months of history, a yearly
tier is the natural extension.

Already on the roadmap horizon. btrbk's existence confirms demand.

### 8. `preserve_day_of_week` and `preserve_hour_of_day`

btrbk lets you control when daily/weekly/monthly retention boundaries fall. Urd
hardcodes midnight boundaries and week-start. If a user runs backups at 04:00,
the "daily" snapshot is the 04:00 one — but retention thinning uses calendar midnight.
This can cause surprising deletions when the daily snapshot falls on the wrong side
of midnight.

`preserve_hour_of_day` matching the timer schedule would prevent this. Low complexity,
high correctness impact.

### 9. Incremental parent selection sophistication

btrbk's `incremental_prefs` system chooses the best parent from multiple candidates
using ordered preference lists (snapshot-older, snapshot-newer, archive-older, etc.).
Urd picks the latest common snapshot between source and target. This works for Urd's
simple topology but would break with multiple targets sharing parent chains.

Not needed today. But if archive/drive-to-drive copy (idea #3) is ever built, parent
selection becomes important.

### 10. `incremental = strict` — prevent accidental full sends

btrbk can refuse to send if no incremental parent is found (`incremental strict`).
Urd always falls back to full send when the chain is broken. For large subvolumes
(htpc-home at 32GB+), an accidental full send during the nightly timer is expensive.

The safety gate already exists in Urd (chain-break sends are blocked in `--auto` mode
unless `--force-full`). This is Urd's equivalent. btrbk validates the approach.

### 11. Transaction log

btrbk writes a structured transaction log: `time type status target_url source_url
parent_url message`. Urd uses SQLite state + heartbeat JSON. The transaction log is
a different shape — an append-only audit trail vs. Urd's structured state.

Urd's state.rs + heartbeat.rs already covers the use case better than a flat log.
But the flat log has one advantage: it's trivially greppable. `grep FAILED
/var/log/btrbk.log` answers "what went wrong?" without SQL. Urd's log files serve
this purpose but aren't as structured.

No action — Urd's approach is superior for its use case.

### 12. Group-based filtering

btrbk assigns subvolumes to named groups and filters operations by group name:
`btrbk run production`. Urd has no grouping — operations run on all enabled subvolumes
or use `--external-only`.

Groups would enable workflows like: `urd backup --group critical` (just home and docs),
`urd backup --group bulk` (media files). But this adds config complexity for minimal
benefit when the priority system already controls execution order and `enabled = false`
can exclude subvolumes.

Park unless user demand surfaces.

### 13. Wildcard subvolume matching

btrbk supports `subvolume docker-*` to dynamically discover subvolumes matching a
pattern. Urd requires explicit subvolume declarations. For users with many btrfs
subvolumes (Docker, LXC, development environments), wildcards would eliminate config
maintenance.

This is genuinely useful but orthogonal to Urd's current priorities. The encounter
(6-H) auto-detects subvolumes at setup time — wildcards would help after setup when
new subvolumes appear.

### 14. SSH filter script model

btrbk ships `ssh_filter_btrbk.sh` — a restrictive command filter for `authorized_keys`
that whitelists only the btrfs commands needed for backup. When Urd adds SSH targets,
this pattern is essential. Ship a `ssh_filter_urd.sh` (or Rust binary) that restricts
the remote session to `btrfs send`, `btrfs receive`, `btrfs subvolume show/list/delete`,
and nothing else.

Store in Urd's source tree now as a design reference. Implement when SSH lands.

### 15. `btrbk diff` — show modified files between snapshots

btrbk can list files changed between two snapshots with size and generation counts.
Urd has no equivalent. This would be a powerful addition to `urd get` — before
restoring, show what changed: "12 files modified, 3 added, 1 deleted since last
snapshot."

The mechanism is `btrfs subvolume find-new <subvol> <generation>` which lists files
modified since a given generation number. The generation comes from the reference
snapshot's metadata.

### 16. `btrbk origin` — show snapshot lineage tree

btrbk can show the parent-child and received-from relationship tree for any subvolume.
Urd has `urd verify` which checks chain health, but doesn't visualize the lineage.
An `urd thread <subvolume>` (or extend `urd verify`) could show:

```
htpc-home thread:
  20260403-2025-htpc-home (local, latest)
    ← 20260402-2220-htpc-home (local, pin for WD-18TB)
    → 20260402-2220-htpc-home (WD-18TB, received)
    → 20260402-2220-htpc-home (WD-18TB1, received)
```

This makes the incremental chain visible and debuggable. When threads break, the user
can see exactly where.

### 17. Two-layer retention (minimum window + schedule)

btrbk separates `preserve_min` (keep all within time window) from `preserve` (keep
N per period). This prevents surprises: `preserve_min = 2d` guarantees the last 48h
of snapshots are untouched regardless of the thinning schedule.

Urd's `GraduatedRetention` only has the schedule layer. A minimum window would address
the "hourly = 24 means exactly 24 hourly slots" confusion — does the user want "keep all
for 24 hours" or "keep 24 hourly snapshots"? btrbk's two-layer model disambiguates.

Worth evaluating during retention rework. The complexity is in communicating it clearly.

### 18. `--wipe` emergency mode

btrbk's `--wipe` deletes all snapshots except the latest — an emergency valve when
disk space is critical. Urd has no equivalent. The space guard prevents new snapshots
when free space is low, but doesn't proactively free space.

An `urd emergency` or `urd retention --aggressive` that temporarily overrides retention
to free space would prevent the catastrophic congestion scenario that already happened
once. The mechanism: run retention with `keep = 1` per subvolume, delete everything
else.

This directly addresses the catastrophic failure memory and north star #1 (data safety).

### 19. btrbk's config is operations-first — Urd's is promises-first

btrbk's config describes *operations*: volume paths, snapshot dirs, targets, retention
schedules. The user must understand the backup machinery to write a config. btrbk is
honest about this — it's a power tool for sysadmins.

Urd's config describes *intent*: protection levels, promises. The operations are derived.
This is Urd's key differentiator and should be protected. But btrbk's config teaches
something: the operations are transparent. A btrbk user can trace from config to behavior
without inference. Urd's `protection = "sheltered"` hides the operations behind a name.

The tension: transparency vs. simplicity. Urd resolves this with `urd plan` (show
derived operations) and custom mode (explicit everything). The encounter should make
this explicit: "here's what sheltered means in practice" with a `plan` preview.

### 20. btrbk has no daemon, no status, no promises

btrbk is a stateless cron job. No daemon, no awareness model, no promise states, no
notifications. It runs, does its thing, exits. Status is derived from filesystem state
on demand (`btrbk list`, `btrbk stats`).

This is Urd's biggest advantage. The sentinel, awareness model, promise states, and
"is my data safe?" answer are things btrbk fundamentally cannot do. btrbk tells you
what snapshots exist. Urd tells you if your data is safe. That gap is Urd's entire
reason for existing.

Protect this. Every feature decision should ask: "does btrbk already do this better?"
If yes, don't compete on operations — compete on the experience layer that btrbk doesn't
have.

### 21. `send_compressed_data` — protocol v2 pass-through

btrbk (experimentally) supports `send_compressed_data` which tells btrfs to pass
compressed extents through without decompression. This can significantly reduce send
time and I/O for compressed filesystems. Urd doesn't pass this flag.

One-line addition to `btrfs.rs`: add `--compressed-data` to the send command when the
btrfs version supports it. Detection: `btrfs send --help` or version check. Quick win
that could measurably improve send performance.

### 22. `btrfs_commit_delete` — wait for transaction commit

btrbk optionally waits for the btrfs transaction to commit after deletions. This ensures
deleted snapshots actually free space before the next operation. Urd doesn't do this —
deletions are fire-and-forget. For space-constrained volumes (the NVMe root), a commit
after deletion before the next snapshot creation could prevent false space-pressure.

`btrfs subvolume sync` is the mechanism. Could be added to the executor's delete path
with a config toggle.

### 23. What btrbk does NOT have that Urd does

Listing these to protect Urd's differentiators:

- **Promise model** — sealed/waning/exposed is Urd's language, not btrbk's
- **Awareness computation** — "is my data safe?" as a pure function
- **Sentinel daemon** — continuous monitoring, drive detection, notifications
- **Mythic voice** — personality in presentation
- **Protection levels** — opaque named levels that derive everything
- **Drive identity** — UUID fingerprinting, token verification, adoption workflow
- **Guided setup** — the encounter (planned) vs. btrbk's manual config editing
- **`urd get`** — file restore from snapshots (btrbk has no restore command)
- **Heartbeat** — health signal for monitoring integration
- **Progressive disclosure** — bare `urd` to `urd status` to `urd doctor` depth ladder

These are not bugs or gaps. They're Urd's product thesis: backups should be a
relationship, not a cron job.

## Handoff to Architecture

1. **#21: `--compressed-data` flag on sends** — One-line change in `btrfs.rs`, measurable
   performance win for compressed filesystems. Needs version detection. Quickest win on
   the list.

2. **#18: Emergency wipe mode** — Directly addresses the catastrophic failure history.
   Override retention to `keep = 1`, free space immediately. High data-safety impact,
   moderate implementation effort.

3. **#1: Garbled backup cleanup** — `urd doctor` or `urd clean` detecting partial receives
   on target drives. Closes a real gap in failure recovery. btrbk's `clean` command
   validates the need.

4. **#6: `snapshot_on_change` — skip unchanged subvolumes** — Reduces noise, saves space,
   serves north star #2. The generation-number check is cheap and precise.

5. **#15: `urd diff` or file-change preview** — Show what changed between snapshots before
   restore. Transforms `urd get` from "trust me" to "see for yourself." btrbk's `diff`
   validates the UX value.
