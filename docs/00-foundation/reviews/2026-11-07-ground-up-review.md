---
type: Review
title: Urd from the ground up — an independent review
categories: ['[[Review]]']
project: ['[[urd]]']
sensitivity: public
status: draft
created: '2026-11-07'
timestamp: '2026-11-07T18:00:00+01:00'
---
# Urd from the ground up — an independent review

> **TL;DR:** Urd works, and the ideas behind it are worth keeping: promises instead of
> operations, a pure planner, filesystem-is-truth, fail-open backups and fail-closed
> deletes, the host wins. The body built around those ideas is not what a first-class
> product would have. It is one host's backup system generalised after the fact: about
> 48 K production lines of Rust, of which some 7 K is the core pipeline, for a core
> that in my estimate needs a quarter of that; a privilege model that bounds Urd's own
> path bugs but neither a process running as the user nor a tampered drive; three
> config parsers; eight deletion sites with one of them carrying all three protective
> layers; three overlapping storage-pressure vocabularies; and its own development
> history written into its comments. A rebuild should keep the invariants, the
> retention algorithm, the chain logic and the voice, and replace the shape: a
> library-first workspace, a privileged helper with a small identifier-based API in
> place of a sudoers grant, one config schema, one deletion path, a pressure model
> with a floor and a footprint cap, real btrfs in CI, and packages. The rest of this
> document says why and how, and section 4.13 lists what the proposal costs.

**Audience:** the author, and anyone deciding whether the next Urd is a refactor or a
rewrite. **Method:** I read the architecture doc, all 21 ADRs, the glossary, the
operating guide, the changelog, the entry points and the privilege, btrfs, planner,
config, lock, sentinel and systemd code myself; I delegated bounded inventories
(CLI, config, on-disk contracts, test suite, changelog classes, vocabulary) and two
adversarial reviews (privilege model, core pipeline), checked their load-bearing
claims against the source, and then had the whole document adversarially reviewed
by a third reader who re-counted every number, re-ran the test suite as root and as
a normal user, and reproduced the privilege findings on loop-device btrfs
filesystems with a test sudoers rule. Where that review corrected me, the
correction is in the text. Line references are to this checkout (0.39.0).

## 1. The project as it stands

| Measure | Value |
|---|---|
| Rust in `src/` | 116,592 lines: about 48,000 production, 69,000 test (59 %); 14,725 comment lines |
| Production lines in the core pipeline (plan, executor, retention, pressure, backup driver), comments and tests excluded | about 7,200 |
| `#[test]` functions | 2,725; the suite runs in about 2 s after a 35 to 50 s debug build |
| Dependencies | 17 direct, 70 at runtime, 122 packages in the lockfile; release binary 7.4 MiB with LTO and strip |
| ADRs | 21, several carrying five or six in-file amendments |
| Glossary | 63 KB, fourteen clusters |
| Releases | 68 between 2026-03-22 and 2026-10-01 |
| Changelog entries | 155 Added, 98 Changed, 116 Fixed, 4 Removed |
| CLI subcommands | 16, plus bare `urd` |
| Config schemas parsed | 3 (legacy, v1, v2) into one `Config` |
| SQLite tables | 8 live, no `user_version` pragma |
| External binaries invoked | sudo, btrfs, visudo, systemctl, loginctl, findmnt, lsblk, du, notify-send, curl, a user hook command, `$EDITOR` |
| Provenance markers in source comments | about 825 `UPI` references, 240 issue numbers, 335 ADR references |
| Library crate | none; 45 modules declared in `main.rs` |
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
- **A type-level read/write split on btrfs.** `BtrfsRead` is a supertrait of
  `BtrfsOps`, so a planner holding `&dyn BtrfsRead` cannot reach a mutator. That
  seam is enforced by the compiler, not by a script.
- **Filesystem is truth, SQLite is history** (ADR-102). Snapshot directories and
  pin files are authoritative; the database can be lost without losing backups.
- **Fail open on backups, fail closed on deletions** (ADR-107), and three
  independent layers keeping pinned snapshots alive (ADR-106), with strict pin
  reads on every emergency path.
- **The host wins** (ADR-113), learned from a real ENOSPC incident and written down
  in a postmortem.
- **Sends are never time-limited** (`TimeoutStartSec=infinity`).
- **Subprocess hygiene**: no shell, `OsStr` arguments, `sudo -n`, `LC_ALL=C`, both
  exit codes of the send/receive pipeline checked, partial receives cleaned up only
  after receive provably exited (`src/btrfs.rs`), and a census in ADR-101 of every
  privileged subprocess.
- **The sudoers install sequence**: render, unprivileged `visudo -c`, consent,
  inert dot-named staging file, root-side byte comparison with what the user saw,
  root-side `visudo -c`, atomic `mv`, `sudo -k` before the probe
  (`src/commands/seal.rs:772-1000`). Whatever privilege model the rebuild chooses,
  this is how to install a privileged artifact.
- **Refuse rather than escape** hostile config values in a hostile medium, and an
  ADR (120) that discloses its own known limit on wildcard matching.
- **Call-site seams in `clippy.toml`** (ADR-119) where module visibility is too
  coarse.
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
is missing for anyone else: no uninstall command or document, no packages, shipped
unit files that say `%h/.cargo/bin/urd` until the seal substitutes the real path,
no explanation anywhere in the config or user docs that a nested subvolume is how
you exclude a path from a snapshot (the Encounter's strategy code knows it), and
two config fields, `general.log_dir` and `drive.max_usage_percent`, that are
parsed, validated, migrated and rendered into generated configs with a comment
calling the latter a "space threshold for aggressive retention", while nothing at
runtime reads either; `src/error.rs:163` even advises users to raise the dead knob.

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
  ADR-113 reads as an archaeology of six amendments.
- **Deletion** happens at eight sites: planned retention (graduated, transient and
  space-pressure deletes), the Critical clear-all and retain-one lifecycle, the
  watchdog and idle-eject reclaim, `delete_candidates` behind `urd emergency` and
  the backup's emergency pre-flight, the same-name partial and the abandoned-partial
  sweep at the destination, the send-failure cleanup, and an interactive delete of
  incomplete snapshots in `urd init` (`src/commands/init.rs:513`), spread over
  `executor/{ops,lifecycle,reclaim,send}.rs`, `btrfs.rs` and `commands/`. ADR-106's
  promise is that every path re-checks pins. Eight sites is eight places for that
  promise to drift, and the deletion-path review rule in AGENTS.md exists because
  it has.
- **Config** is three parsers producing one struct that still carries the legacy
  `[defaults]` and `[local_snapshots]` sections, synthesised for v1 and v2 so that
  downstream code need not know. The internal struct is shaped by the oldest
  schema.
- **Run orchestration** in `commands/backup/` spawns a watchdog thread, a progress
  thread and a ctrl-c handler, coordinated through `Arc<Mutex<WatchdogCoord>>`,
  `Arc<Mutex<Vec<WatchdogFiring>>>`, atomics for bytes and cancel, and a
  completion sink. The command is about 1,800 production lines with a single
  `run` function at its centre that spans 684 lines (412 of them code).
- **Observability** has four overlapping surfaces: a heartbeat JSON (schema 4), a
  Prometheus textfile (26 names), a sentinel state file (schema 3), and an events
  table, with the heartbeat doubling as a backup-to-sentinel mailbox.

Section 3.9 has the measurements behind this.

### 3.3 The code is a journal

Source comments carry about 825 references to the private `UPI` ticket series, 240
issue numbers and 335 ADR citations. Comments explain the history of a line ("UPI
033 floor-suppression left reachable", "reworks the unreleased UPI 031") more often
than its present contract. The architecture doc's module table has rows fifteen
lines long. The glossary has clusters named after tickets ("Planner structure (UPI
089)"). The project enforces "present, not journey" on three documents with a lint
and practises journey everywhere else. For a newcomer, and for the author in a
year, the reading cost is the same as the writing cost was, and most of the
history lives in a vault the public repo cannot see (ADR-118), so the references
dangle.

### 3.4 Metaphor leaked into the engine

The rule is that the mythic voice lives only in `src/voice/`. Measured against
identifiers outside it: `Encounter`, `EncounterState`, `FateAnswers`,
`PromptKind::Runestone`, `RunestoneView`, `carve_config`, `AfterCarve`,
`DelveDeeper`, `LookingView`, `Doorstep`, `SealOutcome`, `SealGap`, `SealPosture`,
`SealThread`, `earn_privilege`, `World`, `WorldView`, `RunArming`, `TightnessTier`,
"posture", "shed". The glossary says the runestone "never appears in engine code";
it is an enum variant at `src/encounter.rs:151` and a struct at `:192`. Counting the
words in production string literals of four words or more outside log macros,
`src/voice/` holds about a quarter; the rest is spread across `awareness/`,
`sentinel.rs`, `preflight.rs`, `error.rs`, `output.rs`, `encounter.rs`, `cli.rs`,
`commands/` and `notify.rs` ("The well remembers, but the thread grows thin.",
`src/notify.rs:359`). The metaphor is good product; as module and type names it
taxes every reader who has to learn that "the earning" means "installing the
sudoers file" and "carving" means "writing the config".

### 3.5 The privilege model bounds Urd's path bugs, not a user process or a tampered drive

ADR-120 is honest about more than I first credited: it says the scoped lines bound
"a bug in Urd's deletion paths", its consequences section admits a same-uid process
can send any subvolume and receive anywhere, and it documents the missing
`FNM_PATHNAME` as a known limit. Its errors are three sentences: that receive "only
creates", that send is "read-only", and that "the scoped directory prefix is the
boundary that matters". The consent screen goes further than the ADR and says send
and receive "move snapshots without destroying anything", that the grant is
"exactly this, no more", and that "creation and deletion stay inside your snapshot
directories". Every one of those claims was tested on a loop-device btrfs
filesystem with the rendered rule installed for a throwaway user, and every one
fails.

The rendered grant (`src/sudoers.rs:199-246`) is, per line: `subvolume snapshot -r
<source> <root>/*`, `subvolume delete <dir>/*`, `send *`, `receive *`, and four
read-only verbs with `*`.

- **`btrfs receive *` as root is root.** A send stream carries chown, chmod and
  xattr commands, and receive applies them as root. A stream holding a setuid-root
  binary with `cap_dac_override` was received by the throwaway user with only the
  `receive *` rule; running the result printed `euid=0`. This needs the target
  filesystem to be mounted without `nosuid`: udisks mounts removable drives
  `nosuid` by default, so on most desktops the vector is the fstab pools and the
  root filesystem, not the backup drive.
- **`btrfs send -f <path> *`** opens the path as root for writing and truncates it.
  The content is a send stream, not attacker-chosen bytes, so this is destruction
  (clobbering any file, for instance in `/etc/sudoers.d/`), not code injection.
  `send` to stdout reads any read-only subvolume, including snapshots of
  root-owned sources.
- **The scoped lines are not scoped.** sudoers compares argument strings literally,
  as one concatenated string, and the manual says a wildcard matches `/` and white
  space. `sudo -l` reports `delete <dir>/../../../x` and `delete <dir>/x /other` as
  allowed, and both deleted subvolumes outside the scope in the test. btrfs follows
  symlinks; snapshot roots under `$HOME` are user-owned, and the earning creates
  missing or unwritable local roots as the user with `sudo install -d -o <user>`
  (`src/commands/seal.rs:982`), so a symlink inside the scope redirects creation and
  deletion anywhere on the filesystem.
- **The destination side trusts the drive.** `partial.exists()` and the cleanup
  delete follow symlinks (`src/btrfs.rs:428-445`), and the pre-send sweep
  (`src/executor/send.rs:90-111`) needs no failed receive at all: any entry on the
  drive with a contract-shaped name newer than the pin whose target has no received
  UUID is deleted as root. A nested host subvolume behind such a symlink was deleted
  in the test; a subvolume that is itself a mount root was not. The UUID check is
  optional and the drive token is documented as "an identity signal, not a security
  control" whose threat model is "accidental hardware swaps" (`src/drives.rs:27-29`),
  and it fails open.
- `btrfs_path` is validated as absolute and free of `..`; there is no ownership or
  writability check, so a config pointing at a user-writable binary makes the grant
  root for any code, though the path is shown on the consent screen.
- After `sudo -k`, the earning runs `sudo install -d -o <user>` on local roots that
  need it, which prompts again and is not on the consent screen.
- The sentinel daemon, a long-running user process, also issues `sudo btrfs
  subvolume delete` for its idle eject. The grant belongs to the uid, so this adds no
  privilege the user's other processes lack, but it does mean a daemon holds it.

Two threat models, two answers. The same-uid vectors (receive, send, `..`, symlinks
in user-owned roots) need code already running as the user, which on a single-user
desktop usually already means the user. The drive vector needs only write access to
the external drive, which an offsite drive gives to anyone who holds it, and it
then costs host data through an Urd code path that the grant does not bound. So the
grant bounds Urd's own path-construction bugs; it bounds neither a user process nor
a tampered drive, and both are threat models a tool "for many users" has to meet.

One more thing the ADR did not consider: sudo 1.9.10 and later accept `^…$` regular
expressions in command specs, so a renderer could forbid `/`, `..`, white space and
`-f` in the tails, and the send sources and receive destinations are known at
earning time. That would close the `..` and extra-argument holes. It would not
close the symlink holes in user-owned directories, the `receive` setuid vector on
filesystems without `nosuid`, or the trust in drive contents, so narrowing is a
patch for the current model, not a model.

### 3.6 Distribution and operation

- Install is "download a static binary, `install -Dm755` it, run `urd init`", or
  `cargo install`. There is no deb, rpm, COPR, AUR or Flatpak.
- Everything is a **user** systemd unit, so backups depend on lingering, which the
  init flow warns about and does not enforce; a headless host that never logs in
  never backs up.
- There is no uninstall path at all; the only unwinder is a dev script that deletes
  subvolumes as root behind a marker file.
- Notifications shell out to `curl` and `notify-send` or a user hook command. The
  sentinel detects drives by re-reading `/proc/mounts` every five seconds and
  spawning `findmnt` for UUIDs, instead of waiting on the mount table or udev.
- `now` is `chrono::Local::now().naive_local()` in 47 places, and snapshot names
  are `YYYYMMDD-HHMM`. Naive local time is ambiguous for one hour a year and the
  on-disk name is a contract (ADR-105).
- SQLite has no schema version; evolution is `CREATE TABLE IF NOT EXISTS` plus
  ad hoc migrations, and the events table is append-only and never pruned by
  design (ADR-114).
- Upgrading a unit or the grant means re-running `urd init`; restarting the
  sentinel after an upgrade is a manual step named in a runbook.

### 3.7 Testing

Test code is about 59 % of the source. There are 2,725 tests and none of them
touches a btrfs filesystem: a grep for loop devices, `mkfs.btrfs` or `losetup`
across `tests/`, `scripts/` and `.github/` finds nothing, and `RealBtrfs` has one
ignored test against a shell stub. Every btrfs interaction is mocked (`MockBtrfs`,
`MockFileSystemState`), which is why the suite runs in two seconds and why seven
of the 0.33.x fixes and all four of the 0.38.0 pin fixes were found in the field
rather than in CI. The integration directory holds one test, ignored by default,
and CI never passes `--ignored`. The fixtures are spread over five test kits and
about forty module-local `test_config()` helpers that build near-identical
configs in different ways, with three different `snap()` constructors and three
different fixed-clock helpers.

Running the suite as root gives six failures and as a normal user three. Three
assume that a `chmod 0555` directory is unwritable, which is false for root. One
(`world::open_with_missing_state_db_degrades_to_none_without_panic`) creates
`/nonexistent-dir-for-urd-test/urd.db` when run as root and leaves it behind,
after which the same test fails for every non-root run on that machine until
someone deletes the directory. Two (`drives::drive_availability_uuid_mismatch`,
`init::drive_identity_check_fails_on_uuid_mismatch`) assume `findmnt -o UUID /`
returns something, which is false in a container. None is a product bug; all are
assumptions about the machine that a packager's build host will violate.

Loop-device btrfs is cheap in CI and is the only test that can catch "the drive's
snapshot directory is a symlink", "receive failed half-way and left a partial",
or "the drive vanished mid-send". Separately, the README's test count (2,224) is
stale.

### 3.8 Shape

A single binary crate with 45 modules declared in `main.rs` and no `lib.rs`, so
nothing is reusable, integration tests must shell out to the binary, and the
pure/impure boundary that the ADRs care about so much is enforced by a bash script
that greps Rust for `std::fs`, `Command::new`, clock reads, `std::env` and
`rusqlite` (the `BtrfsRead` split and the `clippy.toml` seams are the exceptions,
and they are the model to follow).

### 3.9 What the core pipeline review measured

An adversarial read of `plan/`, `executor/`, `retention.rs`, the pressure modules
and `commands/backup/` (production lines only, comments and tests excluded)
produced these numbers; the second reviewer re-counted them.

| Area | Production lines |
|---|---|
| `plan/` | about 1,570 (types alone 412) |
| `executor/` | about 1,660 |
| `retention.rs` | 1,074, of which 361 are the retention rules and 713 are the preview and the ADR-110 gate |
| `commands/backup/` | about 1,780; `backup::run` itself is one function spanning 684 lines, 412 of them code (`src/commands/backup/mod.rs:54-737`) |
| pressure: `storage_critical`, `arming`, `guard`, `storage_signals`, `run_tail`, plus the watchdog, reclaim and lifecycle code already counted above | about 1,500 across ten modules |
| union of all of the above | about 7,200 |

- **The base decision** ("snapshot now? send now? which to delete?") is about
  750 of those lines. The storage-pressure machinery outside the planner is twice
  that, and about a quarter of the planner is routing a declared graduated
  subvolume into the transient lifecycle under Tight or Critical.
- **One pressure value in many coats.** `HeadroomSeverity` and `TightnessTier`
  are isomorphic three-value enums joined by a `From`; the armed tier is copied
  into five or more carrier fields so that it is "resolved once", with a
  `debug_assert` re-derivation to prove the copies agree
  (`src/storage_critical.rs:380-389`); "away pin" exists in about eight forms; the
  tier-adapted lifecycle is copied field by field from `PlannedLifecycle` to
  `SubvolumeContext` to `SubvolJudgment`.
- **About nine free-space thresholds** overlap: the local `min_free` guard, the
  source floor (`min_free + 1.5 % capacity`), the 25 %/15 % ratio tiers with two
  hysteresis bands, the absolute-headroom gate at 3×/3.5× the floor, the
  doctor/emergency ladder at 2×/1×/0.5× `min_free`, the watchdog floor (which
  degrades for a pool that started below it), the idle-eject floor, retention's
  own space-pressure mode, and the executor's space-recovery short-circuit, plus
  a 1.2× send-size margin. `src/guard.rs:142-145` says in a comment that whether
  the ladder survives alongside the tiers "belongs to the ADR-113 re-grill".
- **The planner is not the sole decider.** Three passes mutate the plan after
  `plan()` returns: the retention-change gate (which re-lists directories and
  re-runs retention under the old shape), token gating, and the emergency
  pre-flight followed by a full re-plan; a fourth decision, the executor's
  chain-break policy, is chosen from `INVOCATION_ID` in the command layer.
  Planning also performs I/O through the observation traits (two `sudo btrfs
  subvolume show` calls per subvolume, free bytes read three times, drive
  availability read three ways), so "pure" means "no direct syscalls", which is
  what the lint checks, not "a function of one snapshot of the world".
- **Deletion sites and ADR-106.** Of the eight sites, the planned retention path
  carries all three protective layers; the watchdog/eject reclaim and
  `delete_candidates` carry none of layer 1 and the reclaim re-implements layer 3
  inline rather than calling the one function the ADR names. The "remove pin,
  re-read, delete, collect first error" routine is written three times. When a
  pin file is unreadable the planner reads it leniently, so layers 1 and 2
  silently lose that pin and only layer 3 holds; the result is still safe, but in
  practice there is one layer, not three.
- **One question the paths answer differently.** Under pressure, how much
  local-only history may go? The backup's unattended emergency pre-flight and
  `urd emergency` keep the newest snapshot plus pins and delete the rest, for every
  non-transient subvolume including one never sent anywhere
  (`src/retention.rs:386-418`, `src/commands/backup/preflight.rs:155-172`), and
  fire below half of `min_free`. The watchdog reclaim deletes every unpinned
  snapshot including the newest, so for a never-sent subvolume it refuses
  entirely ("never the only copy", `src/executor/reclaim.rs:217-227`), and fires at
  the higher source floor. Both keep at least one copy; they disagree on depth
  (one versus all) and on trigger, and a user with a `recorded` subvolume is told
  neither.
- **Threads.** `urd backup` runs the main thread, a ctrl-c thread, a 250 ms
  progress poller, a 250 ms watchdog poller when a pool is armed, and three
  threads per send (two stderr drains and the copy pump), coordinated by four
  `AtomicBool`s, one `AtomicU64` and three `Arc<Mutex<_>>`. On a
  cross-filesystem trip the watchdog constructs a second `Executor` and reclaims
  concurrently with the running send, which forces an in-flight/tripped
  handshake, per-send cancel resets, outcome stashing back to the main thread,
  and mutex-poison recovery. Neither reviewer found a race. A watchdog that only
  sets flags and lets the sequential main thread reclaim between operations
  removes all of it at the cost of letting one send on another filesystem finish
  first.
- **Estimated essential size** of the same guarantees, written once: roughly
  1,800 to 2,300 production lines for the core against about 7,200 today, before
  comments. That ratio, somewhere between three and four, is the number to
  remember when deciding between refactor and rebuild. It is an estimate, not a
  measurement.

Small defects the reviews turned up, listed so they are not lost: a dead
`_ext_dir` parameter at `src/plan/mod.rs:619`; a doc comment naming a `reserve`
module that no longer exists at `src/commands/backup/mod.rs:7`; the executor's
lifecycle fallback at `src/executor/mod.rs:202` re-deriving declared transience
against the rule that only the planner derives lifecycle; `preflight.rs:218`
syncing later roots on the run-wide `any_deleted` rather than that root's own
deletions; the operating guide's sudoers template missing the `subvolume list *`
line the renderer emits; `docs/20-reference` saying the heartbeat and metrics
paths have no implicit default when the code defaults both; the operating guide
saying the unit runs `urd backup` when it runs `urd backup --auto`; and the
README's stale test count.

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
                   assessment, pressure model. No I/O.
    urd-btrfs      the only module that knows btrfs-progs exists: snapshot, send,
                   receive, delete, list, show, sync, statvfs; error translation.
    urd-store      pin files, drive tokens, SQLite history (versioned schema).
    urd-agent      the privileged helper (section 4.3). Tiny. Audited.
    urd-cli        clap, commands, plain renderer, voice theme on top.
    urd-daemon     scheduler + mount watcher (section 4.5).
  packaging/       deb, rpm, AUR, systemd units, sudoers fallback
  tests/btrfs/     loop-device integration tests
```

What the crate boundary buys is dependency direction: `urd-core` cannot name
`rusqlite`, `nix` or the btrfs wrapper because they are not in its `Cargo.toml`.
It does not ban `std::fs` or the clock, because those ship with `std`; that ban
stays a `clippy.toml` `disallowed-methods`/`disallowed-types` list scoped to the
core crate, which is the mechanism ADR-119 already uses, replacing the bash lint
with something that understands Rust. `urd-core` becomes testable and reusable (a
GUI, a TUI, a different scheduler); the privileged surface becomes one small crate
that can be read in an afternoon. Extraction is feasible today: the pure modules
import a handful of pure helpers that happen to live in impure files
(`drives::external_snapshot_dir`, `chain::pin_path`, `output::SkipCategory`,
`BtrfsRead` in `btrfs.rs`), which move with them.

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
(free bytes, capacity, floor, cap). Protection levels stay opaque; custom stays
first class. Nothing in this table is a metaphor.

### 4.3 Privilege: a helper with a small identifier-based API, not a sudoers grant

The sudoers grant cannot be made safe, because `receive` is a root-equivalent
primitive, sudoers cannot canonicalise paths or see through symlinks, and the
drive's contents are an input. The rebuild moves the privilege boundary from
"which command lines may run as root" to "which operations may be requested, on
which registered locations, by whom", which is a question a program can answer and
sudoers cannot.

| Option | Same-uid process | Tampered drive | Install burden | Notes |
|---|---|---|---|---|
| Rendered sudoers (today) | no | no | low | root-equivalent via `receive *`, `send -f`, `..`, symlinks |
| Rendered sudoers with sudo ≥ 1.9.10 regex specs | partial | no | low | closes `..`, extra arguments and `-f`; not symlinks or setuid on `nosuid`-less mounts |
| setuid helper binary | yes, if it validates | yes, if it validates | low | hard to get right, hard to package |
| polkit action + D-Bus helper | yes | yes | medium | snapper's model (`snapperd`); fits desktops, awkward headless |
| **Root system service with a Unix socket** | **yes** | **yes** | **low with packages** | the general Linux pattern; D-Bus optional |

Recommendation: `urd-agent`, a systemd **system** service running as root,
listening on a socket-activated unit with `SO_PEERCRED` checks, exposing exactly:

```
snapshot(subvol_id, snapshot_name)
send_receive(subvol_id, snapshot_name, dest_id, parent: Option<name>)
delete(location_id, snapshot_name)
list(location_id) / show(location_id, name) / sync(location_id)
```

The agent never accepts a path from the client. It accepts identifiers that it
resolves against a root-owned manifest written at `urd init` (with the same
consent sequence as today's earning), walks every resolved path through
`O_PATH | O_NOFOLLOW` directory handles and `openat`, refuses anything that is a
symlink or escapes its root, receives under a temporary name inside the
destination directory and renames in place when the receive completes (a
read-only subvolume cannot be moved between directories; a same-directory rename
works), and checks that each location's filesystem UUID matches the manifest
before touching it. Free-space polling needs no root and stays client-side with
`statvfs`; the agent serves one connection per thread so an hours-long
`send_receive` does not block an abort request.

What this buys and what it does not. It buys a trust boundary at the socket:
nothing running as the user can delete outside a registered snapshot root, receive
anywhere else, or clobber a file, and a drive planted with symlinks is refused
rather than followed. It does not buy sandboxing of the agent itself: it needs
`CAP_SYS_ADMIN` for the btrfs ioctls, so `ProtectSystem=strict` is largely
cosmetic there and the hardening set applies in earnest only to `urd-daemon` and
the CLI, which no longer call `sudo` and so can take `NoNewPrivileges` and the
rest. The sentinel's successor never holds root. Headless hosts need no lingering
when `urd-daemon` is also a system unit with `User=`.

ADR-120 rejected a helper because a sudoers file "the user can read line by line"
is less to trust. The argument fails on its own evidence: the file reads as scoped
and is not, so readability bought false assurance. A thousand-line agent plus a
manifest that lists registered locations is also readable, and its meaning is what
it says.

Keep the sudoers renderer as a documented, supported fallback for people who
refuse a root service, with its ADR and consent screen rewritten to state what it
defends against (Urd's own path bugs) and what it does not. Keep `refuse, don't
escape` and the staged install sequence; they move into the agent's manifest
installer.

### 4.4 Storage safety: a floor and a cap

ADR-113's Layer 1 promises more than "stop before the floor": at Tight it keeps
backing up with a reduced local footprint, a stretched interval and the promise
capped at AT RISK; at Critical it keeps a zero steady footprint and a weekly
interval floor; and ADR-116 orders the shedding (away pins first, connected pins
if the floor still demands it). A rebuild should keep the behaviour and drop the
machinery. Per pool, two numbers: `floor` (the host-survival line, bytes) and
`cap` (the footprint above the floor at which Urd starts shedding its own
history). Three states, in priority order:

1. **Below `floor`:** shed away pins, then Urd's unpinned local snapshots on that
   pool oldest first, then connected pins if still below; abort a send reading
   from that pool; never delete the only copy of a subvolume. This is today's
   watchdog, idle eject and emergency pre-flight as one routine that runs during
   and between runs.
2. **Between `floor` and `floor + cap`:** keep snapshotting and sending, but keep
   locally only the newest snapshot and each drive's chain parent; the promise
   reads AT RISK and says why. This is today's retain-one and retain-parents
   without the tier.
3. **Above:** normal.

Defaults derived from capacity, overridable per pool. What is dropped, by name:
the free-ratio classifier with its two hysteresis bands, the absolute-headroom
gate at 3×/3.5×, the doctor ladder, the stretched intervals (a reduced footprint
makes a longer interval unnecessary), and the `Roomy/Tight/Critical` vocabulary
with its five carriers. The question in section 3.9 is settled here by rule 1:
never the only copy, at any depth. Hosts tuned to today's tiers will see
different thresholds; `urd migrate` should print the old and new numbers side by
side.

### 4.5 Scheduling and the daemon

One daemon (`urd-daemon`, a system unit with `User=`, no root) that owns the clock
and the world view: it runs the plan on a timer, watches `/proc/self/mountinfo`
through `poll` (which reports mount and unmount as `POLLPRI`) instead of
re-reading `/proc/mounts` every five seconds, debounces drive arrivals, triggers a
send when an offsite drive comes home, and writes one status file. `urd backup`
becomes "ask the daemon to run now, or run inline if no daemon". The
timer-vs-sentinel split, the heartbeat-as-mailbox, the lock-holder metadata and
the two cadences collapse into one process with one state.

### 4.6 Persistence

Keep: snapshot names, directory layout, pin files, drive tokens, all with the
current on-disk forms so a 0.39 host upgrades in place. Change: SQLite gets
`PRAGMA user_version` and numbered migrations; the events table gets a retention
window; the history DB records UTC and the renderer localises. The snapshot name
stays local `YYYYMMDD-HHMM` because changing it is an ADR-105 migration across
every drive; the one-hour autumn fold is handled by refusing to create a second
snapshot with a name that already exists, which at minute granularity is a rare
skip and never a loss.

### 4.7 Config: one schema, one struct

`config_version = 3` only. `urd migrate` converts 0.39 files in one hop (it already
has the three readers; they move to the migrator and leave the runtime). The
runtime struct mirrors the schema; no synthesised legacy sections; dead fields
removed with a warning in `migrate`. Btrfs snapshots are not recursive, so the
exclusion mechanism is a nested subvolume, and the product should say so: `urd
init` offers to convert cache-shaped directories (`~/.cache`, package caches,
build directories) into nested subvolumes, and the config documents the rule.
Notification channels are implemented in-process (an HTTP client crate and D-Bus
for desktop notifications rather than `curl` and `notify-send`), with the hook
command kept.

### 4.8 Observability: one status document

One JSON status file written by the daemon after every assessment, schema
versioned, containing what the heartbeat, sentinel state and pool rows carry
today. The Prometheus textfile is a projection of that document, kept
byte-compatible for the 22 `backup_*` names and dropping the four `urd_*` names
after a deprecation release with the consumer-side change shipped alongside, as
AGENTS.md requires. The events table stays as the audit log.

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
  a planted drive, and a drive that disappears mid-send.
- One end-to-end test per subcommand through the real binary against the loop
  filesystem.
- Tests that need "unwritable" or "a root filesystem with a UUID" check the
  precondition and skip with a reason rather than fail.
- Fewer mocks: one `FakeBtrfs` in `urd-btrfs`, no separate test kits per module.

### 4.12 Process

- ADRs stay immutable and short, and an amendment is a new ADR; six stacked
  amendments is a sign the decision was not ready.
- No ticket or issue numbers in source. A comment states the contract of the
  line below it, and `git blame` holds the history.
- One changelog entry per user-visible change, not per internal consolidation.
- The "symmetric fixes need symmetric reviews" and "deletion paths get an
  adversarial review" rules stay; with one deletion path they get cheap.

### 4.13 What the proposal costs

- **A root daemon is new attack surface** of its own: the manifest parser and the
  socket protocol run as root. That is the objection ADR-120 raised and it is
  real; the answer is that the surface is a few hundred lines with no user paths
  in it, against a grant that is root today.
- **Users who refuse a root service** keep the sudoers fallback, so the project
  carries two privilege paths indefinitely, and the fallback's limits must be
  stated at consent time.
- **Migration.** Existing grants, user units and tuned pressure thresholds change;
  `urd migrate` must remove the old grant and units, install the manifest, and
  show the pressure numbers before and after.
- **Contracts.** A new status document and the dropped `urd_*` metrics are
  ADR-105 changes with consumers to update.
- **The size estimates are estimates.** "A quarter of the core" and "1,800 to
  2,300 lines" are judgments from reading, not measurements of code that exists.

## 5. How to get there without stopping

A rewrite that stops the backups is a rewrite the author will not finish. The
strangler order, each step shippable:

1. **Extract `urd-core`.** Move `types`, `plan/`, `retention`, `awareness`,
   `advice`, `guard`, `storage_critical`, `arming` and the pure helpers they lean
   on into a library crate with no I/O crates in its dependencies and a clippy
   ban on `std::fs`, `std::process`, `std::env` and the clocks. Mostly mechanical;
   expect the first real cycles and leaks to surface here and fix them by moving
   code, not by adding `pub(crate)` doors.
2. **Build `urd-agent` and introduce an identifier-based operations trait.**
   `BtrfsOps` takes paths today; the agent must not, so the trait changes to
   `(location_id, snapshot_name)` and the planner's output does the same. The
   sudoers path implements the same trait by resolving identifiers to paths
   locally. Both ship; the agent is the default in packages.
3. **Collapse pressure to floor + cap.** Delete the ratio tiers, arming, posture,
   the headroom gate and the ladder; keep the floor arithmetic and the watchdog
   loop; route every deletion through one `delete_snapshots(reason)` in the
   executor.
4. **One config schema.** v3 writer in `migrate`, the three readers move out of the
   runtime.
5. **One daemon.** Fold the timer-driven backup and the sentinel into
   `urd-daemon`; a mount-table watch replaces polling.
6. **Rename.** Metaphor names to plain names, one commit per crate, voice untouched.
7. **Packages and uninstall.**
8. **Loop-device CI**, then start deleting mocks.

Steps 1 to 3 are where the safety and the size come from; after them the codebase
should be well under half its current size with every ADR invariant intact.

## 6. What I would not do

- Add async. Nothing here is concurrent enough to pay for it; the agent is a
  thread per connection.
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
5. Under pressure, how much local-only history of a `recorded` subvolume may go:
   down to one copy, as the pre-flight does today, or none, as the reclaim does?
