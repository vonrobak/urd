---
upi: "000"
date: 2026-04-05
mode: product-review
---

# Steve Jobs Review: The First Nightly Tells the Truth

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** v0.11.0 post-deployment product review — first nightly run results, full command surface
**Mode:** Product Review

## The Verdict

v0.11.0 deployed and the first nightly ran. The invisible worker is genuinely intelligent
now — compressed sends, skip-unchanged, space-aware deletion halting, chain-break recovery
with drive verification — and it all worked. But Urd is lying to the user about htpc-root,
and that lie is the exact shape of the catastrophic failure that almost destroyed this
project. Fix it before celebrating.

## What's Insanely Great

**The nightly was smart.** Run #29 did exactly what Phase E promised. It detected that
htpc-home and htpc-root had broken chains, verified the drive identity, and proceeded with
compressed full sends — 27GB and 22GB — without human intervention. Five subvolumes were
skipped because nothing changed. Retention deletions stopped early because there was already
enough free space. The run took 9 minutes and 31 seconds, and every one of those seconds was
spent on work that mattered. Compare that to run #28 the previous night: 31 seconds, because
there was nothing to do. The invisible worker now has judgment, not just a schedule.

**Skip-unchanged is the feature that doesn't exist.** That's the highest compliment I can
give a backup feature. subvol2-pics hasn't changed in 7 hours — no snapshot created, no
send attempted. subvol5-music hasn't changed in a day — same. The user didn't configure
this. They didn't ask for it. They'll never see it happen. They'll just notice that their
backup runs are faster and their drives have more free space. This is north star #2 in its
purest form: reduced attention to zero.

**The context-aware suggestions are right.** Three subvolumes are degraded. Status output
doesn't just say "degraded" — it says "Consider connecting WD-18TB1." That's the answer
to the obvious follow-up question, delivered before the user asks it. This is what "guide
through affordances, not error messages" looks like in practice.

**Drive token verification works and it's invisible.** Run #29 log: "Chain-break full send
for htpc-home to WD-18TB: proceeding (drive identity verified)." The system confirmed it
was talking to the right drive before sending 27GB. The cloned-drive catastrophe from v0.8
testing (F2.3 — 1.3TB of full sends planned to the wrong drive) would now be caught. This
is the kind of safety that the user should never have to think about. It just works.

**The space-aware executor is elegant.** "Free space on WD-18TB is now 4.2TB (>= 500.0GB),
stopping further deletions." Twenty planned deletions skipped because the first one freed
enough space. This is not an optimization — it's respect for the user's data. Don't delete
history you don't need to delete. Don't thin archives because a schedule says so when the
drive has four terabytes free. The fail-closed deletion philosophy is now *smart* about
when closed is the right answer.

## What's Not Good Enough

**htpc-root is lying.** The config says `local_snapshots = false`. The user set this because
local htpc-root snapshots on the 118GB NVMe caused a catastrophic storage failure. The
status output says `"external_only": true` and `"retention_summary": "none (transient)"`.
Everything in the UI tells the user: "I'm not keeping local copies of your root filesystem."

There are two local copies of the root filesystem sitting on the NVMe right now.

The nightly log proves it: "Creating snapshot: / -> ~/.snapshots/htpc-root/20260405-0401-htpc-root".
Two snapshots. On a drive with 26GB free. Each root filesystem snapshot contains everything
under `/` — system packages, caches, logs, container images. The CoW deduplication between
them might keep the exclusive data small today, but a chain break, a failed cleanup, one
more snapshot accumulating — and you're back in the catastrophic failure that motivated the
entire emergency feature.

This isn't a bug report. This is a product integrity issue. When a user sets
`local_snapshots = false`, they are making a statement: "I understand the risk and I don't
want local copies." When the system ignores that statement, it breaks the most fundamental
contract a backup tool has: do what I told you to do with my data.

I understand the engineering reason. The send pipeline needs a local snapshot to exist before
it can send. The pin parent stays for chain continuity. So "false" doesn't mean "zero" — it
means "as few as the pipeline requires." But that's the engineer's truth, not the user's
truth. The user's truth is: I said false and there are two snapshots on my NVMe.

**The `urd get` experience is broken for piping.** When someone types
`urd get /etc/hostname --at yesterday`, they get:

```
{"subvolume":"htpc-root","snapshot":"20260404-0400-htpc-root",...}fedora-htpc
```

JSON metadata concatenated with file content on stdout. If you pipe that to a file, you get
a corrupt restore. The most critical user journey in any backup tool — "get my file back" —
has a footgun in its default mode.

I know there's a `-o` flag. I know the interactive mode probably renders this better with
metadata on stderr. But the daemon mode path — which is what scripts and pipes use — mixes
data streams. This is the kind of thing that works in a demo and fails at 2am when someone
is desperately trying to recover a config file they just destroyed.

**The post-delete sync isn't in sudoers.** Every nightly run produces this warning:
"btrfs subvolume sync failed: sudo: a terminal is required to read the password." This is
Phase E feature 013 — post-delete sync for accurate space reporting — deployed but broken
in the autonomous context. The invisible worker is telling you, every night, that part of
its new intelligence doesn't work. The fix is one line in the sudoers file, but the fact
that it shipped without that line means the deployment checklist missed it.

**The sentinel logs spurious warnings.** "Drive anomaly: all 0 chains broke on 2TB-backup
simultaneously." Zero chains broke. That's not an anomaly — that's nothing. When I see a
warning in a log, I want it to mean something. A warning that fires when nothing happened
teaches the user to ignore warnings. That's the most dangerous habit a backup tool can
create.

## The Vision

Last review, I said deploy before designing The Encounter. You deployed. The first nightly
ran. And it told you something you didn't know: the contract around `local_snapshots = false`
isn't what you think it is.

This is exactly why I said to live with it first. Production tells truths that tests can't.

Here's what I see now. The invisible worker is ready. Phase E's features are working — not
just technically, but *as a product*. Skip-unchanged saves real work. Compressed sends move
real data faster. Space-aware deletion respects real drives. Context-aware suggestions answer
real questions. The intelligence is there.

But The Encounter — the moment a new user meets Urd — requires something that isn't there
yet: *the ability to explain what "transient" means in human terms*. If I'm a new user
running `urd status` and I see htpc-root with `retention_summary: "none (transient)"` next
to `local_snapshot_count: 2`, I don't know what to make of that. None? But there are two.
Transient? What does that mean? When do they go away?

The Encounter conversation needs to answer these questions before they're asked. When the
Fate Conversation discovers that someone's root filesystem lives on a small NVMe, Urd should
say: "I'll keep temporary copies just long enough to send them to your external drive, then
clean them up immediately. You'll never accumulate root snapshots on this drive." And then
it must actually do that.

The promise model works because it says "PROTECTED" and the user believes it. The moment
the user discovers that "false" doesn't mean false, or "none" doesn't mean none, the
promise model loses its power. Trust is the product. Protect it.

## The Details

**"send disabled" as a skip reason for local-only subvolumes.** Better than v0.8's
`[OFF] Disabled`, but still not right. subvol4-multimedia and subvol6-tmp are actively
snapshotted — they have 6 and 13 local snapshots respectively. "Send disabled" sounds
like something is wrong. "Local only — not sent to external drives" tells the truth.
The category `local_only` is correct; the reason text `"send disabled"` doesn't match it.

**The plan vs dry-run divergence persists from v0.8.** `urd plan` shows 25 deletions.
`urd backup --dry-run` shows 6. Same feature, two answers. The user who runs both will
be confused. Either they should show the same operations, or the difference should be
explained. A user typing `urd backup --dry-run` is asking "what exactly will happen when
I type this without --dry-run?" and the answer should be complete.

**Retention preview shows pre-dedup sizes that are misleading.** "subvol3-opptak: 92
snapshots, estimated 315TB total." The user's drive is 18TB. The number is technically
correct — it's the sum of full snapshot sizes before CoW dedup. But it's worse than useless;
it's actively frightening. Either show delta-based estimates (the actual disk impact) or
don't show estimates at all. A number that's off by 20x erodes trust in every other number
Urd shows.

**The lock file persists after runs complete.** `{"pid":2115074,"started":"2026-04-05T04:01:09",
"trigger":"auto"}` — from a process that finished hours ago. The stale lock doesn't block
anything (the PID check handles that), but it's untidy. A tool that leaves artifacts after
it finishes doesn't feel like a tool that cleans up after itself. And for a tool whose
entire job is managing filesystem state, that matters.

**htpc-root "degraded" reasons are noisy.** "WD-18TB1 away for 13 days, 2TB-backup away
for 13 days." htpc-root sends to all drives because it has no explicit `drives` config.
The 2TB-backup is a test drive that may never return. WD-18TB1 is offsite intentionally.
The user knows this. The degradation is technically correct but practically meaningless —
it's crying wolf every time the user checks status. This connects to the assess() scoping
issue from v0.8 testing: htpc-root shouldn't be assessed against drives it was never
designed to need.

## The Ask

1. **Fix the `local_snapshots = false` contract.** This is not negotiable. When the user
   says false, the system must honor that intent. The engineering solution — "create, send,
   delete old pin parent, keep exactly one" — exists and is straightforward. Or redefine the
   contract honestly: rename it to `local_retention = "minimal"` and document that one
   snapshot exists for chain continuity. But don't call it "false" and keep two. That's a
   lie, and lies in backup tools have consequences measured in lost data.

2. **Fix `urd get` stdout in daemon mode.** Metadata to stderr, content to stdout. This is
   the restore path — the most important user journey. It must be pipe-safe by default.

3. **Add `btrfs subvolume sync` to the sudoers deployment instructions.** One line. The
   feature is already deployed; it just can't run.

4. **Suppress the "0 chains broke" sentinel warning.** If the chain count is zero, there
   is no anomaly. Don't log one.

5. **Change "send disabled" to "local only — not sent to external drives."** The category
   is already right; match the reason text to it.

6. **Scope htpc-root's health assessment.** Either add explicit `drives` config to htpc-root
   so it's only assessed against drives it actually sends to, or make the assess() logic
   aware that some drives are optional for some subvolumes. The current "degraded" state
   is permanent and unfixable without connecting drives the user may not want to connect.

7. **Then start designing The Encounter.** With the contract fixed, the restore path clean,
   and the nightly running honestly. The Encounter is Urd's first impression. It must be
   built on a foundation that tells the truth.
