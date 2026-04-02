---
upi: "000"
date: 2026-04-02
mode: product-review
---

# Steve Jobs Review: The Complete Urd Experience

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Complete user experience — CLI commands, notifications, config, invisible operation, error handling, mythic voice
**Mode:** Product Review

## The Verdict

Urd has the architecture of something great and the surface of something unfinished — the bones are right, but the skin doesn't know what it wants to be yet.

## What's Insanely Great

**The bare `urd` command.** This is the single best design decision in the entire project. You type `urd` with no arguments and you get one sentence:

```
All connected drives are sealed. Last backup 7h ago.
```

That is a complete answer to "is my data safe?" in nine words. If there's trouble:

```
7 of 9 sealed. htpc-docs, subvol3-opptak exposed. Last backup 7h ago.
```

The next-action suggestion chain is exactly right — bare `urd` says "run `urd status`," status says "run `urd doctor`," doctor gives you actual commands. This is progressive disclosure done properly. Each command answers one question and points to the next one. That's how you design a CLI.

**The error translation system.** `translate_btrfs_error()` in error.rs is genuinely excellent. When a btrfs receive fails because the drive is full, the user doesn't see `ERROR: receive: No space left on device`. They see:

```
Destination drive is full
  Why: WD-18TB has insufficient space for this send
  What to do:
    - Check drive space: df -h <mount path for WD-18TB>
    - Run `urd backup` again — retention may free space first
    - If persistent, consider increasing max_usage_percent or adding a drive
```

That's three layers — what happened, why, and what to do about it. At 2am, this is the difference between solving the problem and spiraling. Every pattern is covered: permission denied, read-only filesystem, parent not found, destination missing. The fallthrough case for unknown errors still points to `urd verify` and `journalctl`. Nobody gets stranded.

**The notification voice.** The mythic register works in notifications because notifications are interruptive by nature — they need to earn attention. "The thread of htpc-home has frayed" is arresting without being confusing. "Every thread in the well has snapped. No subvolume is protected. Attend to this — your data stands exposed." — that's a critical notification that will make someone act. The urgency tiers (info/warning/critical) map correctly to the actual gravity.

**The doctor command.** Structured as a proper health check: config, infrastructure, data safety, sentinel, threads. The verdict at the bottom tells you whether to worry. Every warning includes a suggestion arrow pointing to what to do. This is how diagnostics should work.

**The `urd get` command.** It auto-detects the subvolume from the file path, picks the right snapshot, and pipes to stdout or writes to a file. `urd get ~/documents/report.txt --at yesterday` is exactly the command someone types in a panic. The metadata goes to stderr, the content goes to stdout. Pipeline-friendly. Correct.

**Fail-open backups, fail-closed deletions.** This philosophy pervades every decision. SQLite down? Keep backing up. Can't confirm a snapshot is safe to delete? Don't delete it. Pin file write fails after a successful send? Log a warning, send a notification, but don't stop. The user's data is always the priority. This is the right religion.

## What's Not Good Enough

### 1. The vocabulary is split-brained

The code can't decide whether it speaks mythic or mechanical. The status table header says `EXPOSURE` but the column values say `sealed`, `waning`, `exposed`. The drive summary says `connected` (green) and `disconnected` (dimmed). The backup output says `OK` and `FAILED`. The plan says `[CREATE]`, `[SEND]`, `[DELETE]`. The awareness model internally uses `PROTECTED`, `AT RISK`, `UNPROTECTED` — and these leak through in status strings, the JSON daemon output, and the notification titles: "Urd: htpc-home is now AT RISK".

So which is it? Is this tool "sealed/waning/exposed" or "PROTECTED/AT RISK/UNPROTECTED"? Is it "thread" or "chain"? Is it "the well remembers" or "`urd verify` to check thread health"?

The user encounters both registers simultaneously. The status table says `sealed` but the notification that arrives says `AT RISK`. The voice.rs file has a function called `exposure_label()` that maps the old vocabulary to the new one — but only for interactive CLI rendering. Daemon output, notifications, and error messages still use the old vocabulary.

**What great looks like:** One vocabulary everywhere. If the user-facing words are sealed/waning/exposed, then notifications say "htpc-home is now waning," the JSON output uses `"status": "sealed"`, and error messages use the same terms. The internal representation can use whatever enum names it wants — but every touchpoint the user encounters must speak the same language.

### 2. The status table is information-dense but not information-clear

The status command renders something like:

```
EXPOSURE  HEALTH    SUBVOLUME          LOCAL     WD-18TB    THREAD
sealed    healthy   htpc-home          47 (30m)  12 (2h)    unbroken
waning    degraded  htpc-docs          5 (3h)    —          broken — full send (no pin)
```

Seven columns. At 2am. The user has to understand: what is EXPOSURE vs HEALTH? What does THREAD mean? Why does LOCAL show "47 (30m)" — is that 47 snapshots, 30 minutes old? What is the parenthetical after the drive column?

The column headers are: EXPOSURE, HEALTH, PROTECTION, SUBVOLUME, LOCAL, [drive names], THREAD. That's a database query result, not an answer to "is my data safe?"

**What great looks like:** The first thing the user sees answers the question. The summary line already does this — "All sealed." or "2 of 9 sealed. htpc-docs exposed." But then the table hits them with all this detail they didn't ask for. The table should be for people who want the detail, and it should be self-explanatory. "47 (30m)" should be "47 snaps, 30m ago" or the parenthetical semantics should be explained in a header. THREAD should be explained the first time a user sees it, or it should have a more self-evident name.

### 3. The config is a tax form, not a conversation

Look at the example config. It starts with `[general]` containing `state_db`, `metrics_file`, `log_dir`, `btrfs_path`, `heartbeat_file`, `run_frequency`. These are implementation details. The user doesn't care about the heartbeat file path. They care about: what am I backing up? Where am I backing up to? How safe do I want it?

Then `[local_snapshots]` with `roots` containing an array of objects with `path`, `subvolumes`, `min_free_bytes`. Then `[defaults]` with `snapshot_interval`, `send_interval`, `send_enabled`, `enabled`, and nested `[defaults.local_retention]` with `hourly`, `daily`, `weekly`, `monthly`.

A new user has to understand: subvolumes, snapshot roots, retention tiers, protection levels, drive roles, send intervals, snapshot intervals, and the relationship between `run_frequency` and protection level derivation. That's too much. This is a tax form.

**What great looks like:** The minimum config is three things: what to back up, where the drive is, how safe you want it. Everything else has sane defaults. Something like:

```toml
[[subvolumes]]
name = "home"
source = "/home"
protection = "resilient"

[[drives]]
label = "backup-drive"
mount_path = "/run/media/user/backup-drive"
```

That's it. Everything else derived. The current config already has protection levels that derive intervals and retention — but the example config buries this under layers of explicit configuration. The example should teach, not overwhelm.

### 4. The first-run experience is a cliff

Someone installs Urd. They type `urd`. They get:

```
Urd is not configured yet.
Run `urd init` to get started, or see `urd --help`.
```

So they run `urd init`. But `urd init` doesn't create a config — it verifies an existing one. It checks infrastructure, subvolume sources, snapshot roots, drives, pin files. If there's no config, it fails.

So the user has to manually create `~/.config/urd/urd.toml`, figure out their BTRFS subvolume layout, understand snapshot roots and drive paths, set up protection levels, and get it all right before they can even run `urd init` to verify it.

Compare with Time Machine: plug in a drive, click "Use as Backup Disk," done.

**What great looks like:** `urd init` should be the entry point. It detects BTRFS subvolumes, finds mounted external drives, asks a few questions, and generates a config. Then runs verification. The user goes from installation to first backup in minutes, not hours of documentation reading.

### 5. The `--help` text is anemic

```
urd — BTRFS Time Machine for Linux

Commands:
  plan              Preview what Urd will do next
  backup            Back up now — snapshot, send, clean up
  status            Check whether your data is safe
  history           Review past backup runs
  verify            Diagnose thread integrity and pin health
  init              Set up Urd and verify the environment
  calibrate         Measure snapshot sizes for send estimates
  get               Restore a file from a past snapshot
  sentinel          Sentinel — continuous health monitoring
  doctor            Run health diagnostics
  retention-preview Preview retention policy consequences
  completions       Generate shell completion scripts
```

Twelve commands. No grouping. "Diagnose thread integrity and pin health" — what is a thread? What is a pin? "Measure snapshot sizes for send estimates" — why would I do this? The help text assumes the user already understands Urd's internal model.

**What great looks like:** Group them. Core operations (backup, status, get) at the top. Diagnostics (doctor, verify, history) next. Setup (init, calibrate, retention-preview) last. Each description should tell the user when to use it, not what it does internally. "verify" should say "Check that your backup chains are healthy" not "Diagnose thread integrity and pin health."

### 6. The backup output doesn't tell you how long sends will take

The backup summary shows:

```
── Urd backup: success ── [run #42, 93.2s] ──

  OK     htpc-home  [1.2s]  (incremental → WD-18TB, 5.3 MB)
  OK     subvol3-opptak  [847.3s]  (full → WD-18TB, 53.1 GB)
```

During the backup, there's a progress display with a byte counter. But the plan output only estimates sizes, not durations. When someone sees "full → WD-18TB, ~53.1 GB" in the plan, they can't tell if that's going to take 10 minutes or 6 hours. For a tool that runs at 4am, this matters — will it finish before the user wakes up?

### 7. Notification titles use old vocabulary

```
Urd: htpc-home is now AT RISK
```

Should be:

```
Urd: htpc-home is waning
```

The notification body uses the mythic voice ("The thread of htpc-home has frayed") but the title uses the internal enum string. This is a seam the user should never see.

### 8. `--confirm-retention-change` is a terrible flag name

The systemd service runs `urd backup --confirm-retention-change`. This flag exists because retention deletions on promise-level subvolumes are gated by default. The name tells the user nothing about why it exists or when they need it. It's an implementation detail that leaked into the CLI.

If this is the "yes, I really do want to run retention as configured" flag for the autonomous worker, it should be named something like `--scheduled` or should be implicit when `INVOCATION_ID` is set (which the code already checks for full send gating).

## The Vision

When every touchpoint is as good as the bare `urd` command and the error translation system, Urd becomes the tool that makes Linux backup boring. Not boring as in dull — boring as in reliable. The kind of boring where you check `urd` once a week, see "All sealed," and move on with your life. The kind of boring where a disk fails and you type `urd get` and your file appears.

The invisible worker mode is already right in philosophy. The systemd timer runs at 4am. It's nice and quiet. The sentinel watches. Notifications only fire when something actually degrades. The 2am test works: if a disk fails and someone types `urd status`, they get an immediate answer about what's safe and what's not. If they type `urd doctor`, they get actionable steps.

What's missing is polish. The vocabulary needs to be unified. The config needs a guided entry path. The help text needs to assume ignorance, not expertise. The status table needs to be more self-explanatory. These are all fixable without architectural changes — they're in voice.rs, cli.rs, and a hypothetical `urd init --guided` flow.

The mythic voice is right in principle but needs restraint in practice. It works perfectly in notifications and transition events ("first thread to WD-18TB established"). It works in the redundancy advisories ("htpc-home seeks resilience, but all drives share the same fate"). It would be wrong in the status table or error messages. The current boundary is almost right — keep the voice in notifications, advisories, and transition events. Keep the status command clinical and clear. Never let the voice obstruct comprehension.

Urd is 80% of a great product. The remaining 20% is all user-facing surface work. That's good news — it means the hard part is done.

## The Details

**"All connected drives are sealed" is wrong.** The bare `urd` command says "All connected drives are sealed" but drives aren't sealed — subvolumes are. The status summary line correctly says "All sealed." The bare command should match.

**The `Drives:` prefix repeats.** In the drive summary, every drive gets its own line starting with "Drives:" — so if you have three drives, you see:

```
Drives: WD-18TB connected (4.5 TB free)
Drives: WD-18TB1 away
Drives: 2TB-backup disconnected
```

That should be:

```
Drives:
  WD-18TB     connected (4.5 TB free)
  WD-18TB1    away
  2TB-backup  disconnected
```

**The pin summary is meaningless to users.** "Pinned snapshots: 7 across subvolumes" — what is a pinned snapshot? Why does the count matter? This is an internal implementation detail (chain parent protection) that the user didn't ask about. Either explain what it means or remove it from the default output. Move it to `urd verify` or `urd doctor`.

**`[AWAY]`, `[WAIT]`, `[OFF]`, `[SPACE]`, `[SKIP]` tags** — these are used in plan output for skipped operations. `[AWAY]` is fine. `[WAIT]` is fine. But they're presented in a way that requires understanding skip categories. The dimmed rendering helps but doesn't solve the underlying problem: why is the user seeing the planner's internal categorization?

**The transition voice is inconsistent.** After a backup:

```
  htpc-home: thread to WD-18TB mended.
  htpc-home: first thread to WD-18TB1 established.
  All threads hold.
  subvol3-opptak: waning → sealed.
```

"Thread to WD-18TB mended" and "waning → sealed" are two different registers in the same block. The thread language is mythic; the arrow-notation is clinical. Pick one.

**`urd calibrate` — the description is technical.** "Measure snapshot sizes for send estimates" tells the user what it does mechanically but not why they should care. Better: "Help Urd predict how long backups will take."

**The verify command description** says "Diagnose thread integrity and pin health." The user knows neither "thread" nor "pin" at this point. Better: "Check that your backup chains are healthy."

**`run_frequency = "daily"` in config** — this tells Urd how often it runs so it can derive protection promise thresholds. But the user also has to set up the systemd timer independently. There's a seam here: if the user changes the timer but forgets the config field, promise calculations silently diverge from reality.

**The `Last backup:` line in status** shows `2026-03-24T02:00:00 (success, 1m 30s) [#42]`. The ISO timestamp with the T separator is machine-readable but not human-readable. "March 24, 2:00 AM" or even "2 days ago" would be more useful at 2am.

**The redundancy advisory language** is the best writing in the entire project. "htpc-home seeks resilience, but all drives share the same fate." "The offsite copy on WD-18TB1 has aged." "A second drive would guard against the failure of one." This is the mythic voice done right — it adds gravity without sacrificing clarity. Ship more of this.

## The Ask

1. **Unify the vocabulary everywhere.** sealed/waning/exposed must be the words the user sees in every context: status, notifications, JSON output, error messages. This is a one-session task and it fixes the most jarring inconsistency. No internal enum should ever reach the user's eyes as "AT RISK" or "UNPROTECTED."

2. **Fix the notification titles.** Change "Urd: htpc-home is now AT RISK" to "Urd: htpc-home is waning." Change "Urd: all promises broken" to "Urd: all threads exposed." This is the text that appears on the user's desktop and phone. It must be right.

3. **Fix the bare `urd` text.** "All connected drives are sealed" should be "All subvolumes sealed" or just "All sealed." The status summary line already says "All sealed." Be consistent.

4. **Fix the Drives: line repetition.** One "Drives:" header, indented drive list underneath.

5. **Remove or contextualize the pin count** from the status display. Move it to `urd verify` output where it has meaning.

6. **Rewrite the `--help` descriptions.** Group commands into sections. Write descriptions that tell users when to use each command, not what it does internally. "verify" becomes "Check that your backup chains are healthy." "calibrate" becomes "Help Urd predict backup sizes."

7. **Rename `--confirm-retention-change`.** This should be `--scheduled` or eliminated entirely by detecting the systemd invocation context (which the code already does for the full-send policy).

8. **Design `urd init --guided`.** An interactive flow that detects BTRFS subvolumes, finds drives, asks three questions (what to back up, how safe, which drives), and generates a config. This is the biggest gap in the user experience and the highest-leverage improvement. It turns Urd from an expert tool into something anyone with BTRFS can use.

9. **Humanize the Last backup timestamp** in status output. "2 days ago" or "March 24, 2:00 AM" instead of ISO format with the T separator.

10. **Write a minimal example config.** The current example config is 209 lines with 9 subvolumes and 3 drives. Create a `urd.toml.minimal` example that shows the simplest possible configuration: one subvolume, one drive, a protection level, done. Five lines of essential config. Let the full example remain as a reference, but put the minimal one first in the documentation.
