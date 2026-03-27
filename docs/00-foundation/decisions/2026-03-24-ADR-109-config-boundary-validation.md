# ADR-109: Config-Boundary Validation

> **TL;DR:** All user-provided paths and names are validated once at config load time.
> After validation, internal code trusts the values. This constrains the sudo attack
> surface to a single auditable validation point rather than scattered checks throughout
> the codebase.

**Date:** 2026-03-22 (added during Phase 1 hardening; formalized 2026-03-24)
**Status:** Accepted
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
- [Phase 1 hardening review](../../99-reports/2026-03-22-phase1-hardening-review.md) —
  path validation introduced
- [Phase 3.5 adversary review](../../99-reports/2026-03-22-arch-adversary-phase35.md) —
  "Every path that reaches `sudo btrfs` has been validated"
