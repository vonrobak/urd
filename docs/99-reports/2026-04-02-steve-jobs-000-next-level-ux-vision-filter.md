---
upi: "000"
date: 2026-04-02
mode: vision-filter
---

# Steve Jobs Review: Next-Level UX Brainstorm

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Brainstorm document "Next-Level UX -- Norman Principles Applied to Urd v0.5" and its grill-me scoring results
**Mode:** Vision Filter

## The Verdict

This brainstorm is good work that arrives at mostly the right answers, but it is unfocused -- it generates thirty ideas when it needs three, and the scoring session had to do the curation that the brainstorm should have done from the start.

## What's Insanely Great

**The one-sentence status (1.1) is the best idea in this document and it scored a 10 for the right reasons.** `urd` with no arguments answering "is my data safe?" is the entire product philosophy in a single interaction. When we built the original Mac, the test was: can someone who has never seen a computer sit down and do something useful in ten minutes? Here the test is: can someone who forgot Urd exists type three letters and know if their data is safe? That is a perfect feature. Build it first.

**The rejection list is more impressive than the acceptance list.** Saying no to `urd browse`, the guided restore CLI, weekly digest, and problem-first collapsing -- those are the right calls, and the reasoning is sharp. "Don't rebuild the shell." "Urd's silence IS the message." "Terminal users already have `cp`, `ls`, `find`." This is someone who understands that a great product is defined by what you leave out. The catastrophic failure in this project's history makes these rejections even more important -- every feature is a surface area for things to go wrong.

**The "thread" naming decision is elegant.** Replacing "chain" with "thread" in user-facing text while keeping internal naming stable is exactly right. It is intuitive, mythically resonant, and requires zero new infrastructure. That ratio -- high impact, zero complexity -- is the mark of a good design decision.

**The voice layering principle (9.2) is architecturally correct.** Mythic first line, technical second line. The user reads as deep as they want. This is progressive disclosure applied to tone, not just information. It respects both the emotional and rational modes of reading.

## What's Not Good Enough

**The brainstorm does not distinguish between features that serve the two north-star tests and features that are just interesting.** Space forecasting (2.4), the API server (11.2), predictive scheduling (11.3), file manager integration (10.2) -- these are solutions looking for problems. They add complexity the user must manage. They fail both north-star tests. The brainstorm treats them as "explore further" or "uncomfortable ideas" when it should treat them as distractions.

Space forecasting in particular is seductive and wrong at this stage. A backup tool that tells you your drive will be full in 14 months is giving you information you cannot act on today. It adds a number to your status output that you will glance at, feel vaguely reassured by, and ignore -- until it is wrong, at which point you lose trust in all the other numbers. Forecasting is a feature that earns its place after the core promise system is bulletproof. Not before.

**The `urd doctor` concept (4.2) is undersized.** It scored a 9 but the description is a grab-bag of checks. A great diagnostic command is not a checklist -- it is an argument. "Your data is safe because X, Y, Z. The one thing that concerns me is W, and here is what to do about it." The current design reads like a CI pipeline output. It should read like a consultation. Doctor is the command where the mythic voice earns its keep -- the norn examining the threads and telling you which one is fraying.

**The notification design (section 8) is over-engineered for where the project is.** Three tiers, notification memory, "all clear" resolution notifications -- this is a notification framework, not a notification design. The sentinel already has urgency levels and debouncing. The brainstorm should have asked: what is the one notification that, if it works perfectly, makes the user trust Urd completely? That notification is: "Something is wrong with your data, here is exactly what, here is exactly what to do." Everything else is polish on top of that single critical path.

**The document is too long.** Eleven sections, thirty-odd sub-ideas, a scoring table, resolved decisions, rejected candidates, key principles, naming decisions, and a recommended sequence. This is a brainstorm that became a design doc that became a project plan. Each of those is valuable. Mixing them dilutes all three. The brainstorm phase should have ended at the scoring table. Everything after that is a different document.

## The Vision

Here is where Urd should be heading with UX, and it is simpler than this brainstorm suggests.

Urd has two moments of truth. The first is when the user checks on their data -- `urd` or `urd status`. The second is when something goes wrong and the user needs to know about it. Every UX investment should serve one of those two moments. Everything else is secondary.

For the first moment: the one-sentence status is the answer. Make it perfect. Not good, perfect. The sentence must be accurate, immediate, and complete. It must adapt to context -- what happened since the user last looked, not just what the current state is. It must feel like consulting someone who knows everything and wastes no words. When I look at a great status line, I think of the battery indicator on the iPhone -- one glance, total understanding, no interpretation required. That is the bar.

For the second moment: one notification path that is impossible to miss and impossible to misunderstand. Not three tiers. One path with escalating urgency built into the language, not the infrastructure. "WD-18TB1 away for 5 days" and "WD-18TB1 absent 47 days -- protection degrading" are the same notification at different urgency levels. The voice does the escalation. The plumbing stays simple.

Between those two moments, Urd is silent. That silence is the product. When someone asks "how do you know your backups are working?" the answer should be "because Urd would have told me if they weren't." That sentence is the entire brand promise.

## The Details

**Section 2.3 ("Urd remembers")** is a nice touch but the example is wrong. "Last connected: 2026-03-23 (8 days ago). 5 sends completed that session." The user does not care how many sends happened last time. They care whether the drive is current. Reframe: "WD-18TB1 connected. 8 days of snapshots waiting. Estimated send: ~4.2 GB, ~3 minutes."

**Section 5.2 (graduated confirmation)** has the right instinct but the wrong trigger. Do not prompt for full sends -- prompt for sends above a size or time threshold. A full send of 200 MB is routine. An incremental send of 50 GB (because someone downloaded a dataset) is not. Size is the user's actual concern, not the send type.

**Section 6.3 (completion sounds)** is the kind of feature that sounds reasonable in a brainstorm and becomes a configuration burden in practice. The user now has to decide: do I want sounds? How long is "long enough" to trigger a sound? What sound? Kill this. If the user wants sounds, their desktop notification system already provides them.

**The implementation sequence at the bottom** mixes independent work items with a voice overhaul that depends on a vocabulary audit that does not exist yet. This creates a dependency bottleneck. The one-sentence status, shell completions, and doctor can ship independently and immediately. The voice work is a separate arc. Do not let the voice arc block the utility arc.

**"Backup-now imperative" in status.md** is listed as the current priority but does not appear in this brainstorm. These two documents are not talking to each other. The backup-now feature is fundamentally a UX feature -- it is about what happens when a human types `urd backup` versus what happens at 04:00. It belongs in this conversation.

## The Ask

1. **Build the one-sentence status immediately.** `urd` with no arguments calls `assess_all()`, renders one sentence through `voice.rs`, and exits. No design phase needed -- the brainstorm already describes exactly what to build. This is the single highest-impact feature in the pipeline and it has zero dependencies.

2. **Redesign `urd doctor` as a narrative, not a checklist.** The output should read as a structured assessment with a bottom-line conclusion, not a pass/fail list. Open with the verdict ("Your data is safe" or "One issue needs attention"), then support it with evidence, then close with actions. Think of it as `urd status` for someone who is worried.

3. **Merge staleness escalation and next-action suggestions into a single "voice enrichment" work item.** They are both presentation-layer changes to `voice.rs` over existing data. They do not need separate design phases. Write the graduated language table, write the next-action patterns, implement both in one pass.

4. **Kill space forecasting, the API server, predictive scheduling, file manager integration, and completion sounds.** They fail the north-star tests at this stage of the project. If any of them become necessary later, they will be obvious. Do not spend design energy on them now.

5. **Separate the brainstorm document from the grill-me results.** The scoring, resolved decisions, rejected candidates, and implementation sequence should be their own document. The brainstorm is a creative artifact. The decisions are an operational artifact. Mixing them makes both harder to reference.
