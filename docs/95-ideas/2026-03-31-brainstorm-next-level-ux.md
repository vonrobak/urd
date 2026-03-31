# Brainstorm: Next-Level UX — Norman Principles Applied to Urd v0.5

> **TL;DR:** A brainstorm exploring how to elevate Urd's user experience across all
> surfaces — CLI, Spindle tray, notifications, and the future restore experience —
> using Don Norman's design principles as generative prompts. Built on the foundation
> of the 2026-03-23 Norman audit but accounts for everything built since: sentinel
> daemon, awareness model, two-axis status (safety + health), promise levels,
> progressive disclosure design, redundancy encoding. The question isn't "how do we
> fix problems" — it's "what takes this from good to genuinely excellent?"

**Date:** 2026-03-31
**Status:** raw

---

## Framing: What Has Changed Since the First Norman Brainstorm

The 2026-03-23 brainstorm was written against an Urd that had `plan`, `backup`,
`status`, `verify`, `history`. Since then:

- **Awareness model** (`awareness.rs`): Pure function that answers "is my data safe?"
  per subvolume. Two axes: safety (PROTECTED / AT RISK / UNPROTECTED) and operational
  health (healthy / degraded / blocked).
- **Promise levels**: guarded, protected, resilient — encoding what the user declared
  they want, not just what exists.
- **Sentinel daemon**: Watches for drive events, heartbeat changes, ticks. Writes
  `sentinel-state.json`. Already deployed and running.
- **Voice architecture**: `output.rs` → `voice.rs` separation. Interactive vs daemon
  modes. Structured data in, rendered text out.
- **Progressive disclosure design** (6-O): 8 milestones, parameterized identity.
- **Redundancy encoding** (6-E): Promise levels now encode offsite requirements.

The first Norman brainstorm identified gaps. This one asks: given the machinery we
now have, what experiences become *possible* that weren't before?

---

## 1. The One-Sentence Status — Answering the Only Question That Matters

*Norman: Bridge the Gulf of Evaluation. The user's question is always "is my data safe?"*

### 1.1 — `urd` with no arguments shows the sentence

Today `urd` shows clap help. The most useful default:

```
$ urd
All safe. 9 subvolumes protected. Last backup 7 hours ago.
Run `urd status` for details, `urd --help` for commands.
```

Or when things are bad:

```
$ urd
htpc-root needs attention — chain broken, next send will be full.
Run `urd status` for details.
```

The awareness model already computes everything needed. This is a one-line call to
`awareness::assess_all()` rendered through `voice.rs`. The affordance: you never need
to remember any command to check if things are OK.

### 1.2 — The sentence adapts to what changed

After a backup completes, `urd` (or `urd status`) should open with what's *different*:

```
$ urd status
All safe. Last backup completed 3 minutes ago — 1.2 GB sent to WD-18TB.
```

vs. the morning after a nightly run:

```
$ urd status
All safe. Nightly backup completed at 04:02 — 7 snapshots, 3 sends.
```

This is the conceptual model insight: the sentence answers "what happened since I last
looked?" not just "what is the current state?" The awareness model provides the state;
the history from `state.rs` provides the delta. Voice renders both.

### 1.3 — The sentence speaks through every surface

The same sentence renders differently per surface but carries the same semantics:

| Surface | Rendering |
|---------|-----------|
| CLI (`urd status`) | Full text, colored, table follows |
| Spindle tooltip | "All safe — last backup 7h ago" |
| Spindle notification | "htpc-root needs attention" |
| `sentinel-state.json` | Machine-readable fields that produce the sentence |
| Heartbeat | The sentence in the `summary` field |

One computation (`awareness.rs`), one structured output (`output.rs`), many renderings
(`voice.rs` for CLI, Spindle for desktop, notify for alerts). The insight from the
architecture discussion: these are all consumers of the same library, not separate
systems trying to stay in sync.

---

## 2. Temporal Awareness — Urd as the Norn Who Knows Time

*Norman: Feedback should include temporal context. The user needs to know not just what
IS but when things happened and when things will happen.*

### 2.1 — Time-relative language everywhere

The status output already shows `(1d)`, `(5h)` for ages. Push this further:

```
LOCAL    WD-18TB
12 (1d)  2 (5h)     ← "I know when these were made"
```

But also forward-looking:

```
Next snapshot in ~14m. Next send to WD-18TB in ~3h.
```

The planner already computes `next_due`. Exposing it in status turns the system from
a state report into a schedule report. The user sees both past and future.

### 2.2 — Staleness escalation in natural language

Currently: `NOTE htpc-root: offsite drive WD-18TB1 last sent 8 days ago — consider cycling`

The time-awareness could escalate naturally through voice:

| Age | Voice |
|-----|-------|
| < 3 days | (nothing — all is well) |
| 3-7 days | "WD-18TB1 away for 5 days" |
| 7-14 days | "WD-18TB1 away for 10 days — consider connecting" |
| 14-30 days | "WD-18TB1 away for 3 weeks — offsite backup aging" |
| > 30 days | "WD-18TB1 absent 47 days — protection degrading" |

The awareness model already tracks this through `last_send_age`. Voice adds the
graduated urgency. The constraint that matters: never cry wolf. The thresholds should
be generous for offsite drives (their whole purpose is being away).

### 2.3 — "Urd remembers" — historical context in status

When the user connects a drive they haven't used in a while:

```
Drives: WD-18TB1 mounted (3.1 TB free)
  Last connected: 2026-03-23 (8 days ago). 5 sends completed that session.
```

SQLite already records operations per drive. Surfacing this when a drive reappears
makes the system feel like it has memory, not just state. The norn who remembers.

### 2.4 — Growth rate and space forecasting

```
Drives: WD-18TB mounted (4.3 TB free)
  At current rate: ~14 months until full
```

The SQLite `operations` table has `bytes_transferred` per send. A simple linear
regression over the last N sends gives a growth rate. This is the "is my data safe
*in the future*?" question — a forward-looking promise status.

This might seem like over-engineering, but for a backup tool the most important UX
question after "is my data safe now?" is "will it stay safe?" A drive that's 93% full
is safe today and an emergency next month.

---

## 3. The Status Table Rethink — Information Architecture

*Norman: Visual hierarchy should match importance hierarchy. What matters most should
be most visible.*

### 3.1 — Lead with problems, not alphabetical order

Current status sorts subvolumes in config order. Norman says: sort by attention needed.

```
── NEEDS ATTENTION ──────────────────────────────────
htpc-root        1 local (5h)  WD-18TB: 1  chain: FULL (no pin)
  → Connect WD-18TB1 or 2TB-backup, then run `urd backup`

── ALL CLEAR ────────────────────────────────────────
htpc-home        12 local (1d)  WD-18TB: 2 (5h)  chain: incremental
subvol3-opptak   23 local (1d)  WD-18TB: 2 (5h)  chain: incremental
...
```

The user sees what needs their attention first, then confirms everything else is fine.
The table is the same data — just reordered and grouped by safety status.

### 3.2 — Collapse the healthy majority

9 subvolumes is manageable. 50 would not be. Even at 9, the table is wide and dense.
Progressive disclosure (6-O) already designs for this. The next step:

```
$ urd status
All safe. 8 of 9 protected. htpc-root needs attention.

  htpc-root  1 local (5h)  WD-18TB: chain broken (full send pending, ~32 GB)
    → Next backup will send a full copy to WD-18TB

  8 subvolumes healthy — run `urd status --all` for details
```

Default: show problems + one-liner summary. `--all` or `--verbose`: show the full table.
This is the classic Norman pattern: reveal complexity on demand, don't front-load it.

### 3.3 — Named groups replace subvolume lists

When subvolumes share the same promise level and the same drive set, they're
functionally identical from a UX perspective. Group them:

```
── resilient (3 subvolumes: htpc-home, opptak, pics) ──
  Local: 9-23 snapshots, all current
  WD-18TB: incremental, sent 5h ago
  WD-18TB1: away    2TB-backup: away

── protected (2 subvolumes: docs, containers) ──
  Local: 23-24 snapshots, all current
  WD-18TB: incremental, sent 5h ago
  WD-18TB1: away    2TB-backup: away
```

This shifts the conceptual model from "9 individual things" to "3 protection groups."
The user thinks in terms of what matters, not what exists. Dangerous idea: might hide
important per-subvolume differences. Would need a drill-down affordance.

### 3.4 — The status table as a heat map

Instead of OK/gap in the SAFETY column, color the *entire row* based on safety status.
Green background for protected, yellow for at-risk, red for unprotected. Terminal
supports background colors.

Controversial: might be too visually noisy. But the Norman principle is clear — severity
should be perceptible at a glance, without reading the text. A wall of green with one
red row is instantly parsed. A wall of "OK" with one "gap" is not.

---

## 4. Affordances — Making the Right Thing Easy

*Norman: The best affordance is one the user discovers without being taught.*

### 4.1 — Contextual next-action suggestions

After every output, suggest what the user probably wants next:

```
$ urd plan
...
Summary: 1 sends (~31.8 GB total), 7 snapshots

  Run `urd backup` to execute this plan.
  Run `urd plan --verbose` to see skip reasons.
```

```
$ urd backup
...
Backup complete. 7 snapshots created, 1 send (31.8 GB to WD-18TB).

  htpc-root chain repaired — future sends will be incremental.
  Run `urd status` to confirm.
```

The suggestion is specific to what just happened. Not a generic "see --help" but
"here's what you probably want to do given what I just told you."

### 4.2 — `urd doctor` — the diagnostic command

One command that answers "is anything wrong and what do I do about it?"

```
$ urd doctor

Checking Urd health...

  ✓ Config valid (9 subvolumes, 3 drives)
  ✓ State DB accessible
  ✓ Sentinel running (PID 366735, 46h uptime)
  ✓ sudo btrfs available
  ✓ All snapshot roots writable
  ✗ htpc-root: chain broken on WD-18TB (no pin file)
    → Run `urd backup --subvolume htpc-root` to do a full send
  ⚠ WD-18TB UUID not in config — add `uuid = "647..."` for safety
  ⚠ WD-18TB1 not seen in 8 days — consider connecting for offsite sync
  ✓ All other chains healthy
  ✓ No retention pressure (4.3 TB free on WD-18TB)

2 warnings, 1 issue found. Run suggested commands to resolve.
```

This is preflight (5.2 from the first Norman brainstorm) plus chain verification plus
drive health plus config validation — all in one pass. The awareness model, verify
logic, and config validator already exist. `doctor` composes them.

### 4.3 — Drive-plug affordances

When the sentinel detects a drive plug-in, the notification should suggest the action:

```
WD-18TB1 connected. 3 subvolumes have unsent snapshots (8 days).
Run `urd backup --external-only` to sync, or wait for the next nightly run.
```

The sentinel already detects mounts. The awareness model already knows which subvolumes
are behind. The affordance is the suggested command.

### 4.4 — `urd restore` as a guided experience

`urd get` restores a single file. But "I need to restore something" is a workflow, not
a command:

```
$ urd restore
What would you like to restore?

  1. A specific file from a snapshot
  2. Browse snapshots for a subvolume
  3. Compare a file across snapshots

Choice: 1

Which subvolume? [htpc-home]
  (tab-complete from configured subvolumes)

Which file? ~/.config/urd/urd.toml
  (or use a relative path — Urd will resolve it)

Available snapshots containing this file:
  1. 20260331-1145 (today, 11:45)     ← most recent
  2. 20260331-0400 (today, 04:00)
  3. 20260330-0400 (yesterday, 04:00)
  4. 20260329-0400 (2 days ago)

Restore which version? [1]
Restore to: [./urd.toml.restored]

Restored: .snapshots/htpc-home/20260331-1145-htpc-home/<username>/.config/urd/urd.toml
  → ./urd.toml.restored (2.1 KB)
```

Interactive on TTY, flags for scripting (`urd restore --subvol htpc-home --file X --at 20260331`).
This is the Time Machine experience in a terminal — guided, forgiving, discoverable.

### 4.5 — Shell completions with semantic awareness

Beyond `clap_complete` basics: complete subvolume names, drive labels, snapshot dates.

```
$ urd backup --subvolume <TAB>
htpc-home   htpc-root   subvol1-docs   subvol2-pics   ...

$ urd get --at <TAB>
20260331-1145   20260331-0400   20260330-0400   ...

$ urd history --drive <TAB>
WD-18TB   WD-18TB1   2TB-backup
```

The dynamic completions require `clap_complete`'s custom completer — it reads the config
at completion time. Higher effort than static completions, much higher value.

---

## 5. Constraints and Error Design — Guide, Don't Block

*Norman: Constraints should make it impossible to do the wrong thing, not just warn
after the fact.*

### 5.1 — Config as constraint language

Today config validation catches structural errors. The next level: validate *intent*.

```
Warning: subvol4-multimedia has no external sends configured.
  This subvolume exists only as local snapshots — a disk failure
  would lose all data, including the backups.

  To add external protection:
    [subvolumes.subvol4-multimedia]
    send_to = ["WD-18TB"]

  To acknowledge local-only (silences this warning):
    promise = "guarded"
```

The redundancy encoding (6-E) enables this — if a subvolume has `promise = "protected"`
but no external sends, the config is contradictory. Catch it at load time, not at
status time.

### 5.2 — Graduated confirmation for dangerous operations

A full send of 2.8 TB is a 3-hour commitment. Urd should distinguish between routine
operations and unusual ones:

```
$ urd backup

Plan: 7 snapshots (routine), 1 full send to WD-18TB

  ⚠ Full send: htpc-root → WD-18TB (~31.8 GB, est. 3-5 minutes)
    Reason: chain broken (no pin file)

  Proceed? [Y/n]
```

Only prompt on TTY. Only prompt for full sends (incremental sends are expected and
fast). `--yes` bypasses. The systemd timer always passes `--yes`.

This is the Norman "make it hard to do the wrong thing accidentally" principle. A full
send isn't wrong — but it's unusual enough that confirming is prudent.

### 5.3 — Impossible states made unrepresentable

Promise level "resilient" requires an offsite drive in the config. Rather than checking
this at runtime and advising, refuse the config:

```
Error: subvol2-pics: promise level "resilient" requires at least one drive
with role = "offsite", but none of its configured drives have this role.

  Configured drives: WD-18TB (primary)
  Fix: change WD-18TB to role = "offsite", or add an offsite drive,
       or lower the promise to "protected".
```

This is already partially designed in 6-E. The idea here: push the validation as early
as possible. Config load time > plan time > execution time > status time.

---

## 6. Feedback Loops — Making the Invisible Visible

*Norman: Every action must produce immediate, informative feedback. Silence is the
enemy of trust.*

### 6.1 — Backup-as-narrative

Current backup output lists operations. Narrative output tells a story:

```
$ urd backup

Urd backup — 2026-03-31 11:45

  Creating snapshots...
    htpc-home         ✓  (0.2s)
    subvol3-opptak    ✓  (0.1s)
    subvol2-pics      ✓  (0.2s)
    subvol1-docs      ✓  (0.1s)
    subvol7-containers ✓ (0.3s)
    subvol5-music     ✓  (0.1s)
    subvol6-tmp       ✓  (0.1s)

  Sending to WD-18TB...
    htpc-root (full, ~31.8 GB)
    [████████████████████░░░░░░░░░░░] 67% — 21.3 GB @ 156 MB/s — ~1m remaining

  7 snapshots created. 1 send in progress.
```

Then on completion:

```
  Sending to WD-18TB...
    htpc-root (full, 31.8 GB)  ✓  2m 14s

  ── Summary ─────────────────────────────────────
  7 snapshots created, 1 full send (31.8 GB), 0 deleted
  htpc-root chain established — future sends will be incremental
  All 9 subvolumes safe.
```

The narrative has phases (creating, sending, cleaning up, summary), progress, and a
bottom-line answer. The user sees the *story* of the backup, not a log.

### 6.2 — Progress through Spindle

During a backup, the Spindle tray icon should reflect progress:

- Icon changes from static green to animated "working" state
- Tooltip: "Backing up — sending htpc-root to WD-18TB (67%, ~1m remaining)"
- On completion: brief notification, then back to static state

The sentinel already detects "backup running" via lock file. Adding a progress file
(written by the executor, read by sentinel/Spindle) extends this to per-operation
progress. The file is the contract, same as `sentinel-state.json`.

### 6.3 — Sound affordance for long operations

After a multi-minute backup, a completion sound on the desktop:

```
canberra-gtk-play -i complete -d "Urd backup complete"
```

Only on TTY. Only when the backup took more than N seconds (don't ding for a 3-second
incremental). Optional via config. Pairs with desktop notification.

Small thing, massive Norman value: the user walked away during a long send and now
knows it's done without checking.

### 6.4 — The weekly digest

For the invisible worker mode, a weekly summary:

```
Urd — Week of March 24

  7 backup runs, all successful
  42 snapshots created, 35 incremental sends
  12.4 GB transferred total

  Drives:
    WD-18TB: connected all week, 4.3 TB free (~14 months at current rate)
    WD-18TB1: not connected (8 days — consider cycling)
    2TB-backup: not connected (8 days — consider cycling)

  All 9 subvolumes maintained their promise levels.
  No action needed.
```

Written to a well-known file (e.g., `~/.local/share/urd/weekly-digest.txt`). Optionally
pushed via notification channel. The sentinel could generate this on Sunday at midnight.

The value: even when everything is working perfectly, the user gets confirmation that
it's working. Silence means safety, but periodic confirmation reinforces trust.

---

## 7. The Restore Experience — Time Machine in a Terminal

*Norman: The most critical interaction must be the most polished.*

### 7.1 — `urd browse` — snapshot explorer

```
$ urd browse htpc-home

Snapshots for htpc-home (12 local, WD-18TB: 2):

  Today
    20260331-1145  (local)   3 minutes ago
    20260331-0400  (local, WD-18TB)  7 hours ago

  Yesterday
    20260330-0400  (local, WD-18TB)  1 day ago
    20260330-0300  (local)   1 day ago

  This week
    20260329-0400  (local)   2 days ago
    20260328-0400  (local)   3 days ago
    ...

Select a snapshot to browse: [20260331-1145]
```

Then, once a snapshot is selected:

```
Browsing 20260331-1145-htpc-home (read-only snapshot of /home)

  path: /
  .config/
  .local/
  Documents/
  projects/

  Navigate: type a path, or cd/ls as usual
  Restore: `get <file>` copies to current directory
  Diff:    `diff <file>` shows changes from current version
  Quit:    q

> cd projects/urd/
> ls
  CHANGELOG.md   Cargo.toml   src/   docs/   ...
> diff src/main.rs
  --- snapshot 20260331-1145
  +++ current ~/projects/urd/src/main.rs
  @@ -12,3 +12,5 @@
  ...
> get src/main.rs
  Restored to ./main.rs.20260331-1145 (1.8 KB)
```

This is the Time Machine starfield, translated to a terminal. Interactive, forgiving,
undoable. The snapshots are read-only BTRFS subvolumes — they're already mounted and
browsable. Urd just needs to provide the navigation frame.

### 7.2 — `urd diff` — what changed between snapshots?

```
$ urd diff htpc-home --from 20260329 --to 20260331

Changes in htpc-home between Mar 29 04:00 and Mar 31 11:45:

  Modified:  142 files
  Created:    18 files
  Deleted:     3 files

  Largest changes:
    projects/urd/target/ (+340 MB)  ← build artifacts
    .config/Code/            (+12 MB)
    projects/urd/src/voice.rs  (+8 KB)

  Show full file list? [y/N]
```

BTRFS snapshots are full filesystem snapshots. `diff` between two snapshots is
`find`+`stat` comparison. This is useful for "what did I change since Tuesday?"
scenarios — a time-travel `git diff` for the entire home directory.

### 7.3 — `urd search` — find a file across time

```
$ urd search "urd.toml" --subvolume htpc-home

Searching for "urd.toml" across 12 snapshots...

  Found in 12 of 12 snapshots:
    .config/urd/urd.toml

  Versions:
    20260331-1145  2.1 KB  (current — matches live file)
    20260331-0400  2.1 KB  (same as current)
    20260330-0400  2.0 KB  ← different
    20260329-0400  1.9 KB  ← different
    ...

  Restore which version? [Enter to skip]
```

This is the restore workflow for "I know the file exists somewhere in my history."
Especially powerful for config files and documents that change gradually.

---

## 8. Notification as a Trust Channel

*Norman: Feedback must be proportional to importance. Over-notification destroys trust
just as surely as under-notification.*

### 8.1 — Three-tier notification language

| Tier | Trigger | Channel | Frequency | Tone |
|------|---------|---------|-----------|------|
| Informational | Backup complete, drive connected | Desktop notify only | Per event (debounced) | Neutral |
| Advisory | Offsite stale >7 days, space <20% | Desktop + badge | Daily max | Concerned |
| Critical | Promise broken, backup failed 2x | Desktop + sound + persistent | Until resolved | Urgent |

The sentinel already has urgency levels and channel routing. This brainstorm adds the
language design: what does each tier *sound like*?

Informational: "Nightly backup complete. All safe."
Advisory: "WD-18TB1 away for 10 days. 3 subvolumes waiting for offsite sync."
Critical: "htpc-home UNPROTECTED. Last successful backup: 3 days ago."

### 8.2 — Notification memory

"I already told you about this" awareness:

```
[First notification]
WD-18TB1 has been away for 7 days. 3 subvolumes waiting for offsite sync.

[Second notification, 4 hours later]
(suppressed — same condition, already notified today)

[Third notification, next day]
WD-18TB1 away for 8 days. Reminder: 3 subvolumes waiting for offsite sync.
```

The sentinel's circuit breaker already provides debouncing. The idea here is making the
debounce *visible* — when the user checks `urd sentinel status`, show when the last
notification was sent and when the next will be:

```
SENTINEL — watching

  Running       since 46h 46m (PID 366735)
  Assessment    2026-03-31T11:33:09 (tick: 15m — all promises held)
  Mounted       WD-18TB
  Last alert    2026-03-31T08:00 (offsite stale — WD-18TB1, 2TB-backup)
  Next check    2026-03-31T12:00 (4h cooldown)
```

### 8.3 — The "all clear" notification

When a condition that triggered a notification is resolved, send a resolution:

```
[Earlier] htpc-root: chain broken on WD-18TB. Next send will be full.
[Now]     htpc-root: chain repaired. Future sends will be incremental.
```

This closes the feedback loop. The user sees problem → resolution, not just
problem → silence. Silence after a warning is ambiguous — did it get fixed or
did the system stop monitoring?

---

## 9. The Mythic Voice — Norman Meets the Norn

*Norman doesn't discuss character. But Urd has one, and it shapes perception.*

### 9.1 — Voice as trust signal

The mythic voice isn't decoration — it's a trust signal. A tool that speaks with
character feels more *intentional* than one that speaks in tech jargon. Compare:

Generic: "Backup completed successfully. 7 snapshots created."
Urd: "All subvolumes recorded. The thread holds."

The user doesn't need to know what "the thread" means mythologically. They hear
confidence and completeness. The voice says "this tool was made with care."

This is the hardest UX investment to justify technically, but Norman's later work
(Emotional Design) argues that aesthetic coherence improves *perceived* reliability.
A tool that feels intentional gets more trust — and a backup tool lives or dies on
trust.

### 9.2 — Voice layering — technical precision under mythic framing

The voice should be a layer, not a replacement:

```
All subvolumes recorded. The thread holds.
  7 snapshots, 5 sends (3 incremental, 2 full), 1.2 GB transferred
```

First line: mythic, for the emotional response.
Second line: technical, for the rational response.
Both lines are true. The user reads as deep as they want.

### 9.3 — Voice only on transitions, not on steady state

The mythic voice activates on *changes*, not on status checks:

- **Backup complete**: "The thread holds." / "A thread frays."
- **Drive connected**: "A new path opens."
- **Promise broken**: "htpc-home: the thread is cut."
- **Status check**: (no mythic voice — just data and recommendations)

This prevents the voice from becoming a gimmick. It speaks when something *happened*.
Status is neutral, factual, dense. Events are evocative, brief, memorable.

### 9.4 — Voice personality in errors

Errors are where character matters most. A cold error message feels like abandonment.

Generic: "Error: send failed (exit 1): No space left on device"
Urd: "Send to WD-18TB failed — the drive is full. See below for remedies."

Not mythic per se, but personal. The tool acknowledges the problem and immediately
offers help. Norman's error design principle amplified by character.

---

## 10. Invisible Worker — Making Autonomy Visible Without Intrusion

*Norman: The best design is one you never notice — until you need it.*

### 10.1 — Heartbeat-as-presence

The sentinel runs silently. How does the user know it's there? Currently: they check
`urd sentinel status`. But presence should be ambient:

- **Spindle icon**: Green means "sentinel is running, last check N minutes ago"
- **Tooltip drilldown**: "Watching since 46h. Last assessment: all promises held."
- **Icon absence**: If Spindle isn't in the tray, the sentinel isn't running. The
  icon's presence IS the heartbeat.

The negative space matters: the icon being there means the system is working. The
icon being gone means something is wrong. Norman calls this a "constraint through
absence" — the missing signifier IS the signal.

### 10.2 — Ambient safety through desktop integration

Beyond the tray icon, could Urd influence the desktop environment?

- **File manager integration**: A column showing "last backed up" for files. Nautilus
  and Dolphin support custom columns. Urd knows which snapshots contain which paths.
- **Desktop notifications on file save**: When you save a critical document, a brief
  reassurance: "Saved. Next snapshot in 12 minutes." (Probably too intrusive — parking
  this one, but the idea of ambient backup awareness is valid.)

### 10.3 — The emergency break-glass moment

When everything has failed — sentinel down, backup hasn't run in 72 hours, no drives
mounted — the notification should be impossible to ignore:

```
CRITICAL: No backup has completed in 72 hours. Your data is not protected.

  Last successful backup: 2026-03-28 04:00 (3 days ago)
  Sentinel status: not running
  Drives: none mounted

  Immediate action: connect any external drive and run `urd backup`
  Or: run `urd doctor` for a full diagnostic
```

This is the catastrophic UX failure mode from the first Norman brainstorm — but now
with the machinery to actually detect and respond to it. The sentinel circuit breaker
already escalates through notification tiers. The idea here: the final escalation
should break through normal notification filtering. On Linux, this means using
`notify-send --urgency=critical` which some DEs render as persistent.

---

## 11. Uncomfortable Ideas

*Required by brainstorm rules. Too ambitious, possibly impractical, but worth naming.*

### 11.1 — Urd as a filesystem layer

Instead of browsing snapshots through `urd browse`, mount a virtual filesystem that
shows time as a dimension:

```
/home/.urd/
  2026-03-31/
    1145/
      .config/urd/urd.toml
    0400/
      .config/urd/urd.toml
  2026-03-30/
    0400/
      .config/urd/urd.toml
```

BTRFS snapshots ARE filesystem objects — they can be bind-mounted. A FUSE layer or
bind-mount management could expose time-travel as filesystem navigation. This is how
macOS Time Machine works internally (the backup disk IS a browsable filesystem).

Complexity: high. Value: enables every existing file tool (grep, diff, cp) to work
across time without learning Urd commands. The user's entire existing workflow becomes
time-aware.

### 11.2 — Urd as an API server

A local HTTP or Unix socket API that Spindle (and other consumers) talk to:

```
GET /api/v1/status
GET /api/v1/subvolumes/htpc-home/snapshots
POST /api/v1/backup
GET /api/v1/progress
```

This replaces `sentinel-state.json` polling with proper request/response. Enables
richer Spindle interactions (browse snapshots in a GUI popup, initiate backup from
tray menu, show progress).

Complexity: moderate (axum or warp, runs inside sentinel). Value: unblocks every GUI
interaction that needs more than a static state file. Risk: now you have a server
process, authentication concerns, port management.

### 11.3 — Predictive backup scheduling

Instead of fixed intervals, Urd learns when data changes most and schedules backups
accordingly:

- Developer writes code 09:00-17:00 → snapshot every 15 minutes during work hours
- Media server ingests new content Saturday nights → backup Sunday morning
- Config files change rarely → daily is fine

The SQLite history + snapshot sizes provide the signal. A simple heuristic (not ML)
could weight intervals by observed change rate. The planner becomes adaptive.

Tension with "invisible worker" principle: adaptive scheduling is harder to reason
about. The user can't predict when the next backup will run. May violate least
astonishment. But Time Machine already does this (backs up when it detects changes),
and users love it.

---

## Grill-Me Results (2026-03-31)

12 candidates scored 0-10 by the user with comments. Key principles surfaced
during the interview are captured in memory (`feedback_ux_principles.md`).

### Scored Ballot

| Rank | Candidate | Score | Verdict |
|------|-----------|-------|---------|
| 1 | `urd` default = one-sentence status (1.1) | 10 | Build soon |
| 2= | Contextual next-action suggestions (4.1) | 9 | Build soon |
| 2= | Staleness escalation in natural language (2.2) | 9 | Build soon |
| 2= | `urd doctor` unified diagnostic (4.2) | 9 | Build soon |
| 5= | Shell completions — static + config-derived (4.5) | 8 | Build soon |
| 5= | Mythic voice on transitions (9.1 + 9.3) | 8 | Design first |
| 7 | Backup-as-narrative (6.1) | 7 | Design first |
| 8 | Space forecasting (2.4) | 6 | Explore further |
| 9= | Problem-first status collapsing (3.1 + 3.2) | 2 | Rejected |
| 9= | Guided restore CLI (4.4 + 7.1) | 2 | Rejected |
| 9= | `urd browse` snapshot explorer (7.1) | 2 | Rejected |
| 9= | Weekly digest (6.4) | 2 | Rejected |

### Resolved Decisions

**1. `urd` default one-sentence status (score: 10)**
Always-accurate awareness check (not cached from sentinel). First-time path detects
missing config and guides to setup. Implementation: default subcommand in `main.rs`
calls `assess_all()`, renders through `voice.rs`.

**2. `urd doctor` (score: 9)**
Fast by default (config + preflight + awareness). `--thorough` adds verify pass.
New command in `commands/`, renders through `voice.rs`.

**3. Contextual next-action suggestions (score: 9)**
Parsimonious — one or two lines max, dimmed, only when there's a clear next step.
The norn anticipates what the user needs. Implementation in `voice.rs` render
functions, pattern-matching on structured output. The voice has a role here: Urd
knows fate, she knows what the user needs before they ask.

**4. Staleness escalation (score: 9)**
Graduated natural language in `voice.rs` based on age thresholds. Zero new computation
— pure presentation over existing awareness data. Philosophy: gentle nudging while
stakes are low prevents crisis decision-making. Applies to drive absence, space
pressure, thread health — any condition that degrades over time.

**5. Shell completions (score: 8)**
`clap_complete` for subcommands/flags (trivial). Dynamic completion for subvolume
names and drive labels reads config only (fast). Snapshot date completion deferred —
filesystem scanning at tab-time is a performance risk with large snapshot sets.

**6. Mythic voice on transitions (score: 8)**
Voice on events, data on queries. Mythic quality = precision + authority + economy,
not Norse vocabulary. Technical descriptions are the default and fallback. The voice
earns character through correctness. Much to unpack — tone is the hard problem.

**7. Backup-as-narrative (score: 7)**
Phased progress output with arc structure (creating → sending → summary). Overlaps
with next-action suggestions and mythic voice — likely built together as part of
unified voice overhaul. "Thread" replaces "chain" in all user-facing text.

**8. Space forecasting (score: 6)**
Pipeline candidate, not build-now. Needs deep design: what to forecast, threshold
policy, user-configurable thresholds, integration with promise system. High ceiling
if designed well. Connection to promise system is undeniable — preventing storage
exhaustion is a high-value target.

### Rejected Candidates (with reasons)

**Problem-first status collapsing (score: 2):** Wrong philosophy. Urd should
encourage success, not front-load problems. If 99 things are in order and 1 is a
problem, the experience should build trust around what is working as well as marking
trouble. Current table design is correct.

**Guided restore CLI (score: 2):** Terminal users already have `cp`, `ls`, `find`,
`diff`. A guided CLI restore that's slower than browsing the snapshot directory is a
tax. The restore experience belongs in a future graphical interface. If CLI restore is
revisited, it must solve snapshot *discovery*, not file *navigation*.

**`urd browse` (score: 2):** Don't rebuild the shell. The filesystem IS the browsing
interface. Feature bloat concern — Urd must work fast and consistent.

**Weekly digest (score: 2):** Wrong communication model. Urd communicates through
presence (Spindle) and events (notifications), not periodic reports. Monitoring
digests belong in the monitoring stack (Prometheus/Grafana). Urd's silence IS the
message that things are fine.

### Key Principles Surfaced

1. **Encourage success, don't front-load problems.** Trust-building > anxiety-driving.
2. **Urd's silence IS the message.** Presence-based + event-driven, not periodic.
3. **Gentle nudging at low stakes prevents crisis decisions at high stakes.**
4. **Urd's voice is mythic because it is precise, not despite it.**
5. **Don't rebuild tools Linux users already have.**
6. **Feature bloat is the enemy.** Every feature must earn its place.

### Naming Decision

"Thread" replaces "chain" in all user-facing text (`voice.rs` only). Data structures
(`ChainHealth`, `chain.rs`) retain internal naming. "Thread" is both more intuitive
(everyone understands a broken thread) and mythically resonant (the norns spin threads
of fate).

### Recommended Next Steps

**Candidates 3, 4, 6, and 7 converge on `voice.rs`.** User decision: design these as
a unified voice overhaul — one design doc, one implementation arc. A vocabulary audit
across all user-facing strings is the prerequisite: every term (chain→thread, send,
receive, secure, store, etc.) evaluated against clarity, precision, consistency, and
mythic resonance.

**Implementation sequence:**

```
1. /brainstorm — vocabulary audit (every user-facing term)
2. /design    — unified voice design doc (nomenclature, graduated language,
                transition voice, next-action patterns)
3. Build (parallel, independent of voice overhaul):
   a. `urd` default one-sentence status (1)
   b. Shell completions (8)
   c. `urd doctor` (2)
4. Build voice overhaul (after design):
   a. Staleness escalation (10)
   b. Next-action suggestions (6)
   c. Mythic voice on transitions (11)
   d. Backup-as-narrative (12)
5. Explore further: space forecasting (design phase)
```
