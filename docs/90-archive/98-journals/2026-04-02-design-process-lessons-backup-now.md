# Journal: Design process lessons — how /steve and /grill-me transformed UPI 003

> **TL;DR:** The /steve review caught five product-level issues the engineering-focused
> /design missed entirely (flag naming, pre-action tone, empty-plan UX, lock metadata,
> vocabulary precision). The /grill-me session then resolved all open questions by
> triangulating between the spec, the review, and codebase facts — producing a design
> with zero open questions from one that had three.

**Date:** 2026-04-02
**Base commit:** `0d7113f`
**Context:** Designing UPI 003 (backup-now imperative) — ran /design, then /steve, then
/grill-me in sequence on the same feature.

## What happened

The `/design` pass produced a technically sound spec: `--scheduled` flag, `skip_intervals`
parameter on `plan()`, `PreActionSummary` type, clean module map, five rejected
alternatives with reasoning. Three open questions flagged for /grill-me. The architecture
was correct — the spec respected all ADR invariants and module boundaries.

Then `/steve` reviewed it and found five problems the engineering lens didn't see:

1. **Flag name `--scheduled` is wrong.** It describes *when* (implementation), not *what*
   (behavior). `--auto` reads naturally in unit files where it'll be read most.

2. **Pre-action summary is a spreadsheet, not a briefing.** "Snapshotting 7 subvolumes"
   is a status report. "Backing up everything to WD-18TB" is an authority acknowledging
   your request. Same information, completely different experience.

3. **"Nothing to do." is dismissive.** The design correctly made this path near-impossible
   in manual mode but didn't address what happens when it does hit. The invoked norn
   doesn't shrug.

4. **Lock trigger hardcoded to "timer".** Adjacent to the work, one-line fix, makes
   error messages honest. Engineering design missed it because it wasn't "in scope."

5. **Vocabulary nuance.** Steve suggested replacing "away" with "not connected" in the
   pre-action summary, but this was actually wrong — "away" is a load-bearing vocabulary
   term for the off-site lifecycle state, not a synonym for disconnected. This surfaced
   during /grill-me when the user corrected it.

The `/grill-me` session then walked through 13 decision branches, resolving all three
open questions plus uncovering new decisions the spec hadn't considered:
- `skip_intervals` belongs in `PlanFilters`, not as a new `plan()` parameter
- `urd plan` should gain `--auto` for consistency (was missing from the original spec)
- The `INVOCATION_ID` replacement question needs arch-adversary review, not a snap decision
- Drive roles should distinguish "away" (offsite) from "not connected" (primary) in
  the pre-action summary
- Dry-run inherits mode from `--auto` naturally, no special handling

## Lessons learned

**The /steve review finds a different class of problem than /design.** /design catches
architectural mistakes — wrong module boundaries, missing ADR gates, untested edge cases.
/steve catches experience mistakes — wrong words, wrong tone, wrong information hierarchy,
missing emotional beats in the user interaction. Neither review can do the other's job.
The backup-now feature is technically simple (threading a boolean through the planner) but
the *product* is the pre-action summary, the empty-plan messaging, the flag name that
appears in every unit file. /steve caught all of those.

**Steve reviews can also be wrong, and that's valuable.** The "away" → "not connected"
suggestion was incorrect — it would have erased a meaningful vocabulary distinction. But
the act of pushing on the vocabulary forced an explicit discussion of *why* "away" exists
and what it communicates. The wrong suggestion surfaced the right conversation.

**/grill-me is most productive when it has two inputs to triangulate.** With just the
design spec, /grill-me would have walked the three open questions and resolved them. With
the spec *and* the Steve review, it resolved 13 decisions — the three original questions
plus ten new ones that emerged from the tension between engineering correctness and product
quality. The two documents disagreed on several points (flag name, vocabulary, scope
boundary), and resolving those disagreements produced a stronger design than either
document alone.

**"Adjacent to the work" is a valid scope expansion criterion.** The lock trigger fix
wasn't in the original design. Steve flagged it, and /grill-me confirmed it was a one-line
change that makes the feature coherent. The rule isn't "stay in scope" — it's "expand
scope only when the marginal cost is low and the marginal coherence is high."

**Open questions should have recommended answers.** The original spec's three open
questions all had recommendations, and all three recommendations survived /grill-me
unchanged. The format of "here's the question, here's my recommendation, here's why"
makes the stress-test productive — the interviewer can push on the reasoning instead of
starting from scratch.

## Impact on current work

The design spec at `docs/95-ideas/2026-04-02-design-003-backup-now-imperative.md` has
been revised with all 13 resolved decisions. Zero open questions remain. The Steve review
is at `docs/99-reports/2026-04-02-steve-jobs-003-backup-now-almost-right.md`. Both are
ready for `/prepare` → `arch-adversary` → `/post-review` → build.

One architectural question (`INVOCATION_ID` replacement) is explicitly deferred to
arch-adversary review — this was the right call during /grill-me rather than making a
snap decision about overlapping safety signals.
