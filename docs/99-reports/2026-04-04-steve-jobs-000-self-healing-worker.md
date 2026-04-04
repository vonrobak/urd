---
upi: "000"
date: 2026-04-04
mode: design-critique
---

# Steve Jobs Review: The Invisible Worker Is Lying

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-04
**Scope:** v0.10.0 live testing findings — system behavior under real conditions
**Mode:** Design critique with design sketches for remediation

## The Verdict

Urd's invisible worker is doing its job 90% of the time, and *lying about the other
10%.* A backup system that says "success" while data ages toward unprotected isn't
just a bug — it's a betrayal of the user's trust. This is the single most important
thing to fix before anything else ships.

## What's Insanely Great

Let me be clear about what's working, because this is a solid foundation:

**The incremental send pipeline is beautiful.** Five subvolumes sent in 31 seconds,
four of them with 123 bytes of delta. That's a system that knows the difference between
"something changed" and "nothing changed" — and handles both with zero drama. 843MB of
container data in 26 seconds alongside four no-op sends. That's the invisible worker
doing exactly what it should.

**The sentinel is *actually* invisible.** 3.5MB of memory. 17 seconds of CPU over
43 hours. That daemon is so light the system doesn't know it exists. That's what
background software should be. The circuit breaker, the 300-second tick, the
notification deferral — all evidence of thoughtful restraint.

**The space-recovery optimization is inspired.** The executor doesn't delete 26
snapshots when one will do. It checks: "Is there enough space? Then stop deleting."
That's not an optimization — that's a philosophical stance. Don't destroy data you
don't have to. I love this.

**The structured JSON output is clean and complete.** Every field meaningful, no
noise, machine-readable by default in non-TTY. The presentation layer switch between
interactive and daemon modes is exactly right. You've built a tool that can be consumed
by both humans and machines without compromising either.

## What's Not Good Enough

### 1. The Worker Lies About Its Failures

Here is what happened at 04:00 this morning. Urd ran. htpc-root had zero local
snapshots (the user deleted them because they were eating the NVMe alive — which is
*why* transient mode exists). The planner said "no local snapshots to send." The
executor said "success: true." The heartbeat said "PROTECTED." Prometheus said
`backup_success = 1`.

Every surface said everything was fine. Everything was not fine.

htpc-root is AT RISK right now. It will drift to UNPROTECTED. And if the nightly runs
again tonight? Same thing. "Success." Same lie. Tomorrow? Same. This continues
*forever* until someone manually intervenes with a `--force-full` flag that nothing
in the system suggests they should use.

This is the worst kind of software failure. Not a crash — crashes are honest. This
is a system that looks healthy while data protection degrades behind a green dashboard.

When we shipped the first iPod, we had a battery indicator. It didn't show "full"
when the battery was dying. That would have been unforgivable. This is the same thing.

### 2. The Full-Send Gate Creates a Deadlock

The full-send gate was a good idea when it was built. "Don't automatically send 40GB
to a drive that might be cloned or swapped." Correct instinct. But the gate doesn't
know *why* the chain broke. It treats "user deleted snapshots to free disk space" the
same as "someone plugged in a different hard drive."

WD-18TB's identity token is *verified*. The system knows exactly who that drive is.
And yet it refuses to send to it. That's like refusing to let someone into their own
house because their key looks different — while their fingerprint is confirmed.

The gate should *use the information it already has.* If the drive token is verified,
the chain break is from snapshot deletion, not hardware swap. Proceed.

### 3. The Doctor Gives Bad Advice

When you ask your doctor "what's wrong with me?" and they say "you're a bit tired,
try sleeping more" — but actually you have a broken bone — that's malpractice.

`urd doctor` says: "waning — last backup 13 hours ago. Run `urd backup` to refresh."

If you run `urd backup`, here's what happens: a new snapshot is created, the send is
gated, nothing reaches the external drive, the snapshot sits on the NVMe eating space,
and htpc-root is *worse off than before you asked the doctor.* The doctor's own
prescription makes the patient sicker.

The doctor *knows* the chain is broken (the verify section shows it). But the
suggestion doesn't connect these facts.

### 4. The Sentinel Doesn't Know Its Config Changed

The sentinel was started with an old config. The user migrated to v1, added drive
scoping. The sentinel still thinks htpc-home sends to 2TB-backup. It doesn't. This
produces false degradation warnings that would flow through to Spindle.

A daemon that doesn't reload its config is a daemon that drifts from reality. In a
backup system, drifting from reality is the one thing you cannot do.

## The Vision

Here's what I want to see. Three design groups that turn these findings into coherent
solutions. I'm sketching shapes, not blueprints — the design workflow fills in the
rest.

### Design Group A: "The Honest Worker" (F1 + F2 + F3)

**The principle:** The invisible worker never lies. If it can't complete its job, it
says so clearly — and if it can fix the problem itself, it does.

**Design sketch ��� Token-aware chain healing:**

The full-send gate should have three modes, not two:

| Drive token state | Chain break behavior in auto mode |
|---|---|
| Verified | **Proceed with full send.** Identity confirmed. The break is from snapshot lifecycle, not hardware swap. |
| Missing/Mismatched | **Gate the send.** This is what the gate was built for. |
| Unknown (drive absent) | **Skip.** Can't verify, can't send. |

This is not "weakening" the gate. This is making it *smarter.* The gate still catches
the thing it was designed to catch (hardware swaps). It stops catching the thing it
shouldn't (routine space management on transient subvolumes).

**Design sketch — Honest run results:**

A run where a subvolume's send was needed but couldn't happen is not "success." It's
not "failure" either. It's a *new state*: **degrading.**

I don't care what you call it internally. But the external signals need to distinguish:

| Actual situation | Run result | Heartbeat status | Metric |
|---|---|---|---|
| Send completed | success | PROTECTED | 1 |
| Send intentionally deferred (interval) | success | PROTECTED | 1 |
| Send needed but gated/blocked | **degrading** | **AT RISK** | **3** (new value) |
| Send failed | failure | depends on state | 0 |

The heartbeat should reflect the *post-run state*, not the *run execution state.*
"Did the run succeed?" and "Is the data safe?" are different questions. The heartbeat
answers the second one.

**Design sketch — Unified deferred reporting:**

Every subvolume that needs a send but doesn't get one — for any reason — must produce
a deferred entry with:
1. What: "htpc-root send to WD-18TB not completed"
2. Why: "no local snapshots existed at plan time" / "chain-break full send gated"
3. How to fix: "Run `urd backup --force-full --subvolume htpc-root`"

No silent skips. No empty arrays where deferred entries should be. The planner's
"no local snapshots to send" skip for a send-enabled subvolume is not the same as
"multimedia doesn't send because it's local-only." One is a problem. The other is
configuration. Report them differently.

### Design Group B: "The Doctor Knows" (F6 + related)

**The principle:** Every suggestion Urd makes should be one the user can follow
successfully. If following the suggestion would fail, the suggestion is wrong.

**Design sketch — Context-aware suggestions:**

The doctor should compose its suggestions from the full diagnostic picture, not
from a lookup table:

```
htpc-root:
  waning — last external send to WD-18TB was 43 hours ago.
  Thread broken — WD-18TB will need a full send (~33GB).
  → Run `urd backup --force-full --subvolume htpc-root`
```

The suggestion system needs to know:
- Is the chain broken? → append `--force-full`
- Is a specific drive the problem? → append `--subvolume`
- Is the drive mounted? → don't suggest sending to an absent drive

This same logic applies everywhere suggestions appear: `urd doctor`, the bare `urd`
default command, the status next-action hints. One function that computes the right
suggestion given the current state. Don't duplicate this logic — compute it once,
render it everywhere.

**Extend to the default command:** When the user types bare `urd` and sees "htpc-root
waning," that line should carry the same intelligence. Not just "waning" — "waning,
and here's the one command that fixes it." Progressive disclosure: the one-liner
mentions the problem, `urd status` shows the detail, `urd doctor` explains the full
picture and the fix.

### Design Group C: "The Living Daemon" (F4 + F7 + sentinel polish)

**The principle:** The sentinel is the source of truth for system state. It must
never present stale or incorrect information.

**Design sketch — Config reload on change:**

The sentinel should watch the config file and reload when it changes. Not on every
tick — that's wasteful. Watch the file's mtime, or use inotify if you want to be
elegant. When the config changes:

1. Reload
2. Re-assess immediately (don't wait for next tick)
3. Log: "Config reloaded — reassessing"

If the config is invalid after a change, log the error but *keep running with the
old config.* Don't crash. Don't silently use a broken config. Log it and wait for
the user to fix it.

This isn't just about drive scoping. Every time a user edits their config, the
sentinel should adapt. Especially now that `urd migrate` exists — a user migrates
their config and the sentinel should just... know.

**Design sketch — Sentinel anomaly messages:**

"All 0 chains broke on WD-18TB simultaneously" — this message is saying "I detected
an anomaly that doesn't exist." If zero chains broke, there is no anomaly. Fix the
guard condition: don't fire the anomaly notification when the count is zero. When
the count is positive, say "All N chains broke on WD-18TB simultaneously — possible
drive swap."

## The Details

**The plan vs executor deletion count.** `urd plan` says 26 deletions. `urd backup
--dry-run` says 5. Both are correct. Neither explains the discrepancy. The plan should
say: "26 eligible for deletion (executor will stop early if space permits)." One
sentence. No confusion. This is a Phase D progressive-disclosure concern — don't
block the urgent fixes on it, but write it down.

**"no local snapshots to send" as a skip category.** This is currently `category:
"other"` — a junk drawer. For a transient subvolume, this is a *critical state
signal*, not an "other." It should have its own category:
`no_snapshots_available` or `transient_empty`. The category system exists so
consumers (Spindle, monitoring) can distinguish actionable from informational skips.
"Other" tells them nothing.

**The retention summary for htpc-root says "none (transient)."** That's technically
true but misses the point. htpc-root has external retention (daily=30, weekly=26).
The user doesn't keep local snapshots, but they *do* keep external ones. The summary
should say something like "external only: 30d / 26w" — reflecting what protection
actually exists.

**The heartbeat schema says htpc-root `backup_success: true`.** The word "backup"
implies data was backed up. It wasn't. If the heartbeat means "this subvolume's
operations completed without errors," call it `operations_success` or
`run_success`. Or better: add `send_completed: bool` alongside it so consumers
can alert on the thing that actually matters.

**Sentinel `visual_state` counts.** The sentinel reports `safety_counts: {ok: 8,
aging: 1, gap: 0}` and `health_counts: {healthy: 6, degraded: 3, blocked: 0}`.
But two of those three "degraded" are phantom degradation from the stale config.
When the config reload fix lands, verify these counts correct themselves. These
numbers drive Spindle's tray icon — they must be accurate.

## The Ask

**Group these into three design specs and build them in this order:**

1. **Design Group A: "The Honest Worker"** — Token-aware chain healing + honest
   run results + unified deferred reporting. This is the most urgent because it
   affects data safety. One design spec housing F1, F2, and F3. Build this first.
   Touches: `executor.rs` (gate logic, deferred reporting), `plan.rs` (skip
   category), `commands/backup.rs` (run result computation), `heartbeat.rs`,
   metrics.

2. **Design Group B: "The Doctor Knows"** — Context-aware suggestions across
   doctor, default command, and status. One design spec housing F6 and the
   suggestion system refinement. Can follow Group A quickly since it's mostly
   presentation logic. Touches: `voice.rs`, `output.rs`, `commands/doctor.rs`,
   the suggestion function.

3. **Design Group C: "The Living Daemon"** — Sentinel config reload + anomaly
   message fix. One design spec housing F4 and F7. This is lower urgency since
   the immediate workaround is restarting the sentinel. But it's architecturally
   important for Spindle. Touches: `sentinel_runner.rs`, `sentinel.rs`.

**Don't block on each other.** Groups A and B share no modules with Group C. Build
A first. B can follow immediately. C can be parallel or sequential.

**Quick win while designs are in progress:** Restart the sentinel now.
`systemctl --user restart urd-sentinel` fixes F4 immediately. And run
`urd backup --force-full` to heal the broken chains. Your data shouldn't be AT RISK
while you're designing the fix.

**One more thing.** The testing session itself was excellent work. Most teams ship
software and hope it works. Running a systematic live test, observing the nightly,
tracing the journal, finding the deadlock cycle — that's how you build a tool people
trust with their data. The findings are a gift. Every one of them makes the product
better. The fact that the invisible worker was lying is uncomfortable, but now you
know. And now you can fix it.
