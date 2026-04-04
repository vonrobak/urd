# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
857 tests, all passing, clippy clean. Current version: v0.10.0.

**UPI 021 complete.** Sentinel config reload (mtime polling, hot-reload without restart)
and chain-break anomaly guard fix. PR #84 merged. Simplify pass consolidated duplicate
`default_config_path()` in migrate.rs.

**Deployment notes:**
- v0.10.0 tagged but not yet pushed/installed
- After install: hand-edit production config `local_retention = "transient"` →
  `local_snapshots = false` before next timer run
- UPI 021 fix means sentinel will pick up the config change automatically after install

## In Progress

Nothing active.

## Recently Completed

**UPI 021: The Living Daemon** (2026-04-04)
   - 021-a: `total > 0` guard in `detect_simultaneous_chain_breaks()` — prevents false
     anomaly when drives disconnect
   - 021-b: `ConfigChanged` event, mtime polling, `try_reload_config()` with cached path
     refresh — sentinel hot-reloads config without restart
   - 8 new tests, simplify pass fixed migrate.rs duplication

## Next Up

**Immediate: Push v0.10.0 and deploy** (see session journal for verification steps)

**Then sequential (Phase E: Make the invisible worker smart):**
1. **E1: UPI 013** — Btrfs pipeline (compressed sends, sync after delete) ~0.25 session
2. **E2: UPI 018** — External-only runtime (fix false degraded/broken for local_snapshots=false) ~0.5 session
3. **E3: UPI 020** — Context-aware suggestions (compute_advice pure function) ~0.5 session

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

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
- ByteSize Display uses `{:.1}` formatting — `urd migrate` emits "10.0GB" not "10GB"; consider clean display mode
- VersionProbe error message says "failed to read config_version" for TOML syntax errors — UX polish
