---
upi: "003"
date: 2026-04-02
mode: design-critique
---

# Steve Jobs Review: Backup-Now Imperative

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Design spec UPI 003 — backup-now imperative (`docs/95-ideas/2026-04-02-design-003-backup-now-imperative.md`)
**Mode:** Design Critique

## The Verdict

The core insight is exactly right — and it's one of those things that seems obvious in
hindsight, which is how you know it's a real design insight. But the design gets the flag
name wrong, and the pre-action feedback doesn't go far enough to make the manual experience
feel like consulting an authority.

## What's Insanely Great

**The inversion of defaults.** This is the single best decision in the design. When you
type a command, the command should do what you asked. `--scheduled` on the timer instead of
`--force` on the human — that's exactly the right mental model. The rejected alternatives
section explains this beautifully: "the systemd timer is the special case, not the human at
the terminal." That's product thinking, not engineering thinking. Protect this decision at
all costs.

**The TTY rejection.** Refusing to use TTY detection for behavior selection is correct, and
the reasoning is sharp: piping output shouldn't change what gets backed up. When I built the
original Mac, we had a similar principle — how you observe the system should never change
what the system does. The design correctly confines TTY detection to the presentation layer.

**The space guard preservation.** Manual mode skips intervals but never skips the space
guard. This is the kind of safety discipline that separates a tool you trust from a tool
you use carefully. The user says "back up now" and Urd says "yes" — except when saying yes
would destroy the very data it's protecting. That's not a refusal, that's a guardian.

## What's Not Good Enough

### The flag name is wrong

`--scheduled` is an implementation concept. It describes *when* the backup runs, not *what
it means*. A systemd timer doesn't think of itself as "scheduled" — it just fires. The
flag should describe the behavior it enables, from the perspective of someone reading the
unit file six months from now.

`--auto` is better. It says: "I'm running automatically, apply the automatic-run rules."
Short, clear, scannable in `ps` output. `ExecStart=urd backup --auto` reads like English.
`ExecStart=urd backup --scheduled` reads like a developer wrote it.

Even better: consider `--unattended`. It says exactly what's true — nobody is watching,
apply throttling. But it's long. `--auto` wins on brevity.

This matters because this flag will appear in every systemd unit file, every cron job, every
automation script. It's Urd's most-read CLI argument. Get the word right.

### The pre-action summary is too timid

The design proposes:
```
Snapshotting 7 subvolumes
Sending to WD-18TB: 7 subvolumes (~9.2GB estimated)
Drives away: WD-18TB1, 2TB-backup
```

This is a status report. It's accurate. It's also forgettable. When the user types
`urd backup`, they're in a moment of *agency* — they chose to act. The pre-action feedback
should meet that moment.

What great looks like:
```
Backing up everything to WD-18TB.
  7 snapshots, ~9.2GB to send

  WD-18TB1, 2TB-backup not connected — their copies will wait.
```

Three differences:
1. **Lead with the action and the destination**, not the count. The user wants to know
   "where is my data going?" before "how many subvolumes?"
2. **One line for the core action.** Don't make me parse three lines to understand that
   a backup is happening.
3. **"Their copies will wait"** instead of "Drives away." The user isn't monitoring drive
   attachment state — they want to know that Urd noticed, and that it's not a problem.
   "Away" is a status word. "Will wait" is a promise.

When multiple drives are connected, the first line expands naturally: "Backing up
everything to WD-18TB and 2TB-backup."

### The "Nothing to do" path survives

The design correctly makes manual mode skip intervals, so the "nothing to do" path (line
86-87 of backup.rs) should be nearly impossible to hit in manual mode — you'd need all
subvolumes disabled or all drives disconnected with external-only mode.

But the design doesn't address what happens when it *does* hit. Today it prints
`"Nothing to do."` in dim text. For a manual invocation, that's dismissive. The invoked
norn doesn't shrug.

If a user types `urd backup` and nothing can happen (all disabled, or no drives for
external-only), the response should explain *why* nothing can happen and what the user
can do about it:
```
Nothing to back up — all subvolumes are disabled in config.
  Enable subvolumes in ~/.config/urd/urd.toml
```

This is a small thing. But it's the difference between a tool that respects the user's
intent and a tool that silently declines to help.

### The lock trigger source is hardcoded

Line 84 of backup.rs: `lock::acquire_lock(&lock_path, "timer")`. When the user runs
`urd backup` manually, the lock metadata says the trigger source is "timer." That's a lie.

This isn't in the design scope, but it's directly adjacent — if you're building the
concept of manual vs. scheduled invocation, the lock should know which one it is. When
the user hits a lock conflict (`urd backup` while the timer is running), the error
message should say "a scheduled backup is already running" or "a manual backup is already
running" — not just "locked."

## The Vision

Here's what this feature is really about: it's the first time Urd distinguishes between
being a daemon and being a tool. Every feature you build after this — the encounter,
restore verification, directory restore — will inherit this distinction. Get it right here
and everything downstream benefits.

The manual backup experience should feel like pressing a physical button. One action,
immediate response, clear feedback. Not a form submission. Not a pipeline trigger. A
button.

When I think about what "Back Up Now" meant in Time Machine, it wasn't the backing up that
made it great — it was the *certainty*. You pressed the button, and you knew. The spinning
gear, the progress bar, the timestamp update. You never wondered "did it work?" You never
had to check.

The pre-action summary, the progress display (which already exists), and the backup summary
should form a continuous narrative: "Here's what I'm about to do → Here's what I'm doing →
Here's what I did." That narrative arc is what makes a manual backup feel *handled*.

The design gets the architecture right for this. The pre-action summary is the missing
first beat of that narrative. Make it count.

## The Details

1. **"Drives away"** — this phrase appears in the pre-action summary example. "Away" is
   the vocabulary term for disconnected drives, which is correct per the frozen vocabulary.
   But in the pre-action summary context, the phrase reads oddly. "Not connected" or
   "offline" would be clearer in this specific rendering. The vocabulary freeze covers
   status labels and promise states; rendering text can use natural language that maps to
   the underlying status.

2. **The `skip_intervals` parameter name** — threading a negation (`skip_intervals: true`
   means "don't check intervals") is mildly confusing. The design chose it over an enum for
   simplicity, which is fair. But consider `force_all: bool` — it extends the existing
   `force` concept (which already means "skip interval for one subvolume") to all. The code
   reads: `if force || force_all { true }`. Though I acknowledge `skip_intervals` is more
   precise about what it does.

3. **Open Question Q1 is not really open.** The design recommends that `urd plan` should
   also respect the flag, and the reasoning is airtight: "the plan command exists to answer
   'what will happen if I run backup?'" — it should match backup semantics. This isn't
   a question, it's a decision. Make it. Ship it in the same PR.

4. **The design says "~1 session" but includes `urd plan` consistency.** If Q1 is resolved
   as recommended (it should be), that adds `PlanArgs` changes and plan command wiring.
   Still one session, but call it out in the module map — it's currently missing.

5. **The example pre-action text says "Drives away: WD-18TB1, 2TB-backup"** but the
   design also says disconnected drives come from the skipped list. If a drive is
   disconnected and the user is running in `--local-only` mode, does it still show the
   disconnected drives? It shouldn't — the user explicitly said local-only. The pre-action
   summary should respect the same filters the planner does.

## The Ask

1. **Rename `--scheduled` to `--auto`.** This is the highest-impact change because the
   flag name becomes part of every deployment. Get it right before it ships.

2. **Rewrite the pre-action summary** to lead with the action and destination, not the
   counts. Use natural language. Make it feel like a briefing from an authority, not a
   spreadsheet preview.

3. **Update the lock trigger source** to reflect manual vs. auto invocation. Small change,
   adjacent to the work, improves error messages for lock conflicts.

4. **Resolve Q1 as decided** — `urd plan` matches `urd backup` semantics. Add it to the
   module map and ship it together.

5. **Address the "nothing to do" path** for manual invocations. Even if rare, the response
   should explain why and guide the user, not shrug.
