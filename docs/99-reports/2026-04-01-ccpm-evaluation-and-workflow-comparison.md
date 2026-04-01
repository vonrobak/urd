# CCPM Evaluation and Workflow Comparison

> **TL;DR:** CCPM (Claude Code Project Manager) is a spec-driven workflow skill that
> solves several gaps in Urd's current workflow — particularly planning/execution separation,
> task decomposition, and progress tracking. However, it's designed for multi-developer
> product teams building web features, not a solo developer building a systems tool with
> a deep documentation culture. The right approach is selective adoption: take CCPM's
> strongest ideas (structured planning artifacts, task decomposition with dependencies,
> script-based tracking, GitHub issue integration) and adapt them to Urd's existing
> strengths (ADR system, review pipeline, mythic voice philosophy, document-as-context
> design). This document maps CCPM's five phases against Urd's nine-command pipeline,
> identifies what each system does better, and proposes a synthesis.

**Date:** 2026-04-01
**Type:** Comparative analysis
**Sources:** [CCPM repository](https://github.com/automazeio/ccpm), Urd workflow analysis
(`docs/99-reports/2026-04-01-workflow-and-project-management-analysis.md`)

---

## 1. What CCPM Is

CCPM is a Claude Code Agent Skill implementing a five-phase workflow:

```
Plan (PRD) → Structure (Epic + Tasks) → Sync (GitHub Issues) → Execute (Parallel Agents) → Track (Bash Scripts)
```

Key properties:
- **Spec-driven:** Everything starts as a PRD (Product Requirements Document), becomes
  a technical epic, decomposes into numbered tasks with dependency metadata
- **GitHub-centric:** Tasks sync to GitHub Issues with labels, sub-issues, and worktrees
- **Parallelism-first:** Tasks are analyzed for file conflicts and launched as parallel
  agents in isolated worktrees
- **Script-based tracking:** 14 bash scripts handle status/standup/search/validation
  without LLM overhead
- **Local files as source of truth:** `.claude/prds/` and `.claude/epics/` persist across
  sessions; GitHub is a synchronized mirror

---

## 2. Side-by-Side Comparison

### 2.1 Workflow Phase Mapping

| Concern | CCPM | Urd | Assessment |
|---------|------|-----|------------|
| **Ideation** | PRD brainstorming (guided questions) | `/brainstorm` (divergent, no scoring) | Both good. Urd's is more creative; CCPM's is more structured |
| **Requirements** | PRD document with acceptance criteria | No explicit step | **Gap in Urd.** Design docs assume requirements are understood |
| **Design** | Epic (technical decomposition from PRD) | `/design` (module decomposition, ADR gates) | Urd's is deeper — it reasons about architectural invariants |
| **Stress-testing** | None | `/grill-me` (Socratic interview) | **Urd advantage.** CCPM has no equivalent |
| **Task breakdown** | Structure phase (numbered tasks, dependencies, parallelization) | Implicit in design doc | **Gap in Urd.** No formal task decomposition |
| **GitHub sync** | Sync phase (issues, labels, worktrees) | `/commit-push-pr` (branch, commit, PR) | **Gap in Urd.** No issue tracking, no task-level visibility |
| **Execution** | Parallel agents with stream analysis | Manual implementation | **Gap in Urd.** But solo developer rarely needs parallelism |
| **Code review** | None built-in | `arch-adversary` + `/post-review` + `/simplify` | **Urd advantage.** CCPM has no review pipeline |
| **Quality gate** | None built-in | `/check` (clippy + tests + build) | **Urd advantage.** CCPM assumes CI handles this |
| **Tracking** | 14 bash scripts (standup, blocked, next, search) | `status.md` + `roadmap.md` (manual) | **CCPM advantage.** Automated, zero LLM cost |
| **Documentation** | Minimal (PRD + epic + task files) | Rich (ADRs, journals, design docs, reviews, CONTRIBUTING.md) | **Urd advantage.** CCPM treats docs as tickets, not knowledge |
| **Release** | None | `/release` (SemVer, CHANGELOG, tags) | **Urd advantage.** CCPM doesn't cover release workflow |
| **Session continuity** | `.claude/` files persist context | `status.md` → journals → design docs | Both good. Different mechanisms, same goal |

### 2.2 Artifact Comparison

| Artifact | CCPM | Urd |
|----------|------|-----|
| Requirements | `.claude/prds/<name>.md` with frontmatter | Absorbed into design docs or conversation |
| Technical plan | `.claude/epics/<name>/epic.md` | `docs/95-ideas/YYYY-MM-DD-design-slug.md` |
| Tasks | `.claude/epics/<name>/<N>.md` with status, dependencies | None (implicit in design docs) |
| Progress | `.claude/epics/<name>/updates/<N>/progress.md` | `docs/96-project-supervisor/status.md` (manual) |
| Reviews | None | `docs/99-reports/YYYY-MM-DD-{review}.md` |
| Decisions | None | `docs/00-foundation/decisions/ADR-NNN.md` |
| Session logs | None | `docs/98-journals/YYYY-MM-DD-slug.md` |
| Cross-reference | `github-mapping.md` + frontmatter links | Feature table in `roadmap.md` |

### 2.3 Philosophy Comparison

| Dimension | CCPM | Urd |
|-----------|------|-----|
| **Who is it for?** | Teams shipping product features | Solo developer building a systems tool |
| **What's the unit of work?** | GitHub Issue | Design doc + implementation session |
| **What's the source of truth?** | Local `.claude/` files mirrored to GitHub | `docs/` directory tracked in git |
| **How is quality ensured?** | Spec completeness + parallel execution | Review pipeline (design → impl → post-review) |
| **How is knowledge preserved?** | Ticket lifecycle (open → closed → archived) | Documentation lifecycle (idea → design → ADR → journal) |
| **What's the failure mode?** | Ticket debt (stale issues, zombie epics) | Documentation drift (naming inconsistency, growing roadmap) |
| **Context model** | Structured frontmatter with status fields | TL;DR convention + information hierarchy |

---

## 3. What CCPM Does Better

### 3.1 Planning/Execution Separation (Critical Gap in Urd)

CCPM enforces a hard boundary: you cannot start executing until you have a PRD, an epic,
and decomposed tasks. Urd's workflow allows jumping from `/design` directly to implementation
without an explicit "what are the discrete tasks?" step.

**Evidence from Urd:** The `.claude/plans/` directory contains 53 session plans that are
essentially ad-hoc task lists Claude Code created during sessions. These are the task
decomposition step happening implicitly and ephemerally rather than as a durable artifact.

**What to adopt:** A lightweight task decomposition step between design and implementation.
Not CCPM's full PRD → Epic → Task pipeline (too heavyweight for solo development), but a
structured "implementation plan" section in the design doc that lists discrete tasks with
ordering.

### 3.2 Task Decomposition with Dependencies

CCPM's task files carry structured metadata:

```yaml
depends_on: [1234, 1235]     # can't start until these close
parallel: true                # safe to run concurrently
conflicts_with: [1237]        # touches same files
```

Urd has nothing equivalent. The design doc describes what modules are affected, but doesn't
decompose into ordered steps with explicit dependencies.

**What to adopt:** When `/design` produces a multi-session feature, include a task table:

```markdown
## Implementation Tasks

| # | Task | Depends on | Modules | Est. |
|---|------|-----------|---------|------|
| 1 | Add new types to types.rs | - | types | 0.5h |
| 2 | Implement pure logic in awareness.rs | 1 | awareness | 1h |
| 3 | Wire into executor | 1, 2 | executor, commands | 1h |
| 4 | Add voice rendering | 2 | voice, output | 0.5h |
| 5 | Tests for all new paths | 1-4 | tests | 1h |
```

This is lighter than CCPM's per-file task system but captures the same information.

### 3.3 Script-Based Tracking (Zero LLM Cost)

CCPM's 14 bash scripts are brilliant. Status queries, standups, blocked items, and
validation run as deterministic shell scripts. No tokens consumed. Instant results.

Urd's equivalent — reading status.md and roadmap.md — consumes tokens every session and
requires LLM interpretation. The `/journal` command manually overwrites status.md.

**What to adopt:** A small set of tracking scripts that answer common session-start questions
without LLM overhead:

| Script | Purpose | Urd equivalent today |
|--------|---------|---------------------|
| `status.sh` | What's deployed, test count, version | Read status.md (tokens) |
| `next.sh` | What to work on next | Read status.md + roadmap.md (tokens) |
| `validate.sh` | Check for naming inconsistencies, orphaned docs | None |
| `review-map.sh` | Find the review for a given design doc | Manual grep |

These don't replace status.md (which serves as a handoff document for context), but they
reduce the token cost of routine queries.

### 3.4 GitHub Issue Integration

CCPM uses GitHub Issues as a task tracking layer. Each task gets an issue number, labels
link issues to epics, and progress is posted as comments.

Urd currently uses PRs for code integration but doesn't use Issues for task tracking. Work
items are tracked in roadmap.md's feature table and status.md's "Next Up" section.

**What to adopt (carefully):** GitHub Issues could track the "Next Up" queue from
status.md. Each priority item (6-O, P6a, P6b, 6-H) could be an issue with a label. This
provides:
- A public-facing view of what's planned
- A place for implementation notes that's not a full design doc
- Automatic linkage to PRs via `Fixes #N` in commit messages

**What NOT to adopt:** CCPM's full epic/sub-issue hierarchy. For a solo developer, the
overhead of managing issue relationships exceeds the benefit. A flat list of issues
labeled by priority arc is sufficient.

### 3.5 Frontmatter as Structured Metadata

CCPM puts machine-readable frontmatter on every artifact:

```yaml
---
name: feature-name
status: backlog | in-progress | completed
created: 2026-04-01T12:00:00Z
github: https://github.com/.../issues/42
depends_on: [41, 43]
---
```

Urd's documents have TL;DR blocks and metadata fields, but they're not consistently
structured for machine parsing. The `Status:` field in `docs/95-ideas/` files is good,
but it's free-text, not a controlled vocabulary.

**What to adopt:** Standardize the status field vocabulary across all Urd document types.
Use YAML frontmatter where documents might be programmatically queried (especially in
`docs/95-ideas/` and `docs/99-reports/`). This enables future tracking scripts.

---

## 4. What Urd Does Better

### 4.1 Review Pipeline (CCPM Has None)

CCPM goes from "tasks decomposed" to "agents executing" with no review step. There is no
design review, no implementation review, no adversarial challenge. Quality is assumed to
come from spec completeness and test coverage.

Urd's `arch-adversary` → `/post-review` → `/simplify` pipeline is one of its most valuable
assets. The 6-dimension scorecard (premises, flow, gates, error paths, simplicity, naming)
with severity ratings has caught significant bugs before they shipped.

**Verdict:** Keep everything. CCPM has nothing to offer here.

### 4.2 Architectural Decision Records

CCPM has no concept of ADRs. Decisions are embedded in PRDs and epics. There's no mechanism
for recording why architectural choices were made, making them immutable, or superseding them.

Urd's ADR system (14 records, strict numbering, immutability, supersession workflow) is
among the most disciplined aspects of the project. For a systems tool where backward
compatibility matters, this is essential.

**Verdict:** Keep everything. This is infrastructure CCPM doesn't need (web features)
but Urd absolutely does (on-disk data formats, config contracts).

### 4.3 Documentation as Knowledge (Not Just Tickets)

CCPM treats documents as workflow artifacts: they're created, tracked, and archived. Once
an epic is merged, its docs move to `.claude/epics/archived/`. Knowledge lives in the code
and commit history.

Urd treats documents as accumulated knowledge: design docs capture reasoning, ADRs record
decisions, journals preserve context, reviews identify patterns. The documentation system
is designed for a project that will be maintained for years, not a feature that ships and
is forgotten.

**Verdict:** Urd's approach is correct for its domain. A backup tool's documentation needs
to explain *why* decisions were made years after the fact. CCPM's archive-and-forget model
would lose this context.

### 4.4 The Grill-Me Step

`/grill-me` is a Socratic interview that stress-tests a design before committing to
implementation. CCPM has no equivalent — the gap between PRD and execution is filled by
structural decomposition, not intellectual challenge.

For a solo developer where Claude is both the implementer and the reviewer, having a
dedicated adversarial step before implementation is uniquely valuable. It catches
assumption errors that would otherwise survive until the implementation review.

**Verdict:** Keep. This is a workflow innovation that CCPM hasn't discovered yet.

### 4.5 Quality Gate Integration

`/check` runs `cargo clippy -- -D warnings`, the full test suite, and a release build
before any commit is allowed. This is integrated into the workflow, not delegated to CI.

CCPM assumes CI/CD handles quality gating. For a solo developer without CI, the local
quality gate is essential.

**Verdict:** Keep. CCPM's assumption that CI exists doesn't apply here.

### 4.6 Release Workflow

`/release` handles SemVer bumps, CHANGELOG.md updates, git tags, and version consistency.
CCPM has no release concept — it ends at "epic merged."

**Verdict:** Keep. Urd's release workflow is mature and well-integrated.

---

## 5. What's Genuinely Missing from Both

### 5.1 Structured Progress Tracking (Neither Does This Well)

CCPM has progress files but they're designed for parallel agent coordination, not human
status tracking. Urd has status.md but it's manually maintained and only captures
current state, not progress over time.

Neither system answers: "How much of Priority 6 is done? How fast are we moving? When
might we reach v1.0?"

**Proposed:** A lightweight progress section in roadmap.md's feature table:

```markdown
| # | Feature | Status | Sessions Est. | Sessions Actual |
| P1 | Vocabulary landing | Complete | 1 | 1 |
| P2a | urd default | Complete | 1 | 0.5 |
| 6-O | Progressive disclosure | Next | 2 | - |
```

The "Sessions Actual" column provides velocity data. No external tools needed.

### 5.2 Validation Scripts (Neither Has Them for Docs)

CCPM's `validate.sh` checks frontmatter and broken references, but only for its own
`.claude/` files. Urd has no validation for its `docs/` structure.

**Proposed:** A `docs-validate.sh` script that checks:
- All files in `99-reports/` match the naming convention
- All design docs in `95-ideas/` have a Status field
- All files referenced in roadmap.md's feature table actually exist
- No undated files in dated directories

---

## 6. Synthesis: Proposed Workflow Evolution

### 6.1 What to Adopt from CCPM

| CCPM Concept | Adaptation for Urd | Effort |
|-------------|-------------------|--------|
| **Task decomposition in design docs** | Add "Implementation Tasks" table to `/design` output | Low — update skill prompt |
| **Script-based status queries** | 3-4 bash scripts for common queries | Medium — write scripts |
| **GitHub Issues for work items** | Issue per priority item, labeled by arc | Low — new habit |
| **Structured frontmatter** | Standardize Status vocabulary, add YAML frontmatter | Low — convention change |
| **Worktree per feature** | Already using feature branches; worktrees for parallel work if needed | None — available but rarely needed |

### 6.2 What to Keep from Urd

| Urd Strength | Why It Stays |
|-------------|-------------|
| ADR system | On-disk contracts need immutable decision records |
| `/grill-me` | Unique adversarial step CCPM lacks |
| `arch-adversary` + `/post-review` | Full review pipeline CCPM doesn't have |
| `/check` quality gate | No CI to delegate to |
| `/release` workflow | SemVer + CHANGELOG integrated into pipeline |
| Journal system | Knowledge preservation, not just task tracking |
| CONTRIBUTING.md conventions | Document-as-context design serves Claude efficiency |
| TL;DR convention | Token-efficient scanning across 200+ files |

### 6.3 What to Reject from CCPM

| CCPM Feature | Why It Doesn't Fit |
|-------------|-------------------|
| **PRD as separate artifact** | Urd's `/brainstorm` → `/design` pipeline already captures requirements within design docs. A separate PRD adds a document without adding information. |
| **Epic/sub-issue hierarchy** | Overhead exceeds benefit for a solo developer. Flat issue list with labels is sufficient. |
| **Parallel agent execution** | Solo developer rarely works on multiple streams simultaneously. The complexity of agent coordination (progress files, stream analysis, conflict detection) is unwarranted. |
| **`.claude/` as artifact home** | Urd's `docs/` directory is tracked in git, follows naming conventions, and serves as project knowledge. Moving artifacts to `.claude/` (which is less visible, less structured) would be a regression. |
| **Archive-and-forget lifecycle** | Urd's documentation is accumulated knowledge, not disposable tickets. Completed design docs remain reference material. |
| **14 bash scripts** | Most are for parallel agent coordination that doesn't apply. Adopt 3-4 that solve real Urd needs. |

### 6.4 Proposed Evolved Pipeline

```
/brainstorm → /design (with task table) → /grill-me → [build per task] →
  /simplify → arch-adversary → /post-review → /check → /journal → /commit-push-pr
```

Changes from current pipeline:
1. `/design` output gains an "Implementation Tasks" section with ordering and dependencies
2. GitHub Issues created for each priority item (one issue per 6-O, P6a, etc.)
3. PRs reference issues via `Fixes #N` for automatic closure
4. 3-4 bash scripts added for status queries
5. Session start reads status.md (context) then runs `status.sh` (facts)

The pipeline structure doesn't change. The artifacts get richer. The tracking gets cheaper.

---

## 7. GitHub Issues: New Habit Proposal

The single highest-value CCPM concept for Urd is using GitHub Issues as a lightweight
work tracker. Here's what this looks like in practice:

### Current State (Urd)

Work items live in:
- `roadmap.md` feature table (design + review links)
- `status.md` "Next Up" section (1-3 items)
- Session conversation ("let's work on 6-O")

### Proposed State

Work items also exist as GitHub Issues:

```
#61  6-O: Progressive disclosure               [priority-6] [feature]
#62  P6a: ADR-110 enum rename                   [priority-6] [refactor]
#63  P6b: Config Serialize refactor             [priority-6] [refactor]
#64  6-H: Guided setup wizard                   [priority-6] [feature]
```

**Benefits:**
- PRs link to issues (`Fixes #61`) — automatic traceability
- Issues persist as a searchable record of what was planned vs. what was built
- `gh issue list --label priority-6` replaces reading roadmap.md for "what's in this arc"
- Issues can hold implementation notes, blockers, and session handoff context
- Public visibility for anyone following the project

**Rules to keep it lightweight:**
- One issue per work item (not per task within a work item)
- Created when work enters the "Next Up" queue, not when brainstormed
- Closed by PR merge, not manually
- Labels: `priority-N` (arc), `feature`/`fix`/`refactor`/`docs` (type)
- No milestones, no projects, no sub-issues — just flat issues with labels
- The design doc link goes in the issue body; the review link is added when available

**What changes in the workflow:**
- `/commit-push-pr` already creates PRs — add `Fixes #N` to PR body when an issue exists
- `/journal` notes which issues were worked on (already notes what was done)
- Status.md's "Next Up" section links to issues instead of just naming them

---

## 8. Recommendations by Priority

| # | Action | Source | Effort | Impact |
|---|--------|--------|--------|--------|
| 1 | Add "Implementation Tasks" table to `/design` output | CCPM's Structure phase | Low | High — closes planning gap |
| 2 | Start using GitHub Issues for work items | CCPM's Sync concept | Low | High — adds traceability |
| 3 | Write 3-4 status/validation bash scripts | CCPM's Track phase | Medium | Medium — saves tokens |
| 4 | Standardize frontmatter vocabulary | CCPM's conventions | Low | Medium — enables scripts |
| 5 | Add `Fixes #N` to `/commit-push-pr` | CCPM's issue lifecycle | Trivial | Medium — automatic closure |
| 6 | Adopt review naming convention | Workflow analysis | Low | High — solves naming chaos |
| 7 | Split roadmap.md | Workflow analysis | Medium | Medium — reduces token waste |

Items 1-2 and 5-6 can be implemented in a single session (convention and skill prompt
changes only). Items 3-4 and 7 are a second session.

---

## 9. What the User Should Learn

The CCPM evaluation reveals a broader lesson about Claude-first development workflows:

### 9.1 Documents serve two masters

Every document in the system serves both the user (learning, remembering, deciding) and
Claude (context, constraints, continuity). CCPM optimizes for Claude (structured frontmatter,
machine-parseable status). Urd optimizes for the user (TL;DR, narrative design docs, mythic
voice). The best system does both — structured metadata for machines, readable prose for
humans. YAML frontmatter + TL;DR is the synthesis.

### 9.2 Planning and execution are different cognitive modes

CCPM's biggest insight is that planning (divergent, creative, considering alternatives) and
execution (convergent, precise, following a plan) should be structurally separated. Urd's
`/design` step is planning; the build step is execution. But without an explicit task
decomposition between them, the transition is abrupt. Adding a task table to design docs
is a small change with outsized impact on execution clarity.

### 9.3 Tracking should be free

Every status query that requires reading a document consumes tokens and LLM time. CCPM's
script-first principle is correct: if a question can be answered by parsing files, it
should be answered by a script. Reserve the LLM for reasoning, not reporting.

### 9.4 GitHub Issues are underused

For a public project with a git-centric workflow, GitHub Issues are a natural work tracker.
They're free, they integrate with PRs, they're searchable, and they persist as a record
of intent. The overhead of creating an issue is trivial compared to the traceability it
provides. The key is keeping it flat — issues, not epics, not projects, not milestones.

### 9.5 Don't adopt what doesn't fit

CCPM's parallel agent execution, epic hierarchies, and archive-and-forget lifecycle are
designed for a different context (product teams, web features, multiple developers). Adopting
them would add complexity without proportional benefit. The skill of workflow design is
knowing what to leave out.

---

## Appendix: CCPM Reference Architecture

For reference, CCPM's complete file structure:

```
.claude/
├── prds/<feature-name>.md              # Product requirements
├── epics/<feature-name>/
│   ├── epic.md                          # Technical decomposition
│   ├── <N>.md                           # Task files (by issue number)
│   ├── <N>-analysis.md                  # Parallel stream analysis
│   ├── github-mapping.md               # Issue → URL mapping
│   ├── execution-status.md             # Active agent tracker
│   └── updates/<N>/                     # Per-issue progress
│       ├── stream-{A,B,C}.md           # Per-agent progress
│       ├── progress.md                  # Overall completion
│       └── execution.md                # Execution state
└── context/                             # Project context docs
```

CCPM bash scripts: `status.sh`, `standup.sh`, `epic-list.sh`, `epic-show.sh`,
`epic-status.sh`, `prd-list.sh`, `prd-status.sh`, `search.sh`, `in-progress.sh`,
`next.sh`, `blocked.sh`, `validate.sh`, `help.sh`, `init.sh`.

Urd should not replicate this structure. It should learn from the principles behind it.
