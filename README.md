# Urd

**BTRFS Time Machine for Linux**

Urd automates BTRFS snapshot management, incremental sends to external drives, and
graduated retention — so your data is safe without you thinking about it.

Named after [Urðr](https://en.wikipedia.org/wiki/Ur%C3%B0r), the Norse norn who tends
the Well of Fate and knows all that has passed.

> **Status:** Early development — experimental. The core backup pipeline is extensively
> tested (959 tests) and running in production as the sole backup system for the author.
> In future releases, Urd will be made more flexible and accessible for users with
> different setups.

## Why Urd?

BTRFS snapshots are fast and space-efficient, and incremental sends make off-site backups
practical. What's missing is automation that manages the full lifecycle: when to snapshot,
what to send, what to keep, and what to prune — without risking data loss.

Urd fills that gap:

- **Incremental sends.** Always attempts `btrfs send -p` with a tracked parent snapshot.
  Falls back to a full send only when the chain is genuinely broken.
- **Graduated retention.** Time Machine-style thinning — recent snapshots kept densely,
  older ones progressively pruned. Chain-critical snapshots (pinned parents) are never
  deleted, regardless of retention policy.
- **Drive-aware.** Independent incremental chains per external drive. Plug in a drive,
  run a backup, rotate offsite. Urd tracks each drive's send history separately.
- **Space-aware.** Pre-send size estimation prevents multi-hour transfers from failing
  at 99% due to insufficient space on the target drive.
- **Plan before execute.** `urd plan` shows exactly what would happen. `urd backup --dry-run`
  runs the full pipeline without touching the filesystem.
- **Promise-based monitoring.** Assign protection levels to subvolumes. Urd derives
  retention schedules, send intervals, and drive requirements — then tells you whether
  those promises are being kept.

## Quick look

```
$ urd status

EXPOSURE  SUBVOLUME      LOCAL  external-1  external-2
sealed    documents      15     3           2
sealed    photos         15     3           2
sealed    recordings     15     3           2
sealed    music          15     3           —
sealed    home           15     3           —
sealed    multimedia     4      —           —
sealed    scratch        4      —           —
sealed    containers     4      —           —

Drives: external-1 (4.4 TB free), external-2 (1.1 TB free)
```

## Commands

| Command | What it does |
|---------|-------------|
| `urd status` | Promise states, snapshot counts, drive health |
| `urd plan` | Preview planned operations without executing |
| `urd backup` | Snapshot, send, and prune — the full pipeline |
| `urd get FILE --at DATE` | Restore a file from a past snapshot |
| `urd verify` | Check incremental chain integrity and pin health |
| `urd doctor` | Run health diagnostics |
| `urd sentinel run` | Start the passive monitoring daemon |
| `urd drives` | Manage and inspect backup drives |
| `urd history` | Browse backup history |
| `urd calibrate` | Measure snapshot sizes for send estimates |
| `urd emergency` | Guided emergency space recovery |
| `urd init` | Initialize state database and validate readiness |

## How it works

```
config  ->  plan (pure)  ->  execute (I/O)  ->  record (SQLite)
                                  |
                             btrfs (sudo)
```

1. **Plan.** Reads config and filesystem state. Determines which subvolumes need snapshots,
   which need sends (incremental or full), and which old snapshots to prune. The planner is
   a pure function — no side effects.
2. **Execute.** Runs the plan: create read-only snapshots, pipe `btrfs send | btrfs receive`
   to external drives, apply retention policy. Individual subvolume failures never abort the
   run — other subvolumes continue.
3. **Record.** Results stored in SQLite. Promise states reassessed. Heartbeat written for
   external monitoring.
4. **Watch.** The Sentinel daemon (optional) monitors for drive changes and overdue backups,
   reassesses promise states, and surfaces problems before they become emergencies.

### Retention

Local snapshots use graduated retention:
- Last 14 days: keep all daily snapshots
- Weeks 3–8: keep one per ISO week
- Months 3–5: keep one per month

External snapshots use count-based retention. Pinned parent snapshots (incremental chain
anchors) are **never** deleted, regardless of age or policy — this is enforced by three
independent protection layers.

### Safety principles

- **Backups fail open; deletions fail closed.** Proceed on uncertainty, never delete what
  can't be confirmed safe.
- **Three-layer pin protection.** Unsent-parent tracking, planner exclusion, and executor
  re-verification all independently prevent deletion of chain-critical snapshots.
- **UUID fingerprinting.** Detects drive identity by filesystem UUID — won't send to a
  relabeled or swapped drive.
- **Partial send cleanup.** Failed sends automatically remove incomplete snapshots at the
  destination.
- **SQLite is history, not truth.** The filesystem (snapshot directories, pin files) is
  authoritative. Database failures never prevent backups.

## Configuration

```toml
# ~/.config/urd/urd.toml

[general]
snapshot_root = "/mnt/your-filesystem/.snapshots"
run_frequency = "daily"

[[subvolumes]]
name = "documents"
short_name = "docs"
source = "/mnt/your-filesystem/documents"
protection_level = "fortified"
drives = ["external-1", "external-2"]

[[subvolumes]]
name = "multimedia"
short_name = "multimedia"
source = "/mnt/your-filesystem/multimedia"
protection_level = "recorded"

[[drives]]
label = "external-1"
mount_path = "/run/media/you/external-1"
uuid = "abcd-1234"
```

See [`config/urd.toml.example`](config/urd.toml.example) for a complete reference.

## Requirements

- Linux with BTRFS filesystem
- `btrfs-progs` installed
- Scoped sudoers entries for `btrfs` subcommands — see the
  [sudoers template](docs/00-foundation/guides/operating-urd.md#sudoers-configuration)
  (Urd runs as a regular user, invokes `sudo btrfs` for privileged operations)
- systemd (optional, for timer and Sentinel daemon integration)

## Building

```bash
cargo build --release
cargo install --path .    # Install to ~/.cargo/bin/urd

cargo test                # 959 unit tests
cargo clippy -- -D warnings
```

## Architecture

Urd's architecture is documented through ADRs (Architecture Decision Records) in
[`docs/00-foundation/decisions/`](docs/00-foundation/decisions/). Key decisions:

- **[ADR-100](docs/00-foundation/decisions/2026-03-24-ADR-100-planner-executor-separation.md):** Planner/executor separation — pure planning, isolated execution
- **[ADR-106](docs/00-foundation/decisions/2026-03-24-ADR-106-defense-in-depth-data-integrity.md):** Defense-in-depth pin protection
- **[ADR-107](docs/00-foundation/decisions/2026-03-24-ADR-107-fail-open-cleanup-on-failure.md):** Fail-open backups, fail-closed deletions

## License

[GPL-3.0](LICENSE)
