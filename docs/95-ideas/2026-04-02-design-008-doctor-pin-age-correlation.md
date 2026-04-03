---
upi: "008"
status: proposed
date: 2026-04-02
---

# Design: Doctor Pin-Age Correlation with Drive Absence (UPI 008)

> **TL;DR:** Doctor warns "sends may be failing" when pin files are old, but the pin is
> old because the drive was absent — not because sends failed. Correlate pin age with
> drive mount state to give accurate diagnostics.

## Problem

During v0.8.0 testing (T1.4), `urd doctor --thorough` showed:

```
⚠ htpc-home/2TB-backup: Pin file is 8 day(s) old (threshold: 2 day(s)) — sends may be failing
```

2TB-backup had been absent for 8 days. The pin is 8 days old because the drive wasn't
there to send to — not because sends failed. The diagnostic is technically accurate
(the pin IS old) but practically misleading (the reason is not what doctor implies).

This erodes trust. If doctor regularly tells you "sends may be failing" when nothing is
wrong, you learn to ignore doctor warnings — and then miss the real ones.

Steve's review (score: 72/100): "Don't tell the user sends 'may be failing' when the
drive was on a shelf."

## Proposed Design

### Where pin-age warnings are generated

Pin-age warnings come from the verify/thread-checking system, invoked by
`urd doctor --thorough`. The verify output includes per-drive thread checks that
examine pin file freshness.

The fix: before emitting a "pin file is X days old — sends may be failing" warning,
check whether the drive is currently mounted. If not mounted, change the message to
reflect expected staleness.

### Message variants

**Drive mounted, pin old:**
```
⚠ htpc-home/2TB-backup: Pin file is 8 day(s) old (threshold: 2 day(s)) — sends may be failing
```
(Unchanged — this is a genuine warning when the drive is present.)

**Drive not mounted, pin old:**
```
⚠ htpc-home/2TB-backup: Pin file is 8 day(s) old — expected, drive not connected
```
(Downgraded from warning-with-alarm to informational. The pin age is reported for
context but the "sends may be failing" accusation is removed.)

### Implementation

The verify/thread system needs access to drive mount state when generating its output.
The doctor command already has access to config and can check `drives::is_drive_mounted()`.

Two approaches:

**Option A: Fix in the verify module.** Pass drive mount state to the verify function
so it can generate context-appropriate messages. This is cleaner architecturally but
requires changing the verify function signature.

**Option B: Fix in doctor command.** After collecting verify output, post-process pin-age
warnings: for each warning about a specific drive, check if that drive is mounted. If
not, replace the message. This is simpler but feels like a patch.

**Recommendation:** Option A. The verify module should produce accurate diagnostics
given the full context. Passing mount state is a small signature change.

### Additionally: suppress UUID suggestion for cloned drives

Steve's item #10 (score: 55/100). Doctor suggests "Add uuid = X to drive Y" via
`check_missing_uuids()` in `commands/doctor.rs:82`. When the UUID is already configured
on another drive, this advice is impossible to follow (config rejects duplicate UUIDs).

Fix: In `check_missing_uuids()` (or its callsite), before suggesting the UUID addition,
check if any other configured drive already has this UUID. If so, either:
- Suppress the suggestion entirely
- Replace with: "WD-18TB shares its filesystem UUID with WD-18TB1 (cloned drives).
  Run `btrfstune -u` on one drive to give it a unique identity."

This is small enough to include in this design rather than a separate UPI.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `verify.rs` or equivalent | Accept drive mount state; adjust pin-age message | Unit test: pin old + drive absent → "expected" message; pin old + drive present → "may be failing" message |
| `commands/doctor.rs` | Pass mount state to verify; fix UUID suggestion for cloned drives | Unit test: UUID already on another drive → no "add uuid" suggestion |
| `drives.rs` | `check_missing_uuids()` may need a `configured_uuids` parameter | Unit test: UUID dedup detection |

## Effort Estimate

Patch tier. ~0.25 session. Two targeted message changes. No new data flows or modules.

## Sequencing

1. Pin-age correlation (main fix)
2. UUID suggestion suppression (small addition)

No dependencies between them.

## Architectural Gates

None. These are diagnostic message improvements, not contract changes.

## Rejected Alternatives

**Remove pin-age warnings for unmounted drives entirely.** Too aggressive. The pin age
is still useful information — it tells you how stale the drive's data is. The fix is
changing the interpretation ("expected" vs "may be failing"), not hiding the data.

**Track drive absence duration and show it in the warning.** Over-engineered. "Drive
not connected" is sufficient context. The user can see absence duration in `urd status`.

## Assumptions

1. The verify system has access to drive config labels, which can be checked against
   `drives::is_drive_mounted()`. (Need to verify the verify module's interface.)
2. `check_missing_uuids()` in drives.rs returns tuples of (label, uuid, snippet).
   Adding a dedup check against configured UUIDs is straightforward.

## Resolved Decisions (from /grill-me)

**008-Q1: Add `drive_mounted: bool` parameter to `collect_stale_pin_check()`.** When
pin is stale and drive is not mounted: downgrade to info status, message becomes
"Pin file is {N} day(s) old — expected, drive not connected." When pin is stale and
drive is mounted: unchanged warn status, "sends may be failing." The caller already
knows mount state from its drive iteration loop — one parameter addition.

**008-Q2: Suppress UUID suggestion via cross-check in `check_missing_uuids()`.** If
the detected UUID is already configured on another drive in the `drives` slice, don't
include it in results. No replacement message — just suppress the contradictory
suggestion. Simple and sufficient.
