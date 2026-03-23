# Session Journal: Documentation System and Verified Project State

> **TL;DR:** Designed and implemented a documentation system with dual-audience conventions
> (human memory + Claude Code token efficiency), verified status.md against real system
> data and discovered the bash script is still running production backups (not Urd),
> established cross-repo systemd unit ownership rules.

**Date:** 2026-03-23
**Base commit:** `ed3555e`

## What was done

### Documentation structure (CONTRIBUTING.md)

Built a comprehensive documentation system codified in `CONTRIBUTING.md`. The key design
decisions:

- **Dual-audience writing:** Every document serves both human memory and Claude Code
  context. The TL;DR convention (blockquote summary after title) lets Claude scan documents
  without consuming full token budgets.
- **Information hierarchy:** `CLAUDE.md` (auto-loaded) → `status.md` (read first) →
  specific docs (follow links). This gives fresh Claude sessions a reliable entry point.
- **Immutability spectrum:** ADRs are strictly immutable (supersede only), plans/journals/reports
  are mostly immutable (pragmatic updates allowed), guides/runbooks/supervisor are living documents.
- **Numbering scheme:** 00-89 for stable topic docs, 90-99 for process/project management.
  Gaps are intentional for future growth.
- **Document templates:** Lightweight required structure for each type (journal, report,
  plan, ADR, idea) — enough for consistency without rigidity.

### Directory changes

- Created `docs/95-ideas/`, `docs/10-operations/`, `docs/20-reference/`, `docs/90-archive/`
- Moved `docs/97-plans/PLAN.md` → `docs/96-project-supervisor/roadmap.md` with provenance
  note marking it as the founding artifact to be superseded by `status.md`
- Created `docs/96-project-supervisor/status.md` as the living progress tracker
- Renamed 4 undated reports to match `YYYY-MM-DD-slug.md` convention
- Updated `CLAUDE.md` references to point to new locations

### Status verification against real system

Verified `status.md` claims against actual system state and found a critical inaccuracy:
the original status.md claimed "Urd is the sole backup system" but the bash script is
still running production backups.

Evidence:
- `btrfs-backup-daily.timer` is active and enabled, running at 02:00
- `urd-backup.timer` does not exist in `~/.config/systemd/user/`
- Journal logs from 2026-03-23 02:00 show the bash script running
- The Phase 4 journal explicitly listed the cutover as "What Was NOT Built"

Corrected status.md to reflect reality: Phase 4 code complete, operational cutover not started.

### Cross-repo systemd conventions

Discovered that both `~/projects/urd` and `~/containers` use copy-not-symlink for systemd
unit deployment. Formalized this as a convention in CONTRIBUTING.md with an ownership table
so that Claude Code sessions working on separate repos don't step on each other's units.

Wrote first idea document (`95-ideas/2026-03-23-systemd-unit-drift-check.md`): add a
check to `urd verify` that detects when installed units have drifted from repo source.

## What was learned

- **Documentation claims must be verified against system state.** The gap between "code
  complete" and "operationally deployed" is real and consequential. Status documents should
  distinguish between the two explicitly.
- **Cross-repo coordination needs explicit ownership rules.** When two repos contribute
  systemd units to the same `~/.config/systemd/user/` directory, each repo must know what
  it owns and what it must not touch. This is especially important for AI-assisted sessions
  that lack the implicit context a human operator carries.
- **The copy-not-symlink convention is the right call for backup infrastructure.** A stale
  copy still runs; a broken symlink silently stops backups. For a system protecting data on
  a pool with no redundancy, the reliability tradeoff is clear.

## Open questions

- When should the operational cutover actually begin? The code is ready but the parallel
  run requires monitoring over 1-2 weeks.
- Should `~/containers` documentation be updated proactively to reference Urd, or wait
  until the cutover is complete?
- The `90-archive/` directory will mirror the original directory structure — should this
  be enforced by convention only, or should the archive directories be pre-created?
