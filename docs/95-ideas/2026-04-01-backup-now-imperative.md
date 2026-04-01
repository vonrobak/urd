# Idea: `urd backup` as imperative action

## The problem

`urd backup` typed by a human in a terminal does the same thing as `urd backup` called
by the systemd timer: it runs the planner, checks intervals, and skips anything that
hasn't elapsed. A manual invocation after the nightly run produces "0 sends, 0.0s" —
the user asked for a backup and got told nothing needed doing.

This violates the Time Machine mental model. When a user presses "Back Up Now" they
expect backups to happen. The interval logic exists to throttle automated runs, not to
refuse manual ones.

## The insight: two modes of existence, two invocation semantics

CLAUDE.md describes Urd's two modes: the invisible worker (timer, sentinel) and the
invoked norn (user at terminal). These modes have different UX contracts:

- **Invisible worker:** Respect intervals. Don't snapshot more often than configured.
  Silence means safety. The flag `--scheduled` (or equivalent) signals this mode.
- **Invoked norn:** The user is here, asking for action. Take fresh snapshots. Send to
  all connected drives. Report what's happening. Guide toward good practices.

Currently both modes use the same planner path with the same interval filtering. The
planner needs a "manual override" concept where interval checks are skipped.

## What "backup now" should do

1. **Take fresh local snapshots** for all enabled subvolumes, regardless of when the
   last snapshot was taken.
2. **Send to all connected drives** for all subvolumes configured to send, regardless
   of send interval.
3. **Report clearly** what is commencing before it starts — not just what happened after.
   The user should see something like:
   ```
   Snapshotting 7 subvolumes...
   Sending to WD-18TB (7 subvolumes, estimated ~9GB)
   Drives not connected: WD-18TB1, 2TB-backup (11 sends pending when connected)
   ```
4. **Retention still applies.** The manual run doesn't skip retention — old snapshots
   are still cleaned up per policy.
5. **Absent drives are acknowledged** but don't block anything. The user knows which
   drives are plugged in.

## How the scheduler changes

The systemd timer unit currently calls `urd backup`. It should call `urd backup --scheduled`
(or `urd backup --timer`, name TBD). This flag tells the planner to apply interval logic.

This is a one-line change in the systemd unit file but it's a deployed artifact — needs
a migration note in the changelog and `urd doctor` should warn if the timer doesn't
include the flag.

Alternatively: detect TTY. If stdout is a terminal, assume manual. If not, assume
scheduled. The `--scheduled` flag would be an explicit override for edge cases (cron
jobs that pipe output, scripts that want interval logic). This avoids changing the
timer unit but is more implicit.

## Pre-action feedback

The current backup flow is: plan silently, execute, report results. The "invoked norn"
mode should communicate before acting:

- What snapshots will be taken
- What sends will happen and to which drives
- What's blocked (disconnected drives)
- Estimated time/size if available

This isn't a confirmation prompt (the user already said "backup") — it's a status
line that appears before execution starts, so the user knows what to expect during
the potentially long operation.

## Design language principle

When the user is at the terminal typing commands, Urd speaks with authority and clarity.
Every command should:
- **Acknowledge the request** — confirm what Urd understood
- **Report progress** — show what's happening during execution
- **Summarize results** — what changed, what's still pending

The silent worker metaphor applies to set-and-forget automation. The invoked norn
guides the user toward good backup practices with informative, actionable output.

## Scope and dependencies

- Planner needs a `skip_intervals: bool` parameter (or `PlanMode::Manual` vs `Scheduled`)
- Backup command detects TTY or `--scheduled` flag
- Pre-action feedback is a new rendering path in voice.rs
- Systemd timer unit needs `--scheduled` flag (if not using TTY detection)
- Doctor check for timer unit without `--scheduled` (if flag approach)
- Backward compatibility: existing timer configs must not break
