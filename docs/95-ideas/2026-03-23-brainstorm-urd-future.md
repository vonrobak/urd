# Brainstorm: Urd Future Directions

> **TL;DR:** Wide-ranging brainstorm for evolving Urd from a single-user homelab tool into
> a flexible, general-purpose BTRFS backup system for Linux. Ideas span drive intelligence,
> config flexibility, restore workflows, network sends, multi-machine support, and beyond.
> None of these are committed — this is creative exploration.

**Date:** 2026-03-23
**Status:** raw

---

## Theme 1: Smart Drive Discovery & Interaction

### 1.1 — Sentinel: Auto-detect mounted btrfs drives

When an external btrfs drive is mounted (udev event + mount detection), Urd wakes up and
asks the user what to do with it. This is the "Time Machine moment" — plug in a drive and
the system knows what to do.

**Interactive flow on first detection:**

```
Urd: New btrfs drive detected: "Seagate-4TB" at /run/media/<user>/Seagate-4TB
     UUID: 1a2b3c4d-...
     Filesystem: btrfs, 3.7 TB free

     What should Urd do with this drive?

     [1] Use as backup target (configure which subvolumes to send here)
     [2] Ignore this drive permanently
     [3] Ignore this time (ask again next plug-in)
     [4] Open interactive configuration
```

If the user picks [1], Urd walks them through selecting which subvolumes should target
this drive, retention policies, and space thresholds — then writes the config and
immediately starts a backup.

On subsequent plug-ins, Urd recognizes the drive by UUID and automatically starts
the configured backup. Desktop notification: "WD-18TB connected — starting backup
(7 subvolumes, last backup: 2 days ago)."

### 1.2 — Drive fingerprinting by UUID

Today drives are identified by mount path label. This is fragile — a drive with a
different label mounted at the same path would be treated as the expected drive.

Use btrfs filesystem UUID as the canonical drive identifier. The label becomes a
human-friendly alias. On mount, verify UUID matches config. If a new UUID appears at
a known mount path, warn rather than silently sending data to the wrong drive.

```toml
[[drives]]
label = "WD-18TB"          # Human-friendly name
uuid = "1a2b3c4d-..."      # Canonical identifier
mount_path = "/run/media/<user>/WD-18TB"  # Expected mount point
```

### 1.3 — Drive trust levels

Not all drives are equal. A drive kept at home is different from one stored in a
bank safety deposit box visited monthly.

```toml
[[drives]]
label = "offsite-vault"
trust = "offsite"           # offsite | onsite | portable
visit_interval = "30d"      # expected reconnection frequency
encryption_required = true  # refuse to send to unencrypted drives
```

Urd could then:
- Warn if an offsite drive hasn't been connected in longer than `visit_interval`
- Prioritize which data goes where (irreplaceable data → offsite first)
- Show "days since last offsite backup" in status
- Adjust retention to be more conservative on rarely-connected drives

### 1.4 — Drive health monitoring

When a drive is connected, run lightweight health checks:
- SMART status via `smartctl`
- btrfs device stats (read/write/corruption errors)
- Free space trend (is it filling up faster than retention can clean?)

Surface in `urd status` and trigger notifications when drives show early
warning signs.

---

## Theme 2: Per-Subvolume Drive Targeting

### 2.1 — Subvolume → drive mapping

Today, every subvolume sends to every mounted drive. This doesn't scale. A 3TB music
collection shouldn't go to a 2TB drive. Irreplaceable recordings should go to *all*
drives. Temporary caches shouldn't go to offsite drives.

```toml
[[subvolumes]]
name = "subvol3-opptak"
drives = ["WD-18TB", "WD-18TB1", "offsite-vault"]  # explicit targeting

[[subvolumes]]
name = "subvol6-tmp"
drives = []                # local snapshots only, no external sends

[[subvolumes]]
name = "htpc-home"
drives = ["*"]             # send to all configured drives (default)
```

The planner filters sends by checking `subvol.drives` against mounted drives.
Backward compatible: omitting `drives` means "all drives" (current behavior).

### 2.2 — Per-subvolume per-drive retention

Different drives holding the same subvolume might need different retention. The
primary onsite drive can keep 6 months of dailies; the offsite drive visited monthly
needs only monthlies for a year.

```toml
[[subvolumes]]
name = "subvol3-opptak"

[subvolumes.drive_overrides.offsite-vault]
external_retention = { monthly = 24 }
send_interval = "30d"           # only send when connected (~monthly)
```

### 2.3 — Drive groups / roles with policies

Rather than per-drive per-subvolume, define roles with policies:

```toml
[drive_roles.primary]
external_retention = { daily = 30, weekly = 26, monthly = 12 }

[drive_roles.offsite]
external_retention = { monthly = 24 }
send_interval = "30d"

[[drives]]
label = "WD-18TB"
role = "primary"             # inherits primary retention

[[drives]]
label = "offsite-vault"
role = "offsite"             # inherits offsite retention
```

Subvolumes could target roles instead of specific drives:

```toml
[[subvolumes]]
name = "subvol3-opptak"
target_roles = ["primary", "offsite"]   # send to any drive with these roles
```

---

## Theme 3: Interactive Configuration & Guided Setup

### 3.1 — `urd setup` wizard

A guided interactive setup for new users, replacing the "copy example config and
edit" workflow:

```
$ urd setup

Welcome to Urd — BTRFS Time Machine for Linux.

Scanning for btrfs subvolumes...
  Found 9 subvolumes across 2 filesystems:
    /home                     (btrfs-nvme, 77 GB used)
    /                         (btrfs-nvme, 15 GB used)
    /mnt/btrfs-pool/subvol1-docs    (btrfs-pool, 450 GB used)
    ...

Which subvolumes should Urd back up? (space to toggle, enter to confirm)
  [x] /home
  [x] /mnt/btrfs-pool/subvol1-docs
  [ ] /mnt/btrfs-pool/subvol6-tmp
  ...

Scanning for external btrfs drives...
  WD-18TB at /run/media/<user>/WD-18TB (16.2 TB free)
  2TB-backup at /run/media/<user>/2TB-backup (1.8 TB free)

Which drives should receive backups? (space to toggle)
  [x] WD-18TB
  [x] 2TB-backup

Writing config to ~/.config/urd/urd.toml...
Running initial calibration...
Ready! Run `urd plan` to preview or `urd backup` to start.
```

### 3.2 — `urd config` subcommand for live config editing

Instead of editing TOML by hand:

```bash
urd config add-drive             # interactive: detect, name, configure
urd config add-subvolume         # interactive: pick source, name, priority
urd config set htpc-home snapshot_interval 15m
urd config show                  # pretty-print current config
urd config validate              # check for issues without running init
```

### 3.3 — Auto-discovery of btrfs subvolumes

Scan the system for btrfs subvolumes and suggest which ones to back up:

```bash
urd discover
```

Lists all btrfs subvolumes with size, mount point, and whether they're already
configured. Suggests sensible defaults based on common patterns (home dirs = high
priority, root = low priority, docker volumes = medium, tmp = skip).

---

## Theme 4: Restore Workflows

### 4.1 — `urd restore` command

The missing half of any backup system. Today, restoring from Urd requires manual
`btrfs send | receive` or file copying. A restore command makes Urd complete:

```bash
urd restore --subvolume htpc-home --snapshot 20260322-1430-htpc-home --target /tmp/restore
urd restore --subvolume htpc-home --latest --target /tmp/restore
urd restore --file ~/documents/thesis.md --snapshot 20260322-1430-htpc-home
```

Options:
- Restore entire snapshot to a target directory (btrfs send|receive or cp -a)
- Restore a single file or directory from a snapshot
- Browse snapshots interactively (pick date, preview contents, restore)
- Restore from external drive (when local snapshots are gone)

### 4.2 — Time-travel file browser

An interactive TUI (using `ratatui`) that lets you browse snapshots like a timeline:

```
┌─ Urd Time Travel: /home/documents/thesis.md ──────────────┐
│                                                             │
│  ← 2026-03-22 14:30    [today]    2026-03-23 15:00 →      │
│                                                             │
│  Snapshot: 20260322-1430-htpc-home                         │
│  Size: 2.3 MB  (current: 2.1 MB — this version is larger) │
│                                                             │
│  [R] Restore this version  [D] Diff with current           │
│  [←→] Navigate snapshots   [Q] Quit                        │
└─────────────────────────────────────────────────────────────┘
```

Could diff files between snapshots, show which snapshots changed a given file, and
restore individual files to a target location.

### 4.3 — Snapshot mounting helper

Make read-only snapshots easily browsable:

```bash
urd mount htpc-home 20260322-1430-htpc-home /tmp/timemachine
# Mounts the snapshot read-only at /tmp/timemachine
# User browses, copies what they need
urd unmount /tmp/timemachine
```

For btrfs snapshots this is essentially free — they're already subvolumes that can be
accessed directly. But providing a clean UX around it matters.

---

## Theme 5: Network & Multi-Machine

### 5.1 — `btrfs send | ssh | btrfs receive` for remote targets

Send snapshots to a remote machine over SSH instead of (or in addition to) local
external drives:

```toml
[[targets]]
type = "ssh"
label = "nas"
host = "nas.local"
user = "backup"
dest_path = "/mnt/backup-pool/.snapshots"
ssh_key = "~/.ssh/id_backup"
bandwidth_limit = "100M"     # throttle to avoid saturating the link
```

This enables:
- NAS backup without mounting NFS/SMB
- Off-site backup to a VPS or friend's machine
- Multi-machine fleet backup to a central server

The planner treats SSH targets like drives — same send/receive semantics, same
retention, same chain tracking. The executor pipes through SSH instead of local
processes.

### 5.2 — Pull mode (server pulls from clients)

Inverse of 5.1: a central server runs `urd pull` to fetch snapshots from multiple
machines:

```toml
[[sources]]
label = "laptop"
host = "laptop.local"
subvolumes = ["home", "root"]
schedule = "1h"
```

This is how enterprise backup works — the server is in control, not the clients.
Useful for backing up family members' machines to a home NAS.

### 5.3 — Urd mesh: peer-to-peer backup

Two Urd instances back each other up. Machine A sends its snapshots to Machine B,
and Machine B sends its snapshots to Machine A. Each machine is both a source and
a target.

```bash
urd peer add laptop.local --mutual   # both directions
```

No central server needed. Works over LAN or WAN with SSH. Each machine maintains
its own config, retention, and chain tracking. The peer relationship is symmetric
by default but can be one-directional.

---

## Theme 6: Observability & Intelligence

### 6.1 — Growth rate tracking and prediction

Track subvolume growth over time and predict when drives will fill up:

```
$ urd status --forecast

SUBVOLUME          SIZE     GROWTH/DAY  WD-18TB FULL IN  WD-18TB1 FULL IN
htpc-home          77 GB    +120 MB     372 days         289 days
subvol3-opptak     2.8 TB   +2.1 GB     43 days (!)      67 days
subvol5-music      1.1 TB   +50 MB      never            never
```

Alert when a drive is predicted to fill up within a configurable threshold (e.g.,
30 days). This turns "surprise disk full at 3am" into "you have 6 weeks to buy a
bigger drive."

### 6.2 — Backup health score

A single number (0-100) representing overall backup health:

```
$ urd health

Urd Backup Health: 87/100

  ✓ All chains incremental (10 pts)
  ✓ Offsite backup < 7 days old (15 pts)
  ✗ subvol4-multimedia not backed up externally (-10 pts)
  ✓ No drive space warnings (10 pts)
  ⚠ 2TB-backup not connected in 45 days (-3 pts)
  ...
```

Expose this as a Prometheus metric. Set up Grafana alerts when health drops below
a threshold.

### 6.3 — Notification system

Desktop and remote notifications for backup events:

```toml
[notifications]
desktop = true                    # notify-send for start/complete/failure
discord_webhook = "https://..."   # critical failures only
email = "backup-alerts@..."       # weekly summary digest

[notifications.triggers]
backup_complete = "desktop"
backup_failure = "desktop, discord"
drive_connected = "desktop"
drive_health_warning = "desktop, discord, email"
offsite_overdue = "discord"
weekly_summary = "email"
```

### 6.4 — `urd dashboard` — built-in terminal dashboard

A real-time TUI dashboard showing:
- Live backup progress (current operation, bytes, rate)
- Subvolume status grid (local/external counts, chain health)
- Drive status (mounted, free space, last backup)
- Recent history (last 5 runs with results)
- Growth trends (sparkline graphs)

Uses `ratatui` for rendering. Stays open and updates on backup events. The "htop
for backups."

---

## Theme 7: Generalization for Other Users

### 7.1 — Zero-config mode

For users who just want "back up my home to this drive," Urd should work with
zero TOML editing:

```bash
urd backup /home --to /run/media/<user>/MyDrive
```

Auto-detects the source is a btrfs subvolume, creates a snapshot, sends to the drive,
handles retention with sensible defaults. No config file needed for simple cases.
Power users can `urd init` to create a config file for customization.

### 7.2 — Packaging for distributions

- **AUR package** (Arch Linux) — where btrfs-on-root is most common
- **Fedora COPR** — Fedora defaults to btrfs since F33
- **openSUSE OBS** — btrfs is their default filesystem
- **Nix flake** — reproducible builds for NixOS users
- **Flatpak/AppImage** — for universal distribution (though sudo complicates this)
- **Snap** — with appropriate confinement

The binary has zero runtime deps (rusqlite is bundled), so packaging is mainly
about writing the package metadata and setting up sudoers.

### 7.3 — Sudoers generator

The sudo requirement is a barrier for new users. Urd could generate the exact
sudoers entries needed:

```bash
urd sudoers
# Outputs:
# <user> ALL=(root) NOPASSWD: /usr/sbin/btrfs subvolume snapshot -r /home ...
# <user> ALL=(root) NOPASSWD: /usr/sbin/btrfs send /home/.snapshots/*
# ...
# Install with: urd sudoers | sudo tee /etc/sudoers.d/urd
```

Generated from the current config — only the exact commands needed, no more.

### 7.4 — Polkit integration as sudo alternative

Instead of sudoers, use polkit for btrfs authorization. This is more
"Linux desktop native" and doesn't require manual sudoers editing:

```xml
<!-- /usr/share/polkit-1/actions/com.urd.backup.policy -->
<action id="com.urd.backup.btrfs">
  <description>Run btrfs backup operations</description>
  <defaults>
    <allow_active>auth_admin_keep</allow_active>
  </defaults>
</action>
```

Could also support running as root directly via systemd system units (not user
units), with appropriate privilege dropping for non-btrfs operations.

---

## Theme 8: Advanced Btrfs Features

### 8.1 — Compression-aware size estimation

Use `compsize` to get the actual compressed size of data, improving space estimation
accuracy over `du -sb`:

```bash
compsize /path/to/snapshot
# Type  Perc  Disk Usage  Uncompressed  Referenced
# TOTAL  62%   48G          77G          77G
```

This would give much more accurate estimates for compressed filesystems and could
replace the 1.2x safety margin with a measured compression ratio.

### 8.2 — Reflink-aware deduplication analysis

Analyze how much data is shared via reflinks between snapshots. This matters for
space estimation (shared data is sent once in the stream) and for understanding
the true disk usage of snapshot sets.

```bash
urd analyze --subvolume htpc-home
# 47 snapshots, 77 GB unique data, 2.3 TB total referenced
# Dedup ratio: 30:1 (snapshots share most data)
# Estimated full send: 77 GB
# Estimated incremental: 120 MB (avg daily delta)
```

### 8.3 — Qgroup integration (optional)

For users who enable btrfs quotas, read qgroup data for instant, accurate size
information instead of running `du -sb`:

```bash
urd calibrate --method qgroup    # instant, accurate
urd calibrate --method du        # slow, approximate (default)
```

Auto-detect whether quotas are enabled and prefer qgroups when available.

### 8.4 — Scrub scheduling

Integrate with `btrfs scrub` to schedule and track filesystem health checks:

```bash
urd scrub /mnt/btrfs-pool        # start a scrub
urd scrub --status                # check progress
```

Surface scrub results and errors in `urd health`. A filesystem with uncorrectable
errors should generate urgent notifications.

---

## Theme 9: Encryption & Security

### 9.1 — Encrypted sends

Encrypt the btrfs send stream before writing to external drives. Useful for offsite
drives that might be lost or stolen:

```toml
[[drives]]
label = "offsite-vault"
encryption = "age"                # or "gpg"
encryption_recipient = "age1..."
```

The send stream is piped through `age --encrypt` before `btrfs receive`. On restore,
`age --decrypt` is applied first. This means the external drive has encrypted blobs,
not browsable snapshots — a trade-off for security.

### 9.2 — LUKS detection and unlock integration

Detect when a LUKS-encrypted drive is connected, prompt for passphrase (or use
keyring), unlock, mount, then proceed with backup:

```
Urd: Encrypted drive detected: WD-18TB (LUKS2)
     Unlocking... [passphrase or keyring]
     Mounting at /run/media/<user>/WD-18TB
     Starting backup...
```

### 9.3 — Audit trail

Cryptographically signed audit log of all backup operations, for compliance or
personal peace of mind:

```bash
urd audit --verify
# All 1,234 operations verified. Chain is intact.
# First entry: 2026-03-23T02:00:00
# Last entry:  2026-07-15T02:00:00
```

Each operation record in SQLite gets a hash chain (like a mini blockchain). Any
tampering with the history is detectable.

---

## Theme 10: Wild Ideas

### 10.1 — Backup-as-a-filesystem (FUSE)

Mount the backup history as a virtual filesystem:

```bash
urd fuse /tmp/timeline
ls /tmp/timeline/htpc-home/
# 2026-03-20/  2026-03-21/  2026-03-22/  2026-03-23/
ls /tmp/timeline/htpc-home/2026-03-22/documents/
# thesis.md  notes.txt  ...
```

Each date directory is a read-only view of that snapshot. Browse history like
navigating folders. Copy files out with standard tools.

### 10.2 — Urd Cloud — encrypted backup to S3/B2

Send encrypted btrfs streams to cloud object storage for truly off-site backup:

```toml
[[targets]]
type = "s3"
bucket = "my-urd-backups"
prefix = "htpc/"
encryption = "age"
storage_class = "GLACIER_IR"     # cold storage for cost
```

Use btrfs send's incremental nature to minimize transfer size. Store each
incremental as a separate object. Track chain state in S3 metadata.

### 10.3 — Snapshot diffing and change analysis

Show what changed between any two snapshots:

```bash
urd diff htpc-home 20260322-1430 20260323-1430
# Modified: 142 files (+ 23 new, - 5 deleted)
# Largest changes:
#   ~/projects/urd/target/  +450 MB (build artifacts)
#   ~/documents/thesis.md   +12 KB
#   ~/.cache/               +89 MB
```

Could integrate with the planner to explain *why* incremental sends are a certain
size. Also useful for understanding what's eating disk space.

### 10.4 — Configuration profiles

Pre-built configs for common use cases:

```bash
urd init --profile laptop        # /home only, daily external sends
urd init --profile workstation   # /home + /root, hourly snapshots
urd init --profile server        # multiple data volumes, conservative retention
urd init --profile photographer  # large media, external drives, offsite
urd init --profile nas           # pull mode, multiple sources
```

Each profile sets sensible defaults for subvolumes, intervals, retention, and drives
based on the use case. Users can customize after initial setup.

### 10.5 — Urd API / library mode

Extract the core logic into a library crate that other tools can use:

```rust
use urd::{Config, plan, execute};

let config = Config::load(None)?;
let plan = urd::plan(&config, now, &filters, &fs_state)?;
let result = urd::execute(&plan, &btrfs, &state)?;
```

This enables:
- GUI frontends (GTK, Qt, web)
- Integration with desktop environments (GNOME/KDE backup settings)
- Custom automation scripts
- Testing harnesses for other btrfs tools

### 10.6 — Multi-user / system-wide mode

Today Urd runs as a single user. For shared systems (family NAS, small office):

```bash
sudo urd backup --system          # backs up all configured users
```

Each user has their own `~/.config/urd/urd.toml`. The system-wide service
iterates users and runs their configs. Or a single system config manages
all subvolumes centrally.

### 10.7 — Disaster recovery playbook

`urd disaster-recovery` generates a step-by-step recovery guide specific to
your configuration:

```
Urd Disaster Recovery Guide (generated 2026-03-23)

If your system disk fails:
  1. Boot from live USB
  2. Mount WD-18TB at /mnt/recovery
  3. Run: btrfs send /mnt/recovery/.snapshots/htpc-home/20260323-1430-htpc-home | btrfs receive /mnt/newdisk/
  4. ...

Your most recent backups:
  htpc-home: 20260323-1430 (2 hours ago) on WD-18TB, WD-18TB1
  subvol3-opptak: 20260323-1200 (5 hours ago) on WD-18TB
  ...
```

Print it out and keep it with your offsite drives. When disaster strikes, you
don't want to be reading man pages.

---

## Priority Matrix (gut feel, not analysis)

| Idea | User Value | Effort | Makes Urd General-Purpose? |
|------|-----------|--------|---------------------------|
| 1.1 Sentinel auto-detect | Very high | Medium | Yes — the "it just works" moment |
| 1.2 UUID fingerprinting | High | Low | Yes — prevents wrong-drive accidents |
| 2.1 Subvolume→drive mapping | Very high | Low-Medium | Yes — essential for flexibility |
| 3.1 Setup wizard | Very high | Medium | Yes — removes config barrier |
| 3.3 Auto-discover subvolumes | High | Low | Yes — zero-knowledge onboarding |
| 4.1 Restore command | Very high | Medium | Yes — completes the backup story |
| 5.1 SSH remote targets | Very high | Medium-High | Yes — the killer feature |
| 6.1 Growth prediction | High | Low | Nice to have |
| 6.3 Notifications | High | Medium | Yes — essential for unattended |
| 7.1 Zero-config mode | Very high | Medium | Yes — the ultimate onboarding |
| 7.2 Distribution packaging | High | Low per distro | Yes — reach |
| 7.3 Sudoers generator | High | Low | Yes — removes friction |
| 10.1 FUSE filesystem | Very high | High | Wow factor |
| 10.5 Library mode | High | Medium | Enables ecosystem |
