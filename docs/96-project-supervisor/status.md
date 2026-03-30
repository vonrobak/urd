# Urd Project Status

> This is a short current-state document. Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
507 tests, all passing, clippy clean. Current version: v0.4.0.

## In Progress

- **UX-3 complete, PR #44 open** (2026-03-30). Rich progress display during backup:
  subvolume context, send counter, completion trail, ETA for full sends. Ready to merge.

## Build Queue

Seven-step sequence resolved 2026-03-29. See [roadmap.md Priority 5.5](roadmap.md) for
full details, design decisions, and review findings.

1. ~~**HSD-A**~~ — drive session tokens + chain health as awareness input. **Done.**
2. ~~**VFM-A**~~ — `OperationalHealth` enum, two-axis CLI rendering. **Done.**
3. ~~**Sentinel Session 3**~~ — hardening + notification deduplication. **Done.**
4. ~~**UX-1**~~ — plan output: structural headings (D5) + collapsed skips (D1). **Done.**
5. ~~**UX-2**~~ — plan output: estimated send sizes (D2+D3), cross-drive fallback (S1 fix). **Done.**
6. ~~**UX-3**~~ — plan output: progress display: rich context (P1) + ETA (P3). **Done (PR #44).**
7. **HSD-B** — sentinel chain-break detection + full-send gate (Norman escalation). **start here**
   - Reference incident: `docs/98-journals/2026-03-29-clone-drive-incident-analysis.md`
8. **VFM-B** — sentinel visual state in state file + health notifications.
9. **Transient snapshots** — `local_retention = "transient"` for NVMe space pressure.
10. **Tray icon (Spindle)** — reads sentinel-state.json, 4 static icons.

Designs: `docs/95-ideas/2026-03-29-design-*.md`.
Reviews: `docs/99-reports/2026-03-30-ux3-progress-display-review.md`.

**Later:** Config system migration (ADR-111), shell completions (6a).

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |
| Latest reviews | [UX-3 review](../99-reports/2026-03-30-ux3-progress-display-review.md), [UX-2 review](../99-reports/2026-03-30-ux2-estimated-sizes-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Calibrated sizes use `du -sb` but btrfs send streams are ~10% larger — affects size estimates
- Per-drive pin protection for external retention: all-drives-union is conservative but suboptimal for space
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs
- `drive_connections` table has no retention policy (negligible for years at ~1000 rows/year)
- `render_skipped_block` (backup summary) uses ad-hoc string grouping; could adopt `SkipCategory`
- Progress completion line byte count lags true total by up to one poll interval (~45 MB at USB3 speeds)

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
