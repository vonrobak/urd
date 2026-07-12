# Urd

**BTRFS Time Machine for Linux**

Urd automates BTRFS snapshot management, incremental sends to external drives, and
graduated retention — so your data is safe without you thinking about it.

Named after [Urðr](https://en.wikipedia.org/wiki/Ur%C3%B0r), the Norse norn who tends
the Well of Fate and knows all that has passed.

> **Status:** Urd runs in production today as the author's sole backup system, on a
> core that is extensively tested (2,224 tests). It is field-validated on Fedora;
> other Linux distributions should work but haven't been proven yet — if your system
> differs from what Urd expects, `urd doctor` tells you what it found and what it needs.
> Pre-1.0: the commands may still change.

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

EXPOSURE  PROTECTION  SUBVOLUME    LOCAL  external-1  external-2
sealed    fortified   documents    15     3           2
sealed    fortified   photos       15     3           2
sealed    fortified   recordings   15     3           2
sealed    fortified   music        15     3           —
sealed    fortified   home         15     3           —
sealed    recorded    multimedia   4      —           —
sealed    recorded    scratch      4      —           —
sealed    recorded    containers   4      —           —

Drives: external-1 (4.4 TB free), external-2 (1.1 TB free)
```

Protection levels you set once; Urd derives the retention and send schedule from them.

## Install

**You need:**

- Linux with a BTRFS filesystem
- `btrfs-progs`
- systemd — optional, for the nightly timer and the Sentinel monitoring daemon

**Prebuilt binary** (x86_64 Linux, statically linked — no toolchain needed):

```bash
curl -LO https://github.com/vonrobak/urd/releases/latest/download/urd-x86_64-linux
curl -LO https://github.com/vonrobak/urd/releases/latest/download/SHA256SUMS

# Verify it is the binary the author published, then put it on your PATH
sha256sum --ignore-missing -c SHA256SUMS
install -Dm755 urd-x86_64-linux ~/.local/bin/urd
```

If `urd: command not found` follows, `~/.local/bin` isn't on your `PATH` — add it and
open a new shell. Want proof of who built the binary, not just that it arrived intact?
Every release carries a [build provenance
attestation](https://github.com/vonrobak/urd/attestations): `gh attestation verify
urd-x86_64-linux --repo vonrobak/urd`.

**From source** (needs a Rust toolchain):

```bash
git clone https://github.com/vonrobak/urd.git && cd urd
cargo install --path .    # installs to ~/.cargo/bin/urd
```

There is no `curl | bash` installer, and there won't be. You check the sum yourself
and decide when Urd has earned the password it asks for.

## Getting started

Install Urd, run `urd`, and she looks at what you have and proposes how to protect it.
Nothing is written without your approval.

```
$ urd
Urd is not configured yet.
I can look at what this machine has and propose how to protect it.
Nothing is written without your approval; leaving costs nothing.

  1) begin    2) not now    q) leave — nothing is written

I have looked. Here is what this machine holds:

  data — 2.4 TB free of 3.6 TB
    /home  (subvolume @home)

  Drives:
    Elements, 2 TB, usb — an external btrfs drive, a place I could keep a backup

  … a few questions about what matters and where it lives …

The runestone. Read it before you answer:

  Promises:
    home  (/home)  — sheltered: snapshots kept here and sent to the drive
```

You approve the runestone, and only then does she carve it — she never touches your
disks, your `sudo`, or your config until you say the word.

That is the whole first run. For the manual path and the full reference — sudoers,
config schema, systemd units — see the [operating guide](docs/00-foundation/guides/operating-urd.md).

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

Urd writes its config for you during the first run — you approve the plan, it carves the
TOML. To read or hand-edit it afterward, or to configure Urd without the guided first run,
see the [operating guide](docs/00-foundation/guides/operating-urd.md) and
[`config/urd.toml.example`](config/urd.toml.example).

## Architecture

Urd's architecture is documented through ADRs (Architecture Decision Records) in
[`docs/00-foundation/decisions/`](docs/00-foundation/decisions/). Key decisions:

- **[ADR-100](docs/00-foundation/decisions/2026-03-24-ADR-100-planner-executor-separation.md):** Planner/executor separation — pure planning, isolated execution
- **[ADR-106](docs/00-foundation/decisions/2026-03-24-ADR-106-defense-in-depth-data-integrity.md):** Defense-in-depth pin protection
- **[ADR-107](docs/00-foundation/decisions/2026-03-24-ADR-107-fail-open-cleanup-on-failure.md):** Fail-open backups, fail-closed deletions

## License

[GPL-3.0](LICENSE)
