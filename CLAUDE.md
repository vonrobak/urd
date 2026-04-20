# CLAUDE.md

## Vision

**Urd** (Old Norse: Urðr) — a BTRFS Time Machine for Linux, written in Rust.

Urd preserves filesystem history silently and faithfully. When invoked, the encounter
should be pleasant and clear. When Urd demands attention, the user should be glad it did.

**Design north star:** Every feature must pass three tests: (1) does it make the user's
data safer? (2) does it reduce the attention the user needs to spend on backups? (3) does
Urd do no harm to the host she protects? A backup tool that causes storage pressure, I/O
contention, or other burden on the system it's supposed to protect has failed both its
job and its promise. When Urd and the host are in conflict, the host wins. If a feature
adds complexity the user must manage, it needs a very strong justification. See ADR-113.

**Two modes of existence:**
- **The invisible worker.** Runs autonomously via systemd timer (nightly at ~04:00) and
  Sentinel daemon (sub-hourly monitoring, drive detection, backup overdue alerts).
  Silence means data is safe.
- **The invoked norn.** `urd status`, `urd get`, `urd restore` — the user is consulting Urd.
  Speaks with authority and clarity. Surfaces problems only when they matter.

**The mythic voice.** Urd's presentation layer carries the character of the norn — evocative
and grounding. Not cosplay, but a consistent tone. The voice belongs entirely in the
presentation layer (`voice.rs`), never in config or data structures. Technical details
remain precise; the framing is mythic.

**Protection promises.** Urd thinks in promises, not operations. The user declares what
matters; Urd derives the operations. Promise states (PROTECTED / AT RISK / UNPROTECTED)
are the universal language. Current taxonomy (guarded/protected/resilient) is provisional
and needs rework — see ADR-110 maturity model.

## Orient Yourself

Read `docs/96-project-supervisor/status.md` first — short current-state document (~50 lines).
Follow links to `roadmap.md` for priorities and feature tracking.
See `docs/contributing-internal.md` for documentation structure and conventions.

## Architecture

### Core Flow

```
config  -->  plan (pure function)  -->  execute (I/O)
                                           |
                                      btrfs (sudo)
```

All backup logic flows through: config -> plan -> execute. No exceptions.

### Module Responsibilities

| Module | Does | Does NOT |
|--------|------|----------|
| `config.rs` | Parse TOML, validate, expand paths, resolve subvolumes | Touch filesystem beyond path checks |
| `types.rs` | Domain types, parsing, Display, `derive_policy()` | Contain business logic |
| `plan.rs` | Decide what operations to run (pure function) | Execute anything or call btrfs |
| `executor.rs` | Execute planned operations, error isolation | Decide what to do (planner's job) |
| `btrfs.rs` | Wrap `sudo btrfs` subprocess calls via `BtrfsOps` trait | Know about retention, plans, config |
| `retention.rs` | Compute which snapshots to keep/delete (pure) | Delete anything (returns lists) |
| `awareness.rs` | Compute promise states per subvolume (pure) | Perform I/O |
| `chain.rs` | Track incremental chain parents (pin files) | Send snapshots |
| `state.rs` | Record history in SQLite | Influence backup decisions |
| `preflight.rs` | Validate config achievability (pure) | Block backups (advisory only) |
| `heartbeat.rs` | Write JSON health signal after each run | Block backups on failure |
| `metrics.rs` | Write Prometheus `.prom` files | Read metrics |
| `notify.rs` | Compute and dispatch notifications | Decide promise states (uses awareness) |
| `drives.rs` | Detect mounted drives, UUID fingerprinting, check space | Mount/unmount drives |
| `output.rs` | Define structured output types | Render text (voice.rs does that) |
| `voice.rs` | Render structured output as text (mythic voice) | Perform I/O or compute state |
| `lock.rs` | Shared advisory lock with metadata (PID, trigger source) | Decide whether to proceed (caller's job) |
| `sentinel.rs` | Pure state machine for Sentinel daemon (events, actions, circuit breaker) | Perform I/O (sentinel_runner.rs does that) |
| `error.rs` | Error types, `translate_btrfs_error()` for actionable messages | Recovery logic |
| `commands/` | CLI subcommand handlers (wire pure modules to I/O) | Core logic (delegate to above) |

### Architectural Invariants

These rules are load-bearing. Violating them causes architectural damage that compounds.
Each references an ADR in `docs/00-foundation/decisions/` with full rationale.

1. **The planner never modifies anything.** Pure function: config + state in, plan out. (ADR-100)
2. **All btrfs calls go through `BtrfsOps`.** No other module spawns btrfs subprocesses. (ADR-101)
3. **Filesystem is truth, SQLite is history.** Pin files and snapshot dirs are authoritative. SQLite failures never prevent backups. (ADR-102)
4. **Individual subvolume failures never abort the run.** The executor isolates errors per subvolume. (ADR-100)
5. **Retention never deletes pinned snapshots.** Three independent layers: unsent protection, planner exclusion, executor re-check. (ADR-106)
6. **Backups fail open; deletions fail closed.** Proceed on missing data, never delete what can't be confirmed safe. (ADR-107)
7. **Core logic modules are pure functions.** Planner, awareness, retention, voice — inputs in, outputs out, no I/O. (ADR-108)
8. **Validate structure at load time; isolate failures at runtime.** Structural config errors refuse to start. Runtime conditions (unmounted drive, full filesystem) skip per-unit and report. (ADR-109, ADR-111)
9. **Backward compatibility contracts are sacred.** Snapshot names, pin files, Prometheus metrics — on-disk data format changes require an ADR with migration plan. Config schema changes use `urd migrate`. (ADR-105, ADR-111)
10. **Named protection levels are opaque or they don't exist.** No per-field overrides on named levels. Custom is first-class. Named levels must earn opaque status through operational track record. (ADR-110, ADR-111)

### Config System (ADR-111)

Dual-parser architecture supporting **legacy** and **v1** schemas. `Config::load()` pre-parses
`config_version` from `[general]`, then dispatches to `parse_legacy()` (absent) or `parse_v1()`
(`config_version = 1`). Both produce the same internal `Config` struct — v1 synthesizes
`LocalSnapshotsConfig` and `DefaultsConfig` so downstream code is schema-agnostic.

- **Legacy:** `[defaults]`, `[local_snapshots]`, `protection_level`, `short_name` required.
- **v1:** Self-describing `[[subvolumes]]` with inline `snapshot_root`/`min_free_bytes`,
  `protection` field (renamed), `short_name` optional (defaults to `name`), no `[defaults]`
  or `[local_snapshots]`. Named levels are opaque — no operational overrides.
- **`urd migrate`:** Transforms legacy → v1. Reads raw TOML, builds v1 as string output.
  Saves backup to `{path}.legacy`. Dispatched as Strategy A (before config load).
- **Example configs:** `config/urd.toml.example` (legacy), `config/urd.toml.v1.example` (v1).

### Error Handling

- `thiserror` for types in `error.rs`; `anyhow` in `main.rs` / CLI layer
- Individual subvolume failures must NOT abort the entire backup run
- Failed sends must clean up partial snapshots at the destination
- SQLite failures must NOT prevent backups (log warning, continue)
- `translate_btrfs_error()` converts btrfs stderr into actionable `BtrfsErrorDetail`

### UX Principles

- **Invisible worker, invoked norn.** Autonomous operation is silent; invoked interaction
  is rich and guided. Failures are always impossible to miss.
- **Answer "is my data safe?"** Every surface should answer this in promise states and
  plain language — not subvolume IDs.
- **Guide through affordances, not error messages.** Lead users toward correct choices.
  Fewer errors, not better errors.
- **Precision in config, voice in presentation.** Config layer is mechanical and explicit.
  Mythic voice belongs entirely in `voice.rs` and notifications.
- **The Sentinel is the integration layer.** Event-driven state machine that reacts to
  drive events, updates promise states, and drives notifications. Deployed as a systemd
  user service.

## Coding Conventions

- Standard Rust: `snake_case` functions, `CamelCase` types
- `cargo clippy -- -D warnings` (all warnings are errors)
- `rustfmt` before committing
- Strong types over primitives: `SnapshotName` not `String`, `Tier` not `u8`
- `#[must_use]` on functions whose return values matter
- Derive `Debug` on all types; `Clone`, `PartialEq`, `Eq` where sensible
- No `unsafe` — no need for it in this project
- No `unwrap()` / `expect()` in library code — only in tests and `main.rs`
- Fallback values must be *safe*, not just *convenient*. `unwrap_or(0)` is wrong when 0 is in-range but semantically meaningless (e.g., bytes transferred, age in days). Use `Option` to represent absence.
- Daemon code (sentinel): lifecycle events use `warn!()` to be visible at default log levels
- Doc filenames: lowercase kebab-case (exceptions: CLAUDE.md, README.md, CONTRIBUTING.md)

## Testing

- Unit tests: `#[cfg(test)] mod tests` in same file. Run: `cargo test`
- Integration tests: `tests/integration/`, `#[ignore]` by default. Run: `cargo test -- --ignored`
- Use `MockBtrfs` and `MockFileSystemState` for anything that would call btrfs or read filesystem
- Code using path-constructing functions (`external_snapshot_dir()`, `local_snapshot_dir()`) should also have `tempfile::TempDir` tests — mocks are blind to filesystem preconditions like missing parent directories
- Test retention logic exhaustively — it protects against data loss
- When building features, use vertical slicing: write one test, implement to pass, repeat. Never write all tests first then all implementation.
- 521+ tests, all passing, clippy clean

## Backward Compatibility (ADR-105)

These **on-disk data formats** are load-bearing — existing snapshots, monitoring, and pin
files depend on them. Config schema has separate versioning (ADR-111).

1. **Snapshot names:** Legacy `YYYYMMDD-{short_name}` (read-only, parsed as midnight) and
   current `YYYYMMDD-HHMM-{short_name}` (all new snapshots). Ordering by datetime, not string.
2. **Snapshot dirs:** `{snapshot_root}/{name}/` — `name` is the directory, `short_name` is
   in the snapshot name. Both are on-disk contracts.
3. **Pin files:** `.last-external-parent-{DRIVE_LABEL}` in local snapshot dir
4. **Prometheus metrics:** exact names, labels, and value semantics must be preserved

**Downstream consumer.** A homelab monitoring stack consumes Urd's metrics and heartbeat.
Changes to the external interface (metric names/labels, heartbeat schema, systemd unit
names, `.prom` file format) require a corresponding update to the homelab's ADR-021 at
`~/containers/docs/00-foundation/decisions/2026-03-28-ADR-021-urd-backup-tool.md`.

**Public release boundary.** Urd targets general-purpose use on any Linux system with BTRFS.
Metrics, notifications, and observability features must remain monitoring-agnostic. Urd writes
standard Prometheus textfile exposition format to a user-configured path — it must not assume
any specific monitoring stack, alert system, or notification service. Keep homelab-specific
concerns (specific dashboards, webhooks, alert rules) out of Urd's code and defaults.

## Versioning (ADR-112)

Standard SemVer (`MAJOR.MINOR.PATCH`). Single source of truth: `Cargo.toml` version field.

- **Pre-1.0:** MINOR for features/breaking changes, PATCH for fixes
- **Post-1.0:** MAJOR for breaking changes (CLI, config, on-disk contracts), MINOR for
  features, PATCH for fixes
- **CHANGELOG.md:** Keep a Changelog format. `/commit-push-pr` adds entries to `[Unreleased]`
  for feat/fix/refactor commits. `/release` moves them to a dated version section.
- **Git tags:** Annotated tags (`v0.3.0`) on release commits. Dates go in tag annotations.
- **Release workflow:** `/release patch|minor|major` — bumps Cargo.toml, updates changelog,
  runs quality gate, commits, and tags. User pushes manually.
- **Data format versions are independent.** `schema_version` in heartbeat/output and
  `config_version` (ADR-111) version their data contracts, not the application.

## BTRFS Commands

All operations require `sudo` (scoped via sudoers). The `BtrfsOps` trait wraps:

```
snapshot -r, send [-p parent], receive, subvolume delete, subvolume show, filesystem show
```

The send|receive pipeline captures stderr from both sides, checks both exit codes, and
cleans up partial snapshots on failure. Paths passed as `&Path` to `Command::arg()`, never
stringified — prevents shell injection and preserves non-UTF-8 paths.

## Dependency Reference

Docs in `docs/00-foundation/source-documentation/` cover current API patterns and migration
notes for rusqlite 0.39, toml 1.x, nix 0.31, colored 3.x, Rust 2024 edition, and BTRFS.
Consult when working on code that directly uses these APIs (especially `state.rs`, `config.rs`,
`lock.rs`). Not needed for domain-level work (retention, awareness, voice, planning).

## Build & Run

```bash
cargo build                          # Debug
cargo build --release                # Release
cargo test                           # Unit tests (931+ tests)
cargo test -- --ignored              # Integration tests (needs drives)
cargo clippy -- -D warnings          # Lint (all warnings are errors)
cargo run -- plan                    # Preview backup plan
cargo run -- backup --dry-run        # Dry-run
cargo run -- status                  # Current promise states
cargo run -- get FILE --at DATE      # Restore file from snapshot
cargo run -- migrate --dry-run       # Preview legacy → v1 migration
cargo run -- migrate                 # Migrate config to v1 schema
```

## Configuration

- Config: `~/.config/urd/urd.toml` (override: `--config`)
- State DB: `~/.local/share/urd/urd.db`
- Heartbeat: `~/.local/share/urd/heartbeat.json`
- Example (legacy): `config/urd.toml.example`
- Example (v1): `config/urd.toml.v1.example`

## ADR Index

| ADR | Title | Scope |
|-----|-------|-------|
| 100 | Planner/executor separation | Core architecture |
| 101 | BtrfsOps trait | Btrfs abstraction |
| 102 | Filesystem truth, SQLite history | State management |
| 103 | Interval-based scheduling | Snapshot/send timing |
| 104 | Graduated retention | Snapshot lifecycle |
| 105 | Backward compatibility contracts | On-disk data formats |
| 106 | Defense-in-depth data integrity | Pin protection layers |
| 107 | Fail-open backups, fail-closed deletions | Error philosophy |
| 108 | Pure-function module pattern | Module design |
| 109 | Config-boundary validation | Security/correctness |
| 110 | Protection promises | Promise semantics, maturity model |
| 111 | Config system architecture | Config structure, versioning (target, not yet implemented) |
| 112 | SemVer and release workflow | Versioning, CHANGELOG, git tags, /release skill |
| 113 | The Do-No-Harm invariant | Layered, probabilistic defense against Urd-induced host burden |

## Development Workflow

Three tiers based on scope. Use the lightest tier that fits — but when in doubt, tier up.
The review and stress-test phases (`/grill-me`, `arch-adversary`) consistently surface
valuable discoveries. Skipping them should be the exception, not the default.

### Patch — bug fixes, small changes, <3 files

```
systematic-debugging → build → /check → /commit-push-pr → /session-close
```

### Standard — medium features, clear scope, no new modules

```
/design → /grill-me → /prepare → arch-adversary → /post-review → build → /simplify → /check → /commit-push-pr → /session-close
```

### Full — new modules, architectural changes, ADR gates

```
/brainstorm → /design → /grill-me → [/sequence] → /prepare → arch-adversary → /post-review → build → /simplify → /check → /commit-push-pr → /session-close
```

### Tool reference

| Tool | Phase | What it does |
|------|-------|--------------|
| `/brainstorm` | Ideation | Divergent thinking, no scoring. Output: `docs/95-ideas/` |
| `/design` | Design | Module decomposition, UPI assignment, ADR gate identification |
| `/grill-me` | Stress-test | Socratic interview, resolve decisions, update design doc |
| `/sequence` | Sequencing | (Optional) Order reviewed designs by dependencies when multiple are queued |
| `/prepare` | Planning | Read design + codebase, produce implementation plan in `docs/97-plans/`. No code. |
| `arch-adversary` | Plan review | Severity-ranked findings on the implementation plan |
| `/post-review` | Plan revision | Revise the plan to address adversary findings |
| `build` | Implementation | Execute the reviewed plan |
| `/simplify` | Post-build | Simplification pass: abstractions, types, control flow |
| `test-team` | Testing | Risk-proportional coverage analysis and gap identification |
| `systematic-debugging` | Diagnosis | Four-phase root cause investigation (any tier, especially patch) |
| `/check` | Quality gate | `cargo clippy` + `cargo test` + `cargo build --release` |
| `/commit-push-pr` | Integration | PII scan, CHANGELOG, branch, commit, PR |
| `/journal` | Any time | Focused journal entry about a specific topic or lesson learned |
| `/session-close` | Session close | Comprehensive journal + status.md + registry.md updates (always last) |
| `/release` | Release | SemVer bump, CHANGELOG, tag (user pushes manually) |

## Project State

See `docs/96-project-supervisor/status.md` for current state and what to build next.
See `docs/96-project-supervisor/roadmap.md` for strategy, sequencing, and horizon.
See `docs/96-project-supervisor/registry.md` for UPI lookup (work items → artifacts).
See `docs/contributing-internal.md` for documentation structure, conventions, and privacy rules.
