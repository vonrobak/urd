Write a session journal and update the project status document. Two outputs from one invocation.

## Output 1: Journal entry

Write to `docs/98-journals/YYYY-MM-DD-slug.md`. This is gitignored (private, local only).

**Auto-fill metadata:**
- Date: today's date
- Base commit: `git rev-parse --short HEAD`
- Slug: derived from $ARGUMENTS or inferred from what was done this session

**Template:**

```markdown
# Session Journal: {Title}

> **TL;DR:** {2-3 sentences: what was done and what was learned}

**Date:** YYYY-MM-DD
**Base commit:** `{short hash}`

## What was done

{Concrete deliverables. What was built, fixed, or changed. Reference files, modules,
test counts. Keep it factual — the diff tells implementation details.}

## What was learned

{Insights, surprises, non-obvious findings. This is the most valuable section for future
sessions — what would you tell yourself before starting this work?}

## Open questions

{Unresolved issues, deferred decisions, things to investigate next. Remove this section
if nothing is open.}
```

**Content guidelines:**
- Gather context from the current conversation — what was built, reviewed, discussed
- The TL;DR is the most important line. A future session may read only that.
- Be specific: name modules, test counts, ADRs. "Improved error handling" is useless.
  "Added `translate_btrfs_error()` covering 7 btrfs stderr patterns" is useful.
- Journals are private — real paths, real output, real mistakes are fine
- Don't duplicate the commit message. The journal captures context the commit doesn't.

## Output 2: Update status.md

Overwrite `docs/96-project-supervisor/status.md` with the current state. This is a short
document (~50 lines) that a fresh session reads first.

**Structure:**
1. **Current State** — what's deployed, test count, current version
2. **In Progress** — 0-2 items actively being worked on
3. **Next Up** — 1-3 items from roadmap.md that are next
4. **Key Links** — pointers to roadmap, CLAUDE.md, CONTRIBUTING.md, latest review
5. **Known Issues** — only active issues that affect current work (not the full debt list)

**Rules:**
- Overwrite entirely — don't append to the existing content
- Keep under 60 lines. Ruthlessly cut anything that belongs in roadmap.md or journals.
- Update test count, version, and "In Progress" to reflect this session's outcomes
- "Next Up" should reflect what the user would likely work on in the next session
- Use PII placeholders for any tracked paths (status.md is tracked, not gitignored)

## Arguments

$ARGUMENTS — Optional: slug or topic for the journal entry. If empty, infer from conversation context.
