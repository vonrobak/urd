# Contributing to Urd

This guide defines how documentation is organized, written, and used in the Urd project.
For code conventions, see `CLAUDE.md`.

## Purpose

Urd's documentation serves two audiences simultaneously:

1. **Human memory** — a learning tool and historical record of decisions, progress, and
   lessons learned. The human user should be able to return after weeks away and reconstruct
   context from the docs alone.

2. **Claude Code context** — token-efficient material that lets a fresh AI session make
   informed decisions without reading the entire codebase or conversation history.

Every documentation choice should be evaluated against both audiences.

## Information Hierarchy

When starting a fresh session, read documents in this order:

```
CLAUDE.md                              ← always loaded automatically
  → docs/96-project-supervisor/status.md  ← read this first: current state, active work, links
    → specific docs as needed              ← follow links from status.md to relevant plans/journals/reports
```

**CLAUDE.md** defines architecture, code conventions, and build commands. It is always in
context. Keep it focused on what is needed to write correct code.

**status.md** is the routing document. It tells you where the project stands and points to
the right documents for deeper context. Keep it concise — it will be read at the start of
every session.

**Everything else** is pulled in on demand. This is why the TL;DR convention (below) matters:
Claude can read just the summary of a document and decide whether the full content is needed.

## Directory Structure

```
docs/
  00-foundation/            Core concepts, architectural decisions, user guides
    decisions/              ADRs — strictly immutable, supersede to evolve
    guides/                 User-facing how-tos — living documents, undated
  10-operations/            Runbooks, operational procedures — living documents
  20-reference/             API docs, technical reference — living documents
  90-archive/               Superseded documents (mirrors original directory structure)
  95-ideas/                 Brainstorming, rough proposals, pre-plan thinking
  96-project-supervisor/    Progress tracker and project roadmap — living documents
  97-plans/                 Implementation plans — dated, mostly immutable
  98-journals/              Session logs — GITIGNORED, private, authentic
  99-reports/               Reviews, analysis, generated output — dated, mostly immutable
```

### Tracked vs. Untracked Documentation

This repo is public. Documentation is split into two tiers based on whether it may
contain system-specific personal information:

**Tracked (pushed to GitHub):** All directories except `98-journals/`. These documents
use placeholders for system-specific details (see Privacy below). They form the public
record of the project.

**Untracked (local only):** `98-journals/` is gitignored. Journals are the raw
development diary — they may contain real command output, real paths, and real system
details. This authenticity is what makes them valuable as a learning tool. They stay
on the local machine, backed up by Urd itself.

**The bridge between tiers:** Key learnings from journals are distilled into tracked
documents. The workflow is: write freely in journals, then extract insights into
`status.md`, ADRs, or plans using sanitized placeholders. The journal is raw data;
tracked docs are the processed, public-safe record.

Claude Code reads from the local filesystem, so gitignored journals are fully accessible
in local sessions. They just don't reach GitHub.

### Numbering Scheme

The range 00–89 is for **stable, topic-oriented** documentation. The range 90–99 is for
**process and project management**. Gaps are intentional — add new categories without
renumbering.

| Range | Purpose | Current |
|-------|---------|---------|
| 00–09 | Foundation, core concepts | `00-foundation` |
| 10–19 | Operations | `10-operations` |
| 20–29 | Reference material | `20-reference` |
| 30–89 | _(reserved for future use)_ | |
| 90 | Archive | `90-archive` |
| 91–94 | _(reserved)_ | |
| 95 | Ideas | `95-ideas` |
| 96 | Project supervision | `96-project-supervisor` |
| 97 | Plans | `97-plans` |
| 98 | Journals | `98-journals` |
| 99 | Reports | `99-reports` |

### Directory Details

**00-foundation** — Core documentation explaining what Urd is and why it works the way it
does. The `decisions/` subdirectory holds ADRs recording architectural choices. The `guides/`
subdirectory will hold user-facing operating guides (postponed until production-ready).

**10-operations** — Runbooks and operational procedures. Recovery steps, drive rotation
checklists, troubleshooting guides. Living documents maintained alongside the code.

**20-reference** — Technical reference material. API documentation, configuration reference,
metric definitions. Living documents tracking current codebase state.

**90-archive** — Superseded documents are moved here, never deleted. Mirrors the original
directory structure inside (e.g., a superseded plan moves to `90-archive/97-plans/`).
Preserves historical record while keeping active directories uncluttered.

**95-ideas** — Pre-plan thinking. Rough proposals, "what if" explorations, brainstorming
notes. Ideas that mature get promoted to a plan in `97-plans/`; abandoned ideas stay here
with their status updated. Low ceremony — the point is to capture thinking before it's lost.

**96-project-supervisor** — The central tracking hub. Contains `status.md` (short
current-state document, overwritten each session), `roadmap.md` (strategy, sequencing,
and horizon — ~80 lines), and `registry.md` (UPI lookup table linking work items to their
artifacts). Read status.md first for orientation; follow links to roadmap.md for sequencing
and registry.md for artifact cross-references.

**97-plans** — Implementation plans. Each plan is a dated snapshot of intent — what we're
going to build and how. When scope changes significantly, write a new plan.

**98-journals** — Session logs and handoff documents (gitignored — local only). Each
journal serves two functions: (1) historical record of what was done and learned, and
(2) handoff to the next session — verification steps, things to watch for, context that
git history alone can't provide. Write freely: real command output, real paths, real
mistakes. Distill key learnings into tracked docs (status.md, ADRs) so the public record
stays useful without exposing personal details.

**99-reports** — Analysis and review output. Arch-adversary reviews, automated reports,
progress assessments. Each report is tied to a specific point in time and scope.

## Document Conventions

### File Naming

All documentation files use **lowercase kebab-case** with `.md` extension. Exceptions:
`CLAUDE.md`, `README.md`, and `CONTRIBUTING.md` (GitHub conventions).

| Type | Format | Example |
|------|--------|---------|
| Dated documents | `YYYY-MM-DD-slug.md` | `2026-03-22-urd-phase01.md` |
| Living documents | `slug.md` | `status.md`, `registry.md` |
| ADRs | `YYYY-MM-DD-ADR-NNN-slug.md` | `2026-03-21-ADR-020-daily-external-backups.md` |
| Brainstorms | `YYYY-MM-DD-brainstorm-slug.md` | `2026-03-23-brainstorm-ux-norman-principles.md` |
| Design docs | `YYYY-MM-DD-design-{UPI}-slug.md` | `2026-04-01-design-001-workflow-system-overhaul.md` |
| Design reviews | `YYYY-MM-DD-design-review-{UPI}-slug.md` | `2026-04-01-design-review-001-workflow-system-overhaul.md` |
| Adversary reviews | `YYYY-MM-DD-review-adversary-{UPI}-slug.md` | `2026-04-02-review-adversary-001-workflow-system-overhaul.md` |
| Process analyses | `YYYY-MM-DD-review-analysis-slug.md` | `2026-04-01-review-analysis-workflow.md` |

**UPI (Unique Project Identifier):** Format `NNN` or `NNN-a` (opaque group number, sequential
letter suffix for sub-items). Assigned by `/design`, registered in `docs/96-project-supervisor/registry.md`.
Brainstorms do not get UPIs — the identifier is born when an idea becomes a structured design.

**Pre-2026-04-01 naming:** Review files created before 2026-04-01 use legacy naming patterns
(various conventions). The archived roadmap's feature table provides traceability for those files.

Use subdirectories within a category when a topic has multiple related files
(e.g., `decisions/ADR-relating-to-bash-script/`).

### Immutability Rules

| Type | Immutability | What this means |
|------|-------------|-----------------|
| **ADRs** | **Strict** | Never edit content. To evolve a decision, write a new ADR that supersedes the original. Move the original to `90-archive/00-foundation/decisions/`. |
| **Plans** | Guideline | Minor updates (typo fixes, added context) are acceptable. If scope or approach changes, write a new plan referencing the original. |
| **Journals** | Guideline | Append clarifications if needed, but don't rewrite history. The value is in the authentic record of what was understood at the time. |
| **Reports** | Guideline | Minor corrections are acceptable. If conclusions change, write a new report. |
| **Ideas** | Mutable | Update freely — ideas are meant to evolve. Update the status field as thinking develops. |
| **Living docs** | Mutable | Keep current. These should always reflect the present state. |

**When to update vs. supersede:** If the change is a typo, added context, or small
correction — update in place. If the change alters conclusions, scope, or direction —
write a new document. When in doubt, supersede.

**Supersession workflow:**
1. Write the new document in the appropriate active directory
2. Move the superseded document to `90-archive/` (mirroring original directory structure)
3. Add a header to the new document: `**Supersedes:** [original title](../90-archive/path/to/original.md)`

### ADR Numbering

ADRs use two number ranges:

| Range | Era | Location |
|-------|-----|----------|
| ADR-001–099 | Bash script era | `decisions/ADR-relating-to-bash-script/` |
| ADR-100+ | Urd era | `decisions/` (top level) |

New Urd ADRs increment from the highest existing number. Bash-era ADRs retain their
original numbers and subdirectory for historical reference.

### ADR Lifecycle

ADRs use explicit status tracking:

| Status | Meaning |
|--------|---------|
| `Proposed` | Under discussion, not yet accepted |
| `Accepted` | Active decision guiding implementation |
| `Superseded by ADR-NNN` | Replaced by a newer decision (document moved to archive) |

The current ADR always links forward to its replacement. The replacement links back to what
it supersedes. This creates a traceable chain of decisions.

### TL;DR Convention

Every document over ~30 lines should open with a summary block immediately after the title:

```markdown
# Document Title

> **TL;DR:** Two to three sentences summarizing the key points. What was decided,
> what was built, or what was found. Enough for a reader to decide whether to read
> the full document.
```

This serves both audiences: humans can skim quickly, and Claude can read summaries across
multiple documents without consuming excessive tokens.

### Document Templates

These are not rigid forms — they define the minimum structure that every document of each
type should have. Add sections as needed.

#### Journal Entry

```markdown
# Session Journal: {Title}

> **TL;DR:** {2-3 sentence summary of what was done and learned}

**Date:** YYYY-MM-DD
**Base commit:** `{short hash}`

## What was done

## What was learned

## When you return

{Verification steps, things to check, context the next session needs to pick up
efficiently. This is the handoff — what would you tell the next session before it
reads the code? Do NOT include git workflow state (PR open/closed, branch needs
merging) — the next session checks git directly. DO include: operational checks
(did the timer run?), things to verify about the work, context that git can't provide.
Remove this section if nothing needs handoff.}

## Open questions
```

#### Report

```markdown
# {Report Title}

> **TL;DR:** {2-3 sentence summary of findings}

**Date:** YYYY-MM-DD
**Scope:** {what was reviewed or analyzed}

## Executive Summary

{findings, scores, key observations}
```

#### Plan

```markdown
# Plan: {Title}

> **TL;DR:** {2-3 sentence summary of what will be built and why}

**Date:** YYYY-MM-DD

## Context

## Objectives

## Approach
```

#### ADR

```markdown
# ADR-NNN: {Title}

> **TL;DR:** {2-3 sentence summary of the decision}

**Date:** YYYY-MM-DD
**Status:** Proposed | Accepted | Superseded by ADR-NNN
**Supersedes:** ADR-NNN (if applicable)

## Context

## Decision

## Consequences
```

#### Design Proposal

Use for features that introduce a new module, change a public interface, or affect more
than 3 existing files. Not needed for bug fixes, config tweaks, or self-contained additions.
The design review evaluates the proposal before implementation begins.

```markdown
---
upi: "NNN" or "NNN-a"
status: proposed
date: YYYY-MM-DD
---

# Design: {Feature Name}

> **TL;DR:** {2-3 sentences: what, why, key constraint}

**Depends on:** {prior features or ADRs}

## Problem

## Proposed Design

## Invariants

## Integration Points

## Rejected Alternatives

## Ready for Review

## Open Questions
```

**Status vocabulary** (controlled set):
- `raw` — brainstorm output, not yet structured
- `proposed` — structured design, ready for review
- `reviewed` — design review complete
- `promoted` — sequenced for implementation
- `abandoned` — explicitly not proceeding

#### Idea

```markdown
# Idea: {Title}

> **TL;DR:** {1-2 sentence summary}

**Date:** YYYY-MM-DD
**Status:** raw | developing | promoted to [plan](link) | abandoned

{free-form exploration}
```

## Workflows

### After a development session

1. Run `/journal` — writes a journal entry to `98-journals/` and overwrites `status.md`
   with current state (both outputs from one invocation)
2. If an architectural decision was made, write an ADR in `00-foundation/decisions/`

### Before starting new work

1. Read `96-project-supervisor/status.md` for current state and what to build next
2. Read the most recent journal in `98-journals/` — especially "When you return"
3. Check actual git state: `git log --oneline -5`, `gh pr list`, `git branch`
4. Follow links to relevant plans, reports, and ideas as needed

### When you have a rough idea

Write it in `95-ideas/` with status `raw`. No ceremony needed — capture the thinking.
When the idea matures into something actionable, write a plan in `97-plans/` and update
the idea's status to `promoted to [plan](link)`.

### When a decision needs to be recorded

Write an ADR in `00-foundation/decisions/`. If it supersedes an existing ADR, follow the
supersession workflow. ADRs are the most important immutable records — they explain *why*
the system works the way it does.

### When reviewing or analyzing

Run the analysis, save the output in `99-reports/`. Reports are snapshots — they capture
what was true at a specific point in time. Don't update old reports; write new ones.

## Systemd Deployment

Urd's systemd units live in `systemd/` in this repo. They are deployed by **copying** to
`~/.config/systemd/user/`, not by symlinking. This matches the convention used by the
parent `~/containers` project and prioritizes reliability over convenience — a stale copy
still runs, a broken symlink silently stops backups.

### Install / update

```bash
cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now urd-backup.timer
```

Re-run the `cp` + `daemon-reload` after modifying unit files in the repo.

### Cross-repo unit ownership

The homelab uses multiple repos that each own their own systemd units. A Claude Code session
working on one repo must not modify units owned by another repo.

| Unit file | Owned by | Source |
|-----------|----------|--------|
| `btrfs-backup-daily.*` | `~/containers` | `~/containers/systemd/` |
| `btrfs-backup-weekly.*` | `~/containers` | `~/containers/systemd/` |
| `backup-restore-test.*` | `~/containers` | `~/containers/systemd/` |
| `urd-backup.*` | `~/projects/urd` | `~/projects/urd/systemd/` |

When the cutover is complete and `~/containers` retires the bash backup units, its
documentation should reference Urd as the backup system (e.g., "backups are managed by
Urd — see `~/projects/urd`").

## Privacy

This repo is public. All tracked documentation must be free of personal information.

### Placeholders for tracked documents

| Real value | Placeholder | Notes |
|-----------|-------------|-------|
| System username | `<user>` | |
| `/home/<username>/` | `~/` | Standard shell convention |
| `/run/media/<username>/` | `/run/media/<user>/` | |
| Machine hostname | `<hostname>` | |
| Personal email addresses | `<email>` | |
| Machine-specific IPs | `<ip>` | |

**Not personal information** (fine to use as-is): project name `urd`, drive labels
(`WD-18TB`, `2TB-backup`), subvolume names (`htpc-home`, `subvol3-opptak`), BTRFS paths
that use placeholders. These are project architecture, not personal identifiers.

### Per-document-type rules

| Document type | Privacy rule |
|--------------|-------------|
| Journals (untracked) | Write freely — real paths, real output, no sanitization needed |
| Guides, runbooks, reference | Always use placeholders |
| Plans, ADRs, ideas | Always use placeholders |
| Reports | Use placeholders — reports analyze code, not the environment |
| Config examples | Always use placeholders |
| status.md, roadmap | Use placeholders — distill journal learnings into sanitized form |

### The journal → tracked doc workflow

When a journal captures something worth preserving in the public record:

1. Identify the insight (a decision, a finding, a lesson)
2. Write it into the appropriate tracked document (status.md, an ADR, a plan)
3. Replace system-specific details with placeholders
4. The journal retains the raw, authentic version locally

This separation means you never have to choose between authenticity and privacy.
Journals are the private workspace; tracked docs are the public-safe extract.

## Writing Guidelines

- **Lead with the summary.** The TL;DR is the most important line in the document.
- **Keep documents focused.** One topic or session per file. A 200-line focused document
  is more useful than a 500-line document covering three topics.
- **Reference commits by short hash** when tying a document to a specific codebase state.
- **Cross-reference by relative path** (e.g., `see [Phase 1 journal](../98-journals/2026-03-22-urd-phase01.md)`).
- **Write for your future self.** The journal entry that feels obvious today will be
  invaluable context in three months.
- **Write for token efficiency.** Structure documents with headers so Claude can skip
  sections that aren't relevant. Put the most important information first.
