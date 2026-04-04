# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
871 tests, all passing, clippy clean. Current version: v0.10.0.

**UPI 020 complete.** Context-aware suggestions: `compute_advice()` pure function in
awareness.rs replaces static lookup tables. Doctor, status, and bare `urd` now show
specific commands based on chain health, drive state, and subvolume config. PR #85 merged.

**Deployment notes:**
- v0.10.0 tagged but not yet pushed/installed
- After install: hand-edit production config `local_retention = "transient"` →
  `local_snapshots = false` before next timer run
- UPI 021 fix means sentinel will pick up the config change automatically after install

## In Progress

Nothing active.

## Recently Completed

**UPI 020: The Doctor Knows** (2026-04-04)
   - `ActionableAdvice` struct + 8-branch decision tree in awareness.rs
   - Doctor shows chain-break reasons and `--force-full` when appropriate
   - Status/default show inline fix commands or "run urd doctor" for multiple issues
   - `external_only` flag uses send age for transient subvolumes
   - 17 new tests, simplify pass fixed 3 issues

## Next Up

**Immediate: Push v0.10.0 and deploy** (see session journal for verification steps)

**Then sequential (Phase E: Make the invisible worker smart):**
1. **E1: UPI 013** — Btrfs pipeline (compressed sends, sync after delete) ~0.25 session
2. **E2: UPI 018** — External-only runtime (fix false degraded/broken for local_snapshots=false) ~0.5 session
3. **E4: UPI 014** — Skip unchanged subvolumes ~0.5 session

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |

## Known Issues

- htpc-root shows false "degraded"/"broken chain" — awareness doesn't account for `local_snapshots = false` (UPI 018 will fix)
- WD-18TB and WD-18TB1 share BTRFS UUID from cloning — needs `btrfstune -u` when offsite drive returns
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
