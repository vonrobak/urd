# Urd

**BTRFS Time Machine for Linux**

Urd automates BTRFS snapshot management and incremental backup to external drives. It provides Time Machine-style graduated retention, automatic drive detection, and a clear CLI for operational visibility.

Named after [Urðr](https://en.wikipedia.org/wiki/Ur%C3%B0r), the Norse norn who tends the Well of Fate and knows all that has passed.

## Features

- **Incremental-first:** Always attempts incremental `btrfs send -p`; falls back to full send only when the chain is broken
- **Graduated retention:** Time Machine-style thinning — recent snapshots kept densely, older ones progressively pruned
- **Drive-aware:** Maintains independent incremental chains per external drive, supporting offsite rotation
- **Space-aware:** Pre-flight space checks prevent multi-hour sends from failing at 99%
- **Idempotent:** Safe to run multiple times, safe to interrupt, safe to resume
- **Observable:** Prometheus metrics, structured logging, SQLite history
- **Plan before execute:** `urd plan` shows exactly what would happen before any operation runs

## Quick Start

```bash
# Show what Urd would do
urd plan

# Run backup (dry-run first)
urd backup --dry-run

# Run backup for real
urd backup

# Check system status
urd status

# View backup history
urd history --last 10

# Verify chain integrity
urd verify
```

## Status Output

```
SUBVOLUME          LOCAL  WD-18TB  WD-18TB1  LAST SEND    CHAIN
htpc-home          15     14       12        2h ago       incremental
subvol3-opptak     15     1        0         6h ago       full (new)
subvol7-containers 15     2        1         23h ago      incremental
subvol1-docs       15     2        1         23h ago      incremental
subvol2-pics       4      0        0         5d ago       weekly

Drives: WD-18TB1 mounted (4.4TB free / 17TB)
Next scheduled: subvol2-pics in 5d (Saturday)
```

## Configuration

Urd reads its config from `~/.config/urd/urd.toml`. See `config/urd.toml.example` for a complete reference.

```toml
[[subvolumes]]
name = "subvol3-opptak"
short_name = "opptak"
source = "/mnt/btrfs-pool/subvol3-opptak"
tier = 1
local_schedule = "daily"
external_schedule = "daily"
external_retention = 14

[[drives]]
label = "WD-18TB"
mount_path = "/run/media/<user>/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
```

## How It Works

1. **Plan:** Urd reads config and current snapshot state, determines which subvolumes need snapshots, which need external sends (incremental or full), and which old snapshots to prune
2. **Execute:** Operations run sequentially — create snapshots, send to external drives, clean up retention
3. **Record:** Results are stored in a SQLite database for history and status queries
4. **Export:** Prometheus metrics are written atomically for Grafana dashboards

### Retention Strategy

**Local snapshots** use graduated retention (daily-schedule subvolumes):
- Last 14 days: keep all snapshots
- Days 15-56: keep 1 per ISO week
- Days 57-149: keep 1 per month

**External snapshots** use count-based retention (e.g., keep 14 for Tier 1).

Pinned parents (incremental chain anchors) are **never** deleted by retention, regardless of age.

### Incremental Chains

After a successful `btrfs send | btrfs receive`, Urd pins the sent snapshot as the parent for the next incremental send. Each external drive has an independent pin file, supporting offsite rotation where drives have different snapshot histories.

## Requirements

- Linux with BTRFS filesystem
- `btrfs-progs` installed
- Sudoers entries for passwordless `btrfs` operations (Urd runs as a regular user)
- systemd (for timer/service integration)

## Building

```bash
cargo build --release
# Binary at target/release/urd
```

## License

MIT
