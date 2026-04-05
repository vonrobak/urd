---
upi: "000"
date: 2026-04-05
mode: vision-filter
---

# Steve Jobs Review: Phase E Ships — Is the Foundation Ready for the Encounter?

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** Project-level vision check after v0.11.0 / Phase E completion
**Mode:** Vision Filter

## The Verdict

Phase E delivered real substance — the invisible worker is now genuinely intelligent — but
the project is approaching its most important moment, The Encounter, and it hasn't deployed
a single line of Phase E code to production yet. You've built a beautiful engine and left it
on the workbench. Deploy it. Run it. Let it surprise you. The Encounter cannot be designed
in a vacuum — it must be designed from the experience of living with this version.

## What's Insanely Great

**The emergency command gets the framing exactly right.** "Urd sees a crisis." Not "Error:
filesystem usage exceeds configured threshold." Not "WARNING: space critical." A crisis.
Something the norn recognizes and names. Then it tells you what it will do, what it will
preserve, and asks permission. This is exactly the kind of moment where the mythic voice
earns its existence — not decorating routine output, but lending gravity to a moment that
deserves gravity. The user's disk is full and their backups are stuck. They need to feel
like someone competent is in control. "Urd sees a crisis" does that. Protect this.

**The promise model is proving itself.** Six features shipped in Phase E and not one of
them required the user to learn a new concept. Skip unchanged? Invisible. Compressed
sends? Invisible. Emergency thinning? Automatic. Context-aware suggestions? Appears when
relevant. This is north star #2 working at scale: every feature reduces the attention the
user spends on backups. The architecture is delivering on the vision. That's rare.

**The discipline of "invisible worker" vs "invoked norn" held through six features.** The
automatic emergency pre-flight runs silently, under the lock, re-plans afterward. The
interactive emergency command speaks, explains, asks. Same retention logic underneath. Two
different modes of existence, properly separated. This is a design principle that survived
contact with implementation, which means it's real, not aspirational.

**921 tests.** That's not a number — that's trust. When you tell someone to type `y` to
delete 39 snapshots, you better be right about which 39. The three-layer pin protection
surviving through an emergency feature — structurally enforced, not just documented — is
the kind of engineering that earns the right to delete things.

## What's Not Good Enough

**You haven't deployed v0.10.0 or v0.11.0.** This is the single biggest concern. You've
built eleven features since the last deployed version. Eleven. The catastrophic storage
failure that motivated the emergency command happened during development. What happens if
the next one happens before you deploy the feature that prevents it? You're sitting on
your own insurance policy. Ship it.

Beyond the risk: The Encounter is next. You're about to design the first experience a new
user has with Urd. That design must be informed by the real behavior of everything you just
built — how skip-unchanged looks in the actual nightly log, whether the doctor suggestions
make sense when a real drive is actually disconnected, how the emergency pre-flight behaves
when your NVMe really does fill up. You cannot design a first impression from test output.
You need to live with this version.

**The emergency output lost something from the design doc.** The design doc's mockup says:

```
This will delete 39 snapshots from home, freeing approximately 8.2 GB.
```

The implementation says:

```
This will delete 39 snapshots.
```

I understand why — btrfs shared extents make size estimation unreliable, and the plan
acknowledged this as R4. But the user who's staring at "1.8 GB free" and being asked to
delete 39 snapshots desperately wants to know: "will this fix it?" The answer "39 snapshots"
doesn't tell them. Even a rough estimate with an "approximately" caveat is better than
silence. "Freeing approximately 4-8 GB" with a footnote about shared extents would be
honest and useful. Without it, the user is making a trust decision with incomplete
information. That's not worthy of the experience you designed.

**The design doc also showed a progress bar. The implementation doesn't have one.** When
you're deleting 39 snapshots and each one takes a few seconds, staring at a blank terminal
is anxiety-inducing. The user just said "yes, delete my history." They need feedback that
it's working. A simple counter — `Deleting... 12/39` — would transform the experience
from "did it freeze?" to "it's working."

**"The nightly timer will resume normal operation."** This line from the design doc is
missing from the implementation's result output. It's the most important line. The user
didn't run `urd emergency` because they enjoy deleting snapshots. They ran it because their
backups were broken. The answer to "did this fix it?" isn't "Freed 8.2 GB." It's "Your
backups will work again tonight."

## The Vision

Phase E is the invisible worker done right. Phase D is where Urd becomes something people
talk about.

Here's what I see. A person installs Urd on a new machine. They type `urd` and instead of
a help page, Urd says: "I see you have BTRFS volumes but no backup configuration. Want to
tell me what matters to you?" And then a conversation starts — not about retention policies
and snapshot intervals, but about photos and code and documents and "what would hurt to
lose." At the end, Urd writes a config and says: "I'll check on your data every four hours.
If something goes wrong, I'll tell you. Try `urd status` anytime you want to know if your
data is safe."

That's The Encounter. And the reason I'm confident it can work is that everything behind
it — the promise model, the awareness computation, the sentinel, the emergency recovery,
the context-aware suggestions — already exists. You've built the nervous system. Phase D
puts a face on it.

But you have to deploy first. You have to live with v0.11.0 running on your actual system,
backing up your actual data, and telling you the truth about your actual drives. The
Encounter's design will be ten times better if it's informed by a week of living with
Phase E's output.

## The Details

**"No crisis detected."** — This is good but could be warmer. When someone types
`urd emergency` they're worried. "No crisis detected. All snapshot roots are within their
free-space thresholds." is accurate but clinical. Consider: "No crisis. Your snapshot roots
have room to breathe." The norn is reassuring, not just reporting.

**"Chain parents pinned: 3"** — In the emergency output. "Chain parents" is internal
vocabulary. The user doesn't think in chain parents. They think in "what's keeping my
external backups working." Try: "3 snapshots pinned for external drives."

**Per-subvolume detail** — "home: 40 snapshots, keep 5, delete 35" — The formatting uses
engineering language. "keep 5, delete 35" is the implementer's mental model (two lists). The
user's mental model is: "How much of my history am I losing?" Something like "home: 40 →
5 snapshots (keeping your newest and 4 chain parents)" tells the human story.

**"5 unsent snapshots will be deleted."** — Correct but alarming without context. The user
doesn't know if that's bad. Add one sentence: "Your next external backup will rebuild the
chain automatically." That turns a warning into guidance.

**The `[y/N]` prompt** — Good default-to-no. But consider: this is a destructive operation
on backup data. The user should type the full word "yes", not just "y". Make the gravity
match the action. When Apple asks you to type your Apple ID to disable Find My, it's not
because they think you might accidentally type your email — it's because they want you to
pause and think. Same principle. "Type 'yes' to proceed:"

## The Ask

1. **Deploy v0.11.0 this week.** Not next session. This week. Install it, run a nightly,
   verify the output. Live with it before designing The Encounter. The deployment notes in
   status.md are clear — the path is known. Walk it.

2. **Add a progress counter to emergency deletions.** Simple `Deleting... 12/39` on stderr.
   Five minutes of work, transforms the experience from anxious to confident.

3. **Add the "your backups will work again" closing line.** After emergency results, tell
   the user what this means for their future, not just their past.

4. **Refine the emergency voice.** "Chain parents" → "snapshots pinned for external drives."
   "keep 5, delete 35" → "40 → 5 snapshots." The unsent warning needs a reassurance line.
   These are individually small but collectively they're the difference between a tool that
   informs and a tool that guides.

5. **Update the roadmap.** Phase E is complete. 016-interactive shipped (not deferred to
   Phase F as the roadmap says). Clean the map so the next session starts from truth.

6. **Then — and only then — design The Encounter.** With real production data behind you,
   with the emergency command actually available if something goes wrong, with a week of
   nightly logs showing skip-unchanged and compressed sends working. Design from experience,
   not from imagination.
