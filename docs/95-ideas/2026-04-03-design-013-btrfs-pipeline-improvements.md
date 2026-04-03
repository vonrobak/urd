---
upi: "013"
status: proposed
date: 2026-04-03
---

# Design: BTRFS Pipeline Improvements (013-a + 013-b)

> **TL;DR:** Two small, targeted btrfs.rs improvements. 013-a adds `--compressed-data`
> to `btrfs send` when the host supports protocol v2 — strictly better, no config knob.
> 013-b calls `btrfs subvolume sync` after retention deletions so that freed extents are
> committed to disk before the space check that gates the next snapshot. Both changes are
> confined to `btrfs.rs` and `executor.rs`, touch no config schema, and respect ADR-101.

## Problem

### 013-a: Compressed sends decompress and recompress extents unnecessarily

`btrfs send` (protocol v1, the default) always decompresses inline-compressed extents
before writing them into the send stream. `btrfs receive` on the destination side then
re-compresses them — or doesn't, depending on mount options. This round-trip wastes CPU
on both ends and, more importantly, converts compressed extents to uncompressed ones when
the destination has different compression settings.

Protocol v2 (btrfs-progs 5.18+, kernel 5.18+) introduces `--compressed-data`, which
passes compressed extents verbatim. This is strictly better when available: less CPU,
identical data layout on the destination, and no risk of silent decompression. It is not
a trade-off — there is no configuration reason to prefer v1 when v2 is available.

Current code (`btrfs.rs:92-127`) builds the send command with only the optional `-p`
parent flag. It has no protocol-version awareness.

### 013-b: Space is not freed before the post-deletion space check

`btrfs delete` marks subvolumes for deletion but does not guarantee that space is
returned to the filesystem immediately. On COW filesystems, extent deallocation is
deferred to a background commit. The executor's current sequence is:

```
delete snapshot → check free bytes → create next snapshot
```

When many snapshots are deleted in quick succession under the emergency space-recovery
path, the space check runs against stale accounting. This is the failure mode that matters
most on NVMe volumes with a 10 GB `min_free_bytes` threshold — deletes succeed, the free
check sees insufficient space, and the new snapshot is either skipped or blocked even
though there is plenty of room. `btrfs subvolume sync <path>` flushes the commit queue
and waits for space to be returned before returning.

Current code (`btrfs.rs:273-306`) calls `delete_subvolume` and returns. There is no
post-delete sync. The space check in `executor.rs:748-758` runs immediately after, against
potentially stale accounting.

## Proposed Design

### 013-a: `--compressed-data` capability probe and flag injection

#### Capability probe

Detect support once at startup via a `--help` probe. This avoids adding a feature flag to
config (no config knob is needed — protocol v2 is strictly better) and avoids a
`supported_features` table that would need maintenance as btrfs evolves.

```rust
// btrfs.rs
pub struct SystemBtrfs {
    pub btrfs_path: String,
    pub supports_compressed_data: bool,
}

impl SystemBtrfs {
    pub fn probe(btrfs_path: &str) -> Self {
        let supports = Command::new("sudo")
            .arg(btrfs_path)
            .arg("send")
            .arg("--help")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
            .map(|o| {
                let combined = [o.stdout, o.stderr].concat();
                String::from_utf8_lossy(&combined).contains("--compressed-data")
            })
            .unwrap_or(false);
        SystemBtrfs {
            btrfs_path: btrfs_path.to_string(),
            supports_compressed_data: supports,
        }
    }
}
```

`--help` exits non-zero on older btrfs-progs, so both stdout and stderr are captured and
combined. `unwrap_or(false)` is correct here: inability to probe means no assumption of
support. Paths are passed as `&str` to `Command::arg()` because `btrfs_path` is already a
validated path string held by the struct.

#### RealBtrfs integration

`RealBtrfs` gains the capability field. `new()` accepts `supports_compressed_data: bool`
so callers (currently `main.rs`) can provide the probed value:

```rust
pub struct RealBtrfs {
    btrfs_path: String,
    bytes_counter: Arc<AtomicU64>,
    supports_compressed_data: bool,
}

impl RealBtrfs {
    pub fn new(btrfs_path: &str, bytes_counter: Arc<AtomicU64>, supports_compressed_data: bool) -> Self { ... }
}
```

In `send_receive`, after `send_cmd.arg("send")`:

```rust
if self.supports_compressed_data {
    send_cmd.arg("--compressed-data");
    log::info!("btrfs send: compressed data pass-through enabled");
}
```

The log line is `info!` level so it is visible in normal operation without being noisy.
It appears once per send, not once at startup — confirming the flag was active for each
individual transfer.

#### Probe call site

`main.rs` (or wherever `RealBtrfs` is constructed) calls `SystemBtrfs::probe()` once and
passes `supports_compressed_data` into `RealBtrfs::new()`. This keeps the probe out of
`BtrfsOps` (the trait is for operations, not capability negotiation) and out of
`RealBtrfs::new()` (constructors don't perform I/O).

#### MockBtrfs

`MockBtrfs` gains `pub supports_compressed_data: bool` (default `false`). Tests that want
to assert on flag injection set it to `true`. The mock does not call any subprocess, so no
probe logic is needed there — the field is set directly.

`MockBtrfsCall::SendReceive` gains a `compressed_data: bool` field so tests can assert
that the flag was (or was not) passed.

### 013-b: `sync_subvolumes` after retention deletions

#### New trait method

```rust
// btrfs.rs — BtrfsOps trait
fn sync_subvolumes(&self, path: &Path) -> crate::error::Result<()>;
```

Implemented on `RealBtrfs`:

```rust
fn sync_subvolumes(&self, path: &Path) -> crate::error::Result<()> {
    log::debug!(
        "Running: sudo {} subvolume sync {}",
        self.btrfs_path,
        path.display()
    );
    let output = Command::new("sudo")
        .env("LC_ALL", "C")
        .arg(&self.btrfs_path)
        .args(["subvolume", "sync"])
        .arg(path)
        .output()
        .map_err(|e| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Sync,
                exit_code: None,
                stderr: format!("failed to spawn btrfs: {e}"),
                bytes_transferred: None,
            },
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Sync,
                exit_code: output.status.code(),
                stderr,
                bytes_transferred: None,
            },
        });
    }
    Ok(())
}
```

`BtrfsOperation::Sync` is a new variant in `error.rs`. Path is passed as `&Path` to
`Command::arg()`, never stringified.

#### Executor call site

`sync_subvolumes` is called once per retention batch, not once per deleted snapshot. The
executor already groups retention deletions by subvolume; the sync follows all deletions
for a given snapshot root:

```
for each snapshot to delete:
    delete_subvolume(path)    ← existing
sync_subvolumes(snapshot_root)  ← new, after the deletion loop
[space check]                   ← existing, now sees accurate accounting
create_readonly_snapshot(...)   ← existing
```

The sync path is the snapshot root directory (the parent of the deleted snapshot dirs),
not the snapshot path itself. `btrfs subvolume sync <dir>` syncs all pending deletions
under that directory. This avoids N sync calls for N deletions.

#### Failure semantics

Sync failure is logged as a warning but does not abort the run. A sync failure means the
space check may be pessimistic (same behavior as today). The ADR-107 principle applies:
fail open. If we cannot confirm space is freed, we proceed with the space check against
potentially stale numbers — same outcome as the current code, not worse.

```rust
if let Err(e) = self.btrfs.sync_subvolumes(&snapshot_root) {
    log::warn!("btrfs subvolume sync failed for {}: {e} — space check may be pessimistic", snapshot_root.display());
}
```

#### MockBtrfs

`MockBtrfsCall::SyncSubvolumes { path: PathBuf }` is added. `MockBtrfs::sync_subvolumes`
records the call and returns `Ok(())` by default. A `fail_syncs: RefCell<HashSet<PathBuf>>`
field allows tests to inject failures.

## Module Map

| Module | Changes | Test strategy |
|--------|---------|---------------|
| `btrfs.rs` | Add `SystemBtrfs` struct with `probe()`. Add `supports_compressed_data: bool` to `RealBtrfs`. Inject `--compressed-data` in `send_receive` when set. Add `BtrfsOps::sync_subvolumes`. Implement on `RealBtrfs`. Add `MockBtrfsCall::SyncSubvolumes`. Add `mock_fail_syncs` field to `MockBtrfs`. | ~4 unit tests: probe parses `--compressed-data` in help output; probe returns false on missing string; `send_receive` injects flag when enabled; `send_receive` omits flag when disabled. |
| `error.rs` | Add `BtrfsOperation::Sync` variant. | Existing error rendering tests cover. |
| `executor.rs` | Call `sync_subvolumes(snapshot_root)` after the retention deletion loop. Treat failure as warn-and-continue (ADR-107). | ~2 unit tests: sync is called after deletions; sync failure does not abort run; space check follows sync in call order. |
| `main.rs` | Call `SystemBtrfs::probe()` once at startup. Pass `supports_compressed_data` to `RealBtrfs::new()`. | Manual smoke test; no unit test (I/O boundary). |

**Modules NOT touched:** `config.rs`, `types.rs`, `plan.rs`, `retention.rs`, `awareness.rs`,
`chain.rs`, `state.rs` — no logic or schema changes. `BtrfsOps` trait changes are additive;
all existing `MockBtrfs` usage remains valid after adding the new method and call variant.

## Effort Estimate

**~0.25 session for both.** Calibration: UUID fingerprinting (UPI 007) was 1 module,
10 tests, 1 session. This is substantially smaller:

- 013-a: ~3 hours. Two struct fields, one conditional `Command::arg`, one probe function,
  ~4 tests. The probe is the only novel pattern; everything else follows existing
  `btrfs.rs` conventions.
- 013-b: ~2 hours. One new trait method, one `Command` block, two executor call sites
  (local and external snapshot roots may differ), ~2 tests. The sync-after-delete ordering
  is the only logic; no new types.

Both fit in a single implementation pass with no design ambiguity outstanding.

## Sequencing

These two sub-items are fully independent and can be built together or separately in either
order. No other in-flight UPIs depend on them.

**Recommended:** Build 013-a and 013-b together in a single session after UPI 010-a ships.
Neither touches config or types; conflicts with any concurrent work are unlikely.

Suggested order within the session: 013-b first (simpler, purely additive to an existing
pattern), then 013-a (the probe adds a mild structural novelty).

## Architectural Gates

| Gate | ADR | Status |
|------|-----|--------|
| All btrfs calls through `BtrfsOps` | ADR-101 | Met: `sync_subvolumes` added to trait; probe is not a btrfs operation and lives in `SystemBtrfs`, not `BtrfsOps`. |
| Pure-function modules unchanged | ADR-108 | Met: no changes to planner, retention, awareness, voice. |
| Individual subvolume failures don't abort run | ADR-100 | Met: sync failure is warn-and-continue. |
| Backups fail open; deletions fail closed | ADR-107 | Met: sync failure leaves behavior unchanged (stale space check), it does not block the backup. |
| No unsafe code | — | Met: standard `Command` API throughout. |
| Paths as `&Path` to `Command::arg()` | ADR-101 | Met: both new commands follow the existing pattern. |

## Rejected Alternatives

### Config flag for `--compressed-data`

`compressed_data = true` in `[general]` or per-subvolume. Rejected: this is not a policy
choice. Protocol v2 is strictly better than v1 when available. A config knob would imply
a trade-off that does not exist. The probe makes the choice automatically based on what
the host supports. Users should not need to know what "protocol v2" means.

### Probe in `RealBtrfs::new()`

Keeps the probe co-located with the struct but makes the constructor perform I/O — a
violation of the project's convention (constructors are cheap; I/O happens at call sites).
Rejected in favor of `SystemBtrfs::probe()` called explicitly at startup.

### Probe in `BtrfsOps` trait

Adding a `fn probe_capabilities() -> BtrfsCapabilities` method to the trait conflates
capability negotiation with operation execution. The trait is for btrfs operations, not
for interrogating the btrfs binary. Rejected in favor of `SystemBtrfs` as a separate,
startup-only struct.

### `btrfs subvolume sync` after every delete (not batched)

Calling sync once per deleted snapshot is correct but wasteful — each sync blocks until
all pending commits flush. One sync after the entire deletion batch is equivalent and
faster. Rejected: N syncs for N deletes when one suffices.

### Sync failure aborts the run

Would make behavior strictly worse than today (today there is no sync and the run
proceeds). Rejected. ADR-107 is unambiguous: backups fail open. A failed sync means the
space check may be pessimistic — the same outcome as the current code.

### Skip sync for external drives

External drives have the same deferred-commit behavior as local BTRFS filesystems. Space
constraint issues on external drives are less common but not impossible. Rejected: apply
sync consistently to all snapshot roots (local and external) where retention deletions
occurred. The cost is one additional sync call; the benefit is correct space accounting
in all cases.

## Assumptions

1. **`btrfs send --help` outputs `--compressed-data` in its flag list on kernels ≥ 5.18.**
   This is the documented flag name in btrfs-progs source. If the flag name changes in a
   future release, the probe string must be updated — but that would be an upstream
   breaking change, not a silent incompatibility.

2. **`btrfs subvolume sync` is available on all supported kernels.** The `sync` subcommand
   predates 5.18; it is present on any kernel Urd targets. No capability probe needed.

3. **Sync is called against the snapshot root directory, not individual snapshot paths.**
   `btrfs subvolume sync <dir>` syncs all pending deletions under `<dir>`. This matches
   the executor's existing grouping by snapshot root.

4. **`sudo` is required for `sync_subvolumes`.** All btrfs operations are sudoed. The
   sudoers configuration must allow `btrfs subvolume sync`. The existing sudoers grant
   covers `btrfs subvolume *` so no sudoers change is needed.

## Open Questions

1. **Does the probe need to be cached across daemon restarts (Sentinel)?** Currently the
   probe runs once at process startup. The Sentinel daemon is long-lived. If btrfs-progs
   is upgraded while the Sentinel is running, the probe result becomes stale. This is
   harmless — the old (false) value just means no `--compressed-data` until the next
   restart. Not worth re-probing on each backup cycle.

2. **Should the log line for 013-a be `info!` or `debug!`?** `info!` makes it visible in
   normal operation and confirms the flag is active without searching debug logs. On the
   other hand, it fires once per send, which may clutter logs on multi-subvolume configs.
   Tentative: `info!` — confirming protocol capabilities per transfer is useful signal.
   Revisit after first production run.

3. **Should 013-b sync external drives too?** Assuming yes (see Rejected Alternatives).
   If external drive sync proves slow in practice, a `debug!`-level timer log around the
   sync call would help diagnose without changing behavior.
