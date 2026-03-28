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

## Build Queue

Seven-step sequence resolved 2026-03-29. See [roadmap.md Priority 5.5](roadmap.md) for
full details, design decisions, and review findings.

1. **HSD-A** — drive session tokens + chain health as pre-computed awareness input.
   Unblocks VFM-A. [Design](../95-ideas/2026-03-28-design-hardware-swap-defenses.md) ← **start here**
2. **VFM-A** — `OperationalHealth` enum, two-axis CLI rendering.
   Fixes false reassurance problem. [Design](../95-ideas/2026-03-28-design-visual-feedback-model.md)
3. **Sentinel Session 3** — hardening + notification deduplication.
   Needed by HSD-B and VFM-B, not by earlier steps.
4. **HSD-B** — sentinel chain-break detection + full-send gate (Norman escalation, never auto-proceed).
5. **VFM-B** — sentinel visual state in state file + health notifications.
6. **Transient snapshots** — `local_retention = "transient"` for NVMe space pressure.
7. **Tray icon (Spindle)** — reads sentinel-state.json, 4 static icons.

Key design decisions already resolved:
- Chain health: facade pattern — callers pre-compute, awareness is single health facade
- Full-send gate: never auto-proceed, escalate notification urgency (Norman principles)

**Later:** Config system migration (ADR-111), shell completions (6a).

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |
| Latest reviews | [HSD review](../99-reports/2026-03-28-hardware-swap-defenses-design-review.md), [VFM review](../99-reports/2026-03-28-visual-feedback-model-design-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (10 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- WD-18TB UUID needs adding to config when drive is next mounted
- Orphaned snapshot `20250422-multimedia` on WD-18TB1 — clean up or let crash recovery handle
- Per-drive pin protection for external retention: all-drives-union is conservative but suboptimal for space

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
