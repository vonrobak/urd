---
upi: "004"
status: proposed
date: 2026-04-02
---

# Design: TokenMissing Safety Gate (UPI 004)

> **TL;DR:** When a drive has no token file but SQLite already stores a token for that
> label, treat it as suspicious — not benign. This closes the exact gap that let a cloned
> drive pass through verification during v0.8.0 testing.

## Problem

`verify_drive_token()` in `drives.rs:282` returns `TokenMissing` when a drive has no
`.urd-drive-token` file. This is treated as benign everywhere — backup proceeds, and
`maybe_write_drive_token()` writes a new token after the first successful send.

The problem: when SQLite already has a stored token for that drive label, a missing token
file means either (a) the token was deleted, or (b) this is a different physical drive.
The v0.8.0 test (T2.3-T2.8) proved scenario (b): a cloned drive mounted at the original's
path, had no token file, and Urd treated it as a new drive. If a backup had run:

1. Sends would target the wrong physical drive under the wrong label
2. `maybe_write_drive_token()` would write a new token, overwriting the SQLite record
3. When the real drive returned, its on-disk token would no longer match SQLite → `TokenMismatch`
4. The real drive would be blocked from sends — the victim, not the impostor

Source: F2.3 in test report, Steve review item #1 (score: 95/100).

## Proposed Design

### New `DriveAvailability` variant

Add `TokenExpectedButMissing` to the `DriveAvailability` enum in `drives.rs:12`:

```rust
pub enum DriveAvailability {
    Available,
    NotMounted,
    UuidMismatch { expected: String, found: String },
    UuidCheckFailed(String),
    TokenMismatch { expected: String, found: String },
    TokenMissing,                    // No token, no SQLite record (genuine first use)
    TokenExpectedButMissing,         // No token, but SQLite has one (suspicious)
}
```

### Change in `verify_drive_token()`

Current flow (drives.rs:282-339):
1. Read token from drive → `Ok(None)` (missing) → return `TokenMissing`
2. (Never reaches SQLite check)

New flow:
1. Read token from drive → `Ok(None)` (missing)
2. Check SQLite: `state.get_drive_token(&drive.label)`
3. If SQLite has no record → return `TokenMissing` (genuine first use, benign)
4. If SQLite has a record → return `TokenExpectedButMissing` (suspicious)

### Change in `commands/backup.rs`

The token verification block (lines 154-186) currently only filters on `TokenMismatch`.
Add `TokenExpectedButMissing` to the filter:

```rust
if let drives::DriveAvailability::TokenMismatch { .. }
    | drives::DriveAvailability::TokenExpectedButMissing =
    drives::verify_drive_token(drive, db)
{
    // Log warning, add to mismatched set
}
```

The warning message for `TokenExpectedButMissing` should be:

"Drive {label} is mounted but has no identity token — Urd has seen this drive before
with a different token. This may indicate a drive swap or clone. Sends to {label} are
blocked. If this is a new or reformatted drive, run `urd drives adopt {label}` to
accept it."

(The `urd drives adopt` command is designed separately as UPI 009. Until it's built,
the message can say "remove the drive's token record from the state database" or simply
direct the user to `urd doctor` for guidance.)

### Change in `plan.rs`

Add `TokenExpectedButMissing` to the skip-reason generation (around line 222-225 where
`TokenMissing` currently proceeds):

```rust
DriveAvailability::TokenExpectedButMissing => {
    skipped.push((
        subvol.name.clone(),
        format!("drive {} token expected but missing — possible swap", drive.label),
    ));
    continue;
}
```

### Change in `output.rs`

Add a new pattern to `SkipCategory::from_reason()` for the new skip reason string.
Map to `SkipCategory::Other` (or add a new `SkipCategory::IdentitySuspect` if the
rendering should differ — but `Other` is sufficient for now).

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `drives.rs` | Add `TokenExpectedButMissing` variant; change `verify_drive_token()` flow | Unit test: token missing + SQLite has record → `TokenExpectedButMissing`; token missing + no SQLite record → `TokenMissing`; existing tests unchanged |
| `commands/backup.rs` | Add `TokenExpectedButMissing` to filter | Integration-level: mock drive with no token, SQLite with stored token → sends blocked |
| `plan.rs` | Add skip reason for `TokenExpectedButMissing` | Unit test: planner generates correct skip reason |
| `output.rs` | Add pattern to `SkipCategory::from_reason()` | Unit test: new reason string maps correctly |

## Effort Estimate

Patch tier. ~0.5 session. Four focused changes, all in existing code paths. No new
modules, no new data flows. The `verify_drive_token()` change is the core — the rest
are downstream propagation.

## Sequencing

1. `drives.rs` — enum variant + verify logic (core fix)
2. `plan.rs` — skip reason generation
3. `commands/backup.rs` — filter expansion
4. `output.rs` — skip category mapping
5. Tests for each

Risk-first: the `drives.rs` change is where the logic lives. Get it right first.

## Architectural Gates

None. This extends an existing enum and tightens an existing verification flow. No new
public contracts. No ADR needed — the change aligns with ADR-107 (fail-open backups,
fail-closed deletions): we're making sends fail-closed when identity is uncertain, which
matches "deletions fail closed" in spirit (sending to the wrong drive is destructive).

## Rejected Alternatives

**Always require token for sends (no `TokenMissing` at all).** Rejected because this
breaks the first-send bootstrapping: a new drive has no token, and the first successful
send writes one. The `TokenMissing` → benign path is correct for genuine first use.

**Check token at Sentinel mount time instead of backup time.** (Idea 1B from brainstorm.)
This is a better UX — earlier feedback — but it's a larger change involving Sentinel
state and notification infrastructure. The backup-time gate is sufficient for data safety.
Sentinel-time verification can come later as a UX enhancement.

**Use LUKS UUID as secondary fingerprint.** (Idea 1D.) Over-engineered for this problem.
The token system already provides unique identity; it just needs to enforce what it knows.

## Assumptions

1. `state.get_drive_token()` returns `Ok(None)` when no record exists and `Ok(Some(token))`
   when one does. (Verified: state.rs:542-557.)
2. The SQLite database is available when `verify_drive_token()` runs. (True: it's passed
   as `&StateDb` parameter.)
3. A drive that has been used with Urd will always have its token stored in SQLite.
   (True after the first successful send to that drive via `maybe_write_drive_token()`.)

## Resolved Decisions (from /grill-me)

**004-Q1: Hard stop, no `--force` override.** Resolution path is `urd drives adopt`
(UPI 009). A flag on `urd backup` creates muscle-memory risk — users already reach for
`--force-full` and might reflexively add `--force` without understanding the identity
implications. The `adopt` command is deliberate: a conscious acknowledgment.

**004-Q2: Plan command does post-plan token verification in the command layer.**
The planner is pure (ADR-100/108) and has no `StateDb` access. The plan command
(`plan_cmd.rs`) already has `StateDb` — after `plan::plan()` returns, iterate mounted
drives and call `verify_drive_token()`. Add warnings to `PlanOutput` for any
`TokenExpectedButMissing` drives. Voice renders as a prominent warning block.
**Must be clearly commented in code** explaining the rationale: planner purity boundary
requires token checks in the command layer, not the planner.

**004-Q3: Interim error message points to `urd doctor`** until UPI 009 ships.
Message: "Drive {label} is mounted but missing its identity token. Urd has previously
sent to a drive with this label — this may be a different physical drive. Sends to
{label} are blocked. Run `urd doctor` for guidance."
**Comment in code** marking this as placeholder for `urd drives adopt {label}` (UPI 009).

**Q2 (self-answered): `maybe_write_drive_token()` is safe.** Sends are blocked, so
the function never runs (only fires after successful sends). No change needed.

**Q3 (scoped out): Doctor token checks are UPI 008 scope.** Backup-time gate is
sufficient for this design.
