# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
906 tests, all passing, clippy clean. Current version: v0.10.0.

**UPI 014 complete.** Skip unchanged subvolumes: BTRFS generation comparison avoids
creating identical snapshots for quiet subvolumes. Plan output shows `[SAME]` tag with
elapsed time. `--force-snapshot` overrides. Fail-open on generation query failures.
PR #88 merged.

**Deployment notes:**
- v0.10.0 tagged but not yet pushed/installed
- After install: hand-edit production config `local_retention = "transient"` →
  `local_snapshots = false` before next timer run
- UPI 021 fix means sentinel will pick up the config change automatically after install
- Pre-deploy check for 013: run `btrfs send --help` as unprivileged user to verify no-sudo probe
- First sentinel tick after deploy will emit one-time HealthRecovered for htpc-root (expected)

## In Progress

Nothing active.

## Recently Completed

**UPI 014: Skip Unchanged Subvolumes** (2026-04-05)
   - `parse_generation()` + `subvolume_generation()` standalone in btrfs.rs
   - Generation comparison in `plan_local_snapshot()` with fail-open semantics
   - `SkipCategory::Unchanged` with `[SAME]` tag, suppressed in backup summary
   - `--force-snapshot` flag on `urd plan` and `urd backup`
   - Simplify: refactored `plan_local_snapshot` to take `&PlanFilters` (parameter sprawl fix)
   - 11 new tests

## Next Up

**Immediate: Push v0.10.0 and deploy** (see session journal for verification steps)

**Then sequential (Phase E: Make the invisible worker smart):**
1. **E5: UPI 016-auto** — Emergency space response (automatic mode) ~0.5 session

**12 unreleased changes in CHANGELOG.md — consider `/release` soon.**

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |

## Known Issues

- WD-18TB and WD-18TB1 share BTRFS UUID from cloning — needs `btrfstune -u` when offsite drive returns
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- `compute_health` at 8 params — consider struct grouping in next awareness.rs change
