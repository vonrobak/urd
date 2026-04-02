---
upi: "000"
date: 2026-04-02
mode: design-critique
---

# Steve Jobs Review: Progressive Disclosure of Redundancy Concepts

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Design O -- progressive disclosure via milestone-triggered insights
**Mode:** Design Critique

## The Verdict

This is the right idea with the wrong center of gravity -- it's designed around a message delivery system when it should be designed around a relationship.

## What's Insanely Great

The state-triggered philosophy is exactly right. "Tell the user about offsite on day 7" is what a tutorial does. "Tell the user about offsite when they actually connect an offsite drive" is what a companion does. This distinction alone puts the design ahead of every backup tool I've seen.

The anti-patterns section is unusually strong. "Tutorial voice," "generic tips," "urgency escalation" -- these are the exact failure modes that turn every notification system into noise. The fact that someone sat down and named them means they've been thought about, not just avoided by accident.

The "at most once" invariant enforced at the database level is elegant. `INSERT OR IGNORE` on a primary key is a beautiful way to make the "never repeat" promise structural rather than behavioral. You can't nag even if you have a bug.

The separation between Idea I (recommendations, repeating, gap-oriented) and Idea O (insights, once-ever, achievement-oriented) is clean. They address different emotional moments. That table in the design doc is worth more than most architecture diagrams.

And the north-star test: does this make data safer? Yes -- not by preventing loss, but by building the user's understanding of *why* their setup matters. The user who has seen "your data has crossed a threshold -- it endures beyond these walls" thinks differently about offsite drives than the user who just configured one.

## What's Not Good Enough

**The catalog is too mechanical.** Eight insights sounds like a product manager counted features. The question isn't "how many milestones can we track?" It's "which moments actually change how the user feels about their data?" I count three that genuinely matter:

1. First backup (you went from nothing to something)
2. First offsite (your data survives your house burning down)
3. The 30-day streak (everything has been fine, and you didn't have to think about it)

The rest -- AllProtected, NewDrive, FirstTransientCleanup, ChainBreakRecovered, RecoveryFromUnprotected -- are operational events, not emotional ones. They're interesting to the developer, not to the person whose photos are at stake. A user doesn't wake up thinking about incremental chain breaks. If all eight fire over the first month, the feature becomes a drip of messages the user learns to ignore. That's the opposite of the design's stated goal.

**The voice isn't earned yet.** "Every thread is woven tight" for AllProtected -- what does that even mean to someone who just wants to know their data is safe? Compare to the first backup message: "Your data now rests in two places. If one thread frays, the other holds." That one works because the first sentence is concrete and the second adds the metaphor as color. The AllProtected message is all metaphor, no substance. The mythic voice needs a concrete anchor in every single message, or it becomes decoration.

**The delivery architecture is overbuilt for what it does.** A new SQLite table, six new methods on StateDb, a new field in sentinel state JSON, modifications to three commands -- all to deliver eight one-time messages. The design says "100-130 lines of new code" but the architectural surface area is much larger than the line count suggests. Every new table is a migration commitment. Every new sentinel field is a contract with Spindle. This should be lighter.

**"Delivered" semantics are fuzzy.** The design says `urd status` marks an insight as delivered. But what if the user runs `urd status` while troubleshooting something else and doesn't notice the insight at the bottom? It's marked delivered but never actually seen. The "delivered" flag is tracking display, not comprehension. For a once-ever message, that distinction matters.

## The Vision

Imagine this. You install Urd. You configure it. The first backup runs at 4 AM. The next morning, you type `urd status` -- because that's what you do when you've just set up a new tool. And there, quietly, below the status table:

> Your data now rests in two places.

That's it. No "thread frays" metaphor. No "journey milestone" framing. Just a calm acknowledgment that something important happened while you slept.

Three weeks later, you plug in your offsite drive for the first time. Urd sends your data to it. Next time you check status:

> Your data endures beyond this machine. WD-18TB1 carries a copy.

And then a month after that, when you've forgotten Urd is even running, you check status and see:

> Thirty days without incident. Your data is well kept.

Three messages. Three real moments. The rest is silence. Silence means data is safe -- the design doc says so. Live it.

The person who loves this tool doesn't love it because it sent them eight milestone messages. They love it because it said the right thing at the right time and then shut up. That's what trust feels like.

## The Details

**The "thread/weave" vocabulary is doing too much work.** It appeared in the vocabulary redesign (project_vocabulary_decisions.md says "thread" is the standard term), but "thread" in these insight messages competes with the concrete information. "If one thread frays, the other holds" -- is a thread a backup? A subvolume? A drive? The metaphor needs to resolve to something specific or get out of the way. In the first backup message, drop the second sentence entirely. "Your data now rests in two places" is stronger alone.

**Insight priority ordering is unspecified for a real scenario.** The design says "deliver the highest-priority one" when multiple milestones fire simultaneously, but the catalog doesn't define priority. On first run, FirstBackup, AllProtected, and FirstTransientCleanup could all fire at once. Which one wins? This needs an explicit ordered list, not a handwave.

**The streak counter living in the milestones table is clever but fragile.** A single row being updated in place by the sentinel (incrementing streak_days) while also being read by `urd status` (checking if streak_days >= 30) is a concurrency surface. It works because SQLite serializes writes, but it means the HealthyStreak row has fundamentally different lifecycle semantics than every other row in the table. That's a maintenance trap.

**The "observed_at" vs "delivered" gap creates ghost state.** A milestone observed on March 1 but not delivered until April 15 (when the user finally runs `urd status`) will display with... what timestamp? The observed_at? That's confusing -- "Urd said this 45 days ago and I'm only now seeing it?" The design doesn't address temporal coherence of delayed delivery.

**First-run experience is unaddressed.** What does `urd status` look like *before* the first backup? The design assumes the user's first meaningful interaction is after a backup completes. But the actual first-run moment is `urd status` showing... nothing? A blank promise table? That's the moment where an insight would matter most, and the design has nothing for it.

**Panic-moment experience is unaddressed.** When a drive fails and subvolumes go UNPROTECTED, the user runs `urd status` in a state of anxiety. The RecoveryFromUnprotected insight fires later, when the crisis is resolved. But during the crisis, insights are silent. That's actually correct -- insights shouldn't compete with urgent notifications. But the design should explicitly state this: "During degradation, insights step aside." That's a design decision worth documenting, not an accident.

## The Ask

1. **Cut the catalog to three insights.** FirstBackup, FirstOffsite, HealthyStreak. These are the three moments that change how a user thinks about their data. Ship these. If they work, the architecture supports adding more later -- but prove the concept with fewer, better messages first.

2. **Rewrite every message with a concrete-first rule.** The first sentence must be factual and specific. The second sentence (if any) can add voice. If the message is strong enough in one sentence, stop there. Test each message by reading it aloud to someone who doesn't know what BTRFS is.

3. **Simplify the storage.** Three insights don't need a full SQL table with six methods. A JSON file in the state directory with three boolean fields would work. When (if) the catalog grows past five insights, migrate to SQLite. Don't build infrastructure for a feature that hasn't proven itself yet.

4. **Define explicit priority ordering** in the design doc. A simple numbered list. Don't leave it to implementation.

5. **Address the first-run gap.** The user's first `urd status` before any backup has run is the most important moment for building trust. Even if it's not an "insight" per se, the design should acknowledge what happens there.

6. **Drop the "delivered" tracking entirely for v1.** Show the latest milestone in `urd status` for some fixed period (say, 7 days after observed_at), then stop showing it. No tracking of whether the user "saw" it. Simpler, and avoids the false-positive delivery problem. The log has the permanent record.
