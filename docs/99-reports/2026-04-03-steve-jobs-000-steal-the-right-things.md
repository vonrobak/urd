---
upi: "000"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Steal the Right Things from btrbk

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Brainstorm — competitive analysis of btrbk, 23 ideas for Urd
**Mode:** Vision Filter

## The Verdict

This brainstorm is disciplined and self-aware — it knows what to steal and what to
protect — but it buries the two ideas that would actually transform the product under
a pile of infrastructure improvements that would make Urd more like btrbk instead of
more like itself.

## What's Insanely Great

**Idea #23 is the most important idea in the document.** The list of what btrbk does
NOT have — promise model, awareness, sentinel, progressive disclosure, mythic voice,
guided setup — this isn't a footnote, it's the entire strategic thesis. Every idea in
this brainstorm should be measured against this list: does it widen or narrow the gap
between Urd and btrbk? Because the gap is Urd's moat.

**Idea #20 nails the competitive positioning.** "btrbk tells you what snapshots exist.
Urd tells you if your data is safe." That's not a feature comparison — that's a category
difference. When I was building the iPod, the competition was measuring features: more
megabytes, more formats, more buttons. We measured something else: does this person have
their music with them? Urd is measuring the right thing.

**Idea #19 understands the tension without flinching.** The promise-first vs.
operations-first trade-off is real, and the resolution — `urd plan` for transparency,
custom mode for control, the encounter for guidance — is architecturally honest. This
is the kind of insight that prevents a tool from losing its identity while growing.

**The brainstorm correctly kills ideas #5, #9, #11, #12.** Rate limiting, incremental
parent selection, transaction logs, group filtering — all parked with clear reasoning.
The discipline to say "not now, not needed" is rare. Protect that instinct.

## What's Not Good Enough

**The handoff section leads with a performance optimization.** `--compressed-data` is
idea #21 in position #1 of the handoff. It's a one-line change to a flag that makes
sends faster. I don't dispute its value. But it's invisible to the user. Nobody types
`urd backup` and thinks "wow, my compressed extents were passed through without
decompression." It makes Urd faster. It doesn't make Urd *better*.

The handoff should lead with the ideas that change how the user *feels* about their
backups. The compressed-data flag is a patch-day cherry on top, not a strategic priority.

**Idea #18 (emergency wipe) is framed as operations when it should be framed as
experience.** "Override retention to keep = 1, free space immediately" — that's the
mechanism. But the *experience* is: your NVMe is suffocating, Urd is about to stop
working, and instead of panicking you type one command and Urd handles it. The
catastrophic failure memory makes this emotionally urgent, but the brainstorm describes
it as a retention override. That's like describing the iPhone's emergency SOS as "a
function that dials a phone number repeatedly."

What would great look like? `urd emergency` doesn't just wipe — it explains what it's
about to do, asks for confirmation, tells you what it kept and why, and advises you
on what to check afterward. It's a guided panic response. The wipe is the mechanism;
the guidance is the product.

**Ideas #1 and #2 (clean and resume) are the right problems, wrong surface.**
`urd clean` and `urd backup --resume` are btrbk's answers to btrbk's problem: the user
is a sysadmin who manages operations. Urd's answer should be different. When a send
fails, Urd should clean up garbled backups *automatically on the next run* — the user
shouldn't need to know `urd clean` exists. When snapshots exist that haven't been sent,
the planner should *implicitly* prefer sending those before creating new ones.

btrbk needs explicit commands because it has no daemon and no awareness. Urd has both.
The invisible worker should handle recovery invisibly. Expose the capability in
`urd doctor` for power users, but the default path should be: Urd notices, Urd fixes,
Urd tells you it fixed it.

**Idea #6 (snapshot_on_change) is described as noise reduction when it's actually a
trust signal.** "Fewer identical snapshots means less clutter in `urd status`" — that's
the mechanism. But the product insight is deeper: when a user sees that Urd created
no snapshot for their docs subvolume, what do they think? "Nothing changed, so nothing
needed." That's Urd demonstrating intelligence. It noticed. It decided. That's a
trust-building moment that the brainstorm undervalues.

The config field name `snapshot_on_change` is wrong though. It's operations-language.
The user doesn't think about "change detection for snapshot creation." They think:
"skip it if nothing changed." Or better: Urd just does it by default. Do you need a
config field at all? When has anyone *wanted* a snapshot of an unchanged filesystem?

## The Vision

btrbk is the power drill. Urd is the house that builds itself.

btrbk solves backup by giving you excellent tools and trusting you to use them correctly.
It's honest about this — `btrbk run`, `btrbk resume`, `btrbk clean`, `btrbk archive` —
every command is an operation you initiate. You are the backup system. btrbk is your hands.

Urd's thesis is fundamentally different. Urd *is* the backup system. You tell it what
matters. It figures out the rest. When something goes wrong, it tells you. When it can
fix something itself, it does. The encounter is not "here's your config file, edit it" —
it's "tell me what you're afraid of losing."

The ideas worth stealing from btrbk are the ones that make the *invisible worker* smarter,
not the ones that give the *user* more buttons. btrbk's `clean` should become Urd's
automatic recovery. btrbk's `resume` should become Urd's implicit retry. btrbk's
`--compressed-data` should just be on by default when the kernel supports it.

The ideas that would make Urd genuinely great — the ones I'd fight for — are the ones
that widen the gap:

**Show me what changed before I restore.** Idea #15, `urd diff` or a change preview in
`urd get`. This is the moment of maximum anxiety: something is gone, you need it back,
you're not sure which snapshot has the right version. btrbk can list changed files but
has no restore command. Urd has `urd get` but shows you nothing before committing. Combine
them and you have something neither tool offers: the confidence to restore. "These 3 files
changed since yesterday. Want them back?" That's trust. That's the product.

**Make the panic moment humane.** Idea #18 reframed. Not `urd emergency` as a wipe
command — `urd emergency` as a guided crisis response. Assess the situation, explain
what happened, offer options with consequences, execute with confirmation, report what
was saved. The 2am scenario. This is where Urd earns its name — the norn at the well,
speaking with authority when it matters most.

## The Details

- Idea #7 (yearly retention): btrbk confirms demand but the brainstorm doesn't ask the
  product question — who needs yearly snapshots and what are they actually preserving?
  Tax records? System configs? The retention tier is meaningless without the use case.
  Don't add it because btrbk has it. Add it when you know who needs it and why.

- Idea #8 (preserve_hour_of_day): described as "low complexity, high correctness impact"
  but the real question is: should the user configure this, or should Urd derive it from
  `run_frequency`? If the timer runs at 04:00, the retention boundary should be 04:00.
  The user shouldn't have to tell Urd twice.

- Idea #16 (origin/lineage tree): `urd thread <subvolume>` is the right name and the
  right concept. The example output in the brainstorm reads well. This belongs in the
  `urd doctor --thorough` path — not a new command, an enrichment of the diagnostic
  that already exists.

- Idea #22 (btrfs_commit_delete): "wait for transaction commit after deletions" solves
  the NVMe space-pressure problem silently. No config toggle needed. If Urd deletes
  snapshots before creating new ones (which it does), ensuring the space is actually
  freed before proceeding is just correct behavior. Don't make it optional.

- The brainstorm mentions `snapshot_on_change` as a config field. If this is built, the
  default should be `true`. Unchanged snapshots are waste. The user should opt *in* to
  redundant snapshots (e.g., for forensic purposes), not opt out of intelligence.

## The Ask

1. **Build change preview into `urd get`.** Before restoring, show what changed. This is
   the single highest-impact feature Urd can steal from btrbk's playbook — and btrbk
   can't do it because btrbk has no restore command. Urd is the only tool that can offer
   the complete loop: see what changed → choose what to restore → get it back.

2. **Make garbled backup cleanup automatic.** Don't build `urd clean` — build automatic
   detection of partial receives into the executor's pre-send check. If a target has a
   garbled subvolume from a crashed previous run, clean it up, log it, continue. The user
   reads "recovered from interrupted send" in the backup summary, not "run urd clean."

3. **Add `--compressed-data` to sends.** Yes, it's invisible. Yes, it's a performance
   optimization. But it's also free and correct. Detect kernel support, enable by default,
   done. This is a Tuesday afternoon task, not a strategic priority.

4. **Design the emergency response, not the wipe command.** `urd emergency` deserves a
   `/design` pass. The mechanism (aggressive retention) is trivial. The experience (guided
   crisis response) is where the value lives. This is worth getting right because it's
   where people will remember how Urd made them feel.

5. **Skip unchanged subvolumes by default.** Don't add `snapshot_on_change`. Just make
   Urd smart enough to skip empty snapshots. If the generation number hasn't changed,
   don't create the snapshot. Log it. Move on. This is the invisible worker being
   intelligent, not the user configuring intelligence.
