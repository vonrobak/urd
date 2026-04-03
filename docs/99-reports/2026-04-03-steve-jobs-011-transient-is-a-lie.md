---
upi: "011"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Brainstorm 011 — Transient Space Safety

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Brainstorm 011: 16 ideas for preventing transient subvolume space exhaustion
**Mode:** Vision Filter

## The Verdict

"Transient" is a lie Urd tells itself, and the user has nearly lost their system five
times because of it. This isn't a brainstorm about improvements — this is a brainstorm
about fixing a broken promise. Some of these ideas fix the promise. Most of them are
band-aids, safety nets, or interesting distractions. I'm going to be direct about which
is which.

## What's Insanely Great

The problem statement is the best part of this brainstorm. "Transient retention is a
deletion policy, not a creation policy." That sentence is the diagnosis. Everything flows
from it.

The brainstorm also correctly identifies pin file corruption as secondary damage — not an
afterthought but a direct consequence that compounds the original failure. That kind of
chain-of-failure thinking is exactly how you build reliable systems.

And I'll say this: the fact that the homelab's Discord alert caught this before disaster
is the system working as designed. The invisible worker failed, but the monitoring layer
saved the data. That's defense in depth. It shouldn't have been needed, but it worked.

## Priority Scores and Grouping

Let me score every idea, then group the ones worth designing.

### Tier 1: Fix the broken promise (build these)

| Idea | Score | Rationale |
|------|-------|-----------|
| **#6 — Drive-availability preflight** | **92** | The cleanest expression of the correct behavior. If you can't send, don't snapshot. Period. This is not a "check" — this IS what transient means. |
| **#14 — Pin file self-healing** | **88** | This isn't optional. Pin files that reference ghosts are a data integrity time bomb. The next send after manual intervention must be correct, not lucky. |
| **#4 — Transient cap of 1** | **85** | Defense in depth. Even if #6 has a bug, even if drive detection lies, you can never have more than one snapshot. Belt AND suspenders. |

### Tier 2: Right direction, needs design scrutiny

| Idea | Score | Rationale |
|------|-------|-----------|
| **#1 — Transient-aware creation gate** | **70** | Correct instinct, but #6 does the same thing more cleanly at the subvolume level rather than buried inside a helper function. Subsumed by #6. |
| **#16 — Aggressive transient retention** | **65** | Smart safety net — "protect only the newest unsent" caps accumulation even when creation slips through. But this is fixing the retention side when the creation side is the real problem. |
| **#5 — Atomic send-and-delete** | **60** | Appealing conceptually, but "atomic" is a dangerous word in systems that shell out to `sudo btrfs`. The executor already has transient cleanup. Making the existing path work correctly is better than a new execution mode. |
| **#10 — Sentinel drive-gated backup** | **55** | This is the long-term right answer for transient subvolumes. But it requires Sentinel active mode, which is explicitly in the horizon section, not the active arc. Don't build the penthouse before the foundation is solid. |

### Tier 3: Interesting but wrong priority

| Idea | Score | Rationale |
|------|-------|-----------|
| **#2 — External-only mode** | **45** | This is what transient *should have been* from the start. But renaming the concept doesn't fix the bug — #6 does. Consider this for UPI 010 config schema redesign, not for the emergency fix. |
| **#11 — Filesystem pressure notifications** | **40** | The Discord alert already saved the day. Making it native to Urd is nice but doesn't prevent the problem. This is a "reduce attention" feature, not a "make data safer" feature, and right now data safety is on fire. |
| **#12 — `local_snapshots = false`** | **40** | Same as #2 — correct concept, wrong timing. This belongs in the config v1 design conversation, not in the emergency fix. |
| **#9 — Volume-aware scheduling** | **35** | Architecturally fascinating, practically a research project. The 118GB NVMe problem has a simpler solution: don't create snapshots you can't send. Volume awareness is a v2.0 concern. |
| **#3 — Snapshot budget** | **30** | Adds config surface area to solve a problem that #6 + #4 solve without any new config. More knobs is not the answer here. |
| **#8 — Auto-only transient** | **25** | This punishes the user for running `urd backup` manually. "You can't do that" is never the right answer. The right answer is making manual runs safe. |
| **#13 — `urd reclaim`** | **20** | An emergency tool for a problem that shouldn't happen. If #6 and #4 work, this is never needed. If they don't work, you have bigger problems than a CLI command can solve. |
| **#7 — Pin file recovery** | **15** | Subsumed entirely by #14, which does everything this does and more. |
| **#15 — Snapshot in tmpfs** | **5** | I appreciate the ambition, but this is solving the wrong problem with exotic infrastructure. BTRFS doesn't work this way. Park it. |

## Design Groups

Here's how these should be grouped for design work:

### Group A: "Transient means what it says" (emergency fix)

**Ideas: #6 + #4 + #14**
**Priority: IMMEDIATE — this is a production bug causing repeated incidents**

These three ideas form a complete fix:

- **#6 (drive preflight)** prevents the creation of useless snapshots. This is the
  primary fix. If no configured drive is mounted, a transient subvolume produces zero
  local operations.
- **#4 (cap of 1)** is the safety net. Even if drive detection is wrong, even if the
  user finds some way to create extra snapshots, you can never have more than one.
  Defense in depth isn't paranoia — it's what five incidents in ten days earns you.
- **#14 (pin self-healing)** repairs the damage from this incident and makes the system
  resilient to future manual interventions. Filesystem is truth (ADR-102) — enforce it.

One design spec. One implementation session. One PR. These are not three features —
they are three aspects of one fix: "transient subvolumes must not accumulate local
snapshots, and when humans intervene, the chain must self-correct."

### Group B: "Transient as a first-class concept" (config v1 integration)

**Ideas: #2 + #12 (informing UPI 010)**
**Priority: Fold into UPI 010 config schema v1 design**

The current config says `local_retention = "transient"` and the user has to infer what
that means. The v1 config should express the intent directly. Whether that's
`mode = "external-only"` or `local_snapshots = false` or something else — that's a
design conversation for UPI 010. Don't build it now, but don't lose the insight: the
config vocabulary should match the user's mental model, not the implementation's
internal retention enum.

### Group C: "Event-driven transient" (horizon)

**Ideas: #10 + #11**
**Priority: After Sentinel active mode is designed**

The truly elegant solution is that transient backups happen when drives appear, not on
a schedule. But that's Sentinel active mode, and the roadmap correctly places that in
the horizon. These ideas should inform that design when the time comes.

## The Vision

Here's what bothers me about this whole situation. Urd's north star says "does it reduce
the attention the user needs to spend on backups?" And yet the user has had to manually
intervene in a near-disaster *five times in ten days*. That's not reducing attention —
that's demanding it.

The fix isn't complicated. A transient subvolume is a promise: "I will back up your data
externally without costing you local space." Right now, Urd breaks that promise silently,
then relies on a separate monitoring system to catch the failure. That's not acceptable
for a tool whose entire identity is "silence means data is safe."

When Group A ships, a transient subvolume should be genuinely invisible. No local
accumulation. No orphaned pins. No panicked manual deletions. The user configures
`local_retention = "transient"`, and from that moment on, Urd handles it. That's the
promise. Keep it.

## The Details

- The brainstorm problem statement says "five incidents in 10 days." That number should
  make everyone uncomfortable. It means the space guard from incident #1 was a patch, not
  a fix. Each subsequent incident was an escalation that proved the patch insufficient.
  Group A isn't just a fix — it's an acknowledgment that the original fix was incomplete.

- Idea #8 suggests blocking manual `urd backup` for transient subvolumes. Never do this.
  When a user types `urd backup`, they want *all* their data backed up. Telling them "no,
  not that one" breaks trust. The right behavior is: if the drive is mounted, create →
  send → clean up. If it's not, skip gracefully with a clear message. The user should
  never have to think about which subvolumes are transient when running a manual backup.

- The pin file situation is more dangerous than it looks. A stale pin doesn't just cause
  a full send (wasting time and space). If the referenced snapshot name happens to match
  a *different* snapshot on the external drive (name collision after deletion and
  recreation), you could get a corrupt incremental. #14's "verify the pin target exists
  locally" check eliminates this entire class of failure.

- The brainstorm correctly notes that `skip_intervals: !args.auto` makes manual runs
  dangerous for transient subvolumes. But the real insight is simpler: for transient,
  interval doesn't matter at all. The only question is "can I send?" If yes, create and
  send. If no, skip. The interval logic is irrelevant to transient's purpose.

## The Ask

1. **Design Group A immediately as a single spec** — #6 (drive preflight) + #4 (cap of 1)
   + #14 (pin self-healing). This is a production bug. Five incidents. Fix it with the
   urgency it deserves. Patch-tier workflow: design is the bug fix, not a feature.

2. **Carry #2 and #12 into UPI 010 config schema conversations** — the vocabulary
   insight ("external-only" vs "transient retention") should inform how v1 config
   expresses subvolume intent. Don't build now, don't lose the idea.

3. **Note #10 and #11 for Sentinel active mode design** — when Sentinel gets the ability
   to trigger backups, "drive appears → back up transient subvolumes" should be the
   first use case.

4. **After Group A ships, update the catastrophic failure memory** — five incidents with
   the same root cause is a pattern. The fix should be documented as a resolved pattern,
   not an open wound.

5. **Kill #8, #15, #3, and #13** — they either punish the user, add complexity without
   proportional benefit, or solve problems that won't exist after Group A.
