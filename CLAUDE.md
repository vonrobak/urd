# CLAUDE.md

## Vision

**Urd** (Old Norse: Urðr) — a BTRFS Time Machine for Linux, written in Rust. Urd
preserves filesystem history silently and faithfully.

**Design north star.** Every feature must pass three tests: (1) does it make the user's
data safer? (2) does it reduce the attention the user must spend on backups? (3) does Urd
do no harm to the host she protects? A backup tool that causes storage pressure or I/O
contention on the system it protects has failed its promise. When Urd and the host
conflict, the host wins. Features that add complexity the user must manage need very
strong justification (ADR-113).

**Two modes.** *The invisible worker* runs autonomously (systemd timer ~04:00 + the
Sentinel daemon for sub-hourly monitoring, drive detection, overdue alerts) — silence
means data is safe. *The invoked norn* (`urd status`, `urd get`, `urd restore`) speaks
with authority and clarity, surfacing problems only when they matter.

**Voice and promises.** The mythic voice (the character of the norn) belongs entirely in
the presentation layer (`voice/`), never in config or data structures. Urd thinks in
*promises*, not operations: the user declares what matters; Urd derives the operations.
Promise states (PROTECTED / AT RISK / UNPROTECTED) are the universal language. Brevity is
part of the promise: Urd says only what is necessary for fate to be sealed. Every word
carries consequential weight. Taxonomy and full vocabulary: `docs/00-foundation/glossary.md`.

## Orient Yourself

Read `docs/96-project-supervisor/status.md` first (short current-state doc), then follow
links: `roadmap.md` (priorities), `registry.md` (UPI → artifacts). For the architecture
diagram and the authoritative module-responsibility table, see
`docs/00-foundation/architecture.md`. For controlled vocabulary, see
`docs/00-foundation/glossary.md`. For documentation structure and conventions, see
`docs/contributing-internal.md`.

## Architecture

### Core Flow

```
config  -->  plan (pure function)  -->  execute (I/O)  -->  btrfs (sudo)
```

All backup logic flows through config -> plan -> execute. No exceptions.

### Modules (compact)

One clause each. Full responsibilities table (`Does` / `Does NOT`) and the flow diagram
live in `docs/00-foundation/architecture.md`.

- `config.rs` — parse/validate TOML, resolve subvolumes
- `cli.rs` / `cli_validation.rs` — clap command surface / pre-planner input guards
- `types.rs` — domain types, parsing, `derive_policy()`
- `plan.rs` — decide operations (pure); `executor.rs` — run them, isolate failures
- `btrfs.rs` — sole path to `sudo btrfs` (`BtrfsOps: BtrfsRead`)
- `observation.rs` — read-side query traits (`FilesystemQuery` + `HistoryQuery`)
- `retention.rs` — which snapshots to keep/delete (pure)
- `awareness.rs` — promise state ("is my data safe?"); `advice.rs` — what to do about it
- `recommendation.rs` — headroom-aware retention-shape advice (ADR-115)
- `storage_critical.rs` — storage-state tiers for the Do-No-Harm arc (ADR-113)
- `drift.rs` — churn aggregation; `rotation.rs` — offsite rotation cadence + freshness window; `preflight.rs` — achievability advisories
- `chain.rs` — pin files; `state.rs` — SQLite history (granular wrappers)
- `drives.rs` / `pools.rs` — drive + BTRFS-pool detection
- `output.rs` / `voice/` — structured output types / mythic-voice rendering
- `events.rs` / `voice_events.rs` — typed event payloads / their renderer
- `notify.rs`, `heartbeat.rs`, `metrics.rs` — notification + health surfaces
- `lock.rs` — shared advisory lock; `error.rs` — error types + `translate_btrfs_error()`
- `sentinel.rs` / `sentinel_runner.rs` — daemon state machine (pure) / its I/O wrapper
- `commands/` — CLI handlers that wire pure modules to I/O

### Architectural Invariants

These rules are load-bearing. Violating them causes architectural damage that compounds.
Each references an ADR in `docs/00-foundation/decisions/`.

1. **The planner never modifies anything.** Pure function: config + state in, plan out. (ADR-100)
2. **All btrfs calls go through `BtrfsOps`.** No other module spawns btrfs subprocesses. (ADR-101)
3. **Filesystem is truth, SQLite is history.** Pin files and snapshot dirs are authoritative. SQLite failures never prevent backups. (ADR-102)
4. **Individual subvolume failures never abort the run.** The executor isolates errors per subvolume. (ADR-100)
5. **Retention never deletes pinned snapshots.** Three independent layers: unsent protection, planner exclusion, executor re-check. (ADR-106)
6. **Backups fail open; deletions fail closed.** Proceed on missing data, never delete what can't be confirmed safe. (ADR-107)
7. **Core logic modules are pure functions.** Planner, awareness, retention, voice — inputs in, outputs out, no I/O. (ADR-108)
8. **Validate structure at load time; isolate failures at runtime.** Structural config errors refuse to start; runtime conditions (unmounted drive, full filesystem) skip per-unit and report. (ADR-109, ADR-111)
9. **Backward-compatibility contracts are sacred.** On-disk data format changes require an ADR with a migration plan; config schema changes use `urd migrate`. (ADR-105, ADR-111)
10. **Named protection levels are opaque or they don't exist.** No per-field overrides on named levels; custom is first-class. (ADR-110, ADR-111)

### Error Handling

- `thiserror` for types in `error.rs`; `anyhow` in `main.rs` / the CLI layer.
- Subvolume failures must not abort the run; failed sends clean up partial snapshots at
  the destination; SQLite failures log a warning and continue.
- `translate_btrfs_error()` converts btrfs stderr into an actionable `BtrfsErrorDetail`.

## Coding Conventions

- Standard Rust: `snake_case` functions, `CamelCase` types.
- `cargo clippy --all-targets -- -D warnings` (all warnings are errors; `--all-targets` covers test code).
- `rustfmt` before committing — but do **not** run `cargo fmt` repo-wide (see Project State).
- Strong types over primitives: `SnapshotName` not `String`, `Tier` not `u8`.
- `#[must_use]` where return values matter; derive `Debug` everywhere, `Clone`/`PartialEq`/`Eq` where sensible.
- No `unsafe`. No `unwrap()`/`expect()` in library code (tests and `main.rs` only).
- Fallback values must be *safe*, not just *convenient*. `unwrap_or(0)` is wrong when 0 is in-range but semantically meaningless (bytes transferred, age in days) — use `Option` for absence.
- Daemon (sentinel) lifecycle events use `warn!()` to be visible at default log levels.
- Doc filenames: lowercase kebab-case (exceptions: CLAUDE.md, README.md, CONTRIBUTING.md).

## Testing

- Unit tests: `#[cfg(test)] mod tests` in-file (`cargo test`). Integration tests in
  `tests/integration/`, `#[ignore]` by default (`cargo test -- --ignored`).
- Use `MockBtrfs` / `MockFileSystemState` for anything that would call btrfs or read the
  filesystem. Path-constructing code should also have `tempfile::TempDir` tests — mocks are
  blind to filesystem preconditions like missing parent directories.
- Test retention logic exhaustively — it protects against data loss.
- Vertical slicing: one test, implement to pass, repeat. Never all tests then all impl.
- **Symmetric fixes need symmetric reviews.** When a bug is rooted in a shared planning or
  rendering pattern, grep for the pattern in adjacent code paths before closing the fix
  (see `docs/98-journals/2026-05-02-stranded-snapshots-non-transient-planner.md`).
- Comprehensive suite, all passing, clippy clean. (Exact count lives in status.md.)

## Config & Backward Compatibility

- **Config (ADR-111):** a tri-parser — legacy (no `config_version`), v1, and v2 — all
  producing one internal `Config`, so downstream code is schema-agnostic. `urd migrate`
  auto-targets the latest schema (v2). Mechanics and rationale: ADR-111. Examples:
  `config/urd.toml.{example,v1.example,v2.example}`.
- **On-disk contracts (ADR-105) — changes require an ADR + migration plan:** snapshot names
  (`YYYYMMDD-HHMM-{short_name}`; legacy `YYYYMMDD-{short_name}` parsed as midnight; ordered by
  datetime), snapshot dirs (`{snapshot_root}/{name}/`), pin files
  (`.last-external-parent-{DRIVE_LABEL}`), and Prometheus metric names/labels/semantics.
  Field-level detail: `docs/20-reference/` (cli, metrics, heartbeat-schema).
- **External interface:** a homelab monitoring stack consumes Urd's metrics + heartbeat.
  Changes to metric names/labels, the heartbeat schema, systemd unit names, or `.prom` format
  require a matching update to the homelab's ADR-021. Keep observability monitoring-agnostic —
  standard Prometheus textfile to a user-configured path, no assumptions about any stack.
- **Versioning (ADR-112):** SemVer; single source of truth is `Cargo.toml`. `/release` bumps
  the version, updates CHANGELOG, and tags (user pushes). Pre-1.0: MINOR for features/breaking
  changes, PATCH for fixes. `schema_version` (output/heartbeat) and `config_version` version
  their data contracts independently of the app.

## BTRFS

All operations require `sudo` (scoped via sudoers). `BtrfsOps` wraps: create read-only
snapshot, send|receive (optional parent), delete subvolume, check existence, read free
bytes, sync. The send|receive pipeline captures both sides' stderr, checks both exit codes,
and cleans up partial snapshots on failure. Paths pass as `&Path` to `Command::arg()`, never
stringified — prevents shell injection and preserves non-UTF-8 paths. API patterns:
`docs/00-foundation/source-documentation/btrfs-reference.md`.

## Build & Run

```bash
cargo build [--release]                      # build
cargo test [-- --ignored]                    # unit / integration tests
cargo clippy --all-targets -- -D warnings    # lint (covers test code)
cargo check --all-targets                    # fast type-check after mass edits
cargo run -- plan                            # preview backup plan
cargo run -- backup --dry-run                # dry-run a backup
cargo run -- status                          # current promise states
cargo run -- get FILE --at DATE              # restore a file from a snapshot
cargo run -- migrate [--dry-run]             # migrate config to the latest schema (v2)
```

Other subcommands: `history`, `verify`, `init`, `calibrate`, `sentinel`, `drives`,
`doctor`, `emergency`, `retention-preview`, `events`, `completions`.

## Configuration

- Config: `~/.config/urd/urd.toml` (override: `--config`)
- State DB: `~/.local/share/urd/urd.db`
- Heartbeat: `~/.local/share/urd/heartbeat.json`
- Dependency API references (rusqlite, toml, nix, colored, Rust 2024 edition, BTRFS):
  `docs/00-foundation/source-documentation/` — consult for `state.rs` / `config.rs` /
  `lock.rs` work; not needed for domain-level work.

## ADRs

The `docs/00-foundation/decisions/` directory is the index — filenames carry the number and
title. ADRs are immutable; they evolve by amendment or supersession. The architectural
invariants above each cite their ADR.

**ADR gating criteria.** A decision earns ADR status only when all three hold: (1) it is
hard to reverse, (2) the rationale would surprise a reader without context, (3) it is the
result of a real trade-off among considered alternatives. If it fails any of the three, it
belongs in CLAUDE.md, a design doc, or a journal — not the ADR series.

## Development Workflow

Three tiers by scope. Use the lightest tier that fits — when in doubt, tier up. The review
and stress-test phases (`/grill-me`, `arch-adversary`) consistently surface valuable
discoveries; skipping them should be the exception.

- **Patch** (bug fixes, small changes, <3 files):
  `systematic-debugging → build → /check → /commit-push-pr → /session-close`
- **Standard** (medium features, clear scope, no new modules):
  `/design → /grill-me → /prepare → arch-adversary → /post-review → build → /tidy → /check → /commit-push-pr → /session-close`
- **Full** (new modules, architectural changes, ADR gates):
  `/brainstorm → /design → /grill-me → [/sequence] → /prepare → arch-adversary → /post-review → build → /tidy → /check → /commit-push-pr → /session-close`

Available skills and their descriptions are injected into each session — invoke `/<name>`
to use one. For a multi-UPI arc, run an arc-level `/grill-me` before per-UPI design so
format, naming, and cross-UPI sequencing are pinned once.

## Project State

- `docs/96-project-supervisor/status.md` — current state and what to build next.
- `docs/96-project-supervisor/roadmap.md` — strategy, sequencing, horizon.
- `docs/96-project-supervisor/registry.md` — UPI lookup (work items → artifacts).
- **Do NOT run `cargo fmt` repo-wide** — HEAD was formatted with an older rustfmt; the
  current version mass-reformats many files. Hand-match the surrounding style instead.
