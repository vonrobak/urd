---
type: ADR
title: BtrfsOps Trait as Sole Btrfs Interface
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-04T15:03:12+02:00'
---
# ADR-101: BtrfsOps Trait as Sole Btrfs Interface

> **TL;DR:** All btrfs subprocess calls go through a single trait (`BtrfsOps`) in a single
> module (`btrfs.rs`). `RealBtrfs` shells out to `sudo btrfs`; `MockBtrfs` records calls
> for testing. No other module spawns btrfs processes. This constrains the sudo surface
> area to one auditable location and makes the entire backup pipeline testable without
> root privileges.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-the-trait-pair-and-the-precise-subprocess-boundary))
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
- Phase 2 journal (`docs/98-journals/2026-03-22-urd-phase02.md`) — send/receive pipeline design
- Phase 3 adversary review (`docs/99-reports/2026-03-22-phase3-adversary-review.md`) —
  removed `to_string_lossy()` from `RealBtrfs`

## Amendment 2026-09-04: the trait pair and the precise subprocess boundary

Two corrections. The trait block in the Decision section is a *pair* of traits, not one;
and the sentence "No other module spawns btrfs subprocesses" is simultaneously too weak
(it says nothing about the non-btrfs privileged surface) and, read literally, no longer
exactly true.

### `BtrfsRead` + `BtrfsOps`

`btrfs.rs` defines a read half and a mutating half, the mutating half as a subtrait
(`src/btrfs.rs`):

```rust
pub trait BtrfsRead {
    fn subvolume_generation(&self, path: &Path) -> Result<u64>;
    fn received_uuid(&self, path: &Path) -> Result<Option<String>>;
    fn list_subvolumes(&self, path: &Path) -> Result<Vec<PathBuf>>;
}

pub trait BtrfsOps: BtrfsRead {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<()>;
    fn send_receive(&self, snapshot: &Path, parent: Option<&Path>, dest_dir: &Path)
        -> Result<SendResult>;
    fn delete_subvolume(&self, path: &Path) -> Result<()>;
    fn subvolume_exists(&self, path: &Path) -> bool;
    fn filesystem_free_bytes(&self, path: &Path) -> Result<u64>;
    fn sync_subvolumes(&self, path: &Path) -> Result<()>;
}
```

The split is a type-level guarantee, not a convention: `&dyn BtrfsRead` cannot upcast to
`&dyn BtrfsOps`, so a read-only caller holds a handle with no mutators on it at all.
`RealBtrfs::for_reads` builds that handle, and it is what the planner's `Observation`
carries (ADR-100) — the planner and awareness read generation counters through a seam
that cannot delete a subvolume.

`subvolume_show`, named in the original block, is not a trait method. The two facts
callers want out of `btrfs subvolume show` are exposed as `subvolume_generation` and
`received_uuid` (the completeness proof, ADR-107); existence is `subvolume_exists`.
`SendStats` is `SendResult`.

### The invariant, stated precisely

> **No module other than `btrfs.rs` invokes the btrfs binary to do work.** Privileged
> non-btrfs subprocesses are confined to `commands/seal.rs` (the sudoers earning) and the
> `sudo -n -l` privilege-listing probe.

`grep -rn "Command::new" src/` finds 35 sites. Ten are in `btrfs.rs`: eight
`sudo -n btrfs …` invocations serving the two traits (seven through the configured
`btrfs_path`; the eighth is the shared `subvolume show` reader behind
`subvolume_generation` and `received_uuid`, which spells the binary name literally), one
unprivileged `btrfs send --help` capability probe, and one `sudo -n true` availability
gate inside `#[cfg(test)]`. The other 25 sites (23 outside test modules) spawn other
binaries and perform no backup work:

| Where | Production sites | What |
|---|---|---|
| `commands/seal.rs` | 14 | The sudoers earning: `sudo install` / `cat` / `mv` / `rm` against the staged sudoers file, `visudo -c -f`, `sudo -k`, `sudo install -d` for snapshot roots, plus `findmnt`, `systemctl`, `loginctl`, and the two sudo probes below |
| `notify.rs` | 3 | `notify-send`, `curl` (webhook), and the user's configured notify hook |
| `commands/doctor.rs` | 2 | `sudo -n -l` (privilege listing) and `loginctl show-user` (linger check) |
| `discovery.rs` | 1 | `run_probe`, which shells `lsblk -J` and `findmnt -t btrfs -J` |
| `pools.rs` | 1 | `findmnt` |
| `commands/calibrate.rs` | 1 | `du` |
| `commands/encounter.rs` | 1 | The user's `$VISUAL` / `$EDITOR` on the config file |

**One production site invokes the btrfs binary outside `btrfs.rs`, deliberately.**
`seal.rs::probe_grant` runs `LC_ALL=C sudo -n <btrfs_path> filesystem show /` to classify
whether the sudoers grant actually works, and `seal.rs::effective_coverage` /
`doctor.rs` run `LC_ALL=C sudo -n -l` to diff effective privileges against the expected
grant lines. Neither touches a subvolume, neither reads state Urd acts on, and neither
prompts (`-n`). They belong to the earning and diagnosis flows, not the backup pipeline:
this ADR's boundary is about *who may act on subvolumes*, and there the answer remains
`btrfs.rs` alone.

No script enforces the boundary by grep today; it is upheld by review. Adding one was
considered and declined — a grep gate over `Command::new` would have to encode the
seal-and-probe exemptions, and a lint that must be taught its own exceptions is weaker
evidence than the audit above.
