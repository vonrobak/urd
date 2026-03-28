# Design Review: Hardware Swap Defenses

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-28
**Scope:** Design review of `docs/95-ideas/2026-03-28-design-hardware-swap-defenses.md`
**Mode:** Design review (4 dimensions)
**Commit:** `7ff4d8d`
**Reviewer:** arch-adversary

---

## Executive Summary

A well-structured, three-layer defense design with clear separation between identity
verification (Layer 1), anomaly detection (Layer 2), and operational gating (Layer 3).
The layering is the right call — each layer catches what the others miss, and each
is independently deployable. The design has one significant gap in the token
verification architecture and one correctness issue in the full-send gate's interaction
with autonomous mode. Neither is fatal; both need resolution before implementation.

## What Kills You

**Catastrophic failure mode:** Silent data loss via sends to the wrong physical drive,
filling it, causing ENOSPC, and corrupting in-progress receives. This is exactly what
the design is trying to prevent, and it's 1-2 bugs away from production:

- If `TokenMismatch` silently degrades to `Available` (implementation bug in
  `drive_availability`), sends proceed to the wrong drive.
- If the full-send gate in autonomous mode defaults to `Allow` instead of
  `SkipAndNotify` (configuration bug), a 4TB full send proceeds into 1.1TB of space.

Both are single-point failures. The design addresses them but must be airtight in
implementation.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Logic is sound. One gap: `SkipAndNotify` without a resolution path creates permanent send stagnation. |
| **Security** | 4 | Token is not a security boundary (good — it's identity, not auth). Token file format resists injection. TOCTOU surface is minimal. |
| **Architectural Excellence** | 4 | Clean layering, respects module boundaries, pure-function pattern preserved. One scope question on awareness.rs. |
| **Systems Design** | 3 | Token verification timing creates a real operational gap. Autonomous-mode gating needs a timeout or escalation to avoid silent neglect. |

---

## Design Tensions

### Tension 1: Token verification as a separate function vs. part of `drive_availability()`

The design chose Option 2 (separate `verify_drive_token()`) to avoid threading `StateDb`
through the planner. This is the right call for the planner (pure function, no I/O
dependencies), but it creates a **protocol obligation**: every caller that acts on
`DriveAvailability::Available` must *also* call `verify_drive_token()`, or they'll
send to an unverified drive. The design acknowledges this in the "Ready for Review"
section but doesn't solve it.

**Verdict:** Acceptable for now because the callers that matter (executor, sentinel
runner) are both in I/O layers where `StateDb` is available. But this should be
addressed with a wrapper type (see Finding S1) before a third caller appears.

### Tension 2: Fail-open vs. fail-closed on token mismatch

The design makes `TokenMissing` fail-open (sends proceed) and `TokenMismatch`
fail-closed (sends blocked). This is the right split. But there's a temporal gap:
between "drive first mounted" and "first send completes and writes token," the drive
is in `TokenMissing` state. If a swap happens in this window, it's undetected.

**Verdict:** Acceptable. The window is narrow (one backup cycle) and this is a new
capability — going from "no swap detection" to "swap detection after first send" is
a large improvement. Documenting the gap is sufficient.

### Tension 3: Chain health in `awareness.rs` scope expansion

The design proposes moving chain health computation from `commands/status.rs` into
`awareness.rs`. This expands the awareness module's scope from "freshness computation"
to "overall backup health assessment." The design frames this as natural ("compute
promise states and operational readiness"), and I agree — but it should be acknowledged
as a deliberate scope expansion, not treated as a minor addition. The module's doc
comment and CLAUDE.md table entry should be updated.

**Verdict:** Right call. Chain health is a dimension of "is my data safe?" — the same
question awareness already answers. Having it computed in one place instead of two
(status command + sentinel runner) eliminates divergence risk.

### Tension 4: Autonomous mode send gate with no escalation path

The design gates chain-break full sends in autonomous mode with `SkipAndNotify`. But
there's no described mechanism for escalation or auto-resolution. If the user doesn't
check notifications for two weeks, those subvolumes accumulate no new external backups.
The design explicitly punts on a timeout ("Ship the binary gate first"), but the
consequence is that the gate itself can cause the problem it's trying to prevent:
data becomes UNPROTECTED because backups are blocked indefinitely.

**Verdict:** This tension must be resolved in the design, not deferred. See Finding S2.

---

## Findings

### S1 — Significant: Unverified-drive send path from forgotten `verify_drive_token()` call

**What:** The design separates `drive_availability()` from `verify_drive_token()`.
Any code path that checks `DriveAvailability::Available` but doesn't subsequently
call `verify_drive_token()` will send to an unverified drive.

**Consequence:** Today the callers are known (executor, sentinel runner). But the
planner filters drives by availability — `plan_external_send()` is only called for
`Available` drives. If someone adds token verification to the planner's drive
filtering (reasonable — "don't plan sends for mismatched drives"), they'd call
`drive_availability()` but might miss the separate token check.

**Suggested fix:** Introduce a `VerifiedDrive` wrapper that can only be constructed
by a function that performs both checks. The planner's drive filtering code doesn't
need to call this (it filters by availability, the executor does the final check).
But the executor should require `VerifiedDrive`, not `DriveConfig`, for send
operations. This makes the protocol structural rather than documentation-based.

Alternatively (simpler): add `TokenMismatch` and `TokenMissing` as variants of
`DriveAvailability` itself (which the design already shows), and have
`drive_availability()` perform the token check when `StateDb` is available via an
`Option<&StateDb>` parameter. When `None` is passed (planner context), token
verification is skipped. This is less pure but eliminates the two-call protocol.

### S2 — Significant: `SkipAndNotify` without escalation creates unbounded protection gap

**What:** In autonomous mode (systemd timer), chain-break full sends are skipped
indefinitely until the user runs `--force-full`. If the user doesn't read
notifications, those subvolumes stop receiving external backups permanently.

**Consequence:** The gate designed to prevent ENOSPC catastrophe can itself cause
data to become UNPROTECTED. The awareness module will eventually flag `AtRisk` →
`Unprotected`, and the sentinel will fire `PromiseDegraded` notifications, but
this creates a confusing cascade: the user sees "UNPROTECTED" but the fix isn't
to "make a backup" — it's to approve a specific full send with a flag they may
not know about.

**Suggested fix:** Add an escalation timeline to the design:

1. **Day 0-3:** `SkipAndNotify`. Notification urgency: INFO.
2. **Day 3-7:** Continue skipping. Notification urgency escalates to WARNING.
   Notification text changes: "Chain-break full send for htpc-home to WD-18TB1
   has been skipped for 3 days. Run `urd backup --force-full --subvolume htpc-home`
   or the send will auto-proceed on day 7."
3. **Day 7+:** Auto-proceed with the full send. Emit a WARNING notification:
   "Proceeding with chain-break full send after 7 days of deferral."

The 7-day timeout ensures that the gate is self-healing. Space guards remain as the
final safety net (they already caught the dangerous sends in the test). The auto-
proceed restores incrementality: after the full send succeeds, the next send will
be incremental again.

Store the deferral timestamp in `state.rs` alongside the reason and subvolume/drive
pair. The planner reads this to decide whether to proceed or skip.

### M1 — Moderate: Token file readable by any user on the drive

**What:** The `.urd-drive-token` file is written in plaintext to the drive's snapshot
root. Any user who can read the drive can read the token. If they copy the token to
another drive (or include it in a `dd` clone), the second drive passes verification.

**Consequence:** The token protects against *accidental* swaps (the test scenario),
not *intentional* bypasses. This is appropriate — the token is an identity mechanism,
not a security control. But the design should state this explicitly so future
developers don't treat it as a security boundary.

**Suggested fix:** Add a non-security statement to the design: "The drive session
token is an identity signal, not a security control. A user who copies the token file
to a different drive can defeat verification. This is acceptable: the threat model is
accidental hardware swaps, not adversarial drive substitution."

### M2 — Moderate: `FullSendReason` logic has a classification gap

**What:** The design classifies full send reasons as:
- Pin exists, parent missing → `ChainBroken`
- No pin, no external snapshots → `FirstSend`
- No pin, external snapshots exist → `NoPinFile`

But there's a fourth case: pin exists, pin points to a snapshot that exists on the
drive, but the snapshot is *not* the most recent one sent. This happens when a user
manually writes a pin file pointing to an old snapshot. The current planner treats
this as incremental (parent exists), not as a chain break. The classification logic
doesn't address this scenario.

**Consequence:** Minor. Manual pin file editing is unusual. But the classification
should acknowledge this case explicitly — even if the answer is "this is correctly
classified as incremental because the parent does exist."

**Suggested fix:** Add a note: "When a pin file points to a valid parent (exists
locally and on the drive), the send is always incremental regardless of whether
the parent is the 'expected' one. Manual pin file editing is user-directed and
outside the scope of automated chain break detection."

### M3 — Moderate: First assessment chain health comparison is suppressed but chain state is empty

**What:** The design says chain break detection is skipped on the first assessment
(`has_initial_assessment` guard). But the first assessment *does* populate
`last_chain_health`. If a drive is already in a broken-chain state when the sentinel
starts, the second assessment will compare "all chains broken" (current) against
"all chains broken" (first assessment). No anomaly detected because no *change*
occurred.

**Consequence:** If the sentinel restarts after a drive swap (or is started for the
first time with a swapped drive), the simultaneous chain break goes undetected. The
design relies on change detection, but the swap may have happened before the sentinel's
observation window.

**Suggested fix:** Consider adding a "chain health baseline" check on startup that
doesn't compare against a previous state but instead flags an absolute condition:
"all chains on drive X are broken." This is a weaker signal (could be a fresh drive
with no sends yet), so it should be advisory rather than a full anomaly alert. Gate
it on "drive has received sends before" (pin files existed historically, queryable
from `state.rs` operation history).

### C1 — Commendation: Three-layer defense with independent deployability

Each layer (identity, detection, gating) catches different failure modes and can be
shipped independently. This is the right pattern for a system that's the sole backup
tool — you can ship Layer 3 (full-send gate) immediately for the highest-impact
protection while Layer 1 (tokens) requires the migration period. The design explicitly
states this sequencing and doesn't create inter-layer dependencies. This is genuinely
good systems design.

### C2 — Commendation: `TokenMissing` as a benign state

Making `TokenMissing` non-blocking is the correct backward-compatibility decision.
Existing drives continue to work immediately after upgrade. The token is written
silently on the next send. No user action required. This is the kind of migration
path that makes upgrade painless — the feature appears as if it was always there.

### C3 — Commendation: Chain health moving into `awareness.rs`

This is a good simplification opportunity. Currently chain health is computed ad-hoc
in `commands/status.rs`. Moving it into the assessment means one computation, multiple
consumers (status command, sentinel, visual feedback model). It also means chain health
is testable through the same pure-function pattern as everything else in awareness.

---

## The Simplicity Question

**What earns its keep:**

- `FullSendReason` enum: 15 lines, enormous payoff. The planner already decides
  full-vs-incremental; annotating *why* costs nothing and enables the gate.
- `DriveChainHealth` in assessments: replaces duplicate computation, enables sentinel
  detection, feeds the visual feedback model. Three uses for one computation.
- Drive session token: simple file with a UUID. The entire identity verification is
  ~60 lines and a small SQLite table.

**What to watch:**

- `FullSendPolicy` enum with three variants: `Allow`, `SkipAndNotify`, `Confirm`.
  Only two are used (Allow for interactive, SkipAndNotify for autonomous). `Confirm`
  is future-tense. Consider just making this a `bool` (`gate_chain_break_sends`)
  and adding the enum when the interactive prompt is actually built. Speculative
  abstractions create maintenance and test burden before they deliver value.

- The `verify_drive_token()` as a separate function: adds a protocol obligation that
  the type system doesn't enforce. If Option 2 is kept, it should be a temporary state
  of affairs with a plan to collapse it (tracked in tech debt or as a "cleanup after
  Session A" item).

---

## For the Dev Team

Priority-ordered action items for the design author:

1. **Add escalation timeline to `SkipAndNotify`** (Finding S2). Define a 7-day (or
   configurable) auto-proceed after which chain-break full sends execute anyway.
   Store deferral state in SQLite. Without this, the gate can cause the problem it's
   designed to prevent.

2. **Document the token verification protocol obligation** (Finding S1). Either:
   (a) collapse into `drive_availability()` with `Option<&StateDb>`, or (b) add a
   `VerifiedDrive` wrapper type, or at minimum (c) add a code comment at every call
   site documenting the two-step requirement. Choose before implementation begins.

3. **Add non-security statement for drive token** (Finding M1). One sentence in the
   design: "The token is identity, not security. Copied tokens defeat verification.
   Threat model: accidental swaps."

4. **Acknowledge the classification gap in `FullSendReason`** (Finding M2). Document
   the pin-points-to-valid-but-old-parent case explicitly.

5. **Consider startup chain health baseline** (Finding M3). Decide whether to add a
   one-time "all chains broken on drive X" advisory on first assessment, gated on
   historical send activity. If deferred, document the gap.

6. **Simplify `FullSendPolicy`** — consider a `bool` instead of a three-variant enum
   until `Confirm` is actually implemented.

---

## Open Questions

1. **What happens to the token when the user intentionally replaces a drive?** (e.g.,
   "WD-18TB1 died, I bought a new drive and labeled it WD-18TB1"). The new drive has
   no token. `TokenMissing` allows the first send. But the SQLite table still has the
   old token — `last_verified` will be stale. Is there a `urd drive reset-token
   WD-18TB1` command in scope? Or does the first successful send to the new drive
   silently overwrite the stored token?

2. **How does token verification interact with the `--trust-drive` flag mentioned as
   out of scope?** If `--trust-drive` is the only escape hatch for `TokenMismatch`,
   and it doesn't exist yet, what does the user do when they intentionally swap drives
   and hit the block? The design should specify the interim escape hatch (e.g., delete
   the token file from the drive, or clear the SQLite table row).

3. **Does the sentinel have access to `StateDb`?** The sentinel runner opens `StateDb`
   in `execute_assess()` already (confirmed in code). Good — token verification in the
   sentinel is feasible without new infrastructure.
