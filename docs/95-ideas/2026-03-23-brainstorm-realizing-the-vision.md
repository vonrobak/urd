# Brainstorm: Realizing the Urd Vision

> **TL;DR:** A vision-driven brainstorm exploring how Urd becomes the backup system you
> forget exists — until the moment you need it, when it's exactly right. Organized around
> six models: awareness, interaction, trust, configuration, restoration, and lifecycle.
> The unifying insight: Urd should think in terms of *promises about your data* rather
> than *operations on snapshots*.

**Date:** 2026-03-23
**Status:** raw

---

## The Premise

The CLAUDE.md vision says: "If the user forgets Urd exists until the day they need to
restore a file, we've succeeded." This is a radical statement. Most backup tools succeed
by being *visible* — dashboards, progress bars, email reports. Urd succeeds by being
*invisible*. That inversion drives everything below.

But invisibility has a paradox: how does the user trust something they can't see? Time
Machine solved this with a single icon in the menu bar — a clock that spins during backup
and shows the last backup time on hover. One glance, zero interaction, full confidence.
Urd needs its own version of that single-glance confidence.

---

## Model 1: Awareness — How Urd Understands the World

Before Urd can be silent when things are fine and loud when they're not, it needs a
sophisticated model of "fine" and "not fine." Today, Urd knows whether a backup
*succeeded or failed*. That's binary. The vision requires something richer.

### 1.1 — The Protection Promise

Instead of tracking operations, Urd should track *promises*:

```
Promise: "htpc-home is backed up to at least one external drive,
          with the newest backup no older than 48 hours."

Status: KEEPING (last external backup: 6 hours ago, on WD-18TB)
```

```
Promise: "subvol3-opptak has at least 30 days of daily snapshots locally
          and is replicated to at least two external drives."

Status: BREAKING (only on WD-18TB — 2TB-backup not connected in 12 days)
```

The user doesn't configure operations — they declare protection goals. Urd figures out
what operations achieve those goals. This is the deepest form of "guide through
affordances": the user says *what* they want, not *how* to get it.

The config shift:

```toml
# Today: operation-focused
[[subvolumes]]
name = "htpc-home"
snapshot_interval = "15m"
local_retention = { hourly = 96, daily = 30, weekly = 26, monthly = 12 }

# Vision: promise-focused
[[subvolumes]]
name = "htpc-home"
promise = "protected"          # predefined protection level
max_age = "48h"                # newest external backup must be this fresh
min_copies = 1                 # minimum number of external drives
```

Predefined protection levels:

| Level | Meaning |
|-------|---------|
| `guarded` | Local snapshots only, frequent. For fast rollback. |
| `protected` | Local + at least one external drive. The default. |
| `resilient` | Local + multiple external drives, strict freshness. |
| `archival` | Long retention, less frequent, offsite emphasis. |
| `custom` | Full manual control over intervals and retention. |

The planner derives snapshot intervals, retention policies, and send targets from the
promise level. Power users can drop to `custom` for full control — but most users never
need to. This is "flexibility that's easy to operate."

### 1.2 — The State Model: Not Just "Last Run" but "Current Reality"

Today `urd status` shows the result of the last backup run. But the user's question isn't
"what happened last time?" — it's "is my data safe *right now*?"

Urd should maintain a continuous awareness model:

```
Data Protection Report (computed from snapshots + history + drive state)

htpc-home         PROTECTED    last external: 6h ago on WD-18TB
                               local: 47 snapshots spanning 31 days
                               promise: max_age 48h, min_copies 1 — met

subvol3-opptak    AT RISK      last external: 12 days ago (WD-18TB)
                               2TB-backup not seen in 45 days
                               promise: min_copies 2 — UNMET (only 1 drive seen recently)

subvol5-music     GUARDED      local only, no external sends configured
                               22 snapshots spanning 14 days
```

The three states:

- **PROTECTED** — all promises met. Urd is silent.
- **AT RISK** — a promise is degraded or trending toward breach. Urd nudges.
- **UNPROTECTED** — a promise is broken. Urd demands attention.

This state model is what the Sentinel monitors. It's what notifications trigger on. It's
what `urd status` shows. Everything in the app speaks this language.

### 1.3 — Trend Awareness: Predicting Problems Before They Happen

Urd should notice patterns:

- "WD-18TB is filling at 2 GB/day. At this rate it's full in 43 days. If you connect
  2TB-backup and run retention, that buys 6 more months."
- "htpc-home snapshots are growing 15% month-over-month. The local snapshot directory
  will exceed retention capacity in ~4 months."
- "You haven't connected any external drive in 8 days. Three subvolumes are approaching
  their max_age promise threshold."

These aren't alerts — they're observations that Urd stores and surfaces when the user
checks in. In autonomous mode (Sentinel), they become notifications only if they cross
a threshold that threatens a promise.

### 1.4 — The Attention Budget

Not all information deserves the user's attention. Urd should have an internal concept
of an "attention budget" — a prioritized queue of things the user might want to know,
sorted by urgency:

1. **Promise broken** — UNPROTECTED state. Always surface immediately.
2. **Promise degrading** — AT RISK trending toward broken. Surface on next interaction.
3. **Actionable opportunity** — "Drive connected, backup would restore protection." Surface.
4. **Informational** — growth trends, space forecasts, health checks. Surface only when asked.
5. **Routine** — "backup completed, all promises met." Never surface autonomously.

The attention budget means Urd can be both rich in information (when the user asks) and
minimal in interruption (when they don't). The same data, filtered by urgency.

---

## Model 2: Interaction — How Urd Communicates

### 2.1 — Two Personas: The Daemon and The Companion

Urd operates in two fundamentally different modes:

**The Daemon** (systemd timer, Sentinel, cron) — runs autonomously. Success is invisible.
Failure escalates through notifications. The user never sees a terminal. The daemon's
interface is: notifications, `urd status` on demand, Prometheus metrics for dashboards.

**The Companion** (terminal, interactive) — the user is present and engaged. Rich feedback:
progress bars, summaries, explanations. The companion's interface is: live output, color,
animation, suggested next steps.

Today these are conflated. The same `urd backup` output goes to both a systemd journal and
a live terminal. The vision requires Urd to detect which persona is active and adapt:

```rust
// Pseudocode for the interaction model
if atty::is(Stream::Stderr) {
    // Companion mode: rich, live, interactive
    show_progress_bar();
    show_per_operation_status();
    show_summary_with_protection_change();
    suggest_next_actions();
} else {
    // Daemon mode: structured, terse, machine-parseable
    log_json_summary();
    emit_prometheus_metrics();
    if promises_broken { send_notification(); }
}
```

### 2.2 — The Status Heartbeat

In daemon mode, Urd should write a lightweight "heartbeat" file after every run:

```json
{
  "timestamp": "2026-03-23T02:00:15Z",
  "result": "success",
  "promises": { "keeping": 6, "at_risk": 1, "broken": 0 },
  "next_scheduled": "2026-03-24T02:00:00Z",
  "attention_needed": false
}
```

Any external tool (a menu bar widget, a shell prompt decorator, a home automation system)
can read this file to show Urd's state. This is the "spinning clock in the menu bar"
equivalent. The file is the API between Urd and everything else.

A shell prompt integration could look like:

```bash
# In .bashrc / .zshrc
urd_status() {
  local hb="$HOME/.local/share/urd/heartbeat.json"
  [[ -f "$hb" ]] && jq -r '.promises | if .broken > 0 then "⚠" elif .at_risk > 0 then "△" else "" end' "$hb"
}
PS1='$(urd_status)\u@\h:\w\$ '
```

Nothing in the prompt when everything is fine. A subtle triangle when something is at risk.
A warning sign when a promise is broken. Zero effort, always visible.

### 2.3 — Notification Tiers That Match the Attention Budget

Notifications should map directly to the attention budget:

| Attention Level | Channel | Example |
|----------------|---------|---------|
| Promise broken | Desktop notification + optional webhook | "subvol3-opptak has no external backup in 14 days" |
| Promise degrading | Desktop notification (dismissable) | "WD-18TB predicted full in 30 days" |
| Actionable opportunity | Desktop notification (once per event) | "WD-18TB connected — starting backup (3 subvolumes behind)" |
| Informational | Written to heartbeat, shown in `urd status` | Growth trends, space forecasts |
| Routine | Log file only | "Backup completed, all promises met" |

The key insight: **the user configures promises, not notification rules.** Urd decides
what to notify about based on which promises are threatened. The notification system
is derived from the promise model, not configured independently.

### 2.4 — Conversational Setup

What if `urd setup` wasn't a traditional wizard with numbered steps, but a conversational
flow that feels more like talking to a knowledgeable friend?

```
$ urd setup

  Hey! I found 7 BTRFS subvolumes on this system. Let me walk you through
  setting up backups.

  Your /home is on BTRFS (77 GB used). This is usually the most important
  thing to back up — documents, configs, projects.

  → Protect /home with external backups? [Y/n] y

  I also found a large data pool at /mnt/btrfs-pool with 5 subvolumes:
    subvol1-docs      450 GB   (documents, probably important)
    subvol3-opptak    2.8 TB   (large — media or recordings?)
    subvol5-music     1.1 TB   (large — media collection?)
    subvol4-multi     800 GB   (large — media?)
    subvol6-tmp       50 GB    (sounds temporary)

  For the large media subvolumes, would you like to:
  [1] Protect them all (needs a big external drive)
  [2] Let me pick which ones matter most
  [3] Just snapshot locally for now, decide on external later

  → 2

  OK. Which of these are irreplaceable? (recordings you made, documents
  you wrote — things that don't exist anywhere else)

  [x] subvol1-docs       (450 GB — marked important)
  [x] subvol3-opptak     (2.8 TB — you tell me)
  [ ] subvol5-music       (1.1 TB — re-downloadable?)
  [ ] subvol4-multi       (800 GB)
  [ ] subvol6-tmp         (50 GB — sounds temporary)

  Got it. subvol3-opptak gets "resilient" protection (multiple drives),
  subvol1-docs gets "protected" (at least one drive), the rest get
  "guarded" (local snapshots only, fast rollback).

  I see an external drive: WD-18TB (16 TB free). Want to use it? [Y/n] y

  All set. Here's what I'll do:
    • Snapshot everything locally on a schedule
    • Send subvol1-docs and subvol3-opptak to WD-18TB
    • Manage retention automatically
    • Alert you if any protection promise is at risk

  Config written to ~/.config/urd/urd.toml
  Run `urd plan` to preview, or `urd backup` to start now.
```

This isn't just a wizard — it's a guide that makes *recommendations* based on what it
sees. It guesses which subvolumes matter based on size and name patterns. It explains
*why* it's making choices. The user can override anything, but the defaults are thoughtful.

This is "guide through affordances" taken to its natural conclusion: the app makes the
right choice for you, and you just confirm.

---

## Model 3: Trust — How the User Knows Their Data Is Safe

### 3.1 — The Confidence Score (Alternative to Health Score)

Rather than a gamified "87/100 health score," Urd should express confidence as a
*statement*:

```
$ urd status

  Urd is confident your data is safe.

  7 subvolumes protected, all promises met.
  Newest external backup: 6 hours ago.
  Oldest promise margin: htpc-home has 42 hours before max_age breach.

  No action needed.
```

Or when things aren't great:

```
$ urd status

  Urd needs your attention on 1 item.

  6 of 7 subvolumes protected.

  ⚠ subvol3-opptak: PROMISE AT RISK
    Needs 2 external copies, but only WD-18TB has been seen in 30 days.
    Connect 2TB-backup to restore full protection.

  Everything else is fine. Newest external backup: 6 hours ago.
```

The language is deliberate: "confident," "needs your attention," "restore protection."
Not technical jargon. Not colored tables. A statement a non-technical person can read.

This is the answer to "is my data safe?" — in plain language.

### 3.2 — Verification That Proves, Not Just Checks

Today `urd verify` checks that pin files point to existing snapshots and chains are intact.
But it doesn't verify that the data itself is recoverable.

A deeper verification:

```
$ urd verify --deep

  Verifying htpc-home on WD-18TB...
    ✓ 14 snapshots present and readable
    ✓ Most recent matches local: 20260323-1430-htpc-home
    ✓ Random sample: 3 files checked across 3 snapshots (content matches local)
    ✓ btrfs scrub status: clean (last scrub: 2 days ago)

  Verifying subvol3-opptak on WD-18TB...
    ✓ 8 snapshots present and readable
    ⚠ Most recent is 12 days old (local has newer)
    ✓ Random sample: 3 files checked (content matches)
    ✓ btrfs scrub status: clean

  Verification complete: 2 of 2 drives checked, data integrity confirmed.
```

The random sample check is powerful: Urd picks a few files from the snapshot, computes
checksums, and compares against the local copy. It doesn't check everything — that would
take hours — but it statistically proves the backup is readable and correct. If even one
file mismatches, that's an immediate alarm.

### 3.3 — The Recovery Contract

When the user sets up Urd, the final output should be a plain-language recovery contract:

```
Your Urd Protection Summary:

If your system disk fails:
  • /home can be restored from WD-18TB (newest: up to 48 hours old)
  • subvol3-opptak can be restored from WD-18TB (newest: up to 48 hours old)
  • subvol5-music has local snapshots only — not recoverable from disk failure

If you accidentally delete a file in /home:
  • Local snapshots go back 31 days (hourly for 4 days, then daily)
  • You can restore any version from that window

If WD-18TB fails:
  • All data still safe on your system disk
  • Connect 2TB-backup to maintain off-disk protection

Run `urd restore` when you need to get something back.
Save this summary. It's also at ~/.local/share/urd/recovery-contract.txt
```

This is generated from config + state, updated after every backup run, and written to a
well-known path. The user can print it, email it to themselves, or just know it exists.
It answers the question humans actually ask: "what happens if X breaks?"

---

## Model 4: Configuration — Getting to the Right Setup Effortlessly

### 4.1 — Config as Derived State

The fundamental shift: configuration should be *derived* from intentions, not *authored*
by the user. The setup wizard asks about intentions (what's important, what drives do you
have); Urd writes the config. The user may never need to see TOML.

But the TOML layer exists for power users and for debugging. It's the "source of truth"
that the wizard writes to and the planner reads from. This means:

- `urd setup` writes TOML
- `urd config show` displays current config in human-readable form
- `urd config edit` re-enters the wizard for the specific section being changed
- Direct TOML editing is always possible but never required

### 4.2 — Smart Defaults That Learn

When Urd first encounters a subvolume, it should make a reasonable guess about how to
treat it:

| Pattern | Guess | Rationale |
|---------|-------|-----------|
| Name contains "home" or is mounted at /home | High priority, protected | The most common recovery need |
| Name contains "tmp", "cache", "build" | Low priority, guarded only | Ephemeral data |
| Size > 500 GB, name suggests media | Medium priority, conditional | Large but may be replaceable |
| Name contains "doc", "work", "project" | High priority, resilient | Likely irreplaceable |
| Root filesystem (/) | Medium priority, protected | Important but reinstallable |

These are defaults, not rules — the user can override anything. But most users will find
the defaults are exactly right. This is "guide through affordances": the affordance is a
sensible default, not a blank form.

### 4.3 — Configuration Validation as Understanding

Instead of validating config and rejecting errors, Urd should validate config and *explain
the implications*:

```
$ urd config check

  Your current configuration means:

  htpc-home (priority: high)
    → Snapshots every 15 minutes, kept for 31 days locally
    → Sent to WD-18TB daily (incremental, ~120 MB typical)
    → Recovery window: up to 48 hours old from external, 15 minutes from local
    → Promise: protected (1 external copy, 48h max age) — achievable

  subvol3-opptak (priority: high)
    → Snapshots daily, kept for 12 months locally
    → Sent to WD-18TB and 2TB-backup daily
    → ⚠ At current growth (2.1 GB/day), WD-18TB fills in 43 days
    → Promise: resilient (2 external copies) — AT RISK if 2TB-backup not connected

  No syntax errors. 1 operational warning (drive space).
```

This isn't validation — it's a simulation. "Here's what your config *means* in practice."
The user can see the consequences of their choices before any backup runs. This is the
most powerful form of "guide through affordances."

### 4.4 — The One-Line Install Story

The ultimate configuration experience for a new user:

```bash
# Install (future packaging)
sudo dnf install urd        # or: cargo install urd

# First run auto-detects and guides
urd setup

# Done. Urd runs daily via systemd timer.
# Plug in a drive and Urd handles the rest.
```

Three commands. No TOML. No sudoers manual editing. No reading man pages. The setup
wizard handles everything including systemd timer installation and sudoers configuration
(with the user's permission). From zero to protected in under 5 minutes.

---

## Model 5: Restoration — The Moment of Truth

### 5.1 — Restore Should Be Simpler Than Backup

Backup is something you configure once and forget. Restore is something you do in a moment
of stress — you deleted a file, your disk died, you need that version from last Tuesday.
The restore experience must be *simpler* than the backup experience because the user is
already in a bad state.

```bash
# "I deleted thesis.md — where was it?"
urd find thesis.md
  ~/documents/thesis.md
    Found in 23 snapshots (2026-03-01 through 2026-03-23)
    Most recent: 20260323-1430-htpc-home (6 hours ago, 2.3 MB)
    Current: deleted

  → Restore most recent version? [Y/n] y
  Restored ~/documents/thesis.md from 20260323-1430-htpc-home

# "What changed in my home dir yesterday?"
urd diff htpc-home --since yesterday
  142 files modified, 23 new, 5 deleted
  Largest changes:
    ~/projects/urd/target/     +450 MB (build artifacts)
    ~/documents/thesis.md       deleted (!)
    ~/.cache/                  +89 MB

# "Give me my whole home directory from last week"
urd restore htpc-home --from 20260316 --to /mnt/recovery/
  Restoring htpc-home from 20260316-0200-htpc-home...
  77 GB → /mnt/recovery/htpc-home/
  [==============================] 100% — 12 minutes
```

### 5.2 — Restore Discovery: "What Can I Get Back?"

Before restoring, the user needs to know what's available:

```
$ urd snapshots htpc-home

  htpc-home — 47 local snapshots, 14 on WD-18TB

  Today:
    20260323-1500  (30 min ago)  local
    20260323-1430  (1 hour ago)  local, WD-18TB
    20260323-1400  (2 hours ago) local

  This week:
    20260322-0200  local, WD-18TB
    20260321-0200  local, WD-18TB
    ...

  Older (monthly):
    20260301-0200  local, WD-18TB
    20260201-0200  local, WD-18TB

  Tip: Use `urd find <filename>` to search across all snapshots.
```

The display is chronological and grouped by recency — more detail for recent snapshots,
summarized for older ones. The user sees what's available without having to decode snapshot
naming conventions.

### 5.3 — Inline Restore (No Separate Command Needed)

For the simplest restore case — getting a file back from a local snapshot — Urd could
support a direct path syntax:

```bash
# Copy a specific file from a specific snapshot
urd get ~/documents/thesis.md@yesterday

# Copy from a specific snapshot by name
urd get ~/documents/thesis.md@20260322-1430

# Copy to a different location
urd get ~/documents/thesis.md@yesterday -o /tmp/thesis-recovered.md

# Interactive: pick which version
urd get ~/documents/thesis.md
  Found in 23 snapshots. Which version?
  [1] 20260323-1430 (6 hours ago, 2.3 MB)
  [2] 20260322-0200 (yesterday, 2.1 MB)
  [3] 20260321-0200 (2 days ago, 2.1 MB)
  → 1
  Restored to ~/documents/thesis.md
```

The `@` syntax is inspired by git's `file@{revision}` notation. It's terse, memorable,
and feels natural. The smart matching ("yesterday", "last week", "march 15") avoids
forcing users to know snapshot naming conventions.

### 5.4 — File Manager Integration

For desktop users, the ideal restore experience is right-click → "Restore Previous Version."
Urd could support this through:

- **Nautilus/Nemo script:** A shell script in `~/.local/share/nautilus/scripts/` that calls
  `urd get <selected-file>` with a GUI picker (zenity or kdialog for version selection)
- **D-Bus service:** Expose Urd's snapshot data over D-Bus so file managers with plugin
  support can query "what versions of this file exist?"
- **GNOME/KDE integration packages:** Proper desktop integration as a separate project built
  on the Urd CLI

This is how Apple does it — Time Machine is both a command-line tool *and* a Finder
integration. The file manager integration is what makes it "set and forget" for desktop
users.

---

## Model 6: Lifecycle — Growing With the User

### 6.1 — Progressive Disclosure

Urd should have layers of complexity that the user peels back only when needed:

**Layer 0: Just works.** `urd setup`, answer a few questions, forget about it.

**Layer 1: Check in.** `urd status` shows promise state. No config knowledge needed.

**Layer 2: Investigate.** `urd explain`, `urd snapshots`, `urd history` — understand what
Urd is doing. Still no config editing.

**Layer 3: Customize.** `urd config edit` to change protection levels, intervals, drive
assignments. Guided, not raw TOML.

**Layer 4: Power user.** Edit `urd.toml` directly. Custom retention math. Multiple configs.
Scripting with `--json` output.

Most users should live at Layer 0-1 permanently. The fact that Layers 2-4 exist doesn't
impose any complexity on Layer 0 users. This is progressive disclosure done right.

### 6.2 — The Drive Lifecycle

External drives have a lifecycle that Urd should understand:

```
NEW         → First plug-in, Urd asks what to do with it
INITIALIZING → First full send in progress
ACTIVE      → Regular incremental backups
AGING       → Drive space declining, may need replacement
RETIRING    → User marked for retirement, draining data to replacement
ARCHIVED    → Read-only, kept for long-term history
```

When a user buys a new larger drive to replace a full one:

```
$ urd drives replace WD-18TB --with WD-24TB

  WD-24TB detected at /run/media/<user>/WD-24TB (22 TB free)

  Plan:
    1. Initial full send of all subvolumes to WD-24TB (~4.5 TB, est. 6 hours)
    2. Establish incremental chains on WD-24TB
    3. Continue sending to both drives for 7 days (safety overlap)
    4. Retire WD-18TB: stop sending, keep as archival read-only copy

  Start? [Y/n]
```

The drive replacement is guided, safe (overlap period), and results in the old drive
becoming a free archival copy. No data is lost, no chains are broken, and the user didn't
need to understand incremental chains to make it work.

### 6.3 — History as a Feature, Not a Log

`urd history` today shows a table of past runs. But history could be richer — it could
be the story of your data's journey:

```
$ urd history --story

  Urd has been protecting your data for 42 days.

  Total data protected: 4.8 TB across 7 subvolumes
  Total transferred to external drives: 89 GB (incremental saves 98.2% of bandwidth)
  Snapshots managed: 2,341 created, 1,892 pruned by retention

  Highlights:
    Mar 23: First backup of subvol6-tmp. Chain established on WD-18TB.
    Mar 20: WD-18TB approaching 80% — consider freeing space or adding a drive.
    Mar 15: All 7 subvolumes now have incremental chains. No more full sends needed.
    Mar 10: Urd became the sole backup system (bash script retired).

  Zero data loss events. Zero broken promises. You're in good shape.
```

This transforms history from a debugging tool into a confidence builder. The user sees that
Urd has been quietly, competently managing their data. The "42 days, zero data loss" line
is the kind of statement that builds deep trust.

### 6.4 — Urd Learns About Your System

Over time, Urd accumulates knowledge about your specific system:

- Which subvolumes grow fast and which are stable
- What time of day backups have the least I/O contention
- Which drives are connected regularly and which rarely
- How long sends typically take for each subvolume
- Whether compression ratios are stable or changing

This knowledge feeds into smarter defaults. If Urd notices that backups at 02:00 always
contend with a database vacuum that runs at the same time, it could suggest shifting the
timer. If it notices a subvolume has been stable for 6 months, it could suggest relaxing
the snapshot interval to save space.

```
$ urd insights

  Based on 42 days of observation:

  • htpc-home grows ~120 MB/day. Current retention keeps 31 days. Healthy.
  • subvol3-opptak grows ~2.1 GB/day but in bursts (recording sessions).
    Consider: event-triggered snapshots during active recording hours.
  • WD-18TB is your primary backup drive (connected 95% of days).
    2TB-backup is your offsite drive (connected every ~14 days).
  • Backups typically complete in 2-4 minutes (incremental). The longest
    recent backup was 3h 12m (first full send of subvol3-opptak).
```

Not prescriptive, not noisy — just available when the user wants to understand their system
better.

---

## Model 7: The Sentinel Reimagined

### 7.1 — Sentinel as Event-Driven State Machine

The Sentinel isn't just a "watch for drive plug-in and start backup" daemon. It's the
runtime that keeps Urd's awareness model up to date:

```
Events:
  DriveConnected(uuid) →
    if known_drive: check promises, start backup if needed
    if unknown_drive: offer interactive setup (if TTY) or ignore (if daemon)

  DriveDisconnected(uuid) →
    update drive state
    recalculate promise status
    if any promise now unmet: queue notification

  TimerFired →
    run scheduled backup
    update awareness model
    write heartbeat

  BackupCompleted(result) →
    update promise states
    write heartbeat
    notify if state changed (better or worse)

  UserInteraction(command) →
    execute command with full interactive feedback
    always update awareness model after
```

The Sentinel holds the awareness model in memory (backed by SQLite) and reacts to events.
Every event updates the model. Every update potentially changes promise states. Promise
state changes drive notifications. This is the integration layer the CLAUDE.md envisions.

### 7.2 — Sentinel Composability

Other features plug into the Sentinel's event stream:

- **Notifications:** subscribe to promise state changes
- **Metrics:** subscribe to backup completions
- **Heartbeat:** subscribe to all events
- **Drive lifecycle:** subscribe to drive connect/disconnect
- **Insights:** subscribe to backup completions (for trend analysis)

New features don't need to know about the Sentinel's internals — they subscribe to events
and react. This is how the Sentinel "interacts with other features of the app" cleanly.

### 7.3 — Lightweight or Heavy: User's Choice

The Sentinel should work at multiple levels of commitment:

- **No Sentinel:** `urd backup` via systemd timer only. Simple. Reliable. Sufficient.
- **Passive Sentinel:** watches for drive events, writes heartbeat, sends notifications.
  Doesn't initiate backups — that's still the timer's job.
- **Active Sentinel:** watches for drive events AND initiates backups when needed to meet
  promises. The full Time Machine experience.

The user can start with the timer and graduate to the Sentinel when they're ready. No
Sentinel is ever required. This is progressive disclosure applied to system architecture.

---

## The Unifying Insight

All six models converge on one idea: **Urd should think in promises, not operations.**

The user declares: "keep my home directory safe." Urd figures out: create snapshots every
15 minutes, send incrementally to WD-18TB daily, keep 30 days of history, alert if the
drive isn't connected for more than 48 hours.

The operations are implementation details. The promise is the interface.

When the user asks "is my data safe?" Urd doesn't say "the last backup ran at 02:00 and
exited 0." It says "yes, all your protection promises are being kept." Or it says "no —
here's what needs your attention and here's what to do about it."

This is what it means to tend the Well of Urðr. Not to catalog every event in history,
but to know — and to let you know — that what matters is preserved.
