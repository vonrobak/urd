# Arch-Adversary Review: Structured Error Messages Design
**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-03-26
**Scope:** Design proposal -- `docs/95-ideas/2026-03-26-design-structured-errors.md`
**Review type:** Design review (pre-implementation)

---

## 1. Executive Summary

This design proposes a pure-function translation layer that pattern-matches btrfs stderr
strings into structured (summary / cause / remediation / technical) error messages. The
architecture is sound in principle -- presentation-only, no behavior change, raw stderr
preserved -- but the recommended integration path (Option A: parse structured context back
out of flattened error strings at render time) introduces a fragile intermediate
representation that will silently degrade as error message formats drift. The design should
be redirected toward carrying structured context through the executor rather than
reconstructing it from strings.

## 2. What Kills You (Catastrophic Failure Proximity)

This design is **presentation-only** and explicitly declares "no behavior change." That is
its greatest safety property. It does not touch the planner, does not alter error
propagation, does not change cleanup-on-failure logic. For a backup tool where the
catastrophic failure mode is silent data loss, a presentation-only change is low-risk by
nature.

However, two failure modes deserve attention:

**Misclassification leading to wrong remediation.** Pattern 5 maps "No such file or
directory" on delete to "Snapshot already removed" -- framing the error as benign. If the
actual cause is a filesystem corruption or a wrong path (not a stale target), the user
receives reassurance when they should be alarmed. The translation layer cannot distinguish
these cases from substring matching alone. This is not catastrophic on its own -- the raw
stderr is preserved -- but it reduces the probability that a user investigates a real
problem.

**Future btrfs stderr format changes.** When btrfs changes its error strings (which it has
done between versions), patterns silently stop matching and fall through to the generic
handler. This is the correct failure mode -- it degrades to today's behavior, not to
information loss. No catastrophic risk here.

**Verdict:** Low proximity to catastrophic failure. The design respects the "no behavior
change" boundary. The main risk is diagnostic misdirection, not data loss.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 3/5 | Option A's string-parsing reconstruction of structured context is fragile and adds an unnecessary failure mode to a system that already has the data in typed form |
| **Security** | 4/5 | No new attack surface; sudo/btrfs interaction unchanged; remediation advice could theoretically leak path info in logs but this is marginal |
| **Architectural Excellence** | 4/5 | Pure function, data-driven pattern table, presentation-only scope, invariants well-stated; docked for choosing the wrong integration point |
| **Systems Design** | 3/5 | The design creates information, destroys it (flattening to string in btrfs.rs), then attempts to reconstruct it (parsing in voice.rs) -- a classic lossy round-trip |

**Overall: 3.5/5 -- Solid concept, wrong plumbing.**

## 4. Design Tensions

### Tension 1: Module Purity vs. Information Preservation

The design argues Option A keeps `btrfs.rs` focused on subprocess management. True. But
`btrfs.rs` already formats structured data (exit code, stderr, operation name) into a
string, destroying structure. Option A then proposes regex-parsing that string to recover
the structure. The module boundary is maintained at the cost of a lossy round-trip. The
cleaner resolution is not Option B (translation in btrfs.rs) but a **third option**: carry
`BtrfsErrorContext` as a typed field on `UrdError::Btrfs` alongside or instead of the
`msg: String`, and let the voice layer translate from the typed context. This preserves
both the module boundary and the information.

### Tension 2: Data Table vs. Match Block

The pattern table as `&[ErrorPattern]` with function pointers is extensible but not
exhaustive. A `match` block on an enum is exhaustive but less extensible. For a table of
7 patterns that may grow to 15, the data table is the right call -- exhaustiveness checking
buys little when the domain (arbitrary stderr strings) is inherently open-ended. The
fallthrough handler is the correct substitute for exhaustiveness.

### Tension 3: Helpful Framing vs. Diagnostic Accuracy

Translating "No such file or directory" on delete into "Snapshot already removed" is
helpful when true and misleading when false. The design optimizes for the common case
(stale retention target) at the cost of the uncommon case (filesystem corruption,
misconfigured path). This tension is inherent in any translation layer and cannot be fully
resolved, but the design should acknowledge it explicitly and ensure the raw stderr is
rendered prominently enough that a user who reads past the summary will see the truth.

### Tension 4: Backward Compatibility of Exit Codes

Deferring exit code granularity is pragmatic. The structured errors deliver 90% of the
value; exit codes can follow. But the design should note that the deferred exit codes
intersect with the Sentinel/daemon work -- the Sentinel will need to distinguish "config
error" from "partial failure" to make awareness decisions. If exit codes are deferred too
long, the Sentinel will parse strings instead, recreating the same problem this design
solves.

## 5. Findings

### Critical

None.

### Significant

**S1. Option A's string-parsing reconstruction is a design smell.**

The design recommends parsing exit codes via `\(exit (-?\d+)\)` regex and extracting stderr
via splitting on `": "`. This works today because `btrfs.rs` formats errors as `"send
failed (exit 1): <stderr>"`. But this format is not a contract -- it is an implementation
detail of `btrfs.rs`. If someone changes the format string (adds context, rewords, changes
punctuation), the parser in `voice.rs` silently breaks. The design creates a hidden coupling
between `btrfs.rs` error formatting and `voice.rs` error parsing with no compiler
enforcement.

**Recommendation:** Do not parse strings. Instead, add `BtrfsErrorContext` (or a simpler
typed struct with operation, exit_code, stderr, bytes_transferred) as a field on
`UrdError::Btrfs`. This is a smaller change than the design suggests -- it touches `btrfs.rs`
construction sites (8 locations, mechanical change) but eliminates the parsing layer entirely
and makes the coupling explicit and type-checked. The `Display` impl on `UrdError::Btrfs`
can still render the flat string for log output.

**S2. Combined send/receive error messages break the pattern matcher.**

When both send and receive fail (lines 214-231 of `btrfs.rs`), the error message is
semicolon-joined: `"send failed (exit 141): <send stderr>; receive failed (exit 1): <recv
stderr>"`. The pattern table matches on substrings of the full message. If the send stderr
contains "No space left on device" (it won't -- the receiver does), pattern matching works.
But if the send fails with "parent not found" AND the receive fails with "No space left on
device", the first-match-wins rule will match pattern 7 (parent not found) and miss the
space issue entirely. The design does not address composite errors.

**Recommendation:** If sticking with Option A, the parser must split on `"; "` and translate
each half independently, or the design must document that composite errors always match on
the first component. Better: carry separate send/receive error contexts in the typed struct.

**S3. The `subvolume` field addition to `OperationOutcome` is insufficient.**

The design proposes adding only `subvolume: Option<String>` to `OperationOutcome`. But the
translation function also needs `operation` parsed as `BtrfsOperation` (not the string
"send_full" / "send_incremental") and `exit_code` and `stderr` as separate fields. If the
design is going to enrich `OperationOutcome` at all, it should carry the full typed context
rather than a single field that forces the rest to be string-parsed.

### Moderate

**M1. Pattern 5 ("Snapshot already removed") normalizes a potentially abnormal condition.**

"No such file or directory" on delete could mean: (a) snapshot was already deleted (benign),
(b) path is misconfigured (bug), (c) filesystem corruption (serious). Labeling all three as
"Snapshot already removed" is misleading for cases (b) and (c). The remediation advice
("this is normal") actively discourages investigation.

**Recommendation:** Use a more neutral summary like "Snapshot not found at expected path"
and include remediation that covers both benign and concerning interpretations: "If this
snapshot was already deleted, this is safe to ignore. If unexpected, check the subvolume
path configuration."

**M2. Locale dependence is acknowledged but under-mitigated.**

The design notes that btrfs stderr language depends on system locale and accepts this as
"acceptable for now." This is a reasonable call, but the design should add a concrete
mitigation: set `LC_ALL=C` on btrfs subprocess calls in `btrfs.rs`. This is a one-line
change that eliminates the locale risk entirely and is standard practice for tools that parse
subprocess output. The fact that this is not proposed suggests the author did not consider
the fix, only the problem.

**M3. No versioning or staleness detection for the pattern table.**

As btrfs evolves, patterns may stop matching or match incorrectly. The design has no way to
detect this. Consider adding a metric or log entry for "pattern table fallthrough rate" --
if it spikes after a btrfs upgrade, someone knows to update the patterns.

### Minor

**N1. `BtrfsOperation` enum duplicates information already in `OperationOutcome.operation`.**

The design introduces `BtrfsOperation { Snapshot, Send, Receive, Delete }` but
`OperationOutcome.operation` is a string ("snapshot", "send_full", "send_incremental",
"delete", "retention"). These should converge. Either `OperationOutcome.operation` becomes
a typed enum, or `BtrfsOperation` parses from the string. Having both is a maintenance
hazard.

**N2. The effort estimate of "~1 session" is optimistic if Option A is chosen.**

The string-parsing layer (extracting exit codes, splitting composite errors, handling edge
cases) will consume more time than estimated. If the design switches to typed context
propagation, the estimate is more realistic.

### Commendation

**C1. The "no behavior change" invariant is the right call.**

Separating presentation improvement from error-handling logic changes is exactly correct for
a backup tool. This design can be shipped, tested, and iterated without any risk to the
backup pipeline itself. The author clearly internalized the catastrophic failure concern.

**C2. The fallthrough handler preserves raw stderr unconditionally.**

The guarantee that unknown patterns produce full technical output means the translation
layer can never make things worse than today. This is the right default for a safety-critical
tool.

**C3. Rejected alternatives are well-reasoned.**

The document considers four alternatives and rejects them with clear rationale. The reasoning
for keeping translation out of `btrfs.rs` (module boundary) and against a separate module
(coupling) is sound. The only gap is not considering the hybrid option of typed context on
the error type.

## 6. The Simplicity Question

> Does this design add complexity the user must manage?

No. The user sees better error messages. There is no new configuration, no new commands, no
new concepts. This passes the CLAUDE.md simplicity test.

> Does this design add complexity the developer must manage?

Option A: Yes. The string-parsing layer is an ongoing maintenance burden -- every change to
error formatting in `btrfs.rs` must be tested against the parser in `voice.rs`, with no
compiler help.

Option B: Moderate. Touching 8 construction sites is mechanical but noisy.

**Hybrid option (typed context on `UrdError::Btrfs`):** Lowest ongoing complexity. The
construction sites change minimally (they already have the data in typed form -- they just
stop formatting it into a string). The voice layer receives typed input. The compiler
enforces the contract.

## 7. For the Dev Team (Prioritized Action Items)

1. **Reject Option A. Carry typed context on `UrdError::Btrfs`.**
   Replace `msg: String` with a `BtrfsErrorContext` struct (or add it alongside `msg`).
   This eliminates the string-parsing layer, makes the coupling explicit, and is a smaller
   conceptual change than it appears. The 8 construction sites in `btrfs.rs` already have
   exit_code, stderr, and operation in local variables -- they just need to populate a struct
   instead of `format!()`. Keep `Display` for log/debug output. Priority: do this before
   writing any code.

2. **Split composite send/receive errors.**
   Whether using typed context or string parsing, the combined "send failed; receive failed"
   message must be handled as two separate errors for translation. With typed context, carry
   `send_error: Option<SubprocessError>` and `recv_error: Option<SubprocessError>`
   separately.

3. **Neutralize Pattern 5's framing.**
   Change "Snapshot already removed" to "Snapshot not found at expected path" with
   dual-interpretation remediation advice.

4. **Set `LC_ALL=C` on btrfs subprocess calls.**
   One-line change in `btrfs.rs` that eliminates the locale parsing risk. Do this regardless
   of the structured errors work.

5. **Add `BtrfsOperation` as a typed enum to `OperationOutcome`.**
   Unify with the string `operation` field. This is a good cleanup independent of the
   structured errors work.

6. **Defer exit codes (as proposed).**
   The design's recommendation to keep 0/1 is correct. Revisit when the Sentinel needs
   granular exit discrimination.

## 8. Open Questions

1. **How deep should remediation advice go?** The design includes specific commands
   (`df -h /run/media/.../WD-18TB`). Should these include actual paths from the config, or
   generic placeholders? Actual paths are more useful but require the translation function
   to receive config data, which conflicts with the "pure function, no config access"
   invariant. The design should resolve this tension explicitly.

2. **Should the translation layer be tested against real btrfs stderr?** The test strategy
   shows synthetic inputs. Consider capturing real stderr from the observed failures
   (listed in the Problem section) and using those as golden test inputs. This protects
   against subtle format assumptions.

3. **What is the interaction with the Sentinel's error handling?** The Sentinel will need
   to make decisions based on error types (e.g., "drive full" triggers a different awareness
   state than "permission denied"). Should `BtrfsErrorContext` / `BtrfsErrorDetail` be the
   shared vocabulary, or does the Sentinel need its own error classification? This should be
   decided before implementation to avoid a second refactor.

4. **Should the pattern table live in a separate data file (TOML/JSON)?** If pattern
   updates should not require recompilation (e.g., user-contributed patterns for unusual
   btrfs builds), an external table would be appropriate. If patterns are always developer-
   maintained, a Rust array is fine. The current design assumes the latter, which is
   reasonable for now.
