# The Encounter Staging Protocol

> **TL;DR:** A real, physical staging machine — Fedora Workstation, native btrfs with
> the default `/` + `/home` layout, one external btrfs drive reserved for the lab —
> is the ground truth for Urd's first-encounter experience. First impressions burn on
> first use, so this protocol makes blank slates repeatable: a guarded reset script
> (`scripts/staging-reset.sh`) unwinds every artifact Urd creates, an observation
> template captures each run, a scenario matrix defines coverage, and a findings loop
> feeds field reality back into designs and issues. The lab outlives the Encounter
> arc: any future change to the first-meeting flow is validated here before release.

## Purpose

The Encounter (the journey from Urd's GitHub page to the first sealed promise) can
only be validated by actually encountering it on a machine Urd has never seen — and
every field test consumes the blank slate it needs. Without a disciplined reset and
observation protocol, field testing degenerates into one-shot anecdotes. The staging
machine also supplies what no mock can: real `lsblk`/`findmnt` output for discovery
parser fixtures, real sudoers friction, real USB and LUKS behavior.

The reset script is repo tooling for this lab only. It is deliberately **not** a urd
command — a shipped "reset" verb would violate every fail-closed deletion instinct
Urd is built on (ADR-107) and hand users a footgun.

## The staging machine contract

The lab machine must be:

- **Real hardware**, not a VM — USB hotplug, desktop auto-mount, and sudo friction
  are part of what is being tested.
- **Fedora Workstation** with the default btrfs layout: one filesystem hosting the
  `/` and `/home` subvolumes. (Validation is deliberately Fedora-only for v1.0.)
- Equipped with **one external btrfs drive reserved for the lab**, never used for
  real backups. Keep it **LUKS-formatted** on purpose — that is the realistic
  GNOME-formatted case, and the locked-drive scenario depends on it.
- Otherwise disposable: nothing on the machine may be data anyone would miss.

## One-time setup: the marker file

The reset script refuses to run unless `~/.config/urd-staging-marker` exists. Placing
the marker — by hand, once, on the staging machine only — is the act of consent. It
was chosen over a hostname check so the public repo embeds no machine identity and the
script cannot be pattern-matched onto a new host by accident.

The marker is also the **deletion contract**: it declares the scope the script may
touch. Snapshot deletion only ever happens under roots declared here.

```
# urd staging contract — hand-authored once on the staging machine.
# Its presence authorizes scripts/staging-reset.sh on this machine.
snapshot_root=/home/<user>/.snapshots     # repeatable, absolute literal paths only
drive_uuid=<filesystem-uuid>              # optional: the ONE lab drive's btrfs UUID
drive_snapshot_root=.snapshots            # optional: relative on the drive (default)
```

Rules, enforced at parse (the script refuses on violation): `snapshot_root` values
must be absolute literal paths (no `~`, no variables); `drive_snapshot_root` must be
relative with no `..`; unknown keys warn. If the config file declares a snapshot root
the marker does not, the script warns and does not touch it — the marker, not the
config, is the authority (the config is itself a reset target and may be absent or
half-unwound; the marker is the only file the reset never deletes).

**Threat-model boundary (do not "fix" this):** the marker is a consent rail against
*mistakes* — running the script on the wrong machine, or with the wrong drive. It is
not a security boundary against a malicious local process; such a process already has
the user's shell and could place the marker itself. Treating it as a security feature
would only invite false confidence.

## The reset script

```
scripts/staging-reset.sh [--apply] [--drive UUID] [--full]
```

Dry-run is the default and prints the full deletion plan; only `--apply` executes.
`--drive UUID` includes the external drive (category 5); `--full` includes installed
binaries (category 9, for install-experience testing).

Exit codes: `0` clean · `2` usage · `3` refusal (missing/invalid marker, running as
root, drive sanity) · `4` typed-confirmation mismatch or EOF at the prompt · `5`
backup lock held · `6` finished with per-item failures (the summary lists them — the
machine is *not* a clean slate).

**`--apply` with `--drive` needs an interactive terminal.** The typed drive
confirmation reads from stdin; a non-interactive stdin (pipe, harness passthrough,
CI) EOFs the prompt, which exits 4 and aborts the drive category *and every category
after it* — categories already processed stay applied, leaving a half-reset machine
(observed live in field test 01). Dry-runs never prompt and are safe anywhere.

### Safety rails

1. **Marker-file allowlist** — no marker, no run (see above).
2. **Dry-run by default** — `--apply` is the only execution path; there is no `--yes`
   and no environment override.
3. **Contract-scoped deletion** — subvolume deletion only under marker-declared
   roots, only for directory names matching the snapshot-name contract
   (`YYYYMMDD-HHMM-*`, legacy `YYYYMMDD-*` — ADR-105), only for real directories
   (symlinks are reported as anomalies, never followed — `btrfs subvolume delete`
   resolves symlinks), and only after `sudo btrfs subvolume show` confirms the path
   is a subvolume. There is no recursive `rm` outside the known, bounded `logs/`
   directory.
4. **External drive by UUID + typed confirmation** — the drive is addressed by
   `--drive UUID` (never a mount path), must match the marker's declared
   `drive_uuid`, must resolve to exactly one removable mountpoint (`/run/media/*` or
   `/media/*`, never `/` or `/home` — on the Fedora default layout those share one
   btrfs UUID, an easy copy-paste trap), and must not be the filesystem backing any
   declared local snapshot root. Under `--apply` the operator then types the drive's
   filesystem label (or its full UUID when unlabeled) before anything on the drive is
   touched. The script never mounts and never unlocks — a locked LUKS drive is simply
   not mounted, and the honest skip is the correct outcome.

Two more guards: the script refuses to run as root (it invokes `sudo` itself only for
the btrfs and sudoers steps), and after stopping the systemd units it probes Urd's
backup advisory lock — if a backup is still running, the reset aborts rather than
delete state under a live run. Per-item failures never abort a category: the script
isolates them, continues, and lists every failure in the summary.

### What it unwinds

| # | Category | Where |
|---|----------|-------|
| 1 | Config | `~/.config/urd/` — `urd.toml` + `urd migrate` backups (`.legacy`, `.v1`) |
| 2 | State | `~/.local/share/urd/` — `urd.db` (+`-wal`/`-shm`), `urd.lock`, `heartbeat.json`, `sentinel-state.json`, `backup.prom`, `logs/`; warns if anything it does not know survives |
| 3 | Local snapshots | contract-named subvolumes under each marker-declared root |
| 4 | Pin files | `.last-external-parent-{LABEL}` (+ legacy form) in each snapshot dir |
| 5 | External drive | contract-named snapshots + `.urd-drive-token` under the drive's snapshot root, then emptied per-subvolume dirs (`--drive` only) |
| 6 | Sudoers | `/etc/sudoers.d/urd` (removed, never edited — absence is the pre-Urd state) |
| 7 | systemd | `urd-backup.timer`, `urd-backup.service`, `urd-sentinel.service`: disabled, unit files removed, `daemon-reload` + `reset-failed` |
| 8 | Completions | the prescribed install paths (below) |
| 9 | Binary | `--full` only: `~/.cargo/bin/urd` and `~/.local/bin/urd` |

Categories the current run does not cover are reported as honest skips, never
silently omitted — an external drive left unreset would contaminate the next
scenario with foreign urd data.

**Completions on the staging machine** must be installed to these paths, so the reset
can honestly unwind them (`urd completions` writes to stdout; the lab prescribes where
it lands): `~/.local/share/bash-completion/completions/urd`, `~/.zfunc/_urd`,
`~/.config/fish/completions/urd.fish`.

### A full reset, worked

```bash
# 1. Preview — always. Read every line; anomalies mean something needs a look.
scripts/staging-reset.sh --drive <uuid>

# 2. Execute (type the drive label when prompted).
scripts/staging-reset.sh --drive <uuid> --apply

# 3. Verify the blank slate.
urd status                      # must fail: no config
ls ~/.config/urd ~/.local/share/urd 2>&1        # both gone
systemctl --user list-units 'urd-*'             # none
sudo ls /etc/sudoers.d/urd 2>&1                 # gone
findmnt <drive-mount> && ls -a <drive-mount>/.snapshots 2>&1   # no urd artifacts
```

## Observation capture

Every field-test run produces one report:
`docs/99-reports/YYYY-MM-DD-encounter-field-test-NN.md` (local-only). Capture the
terminal with `script(1)` — start it before the first command the scenario calls for.

Template:

```markdown
# Encounter field test NN — <scenario name>

- **Date / urd version / commit:**
- **Scenario:** <id + one line>
- **Timings:** time-to-config · question count · time-to-first-snapshot · time-to-seal
- **Transcript:** (script(1) output, attached or inlined)
- **Friction log:** hesitations, wording that fell flat, questions the tester could
  not answer, anything reached for that did not exist
- **Generated urd.toml:** (verbatim)
- **Seal outcome:** (`urd status` output after)
- **Verdict:** would a stranger have made it through, and does the config match what
  they would have wanted?
```

## Scenario matrix

Minimum coverage; run the full matrix per built seal-path UPI and before release.

| # | Scenario | Expected behavior |
|---|----------|-------------------|
| 1 | Golden path — fresh system, empty unlocked external btrfs drive | Full journey: discovery → conversation → runestone → carve → earn → seal |
| 2 | No external drive | Honest-fate exit: everything recorded, the gap named, one come-back command |
| 3 | External drive present but LUKS-locked | Named-as-absent honesty: one sentence naming the locked drive, zero-drive derivation, "unlock it with your file manager, then run `urd init` again" — no unlock flow |
| 4 | Sudo declined at the earning | "Configured but unsealed" state; `urd status` names it and points at `urd init` |
| 5 | Quit mid-conversation, run again | Clean exit, nothing persisted; returning starts over |
| 6 | Config already exists | The refusal: names the existing file and the two paths forward; no override flag |
| 7 | Non-TTY invocation | Pointer sentence + distinct non-zero exit; never a conversation |
| 8 | Expert skip | `--config <path>` honored; the Encounter only ever offers |

## The findings loop

```
field test → report → triage → journal
```

Triage each finding into exactly one of:

- **Pre-build** (the design was wrong): amend the relevant design capsule directly.
- **Post-build** (the code is wrong): file a GitHub issue.
- **Wording** (the sentence fell flat): add to the voice-rewrite list — the
  post-arc voice pass consumes it; do not wordsmith mid-arc.

Notable lessons go to a journal entry. The UPI 078 registry row links every
field-test report, so the arc's evidence trail stays one click from the UPI table.

**Cadence:** first use is *before* the Encounter is built — capture discovery parser
fixtures (`lsblk -J`, `findmnt`) and rehearse the manual setup path to baseline
today's friction; that baseline is the number the Encounter must beat. Then one full
matrix pass per built seal-path UPI, and a final full pass before release.

## Non-goals

No VM or loopback-image harness (automation may come later; the physical machine is
the ground truth). No CI integration. No distro other than Fedora. No multi-machine
scenarios.
