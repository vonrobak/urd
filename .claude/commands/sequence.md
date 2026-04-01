Propose implementation sequencing for reviewed designs.

## Inputs

Read these before sequencing:

1. `docs/96-project-supervisor/registry.md` — find designs with completed design reviews
2. `docs/96-project-supervisor/roadmap.md` — current active arc and horizon
3. `docs/96-project-supervisor/status.md` — deployed state and in-progress work
4. Relevant design docs and their design reviews — for effort estimates, module
   dependencies, ADR gates, and review findings

## Core job

1. **Identify ready designs.** Scan registry.md for UPIs that have a design review link
   but are not yet in the roadmap's active arc.

2. **Map decision trees.** If feature X requires ADR change Y, that's a prerequisite gate.
   If feature A's design review flagged a dependency on feature B, sequence B first. Make
   the decision tree explicit — draw out the branches and resolve them.

3. **Map dependencies.** Shared modules, prerequisite refactors, features that produce types
   or traits consumed by later features. Two features touching the same module should be
   sequenced to avoid rework, not parallelized.

4. **Group by effort clustering.** Small items touching the same modules batch well into
   a single session. Large items that span multiple modules may need splitting across sessions.

5. **Sequence for risk.** High-uncertainty items first to surface problems early. Features
   with clear designs and predictable scope can go later. If a design review flagged
   significant concerns, those should be resolved earlier rather than later.

## Output

Revised `docs/96-project-supervisor/roadmap.md` with updated:

- **Active Arc** — what to build next, in what order, with sequencing rationale. Reference
  UPIs from registry.md. Include effort estimates from design docs.
- **Horizon** — what comes after the active arc. 2-3 future arcs with one-line descriptions.

Do not modify registry.md — it is a lookup table, not a sequencing tool.

The user drives prioritization decisions (what matters most). This skill does the analytical
work of identifying dependencies, decision trees, and optimal ordering within those priorities.

## Arguments

$ARGUMENTS — Optional: specific designs to sequence, or a priority constraint. If empty, sequence all reviewed-but-unscheduled designs from registry.md.
