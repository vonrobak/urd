# Urd Project Status

> This is a short current-state document. Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
563 tests, all passing, clippy clean. Current version: v0.5.0.

## Recently Completed

- **Transient awareness fix** (2026-03-30). Awareness model now understands transient
  retention: local status = Protected (defers to external send freshness). Includes
  `clamp_age()` extraction, clock-skew advisory for transient, guard for transient+no-sends
  edge case. 6 new tests. Reviewed and post-review fixes applied.
- **Transient snapshots** (2026-03-30). `local_retention = "transient"` — delete local
  snapshots after external send, keep only pinned chain parents. For space-constrained
  NVMe volumes (htpc-root). Merged. Ready to deploy to production config.

## Next Up

1. **Deploy transient to htpc-root** — update production `urd.toml`, monitor after next
   backup cycle. The awareness gap is resolved; this is now a config change only.
2. **Tray icon (Spindle)** — reads sentinel-state.json visual_state, 4 static icons.
   Brainstorm exists, needs design doc. `docs/95-ideas/2026-03-28-brainstorm-tray-icon-spindle.md`
3. **Shell completions (6a)** — `clap_complete` for static completions. Low effort, no design needed.

## Build Queue (Priority 5.5) — Complete

All 10 items in the 5.5 build sequence are done. Designs: `docs/95-ideas/2026-03-2*-design-*.md`.

| # | Feature | Status |
|---|---------|--------|
| 1–8 | HSD-A, VFM-A, Session 3, UX-1/2/3, HSD-B, VFM-B | Done (v0.4.1–v0.4.3) |
| 9 | Transient snapshots | Done (merged, awareness fix applied) |
| 10 | Tray icon (Spindle) | Brainstormed, not designed |

**Later:** Config system migration (ADR-111), shell completions (6a).

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |
| Latest reviews | [Transient awareness review](../99-reports/2026-03-30-transient-awareness-fix-review.md), [Transient snapshots review](../99-reports/2026-03-30-transient-snapshots-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Calibrated sizes use `du -sb` but btrfs send streams are ~10% larger — affects size estimates
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs
- `OpResult::Skipped` is overloaded: four distinct semantics
- `urd sentinel status` interactive mode doesn't render health/visual_state fields yet

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
