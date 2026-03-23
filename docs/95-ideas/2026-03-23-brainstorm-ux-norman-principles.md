# Brainstorm: Urd UX Through Don Norman's Lens

> **TL;DR:** A UX-focused brainstorm applying Don Norman's design principles from
> "The Design of Everyday Things" to Urd's terminal interface. Evaluates every
> user-facing surface against affordances, conceptual models, feedback, mapping,
> constraints, error design, least astonishment, and the two gulfs. The deepest
> insight: a backup tool that fails obscurely is a false-sense-of-security generator.

**Date:** 2026-03-23
**Status:** raw

---

## The Catastrophic UX Failure Mode

Before applying any principle, name the UX catastrophe: **the user believes their data
is backed up, but it isn't.** Every UX decision must be evaluated against this. A pretty
progress bar on a failing backup is worse than an ugly error on a working one. Norman would
say: the presented model ("your data is safe") must match the system model (your data
actually IS safe). When these diverge, the user discovers the truth at restore time — the
worst possible moment.

---

## 1. Affordances & Signifiers

*What actions are possible, and how does the user discover them?*

### Current State Assessment

Urd's `--help` is functional but minimal. Each subcommand has a one-line description and
bare flag listing. Compare with what Norman would expect: the help text is the **signifier**
— the only way a terminal user discovers what the tool can do.

**What's good:**
- Command names are self-documenting: `backup`, `plan`, `status`, `verify`, `calibrate`
- `--dry-run` is discoverable in `backup --help`
- Flag names are descriptive: `--local-only`, `--external-only`, `--subvolume`

**What's missing:**

### 1.1 — Shell completions (tab-discoverable affordances)

Shell completions are literally affordance machinery — they make possible actions visible
at the Tab key. For a backup tool this is table stakes. A user typing `urd <TAB>` should
see all commands. `urd backup --<TAB>` should show all flags. `urd backup --subvolume <TAB>`
should complete to configured subvolume names.

clap's `clap_complete` crate generates these for bash/zsh/fish from the existing CLI
definitions. Near-zero effort, large discoverability gain.

```bash
urd completions bash > /etc/bash_completion.d/urd
urd completions zsh > ~/.zfunc/_urd
urd completions fish > ~/.config/fish/completions/urd.fish
```

The dynamic completions (subvolume names, drive labels) require `clap_complete`'s custom
completer — slightly more work but very high value because the user doesn't need to
remember exact subvolume names.

### 1.2 — Help text with examples and grouped flags

Norman's signifier principle says: don't just list what's possible — show what it looks
like in practice. Compare:

**Current:**
```
Usage: urd backup [OPTIONS]
Options:
      --dry-run                Show what would be done without executing
      --priority <PRIORITY>    Only process subvolumes of this priority (1-3)
```

**Norman-grade:**
```
Usage: urd backup [OPTIONS]

Preview first, then run:
  urd backup --dry-run               Preview what would happen
  urd backup                         Execute the full backup

Filter what to back up:
  urd backup --subvolume htpc-home   Back up one subvolume only
  urd backup --priority 1            Back up high-priority subvolumes
  urd backup --local-only            Snapshots only, skip external sends
  urd backup --external-only         External sends only, skip snapshots

Options:
      --dry-run                Show what would be done without executing
      --priority <PRIORITY>    Only process subvolumes of this priority (1-3)
      ...
```

clap supports `after_help` and `before_help` for this. The flag listing stays for
completeness, but the examples teach the *workflow*, not just the syntax.

### 1.3 — Suggested next actions in output

After every command, suggest what the user might want to do next:

```
$ urd init
  ...
  Initialization complete.

  Next: urd calibrate       Measure snapshot sizes (recommended before first send)
        urd plan             Preview what a backup would do
        urd backup --dry-run Dry-run a full backup
```

```
$ urd backup
  ...
  Urd backup completed: success

  Next: urd status           Check current state
        urd verify           Verify chain integrity
        urd history --last 1 Review this run's details
```

This is a signifier for the next affordance. The user never has to wonder "what do I do
now?" Norman calls this "bridging the Gulf of Execution."

### 1.4 — `urd` with no arguments should be useful

Currently `urd` with no arguments shows the help text (clap default). But the most
useful default for a backup tool is `urd status` — show me the state of my backups.

At minimum: `urd` alone should show the help but with a prominent hint:
```
Tip: Run `urd status` to see your backup state, or `urd --help` for all commands.
```

Or more boldly: make `urd` alone equivalent to `urd status`. The user who types `urd`
wants to know "are my backups OK?" not "what subcommands exist?"

---

## 2. Conceptual Models

*Does the system's presented model match the user's mental model?*

### Current State Assessment

Urd's conceptual model is sound but not always visible:
- **Planner → Executor** — but the user sees `urd plan` and `urd backup`, which maps well
- **Subvolume → Snapshots → External sends** — the hierarchy is clear in `urd status`
- **Chain = incremental send chain** — this is domain-specific and not self-explanatory

### 2.1 — Explain the mental model in help text

The top-level `--help` should teach the model in one paragraph:

```
Urd creates read-only snapshots of your BTRFS subvolumes, sends them incrementally
to external drives, and manages retention so old snapshots are cleaned up. Think of
it as Time Machine for Linux — snapshots are your local safety net, external sends
are your offsite/offline protection.

Commands follow the backup lifecycle:
  plan      → See what would happen (safe, read-only)
  backup    → Execute the plan (create, send, delete)
  status    → Check current state
  verify    → Validate chain integrity
  history   → Review past runs
  calibrate → Measure sizes for space estimation
  init      → First-time setup and validation
```

### 2.2 — `urd explain` — make the model inspectable

A dedicated command that explains *why* the planner made each decision:

```
$ urd explain --subvolume htpc-home

htpc-home — source: /home, priority: 1, interval: 15m

  Local snapshots:
    15 snapshots, newest: 20260323-1500 (30 minutes ago)
    Next snapshot due: in ~0 minutes (interval elapsed)

  External sends:
    WD-18TB: 14 snapshots, chain: incremental
      Last sent: 20260323-1430 (1 hour ago)
      Next send due: now (interval elapsed)
      Estimated send size: ~120 MB (based on last incremental)
      Drive space: 4.4 TB free, need ~144 MB (with 1.2x margin) — OK

    2TB-backup: not mounted

  Retention:
    Local: keeping 24h hourly, 30d daily, 26w weekly, 12m monthly
    Candidates for deletion: 2 snapshots (20260310-htpc-home, 20260309-htpc-home)
    Protected from deletion: 3 snapshots (newer than oldest pin)
```

This bridges both gulfs: the user understands *what* will happen (Execution) and *why*
(Evaluation). They can verify their mental model against the system's actual reasoning.

### 2.3 — Chain health as a first-class concept

The "incremental chain" is the single most important performance property, but it's
presented as a column in a table. Norman would say: if it matters, make it visible.

```
$ urd status

htpc-home       15 local, 14 on WD-18TB
                Chain: healthy (incremental, parent: 20260323-1430-htpc-home)
                Next send: ~120 MB incremental

subvol3-opptak  15 local, 1 on WD-18TB
                Chain: BROKEN (no pin — next send will be FULL: ~2.8 TB)
                Action needed: Run `urd backup --subvolume subvol3-opptak`
```

When the chain is broken, the user immediately sees the consequence (full send = hours
of I/O) and the remedy. This is Norman's "specific, causal, actionable" error pattern
applied proactively.

---

## 3. Feedback

*Every action must produce immediate, informative feedback.*

### Current State Assessment

**Good feedback:**
- Progress display during sends: `47.3 MB @ 156.2 MB/s [0:03]`
- Colored `[CREATE]` / `[SEND]` / `[DELETE]` tags in plan output
- Colored OK/FAIL/WARN in verify output
- Exit code 1 on failure

**Missing feedback:**

### 3.1 — Post-backup structured summary

The current backup output lists results per-subvolume, but doesn't answer the most
important question: "Is my data safer now than it was before?"

```
$ urd backup
  ...

  ━━━ Backup Complete ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Result: success (7 subvolumes, 2m 34s)

  Snapshots created:  7
  Sends completed:    5 (3 incremental, 2 full)
  Bytes transferred:  1.2 GB
  Snapshots deleted:  12 (retention)

  Protection change:
    htpc-home        was 2h behind → now current
    subvol3-opptak   was 1d behind → now current
    subvol6-tmp      local only (no external sends)
    2TB-backup       not mounted — 3 subvolumes NOT backed up externally

  Next scheduled: 2026-03-24 02:00 (systemd timer)
```

The "Protection change" section is the feedback Norman demands: did the system do what
I wanted? The user sees the *effect*, not just the operations.

### 3.2 — Silent failure is the enemy — surface skipped sends loudly

Today, when a send is skipped (drive not mounted, insufficient space), it appears as a
dimmed `[SKIP]` line in the plan output. During a real backup, it's a line among many.

For the catastrophic UX failure mode (user thinks data is backed up, but isn't), skipped
sends need *louder* feedback:

```
  ⚠ WARNING: 3 subvolumes were NOT sent externally this run:
    subvol3-opptak  — drive 2TB-backup not mounted (too large for mounted drives)
    subvol5-music   — estimated ~1.3 TB exceeds 800 GB available on WD-18TB
    subvol4-multi   — send disabled in config
```

This should be impossible to miss. It's the "you might think you're safe but you're not"
warning. Norman's principle: the system must make its state visible even (especially) when
the state is bad.

### 3.3 — Startup confirmation for long operations

Before a multi-hour backup, confirm what's about to happen:

```
$ urd backup

  Urd backup plan — 2026-03-23 15:30

  7 subvolumes, 5 sends planned (2 full, 3 incremental)
  Estimated transfer: ~3.2 GB, estimated time: ~4 minutes

  Largest operation: subvol3-opptak → WD-18TB (full, ~2.8 TB, ~3 hours)

  Press Enter to start, or Ctrl+C to cancel...
```

Only shown on TTY, and only when a full send is planned (incremental sends are fast
enough to not need confirmation). The user sees the commitment before making it.

Skip with `--yes` or `--no-confirm` for scripted/systemd use.

### 3.4 — Per-operation progress with context

The current progress shows bytes and rate, but not *which* send is in progress or how
many remain:

**Current:** `47.3 MB @ 156.2 MB/s [0:03]`

**Norman-grade:**
```
  [2/5] htpc-home → WD-18TB (incremental)
        47.3 MB @ 156.2 MB/s [0:03]
```

The `[2/5]` counter tells the user where they are in the overall process. The subvolume
and drive names provide context. The user knows both "is it working?" and "where am I?"

### 3.5 — Sound/notification on completion

For long-running backups on a workstation, a desktop notification when the backup
completes (or fails):

```
notify-send "Urd backup complete" "7 subvolumes, 1.2 GB transferred, 2m 34s"
```

Only on TTY. Pairs with the sentinel daemon for drive-plug events.

---

## 4. Mapping

*The relationship between controls and their effects should feel natural.*

### 4.1 — Flag consistency across commands

Currently, `--subvolume` works on `plan`, `backup`, `verify`, `history`, and `calibrate`
but each has slightly different semantics. Norman's mapping principle says: same name
should mean the same thing everywhere.

Audit: does `--subvolume htpc-home` always mean "filter to this one subvolume"? Yes —
this is already consistent. Good.

But `--priority` is only on `plan` and `backup`, not on `status`, `verify`, or `history`.
Should it be? If a user thinks in terms of priorities, they might want `urd status --priority 1`.

### 4.2 — Workflow-ordered command listing

The help text lists commands alphabetically-ish. Norman would order them by workflow:

```
Backup lifecycle:
  plan       Preview what would happen (safe, read-only)
  backup     Execute: create snapshots, send, prune
  status     Check current state at a glance
  verify     Deep chain integrity check
  history    Review past backup runs

Setup:
  init       First-time validation and setup
  calibrate  Measure snapshot sizes for estimation
```

This maps to how the user thinks about the tool: the backup cycle is primary, setup
is secondary.

### 4.3 — `urd backup` output should mirror `urd plan` output

If the user runs `urd plan` to preview, then `urd backup` to execute, the output should
look *identical* — same grouping, same operation labels, same colors — but with results
appended:

```
  [CREATE] /home → /.snapshots/htpc-home/20260323-1530-htpc-home .... OK (0.3s)
  [SEND]   20260323-1530-htpc-home → WD-18TB (incremental) ......... OK (4.2s, 120 MB)
  [DELETE]  20260310-htpc-home (monthly thinning) ................... OK (0.1s)
```

The plan is the prediction; the backup is the prediction + outcome. Same structure,
extended with results. The user's mental model from `plan` carries directly to `backup`.

---

## 5. Constraints

*Guide users toward correct use. Prevent misuse before it happens.*

### 5.1 — Config validation with specific fixes

Today, config errors produce messages like "no snapshot root found for subvolume X."
Norman demands: say what's wrong, why, and how to fix it.

```
Error: Subvolume "htpc-home" has no snapshot root configured.

  The [local_snapshots.roots] section must include a root whose
  subvolumes list contains "htpc-home". For example:

  [local_snapshots]
  roots = [
    { path = "~/.snapshots", subvolumes = ["htpc-home"] }
  ]

  Config file: ~/.config/urd/urd.toml (line 12)
```

### 5.2 — Pre-flight checks before destructive operations

Before `urd backup` runs, validate:
- Config is parseable and internally consistent
- State DB is writable
- All configured snapshot sources exist as btrfs subvolumes
- `sudo btrfs` is available (non-interactive test)
- At least one snapshot root is writable

If any check fails, refuse to start with a specific diagnostic. Don't let the user
discover halfway through a 3-hour backup that sudo isn't configured.

```
Pre-flight check failed:

  ✗ sudo btrfs: permission denied (sudoers not configured for btrfs commands)
    Fix: sudo visudo -f /etc/sudoers.d/urd
    See: urd sudoers (generates the needed entries)

  ✗ /mnt/btrfs-pool not mounted
    Fix: mount the btrfs pool, or disable subvolumes sourced from it

Backup not started. Fix the issues above and retry.
```

### 5.3 — Protect against "I forgot to rebuild"

The scenario that prompted this entire session. After code changes, the binary must be
rebuilt. Urd could embed the git commit hash at build time and warn when it's stale:

```
$ urd backup
  Note: Urd binary was built from commit a60f031 (3 days ago).
        Local repo HEAD is 3e43e21 (today).
        Run `cargo install --path .` to update.
```

This uses `built` or `vergen` crate to embed build metadata. The check compares against
the repo's HEAD (if running from within the repo directory).

More simply: `urd --version` could show the build date and commit:

```
$ urd --version
urd 0.1.0 (built 2026-03-23, commit a60f031)
```

### 5.4 — Refuse to run without a config, with guidance

```
$ urd backup

Error: No configuration file found.

  Urd looks for config at: ~/.config/urd/urd.toml
  (Override with: urd --config /path/to/config.toml)

  To get started:
    urd init          Interactive setup (creates config)
    urd setup         Guided wizard (scans system, suggests config)

  Or copy the example:
    cp /path/to/urd/config/urd.toml.example ~/.config/urd/urd.toml
```

### 5.5 — Warn on dangerous retention policies

```
Warning: Subvolume "htpc-home" retention policy keeps only 7 daily snapshots.
         With send_interval = "1d", a 1-week drive absence would leave
         no local snapshots to send from.

         Consider: increase to daily = 14, or decrease send_interval.
```

This is a semantic constraint: the config is valid, but the *combination* of values
creates a footgun. Norman would say: the system should understand the user's intent
well enough to warn when the intent seems contradictory.

---

## 6. Error Messages as First-Class Design

*Every error must be specific, causal, and actionable.*

### 6.1 — Structured error hierarchy

Today, btrfs errors pass through as "Btrfs command failed: send failed (exit 1): ..."
with the raw stderr appended. Norman demands layers:

```
Error: Send failed for htpc-home → WD-18TB

  What: btrfs send | receive pipeline failed after transferring 1.1 TB
  Why:  Destination drive is full (btrfs receive: No space left on device)

  What to do:
    • Run `urd calibrate` to measure current snapshot sizes
    • Check drive space: `df -h /run/media/<user>/WD-18TB`
    • Consider freeing space: `urd backup --external-only` may trigger retention

  Technical details (--verbose for full output):
    btrfs send exit code: 141 (SIGPIPE — receiver died)
    btrfs receive exit code: 1
    btrfs receive stderr: "ERROR: receive: No space left on device"
    Bytes transferred before failure: 1,100,000,000,000
```

The hierarchy: human summary → cause → remediation → technical details. The user reads
only as deep as they need. Most users stop at "drive is full." Power users want the exit
codes.

### 6.2 — Error codes for scripting

Define a set of exit codes that scripts can act on:

| Code | Meaning |
|------|---------|
| 0 | Success — all operations completed |
| 1 | Partial — some operations failed |
| 2 | Failure — all operations failed |
| 3 | Config error — invalid config or missing file |
| 4 | Lock error — another backup is running |
| 5 | Pre-flight error — system not ready |

Today Urd uses 0 and 1. More granular codes let wrappers and monitoring scripts
distinguish "backup had issues" from "backup couldn't even start."

### 6.3 — Contextual "did you mean?" for typos

```
$ urd backup --subvolume htpc-hom

Error: Unknown subvolume "htpc-hom"
  Did you mean: htpc-home ?

  Configured subvolumes: htpc-home, htpc-root, subvol1-docs, ...
```

clap has built-in suggestions for subcommands. For flag values, Urd can compute
Levenshtein distance against configured subvolume names. Small effort, large Norman
points — the system helps the user recover from errors rather than just rejecting them.

---

## 7. Principle of Least Astonishment

*The system should behave in the way the user least expects to be surprised by.*

### 7.1 — `--dry-run` must be sacred

`--dry-run` must never modify state. Not the SQLite database, not pin files, not log
files on disk. Currently Urd's dry-run calls `plan::plan()` which is read-only, then
prints and exits. This is correct. But it should be documented as a guarantee, and any
future feature that touches state should check for dry-run mode.

### 7.2 — Interruption safety should be visible

When the user presses Ctrl+C, Urd currently prints "Signal received, finishing current
operation..." but doesn't explain what "finishing" means:

```
^C
  Shutting down gracefully...
  Finishing: htpc-home → WD-18TB (incremental) — waiting for send to complete
  Cleanup: checking for partial snapshots...
  Partial snapshot cleaned: 20260323-1530-htpc-home on WD-18TB
  Shutdown complete. 4 of 7 subvolumes were processed.
  Re-run `urd backup` to process the remaining 3.
```

The user sees exactly what's happening, what was cleaned up, and what remains. No
surprises.

### 7.3 — Consistent color semantics

Establish and maintain a color vocabulary:

| Color | Meaning | Example |
|-------|---------|---------|
| Green | Success, healthy, OK | `OK`, `success`, `incremental` |
| Red | Failure, error, danger | `FAIL`, `failure`, `ERROR` |
| Yellow | Warning, attention needed | `WARN`, `partial`, stale pins |
| Blue | Active/in-progress | `[SEND]`, progress display |
| Dimmed | Skipped, not applicable, secondary | `[SKIP]`, unmounted drives |
| Bold | Headers, subvolume names, emphasis | table headers, summary lines |

This should be documented in the codebase so future commands don't accidentally use
red for informational output or green for warnings.

### 7.4 — `urd verify` always works, even with no drives

Norman would be appalled if `verify` crashed or showed confusing output when no drives
are mounted. It should gracefully report what it can check and what it can't:

```
$ urd verify

  Verifying htpc-home...
    WD-18TB:  Drive not mounted — chain not verifiable
    WD-18TB1: Drive not mounted — chain not verifiable

  Verifying subvol3-opptak...
    WD-18TB:  Drive not mounted — chain not verifiable

  Note: No external drives are mounted. Chain verification requires drives.
        Local snapshot directories are healthy.

  Verify complete: 0 OK, 2 warnings, 0 failures
```

---

## 8. The Two Gulfs

### Gulf of Execution: "How do I make the system do what I want?"

**Bridges needed:**

### 8.1 — `urd help <topic>` for conceptual help

Beyond `--help` (syntax), provide conceptual help:

```
$ urd help chains
  Incremental Chain

  Urd sends snapshots to external drives using btrfs send/receive. After the
  first "full send" (the entire snapshot), subsequent sends are "incremental"
  — only the differences since the last sent snapshot are transferred.

  The chain is tracked via "pin files" that record which snapshot was last
  sent to each drive. When the chain is healthy, sends are fast (MB instead
  of TB). When broken, the next send must be a full send.

  Common causes of broken chains:
    • Pin file deleted or corrupted
    • Parent snapshot deleted by retention
    • Drive was reformatted

  To check chain health: urd verify
  To repair a chain:     urd backup (will auto-detect and do a full send)

$ urd help retention
  Retention Policies
  ...

$ urd help drives
  External Drives
  ...
```

### 8.2 — Config file comments as inline documentation

The example config already has good comments. But when `urd init` or `urd setup` generates
a config, the generated file should include the same quality of comments — explaining not
just what each field does, but *why* you'd change it.

### Gulf of Evaluation: "Did the system do what I wanted?"

**Bridges needed:**

### 8.3 — `urd status` as the primary evaluation surface

Status should answer all the questions a user has after a backup:
- When was the last backup? (timestamp)
- Did it succeed? (result)
- Is my data on the external drive? (chain status per drive)
- Is anything falling behind? (subvolumes not backed up recently)
- Is any drive running low? (space warnings)
- What should I do next? (suggested actions)

The current `urd status` shows a table and drive info. It should also show:

```
  Attention needed:
    subvol3-opptak has not been sent to WD-18TB in 3 days (threshold: 2 days)
    2TB-backup not connected since 2026-02-15 (45 days — offsite backup stale)
    WD-18TB has 500 GB free — predicted full in 43 days at current growth rate
```

This is the Gulf of Evaluation bridge: the system proactively tells you what needs
attention.

### 8.4 — Weekly digest (for unattended operation)

For users running Urd via systemd timer, the interaction surface is zero most of the
time. A weekly summary (via notification, email, or written to a well-known file) bridges
the evaluation gulf:

```
Urd Weekly Summary (2026-03-17 to 2026-03-23)

  7 backup runs, all successful
  Total transferred: 8.4 GB across 5 drives

  Health:
    All chains incremental — no full sends needed
    All drives connected at least once this week
    subvol4-multimedia: still disabled (no external sends configured)

  Space forecast:
    WD-18TB: 4.4 TB free, 372 days at current rate
    WD-18TB1: 3.1 TB free, 289 days at current rate

  No action needed.
```

### 8.5 — `urd why` — explain the last run's decisions

After a backup, the user might wonder "why was subvol3-opptak skipped?" or "why was
that a full send instead of incremental?"

```
$ urd why subvol3-opptak

  Last run (#42, 2026-03-23 02:00):

  subvol3-opptak was SKIPPED for WD-18TB because:
    Estimated send size (~1.3 TB) exceeds available space (800 GB)
    Source: calibrated size from 2026-03-20 (3 days ago, still fresh)

  subvol3-opptak was sent to WD-18TB1 (full send) because:
    No pin file for WD-18TB1 — this is the first send to this drive
    Transfer completed: 2.8 TB in 2h 34m
```

This is `urd explain` but backward-looking — explaining decisions already made rather
than predicting future ones.

---

## 9. Aesthetic Integrity & Visual Hierarchy

Norman doesn't talk much about aesthetics, but the "Design of Everyday Things" revision
acknowledges that aesthetics affect perceived usability.

### 9.1 — Consistent output structure across commands

Every command's output should follow the same visual grammar:

```
[Header — bold, describes what you're looking at]

  [Content — indented, organized, colored by severity]

  [Summary — bold, bottom-line answer]

  [Next steps — dimmed, optional]
```

### 9.2 — Machine-readable output mode

```
urd status --json
urd history --json
urd plan --json
```

For scripting, monitoring, and integration. The human-readable output is designed for
humans; the JSON output is designed for machines. Norman wouldn't mix them — each
audience gets its own output, optimized for their needs.

### 9.3 — No-color mode (accessibility)

Respect `NO_COLOR` environment variable (de facto standard). Also `--no-color` flag.
Color should enhance but never be the only carrier of meaning — "OK" (green) and "FAIL"
(red) also carry meaning through the text itself. This is already mostly true in Urd,
but should be audited.

---

## Summary: The Norman Audit Scorecard

| Principle | Current Grade | Key Gap |
|-----------|--------------|---------|
| Affordances / Signifiers | B | No shell completions, minimal help examples |
| Conceptual Model | B+ | Model is sound but not explicitly taught |
| Feedback | B | Progress works; post-backup summary needs work |
| Mapping | A- | Good flag naming; command ordering could improve |
| Constraints | C+ | Config errors are vague; no pre-flight checks |
| Error Design | C | Raw btrfs errors passed through; not actionable |
| Least Astonishment | A | Dry-run is safe; Ctrl+C is handled; colors consistent |
| Gulf of Execution | B- | No completions; no conceptual help topics |
| Gulf of Evaluation | B- | Status shows state but not recommendations |

**The single highest-value change:** Shell completions + structured error messages.
These are the two places where Urd most often leaves the user stranded.

**The single most Norman thing Urd could do:** After every backup, answer the question
"is my data safe?" explicitly. Not "backup completed: success" but "all 7 subvolumes
are backed up to at least one external drive, newest backup is 2 minutes old."
