# CLAUDE.md

## Vision

**Urd** (Old Norse: Urðr) — a BTRFS Time Machine for Linux, written in Rust.

Urd is the norn who tends the Well of Urðr and knows all that has passed. She preserves your
filesystem history silently and faithfully. When you invoke her, the encounter should be
pleasant and clear. When she demands your attention, you should be glad she did.

**Design north star:** Every feature must pass two tests: (1) does it make the user's data
safer? (2) does it reduce the attention the user needs to spend on backups? If a feature
adds complexity the user must manage, it needs a very strong justification.

**Two modes of existence:**
- **The invisible worker.** Urd runs autonomously — systemd timer, Sentinel daemon, tray icon.
  Silence is a good sign. The user should trust that if Urd is quiet, their data is safe.
- **The invoked norn.** When the user calls `urd status`, `urd restore`, or any command, they
  are consulting Urd. She speaks with authority and clarity, guiding decisions about their data.
  When Urd surfaces a problem unbidden (notification, broken promise), it's because it matters.

**The mythic voice.** Urd's text-based interactions carry the character of the norn — evocative,
wise, grounding. Not cosplay or gimmick, but a consistent tone that makes the experience of
managing backups feel considered and trustworthy. "Your recordings are woven into the well"
rather than "backup completed: success." Apply this voice to status output, setup conversation,
recovery contracts, and notifications. Technical details remain precise; the framing is mythic.

**Protection promises.** Urd thinks in promises, not operations. The user declares what matters
("protect my home directory," "keep my recordings resilient") and Urd derives the operations.
Anchor promises to the user's actual data: documents, photos, recordings, projects — not
subvolume IDs. Promise states (PROTECTED / AT RISK / UNPROTECTED) are the universal language
of the app.

## Orient Yourself

Read `docs/96-project-supervisor/status.md` first — it has current state, priorities, and
links to everything else. See `CONTRIBUTING.md` for documentation standards.

## Architecture

### Core Invariant: Planner/Executor Separation

The planner (`plan.rs`) is a **pure function**: config + filesystem state in, `BackupPlan` out.
It never modifies anything. The executor (`executor.rs`) takes a plan and runs it.

This is the most important architectural property. Do not bypass it. All backup logic flows
through: config -> plan -> execute.

### BtrfsOps Trait

`btrfs.rs` defines `BtrfsOps`. `RealBtrfs` shells out to `sudo btrfs`; `MockBtrfs` records
calls for testing. This is the **only module that calls btrfs**. Everything else uses the trait.

### Module Responsibilities

| Module | Does | Does NOT |
|--------|------|----------|
| `config.rs` | Parse TOML, validate, expand paths | Touch filesystem beyond path checks |
| `types.rs` | Define domain types, parsing, Display | Contain business logic |
| `plan.rs` | Decide what operations to run | Execute anything or call btrfs |
| `executor.rs` | Execute planned operations | Decide what to do (planner's job) |
| `btrfs.rs` | Wrap btrfs subprocess calls | Know about retention, plans, config |
| `retention.rs` | Compute which snapshots to keep/delete | Delete anything (returns lists) |
| `chain.rs` | Track incremental chain parents (pin files) | Send snapshots |
| `state.rs` | Record history in SQLite | Influence backup decisions |
| `metrics.rs` | Write Prometheus .prom files | Read metrics |
| `drives.rs` | Detect mounted drives, check space | Mount/unmount drives |
| `commands/` | CLI subcommand handlers | Core logic (delegate to above) |

### Error Handling

- `thiserror` for types in `error.rs`; `anyhow` in `main.rs` / CLI layer
- Individual subvolume failures must NOT abort the entire backup run
- Failed sends must clean up partial snapshots at the destination
- SQLite failures must NOT prevent backups (log warning, continue)

### UX Principles

These encode design decisions from the Norman UX analysis and user feedback:

- **Invisible worker, invoked norn.** Two interaction modes (see Vision above). Autonomous
  operation is silent; invoked interaction is rich, guided, and carries the mythic voice.
  Failures and broken promises are always impossible to miss, regardless of mode.
- **Answer "is my data safe?"** Every user-facing surface should answer this in human terms
  — promise states, plain language, data types the user cares about (not subvolume IDs).
- **Guide through affordances, not error messages.** The interface should lead users toward
  correct choices so errors don't happen. Smart defaults, setup guidance, and promise-level
  config are better than post-hoc warnings. The goal is fewer errors, not better errors.
- **Flexibility only earns its keep if it's easy to operate.** A powerful feature behind a
  confusing interface is a feature nobody uses. When in doubt, choose the simpler design.
  Power users interact with config files directly — don't over-build the config UI for them.
- **The Sentinel is the integration layer.** An event-driven state machine that holds the
  awareness model, reacts to events (drive plug, timer, backup result), updates promise
  states, and drives notifications. Other features subscribe to its event stream.

### Architectural Invariants

These rules are load-bearing. Violating them causes architectural damage that compounds.
Each references an ADR in `docs/00-foundation/decisions/` with full rationale.

1. **The planner never modifies anything.** Pure function: config + state in, plan out. (ADR-100)
2. **All btrfs calls go through `BtrfsOps`.** No other module spawns btrfs subprocesses. (ADR-101)
3. **Filesystem is truth, SQLite is history.** Pin files and snapshot dirs are authoritative. SQLite failures never prevent backups. (ADR-102)
4. **Individual subvolume failures never abort the run.** The executor isolates errors per subvolume. (ADR-100)
5. **Retention never deletes pinned snapshots.** Three independent layers: unsent protection, planner exclusion, executor re-check. (ADR-106)
6. **Backups fail open; deletions fail closed.** Missing data means proceed and clean up, never refuse to back up. But never delete a snapshot you can't confirm is safe to remove. (ADR-107)
7. **Core logic modules are pure functions.** Planner, awareness, retention, voice — inputs in, outputs out, no I/O. (ADR-108)
8. **Validate at config boundary, trust afterward.** Paths and names validated once at load; no re-validation in hot paths. (ADR-109)
9. **Backward compatibility contracts are sacred.** Snapshot names, pin files, Prometheus metrics — changes require an ADR with migration plan. (ADR-105)

## Coding Conventions

- Standard Rust: `snake_case` functions, `CamelCase` types
- `cargo clippy -- -D warnings` (all warnings are errors)
- `rustfmt` before committing
- Strong types over primitives: `SnapshotName` not `String`, `Tier` not `u8`
- `#[must_use]` on functions whose return values matter
- Derive `Debug` on all types; `Clone`, `PartialEq`, `Eq` where sensible
- No `unsafe` — no need for it in this project
- No `unwrap()` / `expect()` in library code — only in tests and `main.rs`
- Doc filenames: lowercase kebab-case (exceptions: CLAUDE.md, README.md, CONTRIBUTING.md)

## Testing

- Unit tests: `#[cfg(test)] mod tests` in same file. Run: `cargo test`
- Integration tests: `tests/integration/`, `#[ignore]` by default. Run: `cargo test -- --ignored`
- Use `MockBtrfs` for anything that would call btrfs
- Test retention logic exhaustively — it protects against data loss

## Backward Compatibility

These formats are **load-bearing** — existing snapshots, monitoring, and pin files depend on them:

1. **Snapshot names:** Legacy `YYYYMMDD-<name>` (read-only, parsed as midnight) and current
   `YYYYMMDD-HHMM-<name>` (all new snapshots). Both coexist; ordering by datetime, not string.
2. **Snapshot dirs:** `<snapshot_root>/<subvolume_name>/`
3. **Pin files:** `.last-external-parent-<DRIVE_LABEL>` in local snapshot dir
4. **Prometheus metrics:** exact names, labels, and value semantics must be preserved

## BTRFS Commands

All operations require `sudo` (scoped via sudoers). The `BtrfsOps` trait wraps:

```
snapshot -r, send [-p parent], receive, subvolume delete, subvolume show, filesystem show
```

The send|receive pipeline must capture stderr from both sides, check both exit codes, and
clean up partial snapshots on failure.

## Build & Run

```bash
cargo build                          # Debug
cargo build --release                # Release
cargo test                           # Unit tests
cargo test -- --ignored              # Integration tests (needs drives)
cargo clippy -- -D warnings          # Lint
cargo run -- plan                    # Preview backup plan
cargo run -- backup --dry-run        # Dry-run
cargo run -- status                  # Current state
```

## Configuration

- Config: `~/.config/urd/urd.toml` (override: `--config`)
- State DB: `~/.local/share/urd/urd.db`
- Example: `config/urd.toml.example`

## Project State

See `docs/96-project-supervisor/status.md` for current priorities and what to build next.
See `CONTRIBUTING.md` for documentation structure, conventions, and privacy rules.
