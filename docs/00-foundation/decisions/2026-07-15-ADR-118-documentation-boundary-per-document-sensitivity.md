---
type: ADR
title: Documentation Boundary as Per-Document Sensitivity
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-07-15'
timestamp: '2026-07-15T12:39:55+02:00'
---
# ADR-118: Documentation Boundary as Per-Document Sensitivity

> **TL;DR:** Urd's six internal doc directories (`90-archive`, `95-ideas`,
> `96-project-supervisor`, `97-plans`, `98-journals`, `99-reports`), `contributing-internal.md`,
> and — as an urd-specific extension of the containers model — `CLAUDE.md` itself move into
> a private Obsidian vault (`~/Huldr/projects/urd/`,
> bare git dir `~/.huldr.git`, a private Forgejo instance) and reach this repo only
> through gitignored symlinks. The boundary is physical (`git add` here cannot stage vault
> content) with a configuration backstop (`scripts/check-vault-boundary.sh` rejects any
> staged file whose frontmatter declares `sensitivity: internal|secret`). This adopts the
> model ratified in the containers repo's ADR-043 (per-document `sensitivity:` frontmatter,
> not a directory-shaped trust boundary), with urd-specific deltas recorded below.

**Date:** 2026-07-15
**Status:** Accepted
**Adopts:** containers ADR-043 (`docs/40-monitoring-and-documentation/decisions/2026-07-14-ADR-043-documentation-boundary-per-document-sensitivity.md`, github.com/vonrobak/containers) — same per-document sensitivity model, same vault architecture.

## Context

Urd's six internal doc directories were never tracked by git — a `.gitignore` entry has
covered them from the start, holding roughly 767 files (~151 design/brainstorm docs, ~72
plans, ~276 journals, ~260 reports, ~8 archived, 5 project-supervisor files) plus a 33KB
internal doc-conventions guide. This content lived only on the operator's machine, with no
backup, no cross-device access, and no way for other project bundles (homelab, htpc-mgmt,
jern-mgmt) to reference it.

The containers repo solved the equivalent problem for its own internal docs by moving them
into a private Obsidian vault reached through gitignored symlinks, governed by per-document
`sensitivity:` frontmatter rather than a directory-shaped public/private line (ADR-043,
trajectory T2 chosen at 87/100 over hold-the-line, vault-first, export-pipeline, and
full-privatization alternatives). urd is the next project bundle to make the same move.

urd's starting position is materially simpler than containers' was: nothing here was ever
tracked, so there is no untracking commit, no history boundary a checkout can cross, and no
public paper trail to scrub (containers' L-087 — a checkout replacing a symlink with a real
directory when it crosses the untracking commit — cannot fire in its original form here).
The move is a pure local relocation, not a two-PR untrack-then-symlink sequence.

## Decision

### Adopt the vault architecture and per-document sensitivity model unchanged

- Vault-as-truth: `docs/{90-archive,95-ideas,96-project-supervisor,97-plans,98-journals,99-reports}`
  and `docs/contributing-internal.md` become gitignored symlinks into
  `~/Huldr/projects/urd/`. All existing paths keep resolving.
- Every vault document carries OKF frontmatter with a required `sensitivity:
  public|internal|secret` field — never guessed, unsure defaults to `internal`.
  `~/Huldr/conventions.md` governs the full schema and git discipline; it is the vault-side
  companion to this repo's `CONTRIBUTING.md`.
- The three-zone link policy (vault→vault liberal, public→public relative,
  public→vault never as a path, vault→public free) applies unchanged.
- `scripts/check-vault-boundary.sh` (ported from containers) is the pre-commit backstop:
  it greps staged blobs for `^sensitivity: (internal|secret)` and fails the commit. It runs
  as the second check in `scripts/pre-commit.sh`, a new dispatcher that calls
  `pre-commit-pii.sh` then `check-vault-boundary.sh` in sequence — `install-hooks.sh` now
  symlinks `.git/hooks/pre-commit` to the dispatcher instead of directly to the PII script,
  so a future third check is one more dispatcher line, not a rename of an existing script.

### Deltas from containers ADR-043

1. **`docs/96-project-supervisor/` is in scope, not carved out.** `status.md`,
   `roadmap.md`, and `registry.md` are already gitignored today — a fresh clone (or CI)
   already cannot read them despite `CLAUDE.md`'s "Orient Yourself" section naming
   `status.md` as the first thing any session should read. Moving them into the vault
   changes where the bytes live, not what a fresh clone can already reach; the property is
   pre-existing, not introduced by this ADR.

2. **`docs/99-reports/` is tracked in vault git — the opposite of containers' homelab
   bundle.** Homelab's `99-reports/` is automated machine-churn (script output) and stays
   untracked. Urd's `99-reports/` (~260 files) is narrative prose written during
   design/implementation/adversary review — a documentation record of real decisions, not
   churn. It is committed like every other tier. (Recorded here so a future cross-bundle
   audit does not "fix" this back to match homelab — the two bundles differ by content
   character, not by inconsistency.)

3. **`~/Huldr/conventions.md`'s controlled `type:` list gains two entries: `Brainstorm`
   and `Design`.** The pre-ADR-043 list (Journal | Plan | Report | Guide | ADR | Handoff |
   Entity) has no natural fit for `95-ideas/`'s two dominant, genuinely distinct genres —
   divergent idea generation (`/brainstorm` output, 22 files) versus a structured spec with
   resolved decisions (`/design` output plus arc-level `/grill-me` docs, 117 files). The
   `type:` field exists so an LLM can find real relations across bundles; it should reflect
   document genre, not mirror urd's own directory-numbering convention (which homelab
   doesn't share and needn't). A handful of freeform one-off notes with no skill-prefix
   convention are classified by content, not by a mechanical rule.

4. **Public-tier (`00-foundation/`, `10-operations/`, `20-reference/`) gets full OKF
   frontmatter**, matching containers' choice for its 123 public docs: `type`,
   `sensitivity: public`, `status`, dates — added on top of, not replacing, the existing
   house style (H1 + `> **TL;DR:**` + bold metadata lines). This enables the same
   Obsidian audit views fleet-wide at the cost of one mechanical pass over ~40 files.

5. **No merge-strategy change.** Containers moved to merge-commit-only (its own ADR-038)
   because squash merges replaced the operator's SSH signature with GitHub's web-flow key,
   and its worktree is live-running config. Neither condition holds for urd — normal
   software project, no live-config constraint — so urd's existing squash/PR-numbered
   history is untouched by this ADR.

6. **No untracking phase for the six doc directories.** Unlike containers'
   stop-forward-gitignore-then-untrack sequence, urd's directories were never tracked. The
   migration is: fix six gitignore patterns from trailing-slash to slash-free (a trailing
   slash matches directories only, not the symlinks these become — the single most likely
   silent-failure mode), then `mv` the directories into the vault and symlink back,
   verified with a write-through test per symlink. No repo commit depends on the move
   itself.

7. **`CLAUDE.md` itself is untracked and moves into the vault — a departure from
   containers, which keeps its `CLAUDE.md` public and tracked.** `CLAUDE.md` is loaded
   automatically by every AI-assisted session in this repo, which makes a public, mergeable
   copy a real attack surface: an external PR that alters it (if merged) would have its
   instructions picked up by the next session that reads it, with no separate review lens
   for "does this instruction look like an operator would write it." Untracking it removes
   that surface entirely — the vault (reached only through a gitignored symlink) is the only
   place `CLAUDE.md` can be edited from. The cost: external contributors and CI checkouts
   lose the architecture/conventions doc entirely, not just internal notes, and this is a
   genuine untracking commit (`git rm --cached CLAUDE.md`) rather than a pure local move —
   prior public versions remain visible in GitHub history, which cannot be un-published.
   This delta is scoped to urd only; whether to apply it to containers or other repos is a
   separate future decision, not part of this ADR.

## Consequences

### Positive

- **Internal docs get real backup, cross-device access, and cross-bundle visibility**
  (other project bundles can reference urd's design history directly), matching what
  containers already gained.
- **The public repo's doc-check tooling is unaffected.** `check-docs.sh` (git-ls-files-only,
  already documents that a tracked link into a gitignored area "correctly" flags broken)
  and `check-registry.sh` (already designed to degrade gracefully when
  `registry.md`/`95-ideas/` are absent, e.g. in CI) both already handle the
  present-but-possibly-dangling-symlink case by construction — verified by reading both
  scripts, not merely assumed.
- **No crates.io publish step exists**, so cargo packaging never traverses `docs/` at all;
  the symlink-in-a-package-manifest risk containers had to reason about doesn't apply here.

### Negative

- **`~/Huldr/conventions.md` is a fleet-shared file.** Adding `Brainstorm`/`Design` types
  changes a file containers also depends on, even though containers has no `95-ideas/`
  tier to use them. The addition is purely additive (no existing type renamed or removed),
  so it cannot break containers' existing frontmatter.
- **A second doc-boundary check to maintain.** `scripts/pre-commit.sh` is now a two-line
  dispatcher instead of a single symlink target — negligible cost, but one more file in the
  hook chain than before.
- **External contributors and CI lose `CLAUDE.md` entirely, not just internal notes**
  (delta 7). A fresh clone of the public repo, or a contributor without vault access, now
  sees no architecture/conventions guidance at all. This is the deliberate trade for closing
  the public-instruction-injection surface; `CONTRIBUTING.md` remains public and unaffected.

### Neutral

- **No scrubbing work.** Unlike containers (where already-public RFC1918 topology made
  prose-scrubbing largely theater), urd's internal docs were never public and are moving
  into a private vault — there is nothing to scrub for the migration itself. Classification
  (assigning `sensitivity:`) is the work, not scrubbing.

## Related

- **containers ADR-043** — the adopted model (github.com/vonrobak/containers,
  `docs/40-monitoring-and-documentation/decisions/2026-07-14-ADR-043-documentation-boundary-per-document-sensitivity.md`).
- **ADR-105** — on-disk contract for backward compatibility; this ADR does not change any
  on-disk contract, only where non-contractual internal documentation physically resides.
- Migration guidance and the full decision interview:
  `docs/97-plans/2026-07-15-docs-vault-migration-guidance.md`.
