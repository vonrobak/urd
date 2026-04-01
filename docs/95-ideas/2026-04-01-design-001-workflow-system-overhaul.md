---
upi: "001"
status: proposed
date: 2026-04-01
---

# Design: Workflow System Overhaul

> **TL;DR:** Introduce a Unique Project Identifier (UPI) system that threads through all
> workflow artifacts, standardize review naming, create a project registry, add a `/sequence`
> skill to the pipeline, and update existing skills to produce consistently named and
> cross-referenced documentation. This is a documentation and workflow change — no Rust code
> is modified.

**Depends on:** No ADR gates. This changes documentation conventions and skill prompts only.

## Problem

After 10 days of development (v0.0.0 to v0.7.0), the workflow pipeline (`/brainstorm` →
`/design` → `arch-adversary` → `/post-review` → `/journal` → `/commit-push-pr`) is
productive, but the artifact management system has accumulated five structural problems:

1. **77 review files use 7+ naming patterns** — no reliable way to find a review given its
   design doc
2. **Phase numbering uses 4+ overlapping schemes** — Phase 1-4, Priority 5/5.5/6, 6-B/E/H,
   P1/P2a/P6a
3. **53 Claude Code plan files are orphaned** — auto-generated names, no link to documentation
4. **arch-adversary produces design and implementation reviews** under inconsistent names
5. **roadmap.md is 550+ lines** mixing stable architecture reference with living feature tracker

Evidence and full analysis: `docs/99-reports/2026-04-01-workflow-and-project-management-analysis.md`
and `docs/99-reports/2026-04-01-ccpm-evaluation-and-workflow-comparison.md`.

## Proposed Design

### The UPI System

Every significant unit of work gets a **Unique Project Identifier** assigned at `/design` time.

**Format:** `NNN-a` where:
- `NNN` — opaque, sequential group number (starting at `001`)
- `-a` — sequential letter suffix for sub-items within a group
- Standalone items use `NNN` with no letter suffix

**Properties:**
- Assigned by `/design` (reads registry.md for next number)
- Threads through all artifacts: design doc, design review, adversary review, journal, PR
- The description slug in the filename carries the human-readable meaning — the UPI provides
  machine-linkable uniqueness

**Registry:** `docs/96-project-supervisor/registry.md` — a minimal lookup table, newest
entries at top. No status tracking, no grouping, no sequencing.

```markdown
# Registry

| UPI | Title | Design | Design Review | Adversary Review | PR | GH# |
|-----|-------|--------|---------------|------------------|----|-----|
| 001 | Workflow system overhaul | [design](link) | - | - | - | - |
```

### Artifact Naming Conventions

All artifacts follow `YYYY-MM-DD-{type}-{UPI}-{slug}.md` where the type prefix identifies
the artifact's role in the pipeline:

| Artifact | Type prefix | Location | Example |
|----------|------------|----------|---------|
| Brainstorm | `brainstorm-` | `docs/95-ideas/` | `2026-04-01-brainstorm-progressive-ux.md` |
| Design doc | `design-{UPI}-` | `docs/95-ideas/` | `2026-04-01-design-001-workflow-system-overhaul.md` |
| Design review | `design-review-{UPI}-` | `docs/99-reports/` | `2026-04-01-design-review-001-workflow-system-overhaul.md` |
| Adversary review | `review-adversary-{UPI}-` | `docs/99-reports/` | `2026-04-02-review-adversary-001-workflow-system-overhaul.md` |
| Process analysis | `review-analysis-` | `docs/99-reports/` | `2026-04-01-review-analysis-workflow.md` |

**Brainstorms do not get UPIs.** They are pre-design artifacts. The UPI is born when
`/design` structures an idea into an implementable proposal.

**Slug derivation:** Stripped — no duplication of the type prefix. Given a design doc
`design-001-workflow-system-overhaul`, the design review slug is `001-workflow-system-overhaul`
(the UPI + description, without repeating `design`).

### Updated Pipeline

```
/brainstorm → /design → design-review → /grill-me → /sequence → [plan+build] →
  /simplify → arch-adversary → /post-review → /check → /journal → /commit-push-pr
```

Changes from current pipeline:
- `design-review` — explicit slot for arch-adversary reviewing the design doc (was implicit)
- `/sequence` — new skill: orders reviewed designs for implementation
- `[plan+build]` — replaces `[build]` to acknowledge planning and execution are intertwined

### Document Responsibility Split

| Document | Tracks | Updated by |
|----------|--------|-----------|
| `registry.md` | UPI → artifact links (lookup only) | `/design` (new rows), `/journal` (fill in links) |
| `roadmap.md` | Strategy and sequencing (~80 lines) | `/sequence` (revised active arc) |
| `status.md` | Current state (~60 lines) | `/journal` (overwritten each session) |

### YAML Frontmatter

Design docs and reviews adopt YAML frontmatter for machine-parseable metadata:

```yaml
---
upi: "001-a"
status: proposed
date: 2026-04-01
---
```

**Status vocabulary (controlled set):**
- `raw` — brainstorm output, not yet structured
- `proposed` — structured design, ready for review
- `reviewed` — design review complete
- `promoted` — sequenced for implementation
- `abandoned` — explicitly not proceeding

The `**Date:**` and `**Status:**` bold-field lines in the body are dropped — that
information lives in frontmatter. TL;DR remains the first visible content after frontmatter.

**Forward-only adoption.** Existing docs keep their current format.

### New roadmap.md

The current roadmap.md is archived to
`docs/90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md` (immutable).

New roadmap.md is ~80 lines with three sections:

1. **Active Arc** (~15 lines) — what's being built, why, sequencing rationale
2. **Horizon** (~20 lines) — 2-3 future arcs with one-line descriptions
3. **Strategic Context** (~20 lines) — tech debt that gates features, architectural
   constraints, deferred decisions and why

## Files Affected

### New files

| File | Purpose |
|------|---------|
| `docs/96-project-supervisor/registry.md` | UPI lookup table |
| `.claude/commands/sequence.md` | New `/sequence` skill |
| `docs/90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md` | Archived old roadmap |

### Modified files

| File | Change |
|------|--------|
| `CONTRIBUTING.md` | File naming table (add UPI patterns, review naming), design proposal template (YAML frontmatter, drop bold-field metadata), status vocabulary, add registry.md to directory details |
| `CLAUDE.md` | Pipeline description, add registry.md to "Orient Yourself" and project state sections |
| `.claude/commands/design.md` | UPI assignment logic, new filename format, registry row creation |
| `.claude/skills/arch-adversary/SKILL.md` | Output filename convention with UPI, registry link update |
| `.claude/commands/journal.md` | Optional `**Plan file:**` metadata line, registry link updates |
| `docs/96-project-supervisor/roadmap.md` | Rewritten from scratch (~80 lines) |
| `docs/96-project-supervisor/status.md` | Updated key links (add registry.md) |

### Not modified

- No Rust source code
- No ADRs (no architectural contracts changed)
- No existing design docs or reviews (forward-only adoption)
- `docs/97-plans/` — stays as-is (operational plans that aren't design docs)

## Skill Change Specifications

### `/design` (`.claude/commands/design.md`)

Add to the skill prompt:

1. **UPI assignment:** Read `docs/96-project-supervisor/registry.md`. Find the highest
   existing UPI group number. Assign next number (or next letter within a group if the
   user specifies this is a sub-item of an existing group).

2. **Output filename:** `docs/95-ideas/YYYY-MM-DD-design-{UPI}-{slug}.md`

3. **Registry update:** Append a row to registry.md with UPI, title, and design doc link.
   Other columns filled with `-`.

4. **Frontmatter:** Add YAML frontmatter block with `upi`, `status: proposed`, `date`.

### `arch-adversary` (`~/.claude/skills/arch-adversary/SKILL.md`)

Add to the "Report output" section:

1. **Design review mode:** Output to
   `docs/99-reports/YYYY-MM-DD-design-review-{UPI}-{slug}.md`

2. **Implementation review mode:** Output to
   `docs/99-reports/YYYY-MM-DD-review-adversary-{UPI}-{slug}.md`

3. **Registry update:** After writing the report, update the corresponding UPI row in
   registry.md — fill in the Design Review or Adversary Review column with a link.

4. **UPI detection:** Read the design doc or identify the feature being reviewed to
   determine the UPI. If not identifiable, ask the user.

5. **Frontmatter:** Add YAML frontmatter block with `upi`, `date`.

### `/sequence` (`.claude/commands/sequence.md` — new)

```
Propose implementation sequencing for reviewed designs.

## Inputs
- docs/96-project-supervisor/registry.md — find designs with completed design reviews
- docs/96-project-supervisor/roadmap.md — understand current active arc and horizon
- docs/96-project-supervisor/status.md — understand deployed state and in-progress work
- Relevant design docs and their design reviews — for effort, dependencies, ADR gates

## Core job
1. Identify which designs are ready for implementation (have design reviews in registry)
2. Analyze decision trees: if X requires Y, sequence Y first
3. Analyze dependencies: shared modules, prerequisite refactors, ADR gates
4. Group by effort clustering: small items touching the same modules batch well
5. Sequence for risk: high-uncertainty items first to surface problems early

## Output
Revised docs/96-project-supervisor/roadmap.md with updated:
- Active Arc — what to build next, in what order, with rationale
- Horizon — what comes after, with one-line descriptions

The user drives prioritization decisions. The skill does the analytical work of
identifying dependencies, decision trees, and optimal ordering.
```

### `/journal` (`.claude/commands/journal.md`)

Two additions:

1. **Plan file metadata:** Add optional line to journal template:
   ```
   **Plan file:** `{.claude/plans/filename.md if used}`
   ```
   Filled from conversation context. Omitted if no plan was used.

2. **Registry updates:** When the journal records completion of a design review, adversary
   review, or PR merge, update the corresponding link in registry.md.

## Invariants

1. **UPI is assigned once, at `/design` time.** It never changes. If a design is abandoned
   and restarted, it gets a new UPI.
2. **Registry is a lookup table.** It does not track status, sequencing, or grouping.
   Those responsibilities belong to status.md and roadmap.md respectively.
3. **Forward-only adoption.** Existing documents keep their current naming and format.
   No retroactive renaming, no backfill.
4. **Brainstorms have no UPI.** The identifier is born when an idea becomes a structured
   design.
5. **The slug is stripped.** No redundant prefixes in filenames. The type prefix and
   slug are complementary, not overlapping.

## Rejected Alternatives

1. **CCPM's full PRD → Epic → Task pipeline.** Too heavyweight for solo development.
   Adds document types (PRDs, epics, task files) without proportional benefit. The design
   doc absorbs what a PRD would contain.

2. **GitHub Issues as primary work tracker.** Deferred to Tier 3 as a gradual habit
   adoption. The registry + roadmap + status.md triad covers tracking needs without
   external dependencies.

3. **Backfilling historical items with UPIs.** The archived roadmap's feature table
   provides historical traceability. Retroactive numbering would be busywork producing
   a table nobody consults.

4. **Task decomposition in `/design` output.** The sequencing and planning phases handle
   this. Design docs describe what to build and why, not the step-by-step execution order.

5. **Separate architecture.md document.** Most content is already in CLAUDE.md, ADRs, and
   operating-urd.md. A future documentation effort will address module guides and
   architecture principles systematically.

6. **Multiple tracking scripts.** Only `validate.sh` solves a problem the new document
   system doesn't already address. Status and next-up queries are cheap with an 80-line
   roadmap.

## Tiered Implementation

### Tier 1 — Convention and prompt changes (one session)

Execute in this order (dependency-driven):

1. Create `docs/96-project-supervisor/registry.md` (header row only — unblocks everything)
2. Update `CONTRIBUTING.md` (naming conventions, frontmatter, status vocabulary)
3. Update `.claude/commands/design.md` (UPI assignment, filename, registry row)
4. Update `~/.claude/skills/arch-adversary/SKILL.md` (output naming, registry update)
5. Create `.claude/commands/sequence.md` (new skill)
6. Update `.claude/commands/journal.md` (plan file metadata, registry updates)
7. Update `CLAUDE.md` (pipeline description)

Items 2-6 can be parallelized after item 1.

### Tier 2 — New artifacts (following session)

1. Archive current roadmap.md to `docs/90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md`
2. Write new roadmap.md from scratch (~80 lines)
3. Update status.md key links
4. Write `validate.sh` (checks: registry link consistency, broken links, undated files)

### Tier 3 — Habit adoption (gradual)

- GitHub Issues: one issue per work item, labeled by arc, linked via `Fixes #N` in PRs
- GH# column in registry fills in as issues are created

## Effort Estimate

| Tier | Work | Estimate |
|------|------|----------|
| Tier 1 | 7 file edits/creations, all convention/prompt changes | 1 session |
| Tier 2 | Roadmap archive + rewrite, validate.sh | 1 session |
| Tier 3 | Habit adoption | Ongoing, no dedicated session |

No tests — this is documentation and workflow infrastructure, not Rust code.

## Ready for Review

This design should be evaluated for:

1. **Completeness:** Does the UPI system cover all artifact types in the pipeline? Are
   there artifacts that should carry a UPI but don't?
2. **Consistency:** Do the naming conventions produce unambiguous, predictable filenames
   in all cases? Walk through 3-4 hypothetical features.
3. **Sustainability:** Will the registry stay maintainable as the project grows? At what
   scale does it need revision?
4. **Skill prompt feasibility:** Are the skill changes specific enough that Claude Code
   will follow them consistently across sessions?
5. **Migration risk:** Does forward-only adoption leave any gaps where old and new
   conventions interact poorly?
