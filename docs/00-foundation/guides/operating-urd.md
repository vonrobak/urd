# Operating Urd

> **TL;DR:** How to build, install, update, and run Urd day-to-day. Covers the full
> lifecycle from code changes to a running backup system, including manual runs, systemd
> operation, troubleshooting, and the update workflow that trips people up — you must
> rebuild and reinstall the binary after code changes.

## Building and Installing

Urd compiles to a single binary. The systemd unit expects it at `~/.cargo/bin/urd`.

### First-time install

```bash
cd ~/projects/urd
cargo build --release
cp target/release/urd ~/.cargo/bin/urd
```

Or use cargo's built-in install (also places the binary in `~/.cargo/bin/`):

```bash
cargo install --path .
```

### Updating after code changes

**This is the step people forget.** Code changes (new features, bug fixes, review fixes)
only take effect after rebuilding and replacing the binary. If you pull new code or merge
a PR but don't rebuild, the running binary is still the old version.

```bash
cd ~/projects/urd
git pull                          # or: merge the PR branch
cargo build --release
cp target/release/urd ~/.cargo/bin/urd
```

Or equivalently:

```bash
cargo install --path . --force
```

Verify the binary is current:

```bash
urd --version                     # should show the expected version
```

**If systemd is running the timer**, the next scheduled run will use the new binary
automatically — the service unit runs `%h/.cargo/bin/urd backup` each time, so it
always picks up whatever binary is at that path. No `daemon-reload` needed for binary
updates (only for unit file changes).

## Initial Setup

After the first install, configure and validate:

```bash
# 1. Create config from template
cp ~/projects/urd/config/urd.toml.example ~/.config/urd/urd.toml
# Edit: set drive mount paths, subvolume sources, snapshot roots for your system

# 2. Validate system readiness
urd init

# 3. Measure snapshot sizes (space estimation for first external sends)
urd calibrate

# 4. Preview the backup plan
urd plan

# 5. Dry-run to confirm
urd backup --dry-run

# 6. Run a real backup
urd backup

# 7. Install the systemd timer for nightly runs
cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now urd-backup.timer
```

## CLI Commands

All commands accept `--config PATH` to override the default config location
(`~/.config/urd/urd.toml`) and `--verbose` for debug logging.

### `urd plan`

Preview what a backup run would do. Safe to run anytime — read-only.

```bash
urd plan                          # full plan
urd plan --priority 1             # only high-priority subvolumes
urd plan --subvolume htpc-home    # one subvolume
urd plan --local-only             # skip external send operations
urd plan --external-only          # skip local snapshot operations
```

### `urd backup`

Execute the backup: create snapshots, send to external drives, run retention.

```bash
urd backup                        # full backup
urd backup --dry-run              # same as urd plan
urd backup --priority 1           # high-priority only
urd backup --subvolume htpc-home  # one subvolume
urd backup --local-only           # snapshots + local retention only
urd backup --external-only        # external sends + external retention only
```

The backup acquires an advisory lock — only one instance runs at a time. If another
is running, it exits immediately with an error message.

On a TTY, sends show live progress on stderr: `47.3 MB @ 156.2 MB/s [0:03]`.
Under systemd (no TTY), progress is suppressed.

Ctrl+C triggers graceful shutdown: the current operation finishes, partial snapshots
are cleaned up, and the run exits with a partial result.

### `urd status`

Quick health overview: snapshot counts, drive status, chain health, last run result.

```bash
urd status
```

### `urd history`

Query the SQLite run history.

```bash
urd history                       # last 10 runs
urd history --last 3              # last 3 runs
urd history --subvolume htpc-home # operations for one subvolume
urd history --failures            # only failed operations
```

### `urd verify`

Check incremental chain integrity, pin file health, and detect orphaned snapshots.

```bash
urd verify                        # all subvolumes, all drives
urd verify --subvolume htpc-home  # one subvolume
urd verify --drive WD-18TB        # one drive
```

### `urd calibrate`

Measure snapshot sizes for space estimation. Run before first external sends or to
refresh stale estimates (the planner warns when calibration is >30 days old).

```bash
urd calibrate                     # all subvolumes
urd calibrate --subvolume htpc-home
```

### `urd init`

Validate system readiness. Idempotent — safe to run repeatedly.

Checks: state DB, metrics directory, lock file directory, sudo btrfs access, subvolume
sources, snapshot roots, drive status, pin files. Detects and offers to clean up
incomplete snapshots on external drives from interrupted sends.

```bash
urd init
```

## Systemd Operation

### Timer and service

Urd runs nightly at 02:00 via a user-level systemd timer.

| Unit | Purpose |
|------|---------|
| `urd-backup.timer` | Triggers the service at 02:00 daily (with 5m random delay) |
| `urd-backup.service` | Runs `~/.cargo/bin/urd backup` as a oneshot |

The service runs at low priority (`Nice=19`, `IOSchedulingClass=idle`) and allows up
to 6 hours for large sends.

### Install / update units

```bash
cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now urd-backup.timer
```

Re-run this after modifying unit files in the repo. See
[CONTRIBUTING.md](../../CONTRIBUTING.md#systemd-deployment) for the rationale behind
copying rather than symlinking.

### Monitor

```bash
systemctl --user status urd-backup.timer      # next scheduled run
systemctl --user list-timers urd-backup*      # timer details
journalctl --user -u urd-backup.service       # all run output
journalctl --user -u urd-backup.service -f    # follow live
journalctl --user -u urd-backup.service --since today
```

### Manual trigger via systemd

To run the backup through systemd (uses the same environment as scheduled runs):

```bash
systemctl --user start urd-backup.service
journalctl --user -u urd-backup.service -f    # watch it
```

### Change schedule

```bash
systemctl --user edit urd-backup.timer
# Add override:
# [Timer]
# OnCalendar=*-*-* 03:00:00
systemctl --user daemon-reload
```

### Debug logging

```bash
# One-off:
RUST_LOG=debug urd backup

# For systemd runs:
systemctl --user edit urd-backup.service
# Add:
# [Service]
# Environment=RUST_LOG=debug
systemctl --user daemon-reload
```

## Common Workflows

### After merging new code

```bash
cd ~/projects/urd
git pull
cargo build --release
cp target/release/urd ~/.cargo/bin/urd
urd --version                     # verify
urd plan                          # preview with new logic
```

If systemd units changed too:

```bash
cp systemd/urd-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
```

### Backup to a specific drive

Mount the drive, then run external-only:

```bash
urd backup --external-only
```

The planner automatically detects which drives are mounted and sends to all of them.

### Diagnose a failed backup

```bash
urd history --failures            # what failed and when
urd verify                        # chain health
urd status                        # current state
journalctl --user -u urd-backup.service --since "2 days ago"
```

Common causes:
- **Drive not mounted** — the planner skips unmounted drives (not an error)
- **Insufficient space** — the planner skips sends that would exceed available space
  (shown as "estimated ~X exceeds Y available" in the skip reason)
- **Broken chain** — parent snapshot deleted or missing on external drive. Next send
  will be a full send (large but self-repairing)
- **Sudo permission** — `urd init` validates this; check `/etc/sudoers.d/btrfs-backup`

### Recalibrate after major data changes

If you've added or deleted a large amount of data in a subvolume:

```bash
# Create a fresh snapshot first (calibrate measures the newest snapshot)
urd backup --local-only --subvolume htpc-home

# Then recalibrate
urd calibrate --subvolume htpc-home
```

## Configuration Reference

Config file: `~/.config/urd/urd.toml` (override with `--config`).
State database: `~/.local/share/urd/urd.db`.
Example config: `config/urd.toml.example`.

See the example config for the full structure with comments. Key sections:

| Section | Purpose |
|---------|---------|
| `[general]` | Paths to state DB, metrics file, btrfs binary |
| `[local_snapshots]` | Snapshot roots and which subvolumes go where |
| `[defaults]` | Intervals, retention policies (inherited by subvolumes) |
| `[[drives]]` | External drives: mount path, space thresholds, role |
| `[[subvolumes]]` | What to back up: source, priority, per-subvolume overrides |

### Interval formats

`15m`, `1h`, `4h`, `1d`, `1w` — used for `snapshot_interval` and `send_interval`.

### Priority levels

| Priority | Meaning | Typical interval |
|----------|---------|-----------------|
| 1 | Critical — active data, frequent changes | 15m snapshots, 1-2h sends |
| 2 | Important — regular data | 1h snapshots, 4h sends |
| 3 | Standard — slow-changing or large data | 1d-1w snapshots, 1d sends |

Use `--priority N` with `urd plan` or `urd backup` to operate on one tier at a time.

## Prometheus Metrics

Urd writes metrics in Prometheus text exposition format to the configured
`metrics_file` after every backup run. Point a node_exporter textfile collector at
this directory, or scrape it directly.

Key metrics: `urd_subvolume_success`, `urd_subvolume_duration_seconds`,
`urd_subvolume_last_success_timestamp`, `urd_external_drive_mounted`,
`urd_external_free_bytes`, `urd_script_last_run_timestamp`.
