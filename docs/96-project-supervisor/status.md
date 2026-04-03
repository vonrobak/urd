# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
725 tests, all passing, clippy clean. Current version: v0.8.1.

**Phase A complete (UPI 004 + 005).** Token safety gate blocks sends to drives with
missing identity tokens when SQLite has a stored record (cloned/swapped drive detection).
assess() now respects per-subvolume `drives` scoping — fixes false degradation.
Local-only subvolumes display as `[LOCAL]` instead of `[OFF] Disabled`.
PR #68 shipped, v0.8.1 tagged.

**Deployment notes:**
- Systemd timer needs `--auto` added to `ExecStart` line (pending since v0.8.0)
- After merging PR #68: `cargo install --path .` to deploy v0.8.1

## In Progress

Nothing active.

## Next Up

1. **Phase B: Make communication honest (v0.8.2)** — ~0.75 session total
   - UPI 007: Safety gate communication (`[DEFERRED]` replaces `[FAILED]`)
   - UPI 008: Doctor pin-age correlation (fix contradictory UUID advice)
   - Designs: grill-me complete, ready for /prepare
2. **Phase C: Give drives a face (v0.9.0)** — ~1-2 sessions total
   - UPI 009: `urd drives` subcommand
   - UPI 006: Drive reconnection notifications
3. **Untracked docs commit** — 6 design docs, brainstorm, Steve reviews, test report
   from the 2026-04-02 design session remain untracked. Commit as docs PR.

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Design docs | [95-ideas/](../95-ideas/) |
| Review reports | [99-reports/](../99-reports/) |
| Historic roadmap (pre-UPI) | [archived](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
