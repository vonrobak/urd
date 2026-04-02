Capture a focused journal entry about a specific topic, observation, or lesson learned.

This is the lightweight, mid-session journal — use it whenever something worth recording
happens during work. It writes a single focused entry and nothing else. For end-of-session
wrap-up (journal + status.md + registry.md updates), use `/session-close` instead.

## When to use

- You hit a surprising behavior or non-obvious finding during implementation
- You need to retrace a step in the workflow and want to document why
- You're about to leave and need a quick record so you can pick up later
- You discovered something about the codebase, a tool, or a pattern worth preserving
- A debugging session revealed root causes worth recording
- You encountered a workflow gap or process improvement worth noting

The common thread: something happened that future-you would benefit from knowing, but
the session isn't over and you don't need the full session-close ritual.

## Output

Write to `docs/98-journals/YYYY-MM-DD-{slug}.md`. This is gitignored (private, local only).

**Auto-fill metadata:**
- Date: today's date
- Base commit: `git rev-parse --short HEAD`
- Slug: derived from $ARGUMENTS or the topic being documented

**Template:**

```markdown
# Journal: {Specific Topic Title}

> **TL;DR:** {1-2 sentences: the key insight or observation}

**Date:** YYYY-MM-DD
**Base commit:** `{short hash}`
**Context:** {what you were doing when this came up — 1 line}

## What happened

{The specific event, behavior, or discovery. Be concrete — name modules, error messages,
test results, file paths. This section answers "what did I observe?"}

## Lessons learned

{The insight extracted from the observation. This is the most valuable section — what
would you tell someone about to encounter the same situation? What was non-obvious?
What assumption was wrong?}

## Impact on current work

{How this affects what you're building right now. Does it change the plan? Does it
require a detour? Is it just context for later? Remove this section if purely
informational with no impact on active work.}
```

**Content guidelines:**
- Focus tightly on the specific topic — this is not a session summary
- The TL;DR is the most important line. A future session scanning journal filenames
  and TL;DRs should be able to decide whether to read further.
- Be specific: "retention logic silently keeps snapshots when pin file has a trailing
  newline" is useful. "Found a bug in retention" is not.
- Journals are private — real paths, real output, real mistakes are fine
- Multiple journal entries in one day are fine — use different slugs

## What NOT to do

- Do not update `status.md` — that's `/session-close`'s job
- Do not update `registry.md` — that's `/session-close`'s job
- Do not write a comprehensive session summary — keep it focused on the specific topic
- Do not defer writing because "I'll capture it in the session close" — the detail
  and immediacy of mid-session capture is the whole point

## Arguments

$ARGUMENTS — The topic to document. Can be a short description ("btrfs send failure
on readonly subvolumes"), a focus area ("lessons from debugging the chain module"),
or a reference to what just happened ("what we just discovered about retention edge cases").
