---
upi: "000"
date: 2026-04-02
mode: vision-filter
---

# Steve Jobs Review: v0.8.0 Test Findings Brainstorm

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** `docs/95-ideas/2026-04-02-brainstorm-v080-test-findings.md` — 22 ideas from test results and product review
**Mode:** Vision Filter

## The Verdict

This brainstorm is unusually well-grounded — born from real testing, not speculation — and it correctly identifies that Urd's most urgent work is making promises it can actually keep, not adding new features.

## What's Insanely Great

The brainstorm's best quality is that it traced the drive identity problem all the way to the specific code path: `TokenMissing` treated as benign when SQLite already has a record. That's not an idea — that's a diagnosis. The difference between "we should improve drive identity" and "here's the exact four-line change that would have prevented a catastrophe" is the difference between a brainstorm that generates work and a brainstorm that generates results.

The seven themes are well-organized by what they do for the user, not by what module they touch. Theme 1 is "the drive knows who it is" — that's a user promise, not a code concept. Theme 2 is "close the loop" — that's a feeling, not a feature. This is the right way to think about a backup tool.

The "uncomfortable ideas" section is honest. Auto-backup on drive connect (7A) is the dream — and the brainstorm correctly says it's only safe after drive identity is bulletproof. That's taste: knowing when an idea is right but its time hasn't come.

## What's Not Good Enough

### Too many ideas that nibble when they should bite

Ideas 1D (LUKS UUID), 1E (filesystem label check), and 1F (snapshot fingerprinting) are three different ways to add a secondary identity signal. They're all technically valid. None of them are necessary if you do 1A right. When the simple fix works, three clever alternatives aren't insurance — they're distraction.

The same pattern appears in Theme 5. Ideas 5A and 5B are two approaches to the plan/dry-run divergence. But the divergence was a footnote in the test report — a curiosity, not a pain point. Nobody got confused by it during testing. Nobody made a wrong decision because of it. There are real problems to solve; this isn't one of them yet.

### Missing the emotional design of the safety gate

Idea 6C (relabel "partial" backups) is correct but undersized. The chain-break full send gate was the hero of this test session — it literally saved the user's data on 2TB-backup. But what did the user see? "FAILED htpc-root" in red. A safety feature that works perfectly shouldn't announce itself with the word "FAILED."

This isn't just a label change. It's a question about how Urd communicates safety. When the seatbelt catches you, the car doesn't say "DRIVING FAILED." It caught you. That's what it's for.

### The rescue path ideas are too timid

6A (guided `urd get`) and 6B (`--list` to show versions) are fine incremental improvements. But they don't address the fundamental problem Steve raised: `urd get` is the most important command in the entire tool and it gets the least love.

What if `urd get` without arguments didn't just show a help message — what if it showed your most recent snapshots and asked what you're looking for? What if it was the beginning of a conversation, not the end of an error? The brainstorm suggests printing example commands. That's a manual. I'm talking about an experience.

## Top 10 — Scored and Sequenced

The scoring criteria: how much does this improve the user's life per unit of effort, weighted by "does it make data safer" (the north star that matters most for a backup tool at this stage)?

### Tier 1: Build Now (these fix broken promises)

**1. Idea 1A — TokenMissing gate when SQLite has a token — 95/100**

This is the single most important change in the brainstorm. A backup tool that can't tell its drives apart is a backup tool that can't be trusted. The fix is surgical: one conditional in `verify_drive_token()`. It turns a fail-open gap into a safety gate. It passes both north-star tests — data is safer, and the user doesn't need to visually inspect every backup plan for signs of drive confusion.

Build this first. Before anything else. Today if possible.

**2. Idea 3A — Fix assess() scoping — 90/100**

The status display is Urd's face. When it lies — and false degradation is a lie — the user learns not to trust it. Then when real degradation happens, they ignore it. This is the boy who cried wolf, and for a backup tool, that's not a fable — it's a data loss path.

The fix pattern already exists in the codebase. Apply it. Patch tier.

**3. Idea 3C + 3D combined — Replace [OFF] Disabled, consider omitting from skip list — 82/100**

I'm combining these because 3D is the better idea but 3C is the safer implementation. Start with `[LOCAL]` instead of `[OFF] Disabled`. If that feels right, graduate to omitting local-only subvolumes from the skip section entirely — they're not skipped, they're done.

This matters more than it looks. Every time Urd says "Disabled" about something the user intentionally configured, it undermines their confidence in the tool. "Did I misconfigure this? Is something wrong?" No — Urd just chose the wrong word.

### Tier 2: Build Soon (these close experience gaps)

**4. Idea 2A — Drive reconnection notifications — 78/100**

This is the "click" — the sensory confirmation that something happened. The Sentinel already knows. The notification infrastructure exists. Wire them together. But I want to adjust the proposal: don't just notify on connection. Notify with context.

Not: "2TB-backup connected."
But: "2TB-backup is back after 10 days. 4 subvolumes need catch-up sends."

The notification should answer "do I need to do anything?" not just "something happened." One sentence that closes the loop.

**5. Idea 6C expanded — Safety gate communication redesign — 75/100**

Expand beyond relabeling "partial." When a safety gate fires, the entire output should communicate: "I protected your data by not doing something. Here's what I held back and why. Here's how to proceed when you're ready."

The backup summary should say "success (1 deferred)" not "partial." The per-subvolume line should say "DEFERRED: full send gated — 31.8GB to 2TB-backup requires opt-in" not "FAILED." The footer should say "Run `urd backup --force-full --subvolume htpc-root` when ready" — which it already does, but the word FAILED drowns it out.

**6. Idea 4B — Correlate pin-age warnings with drive absence — 72/100**

Doctor telling you "sends may be failing" when the drive was on a shelf is the tool making you worry about nothing. This erodes trust the same way false degradation does — it trains the user to ignore warnings. And ignored warnings are how data gets lost.

The fix is straightforward: doctor already knows drive mount state. Check it before attributing old pins to send failures.

### Tier 3: Design First (these need more thought)

**7. Idea 1C — `urd drives` subcommand — 68/100**

I believe in the need but I want to adjust the scope. `urd drives` should exist, but it needs to be minimal at first: `urd drives` (list with status) and `urd drives adopt <label>` (reset token). That's it. `identify` and `forget` can come later.

The reason to build this is practical: Idea 1A needs somewhere to point its error message. "This drive doesn't have the expected token" needs to end with "run `urd drives adopt WD-18TB` to accept it." Without that command, the error message points at nothing.

**8. Idea 2E — Drive absence milestones — 65/100**

This is the graduated version of "absent 10d — protection degrading." The insight is that offsite drives have different absence tolerances than primary drives. A 7-day absence for an offsite drive is fine; a 7-day absence for a primary drive is alarming.

I'd simplify the proposal: two tiers, not five. "Away" (informational, expected) and "overdue" (action needed). The threshold comes from `DriveRole` — offsite drives have a longer leash. Don't make this configurable yet; bake in sensible defaults and see if anyone complains.

**9. Idea 6A + 6B fused — `urd get` as a guided experience — 60/100**

Fuse these. When `urd get` is invoked without arguments, show available snapshots with examples. When invoked with a path but no `--at`, show available versions of that file across snapshots. Make the rescue path self-discovering.

But don't build this before the encounter (6-H). The guided setup wizard will establish the pattern for conversational CLI interaction. `urd get` should follow that pattern, not invent its own.

**10. Idea 4A — Suppress UUID suggestion for cloned drives — 55/100**

Small fix, small impact, but it stops doctor from giving advice the system itself prevents you from following. That kind of self-contradiction is the product equivalent of a store employee telling you to use the door that's locked. It's not a crisis — it's just embarrassing.

### Honorable Mentions (park, don't forget)

**Idea 1B (Sentinel token verification)** — 50/100. Right idea, wrong time. After 1A makes backup-time verification solid, move checks earlier to Sentinel for instant feedback. This is a v0.9 or v0.10 enhancement, not a v0.8.x patch.

**Idea 7A (Auto-backup on drive connect)** — 45/100 today, 85/100 after drive identity is bulletproof. This is the destination. Don't drive there until the road is built.

**Idea 3B (Single-sentence status verdict)** — 40/100. I love the concept but it's a redesign of the most-used command. The assess() scoping fix (3A) needs to land first so the status output isn't lying. Then the verdict redesign can work with truthful data. Design this as part of 6-O (progressive disclosure).

### Ideas to kill

**Idea 1D (LUKS UUID)** — Clever engineering solution to a problem that 1A solves more simply. LUKS is an implementation detail; tokens are a relationship. Urd should know its drives by relationship, not by their encryption layer.

**Idea 1E (filesystem label check)** — Marginal value admitted in the brainstorm itself. Kill it.

**Idea 1F (snapshot fingerprinting)** — Interesting as a research idea but absurdly over-engineered for the problem. A UUID-v4 token file already fingerprints a drive uniquely. Comparing snapshot inventories is n^2 complexity for something a 36-character string already handles.

**Idea 5A and 5B (plan/dry-run alignment)** — Solution looking for a problem. Nobody was confused during testing. Defer until someone actually reports it as an issue.

**Idea 2C (Sentinel event log)** — Journald already does this. Building a parallel event log is duplication. If journald isn't visible enough, improve `urd sentinel log` to read from journald, not from a custom store. But even that is low priority.

**Idea 2D (Protection restored as composite event)** — Architecturally interesting but over-designed for the current stage. 2A (simple reconnection notification) gets 80% of the value at 20% of the complexity. Build 2A, see if the remaining 20% matters.

## The Vision

Here's what this brainstorm tells me about where Urd is and where it's going.

Urd has crossed a threshold. It's no longer a tool that needs features — it's a tool that needs to be *trustworthy*. The test session proved the engine works. It also proved that the engine can't always tell what it's working on.

The next phase of Urd isn't about adding capability. It's about making every existing capability honest. When status says "sealed," that must be true. When a drive connects, Urd must actually know which drive it is. When doctor gives advice, following it must actually be possible.

Think of it like the original Macintosh. We didn't ship it when the hardware worked. We shipped it when every pixel on every screen was right — because a computer that works but doesn't feel trustworthy is a computer nobody trusts with their work. Urd works. Now make every word on every screen mean exactly what it says.

The sequencing I've proposed follows a simple principle: **fix the lies first, then improve the truth.** assess() scoping lies about health. `[OFF] Disabled` lies about intent. Silent drive reconnection lies by omission. "FAILED" lies about safety gates that worked. Fix all of these, and the tool that emerges will be one that deserves the trust its promise model claims.

Then — and only then — build toward the dream: a tool where you plug in a drive, Urd recognizes it by name, catches it up automatically, and tells you when it's done. That's the invisible worker and the invoked norn working in concert. That's the tool people love.

## The Ask

Build sequence, respecting dependencies:

1. **Idea 1A** — TokenMissing gate. Patch tier. Do it now.
2. **Idea 3A** — assess() scoping fix. Patch tier. Do it now.
3. **Idea 3C** — `[LOCAL]` label. Patch tier, five minutes.
4. **Idea 1C (minimal)** — `urd drives` and `urd drives adopt`. Needed for 1A's error messages.
5. **Idea 6C expanded** — Safety gate communication ("deferred" not "failed"). Patch tier.
6. **Idea 2A** — Drive reconnection notifications with context.
7. **Idea 4B** — Doctor pin-age correlation with drive absence.
8. **Idea 4A** — Suppress UUID suggestion for cloned drives.
9. **Idea 2E** — Drive absence milestones (two-tier: "away" vs "overdue").
10. **Idea 6A+6B** — Guided `urd get`. Design alongside 6-O/6-H.

Items 1-5 are patch tier. Items 6-8 are standard tier. Items 9-10 need `/design` first.
