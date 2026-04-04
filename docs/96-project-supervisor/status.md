# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
879 tests, all passing, clippy clean. Current version: v0.10.0.

**UPI 013 complete.** Compressed send pass-through (`--compressed-data` auto-detected at
startup, no config knob) and post-delete sync (`btrfs subvolume sync` after each retention
delete for accurate space accounting). PR #86 merged.

**Deployment notes:**
- v0.10.0 tagged but not yet pushed/installed
- After install: hand-edit production config `local_retention = "transient"` →
  `local_snapshots = false` before next timer run
- UPI 021 fix means sentinel will pick up the config change automatically after install
- Pre-deploy check for 013: run `btrfs send --help` as unprivileged user to verify no-sudo probe

## In Progress

Nothing active.

## Recently Completed

**UPI 013: Btrfs Pipeline Improvements** (2026-04-05)
   - 013-a: `SystemBtrfs::probe()` detects `--compressed-data` support, `RealBtrfs` injects flag
   - 013-b: Per-delete `sync_subvolumes` in executor, fail-open (ADR-107)
   - Simplify pass: hoisted probe out of loop in init.rs
   - 8 new tests

## Next Up

**Immediate: Push v0.10.0 and deploy** (see session journal for verification steps)

**Then sequential (Phase E: Make the invisible worker smart):**
1. **E2: UPI 018** — External-only runtime (fix false degraded/broken for local_snapshots=false) ~0.5 session
2. **E4: UPI 014** — Skip unchanged subvolumes ~0.5 session

**9 unreleased changes in CHANGELOG.md — consider `/release` soon.**

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
