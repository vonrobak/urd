---
upi: "000"
date: 2026-04-02
mode: vision-filter
---

# Steve Jobs Review: Backup Strategies and Promises

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Brainstorm — 25 ideas tying backup strategies to protection promises
**Mode:** Vision Filter

## The Verdict

There are two genuinely great ideas buried in here, but most of this brainstorm is building a strategy museum when it should be building a feeling.

## What's Insanely Great

**Idea 6 — Restore verification.** This is the one. Every backup tool in existence lets you take snapshots and feel good about yourself. Almost none of them answer the question that actually matters: "Can I get my data back?" The moment Urd starts verifying restores and tracking that verification as a first-class promise dimension, it stops being a snapshot scheduler and becomes something that actually guarantees recovery. That's a category-defining distinction. The "0 errors" framing from 3-2-1-1-0 is the hook, but the real insight is deeper: *a backup you've never tested is a hope, not a backup.* Making that visible is worth more than every other idea in this document combined.

**Idea 13 — Mirror-awareness.** This is small, but it's exactly the kind of thing that builds trust. When Urd sees BTRFS RAID1 and says "this protects against disk failure, NOT against ransomware or accidental deletion" — that's Urd being smarter than the user needs it to be. That's the moment the user thinks, "Oh, this tool actually understands what it's doing." It's a two-hour implementation that punches far above its weight. And it passes both north-star tests: it makes data safer (by correcting a dangerous misconception) and it reduces attention (the user doesn't have to figure this out themselves).

**Idea 21 — One-command setup.** The `urd setup` flow as written is almost exactly right. Not because of the strategy selection — because of the question "Which subvolumes contain irreplaceable data?" That question reframes the entire interaction from "configure a backup tool" to "tell me what matters to you." That's the right question. Everything else should flow from the answer.

## What's Not Good Enough

**The brainstorm is in love with strategy names.** 3-2-1. 3-2-1-1-0. 4-3-2. GFS. These are industry jargon from the data center world. They're precise and they're useful for professionals who already know what they mean. But look at who Urd is for: a Linux user with BTRFS who wants their data to be safe. They don't walk up to a backup tool thinking "I need a 3-2-1-1-0 strategy." They walk up thinking "I have recordings I can't lose."

When we built the iPod, we didn't ask people "do you want 5GB of flash storage in a DAP with USB 2.0 and AAC codec support?" We said "1,000 songs in your pocket." The specs were real — they mattered enormously in engineering — but the user-facing concept was human.

Ideas 1, 2, 3, 10, 16, 19, 20, 22 — they all assume the user thinks in strategy numbers. They don't. The promise model you already have (guarded/protected/resilient) is the right abstraction level. Those words mean something a human can feel. "Protected" means my data is safe. "Resilient" means even a disaster won't kill it. "3-2-1-1-0" means nothing until you decode it.

The strategy knowledge should be *inside* Urd, not *presented to* the user. When someone chooses "resilient," Urd should satisfy the principles behind 3-2-1 without ever mentioning the number. The doctor output should say "your offsite copy is stale" not "your 3-2-1 strategy is incomplete." Strategy names belong in documentation, in blog posts, in the `--verbose` output for people who want to learn. They don't belong in the primary interface.

**The "Backup Score" (idea 19) is wrong.** A number from 0-100 is a grade. It implies there's a correct answer and you're being measured against it. That's the opposite of what Urd should feel like. Urd is a norn — it doesn't grade you, it tells you the truth about your data's fate. "73/100" makes someone feel judged. "Your recordings are protected. Your documents are at risk — the offsite drive hasn't been connected in 34 days." makes someone feel informed. The promise states (PROTECTED / AT RISK / UNPROTECTED) are already the right vocabulary. Don't replace them with a number.

**Tiered lifecycle (idea 8) and CDP (idea 12) are scope traps.** They're technically interesting. They also turn Urd into something it's not: a data lifecycle management platform. Urd's genius is that it does one thing — BTRFS snapshots and sends — and does it so well you forget it's there. The moment you add tier promotion logic, cloud targets (idea 23), and write-burst monitoring, you're building a different product. These ideas fail the second north-star test catastrophically: they multiply the attention required rather than reducing it.

**Promise composition (idea 10) is complexity theater.** `promises = ["3-2-1", "gfs", "verified"]` — three abstract concepts that the user has to understand individually and then reason about how they merge. This is the antithesis of "set and forget." The right answer is: Urd figures out the composition internally based on what the user actually said they want. One declaration in, complete policy out. The user should never have to compose backup strategies like Lego bricks.

**RPO/RTO framing (idea 18) is the wrong language.** RPO and RTO are business continuity terms. They're precise, they're correct, and they make a homelab user's eyes glaze over. The user's RPO is "I don't want to lose my recordings." Translate that into "resilient" and derive the engineering from there. Don't ask them to quantify their loss tolerance in hours.

## The Vision

Here's what I see when I look at this brainstorm through squinted eyes:

Urd should be the tool where the first experience is a 30-second conversation: "What matters to you?" And the ongoing experience is silence — until the day something goes wrong, and then Urd is the calm voice that says, "Your recordings are safe. Here's how to get them back."

The strategy knowledge — 3-2-1, GFS, immutability, verification — should all live inside Urd as engineering that serves this experience. Not as concepts the user manages. When someone sets "resilient" on their recordings, Urd should:

- Derive GFS retention automatically (it already does this)
- Require offsite rotation (the redundancy encoding design already handles this)
- Track that offsite copy's freshness (awareness model does this)
- Periodically verify that a restore actually works (idea 6 — build this)
- Notice RAID1 and not count it as redundancy (idea 13 — build this)
- Show degradation in terms the user understands: "Your offsite drive hasn't been connected in 34 days. Your recordings can survive a disk failure, but not a house fire." (strategy-aware notifications, but in Urd's voice, not strategy jargon)

That's the product. Not a strategy selection menu. Not a maturity score. Not a compliance dashboard. Just a tool that understands what "resilient" means deeply enough to keep the promise — and honest enough to tell you when it can't.

The maturity ladder idea (14) is interesting but it should be invisible — not "Level 3 of 6" (gamification) but the progressive disclosure work you already have on the roadmap (6-O). Show new users the basics. As they add drives and configure more subvolumes, Urd naturally reveals more capability. The ladder exists, but the user experiences it as growing confidence, not as a score to optimize.

## The Details

- The brainstorm says "rename or alias the retention fields to use GFS terminology: son = hourly/daily, father = weekly, grandfather = monthly." No. "Son" and "father" are gendered, archaic jargon from tape backup. Urd already has better words: hourly, daily, weekly, monthly. Those are plain and correct. Adding `yearly` is good. Renaming to GFS terminology is a step backward in clarity.

- Idea 3's example output uses `✓` and `✗` marks with strategy compliance. The checklist format is right, but the framing should be "what's protecting your data" not "strategy compliance." Compliance is an audit word. Urd is a guardian, not an auditor.

- Idea 7's immutability tracking says "Air gap integrity: ✓ (no retention deletions since 2026-01-15)." The word "integrity" is doing heavy lifting here and it's the wrong word — it sounds like filesystem integrity checking (fsck). "Preserved" or "untouched" is what the user actually cares about. "47 snapshots, untouched since January 15."

- Idea 25 (drive metadata) writes `.urd-drive-metadata.json` to the drive root. If Urd is going to write metadata to external drives, it should be in the same directory where it already writes snapshots — not a dotfile in the root. Don't scatter Urd's presence across the drive.

- The handoff section recommends 5 ideas for `/design`. Five is too many. Pick two. The rest can wait. Focus is saying no.

## The Ask

1. **Build restore verification (idea 6).** This is the single feature that could make Urd fundamentally different from every other BTRFS backup tool. A `urd verify` command that picks a file, restores it, confirms it matches — and tracks the result in the awareness model. Start with manual (`urd verify --subvolume opptak`), graduate to automated (Sentinel triggers periodic verification). This is the "0 errors" leg made real.

2. **Build mirror-awareness (idea 13).** Small, fast, high trust-building impact. When Urd detects RAID1 and explains what it does and doesn't protect against, users learn something important. A few hours of work, permanent goodness.

3. **Absorb strategy knowledge into the existing promise model.** Don't expose strategy names to users. Instead, make "resilient" smarter: it already derives intervals and retention; make it also require offsite, track verification, and detect RAID. The strategies live inside the implementation, not the interface.

4. **Add yearly retention (idea 15).** Simple, additive, enables deep archival. No UX risk. Just a new field in `GraduatedRetention` and the retention logic to match.

5. **Park everything else.** Ideas 8, 10, 12, 16, 17, 18, 19, 23 are either scope traps or complexity for complexity's sake. Ideas 1, 2, 3, 9, 14, 20, 22, 24 have useful kernels that should be absorbed into the promise model rather than built as standalone features. Idea 21 (setup wizard) is already on the roadmap as 6-H — don't duplicate the planning.
