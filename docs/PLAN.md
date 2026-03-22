# Urd: BTRFS Time Machine for Linux

## Context

The homelab's backup system is a 1710-line bash script (`scripts/btrfs-snapshot-backup.sh`) that has been patched through three major incidents. It works but has reached its maintainability limit. The 2026-03-22 journal entry documents a critical audit (15 issues found) and recommends Option C: a purpose-built tool replacing the bash script entirely.

**Name:** Urd — the Norse norn who tends the Well of Urd and knows all that has passed.

**Ambition:** Build a distributable "Time Machine for BTRFS on Linux" that starts as a homelab tool and grows into something others can use.

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Language | **Rust** | Distributable binary, type safety, no runtime deps. Bash script covers us while we build. |
| Drive detection | **udev-first** | True Time Machine behavior. Scheduled fallback (02:00) as nice-to-have for large ops. |
| Execution model | **Hybrid** | Triggered service for backups + small persistent sentinel for drive monitoring/notifications. |
| State management | **SQLite + TOML** | TOML config (what to back up), SQLite state (what has been backed up). Clean separation. |
| CLI name | **`urd`** | `urd status`, `urd backup`, `urd verify`, `urd history` |
| Project location | **`~/projects/urd`** | Separate git repo, own Cargo workspace. |
| Notifications | **Deferred** | Build after core is battle-tested. Desktop (notify-send) + Discord for critical. |

## Project Structure

```
~/projects/urd/
  Cargo.toml
  src/
    main.rs               # CLI entry point (clap), dispatches to commands
    cli.rs                 # Clap command/argument definitions
    config.rs              # TOML config loading, validation, defaults
    types.rs               # Core domain types: Subvolume, Drive, Tier, Schedule, Snapshot
    plan.rs                # Backup planner: reads state, decides operations (pure logic)
    executor.rs            # Executes planned operations (snapshot, send, receive, delete)
    btrfs.rs               # Low-level btrfs command wrappers (trait-based for testing)
    retention.rs           # Retention engine (graduated + count-based)
    chain.rs               # Incremental chain tracking (pin files, parent resolution)
    state.rs               # SQLite state database (history, run records)
    metrics.rs             # Prometheus .prom file writer (atomic tmp+rename)
    drives.rs              # External drive detection (mount check, space via statvfs)
    sentinel.rs            # Drive monitor daemon (Phase 5)
    error.rs               # Error types (thiserror)
    commands/
      mod.rs
      backup.rs            # `urd backup`
      status.rs            # `urd status`
      verify.rs            # `urd verify`
      history.rs           # `urd history`
      plan_cmd.rs          # `urd plan` (dry-run planning)
      init.rs              # `urd init` (state import)
  config/
    urd.toml.example       # Reference config
  systemd/
    urd-backup.service     # Oneshot backup service
    urd-backup.timer       # Nightly fallback timer
    urd-sentinel.service   # Drive monitor (Phase 5)
  udev/
    99-urd-backup.rules    # Drive plug events (Phase 5)
  tests/
    integration/           # Tests requiring 2TB-backup drive (#[ignore])
  README.md
  LICENSE
```

## Configuration Schema (TOML)

Location: `~/.config/urd/urd.toml`

```toml
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/containers/data/backup-metrics/backup.prom"
log_dir = "~/containers/data/backup-logs"

[local_snapshots]
roots = [
  { path = "~/.snapshots", subvolumes = ["htpc-home", "htpc-root"] },
  { path = "/mnt/btrfs-pool/.snapshots", subvolumes = [
    "subvol1-docs", "subvol2-pics", "subvol3-opptak",
    "subvol4-multimedia", "subvol5-music", "subvol6-tmp", "subvol7-containers"
  ]}
]

[[drives]]
label = "WD-18TB"
mount_path = "/run/media/<user>/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "WD-18TB1"
mount_path = "/run/media/<user>/WD-18TB1"
snapshot_root = ".snapshots"
role = "offsite"

[[drives]]
label = "2TB-backup"
mount_path = "/run/media/<user>/2TB-backup"
snapshot_root = ".snapshots"
role = "test"

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0                    # 0 = unlimited

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/home"
priority = 1
snapshot_interval = "15m"
send_interval = "1h"

[[subvolumes]]
name = "subvol3-opptak"
short_name = "opptak"
source = "/mnt/btrfs-pool/subvol3-opptak"
priority = 1
snapshot_interval = "1h"
send_interval = "2h"

# ... (all 9 subvolumes, see config/urd.toml.example for full reference)
```

## Architecture: Key Design Principles

### 1. Planner/Executor Separation (most important)

The planner is a **pure function**: `fn plan(config, state, now, filters) -> BackupPlan`. It produces a list of `PlannedOperation` variants:

```rust
enum PlannedOperation {
    CreateSnapshot { source, dest, subvolume_name },
    SendIncremental { parent, snapshot, dest_dir, drive_label, subvolume_name, pin_on_success },
    SendFull { snapshot, dest_dir, drive_label, subvolume_name, pin_on_success },
    DeleteSnapshot { path, reason, subvolume_name },
}
```

Every variant carries `subvolume_name` so operations are self-describing — no path heuristics needed to determine ownership. Send variants carry `pin_on_success: Option<(PathBuf, SnapshotName)>` — the pin file is written by the executor only on successful send, making the send/pin dependency structural rather than implicit.

`urd plan` prints the plan. `urd backup --dry-run` prints it. `urd backup` executes it. The planner is fully unit-testable without touching any filesystem.

### 2. BtrfsOps Trait (testing seam)

```rust
pub trait BtrfsOps {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<()>;
    fn send_receive(&self, snapshot: &Path, parent: Option<&Path>, dest: &Path) -> Result<()>;
    fn delete_subvolume(&self, path: &Path) -> Result<()>;
    fn subvolume_show(&self, path: &Path) -> Result<SubvolumeInfo>;
}
```

`RealBtrfs` calls `sudo btrfs` via `std::process::Command`. `MockBtrfs` records calls for testing. This is the only module that shells out.

### 3. Never Lose Data, Always Continue

Individual subvolume failures do NOT abort the run. Failed sends trigger partial cleanup. Pin files are only updated on success. Exit code 1 if any subvolume failed, 0 if all succeeded/skipped.

### 4. Incremental Chain Integrity

The incremental send/receive chain is the most performance-critical property of the system. A broken chain forces a full send (potentially hundreds of GB) instead of a small incremental diff.

**Invariant:** Retention (local or external) must never delete a snapshot that is the current pin parent for any drive. The system enforces this through:
- Pin file targets are always in the `pinned` set and are never deleted by retention
- Unsent snapshot protection: when `send_enabled` is true, snapshots newer than the oldest pin are protected from local retention (they may not have been sent to all drives yet)
- The planner verifies the parent exists on both local and external before planning an incremental send; if not, it falls back to a full send

**Corollary:** The executor must re-check space between deletions on external drives rather than batch-deleting everything the planner proposed, because the planner cannot know exact snapshot sizes.

### 5. Backward Compatibility

- Snapshot naming:
  - **Current (write):** `YYYYMMDD-HHMM-shortname` (e.g., `20260322-1430-opptak`)
  - **Legacy (read-only):** `YYYYMMDD-shortname` (e.g., `20260322-opptak`) — parsed as midnight
  - Both formats coexist transparently in snapshot directories
- Directory structure: same locations as bash script
- Pin files: `.last-external-parent-{DRIVE_LABEL}` format preserved (read+write), with legacy `.last-external-parent` fallback for reading
- Prometheus metrics: identical names, labels, value semantics (see Prometheus Metrics below)

## Prometheus Metrics

File: `~/containers/data/backup-metrics/backup.prom` (configurable via `metrics_file`).
Written atomically (temp file + rename) to prevent partial reads by the Prometheus node exporter.

The metrics format must match the bash script's output exactly. Grafana dashboards and alerting depend on these names and labels.

### Per-subvolume metrics

| Metric | Type | Labels | Values |
|--------|------|--------|--------|
| `backup_last_success_timestamp` | gauge | `subvolume` | Unix timestamp; only set when `backup_success=1` |
| `backup_success` | gauge | `subvolume` | `1`=success, `0`=failure, `2`=schedule-skipped |
| `backup_duration_seconds` | gauge | `subvolume` | Integer seconds for the subvolume's operations |
| `backup_snapshot_count` | gauge | `subvolume`, `location` | Count of snapshots; `location` is `"local"` or `"external"` (external = first mounted drive by config order, for bash compat) |
| `backup_send_type` | gauge | `subvolume` | `0`=full, `1`=incremental, `2`=no send this run |

### Global metrics

| Metric | Type | Labels | Values |
|--------|------|--------|--------|
| `backup_external_drive_mounted` | gauge | none | `1`=any external drive mounted, `0`=none |
| `backup_external_free_bytes` | gauge | none | Free bytes on mounted external drive; `0` when unmounted |
| `backup_script_last_run_timestamp` | gauge | none | Unix timestamp when the backup ran |

**Multi-drive note:** The bash script assumes a single external drive. These three global metrics maintain that assumption for backward compatibility: `backup_external_drive_mounted` is `1` if *any* configured drive is mounted, and `backup_external_free_bytes` reports the free space of the first mounted drive (by config order). Phase 4 may add per-drive metrics with a `drive` label, but the global metrics must remain for Grafana compatibility.

### File format

Each metric group has `# HELP`, `# TYPE gauge`, then one or more value lines. Groups separated by blank lines. `backup_send_type` must emit an entry for every subvolume that has a `backup_success` entry (never omit a series).

Example:
```
# HELP backup_success Backup result: 1=success, 0=failure, 2=schedule-skipped
# TYPE backup_success gauge
backup_success{subvolume="subvol3-opptak"} 1
backup_success{subvolume="htpc-home"} 2
```

## SQLite Schema

The SQLite database records backup history. It is **not** the source of truth for current filesystem state — the filesystem (snapshot directories, pin files) is authoritative. SQLite answers "what happened" (runs, operations); the filesystem answers "what exists now" (snapshots, chain state).

```sql
CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    mode TEXT NOT NULL,        -- "full", "local-only", "external-only", "dry-run"
    result TEXT NOT NULL       -- "success", "partial", "failure"
);

CREATE TABLE operations (
    id INTEGER PRIMARY KEY,
    run_id INTEGER REFERENCES runs(id),
    subvolume TEXT NOT NULL,
    operation TEXT NOT NULL,   -- "snapshot", "send_incremental", "send_full", "delete"
    drive_label TEXT,
    duration_secs REAL,
    result TEXT NOT NULL,      -- "success", "failure", "skipped"
    error_message TEXT,
    bytes_transferred INTEGER
);
```

The `snapshots` table from the original plan has been removed. Pin files remain the source of truth for chain state, and snapshot directories are the source of truth for what exists. Duplicating this in SQLite creates a sync problem with no clear benefit — `urd status` and `urd history` can query the filesystem directly for current state and SQLite for historical data.

## CLI Commands

```
urd backup [--dry-run] [--local-only] [--external-only] [--tier N] [--subvolume NAME] [--verbose]
urd plan   [same filters as backup]     # Print planned operations without executing
urd status                              # Drives, chain health, snapshot counts, last run
urd history [--subvolume X] [--last N] [--failures]
urd verify  [--subvolume X] [--drive LABEL]  # Chain integrity, pin file validation
urd init                                # First-run: create config, import existing state
```

`urd status` example output:
```
SUBVOLUME          LOCAL  WD-18TB  WD-18TB1  LAST SEND    CHAIN
htpc-home          15     14       12        2h ago       incremental
subvol3-opptak     15     1        0         6h ago       full (new)
subvol7-containers 15     2        1         23h ago      incremental

Drives: WD-18TB1 mounted (4.4TB free / 17TB)
Next scheduled: subvol2-pics in 5d (Saturday)
```

## Rust Crates

| Crate | Purpose |
|-------|---------|
| `clap` 4.x | CLI with derive macros |
| `serde` + `toml` | Config parsing |
| `rusqlite` (bundled) | SQLite state DB |
| `chrono` | Date/time, ISO week calc |
| `thiserror` + `anyhow` | Error handling |
| `nix` | statvfs for disk space |
| `colored` | Terminal colors |
| `tabled` | Table formatting for status/history |
| `dirs` | XDG directory resolution |
| `log` + `env_logger` | Structured logging |
| `udev` | Phase 5: drive monitoring |

**Not using:** tokio/async (unnecessary for sequential I/O-bound btrfs ops).

## Implementation Phases

### Phase 1: Skeleton + Config + Plan ✅

**Goal:** `urd plan` reads config, discovers real snapshots, prints correct planned operations.

**Completed:**
- `config.rs` — TOML parsing, validation, tilde expansion, PathBuf throughout
- `types.rs` — All domain types (Interval, SnapshotName with dual-format, PlannedOperation, ByteSize)
- `cli.rs` — clap setup for all commands
- `plan.rs` — planner logic (schedule, snapshot discovery, parent resolution, unsent protection)
- `chain.rs` — pin file reading (drive-specific + legacy fallback)
- `retention.rs` — graduated + count-based + space-governed retention
- `drives.rs` — drive detection, space checks via statvfs, external snapshot dir construction
- `error.rs` — thiserror error types
- `commands/plan_cmd.rs` — `urd plan` with colored grouped output
- 67 unit tests across all modules

**Hardening (Phase 1.5):**
- Unsent snapshot protection (prevents retention from deleting snapshots not yet sent externally)
- `PinParent` removed — pin is now `pin_on_success` field on Send variants
- `subvolume_name` added to all `PlannedOperation` variants
- PathBuf migration (no more `to_string_lossy()` roundtrips)
- Path validation (`validate_path_safe`, `validate_name_safe`)
- Future-date snapshot warning
- Monthly retention uses calendar month subtraction (not `days * 30`)

### Phase 2: Execute + State + Metrics (Sessions 3-4)

**Goal:** `urd backup` creates snapshots, sends to 2TB-backup, manages retention, writes metrics.

**New modules:**
- `btrfs.rs` — BtrfsOps trait + RealBtrfs + MockBtrfs
- `executor.rs` — sequential operation execution (see Executor Contract below)
- `state.rs` — SQLite schema, run/operation recording
- `metrics.rs` — Prometheus .prom writer (exact format match)

**Additions to existing modules:**
- `chain.rs` — pin file writing (reading already done in Phase 1)
- `error.rs` — executor error variants (BtrfsError, ExecutorError)

**New commands:**
- `commands/backup.rs` — `urd backup` (planner + executor), `--dry-run` prints plan
- `commands/init.rs` — `urd init` (first-run setup: create SQLite DB, verify config paths exist, verify pin files are readable, detect and flag incomplete snapshots on external drives from interrupted bash script runs — offer cleanup with user confirmation, never silently delete, report system state summary)

**Testing:**
- Unit tests with MockBtrfs for executor logic
- Integration tests on 2TB-backup drive (`#[ignore]`)

**Deliverable:** Successful backup cycle to test drive. `urd backup --dry-run` on production matches bash.

#### Executor Contract

The executor takes a `BackupPlan` and executes each operation sequentially. Its behavior is governed by these rules:

**Error isolation:** A failure in one subvolume must NOT abort operations for other subvolumes. The executor groups operations by `subvolume_name` and tracks per-subvolume success/failure. The overall result is `success` (all OK), `partial` (some failed), or `failure` (all failed).

**Send execution:** The `btrfs send | btrfs receive` pipeline must:
- Capture stderr from both the send and receive sides
- Check exit codes from both processes
- On failure, clean up any partial snapshot at the destination (`btrfs subvolume delete` on the incomplete receive)
- Log both stderr streams for diagnostics

**Pin-on-success:** After a successful send, the executor writes the pin file specified in `pin_on_success`. If the pin file write fails, the executor logs a warning and continues — the send itself succeeded and the snapshot is valid on the destination. The pin can be recovered on the next successful send to that drive. Note: repeated pin failures (e.g., due to a read-only mount) degrade performance by forcing full sends. Phase 3's `urd verify` / `urd status` should surface stale pins so the operator can investigate.

**Retention execution on external drives:** The planner proposes deletions based on a point-in-time space check, but it cannot know exact snapshot sizes. The executor must:
- Execute external deletions oldest-first
- Re-check free space after each deletion
- Stop deleting once the `min_free_bytes` threshold is satisfied, logging skipped deletions with reason ("space recovered, N planned deletions skipped") so `urd plan` vs `urd backup` divergence is visible to the operator
- Never delete a snapshot that is the current pin parent for that drive (defense-in-depth — the planner already excludes these, but the executor double-checks)

**Cascading failure handling:** When an operation fails, the executor must skip dependent operations within the same subvolume rather than letting them fail naturally. Specifically: if a `CreateSnapshot` fails, skip any subsequent `Send` that references the snapshot that was not created. The executor checks that source paths exist before attempting operations. This prevents confusing cascading errors in logs and ensures error messages reflect the root cause.

**Crash recovery:** The executor does not assume clean state from prior runs. Before sending a snapshot to a drive, it checks whether a subvolume with that name already exists at the destination. If it exists but is not the result of a completed send (i.e., the pin file does not point to it), it deletes the partial and proceeds with a fresh send. This handles the case where a prior run was interrupted mid-transfer (power loss, drive disconnect, OOM kill). The pin file is the source of truth for "last successful send" — if the pin doesn't reference a snapshot, that snapshot's presence at the destination is not trusted for incremental chain purposes.

**Operation ordering:** Within a subvolume, operations execute in plan order: create → send → delete. This ensures new snapshots exist before sends reference them, and deletions happen after sends complete. This ordering is load-bearing — the planner emits operations in this order (see comment in `plan()`) and the executor relies on it.

### Phase 3: CLI + Parallel Run (Sessions 5-6) ✅

**Goal:** Full CLI, parallel running with bash script for validation.

**New commands:**
- `commands/status.rs` — per-subvolume table (local/external counts, chain health), drive summary, last run from SQLite
- `commands/history.rs` — recent runs, `--subvolume` filter, `--failures`, `--last N`
- `commands/verify.rs` — pin file validation, pinned snapshot existence (local + external), orphan detection, stale pin detection

**New CLI args:**
- `HistoryArgs`: `--last N` (default 10), `--subvolume NAME`, `--failures`
- `VerifyArgs`: `--subvolume NAME`, `--drive LABEL`

**StateDb query methods added:**
- `last_run()`, `recent_runs(limit)`, `run_operations(run_id)`, `subvolume_history(name, limit)`, `recent_failures(limit)`
- New types: `RunRecord`, `OperationRow` (query result types separate from write type `OperationRecord`)

**Adversary review fixes applied:**
- Fixed `to_string_lossy()` in `RealBtrfs` — `create_readonly_snapshot` and `delete_subvolume` now use `.arg(path)` directly on `Command` instead of `run_btrfs(&[&str])`. The `run_btrfs` helper was removed entirely.
- Extracted `first_mounted_drive_status()` to `drives.rs` for reuse by `status.rs`
- Consolidated duplicate metrics helpers in `backup.rs` (`append_skipped_metrics`, `write_global_metrics`)

**Systemd units:**
- `systemd/urd-backup.service` — oneshot service, `ExecStart=%h/.cargo/bin/urd backup`
- `systemd/urd-backup.timer` — 02:00 daily, `Persistent=true`, `RandomizedDelaySec=300`

**Parallel run strategy:**
- Pin file contention: no separate namespaces needed. Atomic writes + 1-hour separation sufficient. Last writer wins (correct behavior — pin should reflect most recent successful send).
- Timing: Urd at 02:00, bash at 03:00. Separate lock files (can technically overlap, but btrfs ops on different subvolumes are safe concurrently).
- Install: `cp systemd/*.{service,timer} ~/.config/systemd/user/ && systemctl --user daemon-reload && systemctl --user enable --now urd-backup.timer`
- Change bash timer: `systemctl --user edit btrfs-backup-daily.timer` → set `OnCalendar=*-*-* 03:00:00`

**Deliverable:** Both systems running nightly with equivalent results.

### Phase 4: Cutover + Polish (Session 7)

**Goal:** Urd is sole backup system.

- Disable bash script timer, enable urd-backup.timer
- Colored terminal output
- Error message quality pass
- `--help` polish
- Write ADR-021 for the migration
- Archive bash script to `scripts/archive/`

### Phase 5: Sentinel + udev (Session 8, deferred)

**Goal:** Automatic backup on drive plug-in.

- `sentinel.rs` — udev event listener, debounce (wait for LUKS+mount)
- `urd-sentinel.service` — persistent user service
- udev rules for WD-18TB drives
- Desktop notification hooks (infrastructure only — notification system built separately)

## Migration Strategy

1. **`urd init`** creates SQLite DB, verifies config paths, validates pin files, detects incomplete snapshots on external drives
2. **Parallel running** (2 weeks): Urd at 02:00, bash at 03:00, compare metrics
3. **Cutover:** disable bash timer, enable Urd timer, monitor 1 week
4. Pin file format maintained throughout — both systems can coexist

## Testing Prerequisites

**Sudoers entries needed for 2TB-backup drive** (user must add manually):
```
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs subvolume delete /run/media/<user>/2TB-backup/.snapshots/*
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs receive /run/media/<user>/2TB-backup/.snapshots/*
```

Existing `btrfs send *` and `btrfs subvolume snapshot -r` entries cover the local side.

## Verification Plan

After each phase:

1. **Phase 1:** `urd plan` output matches manual calculation of what bash script would do for the same date/drive state
2. **Phase 2:** Successful backup to 2TB-backup drive. Restore a file from the backup and verify byte-for-byte match. `backup.prom` output matches format exactly.
3. **Phase 3:** After 2 weeks parallel run, diff Prometheus metrics between Urd and bash script runs. All subvolumes, all drives, all metrics must match.
4. **Phase 4:** One week of sole Urd operation with no intervention. Grafana dashboards show continuity.
5. **Phase 5:** Plug in test drive, verify backup starts within 60 seconds.

## Critical Files Reference

- `~/containers/scripts/btrfs-snapshot-backup.sh` — behavior to match
- `~/containers/data/backup-metrics/backup.prom` — metrics format to reproduce
- `~/.config/systemd/user/btrfs-backup-daily.service` — systemd patterns to follow
- `~/.snapshots/htpc-home/.last-external-parent-WD-18TB1` — pin file format
- `/etc/sudoers.d/btrfs-backup` — scope constraints for btrfs commands
