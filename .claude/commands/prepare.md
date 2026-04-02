Produce a concrete implementation plan from a reviewed design. Do NOT write any production code.

## Before planning

1. Read the design doc provided as argument (or the most recent `proposed` design in `docs/95-ideas/`)
2. Read CLAUDE.md — module table, invariants, coding conventions
3. Read `docs/96-project-supervisor/status.md` — current state, what exists
4. If a `/grill-me` session preceded this, review the resolved decision tree

## Core job

Translate the design into an executable plan. The design says *what* to build; the plan says
*how*, in what order, touching which files, with what tests.

1. **Identify every file that will be touched.** Read each one. Do not plan changes to code
   you haven't read. For new files, identify where they fit in the module structure.

2. **Map changes to modules.** Each change must respect CLAUDE.md's module responsibility
   table. If a planned change crosses module boundaries, that's a plan signal — flag it and
   resolve it now, not during build.

3. **Sequence the implementation.** Order steps so that:
   - Dependencies are built before dependents
   - High-risk, assumption-heavy pieces come first (surface bugs early)
   - Tests accompany each step (vertical slicing, not "all tests at the end")

4. **Define the test strategy.** For each step: what tests, what mocks, what edge cases.
   Reference existing test patterns in the codebase where applicable.

5. **Flag ADR gates.** If any step requires a new ADR or modifies an existing contract,
   make this explicit. ADRs must be written before the code that depends on them.

6. **Estimate scope.** Use status.md calibration points:
   - UUID fingerprinting: 1 module, 10 tests, one session
   - Awareness model: 1 new module, 24 tests, one session
   - `urd get`: 1 new command, 19 tests, one session

## What NOT to do

- **Do not write code.** Not even "rough sketches" or "example implementations." The plan
  describes what to change; the build phase writes it.
- **Do not skip reading files.** Every file in the plan must be read first. Planning changes
  to unread code produces plans that don't survive contact with reality.
- **Do not merge plan and build.** If you feel the urge to "just quickly implement this part,"
  stop. That impulse is the plan telling you it's not concrete enough — add more detail instead.

## Output

Write the plan to `docs/97-plans/YYYY-MM-DD-plan-{UPI}-{slug}.md` (gitignored — local only).
Then enter plan mode so the user can review and refine interactively.

The plan document should contain:

- **Steps** — numbered, ordered, each with: files touched, what changes, tests to write
- **Risk flags** — assumptions, ADR gates, areas of uncertainty
- **Scope estimate** — session count based on calibration points above
- **Ready signal** — explicit statement: "This plan is ready for arch-adversary review"

The plan stays in plan mode until the user promotes it. Next step: `arch-adversary` reviews
the plan document. After review and `/post-review` revision, the user triggers the build.

## Arguments

$ARGUMENTS — The design doc to plan from. Can be a path to a design in `docs/95-ideas/`,
a UPI from registry.md, or a free-form description for smaller work that skipped formal design.
