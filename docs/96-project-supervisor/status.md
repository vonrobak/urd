# Urd Project Status

> This is a short current-state document. Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
389 tests, all passing, clippy clean. Current version: v0.3.0.

## In Progress

- **Operational cutover monitoring.** Target: 2026-04-01 (1 week from cutover).
  Grafana dashboard continuity not yet verified. Bash script cleanup pending in ~/containers.
- **Sentinel Session 3** next: hardening + notification deduplication.
  Sessions 1-2 complete (pure state machine, I/O runner, CLI).
  [Sentinel design](../95-ideas/2026-03-27-design-sentinel-implementation.md)

## Next Up

1. **Sentinel active mode** (Session 4) — auto-trigger backups on drive mount events.
   `should_trigger_backup()` and `TriggerPermission` already designed in Session 1.
2. **Config system migration** (ADR-111) — target architecture defined, not yet implemented.
   Legacy schema still in use. Implement incrementally; taxonomy rework may change schema.
3. **Shell completions** (Priority 6a) — `clap_complete` for static completions.

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |
| Latest review | [Sentinel Session 2 impl review](../99-reports/2026-03-27-sentinel-session2-implementation-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (10 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- WD-18TB UUID needs adding to config when drive is next mounted
- Orphaned snapshot `20250422-multimedia` on WD-18TB1 — clean up or let crash recovery handle
- Per-drive pin protection for external retention: all-drives-union is conservative but suboptimal for space

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
