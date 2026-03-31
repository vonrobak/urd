# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
589 tests, all passing, clippy clean. Current version: v0.5.0.
Transient retention deployed to htpc-root (2026-03-30). Immediate cleanup built, not yet deployed.

## In Progress

1. **6-E: Promise redundancy encoding** — implemented, reviewed (arch-adversary + simplify +
   post-review), all findings addressed. Ready for commit and PR. 17 new tests.

## Next Up

1. **6-I + 6-N in parallel** — redundancy recommendations + retention policy preview.
   Both designed and reviewed. 1-2 sessions each.
2. **Spindle design post-review update** — incorporate 3 high findings before building.

## Build Queue (Priority 6) — Redundancy Guidance & UX

```
B (merged) → E (ready to merge) → I+N (parallel) → O → H (capstone)
```

| # | Feature | Effort | Status |
|---|---------|--------|--------|
| 6-B | Transient immediate cleanup | 1 session | **Merged** |
| 6-E | Promise redundancy encoding | 1 session | **Built, reviewed, ready to merge** |
| 6-I | Redundancy recommendations | 1-2 sessions | Designed, reviewed |
| 6-N | Retention policy preview | 1 session | Designed, reviewed |
| 6-O | Progressive disclosure | 2 sessions | Designed, reviewed |
| 6-H | Guided setup wizard | 4-5 sessions | Designed, reviewed |
| 6-Sp | Spindle tray icon | 2 sessions | Designed, reviewed |

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest review | [6-E implementation review](../99-reports/2026-03-31-design-e-implementation-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated (6-B partially addresses for transient subvolumes)
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs (addressed by 6-I migration)
- `OpResult::Skipped` is overloaded: four distinct semantics

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
