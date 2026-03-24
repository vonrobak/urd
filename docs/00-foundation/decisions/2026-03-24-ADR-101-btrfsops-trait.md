# ADR-101: BtrfsOps Trait as Sole Btrfs Interface

> **TL;DR:** All btrfs subprocess calls go through a single trait (`BtrfsOps`) in a single
> module (`btrfs.rs`). `RealBtrfs` shells out to `sudo btrfs`; `MockBtrfs` records calls
> for testing. No other module spawns btrfs processes. This constrains the sudo surface
> area to one auditable location and makes the entire backup pipeline testable without
> root privileges.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** None (founding decision)

## Context

Urd runs `sudo btrfs` commands that can create, send, receive, and delete subvolumes. A
bug in path construction or argument handling could delete the wrong data. The bash script
scattered btrfs calls across multiple functions, making it difficult to audit the full
sudo surface area.

Additionally, testing backup logic requires either real btrfs operations (slow, requires
root, risks real data) or a way to mock them. The trait approach solves both problems.

## Decision

**`btrfs.rs` defines the `BtrfsOps` trait:**

```rust
pub trait BtrfsOps {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<()>;
    fn send_receive(&self, snapshot: &Path, parent: Option<&Path>,
                    dest: &Path) -> Result<SendStats>;
    fn delete_subvolume(&self, path: &Path) -> Result<()>;
    fn subvolume_show(&self, path: &Path) -> Result<SubvolumeInfo>;
}
```

**`RealBtrfs`** implements the trait by spawning `sudo btrfs` via `std::process::Command`.
The send/receive pipeline spawns two processes piped together (not `sh -c "... | ..."`),
captures stderr from both sides in background threads, checks both exit codes, and cleans
up partial snapshots on failure.

**`MockBtrfs`** records all calls for test assertions. It enables the executor's 12+ unit
tests to run without root, without btrfs, and without a real filesystem.

**No other module spawns btrfs subprocesses.** The executor, planner, commands, and all
other modules interact with btrfs exclusively through this trait.

## Consequences

### Positive

- The sudo surface area is one file (`btrfs.rs`), auditable in a single read
- Two-process pipeline avoids shell injection risks from `sh -c` approach
- Both stderr streams are captured, enabling precise error diagnostics
- The entire backup pipeline (216 tests) runs without root privileges
- Path arguments are passed as `&Path` to `Command::arg()`, never stringified — this
  preserves non-UTF-8 paths and prevents injection

### Negative

- Two-process pipeline is more code than a shell pipe (thread for stderr draining, two
  exit code checks, partial cleanup logic)
- `MockBtrfs` cannot test filesystem preconditions (e.g., directory must exist for
  `btrfs receive`) — tests requiring real filesystem interaction use `tempfile::TempDir`

### Constraints

- Adding new btrfs operations requires extending the `BtrfsOps` trait, both implementations,
  and updating `MockBtrfs` assertions
- The progress counter (`AtomicU64` for bytes transferred) stays outside the trait — it is
  an implementation detail of `RealBtrfs`, not part of the interface
- `subvolume_show` is used for crash recovery (checking if a partial snapshot exists at the
  destination); it does not bypass the planner — the executor uses it for pre-execution
  validation only

## Related

- ADR-100: Planner/executor separation (the trait's primary consumer)
- [Phase 2 journal](../../98-journals/2026-03-22-urd-phase02.md) — send/receive pipeline design
- [Phase 3 adversary review](../../99-reports/2026-03-22-phase3-adversary-review.md) —
  removed `to_string_lossy()` from `RealBtrfs`
