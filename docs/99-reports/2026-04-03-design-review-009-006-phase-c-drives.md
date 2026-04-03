---
upi: "009, 006"
date: 2026-04-03
---

# Architectural Adversary Review: Phase C — Give Drives a Face (UPI 009 + 006)

**Project:** Urd
**Date:** 2026-04-03
**Scope:** Implementation plan `docs/97-plans/2026-04-03-plan-009-006-phase-c-drives.md`
**Mode:** Design review (plan, pre-implementation)
**Commit:** 8c83c2b (master)

---

## Executive Summary

This is a clean, well-scoped plan for two additive features that don't touch the backup
critical path. The premise is sound, the module boundaries are correct, and the sequencing
is right. The one significant finding is a subtle ordering issue in the sentinel runner
that could cause the reconnection notification to fire *before* the drive's availability
has been confirmed — leading to false reconnection notifications for drives that fail UUID
or token checks. Everything else is moderate-to-minor.

## What Kills You

**Catastrophic failure mode: silent data loss via unintended snapshot deletion.**

Phase C is far from this failure mode. Neither UPI 009 nor UPI 006 touches retention,
the planner, the executor, or the btrfs command layer. `urd drives adopt` writes a token
file and a SQLite record — it doesn't create, delete, or modify snapshots. Drive
reconnection notifications are read-only observation. **Distance: 3+ bugs away from
catastrophe.** This is a safe feature to ship.

The closest risk vector is `urd drives adopt` writing a token to the wrong drive (user
error — adopting a drive that isn't what they think it is). This is mitigated by the
design's requirement that the drive must be mounted, which allows the user to visually
verify. The plan correctly checks `drive_availability()` before adopting.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Logic is sound. One ordering issue in sentinel runner (Finding S1). Step 2's adopt flow has a sequencing gap (Finding S2). |
| 2 | **Security** | 5 | No new privilege escalation. Token adoption is user-initiated, requires mounted drive, writes to user-accessible paths. No btrfs calls. |
| 3 | **Architectural Excellence** | 5 | Module boundaries respected throughout. Pure/I/O split maintained. New command module follows established pattern. |
| 4 | **Systems Design** | 4 | Good use of existing infrastructure. Suppression threshold for reconnection is well-placed. Absent duration proxy via `last_verified` is pragmatic. |

**Overall: 4.5 / 5 — Solid plan, ready to build with minor revisions.**

## Design Tensions

### 1. Reusing `DriveAvailability` enum for list display vs. introducing `TokenState`

The plan introduces a new `TokenState` enum for the list output, separate from
`DriveAvailability`. This is the right call. `DriveAvailability` is a protocol type
(drive-is-ok-to-send-to), not a display type. Mapping it to display concerns inside
the command handler keeps the domain type clean. The cost is a mapping function, which
is trivial. **Tension resolved correctly.**

### 2. `last_verified` as absent-duration proxy vs. tracking disconnect timestamps

The plan uses `last_verified` from SQLite as a proxy for "how long was the drive gone,"
avoiding new state in the sentinel. This is pragmatically correct for v1. The only edge
case: `last_verified` is touched on every backup, not on every sentinel assessment. So
if backups aren't running (the very scenario that makes drives matter), `last_verified`
could be stale even while the drive was connected. The duration would then overreport
absence. This is acceptable — overreporting duration is conservative (it motivates the
user to act), and the alternative (tracking disconnect timestamps in sentinel state)
adds complexity for minimal gain. **Tension resolved correctly for v1.**

### 3. `NotifyDriveReconnected` as a separate action vs. folding into Assess

The plan adds a new sentinel action rather than having the Assess handler detect
reconnections by comparing mounted_drives across assessments. This is architecturally
right — the state machine should express intent explicitly, not have the runner infer
intent from state diffs. It keeps sentinel.rs as the single source of "what events
matter" rather than scattering detection logic into the runner. **Tension resolved
correctly.**

## Findings

### S1: Sentinel runner action ordering — reconnection notification may fire for unavailable drives (Significant)

**What:** The plan's Step 7 emits `NotifyDriveReconnected` inside the `DriveMounted`
transition. But look at `detect_drive_events()` in sentinel_runner.rs:167-187 — it only
emits `DriveMounted` when `drive_availability(d) == DriveAvailability::Available`. This
means `NotifyDriveReconnected` only fires for truly available drives. So the plan is
actually correct here — but the *plan document* doesn't mention this, which means the
implementer might not verify the assumption.

**Actually, wait.** Re-reading `detect_drive_events()` line 172:

```rust
.filter(|d| drives::drive_availability(d) == DriveAvailability::Available)
```

This only checks mount + UUID, not tokens. A drive that passes UUID but has
`TokenExpectedButMissing` will still emit `DriveMounted` → `NotifyDriveReconnected`.
The user gets a cheerful "WD-18TB is back!" notification, and then when they run
`urd backup`, the drive is blocked due to token mismatch.

**Consequence:** False-positive "drive is back" notification when the drive is mounted
but identity-suspect. Not a data safety issue, but an anxiety-loop violation — the
notification says "run backup to catch up" but backup will refuse.

**Suggested fix:** In `execute_drive_reconnection_notification()`, check the drive's
token state before dispatching. If `TokenMismatch` or `TokenExpectedButMissing`, either
suppress the notification entirely or change the message to "WD-18TB is mounted but
needs identity verification. Run `urd drives adopt WD-18TB`." This keeps the pure
state machine simple (it doesn't need to know about tokens) and puts the intelligence
in the runner where it belongs.

### S2: Adopt flow — AlreadyCurrent check races with the adopt operation (Significant)

**What:** Step 2's adopt handler says:
1. Read on-disk token
2. If exists: store in SQLite → `AdoptedExisting`
3. If no token: generate, write, store → `GeneratedNew`
4. Check if already current → `AlreadyCurrent`

Step 6 (the check) happens *after* steps 4-5 have already written the token. By the
time you check "is it already current?", you've already made it current. The check must
happen *before* the write.

**Suggested fix:** Read both on-disk token and SQLite token first. If they match, return
`AlreadyCurrent` immediately without writing anything. Only proceed to adopt if they differ
or one is missing.

```
1. Find DriveConfig, check availability
2. Read on-disk token
3. Read SQLite token
4. If on-disk == SQLite (both present, values match): AlreadyCurrent
5. If on-disk present, differs from SQLite: store on-disk → AdoptedExisting
6. If no on-disk token: generate, write, store → GeneratedNew
```

### M1: `UuidCheckFailed` not handled in list display mapping (Moderate)

**What:** Step 2 maps `DriveAvailability` to display states, covering `Available`,
`TokenMissing`, `TokenMismatch`, and `TokenExpectedButMissing`. But `drive_availability()`
can also return `UuidMismatch` and `UuidCheckFailed`. The plan says "If NotMounted or
UuidMismatch: error" for adopt, but for list, these states aren't mapped.

A drive that's mounted but has a UUID mismatch should show `identity suspect` in the
STATUS column, not silently fall through. A `UuidCheckFailed` (findmnt not available,
non-UTF-8 path) should show `connected (UUID unverified)` or similar.

**Suggested fix:** Add explicit handling for `UuidMismatch` and `UuidCheckFailed` in
the list handler. Map them to appropriate STATUS and TOKEN columns. Don't call
`verify_drive_token()` for these states — the drive doc says verify is only valid after
`Available`.

### M2: Missing test for `urd drives adopt` when token is already current (Moderate)

**What:** The test list (Step 2) includes "no token → generates new" and "token on
drive → adopts existing" but no test for the `AlreadyCurrent` path. This is the most
common production scenario — user runs `adopt` on a drive that's already fine (either
by accident or to verify). Missing test coverage on the happy no-op path.

**Suggested fix:** Add test: "Adopt: on-disk token matches SQLite → returns
AlreadyCurrent, no writes."

### M3: `run_drives_adopt` signature missing `output_mode` parameter (Moderate)

**What:** Step 2 defines `run_drives_adopt(config: &Config, label: &str) -> Result<()>`
but Step 3 defines `render_drives_adopt(data: &DriveAdoptOutput, mode: OutputMode)`.
The adopt handler needs `output_mode` to know whether to render interactive or JSON.

Looking at the main.rs match arm in Step 4:
```rust
Some(cli::DrivesAction::Adopt { label }) => {
    commands::drives::run_drives_adopt(&config, &label)
}
```

This doesn't pass `output_mode`. Compare with the list handler which does.

**Suggested fix:** Add `output_mode: OutputMode` to `run_drives_adopt` signature and
pass it from main.rs.

### C1: Plan correctly identifies shared `StateDb` method (Commendation)

Step 1b adds `get_drive_token_last_verified` as a shared dependency for both UPIs.
Recognizing this shared need upfront and sequencing it early prevents the common
mistake of building one feature, then discovering you need to re-touch a module for
the other. The method is placed in `state.rs` where it belongs (CLAUDE.md: "Record
history in SQLite"), not in the command handler or the notification builder. Clean
module discipline.

### C2: Correct use of `has_initial_assessment` guard (Commendation)

The plan's guard on reconnection notifications (006-Q1) uses `has_initial_assessment`
correctly. Without this guard, first-boot drive discovery would fire notifications for
every configured drive — the user would get 3 "WD-18TB is back!" notifications on
every sentinel restart. The plan explicitly calls out why this guard exists and what
it prevents. This is the kind of guard that's easy to forget in implementation and
painful to debug after deployment.

### C3: Two-step adopt protocol — "on-disk wins, generate if absent" (Commendation)

The adopt semantics are exactly right for a backup tool. The principle "trust what's
physically there" means adopting a cloned drive (no token) generates a new identity,
while adopting a drive with an existing token (moved from another system) preserves
it. The user doesn't need to understand the difference — `adopt` does the right thing
regardless. The design doc's 009-Q1 decision was well-resolved and the plan implements
it faithfully.

## Also Noted

- Step 5 says "find the existing message in voice.rs" but the actual message is in
  `commands/backup.rs:175-182` (log::warn) and `plan.rs:232` (skip reason string). The
  voice.rs search will come up empty. Update the plan to point at the right files.
- The `DriveRole` enum has `Primary`, `Offsite`, `Test` — the design spec shows "default"
  as a role value. Either the design spec is wrong (likely) or there's a `Default` variant
  needed. Check against actual config.
- Token values are included in `DriveAdoptOutput`. These don't contain sensitive
  information (they're UUIDs, not secrets), but consider whether daemon-mode JSON output
  of token values is desirable. Probably fine — they're on disk already.

## The Simplicity Question

**What could be removed?** Not much. This is already a minimal feature set: list and adopt,
no other subcommands. The type system is proportional to the problem — enums for display
states, structs for output. No abstractions beyond what rendering requires.

**What earns its keep?** The `TokenState` enum (7 variants) might seem like a lot, but
each variant maps to a distinct user-visible state that requires different UX treatment
(color, symbol, message). Collapsing them would hide information the user needs. The
`DriveAdoptOutput` with `AdoptAction` enum is similarly justified — the three adopt
outcomes produce different messages.

**If I had to cut 20%:** I'd defer the `AlreadyCurrent` path in adopt — just always write
the token, even if it's the same. Idempotent writes are simpler than conditional writes.
But the no-op message ("already adopted") is genuinely useful UX feedback, so I'd keep it.

## For the Dev Team

Priority order:

1. **Fix adopt ordering (S2).** Move the "already current" check before the write. Read
   both on-disk and SQLite tokens first. Compare. Only write if they differ or one is
   missing. This is a logic fix in Step 2.

2. **Handle identity-suspect drives in reconnection notification (S1).** In
   `execute_drive_reconnection_notification()`, check token state before dispatching.
   If token mismatch/expected-but-missing, change the notification message to direct
   the user to `urd drives adopt` instead of `urd backup`. File: `sentinel_runner.rs`.

3. **Map UUID states in list handler (M1).** Add `UuidMismatch` and `UuidCheckFailed`
   handling to the list command. Show them as status variants, don't call
   `verify_drive_token()` for them. File: `commands/drives.rs`.

4. **Add `AlreadyCurrent` test (M2).** Test that adopt with matching on-disk and SQLite
   tokens returns `AlreadyCurrent` without writing. File: `commands/drives.rs` tests.

5. **Add `output_mode` to adopt handler (M3).** Pass through from main.rs. Files:
   `commands/drives.rs`, `main.rs`.

6. **Fix Step 5 file references.** The TokenExpectedButMissing message lives in
   `commands/backup.rs:175-182` and `plan.rs:232`, not voice.rs.

## Open Questions

1. **DriveRole "default" in design spec.** The design shows `default` as a ROLE column
   value, but `DriveRole` only has `Primary`, `Offsite`, `Test`. Is "default" the
   display name for drives with no explicit role, or does it need a new variant?

2. **Should `urd drives` require config?** Currently it's in main.rs Strategy C
   (mandatory config load). This seems right — drives come from config. But a future
   "no config yet" state (pre-encounter) might want `urd drives` to say "no drives
   configured — run `urd setup`." Not a blocker for Phase C, but worth noting for Phase D.
