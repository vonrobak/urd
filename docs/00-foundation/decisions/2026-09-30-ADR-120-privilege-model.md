---
type: ADR
title: The Privilege Model
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-09-30'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-120: The Privilege Model

> **TL;DR:** Urd's root access is a sudoers grant derived from the config by one pure
> oracle (`sudoers.rs`). The grant scopes snapshot creation and deletion to snapshot
> directories and leaves send and receive broad. Any value that could change the meaning
> of a sudoers line is refused, never escaped. The grant is installed only with the user's
> consent, through a staged, twice-validated, byte-checked, atomic sequence
> (`commands/seal.rs`). The same oracle is the expected side of a drift check that
> `urd doctor` runs against the effective privileges. The systemd units follow the same
> pattern (`systemd_units.rs`). The seal runs its stages in a fixed order, and every
> surface reports the first missing one.

**Date:** 2026-09-30
**Status:** Accepted

## Context

Urd needs root for btrfs: snapshot creation and deletion, send and receive, and four
read-only queries. It runs unattended from a systemd user timer, so the privilege has
to be passwordless (`sudo -n`), and it has to exist before the first backup. It is the
largest trust Urd asks for, and three facts shape how it is granted.

- **A wildcard grant is a root-deletion grant.** `btrfs subvolume delete *` lets any
  process running as the user delete any subvolume on the host, live data included. A
  bug in Urd's path construction would then be bounded by nothing.
- **Sudoers is a hostile medium for config values.** An escaped newline is a line
  continuation, `#` starts a comment, and `ALL` is a reserved word. A snapshot root that
  is a perfectly good path can still become a sudoers line that means something else. No
  escaping discipline fixes this, because the characters that need escaping are the ones
  that change what a line is.
- **A grant drifts from the config.** Add a drive or a snapshot root and the installed
  file no longer covers what Urd will run. The run then fails at 04:00 with a sudo
  refusal, and nobody sees it until promises start going AT RISK. The installed file is
  mode 0440 root:root, so an unprivileged process cannot read it to compare.

A grant maintained by hand fails in all three ways, and none of the failures is visible
until a backup fails.

ADR-101 confines the btrfs *invocations* to `btrfs.rs`. ADR-109 validates config values
at load for use as *paths*. Neither says what authorizes the invocations, or how values
are validated for the sudoers medium. This ADR does.

## Decision

### One oracle renders the grant

`src/sudoers.rs` is pure (ADR-108): config, username, config path, and date in; the
grant's text out. `render_sudoers` renders the whole `/etc/sudoers.d/urd` file, and
`expected_grant_lines` returns its command specs. Nothing else in the repository
describes the grant's shape. The operating guide
([operating-urd.md](../guides/operating-urd.md)) documents the rendering rules, the seal
installs the render, and the drift check diffs against it.

The grant has four sections, each deduplicated and in a stable order:

| Section | Lines | Scope |
|---|---|---|
| Creation | `<btrfs> subvolume snapshot -r <source> <root>/*`, one per source → snapshot-root pair, disabled subvolumes included | The pair. Re-enabling a subvolume never requires re-earning |
| Deletion | `<btrfs> subvolume delete <dir>/*`, one per local snapshot root and one per drive's snapshot directory | The snapshot directory. Never live data |
| Send / receive | `<btrfs> send *`, `<btrfs> receive *` | Broad. Sources and mount points vary, send is read-only, and receive only creates |
| Read-only | `subvolume show *`, `subvolume list *`, `filesystem show *`, `subvolume sync *` | Broad. Diagnostics, the post-seal inventory read, and sync after delete |

Every line is `<user> ALL=(root) NOPASSWD: <spec>`, with `<btrfs>` the configured
`btrfs_path`. Config values inside a spec are backslash-escaped for sudoers' structural
characters and fnmatch metacharacters (`escape_cmnd_token`), so a path always matches
literally. The trailing wildcard is appended after escaping, never passed through it.

A known limit: sudoers matches wildcards without `FNM_PATHNAME`, so the trailing `*`
matches across `/` and whitespace. The scoped directory prefix is the boundary that
matters. The tail is deliberately loose, because snapshot names vary.

### Refuse, don't escape

Some values are refused rather than escaped. A refusal is total: one bad value means
nothing is rendered (`SudoersRefusal`), and the message names the value.

- **Control characters and `#`**, in any value, and **non-UTF-8** in any path. A lossy
  rendering would name a path that does not exist.
- **Shallow scopes.** A snapshot root or drive snapshot directory with fewer than two
  normal path components (`scope_deep_enough`). `/` or `/snapshots` must never produce a
  `delete /*`-shaped grant. Creation scopes get the same floor, because creation writes
  into the scope as root. Strategy derivation (`strategy.rs`) consults the same predicate,
  so the Encounter never proposes a snapshot root the earning would later refuse.
- **The username** is never escaped, only accepted or refused. Whitespace and the
  characters sudoers parses in a User_List are refused, and so is the reserved word `ALL`,
  which would grant to every user on the host. The username comes from the passwd entry
  for the current uid (`invoking_username`), never from `$USER`, which is wrong under `su`.

`systemd_units.rs` applies the same posture one trust boundary further on. The units
oracle substitutes the resolved binary path into `ExecStart=` and refuses (`checked_exe`)
a path that is not UTF-8, not absolute, or contains whitespace or control characters.

### The earning: consented, staged, fail-closed

`commands/seal.rs::earn_privilege` installs the grant. It runs only from a TTY: from the
Encounter's post-carve tail, or from `urd init`. The sequence is:

1. **Render** (pure). A refusal ends the earning with the refusal's sentence.
2. **Unprivileged gate.** Write the render to a 0440 temp file and run `visudo -c -f` on
   it before asking anything. A render that visudo refuses is a bug in Urd and is never
   installable.
3. **Consent.** Show the exact content with a plain-language reading. Enter or `i`
   installs, `p` prints it for manual installation, `q` declines. End of input declines.
4. **Stage inertly.** `sudo install -m 0440 -o root -g root` to
   `/etc/sudoers.d/urd.staging`. sudoers' `includedir` ignores file names containing a
   `.`, so the staged file is inert if anything stops here.
5. **Bind what activates to what was shown.** Read the staged bytes back as root and
   compare them byte-for-byte with the render the user approved. A same-uid process could
   have swapped the temp file's content between the prompt and the copy. The comparison
   closes that window. Re-checking syntax alone would not.
6. **Root-side re-validation.** `sudo visudo -c -f` on the root-owned staged file.
7. **Atomic activation.** `sudo mv` within `/etc/sudoers.d`, so no partial file is ever
   active.
8. **Verify.** `sudo -k` drops the credential the install cached. Without it, the probe
   would pass whatever the file said. Then the passwordless probe runs, followed by the
   coverage cross-check below.

Every failure path leaves either nothing or the inert staged file, and says which. The
command is never `sudo tee`. A snapshot root that is not writable by the user (a
pool-canonical `.snapshots` directory) is created in the same authenticated window with
`sudo install -d -o <user>`, because this is the earning's one interactive-root moment.

The earning ends in one of four outcomes (`SealOutcome`): `Sealed`,
`SealedUnverifiedCoverage` (the grant works, but coverage could not be confirmed),
`InstalledUnverified` (installed, but the passwordless probe did not answer), and
`Declined` (nothing installed, whether the user declined or Urd could not proceed). A
grant that already answers and covers every expected line is `Sealed` without asking. One
that answers but definitively lacks expected lines, meaning a config the installed file
predates, is offered a re-render.

### The drift oracle

The installed file is unreadable to an unprivileged process, so drift is judged against
*effective* privileges instead.

- **The probe** (`seal::probe_grant`): `LC_ALL=C sudo -n <btrfs> filesystem show /`,
  classified by `sudoers::classify_probe` as `Granted` (sudo ran the command, whatever
  btrfs then said), `Denied` (sudo refused), or `Unclear`. An unrecognized failure is
  `Unclear`, never `Denied`. A denied probe writes an auth-log line, so it runs only on
  interactive surfaces.
- **The listing** (`probes::sudo_privilege_listing`): `LC_ALL=C sudo -n -l`, parsed by
  `sudoers::parse_privilege_listing`. Anything the parser cannot place is `ParseUncertain`,
  never a guess.
- **Coverage** (`sudoers::coverage`) diffs `expected_grant_lines` against the listing in
  three states. An expected spec is *covered* by an exact `NOPASSWD` match or a blanket
  `NOPASSWD: ALL`. It is *missing* when nothing could plausibly cover it, and *uncertain*
  when a wildcard grant on the same binary might, since Urd does not interpret fnmatch
  subsumption. A password-tagged match never covers, because automation runs `sudo -n`. A
  listing with negated specs is `CannotInterpret`.

Uncertainty always renders as "cannot verify", never as a pass. A hand-managed broad
grant is never nagged: re-earning is offered only on definitive missing lines.

The units oracle has the same shape. `expected_units` renders the unit set the configured
run frequency selects: the nightly `urd-backup.service` and `.timer`, plus
`urd-sentinel.service` in Sentinel mode. The sources are the repository's `systemd/`
files, embedded at compile time. `diff_units` compares installed contents byte for byte.
The seal's units stage installs from that render, and `urd doctor` reports drift against
it. Install and check therefore cannot disagree.

### Stage order

`seal::resume_seal` runs the seal's stages in a fixed order: **earning → drive adoption →
units → first snapshot → first-send offer → second look → summary.** Each stage opens with
an idempotent done-check, so `urd init` re-enters cleanly after any interruption. No
stage's failure unwinds an earlier stage. Only a grant that answers lets the later stages
run. `Declined` and `InstalledUnverified` stop the seal with their sentences printed,
because a first snapshot guaranteed to fail at `sudo btrfs` would bury the real cause.

Status surfaces report the first gap in the same order, **privilege → units → first
thread**, one sentence for one cause:

- `seal_gap_given_probe` (bare `urd`, `urd status`) is the cheap check. Privilege gaps on
  a `Denied` probe, and units on a missing file (existence only, no `systemctl`). An
  `Unclear` probe reports no gap at all, rather than presume later stages.
- `seal_gap_deep` (`urd init`) also gaps privilege on definitive missing coverage and
  units on content drift.

### Relation to ADR-109

ADR-109's rule is that user values are validated once at a boundary and trusted
afterwards. This ADR adds two boundaries, one per medium. `Config::validate` makes a value
safe as a *path* for `btrfs.rs`. `sudoers.rs` makes it safe as a *sudoers token*, and
`systemd_units.rs` makes the binary path safe as an *`ExecStart=` word*. A value can pass
the first and be refused by the second; the scope floor and `#` are the common cases. The
checks sit where the medium is known, in the pure function that produces the artifact, so
the artifact cannot be produced without them.

## Alternatives Considered

- **A broad wildcard grant** (`<btrfs> *`, or `subvolume delete *`). One line, never
  drifts, needs no oracle. Rejected: any process running as the user could delete any
  subvolume on the host, live data included, and a path-construction bug in Urd would be
  bounded by nothing. Scoping deletion to snapshot directories is the point of the grant.
- **Escape every config value instead of refusing any.** Urd does escape what escaping
  can neutralize (`escape_cmnd_token`). Rejected as the whole answer: a newline or other
  control character, `#`, and the word `ALL` do not make a token odd, they change what the
  line *is* (a continuation, a comment, a grant to everyone). An escaping rule that must
  anticipate every such construct cannot be shown total, and a gap in it is a root grant
  nobody asked for. Refusal is total by construction and costs only a config edit. Shallow
  scopes are refused for a different reason that no escaping would address.
- **Run Urd as root** (a system unit) **or through a privileged helper** (setuid binary,
  polkit action). Rejected: running as root widens the root-running surface from a
  handful of btrfs command shapes to the whole program, config parsing and rendering
  included. A helper adds a privileged component that has to be written, secured,
  packaged and installed separately, which is more to trust than a sudoers file the user
  can read line by line.
- **A hand-maintained sudoers file** (the state before the earning). Rejected: it drifts
  from the config as drives and snapshot roots are added, the drift is invisible because
  the file is root-only, and the first sign is a sudo refusal at 04:00. It fails in all
  three ways the Context describes.

## Consequences

### Positive

- A bug in Urd's deletion paths is bounded by the grant to snapshot directories. It
  cannot delete a live subvolume through sudo.
- What the user consents to is byte-identical to what activates, and nothing
  half-written is ever active under `includedir`.
- Drift is detectable without reading a root-only file, and it is reported with the fix.
  A config change that outgrows the grant surfaces in `urd doctor` and `urd init` before
  a 04:00 run fails on it.
- One render serves the guide, the install, and the check, so none of them can describe
  a different grant.

### Negative

- Send and receive stay broad. A process running as the user can `btrfs send` any
  subvolume, which reads data, and `btrfs receive` into any directory, which creates
  subvolumes. Scoping them would need the source and mount paths of every future drive at
  earning time.
- Refusal can block a legitimate config: a snapshot root at `/snapshots` or a path with
  `#` in it. The user has to change the config, not the renderer.
- The coverage check is conservative. A broad hand-written grant that fully covers Urd
  can read as "uncertain" forever, because Urd does not interpret wildcards.
- A static musl binary resolves users from `/etc/passwd` only (ADR-117), so the earning
  refuses directory-managed users.

### Neutral

- The earning is interactive by construction and never runs from a daemon path. A
  headless host installs the printed grant by hand, and the drift check then covers it the
  same way.
- The trailing-wildcard looseness is a sudoers property. It is documented, not solved.

## Related

- [ADR-101](2026-03-24-ADR-101-btrfsops-trait.md): the btrfs subprocess boundary this
  grant authorizes; `probe_grant` is its one sanctioned non-`btrfs.rs` btrfs invocation.
- [ADR-108](2026-03-24-ADR-108-pure-function-module-pattern.md): `sudoers.rs` and
  `systemd_units.rs` are pure; `commands/seal.rs` and `probes.rs` hold the I/O.
- [ADR-109](2026-03-24-ADR-109-config-boundary-validation.md): config-boundary validation
  and its 2026-09-30 amendment listing the four validation boundaries.
- [ADR-110](2026-03-26-ADR-110-protection-promises.md): the run frequency that selects
  the unit set, and the Sentinel-mode cadence amendment.
- [ADR-117](2026-07-11-ADR-117-release-artifact-contract.md): the release trust chain
  for a binary that earns root, and the musl passwd limit.
