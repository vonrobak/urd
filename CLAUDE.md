# CLAUDE.md

## Project

**Urd** (Old Norse: Urðr) — a BTRFS Time Machine for Linux, written in Rust.

Urd automates BTRFS snapshot creation, incremental send/receive to external drives, and graduated retention. It replaces a 1710-line bash script with a type-safe, testable, distributable tool.

The name comes from the Norse norn who tends the Well of Urðr and knows all that has passed — fitting for a system that preserves filesystem history.

## Architecture

For current project state, read `docs/96-project-supervisor/status.md` first. The original
roadmap and architectural vision is at `docs/96-project-supervisor/roadmap.md`.

### Core Design Principle: Planner/Executor Separation

The planner (`plan.rs`) is a **pure function** that produces a `BackupPlan` — a list of `PlannedOperation` variants. It reads config and filesystem state, but never modifies anything. The executor (`executor.rs`) takes a plan and executes it.

This separation is the most important architectural property of Urd. Do not bypass it. All backup logic flows through: config -> plan -> execute.

### BtrfsOps Trait

`btrfs.rs` defines a `BtrfsOps` trait. `RealBtrfs` calls `sudo /usr/sbin/btrfs` via `std::process::Command`. `MockBtrfs` records calls for testing. This is the **only module that shells out to btrfs**. Every other module works through the trait.

### Module Responsibilities

| Module | Does | Does NOT |
|--------|------|----------|
| `config.rs` | Parse TOML, validate, expand paths | Touch filesystem beyond checking paths exist |
| `types.rs` | Define domain types, parsing, Display | Contain business logic |
| `plan.rs` | Decide what operations to run | Execute operations, call btrfs |
| `executor.rs` | Execute planned operations, handle errors | Decide what to do (that's the planner's job) |
| `btrfs.rs` | Wrap btrfs subprocess calls | Know about retention, plans, config |
| `retention.rs` | Compute which snapshots to keep/delete | Delete anything (returns lists) |
| `chain.rs` | Track incremental chain parents (pin files) | Send snapshots |
| `state.rs` | Record history in SQLite | Influence backup decisions |
| `metrics.rs` | Write Prometheus .prom files | Read metrics |
| `drives.rs` | Detect mounted drives, check space | Mount/unmount drives |

### Error Handling

- Use `thiserror` for error type definitions in `error.rs`
- Use `anyhow` in `main.rs` and CLI layer for error context
- Individual subvolume failures must NOT abort the entire backup run
- Failed btrfs sends must clean up partial snapshots at the destination
- SQLite failures must NOT prevent backups from running (log warning, continue)

## Coding Conventions

### Rust Style

- Follow standard Rust conventions: `snake_case` for functions/variables, `CamelCase` for types
- Use `clippy` — all warnings are errors: `cargo clippy -- -D warnings`
- Format with `rustfmt` before committing
- Documentation filenames are lowercase kebab-case (exceptions: CLAUDE.md, README.md, CONTRIBUTING.md)
- Prefer strong types over primitives: `SnapshotName` not `String`, `Tier` not `u8`
- Use `#[must_use]` on functions that return values that should not be ignored
- Derive `Debug` on all types. Derive `Clone`, `PartialEq`, `Eq` where it makes sense
- No `unsafe` code — there is no need for it in this project
- No `unwrap()` or `expect()` in library code — only in tests and `main.rs` where panicking is acceptable

### Testing

- Unit tests live in the same file as the code (`#[cfg(test)] mod tests`)
- Integration tests that need real btrfs/drives go in `tests/integration/` and are `#[ignore]` by default
- Run unit tests: `cargo test`
- Run integration tests: `cargo test -- --ignored` (requires 2TB-backup drive mounted)
- Use `MockBtrfs` for unit testing anything that would call btrfs
- Test retention logic exhaustively — it protects against data loss

### Backward Compatibility

These formats MUST be preserved exactly (the existing bash backup system and monitoring depend on them):

1. **Snapshot naming:**
   - **Legacy (read-only):** `YYYYMMDD-<short_name>` (e.g., `20260322-opptak`) — parsed as midnight. Existing snapshots in this format are recognized and handled correctly by `SnapshotName::parse()`.
   - **Current (write):** `YYYYMMDD-HHMM-<short_name>` (e.g., `20260322-1430-opptak`) — all new snapshots use this format to support sub-daily snapshot intervals (e.g., 15-minute intervals on htpc-home).
   - **Coexistence:** Both formats may exist in the same snapshot directory. Retention, send, and display logic handle both transparently. Ordering is by datetime, not string comparison.
   - **Phase 3 note:** During parallel running, the bash script may not recognize HHMM-format names. This is acceptable — the bash script only manages snapshots it created (matched by its own naming convention). Urd manages all snapshots in the directory regardless of format.
2. **Snapshot directories:** `<snapshot_root>/<subvolume_name>/` (e.g., `.snapshots/subvol3-opptak/`)
3. **Pin files:** `.last-external-parent-<DRIVE_LABEL>` in the subvolume's local snapshot directory. May contain either legacy or current snapshot name format.
4. **Prometheus metrics:** exact metric names, labels, and value semantics (see `docs/PLAN.md` for full list)

### BTRFS Commands

All btrfs operations require `sudo`. The sudoers file scopes allowed commands to specific paths. Commands used:

```bash
sudo btrfs subvolume snapshot -r <source> <dest>     # Create read-only snapshot
sudo btrfs send [-p <parent>] <snapshot>              # Send (incremental with -p)
sudo btrfs receive <dest_dir>                         # Receive snapshot stream
sudo btrfs subvolume delete <path>                    # Delete snapshot
sudo btrfs subvolume show <path>                      # Metadata (read-only)
sudo btrfs filesystem show <path>                     # Filesystem info (read-only)
```

The send|receive pipeline must capture stderr from both sides and check both exit codes. On failure, clean up any partial snapshot at the destination.

## Build & Run

```bash
cargo build                          # Debug build
cargo build --release                # Release build
cargo test                           # Unit tests
cargo test -- --ignored              # Integration tests (needs 2TB-backup drive)
cargo clippy -- -D warnings          # Lint
cargo run -- plan                    # Run: show backup plan
cargo run -- backup --dry-run        # Run: dry-run backup
cargo run -- status                  # Run: show system status
```

## Project Structure

```
src/
  main.rs            # CLI entry, clap dispatch
  cli.rs             # Clap definitions
  config.rs          # TOML config
  types.rs           # Domain types
  plan.rs            # Backup planner (pure logic)
  executor.rs        # Plan executor
  btrfs.rs           # BtrfsOps trait + RealBtrfs + MockBtrfs
  retention.rs       # Graduated + count-based retention
  chain.rs           # Incremental chain / pin files
  state.rs           # SQLite state DB
  metrics.rs         # Prometheus writer
  drives.rs          # Drive detection + space
  error.rs           # Error types
  commands/          # CLI subcommand implementations
config/
  urd.toml.example   # Reference config
docs/                # See CONTRIBUTING.md for full documentation structure
  96-project-supervisor/
    status.md        # Start here: current state, links to details
    roadmap.md       # Original project roadmap (founding artifact)
systemd/             # Service/timer units (Phase 3+)
udev/                # udev rules (Phase 5)
tests/
  integration/       # Tests requiring real drives
```

## Configuration

Config file: `~/.config/urd/urd.toml` (override with `--config`)
State database: `~/.local/share/urd/urd.db`
Example config: `config/urd.toml.example`

## Current Phase

Check `docs/96-project-supervisor/status.md` for current project state and what to build next.
See `CONTRIBUTING.md` for documentation standards and organization.
