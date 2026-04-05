---
upi: "000"
date: 2026-04-05
mode: vision-filter
---

# Steve Jobs Review: Presentation Polish and Roadmap Fit

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** Brainstorm (presentation layer polish) + roadmap integration
**Mode:** Vision Filter

## The Verdict

This brainstorm found the right problems and the right moment to solve them. Urd is
about to build The Encounter — the first thing a new user sees. Shipping that on top
of an interface with trust gaps and buried findings would be building a beautiful front
door on a house with crooked hallways. Polish the hallways first.

## What's Insanely Great

**The brainstorm correctly identified information hierarchy as the core problem.** Not
styling, not features, not vocabulary — *hierarchy*. The verify and doctor commands have
the right information; they present it in the wrong order. That's the kind of problem
that sounds small and is actually fundamental. When you get information hierarchy right,
everything else clicks. When you get it wrong, no amount of polish on individual elements
helps.

**The trust coherence theme (B) is the most important insight in the document.** When
`urd status` says "2 need attention — run `urd doctor`" and doctor says "All clear,"
that's not a UX annoyance. That's a breach of the tool's implicit promise: "I will tell
you the truth, and my surfaces will agree." For a backup tool — a tool people trust with
their data — command coherence isn't nice to have. It's the foundation.

**The brainstorm stays grounded in real architecture.** Every idea names the actual modules
and functions it touches. That's discipline. Ideas that name `voice.rs` and
`render_verify_interactive` are ideas that can be designed, reviewed, and built. Ideas
that say "improve the output" are wishes.

## What's Not Good Enough

**The brainstorm doesn't prioritize ruthlessly enough.** Twenty-five ideas across six
themes. Half of them are worth doing. Maybe five of them are worth doing *now*. The
brainstorm should have been harder on itself about what matters before The Encounter and
what can wait.

Here's the filter I'd apply:

**Must ship before The Encounter (Phase D):**
- Trust coherence (B1+B2) — a new user who follows breadcrumbs to dead ends will not
  trust the tool
- Findings-first hierarchy (A1+A3) — a new user running `urd verify` should see problems
  first, not 60 lines of "OK"
- Paper cuts that signal low care (E1, E2) — pluralization bugs and unfulfilled promises
  are the kind of thing that makes a user question everything else

**Should ship alongside or shortly after The Encounter:**
- Relative timestamps (C4) — warm, quick, polishes the status command
- Subvolume chooser (D1) — better error for the one surface that currently dumps on users
- Status summary enrichment (C3) — all absent drives named, not just the first
- Protection vocabulary tuning (E4) — "aging" not "degrading"

**Park until after v1.0:**
- --verbose propagation (D3) — needs semantic design per command, not worth the
  distraction now
- Streaming verify (A2) — cool but architecturally expensive for marginal UX gain
- Health score (F4) — rightfully identified as probably bad for CLI
- Diagnostic journey (F3) — beautiful vision but belongs in a future where Urd is
  interactive, not now
- --explain mode (F2) — belongs with progressive disclosure (6-O), not here
- History relative timestamps (E5) — nice but low-impact

**Kill:**
- F4 (health score) — reductive numbers betray the promise model. "Sealed" is better
  than "94/100" because sealed is a *state*, not a *grade*. Don't grade the user's
  backup setup.
- C2 (first-row legend) — too clever. If the column header needs a legend, fix the
  header. Don't make the first row magic.
- A5 (generic compression utility) — premature abstraction. Fix verify and doctor
  specifically. If a pattern emerges, extract it then.

## The Vision

Here's how this fits into the roadmap. The roadmap says:

> **Gate:** Live with v0.11.0 for several days before designing The Encounter.

You've lived with it. You ran the test session. You found the problems. The gate is
passed — not because everything is perfect, but because you know exactly what isn't.

The question isn't whether to fix these things. It's *when*. And the answer is: before
The Encounter, not after.

Think about it this way. The Encounter is the moment a new user meets Urd for the first
time. They run `urd init`, they go through the Fate Conversation, Urd generates their
config. Then what? They run `urd status`. They run `urd doctor`. They run `urd verify`.
And if those commands have trust gaps and buried findings and `issue(s)` pluralization,
the carefully crafted first encounter leads into an uncrafted second encounter.

The original Mac team obsessed over the out-of-box experience. But they *also* obsessed
over what happened five minutes after the out-of-box experience. The transition from
"welcome" to "daily use" is where most products lose people. Urd's transition from
"The Encounter" to "the invoked norn" needs to be seamless.

So here's the sequencing I'd propose:

```
Current: v0.11.1 deployed, test session complete

Phase D-prep: Presentation Polish              (~1-2 sessions)
    UPI 023 — Information hierarchy + trust coherence
    UPI 024 — Paper cuts and warmth

Phase D: Progressive Disclosure + The Encounter (~6-8 sessions)
    6-O — Progressive disclosure
    6-H — The Encounter
```

Phase D-prep is small. It's voice.rs changes, doctor verdict logic, a few rendering
tweaks. No new modules, no new architecture. But it ensures that when The Encounter
hands off to the daily-use commands, those commands are worthy of the handoff.

## The Details

**The roadmap needs one line added.** Between "Deploy v0.11.0" (done) and "Phase D:
Progressive Disclosure + The Encounter," insert a polish phase. Call it what you want —
"Phase D-prep," "Presentation polish," "The Second Encounter" — but make it explicit
that the test session findings get addressed before the first-run experience is built.

**F5 (voice as personality layer) is the most interesting parked idea.** Not for now. But
when The Encounter is built and Urd has contextual communication from progressive
disclosure (6-O), the idea that the same data renders differently based on context —
invoked vs. notification vs. glance — is the natural next step for voice.rs. Note it in
the Horizon section of the roadmap.

**D3 (--verbose) has a hidden dependency on A1.** If verify becomes findings-first with
`--detail` for the full view, then `--verbose` for verify becomes unnecessary — `--detail`
*is* verbose. And if status already shows everything, `--verbose` for status is meaningless.
The semantic question "what does verbose mean per command?" is mostly answered by fixing
the default hierarchy first. Then you see what's left.

## The Ask

Two design specs. Both are pre-Encounter work. Both are voice.rs-centric.

**Design 1: UPI 023 — "The Honest Diagnostic"**

Scope: A1 (findings-first verify) + A3/A4 (doctor findings separation) + B1/B2 (trust
coherence) + E2 (fulfill command suggestions).

This is one coherent design because the principle is one: *every diagnostic command leads
with what matters and agrees with every other command.* The implementation touches
verify rendering, doctor verdict logic, doctor thread rendering, and status advice text.

Why before The Encounter: A new user running `urd verify` after setup must see "all
threads intact" or "1 problem found," not 60 lines of per-check output.

**Design 2: UPI 024 — "The Warm Details"**

Scope: C3 (status summary enrichment) + C4 (relative timestamps) + C5 (ext-only rename)
+ C6 (zero-duration humanization) + D1 (subvolume chooser) + E1 (pluralization) + E3
(Unicode width) + E4 (protection vocabulary).

This is the paper-cuts-and-warmth pass. Each item is small. Together they transform the
feeling of the tool from "engineered correctly" to "crafted with care." None require
architectural decisions — they're all rendering changes in voice.rs.

Why before The Encounter: The Encounter will generate a config and then invite the user
to explore. If `urd status` shows ISO timestamps, `urd retention-preview` dumps subvolume
names, and `urd drives` has misaligned columns, the crafted welcome dissolves into an
uncrafted daily experience.

**Order: 023 first, then 024.** The hierarchy and trust fixes are structural — they change
what users see when they follow the status → doctor → verify diagnostic path. The warmth
fixes are textural — they polish what's already there. Structure before texture.
