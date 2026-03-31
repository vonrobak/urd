# Urd: BTRFS Time Machine for Linux

> **Provenance:** This document combines the original project roadmap (2026-03-22) with
> the living feature tracker. The first half (Context through Critical Files Reference) is
> the founding architectural vision. The second half (Current Priorities onward) tracks
> what to build next, what's been completed, and known tech debt. For current project
> state (what's deployed right now), see [status.md](status.md).

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

### Phase 4: Cutover + Polish (Session 7) — code ✅, operations pending

**Goal:** Urd is sole backup system.

**Code (complete):** CLI help polish, btrfs_path validation, crash-recovery test, --verbose
flag, dead code removal, tabled dependency removed. See [Phase 4 journal](../98-journals/2026-03-22-urd-phase4.md).

**Operations (not started):**
- Install and enable urd-backup.timer, run parallel with bash for 1-2 weeks
- Disable bash script timer after validation
- Write ADR-021 for the migration
- Archive bash script to `scripts/archive/`

See [status.md](status.md) for the detailed cutover checklist.

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

---

## Current Priorities

> This section is a living tracker. Updated when priorities change.

### Priority 5: Sentinel (paused — resumed after Priority 6)

The Sentinel is three independent systems that compose. Sessions 1-2 complete and deployed.
Sessions 3-4 deferred below Priority 6 UX work. Session 3's notification dedup is subsumed
by 6-I's structured advisory cooldown mechanism.

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 5a | **Notification dispatcher** | **Complete** | `notify.rs`: `compute_notifications()` pure function, 4 channel types, urgency filtering. 18 tests. |
| 5b | **Event reactor (passive)** | **Sessions 1-2 complete, deployed** | Pure state machine + I/O runner + CLI. Running as systemd user service. |
| 5b-3 | **Sentinel hardening** | Deferred | Scoped after 6-I ships (notification dedup subsumed by advisory cooldown). |
| 5c | **Active mode** | Designed, not built | Auto-trigger logic: `should_trigger_backup()`, `TriggerPermission`. After Priority 6. |

### Priority 5.5: Safety & Visibility — Complete

All 10 items built and deployed (v0.4.1–v0.5.0):
- Hardware swap defenses (drive session tokens, chain health as awareness input, full-send gate)
- Visual feedback model (OperationalHealth enum, two-axis CLI, sentinel visual_state)
- Transient snapshots (local_retention = "transient", immediate cleanup)

| Item | Design | Review |
|------|--------|--------|
| Hardware swap defenses | [design](../95-ideas/2026-03-28-design-hardware-swap-defenses.md) | [review](../99-reports/2026-03-28-hardware-swap-defenses-design-review.md) |
| Visual feedback model | [design](../95-ideas/2026-03-28-design-visual-feedback-model.md) | [review](../99-reports/2026-03-28-visual-feedback-model-design-review.md) |
| Spindle tray icon | [brainstorm](../95-ideas/2026-03-28-brainstorm-tray-icon-spindle.md) | Needs design doc (build after Phase 5) |

### Priority 6: Voice, UX & Redundancy Guidance

The next arc: unify Urd's voice, build high-impact UX features, and teach 3-2-1 strategy
through the promise system and progressive disclosure. Two brainstorm sessions resolved
the complete vocabulary and scored UX features. Six phase designs reviewed by arch-adversary.

**Two arcs, one dependency chain:**

```
Voice & UX Arc                        Progressive & Setup Arc
──────────────                        ───────────────────────
Merge 6-B + 6-E (ready now)
  │
Phase 1: Vocabulary landing
  │
Phase 2a+2c: urd default + completions
  │
6-I: Advisory system ─────────────────→ 6-O: Progressive disclosure (2 sessions)
  │                                        │
6-N + Phase 2b: Retention + doctor        ADR-110 enum rename (1 session)
  │                                        │
Phase 4a+4b: Escalation + suggestions    Config Serialize refactor (0.5 session)
  │                                        │
Phase 4c: Mythic transitions             6-H: Guided setup wizard (4 sessions)
```

**Estimated total: 15 sessions, ~150 new/modified tests, test suite 589 → ~740.**

#### Feature table

| # | Feature | Effort | Status | Design | Review |
|---|---------|--------|--------|--------|--------|
| 6-B | **Transient immediate cleanup** | 1 session | **Built, reviewed** | [design](../95-ideas/2026-03-31-design-b-transient-immediate-cleanup.md) | [review](../99-reports/2026-03-31-design-b-review.md) |
| 6-E | **Promise redundancy encoding** | 1 session | **Built, reviewed** | [design](../95-ideas/2026-03-31-design-e-promise-redundancy-encoding.md) | [review](../99-reports/2026-03-31-design-e-review.md) |
| P1 | **Vocabulary landing** | 1 session | Designed, reviewed | [design](../95-ideas/2026-03-31-design-phase1-vocabulary-landing.md) | [review](../99-reports/2026-03-31-design-phase1-vocabulary-landing-review.md) |
| P2a | **`urd` default status** | 1 session | Designed, reviewed | [design](../95-ideas/2026-03-31-design-phase2-ux-commands.md) | [review](../99-reports/2026-03-31-design-phase2-ux-commands-review.md) |
| P2b | **`urd doctor`** | 1 session | Designed, reviewed | (same as P2a) | (same as P2a) |
| P2c | **Shell completions** | 0.5 session | Designed, reviewed | (same as P2a) | (same as P2a) |
| 6-I | **Redundancy recommendations** | 1–2 sessions | Designed, reviewed | [design](../95-ideas/2026-03-31-design-i-redundancy-recommendations.md) | [review](../99-reports/2026-03-31-design-phase3-advisory-retention-review.md) |
| 6-N | **Retention policy preview** | 1 session | Designed, reviewed | [design](../95-ideas/2026-03-31-design-n-retention-policy-preview.md) | (same as 6-I) |
| P4a | **Staleness escalation** | 1 session | Designed, reviewed | [design](../95-ideas/2026-03-31-design-phase4-voice-enrichment.md) | [review](../99-reports/2026-03-31-arch-adversary-phase4-voice-enrichment.md) |
| P4b | **Next-action suggestions** | (with P4a) | Designed, reviewed | (same as P4a) | (same as P4a) |
| P4c | **Mythic voice on transitions** | 0.5-1 session | Designed, reviewed | (same as P4a) | (same as P4a) |
| 6-O | **Progressive disclosure** | 2 sessions | Designed, reviewed | [design](../95-ideas/2026-03-31-design-o-progressive-disclosure.md) | [review](../99-reports/2026-03-31-design-phase5-progressive-disclosure-review.md) |
| P6a | **ADR-110 enum rename** | 1 session | Designed, reviewed | [design](../95-ideas/2026-03-31-design-phase6-protection-rename-wizard.md) | [review](../99-reports/2026-03-31-design-phase6-protection-rename-wizard-review.md) |
| P6b | **Config Serialize refactor** | 0.5 session | Prerequisite for 6-H | (same as P6a) | (same as P6a) |
| 6-H | **Guided setup wizard** | 4 sessions | Designed, reviewed | [design](../95-ideas/2026-03-31-design-h-guided-setup-wizard.md) | [review](../99-reports/2026-03-31-design-h-review.md) |

#### Key review findings incorporated

| Finding | Source | Resolution |
|---------|--------|------------|
| Heartbeat schema bump likely unnecessary | Phase 6 S-1 | Drop — heartbeat serializes promise states, not level names |
| Config key rename scope too large | Phase 6 M-1 | Defer to ADR-111 config overhaul |
| Voice thresholds contradict awareness | Phase 4 S-1 | Derive from PromiseStatus, not independent thresholds |
| Cooldown key must include drive_label | Phase 3 S-1 | Design update before 6-I implementation |
| Config error conflation in urd default | Phase 2 S-1 | Distinguish NotFound from ParseError |
| Sentinel notification dedup | Phase 3 | Subsumed by 6-I cooldown mechanism |

#### Brainstorm artifacts

| Document | Content |
|----------|---------|
| [Next-level UX brainstorm](../95-ideas/2026-03-31-brainstorm-next-level-ux.md) | 12 candidates scored, 7 accepted (scores 6-10), 4 rejected |
| [Vocabulary audit](../95-ideas/2026-03-31-brainstorm-vocabulary-audit.md) | Complete vocabulary redesign: exposure triad, protection levels, thread, drives, skip tags |

#### Also in this priority (independent)

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 6-Sp | **Tray icon (Spindle)** | Low–Medium | Design after Phase 4/5 visual improvements. Build after 6-O. [brainstorm](../95-ideas/2026-03-28-brainstorm-tray-icon-spindle.md). |

#### Deferred from old Priority 6

| # | Feature | Notes |
|---|---------|-------|
| 6b | **Smart defaults** | Subsumed by H (wizard infers from filesystem). |
| 6d | **Drive replacement workflow** | Build after H proves the guided interaction pattern. |
| 6e | **`urd find` (cross-snapshot search)** | Unsolved perf problem. Deferred until `urd get` proves restore UX. |

### Priority 7: Experience Polish

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 7a | **Recovery contract** | Low-Medium | Generated from config + awareness model state. Overlaps with H's coverage map. |
| 7b | **Deep verification** | Medium | `urd verify --deep`: random-sample checksums from external vs. local. |
| 7c | **Attention budget** | Medium | Priority queue in awareness model. Filter by urgency. |
| 7d | **Config validation as simulation** | Medium | Subsumed by N (retention preview) and H (setup wizard). |

### Deferred

| Feature | Rationale |
|---------|-----------|
| SSH remote targets | Keep the app simple for now. |
| Cloud backup (S3/B2) | Indefinitely. |
| Pull mode / mesh | Indefinitely. |
| Multi-user / library mode | No current need. |

### Open gates (from ADR-110/111)

- [ ] Drive topology constraints — capacity checks require I/O, deferred to Sentinel
- [ ] Awareness threshold mode — fixed multipliers regardless of run frequency, deferred
- [ ] Config schema migration (ADR-111) — target architecture defined, legacy schema in use; config key rename (`protection_level` → `protection`) deferred here
- [ ] Protection level enum rename — resolved vocabulary (recorded/sheltered/fortified), scheduled as P6a after 6-O ships
- [ ] Heartbeat schema bump — likely unnecessary per Phase 6 review S-1 (heartbeat serializes promise states, not level names); verify before P6a

## Completed Features

### Priority 2: Safety Hardening — Complete

All five items built, adversary-reviewed, and deployed:
- UUID drive fingerprinting (`DriveAvailability` enum, `findmnt` detection)
- Local space guard (planner gates on `min_free_bytes`)
- Surface skipped sends (subsumed by structured summary)
- Post-backup structured summary (`BackupSummary` output type)
- Pre-flight config checks (`preflight.rs`, 2 checks)
- Structured error messages (`translate_btrfs_error()`, 7 patterns)

### Priority 3: Architectural Foundation — Complete

- Awareness model (`awareness.rs`) — pure function, 24 tests
- Heartbeat file (`heartbeat.rs`) — JSON health signal, schema v1, 7 tests
- Presentation layer (`output.rs` + `voice.rs`) — 8/8 commands migrated
- `urd get` (`commands/get.rs`) — file restore, 19 tests

### Priority 4: Protection Promises — Complete

- `ProtectionLevel` enum, `derive_policy()`, config resolution branching
- Preflight achievability checks (drive-count, voiding, weakening)
- Planner drive filtering, `--confirm-retention-change` fail-closed gate
- Promise-anchored status display (conditional PROMISE column)
- ADR-110 written and revised, ADR-111 (config target architecture) written

## Phase Checklist

- [x] Phase 1 — Skeleton + Config + Plan (67 tests)
- [x] Phase 1.5 — Hardening (unsent protection, path safety, pin-on-success)
- [x] Phase 2 — Executor + State DB + Metrics + `urd backup`
- [x] Phase 3 — CLI commands + systemd units
- [x] Phase 3.5 — Hardening for cutover
- [x] Phase 4 code — Cutover polish + space estimation
- [ ] Phase 4 cutover — Operational transition (monitoring target: 2026-04-01)
- [x] Post-cutover — Priorities 2-4 complete
- [x] Phase 5 — Architectural foundation complete
- [x] Phase 6 — Protection promises complete
- [x] Phase 7 — Sentinel Sessions 1-2 deployed (Sessions 3-4 deferred after Priority 6)
- [x] Phase 7.5 — Safety & Visibility (5.5a-d: all 10 items complete, v0.4.1–v0.5.0)
- [ ] Phase 8 — Voice & UX overhaul (P1→P2→6-I→6-N→P4→6-O→P6a→6-H)
- [ ] Phase 9 — Sentinel completion (Session 3 hardening, Session 4 active mode)
- [ ] Phase 10 — Spindle tray icon

## Dropped Features

- **Tier 2 filesystem-level upper bound** — wrong for actual data distribution
- **Tier 3 Option A opportunistic qgroup query** — quotas confirmed off, speculative

## Tech Debt

- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- `du -sb` may follow symlinks in snapshots — consider `-P` flag
- Stale failed send estimates persist indefinitely — consider TTL
- `SubvolumeResult.send_type` records only last send type for multi-drive sends
- `heartbeat::read()` returns `Option` — can't distinguish missing from corrupt
- Per-drive pin protection for external retention: all-drives-union is conservative
- `urd get` doesn't support directory restore (files only in v1)
- `warn_missing_uuids` spawns `findmnt` per drive on every run
- Bootstrap pattern: code touching `external_snapshot_dir()` may assume dirs exist
- MockBtrfs tests don't exercise filesystem preconditions
- Journal persistence gap: journald may purge user-unit logs
- NVMe snapshot accumulation above 10GB threshold not gated
