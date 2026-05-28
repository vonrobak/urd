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

For controlled vocabulary (promise states, voice labels, protection levels, retention
tiers, identifier conventions) see `docs/00-foundation/glossary.md`. For a one-screen
flow diagram of how the modules below connect, see `docs/00-foundation/architecture.md`.

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
| `btrfs.rs` | Wrap `sudo btrfs` subprocess calls via `BtrfsOps` trait. Read-only generation reads go through the `BtrfsRead` supertrait (`BtrfsOps: BtrfsRead`) so pure planners get a non-mutating seam (ADR-100/101) | Know about retention, plans, config |
| `observation.rs` | Read-side query seams (UPI 052): `FilesystemQuery` (filesystem of truth), `HistoryQuery` (SQLite history), and `Observation` — the `{ fs, history, btrfs }` bundle threaded through `plan::plan` and `awareness::assess`. Hosts the `FileSystemState` bridge supertrait | Perform I/O (defines traits only); decide anything |
| `retention.rs` | Compute which snapshots to keep/delete (pure) | Delete anything (returns lists) |
| `awareness.rs` | Pure: observe promise state (PROTECTED / AT RISK / UNPROTECTED) from config + filesystem + history. The "is my data safe right now?" surface | Perform I/O; translate observations into advice (`advice.rs` does that); recommend retention shapes (`recommendation.rs` does that) |
| `advice.rs` | Pure: translate `SubvolAssessment` into actionable advice (issue/command/reason) and structured redundancy advisories. The "what should the user do?" surface — rule-based, the volatile layer where product refinements land | Perform I/O; assess promise state (`awareness.rs` does that) |
| `recommendation.rs` | Pure: head-room-aware retention-shape recommendations and cost projections — the advisory layer that translates drift signals + headroom into a recommended shape with reasons (ADR-115, UPI 041) | Perform I/O; assess promise state (`awareness.rs` does that); mutate config; run in the backup hot path |
| `chain.rs` | Track incremental chain parents (pin files) | Send snapshots |
| `state.rs` | Record history in SQLite — granular SQL wrappers (one method per query). Also hosts `drift_row_to_sample` (row → `drift::DriftSample` conversion) as a small, shared boundary helper. | Influence backup decisions; compose domain-shaped answers (callers compose primitives inline — see `commands/doctor.rs::compute_churn_for` and `commands/backup.rs::build_churn_views`) |
| `preflight.rs` | Validate config achievability (pure) | Block backups (advisory only) |
| `heartbeat.rs` | Write JSON health signal after each run | Block backups on failure |
| `metrics.rs` | Write Prometheus `.prom` files | Read metrics |
| `notify.rs` | Compute and dispatch notifications | Decide promise states (uses awareness) |
| `drift.rs` | Pure: rolling time-windowed churn aggregation from `drift_samples` (UPI 030) | Perform I/O or persist |
| `drives.rs` | Detect mounted drives, UUID fingerprinting, check space | Mount/unmount drives |
| `pools.rs` | Detect BTRFS pools (source + destination), group subvolumes by pool UUID, read sysfs metadata utilization and statvfs free bytes | Know about retention, plans, drive lifecycle, or notification policy |
| `output.rs` | Define structured output types | Render text (voice.rs does that) |
| `voice/` | Render structured output as text (mythic voice). Per-command sub-modules under `voice/` (`backup.rs`, `calibrate.rs`, `chooser.rs`, `doctor.rs`, `drives.rs`, `emergency.rs`, `get.rs`, `history.rs`, `init.rs`, `plan.rs`, `retention.rs`, `sentinel.rs`, `status.rs`, `verify.rs`) — the UPI 050 per-command split is now complete. Cross-renderer helpers (`humanize_duration`, `exposure_label`, `color_*`, `pluralize`, `classify_verify_checks`, `append_suggestion`/`SuggestionContext`, `format_history_table`, `truncate_str`, `skip_tag`, `aggregate_drive_info`, `unmounted_drive_label`, `format_drive_age_label`, `status_severity`) live in `voice/mod.rs` and are `pub(super)` so sub-modules can use them. Public surface preserved via re-exports — callers continue to use `voice::render_*` unchanged | Perform I/O or compute state |
| `voice_events.rs` | Per-variant `EventPayload` renderer (columnar + NDJSON) | Perform I/O or query state |
| `events.rs` | Pure: `Event`, `EventKind`, `EventPayload`, `Severity`, typed payload enums (UPI 036) | Perform I/O |
| `lock.rs` | Shared advisory lock with metadata (PID, trigger source) | Decide whether to proceed (caller's job) |
| `sentinel.rs` | Pure state machine for Sentinel daemon (events, actions, circuit breaker) | Perform I/O (sentinel_runner.rs does that) |
| `storage_critical.rs` | Stub predicate `is_storage_critical(subvolume)` — false in UPI 044; replaced by UPI 031 with its chosen truth source (ADR-115 amendment 2026-05-16) | Decide on critical state (UPI 031's job) |
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

Tri-parser architecture supporting **legacy**, **v1**, and **v2** schemas. `Config::load()`
pre-parses `config_version` from `[general]`, then dispatches to `parse_legacy()` (absent),
`parse_v1()` (`config_version = 1`), or `parse_v2()` (`config_version = 2`). All three
produce the same internal `Config` struct — v1/v2 synthesize `LocalSnapshotsConfig` and
`DefaultsConfig` so downstream code is schema-agnostic.

- **Legacy:** `[defaults]`, `[local_snapshots]`, `protection_level`, `short_name` required.
- **v1:** Self-describing `[[subvolumes]]` with inline `snapshot_root`/`min_free_bytes`,
  `protection` field (renamed), `short_name` optional (defaults to `name`), no `[defaults]`
  or `[local_snapshots]`. Named levels are opaque — no operational overrides.
  `monthly = 0` means "unlimited monthly retention" (v1 contract preserved indefinitely).
- **v2:** As v1, plus explicit `monthly = "unlimited"` (string) for unbounded monthly
  retention; new optional `yearly: u32` retention tier. `monthly = 0` is a parse error
  (v2 closes the v1 footgun at the parse boundary).
- **`urd migrate`:** Auto-targets latest version. Today: legacy → v2 or v1 → v2 (single
  hop). Reads raw TOML, builds v2 as string output. Saves backup to `{path}.legacy` or
  `{path}.v1`. Comments and original formatting are not preserved (`.v1` / `.legacy`
  backup is the verbatim source of truth).
- **Example configs:** `config/urd.toml.example` (legacy), `config/urd.toml.v1.example`
  (v1), `config/urd.toml.v2.example` (v2).

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
- **Capture real CLI output during drive operations.** Snapshot, send, swap, fail — saved
  transcripts of what the user actually sees are the highest-value input to `/steve` and
  to product-level critique generally. Reviewing strings the user encountered beats
  reviewing strings the designer hopes the user will encounter.

## Coding Conventions

- Standard Rust: `snake_case` functions, `CamelCase` types
- `cargo clippy --all-targets -- -D warnings` (all warnings are errors; `--all-targets` includes test code, which bare clippy skips)
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
- **Symmetric fixes need symmetric reviews.** When a bug is rooted in a shared planning or rendering pattern (e.g. "augment local_snaps with planned_snap" in plan.rs), grep for the pattern in adjacent code paths before closing the fix. The May 2 stranded-snapshots incident: commit `0f52555` correctly fixed the transient planner branch in April; the symmetric bug in the non-transient branch went unnoticed for nearly a month. See [2026-05-02-stranded-snapshots-non-transient-planner.md](docs/98-journals/2026-05-02-stranded-snapshots-non-transient-planner.md).
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
cargo clippy --all-targets -- -D warnings   # Lint (covers test code too)
cargo check --all-targets            # Fast type-check after mass edits (covers test code; bare `cargo check` does not)
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
- Example (v2): `config/urd.toml.v2.example`

## ADR Index

| ADR | Title | Scope |
|-----|-------|-------|
| 100 | Planner/executor separation | Core architecture |
| 101 | BtrfsOps trait | Btrfs abstraction |
| 102 | Filesystem truth, SQLite history | State management |
| 103 | Interval-based scheduling | Snapshot/send timing |
| 104 | Graduated retention (amended 2026-05-15) | Snapshot lifecycle |
| 105 | Backward compatibility contracts (amended 2026-05-15) | On-disk data formats |
| 106 | Defense-in-depth data integrity | Pin protection layers |
| 107 | Fail-open backups, fail-closed deletions | Error philosophy |
| 108 | Pure-function module pattern | Module design |
| 109 | Config-boundary validation | Security/correctness |
| 110 | Protection promises (amended 2026-05-15) | Promise semantics, maturity model |
| 111 | Config system architecture (amended 2026-05-15) | Config structure, versioning (target, not yet implemented) |
| 112 | SemVer and release workflow | Versioning, CHANGELOG, git tags, /release skill |
| 113 | The Do-No-Harm invariant | Layered, probabilistic defense against Urd-induced host burden |
| 114 | Structured event log | Typed change-and-decision history; complement to Prometheus gauges and UPI 030 drift_samples |
| 115 | Retention shape symmetry and the recommendation layer | Symmetric data-cost model + advisory recommendation surface; amends ADR-110 |

**ADR gating criteria.** An entry earns ADR status when all three of these hold:
(1) the decision is hard to reverse, (2) the rationale would surprise a reader without
context, (3) it is the result of a real trade-off among considered alternatives. If a
decision fails any of the three, it belongs in CLAUDE.md, the design doc, or a journal —
not in the ADR series. (Adopted from Matt Pocock's `ADR-FORMAT.md`; see
`docs/99-reports/2026-05-16-skill-ecosystem-review.md` §5 and Doc-1.)

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

**Arc-level grilling for multi-UPI work.** For an arc that will be implemented across
several UPIs, run a `/grill-me` session against the arc *before* per-UPI design work
begins. The arc-level grill pins format, naming, and cross-UPI sequencing once;
subsequent per-UPI grills go faster because the cross-cutting decisions are already
resolved. Use letters or descriptive names (Branch A, "drive-detection step") for
to-be-designed pieces — UPI numbers belong to `/design`, not to the grill (lessons §8.6, §8.7).

## Project State

See `docs/96-project-supervisor/status.md` for current state and what to build next.
See `docs/96-project-supervisor/roadmap.md` for strategy, sequencing, and horizon.
See `docs/96-project-supervisor/registry.md` for UPI lookup (work items → artifacts).
See `docs/contributing-internal.md` for documentation structure, conventions, and privacy rules.
