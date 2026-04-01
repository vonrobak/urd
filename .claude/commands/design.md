Decompose an idea or feature into an architecturally sound, implementable design.

## Before designing

Read CLAUDE.md (module table, invariants), `docs/96-project-supervisor/status.md` (current state), and any relevant ideas in `docs/95-ideas/`. Understand what exists before proposing what to build.

## UPI Assignment

Every design gets a Unique Project Identifier (UPI). Before writing the design doc:

1. Read `docs/96-project-supervisor/registry.md`
2. Find the highest existing UPI group number
3. Assign the next number: `NNN` for standalone items, `NNN-a` for the first sub-item
   in a group (user specifies if this is a sub-item of an existing group)
4. After writing the design doc, add a row to registry.md (newest at top) with UPI, title,
   and design doc link. Fill other columns with `-`.

## Core job

1. **Decompose to module level.** Which existing modules are affected? What new types/traits/enums? What's the data flow? What's the test strategy? If an operation doesn't fit cleanly into one module per CLAUDE.md's table, that's a design signal.

2. **Identify architectural gates.** Features that introduce new public contracts, change existing contract meanings, or cross module boundaries need an ADR before code. Flag explicitly: "Gate: ADR needed for X."

3. **Calibrate effort to completed work.** Use status.md as reference:
   - UUID fingerprinting: 1 module extended, 10 tests, one session
   - Awareness model: 1 new module, 24 tests, one session
   - `urd get`: 1 new command, 19 tests, one session

4. **The 9 founding ADRs (ADR-100 through ADR-109) are hard constraints.** If the design benefits from relaxing one, that's an ADR-gate finding, not a design assumption.

5. **Sequence for risk.** High-risk, assumption-heavy pieces first so bugs surface early. The pre-cutover hardening found 3 bugs sharing one root cause — sequencing should expose those patterns.

6. **Leave room for review.** State alternatives you rejected and assumptions you're making. The arch-adversary will push against these — make it productive by being explicit.

## Output

For features affecting >3 files or introducing new modules: write a design proposal to
`docs/95-ideas/YYYY-MM-DD-design-{UPI}-{slug}.md` with YAML frontmatter and status `proposed`.
Include a **Ready for Review** section telling the arch-adversary what to focus on.

**Frontmatter format:**

```yaml
---
upi: "001-a"
status: proposed
date: YYYY-MM-DD
---
```

For smaller features: deliver the decomposition conversationally with module mapping, effort estimate, and any gate flags. Still assign a UPI and add a registry row.

## Arguments

$ARGUMENTS — The idea or feature to design. Can be a reference to an idea doc, a feature name from status.md, or a free-form description.
