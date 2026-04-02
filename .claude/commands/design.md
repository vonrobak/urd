Decompose an idea or feature into an architecturally sound, implementable design.

The design doc is a durable artifact — it will be read by `/grill-me`, `/prepare`, and
`arch-adversary` across separate sessions with no shared conversation context. Every
design must be self-contained: a reader with access to CLAUDE.md and the codebase, but
no memory of how this design was discussed, should understand what's being built and why.

## Before designing

Read CLAUDE.md (module table, invariants), `docs/96-project-supervisor/status.md` (current
state), and any relevant ideas in `docs/95-ideas/`. Understand what exists before proposing
what to build.

**Verify the problem exists.** Before designing a solution, read the relevant source modules
to confirm the problem isn't already solved or partially addressed. Brainstorm ideas
especially can be stale — the codebase may have evolved since the idea was written. If the
problem is already handled, say so and skip the design. A "this is already solved" finding
is more valuable than a design for something that doesn't need building.

**When the input is a brainstorm with multiple ideas:** Triage first, then design. Read
each idea and evaluate it against the current codebase:
- Which ideas are already solved? (Skip with a note explaining what exists)
- Which are mature enough for design? (Design these)
- Which should be deferred? (Note why — dependency, prerequisite, scope)
- Can related ideas be grouped into one design? (If so, explain the grouping and provide
  a clean partial-delivery boundary so each piece can stand alone)

Produce one design doc per coherent scope. Don't force unrelated ideas into a single design.

## UPI Assignment

Every design gets a Unique Project Identifier (UPI). Before writing the design doc:

1. Read `docs/96-project-supervisor/registry.md`
2. Find the highest existing UPI group number
3. Assign the next number: `NNN` for standalone items, `NNN-a` for the first sub-item
   in a group (user specifies if this is a sub-item of an existing group)
4. After writing the design doc, add a row to registry.md (newest at top) with UPI, title,
   and design doc link. Fill other columns with `-`.

## Core job

1. **Decompose to module level.** Which existing modules are affected? What new
   types/traits/enums? What's the data flow? What's the test strategy? If an operation
   doesn't fit cleanly into one module per CLAUDE.md's table, that's a design signal.

2. **Identify architectural gates.** Features that introduce new public contracts, change
   existing contract meanings, or cross module boundaries need an ADR before code. Flag
   explicitly: "Gate: ADR needed for X."

3. **Calibrate effort to completed work.** Use status.md as reference:
   - UUID fingerprinting: 1 module extended, 10 tests, one session
   - Awareness model: 1 new module, 24 tests, one session
   - `urd get`: 1 new command, 19 tests, one session

4. **The 9 founding ADRs (ADR-100 through ADR-109) are hard constraints.** If the design
   benefits from relaxing one, that's an ADR-gate finding, not a design assumption.

5. **Sequence for risk.** High-risk, assumption-heavy pieces first so bugs surface early.
   The pre-cutover hardening found 3 bugs sharing one root cause — sequencing should expose
   those patterns.

6. **Leave room for stress-testing.** State alternatives you rejected, assumptions you're
   making, and decision branches that need resolving. `/grill-me` will push on these — make
   it productive by being explicit.

## Output

For features affecting >3 files or introducing new modules: write a design proposal to
`docs/95-ideas/YYYY-MM-DD-design-{UPI}-{slug}.md` with YAML frontmatter and status `proposed`.

**Frontmatter format:**

```yaml
---
upi: "001-a"
status: proposed
date: YYYY-MM-DD
---
```

For smaller features: deliver the decomposition conversationally with module mapping, effort
estimate, and any gate flags. Still assign a UPI and add a registry row.

### Design document structure

Use this skeleton. Include every section — thin sections are fine, but missing sections
leave gaps that `/grill-me` and `/prepare` will struggle with.

```markdown
# Design: {Title} (UPI {NNN})

> **TL;DR:** {2-3 sentences: what's being built and why it matters}

## Problem
Why this feature exists. What's broken, missing, or suboptimal today.
Ground it in concrete user experience, not abstract architecture.

## Proposed Design
The solution at module level. For each affected module:
- What changes (types, functions, data flow)
- Why this module and not another (reference CLAUDE.md's table)
- Test strategy for this module's changes

## Module Map
Table or list: module → changes → test strategy. Makes scope visible at a glance.

## Effort Estimate
Session count, calibrated against completed work from status.md.

## Sequencing
What order to implement, and why. Dependencies, risk-first ordering.

## Architectural Gates
ADRs needed, contracts affected, or "None" if clean.

## Rejected Alternatives
What you considered and why you chose differently. Be specific — "considered X but
rejected because Y" gives /grill-me something to push on. If the rejection reasoning
is weak, /grill-me will find it.

## Assumptions
What you're taking for granted. Each assumption is a risk — if it's wrong, the design
may need revision. Call them out so /grill-me can verify or challenge them.

## Open Questions
Decision branches that need resolving before implementation. Frame each as a choice
with alternatives, not just an uncertainty:

Good: "Should pin files be updated before or after send? Option A (before): guarantees
protection but creates orphan pins on failure. Option B (after): cleaner but leaves a
window where retention could delete the parent."

Weak: "Need to figure out pin file timing."

These are the primary input to `/grill-me` — the clearer the decision branches, the
more productive the stress-test session. This format matters even for small features:
`/grill-me` needs alternatives to push on regardless of scope.
```

The exact sections can flex for the feature — add sections where the design needs them,
but don't drop the core ones (Problem, Proposed Design, Module Map, Rejected Alternatives,
Assumptions, Open Questions). The downstream consumers depend on them.

## Arguments

$ARGUMENTS — The idea or feature to design. Can be a reference to an idea doc, a feature
name from status.md, or a free-form description.
