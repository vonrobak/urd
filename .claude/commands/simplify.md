Review changed code for reuse, quality, and efficiency, then fix any issues found.

This is a focused simplification pass. The question is not "is this correct?" (that's arch-adversary's job) but "could this be simpler while remaining correct?"

## When to run

After building a feature, before `/check`. The build executes a plan already reviewed by arch-adversary — this pass catches implementation-level complexity that crept in during coding.

## What to examine

Focus on code that was just built or modified — not the whole codebase. Use `git diff` or the current branch's changes as scope.

## The simplification lens

For each change, ask these questions in order:

1. **Module boundaries.** Does the new code respect CLAUDE.md's module responsibility table? If a function does something that belongs in another module, move it — don't add an abstraction to bridge them.

2. **Abstraction audit.** For each abstraction introduced:
   - Can you name a concrete scenario where it pays for itself?
   - If you deleted it and duplicated the code, would the system be worse?
   - Three similar functions are better than one premature generic.

3. **Type-level simplification.** Rust-specific:
   - Generic type parameters that could be concrete
   - Trait bounds that could be simpler or removed
   - Lifetime annotations that could be elided
   - Newtypes or wrappers that aren't earning their keep

4. **Control flow.** Can nesting be reduced? Can early returns replace deep `if/else` chains? Can a `match` replace a series of `if let`? A 50-line function readable top-to-bottom beats five 10-line functions scattered across the file.

5. **Test simplification.** Tests accumulate complexity too. Are test helpers obscuring what's being tested? Can mock configurations be simpler? Are there redundant tests covering the same path?

## Principles

- **Clarity over brevity.** Don't make code shorter; make it clearer.
- **Explicit over clever.** A readable approach beats an elegant one.
- **Preserve behavior.** This is a refactoring pass, not a feature pass. No functional changes.
- **Earn your keep.** Every abstraction, indirection, and generic parameter must justify itself with a concrete scenario — not a hypothetical future need.

## Output

Fix issues directly in the code. After simplification, run `/check` to verify nothing broke. Report what you changed and why in a brief summary — no formal report needed.

## Arguments

$ARGUMENTS — Optional: specific files or modules to focus on. If empty, examines all changes on the current branch.
