---
type: Review
title: Urd from the ground up — an independent review
categories: ['[[Review]]']
project: ['[[urd]]']
sensitivity: public
status: draft
created: '2026-11-07'
timestamp: '2026-11-07T12:00:00+01:00'
---
# Urd from the ground up — an independent review

> **TL;DR:** Urd works, and the ideas behind it are worth keeping: promises instead of
> operations, a pure planner, filesystem-is-truth, fail-open backups and fail-closed
> deletes, the host wins. The body built around those ideas is not what a first-class
> product would have. It is one host's backup system generalised after the fact, at
> 116 K lines of Rust for a problem whose core fits in about 12–15 K, with a privilege
> model that is sound against Urd's own bugs but not against anyone else running as the
> user, with three config parsers, roughly nine deletion paths, three overlapping
> storage-pressure vocabularies, and with its own development history written into
> its comments. A rebuild should keep the invariants, the retention algorithm, the
> chain logic and the voice, and replace the shape: a library-first workspace, a
> privileged helper with a seven-verb API instead of a sudoers grant, one config
> schema, one deletion path, one pressure model with two numbers, real btrfs in CI,
> and packages. The rest of this document says why and how.

**Audience:** the author, and anyone deciding whether the next Urd is a refactor or a
rewrite. **Method:** I read the architecture doc, all 21 ADRs, the glossary, the
operating guide, the changelog, the entry points and the privilege, btrfs, planner,
config, lock, sentinel and systemd code myself; I delegated bounded inventories
(CLI, config, on-disk contracts, test suite, changelog classes, vocabulary) and two
adversarial reviews (privilege model, core pipeline) and checked their load-bearing
claims against the source. Line references are to this checkout (0.39.0).

## 1. The project as it stands

| Measure | Value |
|---|---|
| Rust in `src/` | 116,592 lines: about 45,000 production, 72,000 test (61 %); 14,725 comment lines |
| Production lines in the core pipeline (plan, executor, retention, pressure, backup driver), comments and tests excluded | about 8,100 |
| `#[test]` functions | 2,724; the suite runs in 2.3 s after a 34 s debug build |
| Dependencies | 17 direct, 86 total; release binary 7.4 MiB with LTO and strip |
| ADRs | 21, several carrying five or six in-file amendments |
| Glossary | 63 KB, fourteen clusters |
| Releases | 68 between 2026-03-22 and 2026-10-01 |
| Changelog entries | 155 Added, 98 Changed, 116 Fixed, 4 Removed |
| CLI subcommands | 16, plus bare `urd` |
| Config schemas parsed | 3 (legacy, v1, v2) into one `Config` |
| SQLite tables | 8 live, no `user_version` pragma |
| External binaries invoked | sudo, btrfs, visudo, systemctl, loginctl, findmnt, lsblk, du, notify-send, curl, `$EDITOR` |
| Provenance markers in source comments | 809 `UPI` references, 238 issue numbers, 335 ADR references |
| Library crate | none; every module is a `mod` in `main.rs` |
| Field validation | the author's host, Fedora only (README) |

The trajectory matters as much as the snapshot. Twenty-six of the 116 fixes are
wording and rendering. Twelve are storage-pressure false trips, and the storage
watchdog was designed in 0.22, windowed in 0.27.0, and partly deleted in 0.27.1 one
month later; its `cleanup_budget` config field was retired in 0.29; the sentinel's
circuit breaker was built in 0.3, never finished, and deleted in 0.37, leaving two
permanently-zero metrics as a contract. The protection-level names were renamed
twice. The sudoers earning shipped in 0.33.0 and took seven fixes in 0.33.x. None of
this is unusual for a solo project moving fast. It is evidence that the design
settled by iteration on one machine rather than by deciding the model first, and
that is the thing a rebuild can change.

## 2. What the project got right

These should survive any rebuild unchanged in substance.

- **Promises, not operations.** "recorded / sheltered / fortified" and
  PROTECTED / AT RISK / UNPROTECTED are the right user vocabulary. The user says
  what matters; the tool derives intervals and retention (ADR-110).
- **The planner is a pure function** of config, time and an observation of the
  world; the executor only acts (ADR-100, ADR-108). This is the single best
  structural decision in the codebase and it is enforced, by a lint and by tests.
- **Filesystem is truth, SQLite is history** (ADR-102). Snapshot directories and
  pin files are authoritative; the database can be lost without losing backups.
- **Fail open on backups, fail closed on deletions** (ADR-107), and three
  independent layers keeping pinned snapshots alive (ADR-106).
- **The host wins** (ADR-113), learned from a real ENOSPC incident and written down
  in a postmortem.
- **Sends are never time-limited** (`TimeoutStartSec=infinity`).
- **Subprocess hygiene**: no shell, `OsStr` arguments, `sudo -n`, `LC_ALL=C`, both
  exit codes of the send/receive pipeline checked, partial receives cleaned up only
  after receive provably exited (`src/btrfs.rs`).
- **The sudoers install sequence**: render, unprivileged `visudo -c`, consent,
  inert dot-named staging file, root-side byte comparison with what the user saw,
  root-side `visudo -c`, atomic `mv`, `sudo -k` before the probe
  (`src/commands/seal.rs:772-1000`). Whatever privilege model the rebuild chooses,
  this is how to install a privileged artifact.
- **Refuse rather than escape** hostile config values in a hostile medium.
- **A release contract** (ADR-117): one static musl binary, checksums, sigstore
  provenance, SHA-pinned actions.
- **The voice.** It is a real product character and it is mostly where it belongs.
- **Writing things down.** ADRs, a glossary, a postmortem with a template, runbooks.
  Few solo projects have any of these.

## 3. Where the foundation bends

### 3.1 It is one host's backup system, generalised after the fact

The config example names an 18 TB drive, a 15-minute `/home` cadence and a
118 GB NVMe appear in the postmortem, the storage-tier thresholds were tuned against
"a 15 TB media pool at 20 % free" (ADR-113, 2026-06-14 amendment), and the README
says field validation is Fedora only. Features arrived as the author's host
demanded them: offsite rotation inference when a drive travelled, pool-scoped
watchdog response when two independent pools interfered, the "mapper zombie"
runbook when LUKS misbehaved. Each is a good solution to a real problem on that
machine. None was designed against a population of hosts, and that shows in what
is missing for anyone else: no exclusions concept at all (the only gate is
`enabled = false`), no uninstall command or document, no packages, a hard-coded
`~/.cargo/bin` in the shipped units, and two config fields (`general.log_dir`,
`drive.max_usage_percent`) that are parsed, validated and migrated but read by
nothing outside test fixtures.

### 3.2 Accidental complexity in the engine

The core job is: take a read-only snapshot on an interval, send it incrementally
to each configured drive using the last pinned parent, prune by a graduated policy
that never touches pins, and stop before the host runs out of space. The code
around that job has grown several concepts for one idea:

- **Storage pressure** is expressed as `HeadroomSeverity`, `TightnessTier`
  (Roomy/Tight/Critical), `StoragePosture`, `RunArming` with an armed-tier map and
  hysteresis, `PoolSignal`/`StorageSignalMap`, an absolute-headroom gate layered on a
  free-ratio classifier, a mid-op watchdog, an idle emergency eject, an emergency
  pre-flight, and tier-graded "retain-one / retain-parents / clear-all" lifecycles.
  ADR-113 reads as an archaeology of six amendments. The invariant it protects
  needs two numbers per pool: a floor below which Urd deletes its own footprint,
  and a margin above the floor below which it stops creating new footprint.
- **Deletion** happens in roughly nine places: graduated retention, transient
  cleanup, Critical clear-all, away-pin shed, emergency reclaim (watchdog abort),
  idle eject, `urd emergency`, the backup's emergency pre-flight, and partial-send
  cleanup, spread over `executor/{ops,lifecycle,reclaim,send}.rs` and
  `commands/{emergency,backup/preflight}.rs`. ADR-106's promise is that every path
  re-checks pins. Nine paths is nine places for that promise to drift, and the
  deletion-path review rule in AGENTS.md exists because it has.
- **Config** is three parsers producing one struct that still carries the legacy
  `[defaults]` and `[local_snapshots]` sections, synthesised for v1 and v2 so that
  downstream code need not know. The internal struct is shaped by the oldest
  schema.
- **Run orchestration** in `commands/backup/` spawns a watchdog thread, a progress
  thread and a ctrl-c handler, coordinated through `Arc<Mutex<WatchdogCoord>>`,
  `Arc<Mutex<Vec<WatchdogFiring>>>`, atomics for bytes and cancel, and a
  completion sink. The command is about 1,800 production lines with a single
  684-line `run` function at its centre.
- **Observability** has four overlapping surfaces: a heartbeat JSON (schema 4), a
  Prometheus textfile (26 names), a sentinel state file (schema 3), and an events
  table, with the heartbeat doubling as a backup-to-sentinel mailbox.

(The core-pipeline review's quantitative findings are in section 3.9.)

### 3.3 The code is a journal

Source comments carry 809 references to the private `UPI` ticket series, 238 issue
numbers and 335 ADR citations. Comments explain the history of a line ("UPI 033
floor-suppression left reachable", "reworks the unreleased UPI 031") more often
than its present contract. The architecture doc's module table has rows fifteen
lines long. The glossary has clusters named after tickets ("Planner structure (UPI
089)"). The project enforces "present, not journey" on three documents with a lint
and practises journey everywhere else. For a newcomer, and for the author in a
year, the reading cost is the same as the writing cost was, and most of the
history lives in a vault the public repo cannot see (ADR-118), so the references
dangle.

### 3.4 Metaphor leaked into the engine

The rule is that the mythic voice lives only in `src/voice/`. Measured against
identifiers outside it: `Encounter`, `EncounterState`, `FateAnswers`, `Runestone`,
`RunestoneView`, `carve_config`, `AfterCarve`, `DelveDeeper`, `LookingView`,
`Doorstep`, `SealOutcome`, `SealGap`, `SealPosture`, `SealThread`, `earn_privilege`,
`World`, `WorldView`, `RunArming`, `TightnessTier`, "posture", "shed". The
glossary says `Runestone` "never appears in engine code"; it is a type at
`src/encounter.rs:151`. By line count, `src/voice/` holds about half of the
non-test user-facing prose; `src/commands/` and `src/notify.rs` hold the rest
("The well remembers, but the thread grows thin.", `src/notify.rs:359`). The
metaphor is good product; as module and type names it taxes every reader who has
to learn that "the earning" means "installing the sudoers file" and "carving"
means "writing the config".

### 3.5 The privilege model is sound against Urd and unsound against the user

ADR-120 motivates the scoped grant with "a bug in Urd's path construction would
then be bounded by nothing" and "any process running as the user". The first goal
is met. The second is not, and the ADR and the consent screen say otherwise.

The rendered grant (`src/sudoers.rs:210-246`) is, per line: `subvolume snapshot -r
<source> <root>/*`, `subvolume delete <dir>/*`, `send *`, `receive *`, and four
read-only verbs with `*`.

- `btrfs receive *` as root is root. A send stream carries owner, mode (setuid
  included) and xattrs (`security.capability` included). A process running as the
  user can feed a crafted stream into any directory on any btrfs filesystem not
  mounted `nosuid`. The consent text says send and receive "move snapshots without
  destroying anything".
- `btrfs send -f <path> *` opens an arbitrary path as root for writing, so it is an
  arbitrary root-owned file overwrite; `send` to stdout reads any read-only
  subvolume, including snapshots of root-owned sources.
- sudoers compares argument strings literally and the trailing `*` matches `/`,
  spaces and further arguments, so `delete <dir>/../../x` and `delete <dir>/x
  /other` both match the "scoped" line. btrfs follows symlinks, snapshot roots
  under `$HOME` are user-owned, and the earning chowns pool roots to the user
  (`src/commands/seal.rs:982`), so a symlink inside the scope redirects creation and
  deletion anywhere on the filesystem.
- On the destination side, `partial.exists()` and the cleanup delete follow
  symlinks on the external drive (`src/btrfs.rs:428-445`); the pre-send sweep and
  crash recovery in `src/executor/send.rs` share the exposure. The UUID check is
  optional and the drive token is "a signal, not a control" that fails open. A
  tampered drive can therefore cause root deletions of host subvolumes.
- `btrfs_path` is validated as absolute only; a config pointing at a user-writable
  binary makes the grant root for any code.
- After `sudo -k`, the earning runs `sudo install -d -o <user>` on pool roots,
  which prompts again and is not on the consent screen.
- The sentinel daemon, a long-running user process, also holds the deletion grant
  for its idle eject.

None of this is exploitable without first running code as the user, which on a
single-user desktop usually already means the user. But that is also the threat
model the ADR names, and it is the threat model a tool "for many users" must meet.
The honest statement today is: the grant protects against Urd's own bugs.

### 3.6 Distribution and operation

- Install is "download a static binary, `install -Dm755` it, run `urd init`", or
  `cargo install`. There is no deb, rpm, COPR, AUR or Flatpak, and the shipped
  units say `%h/.cargo/bin/urd` until the seal rewrites them.
- Everything is a **user** systemd unit, so backups depend on lingering, which is
  advised and not enforced; a headless host that never logs in never backs up.
- There is no uninstall path at all; the only unwinder is a dev script that deletes
  subvolumes as root behind a marker file.
- Notifications shell out to `curl` and `notify-send`. Mount detection in the
  sentinel is a 5-second polling loop over `findmnt` output instead of a udev or
  mount-table watch.
- `now` is `chrono::Local::now().naive_local()` everywhere, and snapshot names are
  `YYYYMMDD-HHMM`. Naive local time is ambiguous for one hour a year and the
  on-disk name is a contract (ADR-105).
- SQLite has no schema version; evolution is `CREATE TABLE IF NOT EXISTS` plus
  ad hoc migrations, and the events table is append-only and never pruned by
  design (ADR-114).
- Upgrading a unit or the grant means re-running `urd init`; restarting the
  sentinel after an upgrade is a manual step named in a runbook.

### 3.7 Testing

Test code is 61 % of the source. There are 2,724 tests and none of them touches a
btrfs filesystem: a grep for loop devices, `mkfs.btrfs` or `losetup` across
`tests/`, `scripts/` and `.github/` finds nothing, and `RealBtrfs` has no test
against the real binary. Every btrfs interaction is mocked (`MockBtrfs`,
`MockFileSystemState`), which is why the suite runs in two seconds and why seven
of the 0.33.x fixes and all four of the 0.38.0 pin fixes were found in the field
rather than in CI. The integration directory holds one test, ignored by default,
and CI never passes `--ignored`. The fixtures are spread over five test kits and
about forty module-local `test_config()` helpers that build near-identical
configs in different ways, with three different `snap()` constructors and three
different fixed-clock helpers. Running the suite as root in this sandbox gives
six failures, most of them "an unwritable path is writable to root"; I did not
confirm all six, and they are environment-shaped rather than code bugs, but a
suite that assumes a non-root uid without checking is a suite that will surprise
a packager.

Loop-device btrfs is cheap in CI and is the only test that can catch "the drive's
snapshot directory is a symlink", "receive failed half-way and left a partial",
or "the drive vanished mid-send".

### 3.8 Shape

A single binary crate with 50 modules declared in `main.rs`, no `lib.rs`, so
nothing is reusable, integration tests must shell out to the binary, and the
pure/impure boundary that the ADRs care about so much is enforced by a bash script
that greps Rust rather than by crate boundaries the compiler enforces for free.

### 3.9 What the core pipeline review measured

An adversarial read of `plan/`, `executor/`, `retention.rs`, the pressure modules
and `commands/backup/` (production lines only, comments and tests excluded)
produced these numbers; I checked the ones the argument rests on.

| Area | Production lines |
|---|---|
| `plan/` | about 1,580 (types alone 413) |
| `executor/` | about 1,700 |
| `retention.rs` | 1,075, of which 361 are the retention rules and 713 are the preview and the ADR-110 gate |
| `commands/backup/` | about 1,785; `backup::run` itself is one 684-line function (`src/commands/backup/mod.rs:54-737`) |
| pressure: `storage_critical`, `arming`, `guard`, `storage_signals`, watchdog, reclaim, lifecycle, run-tail teardown | about 1,500 across ten modules |

- **The base decision** ("snapshot now? send now? which to delete?") is about
  750 of those lines. The storage-pressure machinery outside the planner is twice
  that, and about a quarter of the planner is routing a declared graduated
  subvolume into the transient lifecycle under Tight or Critical.
- **One pressure value in many coats.** `HeadroomSeverity` and `TightnessTier`
  are isomorphic three-value enums joined by a `From`; the armed tier is copied
  into five carrier types so that it is "resolved once", with a `debug_assert`
  re-derivation to prove the copies agree (`src/storage_critical.rs:371-400`);
  "away pin" exists in eight forms; the tier-adapted lifecycle is copied field by
  field from `PlannedLifecycle` to `SubvolumeContext` to `SubvolJudgment`.
- **About nine free-space thresholds** overlap: the local `min_free` guard, the
  source floor (`min_free + 1.5 % capacity`), the 25 %/15 % ratio tiers with two
  hysteresis bands, the absolute-headroom gate at 3×/3.5× the floor, the
  doctor/emergency ladder at 2×/1×/0.5× `min_free`, the watchdog floor (which
  degrades for a pool that started below it), the idle-eject floor, retention's
  own space-pressure mode, and the executor's space-recovery short-circuit, plus
  a 1.2× send-size margin. `src/guard.rs:142` says in a comment that whether the
  ladder survives alongside the tiers "belongs to the ADR-113 re-grill".
- **The planner is not the sole decider.** Four passes mutate the plan after
  `plan()` returns: the retention-change gate (which re-lists directories and
  re-runs retention under the old shape), token gating, the emergency pre-flight
  followed by a full re-plan, and the executor's chain-break policy chosen from
  `INVOCATION_ID` in the command layer. Planning also performs I/O through the
  observation traits (a `sudo btrfs subvolume show` per subvolume, free bytes
  read three times, drive availability read three ways), so "pure" means "no
  direct syscalls", which is what the lint checks, not "a function of one
  snapshot of the world".
- **Deletion paths and ADR-106.** Of the eight deletion sites, the planned
  retention path carries all three protective layers; the watchdog/eject
  reclaim and `delete_candidates` carry none of layer 1 and the reclaim
  re-implements layer 3 inline rather than calling the one function the ADR
  names. The "remove pin, re-read, delete, collect first error" routine is
  written three times. When a pin file is unreadable the planner reads it
  leniently, so layers 1 and 2 silently lose that pin and only layer 3 holds; the
  result is still safe, but in practice there is one layer, not three.
- **One inconsistency worth a decision, not a patch.** The backup's unattended
  emergency pre-flight and `urd emergency` both keep only the newest snapshot
  plus pins and offer the rest, for every non-transient subvolume including one
  that has never been sent anywhere (`src/commands/emergency.rs:63-80`,
  `src/commands/backup/preflight.rs:155-172`). The watchdog reclaim refuses
  exactly that case ("never the only copy", `src/executor/reclaim.rs:217`). Two
  pressure responses disagree about whether local-only history may be
  sacrificed for the host. ADR-113 says the host wins, so the pre-flight is
  probably the intended reading, but then the reclaim's gate is a lie, and a
  user with a `recorded` subvolume should be told which it is.
- **Threads.** `urd backup` runs the main thread, a ctrl-c thread, a 250 ms
  progress poller, a 250 ms watchdog poller when a pool is armed, and three
  threads per send (two stderr drains and the copy pump), coordinated by four
  `AtomicBool`s, one `AtomicU64` and three `Arc<Mutex<_>>`. On a
  cross-filesystem trip the watchdog constructs a second `Executor` and reclaims
  concurrently with the running send, which forces an in-flight/tripped
  handshake, per-send cancel resets, outcome stashing back to the main thread,
  and mutex-poison recovery. The reviewer found no race; neither did I. A
  watchdog that only sets flags and lets the sequential main thread reclaim
  between operations removes all of it at the cost of letting one send on
  another filesystem finish first.
- **Estimated essential size** of the same guarantees, written once: roughly
  1,800 to 2,300 production lines for the core against 8,100 today, before
  comments. That ratio, about 3.5 to 4, is the number to remember when deciding
  between refactor and rebuild.

Small defects the review turned up, listed so they are not lost: a dead
`_ext_dir` parameter at `src/plan/mod.rs:619`; a doc comment naming a `reserve`
module that no longer exists at `src/commands/backup/mod.rs:7`; the executor's
lifecycle fallback at `src/executor/mod.rs:202` re-deriving declared transience
against the rule that only the planner derives lifecycle; `preflight.rs:218`
syncing later roots on the run-wide `any_deleted` rather than that root's own
deletions; the operating guide's sudoers template missing the `subvolume list *`
line the renderer emits; ADR-101's "no other module spawns btrfs" being untrue
of the `probe_grant` and `send --help` calls; `docs/20-reference` saying the
heartbeat and metrics paths have no implicit default when the code defaults
both; and the operating guide saying the unit runs `urd backup` when it runs
`urd backup --auto`.

## 4. The rebuild: Urd 1.0

The guiding question for every item: does it make the data safer, reduce the
attention the user spends, or protect the host? Those three tests are the
project's own, and they are enough. The fourth test I add is: can a stranger
install it, trust it and remove it in ten minutes.

### 4.1 Shape: a workspace, library first

```
urd/
  crates/
    urd-core       pure domain: types, policy derivation, planner, retention,
                   assessment, pressure model. No I/O, no clock, no std::fs.
                   The compiler enforces this: the crate has no I/O deps.
    urd-btrfs      the only module that knows btrfs-progs exists: snapshot, send,
                   receive, delete, list, show, sync, statvfs; error translation.
    urd-store      pin files, drive tokens, SQLite history (versioned schema).
    urd-agent      the privileged helper (section 4.3). Tiny. Audited.
    urd-cli        clap, commands, plain renderer, voice theme on top.
    urd-daemon     scheduler + mount watcher (section 4.5).
  packaging/       deb, rpm, AUR, systemd system units, sudoers fallback
  tests/btrfs/     loop-device integration tests
```

What this buys: the purity lints disappear because `urd-core` cannot import
`std::fs` without someone adding a dependency in a reviewable place; `urd-core`
becomes testable and reusable (a GUI, a TUI, a different scheduler); the
privileged surface becomes one small crate that can be read in an afternoon.

### 4.2 Domain model: seven types

| Type | Meaning |
|---|---|
| `Subvolume` | a source path, a name, a protection level, its local snapshot root |
| `Destination` | a drive: label, filesystem UUID (required), mount path, snapshot root, role, rotation cadence (optional) |
| `Policy` | derived from level + run cadence: snapshot interval, send interval, graduated retention (local, external) |
| `Snapshot` | name (timestamp + short name), location, read-only, received-UUID if a copy |
| `Chain` | per (subvolume, destination): the pinned parent |
| `Plan` | ordered operations with reasons, plus skips with reasons |
| `Outcome` | per operation: done / failed(error) / skipped(reason), bytes, duration |

One `SkipReason` enum, one `DeleteReason` enum, one `Pressure` struct per pool
(free bytes, capacity, floor, margin). Protection levels stay opaque; custom stays
first class. Nothing in this table is a metaphor.

### 4.3 Privilege: a helper with seven verbs, not a sudoers grant

The sudoers grant cannot be made safe, because `receive` and `send` are
root-equivalent primitives and sudoers cannot canonicalise paths. The rebuild
moves the privilege boundary from "which command lines may run as root" to "which
operations may be requested, on which paths, by whom", which is a question a
program can answer and sudoers cannot.

| Option | Safe against same-uid attacker | Install burden | Notes |
|---|---|---|---|
| Rendered sudoers (today) | no | low | root-equivalent via `receive *`, `send -f` |
| Rendered sudoers, narrowed | no | low | cannot fix `receive`; `*` still crosses `/` |
| setuid helper binary | yes, if the helper validates | low | hard to get right, hard to package |
| polkit action + helper | yes | medium | fits desktops; awkward headless |
| **Root system service with a Unix socket** | **yes** | **low with packages** | the one everything else on Linux converged on |

Recommendation: `urd-agent`, a systemd **system** service running as root, listening
on a socket activated unit with `SO_PEERCRED` checks, exposing exactly:

```
snapshot(subvol_id, snapshot_name)              -> ro snapshot into the subvol's registered root
send_receive(subvol_id, snapshot_name, dest_id, parent: Option<name>)
delete(location_id, snapshot_name)              -> only inside a registered snapshot root
list(location_id) / show(location_id, name) / sync(location_id)
free_space(location_id)
```

The agent never accepts a path from the client. It accepts identifiers that it
resolves against a root-owned manifest written at `urd init` (with the same
consent sequence as today's earning), canonicalises every resolved path with
`O_PATH | O_NOFOLLOW` directory handles and `openat`, refuses anything that is a
symlink or escapes its root, and only ever receives into its own staging directory
before renaming. With that, "the grant protects against Urd's bugs" becomes "the
grant protects against anything running as the user", the units can use
`ProtectSystem=strict`, `NoNewPrivileges=yes` and the rest of the hardening set
on both sides, headless hosts need no lingering, and the sentinel never holds
root.

Keep the sudoers renderer as a documented fallback for people who refuse a root
service, with its ADR rewritten to state honestly what it defends against. Keep
`refuse, don't escape` and the staged install sequence; they move into the agent's
manifest installer.

### 4.4 Storage safety: one invariant, two numbers

Per pool: `floor` (the host-survival line, bytes) and `margin` (how much room above
the floor Urd needs before it will add footprint). Three rules, in priority order:

1. Below `floor`: delete Urd's own unpinned local snapshots on that pool, oldest
   first, until above `floor`; abort a send reading from that pool. This is
   today's watchdog and eject, as one loop that runs both during and between runs.
2. Between `floor` and `floor + margin`: create nothing new on that pool; sends
   from it still run if a parent exists.
3. Above: normal.

Defaults derived from capacity, overridable per pool, no tiers, no hysteresis
beyond the margin itself, no "posture" and no "arming". Pins are shed only by rule
1 and only for drives not present (ADR-116 stays). This loses nothing ADR-113
promises and removes most of what twelve fixes and six amendments were about.

### 4.5 Scheduling and the daemon

One daemon (`urd-daemon`, user or system, no root) that owns the clock and the
world view: it runs the plan on a timer, watches `/proc/self/mountinfo` through
`poll` (or libudev) instead of polling `findmnt` every five seconds, debounces
drive arrivals, triggers a send when an offsite drive comes home, and writes one
status file. `urd backup` becomes "ask the daemon to run now, or run inline if no
daemon". The timer-vs-sentinel split, the heartbeat-as-mailbox, the lock-holder
metadata and the two cadences collapse into one process with one state.

### 4.6 Persistence

Keep: snapshot names, directory layout, pin files, drive tokens, all with the
current on-disk forms so a 0.39 host upgrades in place. Change: SQLite gets
`PRAGMA user_version` and numbered migrations; the events table gets a retention
window; timestamps are UTC with the local rendering done in the renderer, and
snapshot names carry a zone-safe form from day one (the ADR-105 migration this
needs is the one worth paying for).

### 4.7 Config: one schema, one struct

`config_version = 3` only. `urd migrate` converts 0.39 files in one hop (it already
has the three readers; they move to the migrator and leave the runtime). The
runtime struct mirrors the schema; no synthesised legacy sections. Add the
concepts a population needs and the author's host did not: `exclude` (subvolumes
to skip under a source), a per-destination `encryption = "luks-uuid"` hint for the
unlock story, and `notify` channels implemented in-process (an HTTP client crate
and D-Bus for desktop notifications rather than `curl` and `notify-send`).

### 4.8 Observability: one status document

One JSON status file written by the daemon after every assessment, schema
versioned, containing what the heartbeat, sentinel state and pool rows carry
today. The Prometheus textfile is a projection of that document, kept
byte-compatible for the 22 `backup_*` names and dropping the four `urd_*` names
after a deprecation release. The events table stays as the audit log.

### 4.9 Voice

Keep every sentence. Move it: a plain renderer (`status: at risk — docs: last send
3d ago to WD-18TB`) is the default for `--json`, daemons, logs and notifications; the
mythic theme is a renderer that wraps it for terminals. Type and module names in
every crate are plain English: `setup`, not `encounter`; `install_grant`, not
`earn_privilege`; `proposal`, not `runestone`; `write_config`, not `carve`. The
glossary shrinks to user-facing terms only.

### 4.10 Distribution

A release ships: the static binary, `.deb` and `.rpm` (nfpm or cargo-dist), an AUR
`PKGBUILD`, a COPR spec, system unit files, the agent's manifest installer, and
`urd uninstall` that removes everything it created except snapshots (which it
lists). `urd init` stays the guided path and becomes the only path documented in
the README. Target the three families with btrfs root installers (Fedora,
openSUSE, Arch) first; test in CI on all three.

### 4.11 Testing

- `urd-core`: property tests for retention (never deletes a pin, never deletes the
  newest, keeps at least one per tier when one exists) and for the pressure rules;
  snapshot-name parsing round trips.
- `urd-btrfs` and `urd-agent`: loop-device btrfs in CI (`truncate`, `mkfs.btrfs`,
  `mount`), exercising real send/receive, partial-receive cleanup, symlinked roots,
  and a drive that disappears mid-send.
- One end-to-end test per subcommand through the real binary against the loop
  filesystem.
- Fewer mocks: one `FakeBtrfs` in `urd-btrfs`, no separate test kits per module.

### 4.12 Process

- ADRs stay immutable and short, and an amendment is a new ADR; six stacked
  amendments is a sign the decision was not ready.
- No ticket or issue numbers in source. A comment states the contract of the
  line below it, and `git blame` holds the history.
- One changelog entry per user-visible change, not per internal consolidation.
- The "symmetric fixes need symmetric reviews" and "deletion paths get an
  adversarial review" rules stay; with one deletion path they get cheap.

## 5. How to get there without stopping

A rewrite that stops the backups is a rewrite the author will not finish. The
strangler order, each step shippable:

1. **Extract `urd-core`.** Move `types`, `plan/`, `retention`, `awareness`,
   `advice`, `guard`, `storage_critical`, `arming` into a library crate with no I/O
   dependencies. Mostly mechanical; the purity lint becomes the compiler. Expect
   the first real cycles and leaks to surface here and fix them by moving code,
   not by adding `pub(crate)` doors.
2. **Build `urd-agent` and switch `BtrfsOps` to talk to it.** `RealBtrfs` becomes
   a socket client; the trait does not change, so nothing above it notices. Keep
   the sudoers path behind a feature flag for one release.
3. **Collapse pressure to floor + margin.** Delete tiers, arming, posture and
   hysteresis; keep the floor arithmetic and the watchdog loop; route every
   deletion through one `delete_snapshots(reason)` in the executor.
4. **One config schema.** v3 writer in `migrate`, the three readers move out of the
   runtime.
5. **One daemon.** Fold the timer-driven backup and the sentinel into
   `urd-daemon`; mount watch replaces polling.
6. **Rename.** Metaphor names to plain names, in one commit per crate, voice untouched.
7. **Packages and uninstall.**
8. **Loop-device CI**, then start deleting mocks.

Steps 1 to 3 are where the safety and the size come from; after them the codebase
should be well under half its current size with every ADR invariant intact.

## 6. What I would not do

- Add async. Nothing here is concurrent enough to pay for it; the agent's socket
  server is a `for stream in listener.incoming()` loop.
- Add a plugin system, a web UI, or non-btrfs backends before 1.0. Promise
  semantics on btrfs is the product.
- Keep the sentinel as a polling process. Keep tiers. Keep three parsers.
- Rewrite the voice. It is the part people will remember.
- Start by deleting tests. Delete code; let its tests go with it.

## 7. Questions only the author can answer

1. Is a root system service acceptable for the target users, or must the design
   also work on hosts where the user will not install one?
2. Which of the 0.39 on-disk contracts (metric names, heartbeat fields, sentinel
   state file) have consumers other than the author's monitoring stack? If none,
   1.0 is the moment to change them.
3. Is the offsite-rotation inference (median homecoming gap) used, or would a
   declared `rotation_interval` with a sensible default do?
4. Which distributions should 1.0 promise to run on, and is a btrfs root required
   or is a btrfs data pool on an ext4 root a supported shape?
