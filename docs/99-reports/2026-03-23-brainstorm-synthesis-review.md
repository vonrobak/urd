# Architectural Adversary Review: Brainstorm Synthesis

> **TL;DR:** The synthesis document has a sound conclusion — UX polish before new features,
> cutover before expansion — but arrives at it through a flawed scoring methodology that
> contains 24 arithmetic errors out of 32 scores, drops 20 of 49 source ideas without
> acknowledgment, and lacks a "data safety" dimension despite reviewing a backup tool.
> The tier placements often contradict the composite scores. The document's instincts are
> right; its rigor is not.

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Review of `docs/99-reports/2026-03-23-brainstorm-synthesis.md`
**Reviewer:** Claude (arch-adversary)
**Base commit:** `b66be6f`

---

## What Kills You

**Catastrophic failure mode for a prioritization document:** It leads the developer to build
the wrong things in the wrong order — wasting months on low-impact work while critical gaps
remain open, or worse, creating a false sense of completeness that delays features that
prevent data loss.

**Distance from this failure:** Moderate. The document's sequencing recommendations are
broadly sensible (cutover first, UX polish second, features third). But the scoring
methodology is unreliable enough that if someone followed the composite scores literally,
they'd build shell completions before `urd restore` — optimizing discoverability of a tool
that can't restore data. The instincts are better than the methodology.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Analytical Rigor | 2 | 75% arithmetic error rate in composite scores; scoring formula structurally flawed |
| Completeness | 2 | 20 of 49 source ideas silently dropped; no acknowledgment of omissions |
| Methodology Soundness | 2 | No data-safety dimension for a backup tool; effort scored inversely causing double-count with tier placement |
| Internal Consistency | 2 | Tier placements contradict composite scores in multiple cases |
| Practical Usefulness | 4 | Despite flaws, the conclusion and sequencing are sound; codebase-fit assessments are genuinely valuable |
| Writing Quality | 4 | Well-structured, clear, consistent format; individual evaluations are insightful |

---

## Design Tensions

### 1. Scoring System vs. Editorial Judgment

The document presents a quantitative scoring system (weighted composite, maximum 50) but
then places items into tiers that don't follow the scores. Post-Backup Summary scores
highest (stated 43, actually 44) but is in Tier 2. Zero-Config Mode scores 40 (actually 42)
but is in Tier 3. `urd` bare = status scores 33 (actually 36) but is in Tier 4.

This reveals a tension: the author's editorial judgment about sequencing (which is good) is
fighting with a scoring system that doesn't capture the right dimensions. The tiers are more
trustworthy than the scores, but the document presents the scores as the basis for the tiers.

**Verdict:** The editorial judgment wins on substance, but the scoring framework undermines
credibility. Either fix the methodology to match the conclusions, or drop the composite
scores and use qualitative tier placement with explicit reasoning.

### 2. UX Polish vs. Functional Completeness

The document heavily favors UX polish (shell completions, help text, error messages) over
functional gaps (`urd restore`, subvolume-drive mapping). This is partially a methodology
artifact — Effort is scored inversely (5=easy), so easy items get higher composites. But
it's also a genuine tension: should a tool that can't restore data spend time on help text?

The Norman brainstorm document itself opens with: *"the catastrophic UX failure mode is
the user believes their data is backed up, but it isn't."* By extension, the second-worst
failure is: the user's data IS backed up, but they can't get it back without manual
`btrfs send | receive`. The synthesis rates shell completions (composite 41/42) higher than
`urd restore` (38/40). That's a methodology failure — completions are nice; restore is
existential.

**Verdict:** UX polish and functional completeness aren't in tension for most items — they
can be interleaved. But when forced to choose, complete the backup story (restore) before
polishing the backup story (help text). The recommended sequencing partially corrects for
this by putting restore in "Post-Cutover Expansion," but that's 13th out of 19 items.

### 3. Single User vs. Generalization

Urd has exactly one user. Many highly-scored items (setup wizard, zero-config, sudoers
generator, configuration profiles) serve hypothetical future users. The document doesn't
distinguish between "valuable to the person using Urd today" and "valuable if Urd had
100 users." This matters because the single user already has a config file, already has
sudoers set up, and already knows the subcommands.

**Verdict:** The document should explicitly flag which features serve the current user
vs. future adoption. Both matter, but they have different urgency. For the current user:
structured errors, post-backup summary, restore, subvolume-drive mapping, UUID fingerprinting.
For future users: setup wizard, zero-config, sudoers, packaging. The current user's
operational needs should gate adoption-focused work.

---

## Findings

### Critical: 75% of Composite Scores Are Wrong (Arithmetic)

**Severity: Critical** (undermines the document's quantitative foundation)

24 of 32 composite scores contain arithmetic errors. Most are 1-2 points low, but some are
off by 3 (Sentinel: stated 38, correct 41; Time-Travel Browser: stated 31, correct 34;
FUSE: stated 32, correct 35). The errors are not random — they systematically undercount,
suggesting a consistent miscalculation.

**Consequence:** If anyone relies on the composite scores for prioritization, the ordering
is wrong. Several items would change tiers with correct scores. The corrected ranking of
Tier 1 items shifts; the gap between tiers narrows or disappears for some items.

**Corrected scores for items where the error changes relative ranking:**

| Idea | Stated | Correct | Tier Impact |
|------|--------|---------|-------------|
| Post-Backup Summary | 43 | 44 | Highest score overall; should arguably be Tier 1 |
| Zero-Config Mode | 40 | 42 | Higher than Shell Completions |
| Shell Completions | 41 | 42 | Tied with Zero-Config |
| Sentinel + Notifications | 38 | 41 | Higher than Structured Errors (39) |
| `urd setup` Wizard | 39 | 41 | Tied with Sentinel |
| `urd restore` | 38 | 40 | Higher than UUID, Sudoers, Subvol-Drive |
| `urd` bare = status | 33 | 36 | Close to Tier 2 items |

**Fix:** Recalculate all scores. But more importantly, acknowledge that the tiers are
editorially determined and the scores are supplementary, not determinative.

### Significant: No "Data Safety" Dimension

**Severity: Significant** (the scoring system can't express the most important property
of a backup tool)

The six dimensions are: User Value, Effort, Simplicity, UX Impact, Coolness, Ease of Use.
None of these directly capture "does this prevent data loss or improve recovery reliability."
Data safety is partially embedded in "User Value," but it competes with convenience and
time-saving in that dimension.

For a backup tool, "prevents sending to wrong drive" (UUID) and "enables restoration"
(`urd restore`) and "warns about broken chains" (chain health) are categorically different
from "saves typing" (shell completions) and "looks polished" (help examples). The scoring
methodology treats them as commensurable when they're not.

**Consequence:** Safety features are systematically underweighted. UUID fingerprinting
scored 38 (actually 39) — below shell completions at 41 (actually 42). In a backup tool,
preventing wrong-drive sends should outrank tab completion.

**Fix:** Either add a "Data Safety" dimension with high weight (3x), or explicitly
override composite scores for safety-relevant features with a "safety premium" annotation.

### Significant: 20 of 49 Ideas Silently Dropped

**Severity: Significant** (the document claims "40+ ideas" but evaluates only 29)

The source documents contain 49 distinct ideas. The synthesis fully evaluates 29, mentions
10 in the cross-cutting table without scoring, and completely omits 10 more. The omitted
ideas include several that are safety-relevant or low-effort:

**Safety-relevant omissions:**
- Norman §3.2 — Surface skipped sends loudly (directly addresses the catastrophic failure mode)
- Norman §5.5 — Warn on dangerous retention policies (prevents misconfiguration-caused data loss)
- Norman §2.3 — Chain health as first-class concept (makes the most important state visible)
- Norman §5.1 — Config validation with specific fixes (prevents broken backup runs)

**Low-effort omissions that contradict "evaluate everything":**
- Norman §4.2 — Workflow-ordered command listing (pure text change, like help examples)
- Norman §5.3 — Build staleness warning (small `vergen` addition)
- Norman §5.4 — Refuse to run without config (already partially implemented)
- Future §4.3 — Snapshot mounting helper (trivial compared to FUSE; mentioned but not scored)

**Entire theme omitted:**
- Future Theme 8 (Advanced BTRFS Features) — all four ideas (compsize, reflinks, qgroups,
  scrub) are absent. These are domain-specific and lower priority, but the document should
  at least acknowledge their existence and explain the omission.

**Fix:** Either evaluate all ideas (even with brief one-line assessments for obvious
deferrals) or add a section explicitly listing ideas that were reviewed and intentionally
excluded, with a sentence explaining why. "Claims 40+ ideas" while silently dropping 20
is a completeness problem.

### Significant: Tier Placement Contradicts Scores

**Severity: Significant** (the document's two prioritization systems disagree)

Using corrected composite scores, the tier boundaries are:

| Tier | Score Range (stated) | Score Range (corrected) |
|------|---------------------|------------------------|
| Tier 1 | 36–41 | 37–42 |
| Tier 2 | 29–43 | 30–44 |
| Tier 3 | 31–40 | 34–42 |
| Tier 4 | 25–33 | 25–36 |
| Tier 5 | 16–21 | 17–22 |

Tiers 1, 2, and 3 have overlapping score ranges. Post-Backup Summary (44) is the
highest-scoring item but sits in Tier 2. Sentinel (41) and Zero-Config (42) outscore
most Tier 1 items but sit in Tier 3.

The editorial logic for this is sound — Post-Backup Summary needs production data to
calibrate, Sentinel needs battle-tested backup first, Zero-Config is substantial work.
But the document doesn't explain the overrides. It presents scores as the rationale for
tiers, then quietly violates that relationship.

**Fix:** Add a sentence to each tier explaining the placement logic. "Despite scoring 44,
Post-Backup Summary is Tier 2 because it requires production backup runs to calibrate
what information matters." This makes the editorial judgment transparent instead of
hidden behind unreliable numbers.

### Moderate: Effort Inversely Scored Creates Double-Count

**Severity: Moderate**

Effort is scored 1–5 where 5 means easy. This means effort contributes *positively* to
composite scores — easy items score higher. But items are also tiered by effort (Tier 1
= "Low-Effort"). This double-counts feasibility: easy items score higher *and* get placed
in higher tiers.

The consequence is that a high-effort, high-value feature like `urd restore` (Effort=2,
contributes +4 to composite) is penalized twice: once in the score and once in the tier
placement. Meanwhile, shell completions (Effort=5, contributes +10) gets boosted twice.

**Fix:** Either score effort normally (1=easy, 5=hard) and subtract it from the composite,
or remove effort from the composite entirely and use it only for tier placement. The
current setup conflates "should we build this" (value) with "can we build this quickly"
(feasibility) in a single number.

### Moderate: "Coolness" as a Dimension

**Severity: Moderate**

"Coolness" is defined as "would this make someone say 'that's clever'?" This is not a
meaningful evaluation criterion for a backup tool. Coolness favors novel, visible features
(FUSE filesystem, time-travel browser) over invisible safety features (UUID verification,
retention warnings). It's a vanity metric that biases toward demo-friendly features.

The weight is only 1x (lowest), so the impact is bounded. But its presence signals that
the evaluation framework values impression over substance. In a tool whose catastrophic
failure mode is silent data loss, the question isn't "is this cool?" but "does this
prevent harm?"

**Fix:** Replace "Coolness" with "Data Safety" (does this reduce the probability or
severity of data loss?). Or keep coolness at 0x weight and add a safety dimension.

### Commendation: Codebase-Fit Assessments

The "Codebase fit" paragraphs under each Tier 1 and Tier 2 idea are the best part of the
document. They ground abstract evaluations in concrete implementation reality: which module
to modify, which trait needs extension, what's backward-compatible. These assessments
demonstrate real understanding of the architecture and provide actionable starting points.

Examples:
- "The planner currently iterates all subvolumes × all mounted drives. Adding a filter
  is a single `if` in the planning loop." — Specific, verifiable, immediately useful.
- "Single-file restore doesn't need btrfs receive at all — just copy from the read-only
  snapshot path. Start with the simple case." — Shows decomposition thinking.

**Why this matters:** These paragraphs make the difference between a document that someone
reads and files away, and a document that someone reads and starts building from.

### Commendation: Sequencing Acknowledges Project Reality

The recommended sequencing is the most valuable section of the document. It correctly
identifies that the operational cutover is the gate, that structured errors should be
informed by real failures observed during cutover, and that adoption-focused features
(setup wizard, zero-config) belong after the tool is battle-tested. This shows mature
project judgment that the scoring methodology fails to capture.

---

## The Simplicity Question

The document's scoring framework has 6 dimensions, weighted composites, 5 tiers, and
32 individually-scored items. This is more machinery than the decision requires.

A simpler approach: sort the 49 ideas into four buckets based on two questions:

1. **Does the current single user need this before other users can benefit?** (yes/no)
2. **Can this be built in one session or does it need design first?** (quick/design-needed)

| | Quick | Design Needed |
|--|-------|---------------|
| **Current user needs** | Shell completions, help text, bare=status, pre-flight checks, NO_COLOR, error codes, UUID fingerprinting | Structured errors, post-backup summary, subvolume-drive mapping, restore (simple case) |
| **Adoption / expansion** | Sudoers generator, workflow-ordered help, suggested next actions | Setup wizard, zero-config, sentinel, SSH targets, restore (full), notifications |

This 2×2 captures the essential prioritization without 32 tables of numbers. The detailed
scoring is useful as supporting evidence for contested items, but it shouldn't be the
primary framework.

---

## Priority Action Items

1. **Fix the arithmetic.** 24 wrong scores undermine the document's credibility. Recalculate
   or remove composite scores entirely.

2. **Account for all 49 ideas.** Add a section listing the 20 omitted ideas with one-line
   disposition ("deferred — no current user need" or "subsumed by X").

3. **Add a data-safety dimension or override.** Features that prevent data loss should be
   explicitly flagged and given priority weight.

4. **Make tier overrides transparent.** When editorial judgment places an item in a different
   tier than its score suggests, say so and explain why.

5. **Separate value from feasibility in the scoring.** Either remove Effort from the composite
   or score it normally (1=easy) and subtract.

6. **Promote silent-failure surfacing (Norman §3.2) and dangerous-retention warnings
   (Norman §5.5).** These are safety features that were silently dropped. They're low-effort
   and directly address the catastrophic failure mode.

7. **Reframe the single-user vs. adoption tension.** Be explicit about which features serve
   the current user vs. future users. Both matter; the urgency differs.

---

## Open Questions

1. **Was the scoring done by hand or by formula?** The systematic under-counting (mostly
   off by 1-2) suggests mental arithmetic rather than a spreadsheet. If the intention was
   approximate scoring, the document should say so.

2. **Were the 20 omitted ideas deliberately excluded or accidentally missed?** If deliberate,
   the omission rationale is important. If accidental, the document needs a completeness pass.

3. **Who is the audience for this document?** If it's the sole developer making build
   decisions, the codebase-fit paragraphs and sequencing section are sufficient — the scoring
   framework adds complexity without changing the decisions. If it's for future contributors
   or stakeholders, the quantitative rigor matters more.
