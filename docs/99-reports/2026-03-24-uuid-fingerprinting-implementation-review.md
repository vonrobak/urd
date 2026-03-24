# UUID Drive Fingerprinting — Implementation Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 2a implementation — UUID drive fingerprinting
**Files reviewed:** `src/drives.rs`, `src/config.rs`, `src/plan.rs`, `src/commands/backup.rs`,
`src/commands/plan_cmd.rs`, `config/urd.toml.example`
**Reviewer:** Architectural Adversary (Claude)
**Commit:** uncommitted changes on master (base: 56d25fc)
**Tests:** 214 passing, 0 failing

## Executive Summary

Solid implementation of a targeted safety feature. The core logic — `DriveAvailability` enum,
`findmnt`-based UUID detection, planner integration via the `FileSystemState` trait — is correct
and well-integrated. Two significant findings: (1) a redundant `/proc/mounts` read in
`drive_availability` that `findmnt` already handles, and (2) the `warn_missing_uuids` function
calls `findmnt` for every mounted drive on every run, which adds subprocess overhead to the
critical path. One moderate finding about UUID uniqueness validation being case-sensitive while
comparison is case-insensitive. Everything else is clean.

## What Kills You

**Silent sends to the wrong drive.** This is unchanged from the design review. The
implementation correctly blocks sends when UUID mismatches are detected. The distance from
"UUID mismatch" to "wrong drive receives snapshots" is now one configuration omission (user
hasn't set UUID). The warning system addresses this by nudging users toward adding UUIDs.

The implementation does not introduce any new path to the catastrophic failure mode. It only
adds a gate that was previously absent.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Core logic correct; one redundant check, one case-sensitivity inconsistency |
| Security | 5 | No new trust boundaries; UUID from `findmnt` not used in privileged operations |
| Architectural Excellence | 4 | Clean integration into existing trait pattern; planner purity preserved |
| Systems Design | 3 | Subprocess call on every run; TOCTOU window (acceptable); warning verbosity |
| Rust Idioms | 4 | Good enum design, proper `#[must_use]`, clean match exhaustiveness |
| Code Quality | 4 | Good test coverage; backward-compatible mock design |

## Design Tensions

### Tension 1: Redundant Mount Check in `drive_availability`

`drive_availability()` calls `is_path_mounted()` first, then `get_filesystem_uuid()` which
invokes `findmnt`. But `findmnt` itself will tell you if the path isn't a mount point — it
returns a non-zero exit code or empty output. The `is_path_mounted()` call reads `/proc/mounts`
and parses it, only to have `findmnt` do effectively the same check moments later.

The `is_path_mounted()` pre-check has one upside: it avoids spawning a subprocess for unmounted
drives. Since most runs will have 2-3 drives and 0-1 mounted, the pre-check saves 1-2 process
spawns. This is a reasonable optimization — the pre-check is cheap (read a file, scan lines)
and the subprocess is not (fork/exec). **The tension is correctly resolved**, but deserves a
comment explaining why both checks exist.

### Tension 2: `warn_missing_uuids` Calls `findmnt` Per Drive

`warn_missing_uuids()` is called after `plan()` in both `backup.rs` and `plan_cmd.rs`. For each
mounted drive without a UUID, it calls `get_filesystem_uuid()` — spawning a `findmnt` subprocess.
This means a drive that already had `drive_availability()` called during planning (which also
calls `findmnt` if UUID is configured) might get `findmnt` called again.

More importantly, for drives *without* UUID configured, `drive_availability()` returns
`Available` without calling `findmnt`, but then `warn_missing_uuids()` calls `findmnt` to
show the detected UUID in the warning. This is correct but means `findmnt` is called once per
mounted-but-unconfigured drive on every backup run, purely for the warning message.

**Recommendation:** Accept this for now — it's at most 3 subprocess calls per run, and the
warning is valuable during the transition period. But add a comment noting that once all drives
have UUIDs configured, this function becomes a no-op. Long term, this could be a one-time
`urd verify` check rather than per-run.

### Tension 3: Default Method on `FileSystemState` Trait

`is_drive_mounted()` now has a default implementation that calls `self.drive_availability()`.
This is elegant — callers like `awareness.rs` that only need a bool continue to work. But it
creates a subtle contract: any implementor of `FileSystemState` must implement
`drive_availability()`, and `is_drive_mounted()` becomes a derived convenience method.

The `MockFileSystemState` handles this correctly — it implements `drive_availability()` and
inherits the default `is_drive_mounted()`. But the `mounted_drives` field on the mock is now
semantically part of `drive_availability()`, not `is_drive_mounted()`. The naming is slightly
misleading but acceptable given backward compatibility.

**The tension is correctly resolved.** The default method avoids breaking existing callers while
making the richer signal available to the planner.

## Findings

### Finding 1: UUID Uniqueness Validation Is Case-Sensitive, Comparison Is Not (Significant)

**What:** In `config.rs` line 299, `seen_uuids.insert(uuid)` compares UUIDs using `String`
equality (case-sensitive). But in `drives.rs` line 39, UUID comparison uses
`eq_ignore_ascii_case()`. This means two drives with `uuid = "ABC..."` and `uuid = "abc..."`
would pass config validation (different strings) but refer to the same physical drive.

**Consequence:** A user could accidentally configure two drives with the same UUID in different
cases. Both would pass validation. Both would match the same physical drive. The planner would
send to both logical drives, but only one physical destination exists. Sends would probably
succeed (same filesystem) but the user's mental model would be wrong — they'd think they have
two copies when they have one.

**Fix:** Normalize the UUID to lowercase before inserting into `seen_uuids`:

```rust
if !seen_uuids.insert(uuid.to_lowercase()) {
```

Or better, normalize at deserialization time so the rest of the code doesn't need to worry
about case.

### Finding 2: `warn_missing_uuids` Output Mixes with Plan Output (Moderate)

**What:** In `plan_cmd.rs`, `warn_missing_uuids()` writes to stderr between `plan::plan()` and
`run_with_plan()`. The warning uses `eprintln!` with raw formatting (`"  warning: ..."`). In
`backup.rs`, it's similarly placed after planning but before dry-run check.

**Consequence:** The warning output appears before the plan output (which goes to stdout), so
in a terminal they'll interleave depending on buffering. More importantly, the warning format
doesn't match the plan output's style (no color, no `[WARN]` prefix, manual indentation).
For daemon mode (non-TTY), this stderr output doesn't follow the JSON-on-stdout convention
that the presentation layer established.

**Fix:** Two options, either is fine:
1. Move the warning into the plan output as a note in the `skipped` list (keeps it in the
   structured data path). But this changes the semantics — it's not a skip, it's a config
   recommendation.
2. Use `log::warn!()` instead of `eprintln!()`. This respects log levels and format, and
   the daemon can filter/route it. The current backup command already uses `log::warn!` for
   similar purposes.

### Finding 3: TOCTOU Between Mount Check and UUID Check (Minor — Acceptable)

**What:** `drive_availability()` checks `is_path_mounted()`, then calls `get_filesystem_uuid()`.
Between these two calls, the drive could be unmounted. `findmnt` would then return an error or
empty output, which maps to `UuidCheckFailed`.

**Consequence:** A `UuidCheckFailed` skip reason when the real cause is "drive unmounted during
check." The skip message would be misleading.

**Why this is acceptable:** The window is milliseconds. If a drive is being unmounted during a
backup run, the subsequent `btrfs send` would fail anyway. The only cost is a slightly confusing
skip reason in a race condition that practically never occurs. Not worth adding complexity to
handle.

### Finding 4: `get_filesystem_uuid` Is Public but Called from Two Private Contexts (Minor)

**What:** `get_filesystem_uuid()` is `pub` and called from `drive_availability()` and
`warn_missing_uuids()`, both in the same module. No external callers exist.

**Consequence:** None currently. The function is well-designed and could be useful externally
(e.g., `urd verify` checking UUID consistency). Keeping it public is forward-looking and
reasonable.

**Verdict:** No change needed. Mentioning it only because public surface area should be
intentional, and here it is.

### Finding 5: Planner Integration via Exhaustive Match (Commendation)

**What:** The planner's drive loop uses `match fs.drive_availability(drive)` with explicit
arms for all four `DriveAvailability` variants. Each produces a distinct, actionable skip reason.

**Why this is good:** The exhaustive match means adding a new `DriveAvailability` variant (e.g.,
`DriveEncrypted` in the future) would cause a compile error in the planner, forcing the author
to decide how to handle it. This is the type system doing the work that discipline usually fails
at. Combined with distinct skip reasons, the user can tell the difference between "drive not
plugged in" (normal) and "wrong drive at mount point" (emergency).

### Finding 6: Backward-Compatible Mock Design (Commendation)

**What:** The `MockFileSystemState` keeps the existing `mounted_drives: HashSet<String>` field
and adds `drive_availability_overrides: HashMap<String, DriveAvailability>`. The mock's
`drive_availability()` checks overrides first, then falls back to the old `mounted_drives`
behavior.

**Why this is good:** All 155+ existing tests continue to work without modification. New
UUID-specific tests use the override mechanism. This is the right way to evolve a test
interface — additive, not breaking. The backward compat path is simple enough to verify by
reading (3 lines), and the override path lets new tests exercise all `DriveAvailability` states.

### Finding 7: Example Config Contains a Real UUID (Minor)

**What:** `urd.toml.example` line 59 contains `uuid = "647693ed-490e-4c09-8816-189ba2baf03f"`,
which is the actual UUID of a real drive on the developer's system.

**Consequence:** No security impact — filesystem UUIDs are not secrets. But it could be confusing
for a user who copies the example verbatim without changing it. They'd get UUID mismatches on
their drives, which would correctly prevent sends and produce clear error messages.

**Verdict:** This is actually fine — the error message will tell the user exactly what's wrong.
A fake UUID like `"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"` would fail TOML parsing if someone
tries it. Keeping a realistic-looking value is the right call.

## The Simplicity Question

**What could be removed?**

Nothing. The implementation is minimal for what it achieves:
- `DriveAvailability` enum: 4 variants, each load-bearing
- `get_filesystem_uuid()`: 30 lines, single subprocess call
- `drive_availability()`: 25 lines, clean state machine
- `warn_missing_uuids()`: 25 lines, migration affordance
- Config changes: 1 field + 1 validation rule
- Planner changes: replaced 5 lines with 25 lines (the match arms)
- Tests: 10 new tests across two modules

**What earns its keep?**
- The `DriveAvailability` enum earns its keep (safety-critical distinction between skip reasons)
- The `findmnt` approach earns its keep (no sudo, handles LUKS, single clean output)
- The mock backward compat earns its keep (155+ tests unchanged)
- The `warn_missing_uuids` earns its keep during transition (users need to discover the feature)

## Priority Action Items

1. **Fix case-insensitive UUID uniqueness validation** (Finding 1). Normalize to lowercase in
   `seen_uuids.insert()`. One-line fix, prevents a real misconfiguration scenario.

2. **Switch `warn_missing_uuids` from `eprintln!` to `log::warn!`** (Finding 2). Respects
   log levels and format conventions. Small change, better integration with daemon mode.

3. **(Optional) Add a comment in `drive_availability` explaining why the pre-check exists**
   (Tension 1). Future readers will wonder why we check `/proc/mounts` when `findmnt` does it.

## Open Questions

1. **Should `warn_missing_uuids` run on every backup, or only on `urd verify`?** Currently runs
   on every `plan` and `backup`. This is useful during transition but may become noise once the
   user has seen it several times and chosen not to act. Consider adding a "warned once" mechanism
   later, or moving to `urd verify` only.

2. **Should `urd status` show UUID status?** The status command uses `drives::is_drive_mounted()`
   directly (not through the trait). It could be enhanced to show `[UUID ✓]` or `[no UUID]` per
   drive. Natural follow-up, not needed for this PR.
