---
upi: "000"
date: 2026-04-05
mode: vision-filter
---

# Steve Jobs Review: Fix the Lie First

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** Brainstorm `docs/95-ideas/2026-04-05-v011-production-fixes.md` — roadmap slotting
**Mode:** Vision Filter

## The Verdict

The brainstorm correctly identified the root cause of the critical bug and produced solid
ideas, but it's trying to solve seven problems at once. Three of these are worth doing before
The Encounter. The rest are either already solved, not real problems, or should wait.

## What's Insanely Great

**The root cause analysis for the transient accumulation bug is exceptional.** "Transient
retention protects snapshots for absent drives indefinitely." That's not a surface-level bug
report — that's understanding a system well enough to name the exact mechanism that will
eventually destroy someone's NVMe. The brainstorm didn't just say "snapshots accumulate." It
traced the path: absent drives → unsent protection → unbounded accumulation → catastrophic
failure. That's the kind of analysis that produces fixes that actually work, not patches that
defer the same failure to a Tuesday at 3am.

**Idea 1a is the insight.** Transient cleanup ignoring absent drives is not just a fix — it's
a principle. It says: "If a drive can't receive a send right now, holding hostage a snapshot
on a space-constrained filesystem doesn't protect anything. It endangers everything." That
reframes the problem from "how do we keep the minimum snapshots" to "what is snapshot
retention actually protecting, and does that protection still apply when the drive is gone?"

**The discovery that `urd get` is already pipe-safe is exactly the right kind of finding.**
The brainstorm said "before designing a fix, verify the bug exists." That restraint — checking
before building — is what separates engineering from busywork. If it's confirmed, cross it off
and move on. Don't build a fix for a bug that doesn't exist.

## What's Not Good Enough

**Idea 1g (combine 1a + 1c + 1d) is overthinking it.** Three layers sounds like defense-in-depth.
It's actually three changes for one bug. Let me break this down:

- **1a (ignore absent drives in transient cleanup):** This is the fix. It addresses the root
  cause directly. With this in place, htpc-root snapshots get cleaned up after successful sends
  to mounted drives. The count stays at 1 (the current pin parent). Done.

- **1c (rename `local_snapshots = false` to something honest):** I disagree. `local_snapshots = false`
  is the right name. The user's intent is "I don't want local snapshots." Urd's job is to
  honor that intent as closely as the physics allow. One transient snapshot existing during
  the send pipeline is an implementation detail the user shouldn't need to know about — like
  how the iPod's click wheel generates more events when you spin faster. You don't explain
  the engineering. You make the thing do what the user expects. With 1a fixed, the user will
  see 1 snapshot briefly during the nightly, then 0 until the next run. That IS `false` in
  any meaningful sense. Don't rename it. Fix the behavior.

- **1d (hard cap):** A safety net for a bug you've already fixed is a safety net for your own
  lack of confidence in the fix. If 1a works — and the analysis says it will — the cap never
  triggers. If you need the cap, 1a didn't work, and you should fix 1a, not add a cap. I'll
  concede one exception: the emergency pre-flight (UPI 016) already exists as a space-based
  safety net. That's the right layer to catch edge cases, and it's already built. Don't add
  a second safety net.

**Idea 5d (rotation interval) is too much mechanism for a solved problem.** The brainstorm
correctly identifies that htpc-root shouldn't be assessed against drives it doesn't need.
But rotation intervals are a new config concept, a new user-facing field, a new thing to
explain in The Encounter. For what? So that an offsite drive can be absent for 29 days
without nagging but nags on day 31?

Here's what's actually happening: htpc-root has no `drives` config, so it sends to everything.
The user doesn't want to send htpc-root to everything. They want it on WD-18TB. **Idea 5a —
add `drives = ["WD-18TB"]` to htpc-root's config — is the right answer.** One line. Zero code.
Zero new concepts. The user explicitly declares their intent, and the system honors it. That's
the entire design philosophy of v1 config: self-describing, explicit, no inheritance magic.

If someday a real user says "I want Urd to know my offsite drive comes back monthly and
only nag me if I'm late," then design rotation intervals. Not before. Don't build concepts
for one config line.

**Idea 5b (role-based health weighting) has a seed of something good, but for later.** The
insight that "test" drives shouldn't affect health computation is real. But today, there's
one test drive (2TB-backup) and the fix is to scope htpc-root's drives list. Post-v1.0,
when Urd has real users with real drive topologies, role-aware health might earn its
complexity. Park it.

**The brainstorm treats the plan vs dry-run divergence as bigger than it is.** Seven ideas
across three categories for what is, at its core, a display discrepancy. Idea 6d (unify them)
is clean in theory but blurs two distinct user intents: "show me what the system would do"
(plan) vs "rehearse this specific action" (dry-run). These are different questions. The Mac
had both "About This Mac" and "System Profiler" — same data, different depth, different intent.
Don't merge them. Idea 6c (explain the difference) is the right call. One sentence in the
dry-run footer. Move on.

## The Vision

Here's how I'd slot these into the roadmap. You have three categories:

**Do now (before The Encounter):**
1. Fix the transient accumulation bug (Idea 1a). This is data safety. Ship it.
2. Add `drives = ["WD-18TB"]` to htpc-root config (Idea 5a). One config line. Eliminates the
   permanent degraded state and, critically, also fixes the transient accumulation by reducing
   htpc-root's drive scope to only mounted drives. These two changes together mean htpc-root
   goes from "2 snapshots accumulating because of absent drives, permanently degraded" to
   "1 snapshot max, healthy." That's the whole story.
3. Fix the sentinel 0-chains warning (Idea 3b — guard on chain delta). The sentinel is the
   always-on nervous system. Its warnings must mean something. A spurious warning in the log
   every time a drive disconnects trains the user to ignore the logs. Five lines of code.
4. Change "send disabled" to "local only" (Idea 4b). One string. Matches the category. Done.

**Verify and close:**
5. Confirm `urd get` is pipe-safe (Idea 2a). If it is, update the test report and close. If
   it isn't, fix it — but the code analysis says it's fine.

**Park (post-Encounter or never):**
- 1c (rename config field) — Unnecessary if 1a works. The behavior IS the contract.
- 1d (transient cap) — Emergency pre-flight is the safety net. Don't add another.
- 1e (space-triggered transient cleanup) — Already exists as emergency pre-flight.
- 1f (snapshot-on-demand for transient) — Breaks planner purity for marginal benefit.
- 5b/5c/5d/5e/5f — Overengineered solutions to a one-line config fix. Park all of them.
- 6a/6b/6d (plan/dry-run changes) — 6c (one-sentence footer) is sufficient.
- 7a-7e (retention preview sizes) — Real issue but doesn't affect data safety or user
  attention. The retention preview is a power-user diagnostic tool. Fix it when you're
  polishing, not when you're fixing safety.
- 8a/8b/8c (lock file) — 8c is correct: do nothing. The lock file works. It's in a
  directory nobody looks at. This is the definition of an aesthetic concern that doesn't
  serve either north star.
- 9a/9b/9c (uncomfortable ideas) — Good thinking exercises, physically impossible.
  The tension they reveal is real (btrfs requires local snapshots for sends), and 1a
  is the practical resolution.

**Updated roadmap would look like:**

```
v0.11.1 patch: transient fix (1a) + config fix (5a) + sentinel guard (3b) + text fix (4b)
    → validate with 2-3 nightlies
    → then: Phase D (Progressive Disclosure + The Encounter)
```

One patch release. Four changes. Two that matter for data safety, two that matter for trust.
Then The Encounter, built on a foundation that tells the truth.

## The Details

**On Idea 1c specifically:** The brainstorm suggests `local_retention = "pipeline"` or
`local_retention = "minimal"`. Both of these are engineer words. A user who cares about their
root filesystem filling up doesn't think in "pipeline retention." They think in "don't store
snapshots on my small drive." `local_snapshots = false` is the user's language. Don't
translate it into yours.

**On Idea 5f (degraded vs reduced):** I see the appeal of splitting health states to
reduce alarm fatigue. But the vocabulary is frozen. Adding "reduced" is a new concept the
user must learn, and the distinction between "degraded" and "reduced" is subtle enough to
confuse rather than clarify. The real fix is to not report degradation when there's no
degradation. Scope the drives (5a), and the false alarm goes away entirely.

**On the brainstorm's uncomfortable ideas (9a-9c):** I appreciate these. Not because they're
buildable — they aren't — but because they name the fundamental tension: BTRFS requires a
local snapshot to send, but the user doesn't want local snapshots. The right answer is to
make the local snapshot's existence as brief and invisible as possible. Idea 1a does that.
The user's experience approaches zero-local even though the physics require one-transient.
That gap between user experience and engineering reality is exactly where product design lives.

## The Ask

1. **Ship Idea 1a.** Fix `expand_protected_snapshots()` to exclude absent drives from the
   unsent protection set for transient subvolumes. This is the critical data safety fix. Design
   it, build it, verify it in 2-3 nightlies.

2. **Edit htpc-root config: `drives = ["WD-18TB"]`.** One line. Stops the false degraded state.
   Reduces htpc-root's drive scope so the transient fix (1a) works cleanly. Do this today —
   it's a config change, not a code change.

3. **Ship the sentinel guard (3b) and text fix (4b) in the same patch.** Small, obvious,
   high-trust-impact changes. Bundle them with 1a as v0.11.1.

4. **Verify `urd get` piping and close (2a).** Ten-second test. If it works, update the
   test report.

5. **Add the dry-run footer (6c):** "Retention deletions not shown. Run `urd plan` for the
   full picture." One line in voice.rs. Ship with v0.11.1 if it's easy, otherwise park.

6. **Then design The Encounter.** Not before. The foundation must be honest first.
