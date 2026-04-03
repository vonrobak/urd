---
upi: "010-a"
date: 2026-04-03
mode: design-critique
---

# Steve Jobs Review: Transient as First-Class Config Concept

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Design doc `docs/95-ideas/2026-04-03-design-010a-transient-as-first-class-config.md`
**Mode:** Design Critique

## The Verdict

This design solves the right problem with the right instinct — but it doesn't go far
enough, and it needs to be evaluated in the light of the encounter, not just the config
file.

## What's Insanely Great

The core insight is exactly right: **the config should say what the user means, not what
the system does.** When someone writes `local_retention = "transient"`, they're not
thinking about retention mechanics. They're thinking "my NVMe is 500GB and I can't afford
local snapshot history." The config should express *that*.

`local_snapshots = false` does this beautifully. It reads like English. It's a boolean —
the simplest possible type for the simplest possible question: "Do you want local
snapshots? No." There's no ambiguity, no documentation needed, no mental model to build.
A user sees this field and instantly knows what it does. Compare that to "transient" —
a word that means nothing without a glossary.

The rejected alternatives show good taste. Option B (`mode = "external-only"`) would have
introduced a new axis that overlaps with protection levels — that's the kind of
"flexible" design that makes a product feel like a tax form. `send_only = true` describes
the system's behavior instead of the user's observable outcome. The design correctly chose
the option that matches what the user *sees*: no local snapshots.

And the timing argument is perfect. V1 has one user. The cost of this change is near zero
today and grows with every week v1 stabilizes. This is the moment.

## What's Not Good Enough

**1. This design exists in isolation from the encounter.**

Phase D is the most important thing on Urd's roadmap. The encounter is where a new user
tells Urd what they're afraid of losing, and Urd generates a config. That conversation
will need to express the concept this design addresses — "I want external backups but my
drive is too small for local history."

The design treats this as a config-file problem. It's actually a *conversation* problem.
How does the encounter explain this choice? "Do you want to keep local snapshots?" is a
question that requires understanding what local snapshots are and why you might not want
them. That's too much for a first encounter.

The encounter should be able to say something like: "Your root volume is on a 500GB NVMe.
I'll send backups to your external drive but skip local history to save space." The user
says "yes" and the config gets `local_snapshots = false`. The field name is correct, but
the design should acknowledge that most users will never write this field — they'll
*confirm a choice the encounter made for them.* That changes the priority: the field
needs to be readable (✓), but more importantly, the encounter needs a way to reason about
space constraints and derive this choice automatically.

This isn't a scope creep request — the design doesn't need to build the encounter. But it
should connect to it. A one-paragraph section on "how does this integrate with guided
setup?" would anchor the design in the product trajectory instead of treating it as an
isolated config cleanup.

**2. The named-level interaction (Q3) is still fuzzy.**

The design says `protection = "sheltered" + local_snapshots = false` is "cleaner" than the
transient exception, but the justification — "storage constraint, not policy preference" —
is the same justification the original exception used. If the old version was
"architecturally clean but conceptually fragile" (the design's own words, line 44), why
is the new version not also conceptually fragile?

Here's the real question: **can a user with `protection = "sheltered"` and
`local_snapshots = false` still claim to be "sheltered"?** The sheltered promise says "I
have local snapshots and at least one external copy." If you disable local snapshots,
you've broken that promise. You have external-only. That's not sheltered — it's something
else.

The design should take a position on this. Either:
- `local_snapshots = false` is incompatible with named levels (forces custom), or
- Named levels adapt their promise semantics when local snapshots are disabled

I think the first option is correct. If you're disabling local snapshots, you're making a
custom choice that doesn't fit a named promise. Named levels should be opaque *and
complete* — they describe a full protection posture, not a posture-minus-local. Forcing
custom for this case is honest. And it's fine — the v1 example config already shows how
custom works.

**3. The comment in the example config does the heavy lifting.**

Look at the v1 example (lines 99-113 of `urd.toml.v1.example`):

```toml
# Transient retention keeps only the pinned snapshot needed for incremental
# chains. Ideal for subvolumes on space-constrained volumes (NVMe root)
# where you want external backups but can't afford local snapshot history.
```

Four lines of comment to explain one config field. That's a smell. When you need a
paragraph to explain a boolean, the boolean might be fine — but the surrounding context
isn't self-documenting enough. After 010-a ships, the example should express the *intent*:

```toml
# ── External-only: NVMe root is too small for local history ─────────
```

One line. The field `local_snapshots = false` does the rest.

## The Vision

Here's what I see when I imagine Urd at its best:

The config file is a *narrative*. You read it top to bottom and understand exactly what
Urd is protecting and how. Each subvolume block answers two questions: "what is this?" and
"how safe is it?" Named levels answer the second question in one word. Custom blocks
answer it explicitly.

`local_snapshots = false` fits this vision perfectly — but only if it appears in the right
context. Not as a retention hack. Not as an exception. As a deliberate statement: "this
subvolume doesn't keep local history, and here's why" — and the "why" is expressed by the
combination of fields (small drive, external sends configured).

When the encounter generates this config, it should produce a block that reads like the
conclusion of a conversation:

```toml
# NVMe root — external backup only (500GB too small for local history)
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
local_snapshots = false
snapshot_interval = "1d"
send_interval = "1d"
drives = ["WD-18TB1"]
```

No protection level. No "transient". No jargon. Just a clear statement of what this
subvolume gets and why. The intention comment at the top connects the config to the
human decision. The fields below are mechanical consequences of that decision.

That's the product. This design gets us closer to it.

## The Details

- The design says "~0.25 session as part of UPI 010 session 3" (line 145). Session 3 is
  done. Update the effort estimate to reflect standalone implementation.

- Q2 (field name: `local_snapshots` vs `local_history`) is already answered by the
  existing vocabulary. The config uses `local_retention`, `snapshot_root`,
  `snapshot_interval` — the word is "snapshot" everywhere. `local_snapshots` is consistent.
  Close this question.

- Q1 (require drives?) is correctly answered: yes. `local_snapshots = false` + no drives =
  not backing up. Catch it. Close this question.

- The "Internal implementation" section (lines 117-124) is for the engineers, not the
  product. Fine in a design doc, but don't let it drive decisions. The internal
  representation (`Transient`) can stay or change — what matters is the config surface.

## The Ask

1. **Take a position on Q3.** I recommend: `local_snapshots = false` forces custom (no
   named level). Named levels are complete promises. If you opt out of local snapshots,
   you're making a custom choice. This is the clearest, most honest answer.

2. **Add one paragraph on encounter integration.** How does guided setup detect and suggest
   `local_snapshots = false`? Space constraints on the source volume? User explicitly
   declining local history? This doesn't need to be designed — just acknowledged.

3. **Close Q1 and Q2.** The answers are clear. Open questions that are already answered
   create the illusion of uncertainty.

4. **Update the sequencing section.** Sessions 1-4 are done. This is a standalone ~0.25
   session now, probably best as a follow-on to the v0.9.1 test session (test the
   transient behavior in practice, then improve the vocabulary).

5. **After building, update the v1 example config.** Replace the four-line comment block
   with a one-line section header that expresses intent.
