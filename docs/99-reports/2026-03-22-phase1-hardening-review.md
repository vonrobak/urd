# Urd Phase 1 Hardening Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Phase 1 hardening changes — 7 priority items from architectural review, applied to config.rs, types.rs, plan.rs, plan_cmd.rs, drives.rs, CLAUDE.md
**Reviewer:** Architectural Adversary (Claude Opus 4.6)
**Prior review:** docs/99-reports/2026-03-22-phase1-arch-review-v3.md

---

## 1. Executive Summary

The hardening changes successfully address all 7 priority items from the architectural review. The most important change — unsent snapshot protection — is correctly implemented and well-tested. The `PinParent` removal elegantly resolves the send/pin ordering dependency by making pin a property of the send operation. The PathBuf migration and path validation close the security gap before Phase 2 introduces execution. The codebase is tighter, safer, and ready for Phase 2.

---

## 2. What Kills You

The catastrophic failure mode remains **silent data loss**: retention deleting the last copy of irreplaceable data.

**Current distance from catastrophe after hardening:** Significantly improved.

- **Before:** Unsent snapshots could be deleted by local retention. One executor bug away from data loss.
- **After:** Unsent snapshots are now protected. The planner treats any snapshot newer than the oldest pin as implicitly pinned when `send_enabled` is true. When no pin exists at all (nothing has ever been sent), every local snapshot is protected. This eliminates the most direct path to silent data loss.

The remaining risk lives in the retention algorithm itself (the 30-day month approximation noted in the prior review, unchanged here) and in Phase 2's executor implementation. The foundation is now sound.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Unsent protection logic is correct. One edge case worth noting (see 5.1). |
| Security | 4 | Path validation closes the traversal gap. PathBuf eliminates lossy conversions. |
| Architectural Excellence | 5 | pin_on_success grouping, subvolume_name on all ops, and PathBuf migration all improve the type system's expressiveness. |
| Systems Design | 4 | Unsent protection prevents the most dangerous failure mode. Future-date warning adds observability. |
| Rust Idioms | 4 | Clean use of PathBuf, Option tuple for pin_on_success, Component::ParentDir for validation. |
| Code Quality | 4 | 7 new tests cover the hardening changes. op_subvolume_name replaces fragile path heuristic. |

---

## 4. Design Tensions

### 4.1 Unsent protection: protect-all vs. protect-newer-than-oldest-pin

The implementation protects all snapshots newer than the oldest pin across all drives. When no pin exists, it protects everything.

**Trade-off:** This is conservative — it may prevent retention from cleaning up snapshots that *have* been sent to some drives but not all. A snapshot sent to WD-18TB but not WD-18TB1 is still protected because the oldest pin (from WD-18TB1, which might be much older) is the threshold.

**Verdict:** Correct choice. For a backup tool, conservative means "don't delete data that might be the only copy." The alternative — tracking per-drive sent status — would require knowing which snapshots exist on which drives, which the pin file abstraction doesn't provide (pins only record the *last* sent snapshot, not all sent snapshots). The conservative approach is the right one given the data model.

### 4.2 pin_on_success as Option<(PathBuf, SnapshotName)> vs. separate struct

The pin information is stored as a tuple inside an Option. A named struct would be more self-documenting (`PinInfo { file: PathBuf, snapshot: SnapshotName }`).

**Verdict:** The tuple is fine for now — it has exactly two fields, and the doc comment explains them. If the pin operation gains more fields (e.g., Phase 2 might need a `drive_label` for logging), promote to a struct then. Premature structuring is not a virtue.

---

## 5. Findings by Dimension

### 5.1 Correctness

**[Moderate] Unsent protection doesn't account for the pin snapshot itself.**

`plan.rs` line 248: `if snap > oldest` — this uses strict greater-than. The pin snapshot itself (the oldest pin) is already in the `pinned` set and is therefore protected by the existing mechanism. However, a snapshot that has the *exact same datetime* as the pin but a different short_name would not be caught by `>` (it would be equal). Since `SnapshotName::Ord` sorts by datetime then short_name, two snapshots at the same datetime with different names would have `snap > oldest` be true for the one with the later short_name and false for the one with the earlier short_name.

In practice this is a non-issue: the pin always points to a specific snapshot name, and two snapshots at the exact same minute with different short_names would mean two different subvolumes, which are processed independently. But the comparison could use `>=` without harm — the pin is already in the set and inserting a duplicate is a no-op.

**Consequence:** None in practice. Theoretical edge case only.

**[Commendation] The three-branch structure of unsent protection is clean and correct.**

```
send_enabled + has pins → protect newer than oldest pin
send_enabled + no pins  → protect everything
send_disabled           → no protection (normal retention)
```

Each branch has a clear invariant, and the tests cover all three. The comment explaining "why" (`plan.rs` lines 238-241) is exactly the right level of documentation — it explains the consequence that motivates the code, not just what the code does.

**[Commendation] pin_on_success resolves the send/pin ordering dependency elegantly.**

The prior review flagged `PinParent` as a separate operation with an implicit ordering dependency on the preceding Send. The fix — embedding pin info as a field on the Send operation — makes the dependency structural. The executor cannot accidentally execute a pin without having access to the send result, because they're the same operation. This is the kind of fix that eliminates a class of bugs rather than patching one instance.

### 5.2 Security

**[Commendation] Path validation is well-designed.**

`validate_path_safe` uses `Component::ParentDir` matching instead of string-based `..` detection. This is correct — it handles paths like `/data/something../foo` (which contains ".." as a substring but not as a path component) without false positives. The use of `std::path::Component` is idiomatic Rust and leverages the stdlib's path parsing.

`validate_name_safe` correctly blocks `/`, `\`, `..`, and null bytes in labels, names, and snapshot_root. The string-based `..` check here is appropriate because these are name components, not full paths.

**[Minor] `validate_name_safe` allows names starting with `.`.**

A drive label or subvolume name starting with `.` (e.g., `.hidden`) would pass validation. In `external_snapshot_dir`, this would create a hidden directory (`.hidden` under `.snapshots`). In `read_snapshot_dir`, hidden entries are skipped (line 493: `if name_str.starts_with('.') { continue; }`). This means a subvolume with short_name `.foo` would create snapshots that the planner's own directory reader would skip.

**Consequence:** A config typo with a leading dot could cause snapshots to be created but never seen by retention or send logic. Low risk — no one would intentionally name a subvolume `.foo` — but a validation check for leading dots would catch it.

### 5.3 Architectural Excellence

**[Commendation] subvolume_name on all PlannedOperation variants makes the plan self-describing.**

The prior review flagged `op_belongs_to` as fragile path-heuristic code. The fix — adding `subvolume_name` to every variant — is the right structural change. The new `op_subvolume_name` function is 5 lines of exhaustive pattern matching, impossible to get wrong. The old version was 15 lines of path inspection that could silently misclassify operations.

**[Commendation] PathBuf migration eliminates a category of silent corruption.**

Config paths are now `PathBuf` throughout. The `to_string_lossy()` roundtrip in `expand_paths()` is gone. `expand_tilde` correctly handles the `&Path` input, falling through to `path.to_path_buf()` for non-UTF-8 paths (which cannot meaningfully start with `~`). This is clean and correct.

### 5.4 Systems Design

**[Commendation] Future-date warning is proportional and non-disruptive.**

The `log::warn!` on future-dated snapshots (`plan.rs` lines 179-190) is the right response — it's informational, doesn't change behavior, and gives the operator a diagnostic breadcrumb. A future-dated snapshot is not an error (it could be from a legitimate timezone change), but it has surprising consequences (suppressed automatic snapshots), so warning is correct.

### 5.5 Rust Idioms

**[Minor] `#[allow(unused_imports)]` on `PathBuf` in plan.rs.**

Line 3: `#[allow(unused_imports)] use std::path::{Path, PathBuf};` — `PathBuf` is used in tests (e.g., `PathBuf::from("/snap/sv1")`) but the allow suppresses the warning for the non-test build. This is fine for now but could be cleaned up by moving the `PathBuf` import into the test module.

### 5.6 Code Quality

**[Commendation] Test coverage for unsent protection is thorough and well-structured.**

Three tests cover the three branches:
- `unsent_snapshots_protected_from_retention` — has pin, newer snapshots protected
- `all_snapshots_protected_when_no_pin` — no pin, everything protected
- `send_disabled_no_unsent_protection` — send disabled, normal retention

Test names describe the scenario being verified. Assertions check both the positive case (what should happen) and the negative case (what should not happen). The `send_disabled_no_unsent_protection` test uses a separate config with `send_enabled = false` to isolate the behavior.

---

## 6. The Simplicity Question

**What was added:**
- ~30 lines of unsent protection logic in `plan_local_retention` — earns its keep (prevents data loss)
- ~30 lines of path validation helpers — earns its keep (prevents path traversal in Phase 2)
- `pin_on_success` field on Send variants — earns its keep (eliminates implicit ordering dependency)
- `subvolume_name` field on all variants — earns its keep (eliminates fragile path heuristic)
- Future-date warning: 8 lines — earns its keep (operator observability)

**What was removed:**
- `PinParent` variant — good, it was a source of implicit coupling
- `to_string_lossy()` roundtrips in `expand_paths()` — good, lossy conversion is gone
- `op_belongs_to` with path inspection — good, replaced by trivial field match
- `pins` field in `PlanSummary` — good, pins are now implicit in sends

**Net assessment:** The codebase got slightly larger but significantly more robust. Every addition is load-bearing. Nothing was added speculatively.

---

## 7. Priority Action Items

The hardening successfully addressed all 7 items from the prior review. Remaining items for Phase 2 readiness:

1. **Retention monthly window still uses `Duration::days(monthly * 30)`** (noted in prior review, not in scope for this hardening). Fix before Phase 2 if retention will be active.

2. **`space_governed_retention` still proposes deleting down to 1 snapshot under pressure** (noted in prior review, not in scope). The executor will need to re-check space between deletions.

3. **Consider validating names don't start with `.`** to prevent hidden-directory edge case (minor, see 5.2).

4. **Move `#[allow(unused_imports)]` on PathBuf** to the test module for cleanliness (minor).

---

## 8. Open Questions

1. **How will the Phase 2 executor use `pin_on_success`?** The plan is clear — write the pin file only on successful send. But should a failed pin-file write fail the entire send operation? Or should the send be considered successful and the pin failure logged as a warning? The pin file is important for incremental chain continuity but not for the integrity of the snapshot that was just sent.

2. **Should unsent protection have an override?** In extreme disk pressure scenarios, the operator might want to force-delete unsent snapshots to free space. Currently, unsent protection prevents this. A `--force-retention` flag or a config option could provide an escape hatch. This is not urgent but worth considering for Phase 4 polish.

---

*Metadata: Review covers the diff between commit `afec570` (Phase 1 original) and the current working tree (Phase 1 hardening). All modified files read in full. 66 tests passing, clippy clean. No areas excluded.*
