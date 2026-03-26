# Design: Structured Error Messages (Priority 2e)

> **TL;DR:** A translation layer in `error.rs` that pattern-matches common btrfs stderr
> patterns into structured "what / why / what to do" error messages, replacing raw stderr
> passthrough. Pure function — no I/O, no behavior change, presentation only.

**Date:** 2026-03-26
**Status:** reviewed (all findings addressed)
**Depends on:** None (existing `UrdError::Btrfs { msg, bytes_transferred }` is sufficient)

## Problem

Btrfs errors currently flow to users as raw stderr wrapped in a fixed format:

```
ERROR send_full: btrfs command failed: send failed (exit 1): ERROR: ...
```

This tells a developer what happened but fails every Norman UX principle for error design.
Users must decode exit codes, recognize btrfs-specific error strings, and independently
figure out remediation steps. The [UX brainstorm](2026-03-23-brainstorm-ux-norman-principles.md)
§6 established the hierarchy: **human summary → cause → remediation → technical details**.

Real failure patterns observed during cutover and testing:

| Failure | Raw stderr | Frequency |
|---------|-----------|-----------|
| Destination full | `ERROR: receive: No space left on device` | 3 incidents (NVMe exhaustion) |
| Source full (snapshot creation) | `ERROR: cannot snapshot: No space left on device` | Same root cause |
| Missing parent (chain break) | `ERROR: send: parent not found` or similar | htpc-root chain break |
| Destination dir missing | `btrfs receive` fails, no dir | Pre-cutover bug (fixed, but still possible on new drives) |
| Permission denied | `ERROR: cannot snapshot: Permission denied` | Setup misconfiguration |
| Read-only filesystem | `ERROR: ...read-only file system` | Drive hardware failure |
| Subvolume not found (delete) | `ERROR: cannot delete: No such file or directory` | Stale retention target |

## Proposed Design

### Module: Translation layer in `error.rs`

A single pure function that takes a raw btrfs error context and returns a structured
error with layered detail:

```rust
/// Structured representation of a btrfs error for user presentation.
#[derive(Debug, Clone)]
pub struct BtrfsErrorDetail {
    /// Human-readable one-line summary (e.g., "Destination drive is full")
    pub summary: String,
    /// What caused it (e.g., "WD-18TB has insufficient space for this send")
    pub cause: String,
    /// Actionable remediation steps
    pub remediation: Vec<String>,
    /// Raw technical details (exit codes, stderr, bytes transferred)
    pub technical: BtrfsTechnical,
}

#[derive(Debug, Clone)]
pub struct BtrfsTechnical {
    pub operation: String,         // "snapshot", "send", "receive", "delete"
    pub exit_code: Option<i32>,
    pub stderr: String,            // raw, unmodified
    pub bytes_transferred: Option<u64>,
}

/// Context passed to the translator. Built at error construction sites in btrfs.rs.
#[derive(Debug)]
pub struct BtrfsErrorContext {
    pub operation: BtrfsOperation,
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub bytes_transferred: Option<u64>,
    /// Destination drive label, if applicable
    pub drive_label: Option<String>,
    /// Subvolume name, if applicable
    pub subvolume: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum BtrfsOperation {
    Snapshot,
    Send,
    Receive,
    Delete,
}

/// Translate raw btrfs error into structured detail.
/// Pure function — pattern-matches stderr, always succeeds.
/// Unknown patterns produce a generic message with full technical details preserved.
pub fn translate_btrfs_error(ctx: &BtrfsErrorContext) -> BtrfsErrorDetail
```

### Pattern matching rules

Each rule matches a substring or regex in stderr and produces a specific
(summary, cause, remediation) triple. Rules are tried in order; first match wins.
Unknown stderr falls through to a generic handler.

```rust
struct ErrorPattern {
    /// Substring match on stderr (case-insensitive)
    pattern: &'static str,
    /// Which operations this pattern applies to (None = any)
    operations: Option<&'static [BtrfsOperation]>,
    /// Builder for the structured detail
    build: fn(&BtrfsErrorContext) -> (String, String, Vec<String>),
}
```

**Initial pattern set** (ordered by frequency/severity):

| # | Pattern (stderr substring) | Operations | Summary |
|---|---------------------------|-----------|---------|
| 1 | `No space left on device` | Receive | "Destination drive is full" |
| 2 | `No space left on device` | Snapshot | "Local filesystem is full" |
| 3 | `Permission denied` | Any | "Insufficient permissions" |
| 4 | `Read-only file system` | Any | "Drive is read-only (possible hardware failure)" |
| 5 | `No such file or directory` | Delete | "Snapshot not found at expected path" |
| 6 | `No such file or directory` | Receive | "Destination directory missing" |
| 7 | `parent not found` | Send | "Incremental parent missing (chain broken)" |
| 8 | (fallthrough) | Any | "btrfs {operation} failed" |

**Remediation examples:**

Pattern 1 (destination full):
```
What to do:
  • Check drive space: df -h /run/media/.../WD-18TB
  • Run `urd backup` again — retention may free space first
  • If persistent, consider increasing `max_usage_percent` or adding a drive
```

Pattern 3 (permission denied):
```
What to do:
  • Verify sudoers configuration: `urd init` checks this
  • Expected entry: <user> ALL=(root) NOPASSWD: /usr/bin/btrfs
```

Pattern 7 (parent not found):
```
What to do:
  • This is recoverable — next send will be a full send
  • Check `urd verify` for chain health
  • If recurring, check retention/send interval alignment: `urd verify`
```

### Integration: Typed context on `UrdError::Btrfs`

**Decision (post-review):** The arch-adversary review (S1) rejected Option A (parsing
structured context back from flattened error strings at render time). The round-trip —
`btrfs.rs` has typed data, formats it into a string, then `voice.rs` regex-parses the
string to recover the types — is a lossy reconstruction with no compiler enforcement.
If the format string changes, the parser silently breaks.

**Adopted approach: Carry typed context on the error variant.**

Replace `UrdError::Btrfs { msg, bytes_transferred }` with:

```rust
#[derive(Debug, Error)]
pub enum UrdError {
    // ... other variants unchanged ...

    #[error("btrfs command failed: {}", context.display_summary())]
    Btrfs {
        context: BtrfsErrorContext,
    },
}
```

`BtrfsErrorContext` carries the structured data that `btrfs.rs` already has in local
variables at every construction site:

```rust
/// Structured context from a failed btrfs subprocess call.
/// Built at error construction sites in btrfs.rs — no parsing needed.
#[derive(Debug, Clone)]
pub struct BtrfsErrorContext {
    pub operation: BtrfsOperation,
    pub exit_code: Option<i32>,
    /// Raw stderr from the subprocess, unmodified
    pub stderr: String,
    pub bytes_transferred: Option<u64>,
}
```

For send/receive composite errors (review finding S2), carry both sides:

```rust
/// Send|receive pipeline can fail on either or both sides.
#[derive(Debug, Clone)]
pub struct SendReceiveContext {
    pub send_error: Option<SubprocessError>,
    pub recv_error: Option<SubprocessError>,
    pub bytes_transferred: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SubprocessError {
    pub exit_code: Option<i32>,
    pub stderr: String,
}
```

The `BtrfsOperation::SendReceive` variant carries `SendReceiveContext` so the
translation layer can inspect both sides independently. This prevents the first-match
problem where a send stderr masks the receive stderr.

**Why this works:**
1. `btrfs.rs` already has exit_code, stderr, and operation in local variables at all
   8 construction sites — they just populate a struct instead of `format!()`.
2. `Display` impl renders a flat one-line summary for log output (same as today).
3. The voice layer receives typed `BtrfsErrorContext` and calls
   `translate_btrfs_error()` — no parsing, no regex, compiler-enforced contract.
4. `btrfs.rs` still doesn't know about remediation, drives, or subvolume names.
   The translation function adds that context from `OperationOutcome` fields.

**`OperationOutcome` enrichment:**

```rust
pub struct OperationOutcome {
    pub operation: BtrfsOperation, // typed enum, not String (review N1)
    pub drive_label: Option<String>,
    pub result: OpResult,
    pub duration: Duration,
    pub error_context: Option<BtrfsErrorContext>, // replaces error: Option<String>
    pub bytes_transferred: Option<u64>,
    pub subvolume: Option<String>,
}
```

The voice layer calls `translate_btrfs_error()` using the typed context directly:

```rust
fn render_operation_error(outcome: &OperationOutcome) {
    if let Some(ref ctx) = outcome.error_context {
        let detail = translate_btrfs_error(ctx, outcome.drive_label.as_deref(),
                                            outcome.subvolume.as_deref());
        // Render layered output
    }
}
```

### Locale hardening

**Decision (review M2):** Set `LC_ALL=C` on all btrfs subprocess calls in `btrfs.rs`.
This forces English stderr regardless of system locale, eliminating the pattern-matching
locale dependency. One-line change per `Command::new()` call (`.env("LC_ALL", "C")`).
This should be done immediately, independent of the structured errors work.

### Voice rendering

Interactive mode renders the full hierarchy:

```
  ERROR  Send failed: htpc-home → WD-18TB
         Destination drive is full
         Why: WD-18TB has insufficient space (transferred 1.1 TB before failure)
         What to do:
           • Check drive space: df -h /run/media/.../WD-18TB
           • Run `urd backup` again — retention may free space first
```

Daemon mode includes the structured detail in JSON:

```json
{
  "error": {
    "summary": "Destination drive is full",
    "cause": "WD-18TB has insufficient space",
    "remediation": ["Check drive space: df -h ...", "Run urd backup again..."],
    "technical": { "operation": "receive", "exit_code": 1, "stderr": "...", "bytes_transferred": 1100000000000 }
  }
}
```

### Exit codes

Adopt the graduated exit code scheme from the UX brainstorm, as a separate concern
that can be implemented alongside or independently:

| Code | Meaning | Current |
|------|---------|---------|
| 0 | All operations succeeded | Same |
| 1 | Partial — some operations failed | Same |
| 2 | Total failure — all operations failed | New (currently 1) |
| 3 | Config error — couldn't start | New (currently 1) |

This is a **backward compatibility concern** (ADR-105) since scripts may check for
`exit 1`. Gate: document the change and provide a transition period, OR keep 0/1 and
add the granular codes only in daemon mode JSON.

**Recommendation:** Keep exit codes 0/1 for now. The structured errors are the
high-value change; exit code granularity can come later without architectural impact.

## Invariants

1. **Translation is pure.** `translate_btrfs_error()` is a function of its input only.
   No I/O, no config access, no filesystem queries. (ADR-108)
2. **Unknown patterns always produce output.** The fallthrough case renders the raw
   error with full technical details. Translation never swallows information.
3. **Raw stderr is always preserved.** `BtrfsTechnical.stderr` contains the unmodified
   btrfs output. The structured message is an addition, not a replacement.
4. **No behavior change.** Translation is presentation only. Error propagation, cleanup
   on failure, partial byte tracking — all unchanged.
5. **Pattern list is data, not scattered code.** All patterns in one array/vec in
   `error.rs`. Adding a new pattern is adding an entry, not modifying control flow.
6. **Subprocess locale is forced to `C`.** All btrfs subprocess calls set `LC_ALL=C`
   so stderr is always English, regardless of system locale. Pattern matching depends
   on this.

## Integration Points

| Module | Change | Scope |
|--------|--------|-------|
| `error.rs` | `BtrfsErrorContext`, `BtrfsErrorDetail`, `BtrfsOperation` enum, `translate_btrfs_error()`, pattern table | New types + function |
| `btrfs.rs` | Construct `BtrfsErrorContext` instead of `format!()` at 8 sites; add `.env("LC_ALL", "C")` to all subprocess calls; split send/receive into `SendReceiveContext` | Mechanical refactor |
| `executor.rs` | `OperationOutcome.operation` becomes `BtrfsOperation` enum; `error: Option<String>` becomes `error_context: Option<BtrfsErrorContext>`; add `subvolume: Option<String>` | Struct change |
| `voice.rs` | Call `translate_btrfs_error()` when rendering errors in backup summary | New render path |
| `output.rs` | Extend daemon JSON error representation | Minor |
| `plan.rs` | **No changes** | — |

**Files affected:** 5 (error.rs, btrfs.rs, executor.rs, voice.rs, output.rs)
**New module:** No (extends error.rs)

## Effort Estimate

Comparable to pre-flight checks (Priority 2c): one module with a table of rules, integration
into voice layer, ~12-15 tests.

- Pattern table + `translate_btrfs_error()`: ~2h
- Context extraction (parsing existing error format): ~1h
- Voice rendering (interactive + daemon): ~2h
- Tests: ~2h (one per pattern + edge cases + unknown fallthrough)
- Integration into backup summary rendering: ~1h

**Total: ~1 session**

## Test Strategy

```
// Pattern matching (pure function, typed context input)
test_translate_no_space_receive → "Destination drive is full"
test_translate_no_space_snapshot → "Local filesystem is full"
test_translate_permission_denied → "Insufficient permissions"
test_translate_read_only_fs → "Drive is read-only"
test_translate_no_such_file_delete → "Snapshot not found at expected path"
test_translate_no_such_file_receive → "Destination directory missing"
test_translate_parent_not_found → "Incremental parent missing"
test_translate_unknown_error → generic with full stderr preserved

// Composite send/receive errors (review S2)
test_translate_send_parent_not_found_recv_no_space → both translated independently
test_translate_send_only_failure → recv_error is None
test_translate_recv_only_failure → send_error is None

// Rendering
test_render_structured_error_interactive → layered output with color
test_render_structured_error_daemon → JSON with all fields
test_bytes_transferred_in_output → "transferred 1.1 TB before failure"

// Fallthrough monitoring (review M3)
test_unknown_pattern_logs_fallthrough → log::debug with raw stderr for monitoring

// Hardening (test-team additions)
test_btrfs_error_display_contains_operation_and_stderr
test_backup_summary_renders_structured_error_instead_of_raw
// LC_ALL=C enforcement: verify via grep in btrfs.rs (all Command::new sites)
```

## Rejected Alternatives

### A. Parse structured context from flattened error strings (original Option A)

**Rejected per arch-adversary review (S1).** The original design recommended leaving
`UrdError::Btrfs { msg, bytes_transferred }` unchanged and regex-parsing the error
string in `voice.rs` to recover exit codes and stderr. This creates a lossy round-trip
with no compiler enforcement — if the format string in `btrfs.rs` changes, the parser
in `voice.rs` silently breaks. Additionally, composite send/receive errors (review S2)
would require splitting on `"; "` which is fragile. Carrying typed context eliminates
the entire parsing layer.

### B. Translate at construction time in `btrfs.rs`

This would require `btrfs.rs` to know about remediation advice, drive labels, and
subvolume names — violating its module boundary (CLAUDE.md: "btrfs.rs does NOT know
about retention, plans, config"). The adopted approach keeps translation in the voice
layer while providing typed input. `btrfs.rs` constructs `BtrfsErrorContext` (which
is its own data) but doesn't translate it.

### C. Regex-based pattern matching

Full regex gives more precision but is harder to maintain and test. Substring matching
covers all observed patterns. If a pattern needs regex later (e.g., extracting a
specific path from stderr), add it as a single-pattern enhancement, not a wholesale
change.

### D. Separate `error_translation.rs` module

A separate module would be cleaner in theory, but the translation types (`BtrfsErrorDetail`,
`BtrfsErrorContext`) are closely coupled to `UrdError::Btrfs`. Keeping them in `error.rs`
avoids circular dependencies and keeps error-related types together. If the pattern table
grows beyond ~20 entries, revisit.

## Open Questions

1. **Should `urd verify` also translate errors it encounters?** Currently verify
   produces its own check results. If verify calls btrfs operations that fail, the
   translation layer would apply. Low priority — verify errors are rare.

2. **Voice tone for errors.** The mythic voice applies to success states. Should errors
   also carry the mythic register, or should they be direct and clinical? Recommendation:
   errors should be direct. "The well overflows" is less helpful than "destination drive
   is full" when you're debugging at 3am.

3. **Sentinel error classification (review OQ3).** The Sentinel will need to make
   decisions based on error types (e.g., "drive full" triggers different awareness than
   "permission denied"). Should `BtrfsErrorContext` / `BtrfsErrorDetail` be the shared
   vocabulary, or does the Sentinel need its own classification? Decide before
   implementation to avoid a second refactor.

4. **How deep should remediation advice go (review OQ1)?** Should remediation include
   actual paths from config (e.g., `df -h /run/media/.../WD-18TB`)? This requires the
   translation function to receive config data, conflicting with "pure function, no config
   access." Resolution: the translation function receives `drive_label` and `subvolume`
   from `OperationOutcome` fields (already available). Generic path templates use these
   names, not full filesystem paths.

## Resolved Questions (from review)

- **Locale dependency (was OQ3):** Resolved by setting `LC_ALL=C` on all btrfs subprocess
  calls. Pattern matching always operates on English stderr.
- **Option A vs typed context (was review focus 1):** Rejected Option A. Typed context
  on `UrdError::Btrfs` is the adopted approach.
- **Pattern table vs match block (was review focus 2):** Data table confirmed as right
  choice. Domain (arbitrary stderr strings) is inherently open-ended; exhaustiveness
  checking buys little. Fallthrough handler is the correct substitute.
- **Exit codes (was review focus 3):** Deferred, confirmed correct. Revisit when Sentinel
  needs granular exit discrimination.

## Review Findings Addressed

This design was reviewed by arch-adversary on 2026-03-26. All findings addressed:

| Finding | Severity | Resolution |
|---------|----------|------------|
| S1. String-parsing reconstruction is fragile | Significant | Rejected Option A. Adopted typed `BtrfsErrorContext` on `UrdError::Btrfs`. Eliminates regex parsing entirely. |
| S2. Composite send/receive errors break pattern matcher | Significant | `SendReceiveContext` carries separate `send_error` and `recv_error`. Each side translated independently. |
| S3. `OperationOutcome` enrichment insufficient | Significant | `OperationOutcome` now carries typed `BtrfsOperation` enum and `error_context: Option<BtrfsErrorContext>`. |
| M1. Pattern 5 normalizes abnormal condition | Moderate | Reframed as "Snapshot not found at expected path" with dual-interpretation remediation. |
| M2. Locale dependence under-mitigated | Moderate | `LC_ALL=C` on all btrfs subprocess calls. One-line change, eliminates risk. |
| M3. No staleness detection for pattern table | Moderate | Added `log::debug` on fallthrough for monitoring. Spike after btrfs upgrade signals stale patterns. |
| N1. `BtrfsOperation` duplicates `OperationOutcome.operation` | Minor | Unified: `OperationOutcome.operation` becomes `BtrfsOperation` enum. |
| N2. Effort estimate optimistic for Option A | Minor | Moot — Option A rejected. Typed context approach is closer to original estimate. |

[Review report](../99-reports/2026-03-26-structured-errors-design-review.md)
