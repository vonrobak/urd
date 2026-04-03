---
upi: "010-a"
date: 2026-04-03
mode: product-review
---

# Steve Jobs Review: `local_snapshots = false` — The Boolean That Tells the Truth

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** UPI 010-a post-build — `local_snapshots = false` replaces `local_retention = "transient"` in v1 config, migration, validation, and live deployment
**Mode:** Product Review

## The Verdict

This is the right change, shipped at the right time, and it makes the config
meaningfully more honest — but the downstream experience hasn't caught up yet,
and that gap is where the real product work remains.

## What's Insanely Great

**The name is the documentation.** `local_snapshots = false` reads like English. You
don't need a man page, you don't need a glossary, you don't need to understand what
"transient retention" means. You read the config and you know: this subvolume doesn't
keep local snapshots. That's exactly how a config field should work. Compare to what it
replaced — `local_retention = "transient"` — which required you to hold a mental model
of Urd's retention engine just to understand your own config file.

When I built the original Mac, I insisted that the desktop metaphor be self-evident.
Folders look like folders. Trash looks like a trash can. A config field that says what
it means is the same principle applied to text. `local_snapshots = false` is a folder
that looks like a folder.

**Named-level opacity is now absolute.** No exceptions. No footnotes. No "well,
transient is special because..." The rule is: named levels are complete promises. If
you want to deviate, you choose custom. This is clean. Users don't need to learn which
exceptions exist — there are none.

**The validation errors guide instead of scold.** I looked at these:

```
subvolume "htpc-root": local_snapshots = false is incompatible with protection = "sheltered"
— named levels require local snapshots. Remove the protection field for custom configuration.
```

That's good. It tells you what's wrong, why, and what to do. Most config validators
stop at step one. This one finishes the thought.

**The migration is invisible.** `urd migrate` handles transient-on-custom and
transient-on-named-level and produces valid v1 that resolves identically. The user
doesn't need to understand the transformation — they run one command and their config
is modern. That's respect for the user's time.

## What's Not Good Enough

**The skip message is wrong.** Look at what the user sees during backup:

```
[SKIP]  htpc-root  no local snapshots to send
```

"No local snapshots to send" is an implementation observation, not a user-meaningful
status. The user configured `local_snapshots = false`. They *know* there are no local
snapshots. What they want to know is: did the external send happen?

And in the plan output:
```
[SKIP]  htpc-root: no local snapshots to send
```

"SKIP" is the wrong word entirely. htpc-root isn't being skipped — it's working
exactly as designed. It creates a snapshot, sends it, deletes it. The skip is only
in the plan's *send listing* because the send can't happen until there's a snapshot
to send. But the user reads "SKIP" and thinks something is wrong.

This is a vocabulary problem that 010-a didn't create but should have anticipated.
When you rename the config to speak the user's language, the runtime output must
follow.

**htpc-root shows `LOCAL: 0` and `degraded` — and nobody can tell if that's fine.**
From the `urd status` output:

```
sealed    degraded  htpc-root           0         5 (21h)  broken — full send (pin missing locally)
```

Is this a problem? For a normal subvolume, yes — zero local snapshots and a broken
chain is alarming. For htpc-root with `local_snapshots = false`, this is *exactly
expected behavior between runs*. But the status display doesn't know the difference.
The user sees "degraded" in yellow and "broken" in the thread column and has to
remember: "oh right, that one is supposed to look like that."

This is the Time Machine equivalent of showing a red exclamation mark on a volume
you deliberately excluded from backups. It creates anxiety about a situation that's
working correctly. The system knows htpc-root has `local_snapshots = false`. It
should present accordingly.

**The config example lost its intent comment.** The old v1 example had:

```toml
# ── Custom: manual config (no protection level) ─────────────────────────
# Transient retention keeps only the pinned snapshot needed for incremental
# chains. Ideal for subvolumes on space-constrained volumes (NVMe root)
# where you want external backups but can't afford local snapshot history.
```

The new one has:

```toml
# ── External-only: NVMe root is too small for local history ─────────
```

The section header is better — much better. But the explanation was reduced from
four lines to one. A new user reading this example config has no idea *why* they
might want `local_snapshots = false`. The old comment, for all its jargon, at least
described the use case: space-constrained volume, external-only backups. The new
comment names the specific case (NVMe root) but doesn't explain the general pattern.

## The Vision

Here's what great looks like for transient subvolumes — the ones where
`local_snapshots = false`:

**In status**, htpc-root should show something that communicates "external-only,
working as designed" — not "degraded." The health model should understand that zero
local snapshots is the intended state for this subvolume. Maybe the LOCAL column
shows `—` or `ext-only` instead of `0`. Maybe the THREAD column shows the external
chain state instead of reporting a local chain break that's irrelevant.

**In backup output**, htpc-root shouldn't appear in the "Skipped" section at all when
its send succeeded. If the send was skipped because the drive is away, *that's* worth
reporting. But "no local snapshots to send" is never worth reporting — it's the permanent
state of this subvolume.

**In plan output**, the skip should say what's actually happening:
`htpc-root: external-only (local snapshots disabled)` — not what's missing.

These aren't 010-a's problem to solve. They're UPI 011 territory, or a new UPI. But
010-a created the vocabulary that makes them solvable. The config now says what the
user means. The runtime needs to catch up.

## The Details

1. **`snapshot_interval` and `send_interval` still appear in the example config for
   htpc-root.** With `local_snapshots = false`, these are operational fields that
   require the user to know what intervals mean. In the encounter (6-H), these should
   be derived or defaulted — but for now, the example should at least comment on why
   they're there.

2. **The example removed `send_interval = "1d"` comments.** The old example had inline
   comments (`# Delete local after send, keep only chain parent` and
   `# Send to offsite drive only`). The new one is bare. For an example config that
   teaches, bare is worse.

3. **`SkipCategory::Other` is the catch-all for "no local snapshots to send."** This skip
   reason deserves its own category — it's a permanent, designed-in state, not an
   incidental skip. Grouping it with UUID mismatches and "snapshot already exists" is
   a lie about its importance.

## The Ask

1. **UPI 011 or a new UPI: Make the runtime experience match the config vocabulary.**
   Status, plan, and backup output should understand `local_snapshots = false` as a
   first-class state, not an anomaly. This is the single most impactful change — it
   closes the gap between the config telling the truth and the runtime telling the truth.

2. **Add a `SkipCategory::ExternalOnly` variant** — quick win, separates "working as
   designed" from "something unexpected." This is a 10-minute change that immediately
   improves backup output.

3. **Restore intent comments to the v1 example config.** Two lines explaining the use
   case. Not the old jargon — the new vocabulary. "Disable local snapshot history when
   the source volume can't afford the space. Backups go directly to external drives."

4. **During the test session, watch htpc-root.** Every time you see "degraded" or
   "broken" in status and have to mentally filter it — that's a product bug. Write
   down how it makes you feel. That feeling is the spec for the fix.
