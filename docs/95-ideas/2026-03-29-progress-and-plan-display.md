# Brainstorm: Progress Display and Dry-Run Output Improvements

**Status:** raw
**Date:** 2026-03-29
**Trigger:** User feedback during a 3+ hour manual backup establishing fresh chain on
WD-18TB. Progress display praised ("chef's kiss") but gaps identified. Dry-run output
has noise-to-signal issues.

## Context

### Current progress display (`backup.rs:496-543`)

A background thread polls an `AtomicU64` byte counter shared with `btrfs.rs`. Displays:
```
  53.2 GB @ 178.3 MB/s  [4:58]
```
Timer resets to 0 when counter resets between sends. No subvolume name, no global
progress, no ETA. The user observed the timer resetting and correctly deduced it tracks
per-subvolume sends.

### Current dry-run output (`voice.rs:766-834`)

Groups operations by subvolume, then dumps all skips as a flat list. On a system with
8 subvolumes and 3 drives, the skip list (20 entries) dominates the output and buries
the action items. Summary line counts operations but gives no size estimates.

### Data already available in the system

- `AtomicU64` byte counter (live, per-send)
- Calibrated sizes in SQLite (`subvolume_sizes` table) for full send estimates
- Historical send sizes per subvolume per drive per type (`operations` table)
- Subvolume names in executor (the loop variable `subvol_name`)
- Planned operation list (known before execution starts)

## Ideas

### Progress display

**P1. Show current subvolume name.** The executor knows `subvol_name` in its loop.
Share it with the progress thread via an `Arc<Mutex<String>>` or a second atomic/channel.
Display becomes:
```
  subvol2-pics: 23.1 GB @ 178.3 MB/s  [2:10]
```

**P2. Show global progress counter.** "Sending 3/6" or "3 of 6 sends". The plan knows
the total send count. The progress thread could track how many counter-resets have
occurred (each reset = one send completed).
```
  [3/6] subvol2-pics: 23.1 GB @ 178.3 MB/s  [2:10]
```

**P3. Show estimated total size for full sends.** Calibrated sizes from `subvolume_sizes`
or last successful send from `operations` table. For full sends, display:
```
  [3/6] subvol2-pics: 23.1 GB / ~47.6 GB @ 178.3 MB/s  [2:10, ~2:17 remaining]
```
For incremental sends where the size is unpredictable, omit the denominator.

**P4. Show completed subvolumes inline.** When a send finishes and the next begins,
briefly flash or print a completion line before starting the new counter:
```
  subvol1-docs: 12.7 GB in 3:12 ✓
  [4/6] subvol7-containers: 1.2 GB @ 165.1 MB/s  [0:08]
```
This gives a log-like trail while the dynamic line stays at the bottom.

**P5. Global elapsed timer.** A second timer that doesn't reset — total wall time for
the entire run. Could be a subtle addition:
```
  [3/6] subvol2-pics: 23.1 GB @ 178.3 MB/s  [2:10]  total: 1:23:45
```

**P6. Percentage progress bar for full sends.** When estimated size is known:
```
  [3/6] subvol2-pics: [████████░░░░░░░░] 49%  23.1 GB @ 178.3 MB/s  ~2:17 left
```
Classic progress bar UX. Requires size estimate which may be inaccurate for changed data.

**P7. Adaptive rate display.** Current rate is a simple total/elapsed average. For long
transfers, a rolling window (last 30s) would be more responsive to speed changes (USB
throttling, drive cache flushing, compression ratio shifts).

**P8. Show compression ratio in progress.** If the send stream is being received on a
zstd-compressed mount, the actual disk usage differs from bytes transferred. This is
hard to measure live (would need to query btrfs filesystem usage during receive), but
post-send the difference could be reported.

**P9. Send completion summary between subvolumes.** After each send, print a permanent
line to stderr showing what completed:
```
  ✓ htpc-home → WD-18TB: 53.2 GB in 4:58 (full)
  ✓ subvol3-opptak → WD-18TB: 3.8 TB in 1:42:00 (full)
  [3/6] subvol2-pics: 23.1 GB @ 178.3 MB/s  [2:10]
```
The dynamic progress line overwrites only the last line; completed sends accumulate above.

**P10. Quiet mode vs. verbose mode.** Some users want minimal output (just errors and
final summary). Others want maximum detail. A `--verbose` or `--quiet` flag could control
progress verbosity independently of log levels.

### Dry-run / plan output

**D1. Collapse skip reasons by category.** Instead of 20 individual skip lines, group:
```
  Skipped (drive not mounted): WD-18TB1 (6 subvolumes), 2TB-backup (3 subvolumes)
  Skipped (interval not elapsed): 7 subvolumes (next in ~14h6m)
  Skipped (disabled): htpc-root, subvol6-tmp (send), subvol4-multimedia (send + interval)
```
Three lines instead of twenty.

**D2. Show estimated send sizes in plan.** The planner already checks calibrated sizes
for space estimation. Display them:
```
  htpc-home:
    [SEND] 20260329-0404-htpc-home -> WD-18TB (full, ~53 GB) + pin
```
For incremental sends where size is unknown, show last incremental size as hint:
```
    [SEND] 20260329-0404-docs -> WD-18TB (incremental, last: 5.5 MB) + pin
```

**D3. Show total estimated transfer in summary.** Replace:
```
  Summary: 0 snapshots, 6 sends, 0 deletions, 20 skipped
```
With:
```
  Summary: 6 sends (~623 GB total), 0 snapshots, 0 deletions, 20 skipped
```

**D4. Estimated total duration in plan.** Using historical transfer rates per drive from
`operations` table, estimate total run time:
```
  Summary: 6 sends (~623 GB, ~3h20m estimated), 0 snapshots, 0 deletions
```
This is valuable for the user deciding whether to start a long run now or wait.

**D5. Separate "action" from "no-action" sections.** Put the active operations first
under a clear heading, then group all skips under a collapsed section:
```
  Urd backup plan for 2026-03-29 13:57

  === Planned operations ===
  htpc-home:        [SEND] full → WD-18TB (~53 GB)
  subvol3-opptak:   [SEND] full → WD-18TB (~3.8 TB)
  ...

  === Skipped (20) ===
  Not mounted: WD-18TB1 (6), 2TB-backup (3)
  Interval not elapsed: 7 subvolumes (next in ~14h)
  Disabled: htpc-root, subvol4-multimedia, subvol6-tmp
```

**D6. Highlight chain state in plan.** When a full send is planned because no incremental
parent exists, say so explicitly:
```
  [SEND] 20260329-0404-htpc-home -> WD-18TB (full — no chain parent) + pin
```
vs. a normal full send (first time) vs. a full send by policy. This helps the user
understand WHY it's full.

**D7. Drive health summary in plan header.** Before listing operations, show drive state:
```
  Drives: WD-18TB mounted (1.6 TB free), WD-18TB1 not mounted, 2TB-backup not mounted
```

**D8. Warn about one-behind send.** When the snapshot to send is older than the newest
local (the common case due to plan-before-execute), note this:
```
  [SEND] 20260329-0404-htpc-home -> WD-18TB (full) + pin
         Note: latest local is 20260329-0404, created this run
```
Actually this is always the case and would be noisy. Only useful if the gap is unusually
large (>1 cycle).

### Uncomfortable ideas

**U1. TUI dashboard for long runs.** A full terminal UI (using `ratatui` or similar) with
per-subvolume progress bars, a scrolling log, and drive status panel. Massively increases
complexity and dependency footprint. But for a 3-hour run, a rich display could be the
difference between anxiety and confidence.

**U2. Desktop notifications during transfer.** Send a system notification (via `notify-send`
or D-Bus) when each subvolume send completes. For multi-hour runs where the user walks away,
this gives non-terminal feedback. Sentinel already has notification infrastructure.

**U3. Web dashboard / progress socket.** Write progress state to a JSON file or Unix socket
that a browser or Spindle (tray icon) can poll. Turns the invisible worker into a visible
one during manual runs without coupling to terminal output.

## Handoff to Architecture

Most promising ideas for deeper analysis:

1. **P1 + P2 + P9 (subvolume name + counter + completion trail)** — Low complexity, high
   impact. The executor already has `subvol_name` and the plan has send count. Sharing one
   more piece of state with the progress thread gives the user situational awareness during
   multi-hour runs.

2. **D1 (collapsed skip reasons)** — The 20-line skip dump is the most visible noise
   problem in the current output. Grouping by category transforms it from a wall of text
   into a scannable summary.

3. **D2 + D3 (estimated sizes in plan and summary)** — The data already exists in SQLite.
   Surfacing it in the plan lets the user make informed decisions about when to start a
   long backup.

4. **P3 (estimated size + ETA for full sends)** — Builds on P1/P2 and the size data from
   D2. The "23.1 GB / ~47.6 GB" denominator transforms the progress from "how much so far"
   to "how much left." Most valuable for the massive subvolumes (opptak at 3.8 TB).

5. **D5 (separate action from no-action)** — Structural improvement to plan readability.
   Actions first, skips collapsed. Pairs naturally with D1.
