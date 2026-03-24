# Design Evolution Analysis: ADRs and Documentation Tools for Urd

> **TL;DR:** Urd has an exceptionally mature documentation practice — journals, adversary
> reviews, brainstorms, and a living status tracker — but most architectural decisions live
> in scattered one-line entries rather than formal ADRs. Only one ADR exists (ADR-020) while
> 40+ decisions are recorded informally in status.md. The project needs a lightweight ADR
> practice, a design proposal template, and a few supporting tools to bridge the gap between
> "decisions were made" and "decisions are findable, challengeable, and evolvable."

**Date:** 2026-03-24
**Scope:** Full project evolution — journals (18), reports (30), git history (43 commits),
plans (1), ADRs (1), ideas (4), CLAUDE.md, CONTRIBUTING.md, status.md, roadmap.md
**Reviewer:** Claude (arch-adversary)
**Commit:** `4712be2`

---

## Executive Summary

Urd's design evolution workflow is remarkably disciplined for a 3-day-old project. The
design-review-before-implementation pattern (adversary review → code → adversary review)
has caught real bugs (clock skew, mkdir precondition, legacy pin false positives) and
prevented architectural mistakes (Sentinel as monolith, promises as config extension).
The journal → status.md → tracked docs pipeline preserves institutional memory while
maintaining privacy.

But there is a structural gap: **decisions are being made and recorded, but not in a form
that makes them findable, challengeable, or evolvable.** The "Recent Decisions" table in
status.md has 40+ entries — each a one-line summary with a reference. These are effectively
ADR titles without ADR bodies. When a future session needs to understand *why* asymmetric
multipliers were chosen for the awareness model, it must chase through a design review,
find Finding 1, reconstruct the reasoning, and hope it's complete.

This report identifies where the current workflow works well, where it breaks down, and
what documentation tools would keep Urd's architecture consistent as it grows.

---

## How Ideas Currently Flow from Conception to Implementation

### The observed pipeline

```
Brainstorm (95-ideas/)
    ↓ user ranking + editorial judgment
Vision document (CLAUDE.md updates, brainstorm synthesis)
    ↓ adversary review of vision
Architectural constraints identified (vision architecture review)
    ↓ decomposition into priorities
status.md priority list (with architectural gates)
    ↓ design plan (in journal or ad-hoc)
Design review (arch-adversary, saved to 99-reports/)
    ↓ findings change the design
Implementation (code)
    ↓ implementation review (arch-adversary)
Fix review findings → merge
    ↓
status.md updated (Recent Decisions table, completed items)
Journal written (98-journals/)
```

### What works well

**1. The adversary review as architectural gate.** Every significant feature gets a design
review *before* code is written. This has prevented real problems:
- Sentinel decomposed into three systems instead of one monolith (vision architecture review)
- Promises identified as policy design problem, not config extension (vision architecture review)
- Asymmetric multipliers for awareness model (awareness model design review)
- Best-drive aggregation instead of worst-drive (awareness model design review)
- `urd get` shipped before `urd find` due to unsolved performance problem (vision architecture review)

**2. Journals as private working memory.** The tracked/untracked split is well-designed.
Journals capture raw learning with authentic command output; status.md and ADRs capture
the sanitized public record. This means the human operator has full context locally while
the public repo stays clean.

**3. status.md as living router.** The information hierarchy (CLAUDE.md → status.md →
specific docs) gives every session a reliable starting point. The "What to Build Next"
section with architectural gates prevents building features on shaky foundations.

**4. The "design before code" culture.** The awareness model journal shows the ideal
pattern: read existing code → write design plan → adversary review of design → implement
→ adversary review of implementation → fix findings. This produced a module that scored
4-5/5 across all dimensions.

### What breaks down

**1. Decisions scatter across document types.** The decision that `OutputMode` should be
an enum rather than a trait lives in the presentation layer design review. The decision
that `urd get` should use `--at` instead of `@` syntax lives in the urd get design review.
The decision that the executor must mkdir before `btrfs receive` lives in the pre-cutover
journal. None of these are in the ADR directory where a future session would look for
architectural decisions.

**2. The "Recent Decisions" table is an ADR registry without ADR bodies.** Status.md has
40+ entries like:

```
| OutputMode enum + match, not Renderer trait | 2026-03-24 | Presentation layer review |
```

This tells you *what* was decided and *where* the reasoning lives. But to understand *why*,
you must read the referenced review, find the relevant finding or tension, and reconstruct
the reasoning. This works when the project is 3 days old. It will not work in 3 months.

**3. No clear "design proposal" template.** When a new feature is designed, the plan
currently lives in a journal entry or ad-hoc in the adversary review's scope description.
There is a plan template in CONTRIBUTING.md but only one plan document exists
(`git-history-pii-scrub.md`, an operational plan). There are no *design* plans — the
closest equivalent is the awareness model section of its journal entry.

**4. Founding decisions have no ADRs.** The most important architectural decisions were
made during project inception and are documented in CLAUDE.md and roadmap.md, but not as
ADRs:
- Planner/executor separation (pure function planner, never-modify-anything)
- BtrfsOps trait as the only module that calls btrfs
- SQLite for history, filesystem for source of truth
- Two-process pipeline for send|receive (not shell pipe)
- Interval-based scheduling replacing cron-like schedules
- Graduated retention replacing fixed counts

These founding decisions are the most important to preserve because they constrain everything
built on top of them. Without ADRs, a future session might unknowingly violate one.

**5. The ADR numbering is opaque.** The single existing ADR is ADR-020, which implies 19
prior ADRs from the bash script era. The ADR directory structure
(`decisions/ADR-relating-to-bash-script/`) suggests a pre-Urd numbering space. There is no
documented ADR numbering scheme for the Urd era.

---

## Proposed Documentation Tools

### Tool 1: Lightweight ADRs for Architectural Decisions

**Problem:** Decisions are made and recorded, but not in a findable, challengeable form.

**Proposal:** Write ADRs for decisions that constrain future work. Not every decision needs
one — only those where violating the decision would cause architectural damage. Use the
existing ADR template from CONTRIBUTING.md but keep them short (under 50 lines for most).

**Categories of decisions that warrant ADRs:**

| Category | Example | Why it needs an ADR |
|----------|---------|---------------------|
| **Foundational invariant** | Planner/executor separation | Violating it breaks testability and the entire architecture |
| **Data format contract** | Snapshot naming, pin file format, Prometheus metrics | External systems depend on the format |
| **Policy design** | Protection promise levels → retention derivation | Affects user trust and data safety |
| **Technology choice** | SQLite for history, filesystem for truth | Constrains future features |
| **Rejected alternative** | No async/tokio, no shell pipes for send/receive | Future contributors will ask "why not?" |

**Categories that do NOT need ADRs:**

| Category | Example | Where it lives instead |
|----------|---------|----------------------|
| Implementation detail | `OutputMode` as enum vs. trait | Code comment or review finding |
| Bug fix | mkdir before btrfs receive | Commit message + journal |
| Config field design | `--at` vs `@` syntax for urd get | Review finding |
| Operational procedure | Systemd copy-not-symlink | CONTRIBUTING.md |

**Proposed ADR numbering:** Start fresh at ADR-100 for the Urd era. ADR-020 and any
bash-era ADRs keep their numbers in the `ADR-relating-to-bash-script/` subdirectory.
New ADRs go in `docs/00-foundation/decisions/` directly.

**Retroactive ADRs to write (prioritized):**

1. **ADR-100: Planner/executor separation.** The core invariant. Why the planner is a pure
   function. Why the executor never decides what to do. What breaks if this is violated.
2. **ADR-101: BtrfsOps trait as sole btrfs interface.** Why all btrfs calls go through one
   module. How MockBtrfs enables testing. What happens if someone bypasses it.
3. **ADR-102: Filesystem as source of truth, SQLite as history.** Why snapshots and pin
   files are authoritative. Why SQLite failures must never prevent backups.
4. **ADR-103: Interval-based scheduling.** Why Urd uses intervals instead of cron. How
   this differs from the bash script and why.
5. **ADR-104: Graduated retention model.** Why Time Machine-style retention. How space
   pressure interacts. What the NVMe constraint means.
6. **ADR-105: Backward compatibility contracts.** Which formats are load-bearing (snapshot
   names, pin files, metrics). What "backward compatible" means concretely.

These are all already decided and documented — the ADRs just formalize them in a findable
location. Each should be under 50 lines and reference the original journal/roadmap for
full context.

**Forward ADRs already identified:**

- **ADR for protection promises** (already gated in status.md Priority 4)
- **ADR for Sentinel decomposition** (awareness + reactor + notifications)
- **ADR for migration** (ADR-021, already planned for cutover)

### Tool 2: Design Proposal Template

**Problem:** Feature design currently lives in journals or ad-hoc in review scopes. There
is no consistent place for "here is what we plan to build and why" before the adversary
review happens.

**Proposal:** Add a design proposal template for `docs/97-plans/`. A design proposal is
distinct from an operational plan — it describes an architectural addition, its interfaces,
its invariants, and its integration points. The adversary review then reviews the proposal.

```markdown
# Design: {Feature Name}

> **TL;DR:** {2-3 sentences: what, why, and key constraint}

**Date:** YYYY-MM-DD
**Status:** proposed | reviewed | accepted | abandoned
**Depends on:** {prior features or ADRs}

## Problem

{What problem does this solve? What happens if we don't build it?}

## Proposed Design

{Module name, key types, function signatures, data flow}

## Invariants

{What must always be true? What would break if these were violated?}

## Integration Points

{Which existing modules are affected? What interfaces change?}

## Rejected Alternatives

{What else was considered and why it was rejected?}

## Open Questions

{What needs to be resolved before or during implementation?}
```

**When to write one:** Before any feature that introduces a new module, changes a public
interface, or affects more than 3 existing files. Not needed for bug fixes, config tweaks,
or self-contained additions.

### Tool 3: Decision Log Graduation

**Problem:** The "Recent Decisions" table in status.md is growing unboundedly (40+ entries)
and mixing foundational decisions with implementation details.

**Proposal:** Graduate decisions from status.md into appropriate permanent homes:

1. **Foundational decisions → ADRs** (as described above)
2. **Design decisions → code comments or review references** (no change needed, but stop
   adding one-liners to the table for these)
3. **Active/recent decisions → keep in status.md** (last 30 days or last major phase)
4. **Graduated decisions → remove from status.md** (the ADR or review is the permanent home)

**Target:** The "Recent Decisions" table should have 10-15 entries at any time, covering
the current and previous phase. Older decisions should be findable via ADRs or reviews,
not via an ever-growing table.

### Tool 4: Architectural Invariant Checklist

**Problem:** CLAUDE.md documents module responsibilities and conventions, but architectural
invariants are scattered. A new session can read CLAUDE.md and still not know that "the
planner must never call btrfs" or "SQLite failures must never prevent backups."

**Proposal:** Add an "Architectural Invariants" section to CLAUDE.md (or a linked document)
that lists the rules a session must never violate, with references to the ADRs that explain
why.

```markdown
## Architectural Invariants

These rules are load-bearing. Violating them causes architectural damage that compounds.
Each links to an ADR explaining the rationale.

1. **The planner is a pure function.** It reads config and filesystem state through traits.
   It never calls btrfs, writes files, or modifies state. (ADR-100)
2. **All btrfs calls go through BtrfsOps.** No module except btrfs.rs spawns btrfs
   subprocesses. (ADR-101)
3. **Filesystem is source of truth. SQLite is history.** Pin files and snapshot directories
   are authoritative. SQLite failures must never prevent backups. (ADR-102)
4. **Individual subvolume failures never abort the run.** The executor isolates errors
   per subvolume. (ADR-100)
5. **Retention never deletes pinned snapshots.** Defense-in-depth: planner excludes them,
   executor re-checks before deletion. (ADR-105)
6. **Send pipeline captures both exit codes and both stderr streams.** Partial snapshots
   are cleaned up on failure. (ADR-101)
```

This is a quick-reference for sessions that need to make changes without reading every ADR.

### Tool 5: Phase Retrospective Practice

**Problem:** The project has grown from 0 to 216 tests in 3 days with excellent per-feature
reviews, but there has been no cross-cutting retrospective asking "what patterns are
emerging that we should codify or correct?"

**Proposal:** After each major phase (or every ~2 weeks during active development), write
a brief retrospective in `99-reports/` covering:

1. **What architectural patterns emerged?** (e.g., the "pure function + trait + mock" pattern
   used by planner, awareness, heartbeat)
2. **What decisions were made implicitly?** (e.g., the awareness model quietly established
   that new modules follow the pure-function pattern)
3. **What should be codified?** (e.g., write an ADR for the pure-function module pattern)
4. **What tech debt accumulated?** (status.md already tracks this; the retrospective reviews
   whether it's growing or shrinking)

---

## How the Tools Compose

The proposed tools create a complete lifecycle for architectural decisions:

```
Brainstorm (95-ideas/)
    ↓ user ranking
Design Proposal (97-plans/)          ← Tool 2: NEW
    ↓ adversary review
Reviewed proposal (99-reports/)      ← existing practice
    ↓ implementation
Code + tests
    ↓ implementation review
Merged code
    ↓
ADR written for load-bearing decisions  ← Tool 1: NEW
status.md updated (recent decisions)
    ↓ periodically
Decisions graduated from status.md      ← Tool 3: NEW
Invariants updated in CLAUDE.md         ← Tool 4: NEW
Phase retrospective                     ← Tool 5: NEW
```

The key insight: **the existing workflow is already doing most of the work.** The adversary
reviews are catching real problems. The journals are preserving context. The status.md is
routing sessions to the right information. What's missing is the final step: distilling
decisions into findable, permanent, challengeable records (ADRs) and keeping the invariant
documentation current.

---

## Design Tensions in the Current Workflow

### Tension 1: Thoroughness vs. Velocity

The adversary review practice is thorough — every feature gets design + implementation
reviews. But each review is a substantial document (50-150 lines of findings). At the
current development velocity (multiple features per day), the reviews may become a
bottleneck or, worse, may be conducted less carefully to maintain pace.

**Resolution:** The reviews are earning their keep. The clock skew bug, mkdir precondition,
and legacy pin false positives were all caught by reviews. But consider *scoping* reviews:
not every feature needs a full six-dimension review. A low-risk bug fix needs a focused
correctness check, not an architecture assessment. The skill already supports this
(arguments can scope the review), but the practice should explicitly distinguish between
full reviews and focused reviews.

### Tension 2: Living Documents vs. Immutable Records

Status.md is a living document that is also accumulating historical decisions. This creates
a tension: it needs to be concise for new sessions (living document concern) but also
comprehensive for decision traceability (immutable record concern).

**Resolution:** Tool 3 (graduation) resolves this. Status.md stays focused on current state
and recent decisions. ADRs take over as the permanent record for foundational choices.

### Tension 3: Journal Privacy vs. Decision Traceability

Journals are gitignored (correctly, for privacy). But some decisions are only documented in
journals. If the local machine's disk fails before those decisions are distilled into tracked
docs, the reasoning is lost.

**Resolution:** The existing journal → tracked doc workflow handles this, but it depends on
discipline. The phase retrospective (Tool 5) creates a periodic checkpoint that catches any
decisions that were made in journals but not yet distilled into ADRs or status.md.

### Tension 4: CLAUDE.md Size vs. Completeness

CLAUDE.md is auto-loaded and should stay under ~200 lines. But architectural invariants,
module responsibilities, and coding conventions are all competing for space. Adding an
invariant checklist (Tool 4) pushes the document longer.

**Resolution:** Keep the invariant checklist short (6-8 items, one line each). Move the
detailed module responsibility table to a separate reference document if CLAUDE.md grows
beyond 200 lines. The invariant list is more important than the module table — invariants
prevent architectural damage; module tables prevent confusion.

---

## Commendations

**1. The "architectural gates" pattern in status.md.** Gating Priority 4 (protection
promises) on an ADR is exactly right. This prevents building on undefined foundations.
More priorities should have explicit gates.

**2. The adversary review integration.** Using the arch-adversary skill as a standard step
in the design workflow is unusual and effective. The design review → implementation review
cycle catches problems at both the conceptual and code levels. The presentation layer
review's rejection of the trait approach in favor of an enum is a good example of a review
preventing unnecessary abstraction.

**3. The "Not Building" section in status.md.** Explicitly recording what was *rejected*
and why (Tier 2 filesystem-level upper bound, qgroup opportunistic query) is as valuable
as recording what was accepted. This prevents future sessions from re-proposing rejected
ideas without new information.

**4. The decision to decompose the Sentinel.** The vision architecture review identified
that "Sentinel" was actually three systems. This decomposition happened *before* any
Sentinel code was written, saving potentially months of refactoring.

---

## Priority Action Items

1. **Write retroactive ADRs for founding decisions** (ADR-100 through ADR-105). These are
   the most load-bearing decisions in the project and currently have no formal record. Start
   with ADR-100 (planner/executor separation) since it is the "core invariant" mentioned
   throughout the codebase.

2. **Establish ADR numbering for the Urd era.** Document that ADR-100+ is the Urd space.
   Add a brief note to CONTRIBUTING.md's ADR section.

3. **Graduate 25+ older decisions from status.md.** The Recent Decisions table should cover
   the current phase, not the entire project history. Move foundational decisions to ADRs;
   let implementation details live in their review references.

4. **Add architectural invariant checklist to CLAUDE.md.** Six to eight rules, one line
   each, with ADR references. This is the "don't violate these" quick-reference.

5. **Create the design proposal template.** Add to CONTRIBUTING.md's document templates
   section. Use it for the protection promises ADR (already gated), Sentinel decomposition,
   and any future multi-module features.

6. **Write a Phase 1-5 retrospective.** Capture the patterns that emerged (pure-function
   modules, design-review-before-code, trait-based testing seams) and codify them as
   project conventions.

---

## Open Questions

- **ADR granularity:** Should each founding decision get its own ADR, or should related
  decisions be grouped (e.g., one ADR for "data integrity invariants" covering planner
  purity, pin protection, and error isolation)?

- **Review scoping:** Should CONTRIBUTING.md define when a full adversary review is needed
  vs. a focused review? What's the threshold (new module? interface change? risk level?)?

- **Decision table lifecycle:** Should status.md decisions auto-archive after a certain
  period, or should graduation be a manual step during retrospectives?

- **Who writes the mythic voice text for ADRs?** The vision architecture review correctly
  identified that Urd's character requires sustained creative attention. Should ADRs (which
  are public-facing) use the mythic voice, or stay technical?
