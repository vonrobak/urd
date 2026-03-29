# Urd Project Status

> This is a short current-state document. Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
462 tests, all passing, clippy clean. Current version: v0.4.0.

## In Progress

- **UX-1 complete, uncommitted.** Structural headings (D5) + collapsed skip reasons (D1)
  in `urd plan` output. `SkipCategory` enum in output.rs, grouped rendering in voice.rs,
  JSON daemon output enriched with category field. Ready to commit.
- **Cross-drive fallback infrastructure** (from previous session) also uncommitted in
  `state.rs`/`plan.rs` — methods not yet called, built for UX-2.

## Next Up

1. **Commit UX-1 + cross-drive fallback** — all changes are uncommitted, tests pass.
2. **UX-2** — estimated send sizes in plan output (D2+D3), uses cross-drive fallback.
   Design: `docs/95-ideas/2026-03-29-design-d2-estimated-sizes.md`.
3. **UX-3** — rich progress display (P1) + ETA (P3).
   Design: `docs/95-ideas/2026-03-29-design-p1-rich-progress-display.md`.

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |
| Design review | [Progress display design review](../99-reports/2026-03-29-progress-display-design-review.md) |
| Post-review | [Cross-drive fallback review](../99-reports/2026-03-29-post-review-cross-drive-fallback-review.md) |
| UX designs | `docs/95-ideas/2026-03-29-design-{d1,d2,d5,p1,p3}.md` |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Calibrated sizes use `du -sb` but btrfs send streams are ~10% larger — affects size estimates
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs
- `parse_duration_to_minutes` lacks unit test for cross-unit comparisons (d vs h)

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
