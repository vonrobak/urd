Generate ideas through divergent thinking. Expand the possibility space — convergence comes later.

## Rules

1. **Generate first, evaluate never.** Do not score, rank, or filter ideas during generation. Quantitative scoring by LLMs produces systematic errors (75% arithmetic mistakes in the 2026-03-23 synthesis). If the user asks for synthesis afterward, use qualitative tier placement only: Build soon / Design first / Explore further / Park — with one sentence of reasoning per placement. Account for every idea (no silent drops).

2. **Use the project vision as generative prompts.** CLAUDE.md's two north stars — "does it make data safer?" and "does it reduce attention on backups?" — are springboards, not filters. Ask "what *else* could make data safer?" Don't kill ideas that fail both tests — they may inspire ideas that pass.

3. **Reference real architecture.** Ideas that name real modules, traits, and types are more useful downstream. Read CLAUDE.md's module table. "What if `awareness.rs` tracked drive temperature?" beats "what about drive health?"

4. **"Data safety" is always a lens.** For a backup tool, every idea should be evaluated for whether it makes data safer, less safe, or is neutral. The 2026-03-23 synthesis omitted this entirely.

5. **Include uncomfortable ideas.** At least 2-3 ideas that feel too ambitious. The solutions-architect will tell you what's infeasible.

## Output

Write to `docs/95-ideas/YYYY-MM-DD-slug.md` per CONTRIBUTING.md idea template. Status: `raw`. End with a **Handoff to Architecture** section listing 3-5 most promising ideas with one sentence each on why they deserve deeper analysis.

## Arguments

$ARGUMENTS — Optional: topic to brainstorm about. If empty, read status.md for current priorities and explore open areas.
