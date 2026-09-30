---
type: ADR
title: Config-Boundary Validation
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-109: Config-Boundary Validation

> **TL;DR:** All user-provided paths and names are validated once at config load time.
> After validation, internal code trusts the values. This constrains the sudo attack
> surface to a single auditable validation point rather than scattered checks throughout
> the codebase.

**Date:** 2026-03-22 (added during Phase 1 hardening; formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-the-validation-points-beyond-configvalidate))
**Supersedes:** None (crystallized across Phase 1 hardening and Phase 3.5 reviews)

## Context

Urd passes user-configured paths to `sudo btrfs subvolume delete`, `sudo btrfs send`,
and `sudo btrfs receive`. A path traversal bug (e.g., `../../../important-data` in a
snapshot name) could cause deletion of arbitrary data with root privileges.

The Phase 1 hardening review identified this risk and introduced validation functions.
The Phase 3.5 adversary review confirmed the approach: "Every path that reaches `sudo
btrfs` has been validated."

## Decision

### Validate at config load, trust afterward

`Config::validate()` runs once when the config is loaded. It enforces:

| Check | Rejects | Why |
|-------|---------|-----|
| Paths are absolute | `./relative/path` | Prevents CWD-dependent behavior |
| No `..` components | `/snapshots/../etc/shadow` | Prevents path traversal |
| Names contain no path separators | `foo/bar` as subvolume name | Prevents directory escape |
| Names contain no null bytes | `foo\x00bar` | Prevents C-string truncation |
| Drive labels are filesystem-safe | Labels with `/` or `\` | Labels become path components |
| UUIDs are unique (case-insensitive) | Two drives with same UUID | Prevents identity confusion |

### Config file is trusted input

The config file (`~/.config/urd/urd.toml`) is owned by the user and writable only by
them. It is treated as trusted input — if the user can write arbitrary content to their
own config file, they already have the access that a config injection would provide.

This means validation is for **correctness** (catching typos, malformed paths), not for
**security against the config author**. The security boundary is between the config and
the system — validated paths are safe to pass to sudo commands.

### Structural vs runtime errors

Config validation catches **structural errors** — authoring mistakes that make the config
meaningless. These are hard failures: Urd refuses to start.

**Runtime conditions** (drive not mounted, filesystem below `min_free_bytes`, source path
missing) are not config errors — the config is correct but the world isn't ready. These
are handled at runtime by the executor, which isolates failures per-unit and produces a
structured result describing what was skipped and why (ADR-111).

The distinction: "this config is *wrong*" vs "this config is *right but the world isn't
ready*." Config validation owns the first; the executor owns the second.

### No re-validation in hot paths

After `Config::validate()` succeeds, modules that consume config values (planner, executor,
btrfs.rs) do not re-validate paths. This is intentional — re-validation in hot paths is
both wasteful and prone to inconsistency (different modules validating different subsets
of rules).

### Filesystem-derived values get separate validation

Values read from the filesystem (snapshot names from directory listings, pin file contents)
are validated at their own boundary — when they are parsed. `SnapshotName::new()` validates
format. Pin file contents are validated against the expected snapshot name pattern. These
are separate from config validation because they come from a different trust boundary.

## Consequences

### Positive

- The sudo attack surface is auditable in one function (`Config::validate()`)
- Path construction throughout the codebase is simple — `join()` on validated components
  without defensive checks at every call site
- Config errors are caught early (at startup), not mid-backup when a malformed path
  reaches a sudo command

### Negative

- If validation is incomplete (a new field is added without validation), the gap silently
  passes invalid values through. Mitigation: adversary reviews check new config fields
  against the validation function.
- Snapshot names from the filesystem could theoretically contain adversarial values if
  someone manually creates a snapshot with a malicious name. Mitigation: `SnapshotName`
  parsing rejects names that don't match the expected format.

### Constraints

- New config fields that become path components or command arguments must be added to
  `Config::validate()`.
- `btrfs.rs` must pass paths as `&Path` to `Command::arg()`, never as stringified
  arguments. This preserves non-UTF-8 paths and prevents shell injection.
- `urd get` has its own path validation (normalize, traversal check, starts_with) because
  it accepts user-provided paths at runtime, not from config.

## Related

- ADR-101: BtrfsOps trait (the module where validated paths reach sudo commands)
- ADR-111: Config system architecture (structural vs runtime error distinction, new fields)
- Phase 1 hardening review (`docs/99-reports/2026-03-22-phase1-hardening-review.md`) —
  path validation introduced
- Phase 3.5 adversary review (`docs/99-reports/2026-03-22-arch-adversary-phase35.md`) —
  "Every path that reaches `sudo btrfs` has been validated"

## Amendment 2026-09-30: the validation points beyond `Config::validate`

The rule stands: user-provided values are validated once, at a boundary, and trusted
afterward. The claim under Positive that "the sudo attack surface is auditable in one
function (`Config::validate()`)" is false. There are four boundaries. Each validates for
the medium the value is about to enter, and a value can pass one and be refused by the
next.

| Boundary | Where | Validates for | On a bad value |
|---|---|---|---|
| Config load | `Config::validate` (`src/config/validate.rs`), called from `Config::load` after `parse_versioned` | Filesystem paths and path components: the checks in the table above, plus `"` and newline refused in names (`validate_name_safe`) | Refuses the config; Urd does not start |
| Sudoers render | `sudoers::render_sudoers` / `expected_grant_lines` (`src/sudoers.rs`) | A sudoers line: control characters, `#`, and non-UTF-8 refused in every value; a snapshot scope with fewer than two path components refused (`scope_deep_enough`); the username checked against sudoers' User_List syntax and the reserved word `ALL` | Renders nothing (`SudoersRefusal`); the seal installs nothing |
| Unit render | `systemd_units::checked_exe` (`src/systemd_units.rs`) | A systemd `ExecStart=` line: the resolved binary path must be UTF-8, absolute, and free of whitespace and control characters | Renders nothing (`UnitsRefusal`); the seal's units stage reports it |
| CLI arguments | `cli_validation::require_known_subvolume` (`src/cli_validation.rs`); `urd get`'s own traversal check (`commands/get.rs`) | A `--subvolume NAME` must name a configured subvolume; a `urd get` path must not traverse out of its snapshot | Refuses the command with the configured names and a nearest-match suggestion |

The sudoers and unit boundaries **refuse rather than escape** (ADR-120). A config value
that is a perfectly good path can still change the meaning of a sudoers line. An escaped
newline there is a line continuation, so no escaping discipline makes such a value safe.
Refusal is total: one bad value means nothing is rendered, and the message names the
value. Those two boundaries sit in pure modules so that the check and the artifact
cannot disagree. The same render is what the seal installs and what `urd doctor` diffs
against.

`cli_validation.rs` exists because the planner trusts `filters.subvolume` to name a real
subvolume. An unknown name used to match the empty set and report "Nothing to do." That
trust is established at the CLI boundary, before the planner runs, as this ADR's rule
requires.

The corrected Positive consequence: **every value that reaches a privileged command or a
privileged file is checked at exactly one boundary for that medium, in a pure function
that renders nothing on refusal.** Auditing the sudo surface means reading
`Config::validate` for the paths `btrfs.rs` receives, and `sudoers.rs` for the grant
that authorizes them. The Constraint "new config fields that become path components or
command arguments must be added to `Config::validate()`" still holds. A field that also
reaches the sudoers grant or a unit file must also pass that module's checks.
