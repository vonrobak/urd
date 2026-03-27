# Urd

**BTRFS Time Machine for Linux**

Urd automates BTRFS snapshots, incremental sends to external drives, and graduated
retention — so your data is safe without you thinking about it.

Named after [Urðr](https://en.wikipedia.org/wiki/Ur%C3%B0r), the Norse norn who tends
the Well of Fate and knows all that has passed.

> **Status:** Under active development. Running in production as the sole backup system on
> the author's machine since March 2026. 389 tests. The core backup pipeline is stable;
> the Sentinel daemon (passive monitoring) just shipped. Active mode and config redesign
> are next.

## Why Urd?

BTRFS gives you instant, free snapshots and incremental sends. The missing piece is
automation that's smart enough to manage the lifecycle: when to snapshot, what to send,
what to keep, and what to prune — without losing data.

Urd fills that gap:

- **Incremental-first.** Always attempts `btrfs send -p` with a tracked parent. Full sends
  only when the chain is genuinely broken.
- **Graduated retention.** Time Machine-style thinning — recent snapshots kept densely,
  older ones progressively pruned. Pinned chain parents are never deleted.
- **Drive-aware.** Independent incremental chains per external drive. Plug in a drive,
  run a backup, rotate offsite. Urd tracks each drive's history separately.
- **Space-aware.** Pre-send space estimation prevents multi-hour sends from failing at 99%.
  Local snapshot guard prevents filling your NVMe.
- **Plan before execute.** `urd plan` shows exactly what would happen. `urd backup --dry-run`
  does everything except touch the filesystem.
- **Promise-based.** Assign protection levels to subvolumes. Urd derives retention, intervals,
  and drive requirements — then tells you whether promises are being kept.
- **Observable.** Prometheus metrics, SQLite history, structured JSON output, heartbeat file
  for external monitoring.

## Quick look

```
$ urd status

PROMISE    SUBVOLUME           STATUS      LOCAL  WD-18TB1  2TB-backup
resilient  subvol1-docs        PROTECTED   15     3         2
resilient  subvol2-pics        PROTECTED   15     3         2
resilient  subvol3-opptak      PROTECTED   15     3         2
protected  subvol5-music       PROTECTED   15     3         —
protected  htpc-home           PROTECTED   15     3         —
protected  htpc-root           PROTECTED   15     3         —
guarded    subvol4-multimedia  PROTECTED   4      —         —
guarded    subvol6-tmp         PROTECTED   4      —         —
guarded    subvol7-containers  PROTECTED   4      —         —

Drives: WD-18TB1 (4.4 TB free), 2TB-backup (1.1 TB free)
```

```
$ urd sentinel status

SENTINEL — watching
  Uptime     7h 23m (PID 500258)
  Last check 4m ago, next in 11m
  Drives     WD-18TB1, 2TB-backup
  Promises   9 PROTECTED
```

## Commands

| Command | What it does |
|---------|-------------|
| `urd plan` | Preview planned operations without executing |
| `urd backup` | Snapshot, send, and prune — the full pipeline |
| `urd status` | Promise states, snapshot counts, drive health |
| `urd get FILE --at DATE` | Restore a file from a past snapshot |
| `urd sentinel run` | Start the passive monitoring daemon |
| `urd sentinel status` | Check if the Sentinel is running and what it sees |
| `urd history` | Browse backup history from SQLite |
| `urd verify` | Check incremental chain integrity and pin health |
| `urd calibrate` | Measure snapshot sizes for space estimation |
| `urd init` | Initialize state database and validate readiness |

## How it works

```
config  →  plan (pure)  →  execute (I/O)  →  record (SQLite)
                                 |
                            btrfs (sudo)
```

1. **Plan.** Reads config and filesystem state. Determines which subvolumes need snapshots,
   which need sends (incremental or full), and which old snapshots to prune. Pure function —
   no side effects.
2. **Execute.** Runs the plan: create read-only snapshots, pipe `btrfs send | btrfs receive`
   to external drives, clean up retention. Individual subvolume failures never abort the run.
3. **Record.** Results stored in SQLite. Promise states assessed. Heartbeat written.
4. **Watch.** The Sentinel daemon (optional) polls for drive changes and heartbeat updates,
   reassesses promises, and notifies when something needs attention.

### Retention

Local snapshots use graduated retention:
- Last 14 days: keep all
- Weeks 3–8: keep one per ISO week
- Months 3–5: keep one per month

External snapshots use count-based retention. Pinned parents (incremental chain anchors)
are **never** deleted, regardless of age or policy.

### Safety

- Backups fail open; deletions fail closed
- Three independent layers protect pinned snapshots from accidental deletion
- UUID fingerprinting detects drive swaps (won't blindly send to a relabeled drive)
- Failed sends clean up partial snapshots at the destination
- SQLite failures never prevent backups

## Configuration

```toml
# ~/.config/urd/urd.toml

[general]
snapshot_root = "/mnt/btrfs-pool/.snapshots"
run_frequency = "daily"

[[subvolumes]]
name = "subvol1-docs"
short_name = "docs"
source = "/mnt/btrfs-pool/subvol1-docs"
protection_level = "resilient"     # Urd derives retention + intervals
drives = ["WD-18TB1", "2TB-backup"]

[[subvolumes]]
name = "subvol4-multimedia"
short_name = "multimedia"
source = "/mnt/btrfs-pool/subvol4-multimedia"
protection_level = "guarded"       # Local snapshots only

[[drives]]
label = "WD-18TB1"
mount_path = "/run/media/user/WD-18TB1"
uuid = "abcd-1234"
```

See [`config/urd.toml.example`](config/urd.toml.example) for a complete reference.

## Requirements

- Linux with BTRFS
- `btrfs-progs`
- Sudoers entries for `btrfs` subcommands (Urd runs as a regular user, calls `sudo btrfs`)
- systemd (for timer/service integration)

## Building

```bash
cargo build --release
# Binary at target/release/urd

cargo test              # 389 unit tests
cargo clippy -- -D warnings
```

## Project status

Urd is a personal project built for real use. It runs nightly via systemd timer and the
Sentinel daemon monitors between runs. The architecture is designed carefully — pure-function
core, defense-in-depth safety, adversary-reviewed at every stage — but it's built for one
machine so far.

If you're interested in BTRFS automation, feel free to explore. Contributions and
conversations welcome.

## License

MIT
