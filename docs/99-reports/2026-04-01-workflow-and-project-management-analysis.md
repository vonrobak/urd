# Workflow and Project Management Analysis

> **TL;DR:** After 10 days of AI-assisted development (v0.0.0 to v0.7.0, 692 tests,
> 14 ADRs, 60 PRs), the Urd project's development pipeline is well-designed and
> productive. The artifact management system around that pipeline has not kept pace,
> accumulating five structural problems: inconsistent review naming (7 patterns across
> 77 reports), incoherent phase numbering (4+ overlapping schemes), orphaned Claude Code
> plans (53 unmappable files), undistinguished review types (design vs. implementation),
> and a roadmap document serving dual duty as architecture reference and living tracker.
> All proposed solutions are convention-based — no external tools, no databases, no
> retroactive renaming.

**Date:** 2026-04-01
**Type:** Process analysis
**Scope:** Development workflow, documentation artifacts, project management conventions

---

## 1. Executive Summary

The Urd project has built a remarkably productive development workflow in 10 days.
Nine slash commands form a coherent pipeline from ideation to release. The ADR system
is exemplary. The information hierarchy (status.md -> roadmap.md -> specific docs) works.
The CONTRIBUTING.md conventions (TL;DR, privacy, immutability rules) are thoughtful and
consistently applied.

The problem is not the pipeline — it's the artifact trail the pipeline leaves behind.
Design docs don't systematically connect to their reviews. Phase identifiers have fractured
into four overlapping numbering schemes. Claude Code's working plans are invisible to the
documentation system. The arch-adversary skill produces two fundamentally different artifact
types under one naming convention. And the roadmap has grown into a 550+ line document that
mixes stable architecture reference with rapidly-changing feature tracking.

These are problems of success — a fast-moving project outgrowing its initial conventions.
The solutions below are proportional to a solo developer workflow: naming conventions,
file placement rules, and Markdown indexes that Claude Code can maintain as part of existing
slash commands.

---

## 2. What Works Well

These strengths should be preserved in any changes.

### 2.1 ADR System

Zero naming inconsistencies across 14 ADRs. Strict sequential numbering (ADR-100 through
ADR-112), immutability enforced by convention, supersession workflow documented. The split
between bash-era (001-099) and Urd-era (100+) is clean. Every ADR has a date, explicit
dependencies, and a TL;DR. This is the project's gold standard for documentation.

### 2.2 Slash Command Pipeline

Nine commands forming a complete lifecycle:

```
/brainstorm -> /design -> /grill-me -> [build] -> /simplify -> arch-adversary -> /post-review -> /check -> /journal -> /commit-push-pr
```

Each command has a clear scope, defined inputs and outputs, and a known artifact location.
The pipeline emerged from deliberate skill evaluation (2026-03-30 mattpocock-skills-evaluation
journal) and subsequent refinement.

### 2.3 Information Hierarchy

The three-tier routing system works:

1. `status.md` (~60 lines, overwritten each session) — "what's happening now"
2. `roadmap.md` (feature tables with design + review links) — "what's planned"
3. Specific docs (ADRs, designs, reviews) — "the details"

A new session reads status.md first and follows links. This is explicitly codified in
CONTRIBUTING.md and consistently applied.

### 2.4 Document Conventions

- **TL;DR on every document >30 lines** — token-efficient scanning across 200+ files
- **Privacy model** — journals gitignored, tracked docs use `<user>` placeholders
- **Immutability tiers** — strict for ADRs, guideline for reports, mutable for ideas
- **Document templates** in CONTRIBUTING.md for each type

### 2.5 Feature Table Traceability

The Priority 6 feature table in roadmap.md is excellent:

```
| # | Feature | Status | Design | Review |
| 6-B | Transient immediate cleanup | Built | [design](link) | [review](link) |
```

This provides bidirectional traceability from roadmap to artifacts. The pattern should
be the universal standard, not an exception.

### 2.6 Commit and Branch Conventions

Consistent patterns across 159 commits and 60 PRs:
- Branches: `{type}/{slug}` (feat/, fix/, docs/, chore/, refactor/)
- Commits: `{type}: {description}` with module lists and co-author attribution
- PRs: feature bundles with CHANGELOG entries and status.md updates

---

## 3. Five Structural Problems

### 3.1 Review Naming: Seven Competing Patterns

The 77 files in `docs/99-reports/` use at least seven distinct naming patterns:

| Pattern | Example | Count |
|---------|---------|-------|
| `arch-adversary-{topic}` | `arch-adversary-phase35.md`, `arch-adversary-space-estimation.md` | 8 |
| `{topic}-design-review` | `heartbeat-design-review.md`, `sentinel-design-review.md` | 14 |
| `{topic}-implementation-review` | `awareness-model-implementation-review.md`, `urd-get-implementation-review.md` | 11 |
| `design-{letter}-review` | `design-b-review.md`, `design-h-review.md` | 8 |
| `design-{letter}-implementation-review` | `design-b-implementation-review.md`, `design-e-implementation-review.md` | 2 |
| `phase{N}-{type}-review` | `phase1-arch-review.md`, `phase2-adversary-review.md` | 7 |
| `{topic}-review` | `sentinel-session1-review.md`, `transient-snapshots-review.md` | 16 |
| Miscellaneous | `brainstorm-synthesis.md`, `design-evolution-analysis.md` | 5 |
| **Undated** | `adversary-review-phase-4-comprehensive.md` | 1 |

**Consequence:** Given a design doc like `2026-03-31-design-i-redundancy-recommendations.md`,
its review is `2026-03-31-design-i-review.md` — but only if you know the `design-{letter}-review`
pattern. For `2026-03-28-design-hardware-swap-defenses.md`, the review is
`2026-03-28-hardware-swap-defenses-design-review.md` — a different pattern entirely.
Neither Claude Code nor the user can reliably predict the review filename from the design
filename without consulting roadmap.md.

**How it happened:** The naming evolved across three periods:
1. **Phase 1-4 (March 22):** `phase{N}-{type}-review` and `arch-adversary-{topic}`
2. **Feature designs (March 23-29):** `{topic}-design-review` and `{topic}-implementation-review`
3. **Priority 6 batch (March 31):** `design-{letter}-review` (shortest form, for the 7 batch reviews)

Each pattern was internally consistent within its period, but the periods weren't coordinated.

### 3.2 Phase Numbering: Four Overlapping Schemes

The project uses at least four numbering schemes simultaneously:

| Scheme | Examples | Context |
|--------|----------|---------|
| Phase N | Phase 1, 2, 3, 3.5, 4 | Infrastructure (roadmap sections) |
| Priority N | Priority 5, 5.5, 6 | Feature arcs (roadmap sections) |
| N-{Letter} | 6-B, 6-E, 6-H, 6-I, 6-N, 6-O | Sub-features within Priority 6 |
| P{N}{letter} | P1, P2a, P2b, P2c, P4a, P4b, P4c, P6a, P6b | Phases within Priority 6 |

**Consequence:** "Phase 1" in the roadmap's infrastructure section and "P1" (Phase 1 of
Priority 6's Voice & UX arc) are different things that look similar. "6-I" and "P2b" are
peers in the same build queue but use completely different notation. A reader encountering
"P4c" in a commit message cannot decode it without roadmap.md context.

**How it happened:** Organic growth. Phases 1-4 were implementation stages. "Priority" was
introduced to distinguish planned feature arcs from completed infrastructure phases. The
letter designators (6-B, 6-I) came from brainstorm scoring. The P-prefixes came from
internal sequencing of the Voice & UX arc. Each made local sense at the time of introduction.

### 3.3 Claude Code Plans: Orphaned from the Documentation System

The `.claude/plans/` directory contains 53 files with auto-generated names:

```
eager-plotting-wigderson.md
elegant-soaring-meerkat.md
imperative-spinning-finch.md
```

These are Claude Code session artifacts. They carry no dates in their names (file timestamps
exist but aren't surfaced). They carry no topic slugs. They cannot be mapped to design docs,
journal entries, or PRs.

Meanwhile, `docs/97-plans/` — the project's designated plan directory — contains only 2 files:
- `2026-03-23-git-history-pii-scrub.md`
- `2026-03-29-hsd-a-drive-tokens-chain-health.md`

**Consequence:** The "real" implementation plans that guided 90%+ of sessions exist only
in `.claude/plans/` under opaque names. A future session cannot find the plan that preceded
a given implementation without reading all 53 files. The plans are not linked from journals,
designs, or reviews.

**How it happened:** `.claude/plans/` is Claude Code infrastructure with auto-naming. The
project's `/design` command writes to `docs/95-ideas/`, not to `docs/97-plans/`. The two
systems never connected.

### 3.4 arch-adversary: Two Roles, One Name

The arch-adversary skill reviews two fundamentally different artifacts:

1. **Design reviews** (pre-implementation): Evaluate a design proposal from `docs/95-ideas/`.
   Gate implementation decisions. Findings must be addressed before building.

2. **Implementation reviews** (post-implementation): Evaluate completed code on a feature
   branch. Validate that the design was realized correctly. Findings feed into `/post-review`.

Both are valuable. Both produce reports in `docs/99-reports/`. But they serve different
audiences at different points in the pipeline:

| Aspect | Design Review | Implementation Review |
|--------|--------------|----------------------|
| Input | Design document | Source code + tests |
| Timing | Before implementation | After implementation |
| Consumer | `/design` refinement, implementation planning | `/post-review` fixes |
| Shelf life | Superseded by implementation decisions | Superseded by next implementation |
| Failure mode | Design flaw passes through to code | Code quality issue ships |

**Consequence:** When `/post-review` looks for "the most recent arch-adversary review," it
cannot distinguish the design review (already addressed during implementation) from the
implementation review (the one with actionable findings). The naming doesn't help — both
might be `{topic}-review.md` or `arch-adversary-{topic}.md`.

**How it happened:** The arch-adversary skill was originally designed for design review
(the `/design` -> `arch-adversary` -> `/grill-me` pipeline). Implementation review was added
as the same skill applied to a different input, without updating the naming convention.

### 3.5 roadmap.md: Dual Duty

The roadmap file is currently 550+ lines serving two distinct purposes:

1. **Lines 1-477: Founding architecture reference** — context, decisions table, project
   structure, config schema, architecture principles, Prometheus metrics format, SQLite
   schema, CLI commands, Rust crates, implementation phases 1-5, migration strategy,
   testing prerequisites, verification plan, critical files reference.

2. **Lines 480+: Living feature tracker** — Priority 5-6 status, feature tables with
   design/review links, deferred items, completed work, tech debt.

The first half changes rarely. The second half changes every session.

**Consequence:** A session reading roadmap.md for "what to build next" consumes tokens on
architectural reference that CLAUDE.md already covers more authoritatively. The provenance
note at the top acknowledges this tension ("This document combines the original project
roadmap with the living feature tracker") but doesn't resolve it.

**How it happened:** The roadmap was written on day 1 (2026-03-22) as a comprehensive
project plan. It was the right document at the time. Ten days later, the stable architecture
content has migrated to CLAUDE.md and ADRs, but the original text remains in roadmap.md as
well, creating redundancy.

---

## 4. Proposed Solutions

### 4.1 Standardize Review Naming

**Convention:**
```
YYYY-MM-DD-review-{type}-{slug}.md
```

Where:
- `{type}` is one of: `design`, `impl`, `analysis`
- `{slug}` matches the source document's slug exactly

**Examples:**

| Source Document | Review |
|----------------|--------|
| `2026-03-31-design-b-transient-immediate-cleanup.md` | `2026-03-31-review-design-b-transient-immediate-cleanup.md` |
| Same feature after implementation | `2026-04-01-review-impl-b-transient-immediate-cleanup.md` |
| This document (process analysis) | `2026-04-01-review-analysis-workflow.md` (if reviewed) |

**Properties:**
- `review-` prefix makes all reviews greppable: `ls docs/99-reports/review-*`
- `design`/`impl`/`analysis` type tag is immediately visible
- Slug matching enables programmatic cross-referencing
- Date may differ between design review and impl review (correct — they happen on different days)

**Migration:** Forward-only. Existing 77 files keep their names. A note in CONTRIBUTING.md
states that pre-2026-04-01 reviews use legacy naming. The roadmap.md feature table already
provides traceability for historical reviews.

**Implementation:** Update CONTRIBUTING.md's file naming table and the arch-adversary skill
prompt to emit the new naming convention.

### 4.2 Adopt a Simple Work-Item Scheme Going Forward

The existing Phase/Priority/letter/P-prefix system is too entangled to rename retroactively.
The pragmatic approach has two parts:

**Part A: Document what exists.** Add a "Numbering Legend" section to roadmap.md that
explicitly maps every identifier:

```markdown
## Numbering Legend

| ID | Full Name | Status |
|----|-----------|--------|
| Phase 1-4 | Infrastructure phases | Complete |
| Phase 3.5 | Post-cutover hardening | Complete |
| P5 / Priority 5 | Sentinel daemon | Sessions 1-2 complete, 3-4 deferred |
| P5.5 / Priority 5.5 | Safety & Visibility | Complete |
| P6 / Priority 6 | Voice, UX & Redundancy | In progress |
| 6-B | Transient immediate cleanup | Complete |
| 6-E | Promise redundancy encoding | Complete |
| P1 | Vocabulary landing (within P6 arc) | Complete |
| P2a | `urd` default status (within P6 arc) | Complete |
| P2b | `urd doctor` (within P6 arc) | Complete |
| P2c | Shell completions (within P6 arc) | Complete |
| 6-I | Redundancy recommendations | Complete |
| 6-N | Retention policy preview | Complete |
| P4a+4b | Staleness escalation + suggestions | Complete |
| P4c | Mythic transitions | Complete |
| 6-O | Progressive disclosure | Next |
| P6a | ADR-110 enum rename | Next |
| P6b | Config Serialize refactor | Next |
| 6-H | Guided setup wizard | Next |
```

**Part B: Clean scheme for Priority 7+.** Use a simpler flat numbering within each
priority arc:

```
P7.1, P7.2, P7.3 ...
```

No letter designators. No nested phase numbers. Sub-items use `.N` notation. The priority
number provides the arc context; the decimal provides sequence.

**Trade-off:** This doesn't fix the existing inconsistency — it documents it and prevents
it from recurring. Retroactive renaming would require updating commit messages, journal
entries, design docs, review filenames, roadmap references, and status.md links. The cost
far exceeds the benefit.

### 4.3 Bridge Claude Code Plans into Documentation

The gap between `.claude/plans/` (53 ephemeral files) and `docs/97-plans/` (2 promoted
files) is structural. Not all plans warrant promotion — many are exploratory scaffolding
that becomes irrelevant after the session. The solution is selective promotion at a natural
workflow moment.

**Convention:** When `/design` produces a design document ready for implementation, the
plan's key decisions and approach should be captured in the design doc itself (in
`docs/95-ideas/`). The design doc IS the promoted plan.

This means `docs/97-plans/` becomes the home for plans that are NOT design docs — operational
plans like the PII scrub, incident response plans, or multi-session coordination plans. These
are rare (2 in 10 days), and the current ad-hoc promotion works fine for rare events.

**What changes:**
- `/journal` should note which `.claude/plans/` file was used during the session (the
  auto-generated name, for forensic traceability if needed later)
- Design docs should include a "Plan" or "Approach" section that captures the implementation
  strategy (most already do)
- No changes to `.claude/plans/` naming — it's Claude Code infrastructure, not project
  documentation

**Trade-off:** This accepts that `.claude/plans/` files are ephemeral and not worth
integrating. The design doc absorbs the plan's content. The plan file name is recorded
in the journal for forensic purposes only.

### 4.4 Distinguish Design Reviews from Implementation Reviews

Split the arch-adversary's dual role into clearly distinguished outputs. Two approaches:

**Option A: One skill, two modes.** The arch-adversary skill detects its context:
- If reviewing a document from `docs/95-ideas/`: design review mode, output named
  `review-design-{slug}.md`
- If reviewing code on a feature branch: implementation review mode, output named
  `review-impl-{slug}.md`
- If ambiguous: ask the user

**Option B: Two skills.** Split into `design-adversary` and `impl-adversary` with distinct
prompts optimized for their respective review types.

**Recommendation: Option A.** The review methodology is fundamentally the same (6-dimension
scorecard, severity ratings, catastrophic failure checklist). The difference is input and
output naming, not review philosophy. Two skills would duplicate 90% of the prompt content
and create a maintenance burden.

**Implementation:** Update the arch-adversary skill prompt to:
1. Detect whether the review target is a design doc or source code
2. Include the review type in the output filename
3. Adjust the review framing (design reviews assess feasibility and completeness;
   implementation reviews assess correctness and quality)

### 4.5 Split roadmap.md

Separate the stable architecture reference from the living feature tracker:

**New file:** `docs/00-foundation/architecture.md`
Contains: Context, Decisions table, Project Structure, Configuration Schema, Architecture
Key Design Principles, Prometheus Metrics, SQLite Schema, CLI Commands, Rust Crates,
Implementation Phases 1-5 (historical record), Migration Strategy, Testing Prerequisites,
Verification Plan, Critical Files Reference.

**Trimmed file:** `docs/96-project-supervisor/roadmap.md`
Contains: A provenance header linking to architecture.md, then the living content: Current
Priorities, feature tables, deferred items, completed work log, tech debt, numbering legend.

**Link updates needed:**
- `status.md` — add architecture.md to Key Links
- `CLAUDE.md` — the "Orient Yourself" section already points to status.md, which will
  link to both documents

**Trade-off:** This is a one-time structural change. The architecture content in
roadmap.md is largely duplicated by CLAUDE.md at this point — CLAUDE.md has the module
table, architectural invariants, config system notes, and error handling conventions.
The split clarifies what roadmap.md is: a tracker, not a reference.

**Token impact:** Positive. Sessions that need "what to build next" read a shorter
roadmap.md. Sessions that need architecture reference read CLAUDE.md (already loaded)
or architecture.md (stable, rarely needed beyond CLAUDE.md).

---

## 5. Artifact Registry

The Priority 6 feature table in roadmap.md already serves as a proto-registry:

```
| # | Feature | Status | Design | Review |
| 6-B | Transient immediate cleanup | Built | [design](link) | [review](link) |
```

The question is whether to formalize this into a separate registry document or keep it
embedded in roadmap.md.

**Recommendation: Keep it in roadmap.md.** The feature table works well where it is. A
separate registry would duplicate information and add another document to maintain. The
feature table already provides the cross-referencing the system needs — the problem was
never the registry's absence, but the naming inconsistency that makes the registry necessary.

If the review naming convention (Solution 4.1) is adopted, the need for a registry
diminishes: given a design doc slug, the review filename becomes predictable. The
feature table remains valuable for tracking status and effort estimates, but it no longer
needs to be the sole traceability mechanism.

**If the project grows beyond Priority 8-9**, a dedicated registry may become worthwhile.
The trigger would be: "the roadmap feature table exceeds 50 rows and spans multiple
priority arcs." At that point, extract it to `docs/96-project-supervisor/registry.md`.

---

## 6. Document Type Classification

This document is a **process analysis** — an analytical report examining project practices
and proposing improvements. It fits naturally in `docs/99-reports/` as a report subtype.

Under the proposed naming convention (Solution 4.1), future process analyses would be:
```
YYYY-MM-DD-review-analysis-{topic}.md
```

No new document type, directory, or template is needed. The existing report template in
CONTRIBUTING.md covers this use case. The `analysis` type tag in the naming convention
distinguishes process analyses from design and implementation reviews.

---

## 7. Implementation Priority

Ordered by impact-to-effort ratio:

| Priority | Solution | Effort | Impact | Dependencies |
|----------|----------|--------|--------|-------------|
| 1 | Review naming convention (4.1) | Low — update 2 files | High — all future reviews predictable | None |
| 2 | Review type distinction (4.4) | Low — update 1 skill | High — resolves arch-adversary ambiguity | Depends on 4.1 for naming |
| 3 | Numbering legend (4.2 Part A) | Low — add section to roadmap.md | Medium — documents existing system | None |
| 4 | Roadmap split (4.5) | Medium — one-time file surgery | Medium — reduces token waste | None |
| 5 | Clean numbering for P7+ (4.2 Part B) | Low — convention decision | Medium — prevents future inconsistency | Depends on 4.2A |
| 6 | Plan bridging (4.3) | Low — convention adjustment | Low — ephemeral plans are acceptable | None |

**Recommended first session:** Items 1-3 (review naming, type distinction, numbering legend).
These are all convention changes — updating CONTRIBUTING.md, the arch-adversary skill, and
adding a roadmap section. No code changes, no file renaming.

**Recommended second session:** Item 4 (roadmap split). This requires careful file surgery
to separate content without losing cross-references.

---

## 8. Risks and Constraints

### Over-engineering risk

This is a solo developer project with Claude Code as the primary development partner. Every
convention must be simple enough that Claude Code can follow it from a skill prompt, and
simple enough that the user can remember it without consulting documentation. The solutions
above are all naming conventions and Markdown structure — no external tools, no databases,
no CI integrations, no issue trackers.

### Migration burden

Retroactive renaming of 77 review files would require updating every reference in roadmap.md,
status.md, design docs, journal entries, and CONTRIBUTING.md examples. The benefit does not
justify the cost. Forward-only adoption is the right strategy — the existing roadmap feature
table provides traceability for historical artifacts.

### Token budget

Every new document competes for Claude Code's context window. The proposed changes should
be net-positive for token consumption:
- Splitting roadmap.md means sessions read a shorter tracker document
- Predictable review naming means fewer exploratory file reads to find the right review
- The numbering legend adds ~30 lines to roadmap.md but saves explanation time in every
  session that encounters an unfamiliar identifier

### Convention drift

The highest risk is that new conventions are defined but not followed. Mitigation: encode
the conventions in the skill prompts that produce the artifacts. If `/design` and
`arch-adversary` emit files with the correct naming, the convention is self-enforcing.
CONTRIBUTING.md is the reference; skill prompts are the enforcement mechanism.

---

## Appendix A: Complete Review Naming Audit

All 77 files in `docs/99-reports/` grouped by naming pattern:

**Pattern 1: `arch-adversary-{topic}`** (8 files)
```
2026-03-22-arch-adversary-phase35.md
2026-03-22-arch-adversary-phase4.md
2026-03-23-arch-adversary-proposal-review.md
2026-03-23-arch-adversary-space-estimation.md
2026-03-31-arch-adversary-phase4-voice-enrichment.md
2026-04-01-arch-adversary-6i-implementation-review.md
2026-04-01-arch-adversary-6n-2b-implementation-review.md
2026-04-01-arch-adversary-phase4ab-implementation-review.md
2026-04-01-arch-adversary-phase4c-transitions.md
```

**Pattern 2: `{topic}-design-review`** (14 files)
```
2026-03-23-awareness-model-design-review.md
2026-03-24-heartbeat-design-review.md
2026-03-24-presentation-layer-design-review.md
2026-03-24-urd-get-design-review.md
2026-03-24-uuid-fingerprinting-design-review.md
2026-03-26-backup-summary-design-review.md
2026-03-26-next-sessions-design-review.md
2026-03-26-protection-promises-design-review.md
2026-03-26-sentinel-design-review.md
2026-03-26-structured-errors-design-review.md
2026-03-27-sentinel-implementation-design-review.md
2026-03-27-sentinel-session2-design-review.md
2026-03-28-hardware-swap-defenses-design-review.md
2026-03-28-visual-feedback-model-design-review.md
```

**Pattern 3: `{topic}-implementation-review`** (11 files)
```
2026-03-22-phase2-implementation-review.md
2026-03-23-awareness-model-implementation-review.md
2026-03-24-heartbeat-implementation-review.md
2026-03-24-presentation-layer-implementation-review.md
2026-03-24-urd-get-implementation-review.md
2026-03-24-uuid-fingerprinting-implementation-review.md
2026-03-27-sentinel-session2-implementation-review.md
2026-03-29-hsd-a-implementation-review.md
2026-03-29-vfm-a-implementation-review.md
2026-03-31-design-b-implementation-review.md
2026-03-31-design-e-implementation-review.md
2026-04-01-phase1-vocabulary-landing-implementation-review.md
2026-04-01-phase2a-2c-implementation-review.md
```

**Pattern 4: `design-{letter}-review`** (8 files)
```
2026-03-31-design-b-review.md
2026-03-31-design-e-review.md
2026-03-31-design-h-review.md
2026-03-31-design-i-review.md
2026-03-31-design-n-review.md
2026-03-31-design-o-review.md
2026-03-31-design-spindle-review.md
2026-03-31-design-phase1-vocabulary-landing-review.md
2026-03-31-design-phase2-ux-commands-review.md
2026-03-31-design-phase3-advisory-retention-review.md
2026-03-31-design-phase5-progressive-disclosure-review.md
2026-03-31-design-phase6-protection-rename-wizard-review.md
```

**Pattern 5: `phase{N}-{type}-review`** (7 files)
```
2026-03-22-phase1-arch-review.md
2026-03-22-phase1-arch-review-v3.md
2026-03-22-phase1-hardening-review.md
2026-03-22-phase2-adversary-review.md
2026-03-22-phase2-plan-review.md
2026-03-22-phase3-adversary-review.md
2026-03-22-phase3-final-adversary-review.md
```

**Pattern 6: `{topic}-review`** (miscellaneous)
```
2026-03-23-brainstorm-synthesis-review.md
2026-03-23-post-cutover-features-review.md
2026-03-24-pre-cutover-testing-review.md
2026-03-26-test-strategy-review.md
2026-03-27-adr-suite-consistency-review.md
2026-03-27-sentinel-session1-review.md
2026-03-29-post-review-cross-drive-fallback-review.md
2026-03-29-progress-display-design-review.md
2026-03-29-sentinel-session3-implementation-review.md
2026-03-30-hsd-b-chain-break-detection-review.md
2026-03-30-transient-awareness-fix-review.md
2026-03-30-transient-snapshots-review.md
2026-03-30-ux1-plan-output-review.md
2026-03-30-ux2-estimated-sizes-review.md
2026-03-30-ux3-progress-display-review.md
2026-03-30-vfm-b-visual-state-review.md
2026-03-26-preflight-implementation-review.md
```

**Pattern 7: Non-review reports**
```
2026-03-23-brainstorm-synthesis.md
2026-03-23-proposal-progress-and-size-estimation.md
2026-03-23-vision-architecture-review.md
2026-03-24-design-evolution-analysis.md
```

**Undated:**
```
adversary-review-phase-4-comprehensive.md
```

---

## Appendix B: Design-to-Review Mapping Examples

Illustrating the naming inconsistency across periods:

| Design Document | Actual Review Name | Predicted Name (if consistent) |
|----------------|-------------------|-------------------------------|
| `design-hardware-swap-defenses.md` | `hardware-swap-defenses-design-review.md` | `review-design-hardware-swap-defenses.md` |
| `design-b-transient-immediate-cleanup.md` | `design-b-review.md` | `review-design-b-transient-immediate-cleanup.md` |
| `design-i-redundancy-recommendations.md` | `design-i-review.md` | `review-design-i-redundancy-recommendations.md` |
| `design-phase1-vocabulary-landing.md` | `design-phase1-vocabulary-landing-review.md` | `review-design-phase1-vocabulary-landing.md` |
| `design-sentinel.md` | `sentinel-design-review.md` | `review-design-sentinel.md` |

The "Predicted Name" column shows what the proposed convention (Solution 4.1) would produce —
consistent, greppable, and mechanically derivable from the source document name.
