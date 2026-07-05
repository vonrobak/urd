# CLI Reference

> **TL;DR:** Reference for every `urd` subcommand — its contract (what it
> guarantees and what it refuses), notable flags, output channels, and exit
> codes. This is not a `--help` mirror; it documents the *behavior contract*
> a script or operator can rely on. For flag spelling and shell completion,
> use `urd <subcommand> --help`.

**Source of truth:** `src/cli.rs` (parser) and `src/commands/` (handlers).
**Binary name:** `urd`.

---

## Conventions

### Output mode

Urd auto-detects its output mode from stdout:

- **Interactive (TTY):** human-readable text, colored, with the mythic voice
  carried in `voice.rs`. Format may evolve across versions.
- **Daemon (non-TTY):** machine-readable JSON, no ANSI codes. Schema is
  internal but stable enough for monitoring scripts.

Pipe output to force daemon mode (e.g., `urd status | jq`). Set `NO_COLOR=1`
to suppress colors even on a TTY.

### Exit codes

- **0** — success or advisory output only (warnings do not raise).
- **1** — failure. Either an `anyhow::Error` propagated from the command,
  or an explicit `std::process::exit(1)` for partial-success cases (see
  `backup` and `verify` below).
- **2** — reserved by clap for usage errors (bad flags, unknown
  subcommands).
- **3** — not configured. No config file exists at the resolved path; the
  command printed a one-sentence pointer at `urd init` (JSON
  `{"status":"not_configured"}` in daemon mode) and did nothing. Scriptable:
  `urd status; test $? -eq 3` distinguishes "unconfigured" from "broken".

Beyond these, distinguish failure causes by parsing the diagnostic message
or, for monitoring, by reading the heartbeat / metrics files instead of the
exit code.

### stdout vs stderr

- **stdout** carries the command's primary output (status text, plan tables,
  restored file content for `urd get`).
- **stderr** carries log lines (suppressed below `Error` on TTY, `Warn`
  off-TTY; `--verbose` raises to `Debug` on both). `RUST_LOG` overrides.

### Global flags

| Flag | Semantics |
|------|-----------|
| `--config <PATH>` / `-c` | Override config path. Default: `~/.config/urd/urd.toml`. |
| `--verbose` / `-v` | Raise log level to `Debug`. Affects stderr only. |

### Config requirements

Each subcommand declares whether it requires a config load:

- **No config required:** `completions`, `migrate`. Run on a fresh system.
- **Config offered:** bare `urd` and `urd init` — with no config and a
  terminal on both stdin and stdout, they offer the Encounter (the guided
  first-time conversation); otherwise they print the pointer and exit 3.
- **Config required:** all other subcommands. A missing config prints one
  pointer sentence and exits 3; an invalid config fails fast with a
  diagnostic; no partial behavior.

---

## Subcommands

### `urd` (no subcommand)

**Contract.** With a config: a one-screen promise summary (compact status);
interactively it checks the seal once (a `sudo -n` probe, never prompting,
then unit-file existence and first-snapshot presence) and appends one clause
for the first incomplete seal stage — configured-but-unsealed, schedule not
yet enabled, or first thread not yet spun — each pointing at `urd init`.
Without a config: offers the Encounter
when a human is on both ends (stdin and stdout are terminals); otherwise
prints the pointer and exits 3. Declining or quitting the Encounter writes
nothing — the config file appears only at the carve, after explicit
approval of the runestone.

**Output.** Stdout — text; `{"status":"not_configured"}` in daemon mode.

**Exit codes.** `0` with a config, or after a declined/quit/carved
Encounter; `3` when unconfigured and no conversation is possible; `1` on
real failures (including a carve refusal).

---

### `urd plan`

**Contract.** Pure preview. Computes what `urd backup` *would* do given the
current config and filesystem state, without executing anything. Reads
filesystem and SQLite state; never writes. Safe to run any time, on any
host, without sudo.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--auto` | Apply interval gating (skip subvolumes whose interval has not elapsed). Without this, plan shows what an immediate manual run would do. |
| `--priority <1-3>` | Restrict to subvolumes of the given priority. |
| `--subvolume <name>` | Restrict to a single subvolume by name. |
| `--local-only` / `--external-only` | Restrict to local or external operations. |
| `--force-snapshot` | Include snapshot creation even for unchanged subvolumes. |

**Output.** Interactive — table per subvolume. Daemon — JSON.

**Exit codes.** `0` on success; `1` on config or filesystem read failure.

---

### `urd backup`

**Contract.** Executes the plan: snapshots, sends, retention, heartbeat,
metrics. Requires sudo for btrfs operations (configured via sudoers). Per
[ADR-100](../00-foundation/decisions/2026-03-24-ADR-100-planner-executor-separation.md),
individual subvolume failures do not abort the run — the executor isolates
errors and continues. Per
[ADR-107](../00-foundation/decisions/2026-03-24-ADR-107-fail-open-cleanup-on-failure.md),
backups proceed when in doubt; deletions refuse when in doubt. Per
[ADR-113](../00-foundation/decisions/2026-04-18-ADR-113-do-no-harm-invariant.md),
the planner refuses to create local snapshots when free space is below
`min_free_bytes` (no override).

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--dry-run` | Plan + simulate; no btrfs operations executed, no state writes. |
| `--auto` | Automated run mode — apply interval gating, suppress non-essential output. Used by the systemd timer. |
| `--confirm-retention-change` | Required to delete snapshots whose protection level was relaxed in this config session. Fail-closed: without the flag, retention is skipped for affected subvolumes. |
| `--force-full` | Force full sends for chain-broken subvolumes. Without this, chain-break full sends are skipped in `--auto` mode (avoids surprise multi-TB sends from the timer). |
| `--priority <1-3>`, `--subvolume <name>`, `--local-only`, `--external-only`, `--force-snapshot` | Same scoping as `plan`. |

**Output.** Interactive — per-subvolume summary plus an aggregated voice
block. Daemon — JSON. Heartbeat and metrics files are written regardless of
output mode.

**Exit codes.** `0` if `result.overall == Success` (every subvolume succeeded
or was legitimately skipped). `1` for `Partial` (some subvolume failed) or
`Failure` (run-level failure). Distinguishing partial vs total failure
requires reading the heartbeat or metrics, not the exit code.

---

### `urd status`

**Contract.** Read-only. Reports current promise states per subvolume,
drive presence, and overall data safety. Reads filesystem, SQLite, and the
heartbeat file; writes nothing. Safe under any condition, including a
running backup (advisory locking via `lock.rs` makes the read non-conflicting).
Interactively it also checks the seal once (a `sudo -n` probe, never prompting,
plus unit-file existence and first-snapshot presence) and names the **first
incomplete seal stage** — `privilege` (configured but unsealed), `units` (the
schedule is not enabled), or `first_thread` (a promise has no local snapshot) —
with `urd init` as the resume verb, one gap, one sentence. Daemon runs never
probe (a denied probe writes an auth-log line), so the `seal_gap` field is
absent from daemon JSON — it serializes only when a gap exists, and only
interactive runs check.

**Output.** Interactive — voice-rendered status block answering "is my data
safe?" Daemon — JSON.

**Exit codes.** `0` always (an unprotected subvolume is a displayable state,
not an error).

---

### `urd history`

**Contract.** Reads the SQLite `runs` and per-run tables and renders the most
recent N runs. Read-only.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--last <N>` | Number of recent runs to show (default 10). |
| `--subvolume <name>` | Filter by subvolume. |
| `--failures` | Show only failed operations. |

**Output.** Interactive — table. Daemon — JSON.

**Exit codes.** `0` on success; `1` on SQLite read failure.

---

### `urd verify`

**Contract.** Diagnoses thread integrity and pin health: walks every
subvolume × drive pair, checks that the pin file points at a snapshot that
exists locally and on the drive (or that the chain origin is sound).
Read-only with respect to the filesystem and state. Used standalone or as
part of `urd doctor --thorough`.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--subvolume <name>` | Scope to a single subvolume. |
| `--drive <label>` | Scope to a single drive. |
| `--detail` | Show every check, not just findings. |

**Output.** Interactive — voice-rendered verify block. Daemon — JSON.

**Exit codes.** `0` if every check passed. `1` if any check failed
(`fail_count > 0`). Warnings alone do not raise.

---

### `urd init`

**Contract.** The idempotent make-whole verb. No config → offers the
Encounter (pointer + exit 3 without a terminal on both ends). Invalid
config + terminal → the fix-it loop ((e)dit in `$VISUAL`/`$EDITOR` /
(q)uit keeping the file, error named), then continues into the checks;
I/O failures (permissions) always surface instead. With a loadable
config and a terminal on both ends, ANY incomplete seal stage resumes
**the seal at its first incomplete stage** (UPI 071/075); a fully
sealed system enters no ceremony. The stages, each opening with an
idempotent done-check:

1. **The earning** (denied grant): render the scoped sudoers file from
   the config, `visudo -c` gate, show + consent, staged fail-closed
   install (`/etc/sudoers.d/urd.staging`, root-side re-validation,
   atomic rename to `/etc/sudoers.d/urd`), passwordless verification
   probe + `sudo -l` coverage cross-check. Declining prints the content
   and the manual command; EOF never installs. A declined or unverified
   earning ends the seal (later stages need the grant).
2. **Drive adoption**: each configured, mounted, UUID-verified drive
   gets its snapshot home and identity token (the `urd drives adopt`
   decision, shared). Unreachable drives get one honest sentence each;
   the seal continues.
3. **Units**: the embedded systemd units (ExecStart substituted with
   this binary's resolved path), written to `~/.config/systemd/user/`
   and enabled with consent — the nightly pair, plus the sentinel
   service in sentinel mode. Lingering off earns one honest sentence
   (user timers fire only while logged in); Urd never enables lingering.
4. **The first local snapshot**: local snapshot homes are created, then
   a normal `backup --local-only` run through the full pipeline.
5. **The first-send offer**: explicit, honest about duration, never
   time-limited; Enter sends now, `t`/EOF defer to tonight's timer.
   Declining is recorded nowhere — a later `urd init` re-offers.
6. **The second look** (privileged, annotate-only): `btrfs subvolume
   list` per promised pool; at most one summary sentence about
   subvolumes no promise covers. Never re-opens the conversation.
7. **The summary scroll**: what was woven, when Urd acts next (derived
   only from the installed units), the honest partial states, and the
   handoff — `urd status`.

After (or without) the seal: ensures the state DB directory and
heartbeat directory exist, runs the same infrastructure checks
`doctor` runs, and exits cleanly. Safe to run multiple times. The
install steps prompt for your password via sudo.

**Output.** Interactive — the conversation or the checklist of
infrastructure items; `{"status":"not_configured"}` in daemon mode
when unconfigured.

**Exit codes.** `0` on success (including a declined/quit Encounter);
`3` unconfigured without a conversation; `1` if the config stays
invalid (fix-it quit) or required infrastructure cannot be created or
verified.

---

### `urd calibrate`

**Contract.** Measures snapshot sizes (via `du`) to improve send-time
estimates. Reads the filesystem; may take significant wall-clock time on
large subvolumes. Writes calibration data to the state DB.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--subvolume <name>` | Restrict to a single subvolume. |

**Output.** Interactive — per-subvolume size summary. Daemon — JSON.

**Exit codes.** `0` on success; `1` on filesystem or state DB error.

---

### `urd get`

**Contract.** Restores a single file from a past snapshot to stdout (or to
`--output`). Read-only with respect to all Urd state. Subvolume is
auto-detected from the source path or selected via `--subvolume`. Refuses
when no snapshot exists at or before the target date, or when the target
file does not exist in the selected snapshot.

**Required positional + flags.**

| Argument | Semantics |
|----------|-----------|
| `<path>` (positional) | File to retrieve. Resolved against the working directory. |
| `--at <DATE>` | Date reference. Accepts `YYYY-MM-DD`, `YYYYMMDD`, `today`, `yesterday`. |
| `--output <PATH>` / `-o` | Write to file instead of stdout. |
| `--subvolume <name>` | Override automatic subvolume detection. |

**Output.** Stdout — file contents (binary-safe), or human summary if
`--output` is set.

**Exit codes.** `0` on success; `1` on any failure (no matching subvolume,
no snapshot before date, file absent in snapshot, write failure).

---

### `urd sentinel run`

**Contract.** Starts the Sentinel daemon in the foreground. Designed to be
run by systemd as a user service (`urd-sentinel.service`); `Restart=on-failure`
in the unit handles crash recovery. Owns sub-hourly monitoring, drive
detection, and overdue-backup notifications. The Sentinel never executes
backups itself — it triggers them via `urd backup` invocations.

**No flags.** Configuration is via `urd.toml`'s `[notifications]` and
`[sentinel]` sections.

**Output.** Stderr — log lines (lifecycle events at `warn` level by
convention so they remain visible at default log levels). Stdout is
unused.

**Exit codes.** `0` on clean shutdown (SIGTERM); `1` on crash or
configuration error.

---

### `urd sentinel status`

**Contract.** Reads the Sentinel's state file (PID, last activity, circuit
breaker state) and reports it. Read-only.

**Output.** Interactive — daemon status block. Daemon — JSON.

**Exit codes.** `0` if Sentinel is running. `1` if state file missing or
PID is dead.

---

### `urd drives` (no subcommand)

**Contract.** Lists configured drives and their current state (mounted /
absent / unrecognized UUID). Read-only.

**Output.** Interactive — drive table. Daemon — JSON.

**Exit codes.** `0` always.

---

### `urd drives adopt <LABEL>`

**Contract.** Accepts a drive into Urd's identity system by recording its
BTRFS UUID against the configured label. Refuses if the drive is not
mounted, if the label is not in the config, or if the drive already has
a different UUID recorded (the operator must reconcile the conflict in
config first). Writes to the state DB.

**Output.** Interactive — adoption confirmation. Daemon — JSON.

**Exit codes.** `0` on success; `1` on refusal or write failure.

---

### `urd doctor`

**Contract.** Runs the full diagnostic battery: config preflight,
infrastructure checks (DB, dirs, sudo btrfs), the sudoers drift advisory,
the systemd-units drift advisory + linger check, drive UUID fingerprinting,
and local-space trend warnings. Read-only.
Designed to be the first thing an operator runs when something feels off.
The drift advisory diffs the config's expected grants (single oracle:
`sudoers.rs`) against effective privileges from `sudo -n -l`: a working
grant with full coverage renders one Ok row; a config mapping with no
covering grant warns and names `urd init` as the re-render verb; a listing
that needs a password, contains negations, or resists parsing is an honest
"cannot verify" warning — never a silent pass. When the grant itself is
denied, only the sudo-btrfs check speaks (one cause, one finding).
The units advisory diffs the installed unit files against what THIS binary
would render (single oracle: `systemd_units.rs`): all-match is one Ok row;
a missing unit warns with `urd init` as the completing verb; a differing
unit warns naming both ExecStart paths (a doctor run from a dev build
self-diagnoses). With the units in place, `Linger=no` earns one Warn naming
`loginctl enable-linger` — user timers fire only while a session exists.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--thorough` | Add thread-verification (`urd verify`), churn, retention-shape recommendations, and the orphan-pin retention scan to the battery. Slower; reads every pin file. |

**Output.** Interactive — voice-rendered diagnostic block with severity
icons and suggested next steps. Daemon — JSON.

**Exit codes.** `0` if no checks reached `Error` severity (warnings allowed).
`1` if any check is `Error`.

See [doctor-walk runbook](../10-operations/runbooks/doctor-walk.md) for
interpretation of findings.

---

### `urd emergency`

**Contract.** Guided interactive flow for space recovery. Surfaces eligible
snapshots for emergency deletion, prompts for confirmation, and executes
the deletion via `BtrfsOps`. Refuses non-interactive use (no `--yes`
available by design — this is the human-in-the-loop mode for catastrophic
pressure). Honors pin protection: pinned snapshots never appear in the
delete-eligible list.

**Output.** Interactive — guided prompts. Daemon mode is not supported
(non-interactive runs are refused).

**Exit codes.** `0` on completion (whether the operator deleted anything
or not); `1` on hard failure.

---

### `urd retention-preview`

**Contract.** Shows what retention would prune on the next run, per
subvolume, given the current config. Read-only. Useful for evaluating a
config change before running `backup`.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `<subvolume>` (positional) | Single subvolume to preview. |
| `--all` | Preview every configured subvolume. |
| `--compare` | Show the transient/graduated comparison alongside. |

**Output.** Interactive — per-subvolume keep/prune table. Daemon — JSON.

**Exit codes.** `0` on success; `1` on filesystem or state read failure.

---

### `urd events`

**Contract.** Reads the structured event log (see
[ADR-114](../00-foundation/decisions/2026-04-30-ADR-114-structured-event-log.md))
and prints filtered events. Read-only.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--since <DURATION>` | Only events from the last duration (e.g., `7d`, `24h`, `30m`). |
| `--kind <KIND>` | Filter by kind: `retention`, `planner`, `promise`, `sentinel`, `config`, `drive`. |
| `--subvolume <name>` | Filter by subvolume. |
| `--drive <label>` | Filter by drive label. |
| `--limit <N>` | Maximum events to display. Default `50`, max `1000`. |
| `--json` | Line-delimited JSON output. **Not** a stable public contract — additive but versioned-by-evolution; expect new fields and event variants. |

**Output.** Interactive — columnar event log. Daemon or `--json` —
NDJSON.

**Exit codes.** `0` on success; `1` on state DB read failure.

---

### `urd migrate`

**Contract.** Transforms a legacy `urd.toml` to the v1 schema (see
[ADR-111](../00-foundation/decisions/2026-03-27-ADR-111-config-system-architecture.md)).
Reads the raw TOML, builds v1 as string output, writes it back to the
config path, and saves the original to `<path>.legacy` as a backup.
Refuses configs already at `config_version = 1` or higher (no-op with
diagnostic).

This is one of two **config-free** commands — it is dispatched before
config load, so it works on a config that the regular loader would reject.

**Notable flags.**

| Flag | Semantics |
|------|-----------|
| `--dry-run` | Show what would change without writing files. |

**Output.** Interactive — diff summary. The `--dry-run` summary contains
the proposed v1 TOML for review.

**Exit codes.** `0` on success or no-op; `1` on parse failure or write
failure.

---

### `urd completions <SHELL>`

**Contract.** Generates a shell completion script for the named shell
(`bash`, `zsh`, `fish`, `elvish`, `powershell`) and prints it to stdout.
Config-free — does not load `urd.toml`.

**Output.** Stdout — completion script.

**Exit codes.** `0` always (clap rejects unknown shells before reaching
the handler).

---

## Stability classes

| Surface | Stability |
|---------|-----------|
| Subcommand names (`backup`, `status`, ...) | Stable. Renames require ADR + deprecation period. |
| Flag spelling | Stable for documented flags. New flags are additive. |
| Exit code semantics (`0` / `1`) | Stable. |
| Interactive (TTY) text format | Evolving. Do not parse. |
| Daemon JSON output | Internal — additive evolution, but no formal contract. Use heartbeat / metrics for monitoring. |
| `urd events --json` | Explicitly **not** a stable contract. Expect new fields and new event variants. |

For monitoring, prefer the heartbeat ([heartbeat-schema.md](heartbeat-schema.md))
and metrics ([metrics.md](metrics.md)) over parsing CLI output.

---

## See also

- [Prometheus metrics reference](metrics.md)
- [Heartbeat schema reference](heartbeat-schema.md)
- [Runbooks](../10-operations/runbooks/) — operational procedures that use these commands
- [ADR-111 — Config system architecture](../00-foundation/decisions/2026-03-27-ADR-111-config-system-architecture.md) — `urd migrate` design
- [ADR-114 — Structured event log](../00-foundation/decisions/2026-04-30-ADR-114-structured-event-log.md) — `urd events` data source
