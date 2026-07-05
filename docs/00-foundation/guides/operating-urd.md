# Operating Urd

> **TL;DR:** How to build, install, update, and run Urd day-to-day. Covers the full
> lifecycle from code changes to a running backup system, including manual runs, systemd
> operation, troubleshooting, and the update workflow that trips people up — you must
> rebuild and reinstall the binary after code changes.

## Building and Installing

Urd compiles to a single binary. The systemd unit expects it at `~/.cargo/bin/urd`.

### First-time install

From the root of your clone:

```bash
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
a PR but don't rebuild, the running binary is still the old version. From the root of
your clone:

```bash
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

> **This manual path is interim.** The Encounter — Urd's guided first-run
> conversation, in development — replaces everything from here through Initial
> Setup: generated config, initialized drive, created directories, verified
> identity. Until it ships, follow these steps exactly and in order; the traps
> called out below were all hit in live field testing (2026-07-04).

## Sudoers Configuration

Urd runs as a regular user and calls `sudo btrfs` for privileged filesystem operations.
A scoped sudoers file grants exactly the permissions needed — nothing more.

**The guided path (recommended): let Urd render and install it.** The sudoers content
is derived from your config, and `urd` is the single oracle for its shape
(`src/sudoers.rs` — the module doc carries the full scope rationale). After the Fate
Conversation carves a config — or any time later — run:

```bash
urd init
```

With a config present and no working grant, `urd init` resumes **the earning**: it
renders the exact file your config requires, checks it with `visudo -c`, shows it to
you, and — with your consent — installs it fail-closed (staged inertly inside
`/etc/sudoers.d` under a dot-name sudo ignores, re-validated as root, then activated
by an atomic rename). It verifies the grant afterwards with a passwordless probe and
cross-checks coverage via `sudo -l`. If you decline, Urd prints the content and the
manual command instead; `urd status` names the "configured but unsealed" state until
the grant exists, and `urd doctor` warns when the installed grant drifts behind a
grown config.

> **Declined-path note for monitoring operators:** human-invoked `urd` commands
> (`status`, `init`, `doctor`, bare `urd`) each probe the grant with a single
> non-interactive `sudo -n` call. On an unsealed system that denied probe writes one
> auth-log line per invocation — whitelist it or seal. Urd's daemons (sentinel,
> heartbeat, metrics) never probe.

**The manual path** (also the printed fallback when you refuse the guided install).
Create `/etc/sudoers.d/urd` (requires root):

```bash
sudo visudo -f /etc/sudoers.d/urd
```

Below is a complete, working example for user **`alice`** backing up `/home` to the
local snapshot root `/home/alice/.snapshots` and an external drive labeled
**`backup-1`**, with btrfs at `/usr/bin/btrfs`. **Do not paste it unchanged** —
substitute three things for your system first:

1. the username (`alice` → yours),
2. the btrfs path (use the output of `which btrfs`, everywhere it appears),
3. the source and snapshot-root paths (from your config).

```sudoers
# Urd — scoped btrfs permissions for automated backups
# Security principle: scope snapshot creation and deletion to snapshot directories.
# send/receive need broad paths (source subvolumes and external drives vary).
# show/sync are read-only diagnostics.

# Snapshot creation — scoped to snapshot directories
# One line per source → snapshot-root mapping in your config.
alice ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume snapshot -r /home /home/alice/.snapshots/*

# Snapshot deletion — scoped to snapshot directories only
# One line per snapshot root (local and each external drive).
alice ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume delete /home/alice/.snapshots/*
alice ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume delete /run/media/alice/backup-1/.snapshots/*

# Send/receive — broad paths (source subvolumes and external drives vary)
alice ALL=(root) NOPASSWD: /usr/bin/btrfs send *
alice ALL=(root) NOPASSWD: /usr/bin/btrfs receive *

# Read-only commands — space estimation, diagnostics, sync after delete
alice ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume show *
alice ALL=(root) NOPASSWD: /usr/bin/btrfs filesystem show *
alice ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume sync *
```

If visudo reports `syntax error` lines when you save and drops you at a
`What now?` prompt, type `e` to re-open the file and fix it — usually a
placeholder or path left unsubstituted. visudo never installs a broken file,
so nothing is damaged; just correct and re-save.

Verify it works:

```bash
sudo btrfs subvolume show /    # should succeed without a password prompt
```

**Why scope snapshot and delete?** A wildcard like `btrfs subvolume delete *` would let
any process running as your user delete any subvolume on the system — not just snapshots.
Scoping to snapshot directories means a bug or misuse can only affect snapshots, not your
live data. Send and receive need broad paths because source subvolumes and external drive
mount points vary, but these operations are non-destructive (send is read-only; receive
creates new subvolumes).

**How many lines do you need?** One snapshot-creation line per source → snapshot-root
pair in your config. One deletion line per snapshot directory (each local snapshot root
plus each external drive's snapshot directory). The read-only commands and send/receive
are one line each.

> **Path note:** run `which btrfs` and use that exact path both here and in the
> config's btrfs path. Current Fedora resolves `/usr/bin/btrfs` (`/usr/sbin` is a
> symlink to `bin`; a sudoers line written against either spelling matches an
> invocation via the other — verified on Fedora 44, sudo 1.9.17). Older
> Fedora/RHEL report `/usr/sbin/btrfs`; Arch and Ubuntu use `/usr/bin/btrfs`.

## Initial Setup

After the first install, configure and validate. Run the `cp` steps from the root
of your clone (wherever you cloned the repo — the paths below are repo-relative):

```bash
# 1. Create config from template (~/.config/urd/ does not exist on a fresh
#    system — create it first)
mkdir -p ~/.config/urd
cp config/urd.toml.example ~/.config/urd/urd.toml
# Edit: set drive mount paths, subvolume sources, snapshot roots for your system

# 2. Validate system readiness
urd init

# 3. Preview the backup plan
urd plan

# 4. Dry-run to confirm
urd backup --dry-run

# 5. Create the first local snapshot, then measure it (calibrate reads local
#    snapshots — on a fresh system it skips with "no local snapshots" until
#    one exists)
urd backup --local-only
urd calibrate

# 6. Run a real backup (external sends now have size estimates)
urd backup

# 7. Install the systemd units for nightly backups and the sentinel
mkdir -p ~/.config/systemd/user
cp systemd/urd-*.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now urd-backup.timer
systemctl --user enable --now urd-sentinel.service   # recommended; see below
```

The sentinel is the continuous protection layer — without it, Urd runs only at
04:00 and promise states drift between runs. Skip the sentinel enable line if you
want the nightly cron without the always-on watchdog.

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

### Units

Urd ships three systemd user units. The nightly pair is required; the sentinel
is recommended.

| Unit | Purpose |
|------|---------|
| `urd-backup.timer` | Triggers the service nightly at 04:00 (with 5m random delay) |
| `urd-backup.service` | Runs `~/.cargo/bin/urd backup` as a oneshot |
| `urd-sentinel.service` | Long-running daemon: drive events, promise refresh, notifications |

All three run at low priority (`Nice=19`, `IOSchedulingClass=idle`). The backup
service allows up to 6 hours for large sends. The sentinel restarts on failure
with a 30-second back-off.

The sentinel is the integration layer — drive plug/unplug detection, sub-hourly
promise-state updates, and notification dispatch all live there. Without it,
Urd is a nightly cron job; with it, Urd is a continuous protection layer.

### Install / update units

From the root of your clone:

```bash
mkdir -p ~/.config/systemd/user
cp systemd/urd-*.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now urd-backup.timer
systemctl --user enable --now urd-sentinel.service   # optional but recommended
```

Re-run this after modifying unit files in the repo. Copying (rather than
symlinking) keeps the installed units stable across `git checkout` operations.

### Monitor

```bash
# Backup timer
systemctl --user status urd-backup.timer      # next scheduled run
systemctl --user list-timers urd-backup*      # timer details
journalctl --user -u urd-backup.service       # all run output
journalctl --user -u urd-backup.service -f    # follow live
journalctl --user -u urd-backup.service --since today

# Sentinel daemon
systemctl --user status urd-sentinel.service  # daemon health, restart count
journalctl --user -u urd-sentinel.service -f  # follow live (warn-level lifecycle by default)
urd sentinel status                           # Urd's own view of the daemon
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

From the root of your clone:

```bash
git pull
cargo build --release
cp target/release/urd ~/.cargo/bin/urd
urd --version                     # verify
urd plan                          # preview with new logic
```

If systemd units changed too:

```bash
cp systemd/urd-*.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user restart urd-sentinel.service   # if you run the sentinel
```

(The backup unit is a oneshot driven by the timer — no restart needed; the next
scheduled run picks up the new unit. The sentinel is long-lived, so a restart is
required for unit changes to take effect.)

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
- **Sudo permission** — `urd init` validates this; check `/etc/sudoers.d/urd`

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
