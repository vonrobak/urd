# Implementation Review: HSD-A — Drive Session Tokens + Chain Health as Awareness Input

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-29
**Scope:** Implementation review of HSD-A (Session A of Hardware Swap Defenses)
**Mode:** Implementation review (6 dimensions)
**Commit:** post-`b7c3e59` (working tree)
**Reviewer:** arch-adversary

---

## Executive Summary

A clean implementation that closely follows the design and plan documents. The drive
token infrastructure (`drives.rs`, `state.rs`, `executor.rs`) is well-structured with
proper fail-open semantics. The chain health computation in `awareness.rs` is genuinely
pure and the `ChainStatus`/`ChainBreakReason` enum design is a significant improvement
over stringly-typed alternatives. The status command simplification confirms the design
payoff — duplicate logic was eliminated.

There are no catastrophic findings. The main risks are in the not-yet-wired integration
points: `verify_drive_token()` exists but is not called in any production code path yet
(marked `#[allow(dead_code)]` for HSD-B), meaning the full defense is not yet active.
One moderate finding in the `store_drive_token` SQL behavior affects the self-healing
path's idempotency. Two minor findings relate to token file parsing robustness and
test coverage gaps.

## What Kills You

**Catastrophic failure mode:** Silent sends to a swapped drive filling it and causing
ENOSPC. This implementation adds the infrastructure to detect swaps but does not yet
wire it into production code paths. The catastrophic risk is unchanged from before HSD-A
until HSD-B connects `verify_drive_token()` into `commands/backup.rs` and the sentinel
runner.

**Specific catastrophic failure checklist results:**

1. **Can `TokenMismatch` silently degrade to `Available`?** Not in the current code.
   The `verify_drive_token()` function has exactly three paths that return `Available`:
   (a) tokens match, (b) drive has token but SQLite has none (self-healing), (c) read
   errors (fail-open per ADR-107). Path (c) is the intentional fail-open path and is
   logged. Path (b) is the self-healing path — correctly returns `Available` because
   the drive's token becomes the new reference. **However**, `verify_drive_token()` is
   not yet called in any production code path (marked `dead_code`). The defense is
   dormant. **No current risk, but the wiring in HSD-B is the critical moment.**

2. **Can the token write in `executor.rs` fail and leave inconsistent state?** Partially.
   If `write_drive_token()` succeeds but `store_drive_token()` fails (SQLite error),
   the drive has a token that SQLite doesn't know about. On the next `verify_drive_token()`
   call, this hits the self-healing path (drive has token, SQLite has none) and stores
   it. **Self-healing works correctly here.** If `write_drive_token()` fails (read-only
   drive), the function returns early with a warning and no token is written anywhere.
   **Consistent: no token on drive, no token in SQLite.**

3. **Does chain health computation handle all pin file states?** Yes. The
   `assess_chain_health()` function handles: (a) empty external snapshots, (b) pin
   file exists with parent found both locally and on drive, (c) pin exists but parent
   missing locally, (d) pin exists but parent missing on drive, (e) no pin file, (f)
   pin read error. All six cases are tested. The `ChainBreakReason` enum is exhaustive.

4. **Does `plan.rs` correctly handle `TokenMissing`?** Yes. The match arm for
   `TokenMissing` is an empty block (falls through to `plan_external_send()`).
   `TokenMismatch` correctly `continue`s (skips the drive). Both are tested.

5. **Any path where a drive swap goes undetected?** Yes, by design: the entire
   verification path is not wired yet. Once wired (HSD-B), the remaining gap is:
   if SQLite is unavailable AND the drive's token file is unreadable, both fail-open
   paths combine to return `Available`. This is the correct ADR-107 behavior but
   represents a detection gap when both data sources are simultaneously unavailable.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Logic is sound across all modules. One SQL idempotency issue (S1). All edge cases tested. |
| **Security** | 5 | Token path construction is safe (uses `Path::join`). Token parsing is minimal and robust. Atomic write pattern is correct. No injection surface. |
| **Architectural Excellence** | 5 | Pure function boundary in `awareness.rs` maintained. Clean enum design replaces stringly-typed alternatives. Module responsibilities respected. |
| **Systems Design** | 4 | Fail-open semantics correct. Self-healing path works. One gap in concurrent token write (M1). |
| **Test Quality** | 4 | Good coverage of the happy path and key edge cases. One gap in pin read error coverage (M2). |
| **Simplicity** | 5 | No over-engineering. Types are right-sized. Dead code is properly annotated with rationale. |

---

## Findings

### S1 — Significant: `store_drive_token` upsert does not preserve `first_seen` on self-healing re-store

**What:** The `store_drive_token` SQL uses `ON CONFLICT(drive_label) DO UPDATE SET token = ?2, last_verified = ?3`. This correctly preserves `first_seen` when the same `drive_label` already has a row. However, in the self-healing path of `verify_drive_token()` (line 312-319 of `drives.rs`), when a drive has a token but SQLite has none, the code calls `store_drive_token()` with the current timestamp. Since there is no existing row, `first_seen` is set to `now` — but the token may have been on the drive for months. The `first_seen` timestamp is inaccurate for self-healed tokens.

**Consequence:** Minor data quality issue. `first_seen` represents "first seen by SQLite" not "first written to drive." If anyone queries `first_seen` for debugging or operational monitoring, the timestamp could be misleading for self-healed tokens.

**Suggested fix:** This is acceptable as-is — `first_seen` is not consumed by any code path currently, and "first seen by this SQLite instance" is a defensible interpretation. If accuracy matters later, the token file already contains a `# Written: {timestamp}` comment that could be parsed as the authoritative `first_seen`. Document the semantics: "`first_seen` records when SQLite first learned about this token, not when the token was created."

**Severity justification:** Significant because the semantics are subtly wrong and could mislead future developers, but no current code depends on `first_seen` accuracy.

### M1 — Moderate: Concurrent token writes are not protected

**What:** If two Urd processes run simultaneously (e.g., manual `urd backup` and systemd timer race), both could execute `maybe_write_drive_token()` for the same drive. The sequence: Process A reads no token, Process B reads no token, Process A writes token-A, Process B writes token-B (overwriting token-A), Process A stores token-A in SQLite, Process B stores token-B in SQLite (overwriting token-A's entry). Result: drive has token-B, SQLite has token-B. Consistent, but token-A was briefly the "truth" and was silently replaced.

**Consequence:** Low risk in practice. Urd uses an advisory lock (`lock.rs`) to prevent concurrent runs, so this scenario requires the lock to be bypassed. The final state is consistent (drive and SQLite agree). The risk is theoretical, not practical.

**Suggested fix:** No fix needed. The advisory lock prevents this in practice, and even if the lock were bypassed, the final state is consistent. The atomic rename ensures no partial token files exist. Document that the advisory lock is the concurrency control for token writes.

### M2 — Moderate: `assess_chain_health` pin read error test is missing from integration tests

**What:** The `assess_chain_health()` function handles `Err(_)` from `fs.read_pin_file()` by returning `ChainBreakReason::PinReadError`. However, the `MockFileSystemState::read_pin_file()` always returns `Ok(Some(_))` or `Ok(None)` — there is no test that exercises the `Err` path through `assess()`.

**Consequence:** The error path is simple (returns `Broken` with `PinReadError` reason), so the risk of a bug there is low. But it means the `PinReadError` variant is tested only by visual inspection, not by execution.

**Suggested fix:** Add a `fail_pin_reads` field to `MockFileSystemState` (similar to `fail_local_snapshots`) and a test that exercises the pin read error path through `assess()`. Low priority but would close the coverage gap.

### M3 — Moderate: Token file with no `token=` line returns a hard error

**What:** In `read_drive_token()` (line 221-226 of `drives.rs`), if the file exists but contains no `token=` line, the function returns `Err(UrdError::Io { ... InvalidData ... })`. In `verify_drive_token()`, this error is caught at line 302-305 and returns `Available` (fail-open). This means a corrupted token file (e.g., truncated to just comments) silently degrades to "no verification."

**Consequence:** This is the correct fail-open behavior per ADR-107. However, the log message says "Failed to read drive token" which doesn't distinguish between I/O errors and corrupted content. A user investigating a potential swap would benefit from knowing the token file exists but is malformed.

**Suggested fix:** Log at `warn` level with a more specific message: "Drive token file exists but is malformed (no token= line) for {label}". The current `Err` already carries the path and "no token= line" message, so the log statement in `verify_drive_token` already captures this. Verify the log output is actionable — it currently is: `"Failed to read drive token for {}: {e}"` where `e` includes "token file exists but contains no token= line". **No code change needed**, but confirm this is sufficient.

### C1 — Commendation: `ChainStatus` / `ChainBreakReason` enum design

The move from stringly-typed chain health (the old `reason: String` field proposed in the design doc) to a proper `ChainStatus` enum with `ChainBreakReason` variants is excellent. The implementation departs from the design doc (which proposed `intact: bool` + `reason: String`) in favor of this richer type, which is the right call. Benefits:

- Exhaustive matching prevents forgetting a case
- `Display` impl on `ChainBreakReason` keeps string rendering in one place
- The `Broken { reason, pin_parent }` variant carries the pin parent optionally, which is exactly the data the sentinel needs in HSD-B
- `ChainStatus::Intact { pin_parent }` makes the happy-path data non-optional — you always have the pin parent when the chain is intact

This is a good example of letting the implementation improve on the design.

### C2 — Commendation: Status command simplification

The `commands/status.rs` file went from containing its own `compute_chain_health()` function with direct filesystem calls to a clean derivation from the awareness assessment. The conversion logic (lines 29-51) is straightforward: pattern-match on `ChainStatus` variants, map to `output::ChainHealth`, take `min()` across drives. This validates the design decision to move chain health into awareness.

### C3 — Commendation: Token write follows the pin-on-success pattern

The `maybe_write_drive_token()` method in `executor.rs` follows the exact same pattern as pin-on-success: check if already present, generate if not, write to drive, store in SQLite, log failures as warnings. The comment "Same pattern as pin-on-success" makes the design intent explicit. This consistency makes the code predictable.

### C4 — Commendation: `#[allow(dead_code)]` annotations with rationale

Every `dead_code` annotation includes a comment explaining when the code will be wired: `"wired in HSD-B"`, `"wired into sentinel_runner and commands/backup in HSD-B"`. This prevents future developers from deleting infrastructure that is intentionally staged for the next session.

---

## Architecture Evaluation

### 1. Does `awareness.rs` stay pure? (ADR-108)

**Yes.** The `assess_chain_health()` function takes `&dyn FileSystemState`, which is a test-friendly trait. The `FileSystemState` trait's `read_pin_file()` method is the only I/O path, and it goes through the same abstraction that the planner uses. All chain health tests use `MockFileSystemState`. The awareness module does not import `std::fs`, `std::process`, or any I/O module directly. **ADR-108 preserved.**

### 2. Is the `ChainStatus`/`ChainBreakReason` enum design sound?

**Yes.** See C1 above. The two-level enum (`ChainStatus` wrapping `ChainBreakReason`) separates "is it broken?" from "why is it broken?" cleanly. The `Copy` derive on `ChainBreakReason` is correct since all variants are unit variants. The `Display` impl provides the string rendering once, consumed by the status command's conversion to `output::ChainHealth`.

### 3. Is `verify_drive_token()` correctly separated from `drive_availability()`?

**Yes.** `drive_availability()` is unchanged — no new parameters, no SQLite dependency. `verify_drive_token()` is a standalone function that takes `(&DriveConfig, &StateDb)` and returns `DriveAvailability`. The protocol obligation is documented in a doc comment with `PROTOCOL OBLIGATION` in bold. The `#[allow(dead_code)]` annotation confirms it is not yet wired, which means the protocol obligation is not yet in effect.

### 4. Is the protocol obligation documented and enforceable?

**Documented: yes.** The `verify_drive_token()` doc comment explicitly states the obligation. **Enforceable: not yet.** The design review (S1) suggested a `VerifiedDrive` wrapper type to make the protocol structural. This was not implemented in HSD-A, which is acceptable since the function is not yet wired. **When HSD-B wires this, the protocol obligation becomes load-bearing and should be revisited.** At minimum, the two call sites (executor/backup command and sentinel runner) should have comments explaining why they call both functions.

---

## Systems Design Evaluation

### 1. First run after upgrade (existing drives have no tokens)

**Handled correctly.** Existing drives have no `.urd-drive-token` file. `verify_drive_token()` returns `TokenMissing`. The planner's `TokenMissing` arm falls through to `plan_external_send()`. Sends proceed. `maybe_write_drive_token()` writes the token after the first successful send. **Seamless upgrade path, no user action required.**

### 2. SQLite unavailable

**Handled correctly.** In `verify_drive_token()`: `state.get_drive_token()` returns `Err` -> log warning, return `Available` (fail-open). In `maybe_write_drive_token()`: `self.state` is `Option<&StateDb>`, checked with `if let Some(state)` — if `None`, token is still written to drive but not stored in SQLite. On next verification, the self-healing path stores it. **ADR-107 compliant.**

### 3. Drive is read-only

**Handled correctly.** `write_drive_token()` returns an `Err` (from `std::fs::write`). `maybe_write_drive_token()` catches the error at line 714-717 with `log::warn!` and returns. The send that triggered the token write is already recorded as successful (the send happened before the token write). **Read-only drives simply never get tokens, and sends proceed normally.**

### 4. Token file is corrupted

**Handled correctly** (see M3). Malformed files (no `token=` line) return an error from `read_drive_token()`. In `verify_drive_token()`, this is caught and returns `Available` (fail-open). In `maybe_write_drive_token()`, it is caught and the function returns early (no overwrite of the corrupted file). **The corrupted file persists but does not block operations.** If the user wants to fix it, they can delete the file and the next send will regenerate it.

### 5. Two Urd processes write tokens simultaneously

**See M1.** Protected by the advisory lock in practice. Final state is always consistent even without the lock.

---

## Security Evaluation (sudo context)

### 1. Token file path construction

**Safe.** `token_file_path()` uses `Path::join` on `drive.mount_path`, `drive.snapshot_root`, and the constant `TOKEN_FILENAME` (`.urd-drive-token`). The drive config values come from TOML parsing. No user-controlled runtime input is injected into the path. No string concatenation of paths. **No path traversal risk.**

### 2. Token file parsing

**Safe.** `read_drive_token()` reads the file as a string, splits by lines, skips comments and blanks, and looks for `token=` prefix. The parsed value is used only for string comparison in `verify_drive_token()`. It is never passed to a shell, formatted into SQL (the SQL uses parameterized queries), or used as a path. Even a maliciously crafted token value (e.g., containing newlines, SQL, or shell metacharacters) would only be compared as a string and stored as a parameterized SQL value. **No injection risk.**

### 3. Atomic write TOCTOU

**Minimal risk.** `write_drive_token()` writes to `.urd-drive-token.tmp` then renames to `.urd-drive-token`. The temp file and final file are in the same directory, so `rename()` is atomic on POSIX. The TOCTOU window is between `read_drive_token()` (in `maybe_write_drive_token()`) and the write — if another process creates the token in this window, it will be overwritten. But this is the same concurrency scenario as M1, which is protected by the advisory lock.

---

## The Simplicity Question

**What earns its keep:**

- `ChainStatus` / `ChainBreakReason` enums: ~40 lines that replace stringly-typed alternatives and enable exhaustive matching. The sentinel (HSD-B) will pattern-match on these directly.
- `DriveChainHealth` struct: 5-field struct that serves as the single source of chain health truth. Eliminated duplicate computation in `commands/status.rs`.
- `maybe_write_drive_token()`: 37 lines that follow the existing pin-on-success pattern exactly. No new abstractions introduced.
- `store_drive_token` / `get_drive_token` / `touch_drive_token`: Standard SQLite CRUD, ~50 lines total. Nothing clever.

**What to watch:**

- The `#[allow(dead_code)]` annotations on `verify_drive_token`, `get_drive_token`, `touch_drive_token`, `TokenMismatch`, and `TokenMissing` — these are staged for HSD-B. If HSD-B is delayed significantly, the dead code accumulates without providing value. Track HSD-B as a near-term follow-up.

---

## For the Dev Team

Priority-ordered action items:

1. **Document `first_seen` semantics** (Finding S1). Add a comment to `store_drive_token()` clarifying that `first_seen` means "first stored in SQLite" not "first written to drive." One-line comment, prevents future confusion.

2. **Add pin read error test path** (Finding M2). Add a `fail_pin_reads` field to `MockFileSystemState` and a test exercising `ChainBreakReason::PinReadError` through `assess()`. Low effort, closes a coverage gap.

3. **Plan HSD-B promptly** (observation). The drive token infrastructure is complete but dormant. The protection it provides is zero until `verify_drive_token()` is wired into `commands/backup.rs` and/or the sentinel runner. The longer HSD-B is delayed, the longer the project carries dead code without the defense it was designed to provide.

4. **Revisit protocol obligation enforcement at HSD-B** (architecture). When wiring `verify_drive_token()`, add comments at each call site explaining the two-step requirement. Consider the `VerifiedDrive` wrapper type from the design review if a third caller appears.

---

## Open Questions

1. **Should `maybe_write_drive_token()` be called on the crash-recovery "already exists and is pinned" path?** Currently, when a send is skipped because the snapshot already exists at the destination and is pinned (line 380-401 of `executor.rs`), the function returns `Success` without calling `maybe_write_drive_token()`. This means a drive that was fully populated via a crash-recovery resumption might not get its token written until a subsequent "real" send. This is a minor gap — the next normal send will write the token. No fix needed unless it causes observable issues.

2. **What happens when the same drive is unmounted, reformatted, and remounted with the same label?** The drive has no token file (reformatted). SQLite has the old token. `verify_drive_token()` returns `TokenMissing`. Sends proceed (fail-open). The next send writes a new token to the drive AND stores it in SQLite (overwriting the old one via `ON CONFLICT`). **The old token is silently replaced.** This is the correct behavior for an intentional drive replacement, but it means the defense is temporarily weakened (the first send to the reformatted drive is not verified). This is the same temporal gap identified in the design review, and is acceptable.
