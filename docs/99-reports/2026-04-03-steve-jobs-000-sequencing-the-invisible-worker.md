---
upi: "000"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Sequencing the Invisible Worker

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Sequencing UPIs 013-017 into the existing roadmap (Phases A-D complete through 010, test session → Phase D ahead)
**Mode:** Vision Filter (strategic sequencing)

## The Verdict

You have five new features, an existing roadmap pointing toward Phase D (the encounter),
and a test session that hasn't happened yet — and the right answer is to resist the
engineer's instinct to build everything before testing anything.

## What's Insanely Great

**The existing roadmap has a principle that must survive this expansion.** Phases A-D
answered questions in order: "Is Urd telling the truth?" → "Is Urd speaking clearly?" →
"Does Urd know its drives?" → "Can Urd welcome a new user?" Each phase built on the
last. Each had a gate. This discipline is what got you from v0.3.0 to v0.10.0 without
losing the thread.

The new UPIs are good ideas. But good ideas are dangerous when they don't have a
question they're answering.

## What's Not Good Enough

**The new UPIs don't have a phase.** The roadmap has Phase A through D, each answering
a question about the product. UPIs 013-017 are floating — they came from a competitive
analysis, not from a product question. They need to be grounded in the same framework,
or they'll scatter the roadmap.

Here's the question each one actually answers:

| UPI | Feature | The real question |
|-----|---------|-------------------|
| 013 | Btrfs pipeline | "Is Urd doing its job efficiently?" |
| 014 | Skip unchanged | "Is Urd being intelligent?" |
| 015 | Change preview | "Can I trust what I'm restoring?" |
| 016 | Emergency response | "Will Urd save me when things go wrong?" |
| 017 | Thread lineage | "Can I understand what happened?" |

These cluster into two product phases:

**Phase E: "Is the invisible worker smart?"** — 013, 014, 016
These make the nightly run better without the user doing anything. Compressed sends,
skipping unchanged subvolumes, emergency space recovery — they're all invisible worker
improvements. The user doesn't invoke them. The user benefits from them. This is the
heart of north star #2: reduce the attention the user spends on backups.

**Phase F: "Can I trust what I see?"** — 015, 017
These make the invoked norn more trustworthy. Change preview answers "what am I getting
back?" Thread lineage answers "what happened to my chain?" Both are consultation
experiences. They pass north star #1 by making the user more confident in their data's
safety.

## The Vision

Here's how I'd sequence this:

```
Current state: v0.10.0 deployed, test session pending
                    │
Test session (calendar days — live with the tool)
                    │
Phase E: Make the invisible worker smart (~1.5 sessions)
  013 (btrfs pipeline, 0.25 session) ──┐
  014 (skip unchanged, 0.5 session) ───┤── tag v0.11.0
  016 (emergency response, 1 session) ─┘
    But wait — read below.
                    │
Phase D: The Encounter (~6-8 sessions)
  6-O (progressive disclosure)
  6-H (the encounter) ─→ v1.0 horizon
                    │
Phase F: Trust the invoked norn (~1 session)
  015 (change preview, 0.5 session) ──┐── tag v1.1 or v1.0 stretch
  017 (thread lineage, 0.5 session) ──┘
```

**Why this order:**

**013 goes first because it's free.** `--compressed-data` and `subvolume sync` are
invisible correctness improvements. Zero UX surface. Zero risk. The test session will
run nightly backups — might as well run them with the optimal btrfs flags. Ship this
*before* or *during* the test session. It's a Tuesday afternoon task.

**014 goes before Phase D.** Skip-unchanged is the invisible worker demonstrating
intelligence. When the encounter generates a config with 9 subvolumes, half of which
change rarely, the user shouldn't see 9 identical snapshots every night. They should
see: "Skipped 5 unchanged subvolumes." That's the encounter working as designed —
the tool is smart enough to manage itself. If 014 ships after the encounter, the first
impression is worse than it needs to be.

**016 is the hardest call.** Emergency response directly addresses the catastrophic
failure that shaped every risk decision in this project. Part of me wants it yesterday.
But the interactive `urd emergency` command requires design review (`/grill-me` +
adversary), the automatic pre-backup thinning touches the executor's critical path, and
the sentinel integration has notification implications. This is a standard-tier feature
that needs the full workflow.

My recommendation: build 016's *automatic* mode (pre-backup thinning) before Phase D.
Defer the interactive `urd emergency` command until after the encounter — it's the kind
of power-user tool that can come in v1.1 without hurting the first impression. The
automatic mode is the invisible worker. The command is the invoked norn. Sequence them
accordingly.

**Phase D (the encounter) stays where it is.** The encounter is Urd's first impression.
It must not be delayed by feature work that doesn't directly serve it. 013 and 014
serve it indirectly (smarter runtime behind the encounter). 015-017 don't serve it at
all — they're post-encounter depth.

**015 and 017 go after Phase D.** Change preview and thread lineage are depth features
for users who already trust Urd. They make a good product better. They don't make the
first encounter better. Ship them as the quality-of-life release after v1.0 — the kind
of update that makes users tell their friends "this tool keeps getting better."

## The Details

- **Don't forget the external-only runtime experience.** The brainstorm from earlier
  today (status table showing `LOCAL: 0` and `degraded` for external-only subvolumes)
  is not in any UPI yet. It should be. It's a Phase E item — making the invisible
  worker's output honest about `local_snapshots = false`. Call it 010-b or fold it
  into 014 (both touch awareness and voice for "how does Urd present subvolume state?").

- **014 and 015 share `subvolume_generation`.** If you build 014 first (which you
  should), 015 gets the trait method for free. Don't build 015 first just because it
  feels more exciting — the shared infrastructure matters.

- **016's automatic mode doesn't need a `/grill-me`.** Automatic pre-backup thinning
  is a retention policy change under space pressure. It's an enhancement to existing
  executor logic, not a new command. The interactive `urd emergency` command needs the
  full design workflow. Split them.

- **The test session should specifically test 013's changes.** If you ship `--compressed-data`
  and `subvolume sync` during the test session, you get real-world validation for free.
  Watch for: sync latency after deletion batches, send performance with compressed data
  flag, any btrfs version incompatibilities.

## The Ask

1. **Ship 013 now** — before or during the test session. It's invisible, correct, and
   makes every nightly run better. Patch-tier: build → check → PR → done.

2. **Ship 014 after the test session, before Phase D.** Skip-unchanged is the single
   feature that most improves the encounter's first impression. It's the difference
   between "Urd created 9 snapshots" and "Urd created 4 snapshots (5 unchanged)."

3. **Split 016 into automatic (Phase E) and interactive (post-Phase D).** Build the
   pre-backup thinning now. Build `urd emergency` later. The invisible worker doesn't
   wait for the full design workflow. The command does.

4. **Create a UPI for the external-only runtime experience.** The `LOCAL: 0` / `degraded`
   false alarm is a real product bug that affects the test session right now. Either fold
   it into an existing UPI or create 010-b.

5. **Ship 015 and 017 as post-encounter depth.** They're great features. They're not
   first-impression features. Let the encounter land first, then reward the users who
   stayed.
