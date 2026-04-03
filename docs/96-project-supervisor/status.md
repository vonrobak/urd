# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
833 tests, all passing, clippy clean. Current version: v0.10.0.

**UPI 010 complete (all sessions + 010-a).** Config schema v1 fully implemented:
V1 parser, validation, `urd migrate`, `local_snapshots = false`. PR #75-80 merged.
The full legacy→v1 config pipeline works: parse either schema, migrate between them,
semantic equivalence confirmed. Named-level opacity has no exceptions.

**Deployment notes:**
- v0.10.0 tagged but not yet pushed/installed
- After install: hand-edit production config `local_retention = "transient"` →
  `local_snapshots = false` before next timer run

## In Progress

Nothing active.

## Recently Completed

**UPI 010-a: `local_snapshots = false`** (2026-04-03)
   - Replaced `local_retention = "transient"` with boolean opt-out in v1 config
   - Eliminated the only exception to named-level opacity
   - Migration handles custom+transient and named+transient→custom with baked fields
   - 12 new tests, simplify pass fixed 5 issues including a double-count bug

## Next Up

**Immediate: Push v0.10.0 and deploy** (see "When you return" in session journal)

**Track A: v0.10.0 test session** (calendar time — live with the tool)
   - Live with v0.10.0 for several days (timer, Sentinel, drive plug/unplug cycles)
   - Output: prioritized issue list → targeted fix phase if needed

**Then sequential:**
1. Fix test session findings (~0-2 sessions)
2. **Phase D: Progressive disclosure + The Encounter** — ~6-8 sessions

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
