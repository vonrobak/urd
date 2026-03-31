# Arch-Adversary Implementation Review: Phase 1 — Vocabulary Landing

**Date:** 2026-04-01
**Artifact:** Implementation on `master` (commit cc3fd69 and prior merges)
**Files reviewed:** `src/voice.rs`, `src/error.rs`, `src/cli.rs`, `src/notify.rs`, `src/output.rs`, `src/awareness.rs`, `src/sentinel_runner.rs`
**Type:** Implementation review (code on master)
**Reviewer:** arch-adversary

---

## 1. Executive Summary

The Phase 1 vocabulary landing is **well-executed in the four target files** (voice.rs,
error.rs, cli.rs, notify.rs) with correct boundary discipline. The ChainHealth Display
impl is untouched, the exposure triad (sealed/waning/exposed) is properly implemented,
the summary line uses the differentiated approach, and role-aware drive vocabulary works
correctly.

However, the implementation has one significant gap: **`sentinel_runner.rs` was not
included in the vocabulary changes** and retains 5 instances of deprecated mythology
(loom, weave, rewoven, unguarded). This creates a vocabulary inconsistency between
`notify.rs` (the backup-path notification builder, correctly updated) and
`sentinel_runner.rs` (the sentinel-path notification builder, not updated). Users
receiving notifications from the Sentinel will see old vocabulary while backup
notifications use new vocabulary.

---

## 2. What Kills You

**Catastrophic failure mode for Urd: silent data loss through incorrect snapshot deletion.**

This implementation is **far from the kill zone**. Confirmed:

- **No path to silent data loss.** Every change is a string literal in a rendering or
  notification function. No computation, retention logic, planner decisions, or executor
  behavior was modified.
- **No pin file changes.** Pin file paths, names, and read/write logic are untouched.
- **No snapshot deletion logic changes.** Retention module untouched.
- **No btrfs command changes.** `BtrfsOps` trait and implementations untouched.

**Distance from catastrophic failure: 3+ bugs away.** Comfortable.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | Core vocabulary landed correctly. sentinel_runner.rs omission creates user-facing inconsistency. |
| **Security** | 5/5 | No security-relevant surface. No I/O changes, no privilege changes, no new inputs. |
| **Architectural Excellence** | 4/5 | Excellent boundary discipline in the four target files. Stale comments in two locations. Missed a fifth file. |
| **Systems Design** | 4/5 | Design review findings mostly resolved well. The sentinel_runner.rs parallel notification builder was a blind spot. |
| **Backward Compatibility** | 5/5 | All on-disk contracts preserved. ChainHealth Display untouched. PromiseStatus Display untouched. Heartbeat strings untouched. Prometheus metrics untouched. SafetyCounts serde fields untouched. |
| **Test Coverage** | 4/5 | Good test updates: new vocabulary assertions, daemon JSON contract tests, role-aware drive tests. Missing: unit test for `exposure_label()` function directly, missing: unit test for `render_thread_status()` function directly. |

**Overall: 4.3/5** — solid implementation with one meaningful omission.

---

## 4. Design Review Findings Resolution

### S-1: ChainHealth rendering fork (RESOLVED CORRECTLY)

The `Display` impl on `ChainHealth` in `output.rs` lines 70-78 is **untouched** — still
produces `"none"`, `"full ({reason})"`, `"incremental ({pin})"`. The interactive rendering
fork is correctly implemented via `render_thread_status()` (voice.rs line 263), called at
line 241 instead of `.to_string()`. The function has a clear doc comment: "The `Display`
impl on `ChainHealth` feeds daemon JSON and must not change."

### S-2: Role-aware "away" (RESOLVED CORRECTLY)

The subvolume table drive cell at voice.rs line 228 correctly uses `e.role` from
`StatusDriveAssessment` (which carries `DriveRole`):

```rust
Some(e) if e.role == DriveRole::Offsite && e.last_send_age_secs.is_some() => {
    "away".dimmed().to_string()
}
```

The `render_drive_summary()` at line 336 also correctly checks `drive.role == DriveRole::Offsite`
for the "away" vs "disconnected" distinction. Both sites use the correct struct field.

### M-1: Summary line semantics (RESOLVED — DIFFERENTIATED APPROACH)

The implementation chose the differentiated approach from the design review options:

```rust
"6 of 9 sealed. htpc-root exposed. docs waning."
```

Lines 86-105 correctly separate exposed (UNPROTECTED) and waning (AT RISK) subvolumes
into distinct sentence fragments with appropriate vocabulary. This is the best of the
three options — accurate and concise.

### M-2: "Chain error" log grep change (PARTIALLY RESOLVED)

The `UrdError::Chain` error message at error.rs line 275 was changed to
`"Thread error: {0}"`. The `translate_btrfs_error()` messages at lines 237, 241 now use
"thread broken" and "thread health". However, **no CHANGELOG entry documents this string
change**. For a pre-1.0 project this is acceptable, but should be noted in the next
CHANGELOG update.

### M-3: PROTECTION column transitional note (RESOLVED)

Voice.rs line 172 has the transitional note:
```rust
// NOTE: Level names (guarded/protected/resilient) stay until Phase 6
```

This appears in both `render_subvolume_table()` and `render_assessment_table()` (line 696).
Implementers will not accidentally rename level Display impls.

### N-1: CLI description for status (RESOLVED — DESIGN VERSION)

`cli.rs` line 27: `"Check whether your data is safe"` — the design version was chosen
over the brainstorm version. Consistent with the intent-first UX philosophy.

### N-2: "unguarded" notification (PARTIALLY RESOLVED)

`notify.rs` line 242: correctly changed to `"your data stands exposed."`

**BUT** `sentinel_runner.rs` line 511: still says `"your data stands unguarded."` — this
parallel notification builder was missed. See finding S-1 below.

### N-3: "Drives:" prefix (KEPT)

The `render_drive_summary()` function at voice.rs lines 317, 329, 341 retains the
`"Drives: "` prefix on every line. This was not explicitly resolved in the design but the
implementation decision to keep it is reasonable — the prefix anchors the section visually.

---

## 5. Findings

### Significant

**S-1: sentinel_runner.rs notification builder was not updated with vocabulary changes.**

The Sentinel has a parallel notification builder (`build_notifications()` at line 441 and
`build_health_notifications()` at line 565) that constructs the same notification types as
`notify.rs` but operates on `SubvolAssessment` directly (not `Heartbeat`). This parallel
builder was **not included** in the Phase 1 vocabulary changes.

Five instances of deprecated vocabulary remain:

| Line | Current (old) | Expected (new) |
|------|---------------|-----------------|
| 476 | "the weave grows thin" | "the thread grows thin" |
| 491 | "is rewoven" | "is mended" |
| 511 | "stands unguarded" | "stands exposed" |
| 552 | "The loom sits idle" | "The spindle sits idle" (or similar) |
| 596, 610 | "The loom for {}" | needs mythology cleanup |

**Impact:** Users receiving notifications from the Sentinel daemon will see "loom" and
"weave" vocabulary while backup-path notifications use "thread" and "mended." This is a
user-facing inconsistency that undermines the vocabulary landing's purpose.

**Why significant:** The Phase 1 design explicitly listed notification mythology cleanup
as Change 6, and `notify.rs` was correctly updated. But `sentinel_runner.rs` was not listed
in the design's Module Mapping table, creating a blind spot. The design listed only 4 files;
`sentinel_runner.rs` is a 5th that constructs the same notification types.

**Fix:** Apply the same string substitutions to `sentinel_runner.rs` lines 476, 491, 511,
552, 596, and 610.

---

### Moderate

**M-1: Two stale comments reference old column names.**

- Voice.rs line 166: Comment says `CHAIN` but the actual header is `THREAD`.
- Voice.rs line 693: Comment says `[PROMISE]` but the actual header is `PROTECTION`.

These are maintenance hazards — a future developer reading the comment will have a
different mental model than what the code produces.

**Fix:** Update both comments.

---

**M-2: No dedicated unit tests for `exposure_label()` and `render_thread_status()`.**

These two new helper functions have specific mapping behavior:

- `exposure_label()`: "PROTECTED" -> "sealed", "AT RISK" -> "waning", "UNPROTECTED" -> "exposed", unknown -> passthrough.
- `render_thread_status()`: NoDriveData -> em dash, Incremental -> "unbroken", Full -> "broken -- full send (reason)".

They are tested *indirectly* through the full `render_status()` integration tests (e.g.,
`safety_column_uses_new_vocabulary`, `interactive_contains_thread_health`). However,
dedicated unit tests would catch regressions more precisely and serve as documentation of
the mapping contract.

**Recommendation:** Add 2 focused unit tests exercising each mapping case directly.

---

**M-3: `[OFF]` skip tag alignment uses trailing space, but `[SPACE]` is still wider.**

```
[SPACE]  — 7 chars (including brackets)
[WAIT]   — 6 chars
[AWAY]   — 6 chars
[OFF]    — 5 chars + 1 trailing space = 6 chars
[SKIP]   — 6 chars
```

`[SPACE]` is one character wider than all others. In practice this creates a 1-character
misalignment in grouped skip output when space-exceeded skips appear alongside other skip
types. The visual impact is minimal since space-exceeded skips render individually (not
grouped), so they rarely appear adjacent to grouped skips. But the inconsistency exists.

**Recommendation:** Either pad all tags to 7 chars or accept the 1-char variance. The
current state is acceptable.

---

### Minor

**N-1: `SafetyCounts` struct uses old vocabulary field names (ok/aging/gap).**

`output.rs` lines 608-612: The `SafetyCounts` struct has serde-serialized field names
`ok`, `aging`, `gap` — the old vocabulary. These are consumed by the Spindle/tray icon
and are an on-disk/API contract. They **must not** change without migration. However, the
doc comment "Safety axis counts using tray-friendly vocabulary" will become confusing as
the voice layer vocabulary diverges. Consider adding a comment noting the divergence:
"These field names are a serde contract; the voice layer uses sealed/waning/exposed."

---

**N-2: `chain_health` daemon JSON field name retained (correct, but undocumented).**

The daemon JSON serializes `chain_health` as the field name. The internal struct names
(`ChainHealth`, `ChainHealthEntry`, `chain_health`) are unchanged. This is correct
(ADR-105 backward compatibility), but should be noted as an intentional decision for the
same reason as M-3 in the design review: implementers might feel the urge to rename the
field to `thread_health` in a future phase.

---

**N-3: No CHANGELOG entry for vocabulary changes.**

The Phase 1 vocabulary landing is a user-facing change (every `urd status` output now
uses different words). The `[Unreleased]` section of CHANGELOG.md should note this.

---

### Commendation

**C-1: Excellent boundary discipline on ChainHealth.**

The most dangerous part of this refactor was the `ChainHealth` boundary between interactive
and daemon output. The implementation handles it perfectly: a new `render_thread_status()`
function with an explicit doc comment warning against changing the `Display` impl, called
at precisely the right site. The daemon JSON contract is preserved.

**C-2: Differentiated summary line is well-implemented.**

The three-way split (all sealed / some exposed + some waning / mixed) produces precise,
accurate output. The implementation correctly collects exposed and waning names separately
and constructs distinct sentence fragments. This is better than the original design's
"`{names} exposed`" proposal, which the design review (M-1) correctly flagged as
semantically imprecise.

**C-3: Role-aware drive vocabulary is clean.**

Both the drive summary and the subvolume table cells correctly dispatch on `DriveRole` from
the appropriate struct. The offsite -> "away", primary -> "disconnected" distinction is
implemented consistently across all render sites.

**C-4: Skip tag system is well-designed.**

The `skip_tag()` helper function centralizes the tag-to-color mapping. The five tags
([WAIT], [AWAY], [SPACE], [OFF], [SKIP]) are used consistently across both individual and
grouped renderers. The dimmed coloring for informational tags vs. yellow for space warnings
is a good UX choice.

---

## 6. Backward Compatibility Verification

| Contract | Status | Evidence |
|----------|--------|----------|
| `ChainHealth::Display` impl | PRESERVED | output.rs lines 70-78 unchanged; test `chain_health_display` passes |
| `PromiseStatus::Display` impl | PRESERVED | awareness.rs lines 50-58 unchanged |
| Heartbeat `promise_status` strings | PRESERVED | Heartbeat writes `PromiseStatus::Display` output |
| Prometheus metrics | PRESERVED | metrics.rs not in diff |
| Daemon JSON field names | PRESERVED | serde attributes unchanged; `daemon_produces_valid_json` test passes |
| `SafetyCounts` serde fields | PRESERVED | output.rs lines 608-612 unchanged |
| `SentinelStateFile` serde schema | PRESERVED | output.rs sentinel types unchanged |
| Pin file format | PRESERVED | Not in scope; no changes |
| Snapshot name format | PRESERVED | Not in scope; no changes |

**All on-disk contracts are intact.**

---

## 7. Test Coverage Assessment

**Current state:** 589 tests, all passing, clippy clean.

| Area | Coverage | Gap |
|------|----------|-----|
| Exposure labels (sealed/waning/exposed) | Covered via `safety_column_uses_new_vocabulary` | No direct `exposure_label()` unit test |
| Thread rendering (unbroken/broken) | Covered via `interactive_contains_thread_health` | No direct `render_thread_status()` unit test |
| Role-aware "away" | Covered via `unmounted_drive_shows_away` | Only tests Offsite->away; no test for Primary->dash |
| Differentiated summary line | Covered via `summary_line_all_safe_all_healthy` | No test for mixed exposed+waning summary |
| Skip tags | Covered indirectly via grouped skip tests | No direct `skip_tag()` unit test |
| Daemon JSON contract | Covered via `daemon_produces_valid_json`, `daemon_contains_subvolume_data` | Good |
| CLI descriptions | Not unit-tested | Clap generates these; testing is low value |
| Notify mythology | Covered via existing `compute_notifications` tests | Assertions check title/urgency, not body text |
| Sentinel notifications | Not tested for vocabulary | Gap: sentinel_runner.rs vocabulary mismatch |

**Missing test cases (prioritized):**

1. **Primary drive disconnected shows dash, not "away"** — role-aware behavior for non-offsite
   drives is untested. A test with a Primary drive that is unmounted should verify the cell
   shows em dash rather than "away".

2. **Mixed exposed+waning summary line** — the differentiated summary output with both
   exposed and waning subvolumes present is not directly tested.

3. **Direct `exposure_label()` exhaustive test** — all three inputs plus unknown passthrough.

---

## 8. For the Dev Team

Prioritized action items:

1. **[S-1] Update sentinel_runner.rs vocabulary.** Five string replacements in the parallel
   notification builder. This is the only finding that creates user-visible inconsistency.

2. **[M-1] Fix stale comments.** Two locations: voice.rs line 166 (`CHAIN` -> `THREAD`),
   voice.rs line 693 (`[PROMISE]` -> `[PROTECTION]`).

3. **[M-2] Add focused unit tests** for `exposure_label()` and `render_thread_status()`.

4. **[N-3] Add CHANGELOG entry** documenting the vocabulary change.

5. **[M-3, N-1, N-2] — informational.** No action required; awareness is sufficient.

---

## 9. The Simplicity Question

**Is this implementation as simple as it could be while achieving its goals?**

Yes. The implementation is a clean mechanical application of the design: string literals
changed in rendering functions, one new helper function (`render_thread_status()`) for the
daemon JSON boundary, one extracted helper (`skip_tag()`) for consistent tag rendering. No
new types, no new abstractions, no structural changes.

The only complexity addition is the differentiated summary line (lines 86-105), which
correctly replaces the generic "needs attention" with precise exposed/waning vocabulary.
This is more code than the original single-phrase approach but produces more accurate output.
The complexity is justified.
