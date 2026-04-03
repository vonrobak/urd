---
upi: "007"
status: proposed
date: 2026-04-02
---

# Design: Safety Gate Communication (UPI 007)

> **TL;DR:** When a backup safety gate fires (chain-break full send blocked), communicate
> it as a deliberate protection, not a failure. Change "FAILED" to "DEFERRED" and "partial"
> to "success (1 deferred)" so the output reflects that the tool worked correctly.

## Problem

During v0.8.0 testing (T1.6), htpc-root's chain-break full send gate fired correctly —
blocking a 31.8GB full send to 2TB-backup that required explicit opt-in. This is a
safety feature working as designed. But the output said:

```
FAILED htpc-root  [30.7s]  (incremental → WD-18TB, 1.6GB)
  ERROR send_full: chain-break full send gated — run `urd backup --force-full --subvolume htpc-root` to proceed
```

And the summary said:

```
── Urd backup: partial ── [run #23, 284.4s] ──
```

"FAILED" implies something went wrong. "partial" implies the backup didn't complete.
Neither is true. The tool made a correct safety decision. The language should celebrate
that, not apologize for it.

Steve's review: "When the seatbelt catches you, the car doesn't say 'DRIVING FAILED.'"

## Proposed Design

### New result category: "deferred"

The backup execution already tracks per-subvolume results. Currently, a gated full send
is classified as a failure. Add a "deferred" classification that is distinct from both
success and failure.

### Changes in executor result types

The executor returns results per subvolume. The result classification needs a new variant
for "gated by safety check" vs "failed due to error." This requires tracing back to where
the chain-break gate fires.

Looking at the executor: when a full send is gated, it returns an error with the message
"chain-break full send gated." The `commands/backup.rs` code then classifies any error as
a failure.

The cleanest fix: have the executor return a distinct result for gated operations, not
an error. This could be:

**Option A:** A new field on the subvolume result: `deferred: bool` alongside success/error.

**Option B:** A tri-state result: Success / Deferred / Failed.

**Recommendation:** Option B is cleaner. The result type should express what happened:

```rust
pub enum SubvolResult {
    Success,
    Deferred { reason: String, suggestion: String },
    Failed { error: String },
}
```

### Changes in backup summary

In `commands/backup.rs:287` (`build_backup_summary()`), the overall `result` field
is currently a string: "success", "partial", or "failure".

New logic:
- All subvolumes succeeded → "success"
- Some succeeded, some deferred, none failed → "success" (deferred count in display)
- Some failed → "partial" (genuine failures)
- All failed → "failure"

The key insight: deferred operations don't downgrade the result. The backup did
everything it was supposed to do; it deliberately chose not to do something unsafe.

### Changes in voice rendering

**Per-subvolume line:** Instead of:
```
FAILED htpc-root  [30.7s]  (incremental → WD-18TB, 1.6GB)
  ERROR send_full: chain-break full send gated — ...
```

Render as:
```
DEFERRED htpc-root  (full send to 2TB-backup gated — 31.8GB requires opt-in)
  → Run `urd backup --force-full --subvolume htpc-root` when ready
```

Color: yellow (caution, not error). Not red (failure) or green (success).

**Summary line:** Instead of:
```
── Urd backup: partial ── [run #23, 284.4s] ──
```

Render as:
```
── Urd backup: success ── [run #23, 284.4s] ── (1 deferred)
```

Or if there are both deferred and failed:
```
── Urd backup: partial ── [run #23, 284.4s] ── (1 failed, 1 deferred)
```

### Changes in output types

In `output.rs`, `SubvolumeSummary` (used in `BackupSummary`) needs to express the
deferred state. Currently it has `status: String` ("OK" or "FAILED") and
`error: Option<String>`.

Add:
```rust
pub struct SubvolumeSummary {
    pub name: String,
    pub status: SubvolStatus,  // enum instead of String
    pub duration_secs: f64,
    pub operations: Vec<String>,
    pub error: Option<String>,
    pub deferred: Option<DeferredInfo>,
}

pub enum SubvolStatus {
    Ok,
    Deferred,
    Failed,
}

pub struct DeferredInfo {
    pub reason: String,
    pub suggestion: String,
}
```

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `executor.rs` | Return deferred result for gated operations instead of error | Unit test: chain-break gate returns deferred, not error; genuine send failures still return error |
| `output.rs` | Add `SubvolStatus` enum, `DeferredInfo` struct; update `SubvolumeSummary` | Struct tests |
| `commands/backup.rs` | Classify deferred results; update `build_backup_summary()` result logic | Unit test: all success + deferred → "success"; deferred + failed → "partial" |
| `voice.rs` | Render DEFERRED in yellow; update summary line format | Unit test: deferred renders correctly; summary includes deferred count |

## Effort Estimate

Patch tier. ~0.5 session. The main work is tracing the chain-break gate through the
executor and ensuring the deferred result propagates cleanly to the summary.

## Sequencing

1. `output.rs` — new types (pure, no behavior change)
2. `executor.rs` — return deferred instead of error for gates
3. `commands/backup.rs` — classify deferred results
4. `voice.rs` — rendering
5. Update existing tests that expect "FAILED" for gated operations

## Architectural Gates

None. This changes internal result classification, not public contracts. The backup
summary schema adds a field but doesn't remove any.

Note: if the daemon/JSON output mode is consumed by external tools, the schema change
(SubvolStatus enum in JSON) needs consideration. Since this is pre-1.0, schema changes
are expected. The `schema_version` field in heartbeat/output can be bumped.

## Rejected Alternatives

**Just change the label from "FAILED" to "DEFERRED" in voice.rs without changing the
result model.** This would be a cosmetic fix. The underlying data model should accurately
represent what happened — deferred is semantically different from failed, and downstream
consumers (metrics, history, heartbeat) should be able to distinguish them.

**Add a "warnings" category between success and failure.** A warning implies something
mildly wrong. A deferred operation is a correct decision, not a warning. The vocabulary
should be precise.

**Remove the "partial" result entirely.** Too aggressive. "Partial" is correct when
some subvolumes genuinely fail (I/O error, permission denied, etc.). The fix is ensuring
safety gates don't trigger "partial," not removing the concept.

## Assumptions

1. The chain-break full send gate is the only safety gate that produces a "failure"
   result. (Need to verify: are there other gates like space checks that also fail
   instead of skip?)
2. The executor's error path for gated operations is distinguishable from genuine
   errors. (The error message contains "chain-break full send gated" — this is a
   string match, which is fragile. Consider a typed error variant.)
3. Metrics and heartbeat consumption won't break with a new result category. (Pre-1.0,
   so acceptable.)

## Resolved Decisions (from /grill-me)

**007-Q1: Add `OpResult::Deferred` to existing enum.** The gate at executor.rs:318
changes from `OpResult::Failure` to `OpResult::Deferred`. No function signature changes.
`record_operation()` at line 967 maps `Deferred` to `"deferred"` for SQLite. One test
update at line 2010 (expects `Deferred` instead of `Failure`).

**007-Q2: Deferred-only runs record as "success" in state database.** Only genuine
`OpResult::Failure` results cause "partial" or "failure." A deferred operation is a
correct decision — the tool did everything it decided to do. Display layer (voice) can
show "success (1 deferred)" in history and status.

**007-Q3 (verified): Chain-break gate is the only safety gate.** All other
`OpResult::Failure` sites in executor.rs are genuine errors (snapshot creation, dest dir
creation, send/receive I/O, delete). One change site (line 318), one test update
(line 2010).
