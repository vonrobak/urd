Interview me relentlessly about every aspect of this plan or design until we reach shared understanding.

Walk down each branch of the decision tree, resolving dependencies between decisions one by one. Earlier decisions constrain later ones — resolve upstream branches before opening downstream ones.

## Rules

1. **One question at a time.** Do not batch questions. Each answer may change what you ask next.

2. **Provide your recommended answer.** For each question, state what you think the answer should be and why — grounded in CLAUDE.md's architectural invariants, the module responsibility table, and the current state in status.md. The user can accept, reject, or refine.

3. **Check the codebase before asking.** If a question can be answered by reading code, reading docs, or checking existing patterns — do that instead of asking. Only ask the user about decisions that require judgment, not facts that are already recorded.

4. **Track resolved branches.** Maintain a running summary of resolved decisions so we can see the tree take shape. When a branch is resolved, name it and move on.

5. **Stop when the tree is resolved.** When all branches that matter for the next step (implementation or design doc) are resolved, summarize the full decision tree and identify the output artifact — whether that's a `/design` doc, an update to an existing plan, or direct implementation.

## What to probe

- Assumptions the plan makes about existing code (verify them)
- Module boundaries — does this respect CLAUDE.md's responsibility table?
- ADR gates — does this need a new ADR or touch an existing contract?
- Error modes — what happens when this fails at 3am unattended?
- Sequencing — does the order of implementation matter? What depends on what?
- Scope — what's in, what's explicitly out, and is the boundary clean?

## Output

The interview itself is the output. The resolved decision tree becomes input to whatever comes next in the pipeline — typically `/design` (if the idea is still forming) or direct implementation (if the design is already written and we're stress-testing it).

## Arguments

$ARGUMENTS — The plan, design, or idea to grill. Can be a reference to a doc in docs/95-ideas/, a feature name from status.md, or a free-form description of what you're considering.
