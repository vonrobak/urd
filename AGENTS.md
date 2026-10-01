# AGENTS.md

Instructions for AI coding agents working in this repository. Human contributors: see
`CONTRIBUTING.md`. Changes to this file steer every agent session that reads it; review
them as code (ADR-118).

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

## Orient yourself

- `docs/00-foundation/architecture.md` — the flow diagram and the authoritative
  module-responsibility table (`Does` / `Does NOT`).
- `docs/00-foundation/glossary.md` — controlled vocabulary.
- `docs/00-foundation/decisions/` — the ADR index; filenames carry number and title.
- `docs/20-reference/` — CLI, metrics and heartbeat-schema contracts.

## Architecture

### Core flow

```
config  -->  plan (pure function)  -->  execute (I/O)  -->  btrfs (sudo)
```

All backup logic flows through config -> plan -> execute. No exceptions.

### Architectural invariants

These rules are load-bearing. Violating them causes architectural damage that compounds.
Each references an ADR in `docs/00-foundation/decisions/`.

1. **The planner never modifies anything.** Pure function: config + state in, plan out. (ADR-100)
2. **All btrfs calls go through `BtrfsOps`.** No other module invokes the btrfs binary to do work; the only other privileged subprocesses are the sudoers earning in `commands/seal.rs` and the `sudo -n -l` probe in `probes.rs`. (ADR-101; the grant itself: ADR-120)
3. **Filesystem is truth, SQLite is history.** Pin files and snapshot dirs are authoritative. SQLite failures never prevent backups. (ADR-102)
4. **Individual subvolume failures never abort the run.** The executor isolates errors per subvolume. (ADR-100)
5. **Retention never deletes pinned snapshots.** Three independent layers: unsent protection, planner exclusion, executor re-check. (ADR-106)
6. **Backups fail open; deletions fail closed.** Proceed on missing data, never delete what can't be confirmed safe. (ADR-107)
7. **Core logic modules are pure functions.** Planner, awareness, retention, voice — inputs in, outputs out, no I/O. (ADR-108)
8. **Validate structure at load time; isolate failures at runtime.** Structural config errors refuse to start; runtime conditions (unmounted drive, full filesystem) skip per-unit and report. (ADR-109, ADR-111)
9. **Backward-compatibility contracts are sacred.** On-disk data format changes require an ADR with a migration plan; config schema changes use `urd migrate`. (ADR-105, ADR-111)
10. **Named protection levels are opaque or they don't exist.** No per-field overrides on named levels; custom is first-class. (ADR-110, ADR-111)
11. **Sends are never time-limited.** Only a genuine storage emergency (the ADR-113 watchdog) may abort a running send — never a wall-clock timeout; systemd units set `TimeoutStartSec=infinity`. (ADR-113; `docs/00-foundation/guides/operating-urd.md`)

### Error handling

- `thiserror` for types in `error.rs`; `anyhow` in `main.rs` / the CLI layer.
- Subvolume failures must not abort the run; failed sends clean up partial snapshots at
  the destination; SQLite failures log a warning and continue.
- `translate_btrfs_error()` converts btrfs stderr into an actionable `BtrfsErrorDetail`.

## Area rules

Rules that bind when you touch a specific part of the code:

- **Deletion paths** (retention, reclaim, emergency, executor deletes): a change here gets
  an adversarial implementation review by a reviewer that did not write it, before it
  lands — even a small patch. Its author is its worst reviewer.
- **`src/voice/`**: user-facing strings are product character. A refactor keeps them
  byte-identical; consolidate only renderers whose output is exactly equal.
- **Config parsing**: the tri-parser (legacy, v1, v2) is intentional and produces one
  internal `Config`. Schema changes go through `urd migrate` (ADR-111); never collapse
  the parsers.
- **Metrics, heartbeat, `.prom` output and systemd unit names**: an external monitoring
  stack consumes them. A change to names, labels, semantics or schema is a contract change
  (ADR-105) and needs a matching update on the consumer side, shipped with the change.

## Coding conventions

- `cargo clippy --all-targets -- -D warnings` (all warnings are errors; `--all-targets` covers test code).
- Do **not** run `cargo fmt` repo-wide: HEAD was formatted with an older rustfmt and the
  current version mass-reformats many files. Hand-match the surrounding style.
- Strong types over primitives: `SnapshotName` not `String`, `Tier` not `u8`.
- `#[must_use]` where return values matter; derive `Debug` everywhere, `Clone`/`PartialEq`/`Eq` where sensible.
- No `unsafe`. No `unwrap()`/`expect()` in library code (tests and `main.rs` only).
- Fallback values must be *safe*, not just *convenient*. `unwrap_or(0)` is wrong when 0 is in-range but semantically meaningless (bytes transferred, age in days) — use `Option` for absence.
- Daemon (sentinel) lifecycle events use `warn!()` to be visible at default log levels.
- Doc filenames: lowercase kebab-case (exceptions: AGENTS.md, CLAUDE.md, README.md, CONTRIBUTING.md).
- The repo is public: no usernames, hostnames or home paths in tracked files
  (`scripts/pre-commit-pii.sh` guards commits).

## Testing

- Unit tests: `#[cfg(test)] mod tests` in-file (`cargo test`). Integration tests in
  `tests/integration/`, `#[ignore]` by default (`cargo test -- --ignored`).
- Use `MockBtrfs` / `MockFileSystemState` for anything that would call btrfs or read the
  filesystem. Path-constructing code should also have `tempfile::TempDir` tests — mocks are
  blind to filesystem preconditions like missing parent directories.
- Test retention logic exhaustively — it protects against data loss.
- Vertical slicing: one test, implement to pass, repeat. Never all tests then all impl.
- A regression test must fail without its fix: revert the fix locally once and watch it fail.
- TTY-gated flows (the Encounter, any `is_terminal()` prompt) take the non-interactive path
  under plain pipes. Drive them through a pty:
  `printf '1\n' | python3 -c "import pty; pty.spawn(['target/debug/urd', ...])"`.
- **Symmetric fixes need symmetric reviews.** When a bug is rooted in a shared planning or
  rendering pattern, grep for the pattern in adjacent code paths before closing the fix.
- `scripts/check.sh` is the full gate (clippy, tests, release build, doc lints).

## Config and backward compatibility

- **Config (ADR-111):** a tri-parser — legacy (no `config_version`), v1, and v2 — all
  producing one internal `Config`, so downstream code is schema-agnostic. `urd migrate`
  auto-targets the latest schema (v2). Examples: `config/urd.toml.{example,v1.example,v2.example}`.
- **On-disk contracts (ADR-105) — changes require an ADR + migration plan:** snapshot names
  (`YYYYMMDD-HHMM-{short_name}`; legacy `YYYYMMDD-{short_name}` parsed as midnight; ordered by
  datetime), snapshot dirs (`{snapshot_root}/{name}/{YYYYMMDD-HHMM-short_name}/`), pin files
  (`.last-external-parent-{DRIVE_LABEL}`), and Prometheus metric names/labels/semantics.
  Field-level detail: `docs/20-reference/` (cli, metrics, heartbeat-schema).
- **A new schema ships with its retirement plan.** When a data format gains a successor,
  the design states the retirement criterion, what clears the legacy artifact, and when the
  code drops legacy handling. Preservation without retirement lets dead history keep acting:
  a legacy pin file once anchored two months of snapshots against retention.
- **Observability stays monitoring-agnostic:** standard Prometheus textfile to a
  user-configured path, no assumptions about any stack.
- **Versioning (ADR-112):** SemVer; single source of truth is `Cargo.toml`. Pre-1.0: MINOR
  for features/breaking changes, PATCH for fixes. `schema_version` (output/heartbeat) and
  `config_version` version their data contracts independently of the app.

## BTRFS

All operations require `sudo` (scoped via sudoers). `BtrfsOps` wraps: create read-only
snapshot, send|receive (optional parent), delete subvolume, check existence, read free
bytes, sync. The send|receive pipeline captures both sides' stderr, checks both exit codes,
and cleans up partial snapshots on failure. Paths pass as `&Path` to `Command::arg()`, never
stringified — prevents shell injection and preserves non-UTF-8 paths. API patterns:
`docs/00-foundation/source-documentation/btrfs-reference.md`; the other dependency
references there (rusqlite, toml, nix, colored, Rust 2024) serve `state.rs`, `config.rs`
and `lock.rs` work.

## Build and run

```bash
cargo build [--release]                      # build
./scripts/check.sh [filter]                  # full quality gate
cargo test [-- --ignored]                    # unit / integration tests
cargo clippy --all-targets -- -D warnings    # lint (covers test code)
cargo check --all-targets                    # fast type-check after mass edits
cargo run -- plan                            # preview backup plan
cargo run -- backup --dry-run                # dry-run a backup
cargo run -- status                          # current promise states
cargo run -- get FILE --at DATE              # restore a file from a snapshot
cargo run -- migrate [--dry-run]             # migrate config to the latest schema (v2)
```

`urd --help` lists the other subcommands.

## ADRs

ADRs are immutable; they evolve by amendment or supersession. A decision earns ADR status
only when all three hold: (1) it is hard to reverse, (2) the rationale would surprise a
reader without context, (3) it is the result of a real trade-off among considered
alternatives. An ADR records a decision, not an inventory of code; if it fails any of the
three, it belongs in a design doc or a code comment.
