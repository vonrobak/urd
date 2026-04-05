# Source Documentation

Reference documents for Urd's key dependencies and toolchain. Written to help
Claude Code sessions apply conventions and APIs that are more recent than the
model's training data.

**Generated:** 2026-04-05
**Rust toolchain:** 1.94.0, edition 2024
**Context:** Urd v0.11.1

| Document | Covers |
|----------|--------|
| [rust-2024-edition.md](rust-2024-edition.md) | Rust 2024 edition changes, new idioms, lint defaults |
| [colored-3.md](colored-3.md) | colored 2.x -> 3.x migration, current API |
| [rusqlite-0.39.md](rusqlite-0.39.md) | rusqlite 0.32 -> 0.39 migration, breaking changes |
| [toml-1.md](toml-1.md) | toml 0.8 -> 1.x migration, TOML 1.1 support |
| [nix-0.31.md](nix-0.31.md) | nix 0.29 -> 0.31 migration, I/O safety, file locking |
| [btrfs-reference.md](btrfs-reference.md) | BTRFS snapshot, send/receive, space management |

These documents are reference material, not prescriptive. Always verify against
actual crate docs and compiler output when patterns seem uncertain.
