# Contributing to Urd

Contributions and conversations welcome. Here's what you need to get started.

## Building

```bash
cargo build --release       # Binary at target/release/urd
cargo install --path .      # Install to ~/.cargo/bin/urd
```

## Testing

```bash
cargo test                  # 930+ unit tests
cargo test -- --ignored     # Integration tests (requires BTRFS drives)
cargo clippy -- -D warnings # Lint (all warnings are errors)
```

All three must pass before submitting a PR.

## Code style

- `rustfmt` before committing
- `cargo clippy -- -D warnings` — warnings are errors
- Strong types over primitives (`SnapshotName` not `String`)
- No `unsafe`, no `unwrap()`/`expect()` in library code
- Unit tests live in `#[cfg(test)] mod tests` in the same file

See [`CLAUDE.md`](CLAUDE.md) for the full coding conventions and architectural invariants.

## Architecture

The core flow is `config -> plan (pure) -> execute (I/O) -> record`. The planner is a
pure function; all btrfs calls go through the `BtrfsOps` trait; individual subvolume
failures never abort a run.

Architectural decisions are documented as ADRs in
[`docs/00-foundation/decisions/`](docs/00-foundation/decisions/). Start with ADR-100
(planner/executor separation) for the foundational pattern.

## Pull requests

- One focused change per PR
- Include tests for new behavior
- Describe the *why*, not just the *what*

## License

By contributing, you agree that your contributions will be licensed under the
[GPL-3.0 License](LICENSE).
