# VFM-A Implementation Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-29
**Scope:** VFM-A (Session A) — `OperationalHealth` enum and two-axis CLI rendering
**Reviewer:** arch-adversary
**Commit:** unstaged (working tree changes)
**Files reviewed:** `awareness.rs`, `output.rs`, `voice.rs`, `commands/status.rs`, `sentinel.rs`, `sentinel_runner.rs`, `heartbeat.rs`, `commands/backup.rs`
**Design doc:** `docs/95-ideas/2026-03-28-design-visual-feedback-model.md`

---

## Executive Summary

VFM-A adds a second assessment axis (operational health) to the awareness model and updates the
CLI to display it. The implementation is clean, well-scoped, and respects the pure-function
module pattern. The primary concern is a false-reassurance gap in `compute_health` where local
filesystem pressure is invisible — the function reports `Healthy` for local-only health even when
the NVMe is near its space guard threshold. This is not a data-loss risk but it is exactly the
false reassurance this feature was built to eliminate.

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be deleted.

**Distance from VFM-A changes:** Far. VFM-A is read-only advisory code. `compute_health` and
`render_summary_line` compute and display information. They don't influence the planner, executor,
or retention decisions. Nothing in this changeset can cause a snapshot to be created, deleted,
or sent. The blast radius of a bug here is wrong status text, not data loss.

**Catastrophic failure checklist:**
1. Silent data loss — **not applicable.** No write operations.
2. Path traversal — **not applicable.** No path construction for btrfs.
3. Pinned snapshot deletion — **not applicable.** Advisory only.
4. Space exhaustion — **not applicable.** Reads space, doesn't consume it.
5. Config orphaning — **not applicable.** No config writes.
6. TOCTOU — **not applicable.** No privileged actions.

This is the safest kind of feature to ship: it observes but doesn't act.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Health computation covers the drive-swap scenario correctly. One gap: local-only health blind spot. |
| 2 | Security | 5 | Pure advisory code. No privilege escalation surface. |
| 3 | Architectural Excellence | 4 | Follows the pure-function module pattern exactly. Clean separation awareness→output→voice. |
| 4 | Systems Design | 3 | Space estimation uses `calibrated_size` fallback chain — but calibration data is rarely available. The 7-day "away" threshold is hardcoded where it should be configurable (or at least a named constant). |
| 5 | Rust Idioms | 4 | Good use of `PartialOrd` for min-aggregation. `strip_ansi_len` is hand-rolled where a crate could help, but acceptable for a single use. |
| 6 | Code Quality | 4 | Readable, well-structured. The stringly-typed boundary is pre-existing tech debt, not introduced here. |

## Design Tensions

### 1. "Healthy" for send_enabled=false — simplicity vs. completeness

**Trade-off:** `compute_health` returns `Healthy` immediately when `send_enabled=false`. This
trades completeness (local space pressure is real) for simplicity (the health model is about
external sends).

**Assessment:** Defensible for Session A. The design doc scopes operational health to "can the
next backup succeed efficiently?" — and for local-only subvolumes, the planner's space guard
handles this. But the user looking at `urd status` sees `OK healthy` for a subvolume whose NVMe
is 95% full. The known issue list already calls out NVMe accumulation above 10GB threshold.
This tension will surface in production. **Right call for now, but document the gap.**

### 2. Stringly-typed output boundary — serialization cleanliness vs. fragility

**Trade-off:** Enums are converted to strings at the output.rs boundary. Voice.rs matches on
string literals. This keeps the serialization boundary clean (JSON output is just strings) but
creates a fragile contract between Display impls and match arms.

**Assessment:** This is pre-existing tech debt. VFM-A follows the established pattern correctly.
Fixing it (keeping enums through to voice.rs) would be a broader refactor touching notify.rs,
sentinel, and heartbeat. Not VFM-A's problem to solve. **No action needed in this PR.**

### 3. Chain health on unmounted drives — what you can't see

**Trade-off:** Chain health is only computed for mounted drives (because reading external snapshots
requires the drive to be present). This means an unmounted drive with broken chains gets no
`Degraded` signal until you mount it.

**Assessment:** This is a physical constraint, not a design flaw. You can't read a drive that
isn't there. The `last_send_age > 7 days` degradation partially compensates — if a drive is
gone long enough, health degrades anyway. The sentinel (Session B) will add mount-time chain
health checks. **Correct trade-off.**

### 4. Showing all drives as columns — information density vs. noise

**Trade-off:** The table now shows all configured drives as columns (not just mounted ones),
with "away" for unmounted drives that have send history. This gives the user a complete picture
but widens the table.

**Assessment:** Good call. The previous behavior (hiding unmounted columns) erased information.
The "away" indicator is exactly what's needed — it answers "has this drive ever been used?" at a
glance. **Right decision.**

## Findings

### Finding 1: Local filesystem health blind spot (Moderate)

**What:** `compute_health` returns `Healthy` immediately for `send_enabled=false` subvolumes
and doesn't check local space pressure for any subvolume. A subvolume can be `OK healthy` while
its snapshot root filesystem is within 5% of `min_free_bytes`.

**Consequence:** The user sees "All data safe" with a green summary while the next scheduled
snapshot will be blocked by the space guard. This is the same class of false reassurance the
VFM design was created to fix — just on the local axis instead of external.

**Suggested fix:** Add a local space pressure check before the `send_enabled` early return:

```rust
// Check local space pressure regardless of send_enabled
if let Some(min_free) = config.root_min_free_bytes(subvol_name) {
    if min_free > 0 {
        if let Ok(free) = fs.filesystem_free_bytes(local_snapshot_dir) {
            let tight = min_free + min_free / 5;
            if free < tight {
                reasons.push("local space tight".to_string());
                worst = worst.min(OperationalHealth::Degraded);
            }
        }
    }
}
```

This requires threading `snapshot_root` and `config` into `compute_health`. Alternatively, compute
it in the `assess` loop before calling `compute_health` and pass a pre-computed local health signal.

**Distance from catastrophic failure:** Far — advisory only. But this is the primary false
reassurance the user asked VFM-A to fix, now on the local axis.

### Finding 2: Space estimation fallback rarely has data (Moderate)

**What:** The "blocked: insufficient space" check uses a fallback chain: `calibrated_size` →
`last_send_size(incremental)` → `last_send_size(full)`. In practice, `calibrated_size` requires
running `urd calibrate` (rarely done), and `last_send_size` requires a prior successful send to
that drive. For a fresh drive or after a chain break, all three return `None`, and the check
silently skips (fail-open).

**Consequence:** The space-blocked check effectively does nothing until the user has at least one
successful send to each drive. First sends are the most likely to fail from space exhaustion
(because they're full sends, the largest operations). The check is most absent when it's most
needed.

**Suggested fix:** When all size estimates are `None` and a chain is broken (implying a full
send is needed), use a conservative heuristic — e.g., check if `free - min_free` is at least
some configured minimum or a percentage of the drive. Or: explicitly flag the uncertainty in
`health_reasons` ("space unknown — no send history for {drive}") at `Degraded` rather than
silently skipping. This respects ADR-107 (fail open for backups) while surfacing the uncertainty.

### Finding 3: Hardcoded 7-day threshold (Minor)

**What:** The "drive away" degradation threshold is hardcoded as `age.num_days() > 7` at
awareness.rs:598. The 20% space tight margin is also hardcoded (line 585: `min_free / 5`).

**Consequence:** These are operational tuning parameters that different users will want different
values for. A user with an offsite drive they rotate monthly doesn't want degradation warnings
after 7 days. A user with tight NVMe space might want the space margin at 30%.

**Suggested fix:** Define named constants at the top of `compute_health` (like the existing
`LOCAL_AT_RISK_MULTIPLIER` pattern). Not config fields — constants are fine for now, but they
should be grep-able and documented:

```rust
const DRIVE_AWAY_DEGRADED_DAYS: i64 = 7;
const SPACE_TIGHT_MARGIN_PERCENT: u64 = 20;
```

### Finding 4: Summary line conflates safety and health "attention" (Minor)

**What:** The summary line can read: `"1 of 4 safe. htpc-root needs attention. 2 need attention
— chain broken on WD-18TB."` — two different "needs attention" clauses with different meanings.
The first is safety (data freshness), the second is health (operational readiness).

**Consequence:** The summary line was designed to answer two questions in order: (1) is my data
safe? (2) is anything off? But when both axes have issues, the sentence structure blurs them.

**Suggested fix:** Differentiate the clauses: `"1 of 4 safe. 2 degraded — chain broken on
WD-18TB."` — drop the word "attention" from the health part and use the health vocabulary
directly. The safety part already communicates urgency through the count.

### Finding 5: Commendation — compute_health is a pure function with correct fail-open semantics

**What:** `compute_health` follows the project's pure-function module pattern perfectly: all
inputs arrive through parameters, no I/O is performed directly, and it returns a value. The
`FileSystemState` trait is the I/O boundary, consistent with every other pure module.

More importantly, the space check fails open — when `filesystem_free_bytes` returns an error,
it falls back to `u64::MAX` (unlimited). When no size estimate exists, the drive is assumed
unblocked. This honors ADR-107 (backups fail open) in the advisory layer. The function never
prevents a backup from happening; it only advises the user.

### Finding 6: Commendation — scope discipline

**What:** VFM-A implements exactly Session A from the design doc and nothing more. The sentinel
state file is untouched (schema_version stays at 1). No notification events were added. No
`visual_state` block. All downstream construction sites (sentinel, heartbeat, backup) add the
health fields only in `#[cfg(test)]` blocks. Production code in these modules doesn't access
the new fields.

This is the kind of scope discipline that prevents feature creep from breaking things. The
sentinel (Session B) can be built independently because Session A didn't leak into it.

## Also Noted

- The `strip_ansi_len` function is a minimal hand-rolled ANSI parser. Fine for now; if ANSI
  handling gets more complex, consider the `strip-ansi-escapes` crate.
- Three duration formatters exist in the codebase (`humanize_duration`, `format_duration_secs`,
  `format_duration_short`). They serve different purposes. Low priority to consolidate.
- `color_and_pad` is called with `cell.len()` as `visible_len` for safety/health columns.
  Since these cells don't contain ANSI codes at that point, `cell.len()` is correct. But if
  pre-colored cells are ever passed to these columns, the padding will break.

## The Simplicity Question

**What's earning its keep:**
- `OperationalHealth` enum — necessary. The whole point of VFM-A.
- `compute_health` — necessary. Central logic, pure function, well-tested.
- `safety_label()` — necessary. Vocabulary translation for the two-axis model.
- `render_summary_line()` — necessary. Answers "is my data safe?" in one glance.
- `humanize_duration()` — necessary. Temporal context is the single most valuable UX addition.
- `strip_ansi_len()` — necessary. Pre-colored cells need ANSI-aware width calculation.

**What could be simpler:**
- The `format_status_table` / `format_table` split could be eliminated by always passing
  `Option<usize>` for colored columns. The generic `format_table` wrapper is one line, but it's
  an extra indirection that exists only because the backup summary table doesn't want coloring.
  Consider making the backup summary use `format_status_table` directly with `None, None`.

**Nothing should be deleted.** The implementation is lean. ~120 lines of new production code
in `compute_health`, ~100 in voice.rs rendering, ~20 in output.rs types. The rest is tests.

## For the Dev Team

Priority-ordered action items:

1. **Add local space pressure to health computation** (Finding 1)
   - File: `src/awareness.rs`, `compute_health()` function
   - What: Before the `send_enabled` early return, check local snapshot root space against
     `min_free_bytes`. Thread `snapshot_root` path through or pre-compute in the `assess` loop.
   - Why: The feature was built to eliminate false reassurance. A blind spot on local space
     is the same class of problem on a different axis.

2. **Surface space estimation uncertainty** (Finding 2)
   - File: `src/awareness.rs`, `compute_health()` space-blocked check
   - What: When all size estimates are `None` and chain is broken (full send pending), add a
     `Degraded` reason: "space estimate unknown for {drive} — full send size unpredictable".
   - Why: Silent skip on first sends (the riskiest operations) undermines the check.

3. **Extract hardcoded thresholds to named constants** (Finding 3)
   - File: `src/awareness.rs`, top of file near existing threshold constants
   - What: `DRIVE_AWAY_DEGRADED_DAYS = 7`, `SPACE_TIGHT_MARGIN_PERCENT = 20`
   - Why: Grep-able, documented, consistent with existing `LOCAL_AT_RISK_MULTIPLIER` pattern.

4. **Differentiate summary line clauses** (Finding 4)
   - File: `src/voice.rs`, `render_summary_line()`
   - What: Change health clause from "N need attention" to "N degraded" / "N blocked" to
     avoid echoing the safety clause's "needs attention" wording.
   - Why: When both axes have issues, the current wording is confusing.

## Open Questions

1. Should `OperationalHealth::Blocked` suppress the green "All data safe." in the summary line?
   Currently, `OK blocked` shows "All data safe." followed by "1 needs attention — no backup
   drives connected." The green text could be misleading when backups literally can't happen.

2. The design doc proposes temporal context on external drive columns too (`"7 (18h)"`), and
   the implementation delivers this. But for unmounted drives showing "away", should the age of
   the last send be shown? E.g., `"away (13d)"` would be more informative than just `"away"`.

3. How should this interact with the heartbeat? The heartbeat currently doesn't carry health
   fields. When the monitoring stack reads `promise_status: "PROTECTED"`, it can't distinguish
   "protected and healthy" from "protected but degraded." Is this a gap that needs closing
   before VFM-B, or is the heartbeat solely about data safety?
