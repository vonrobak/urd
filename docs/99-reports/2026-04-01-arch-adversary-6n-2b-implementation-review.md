# Arch-Adversary Review: 6-N Retention Preview + Phase 2b Doctor

**Date:** 2026-04-01
**Reviewer:** arch-adversary
**Scope:** Implementation review — all new/modified files for `urd retention-preview` and `urd doctor` commands
**Commit:** uncommitted changes on master (base: `4e31d55`)
**Type:** Implementation review (all 6 dimensions)

---

## 1. Executive Summary

Two well-scoped, read-only diagnostic commands that follow established patterns and stay far
from data-modifying code paths. The retention preview has one correctness gap where `monthly = 0`
(unlimited) is silently omitted from the preview, misleading users about their actual coverage.
The doctor command is clean composition of existing modules with no new I/O paths. Both earned
their complexity — these are features users will actually reach for.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss via deleted snapshots.**

Neither feature is within striking distance. Both commands are entirely read-only — no calls
to the executor, retention deletion logic, btrfs subprocess, or any mutation path. The retention
preview replicates the window math from `graduated_retention()` but never calls it, and never
touches the retention result (keep/delete lists). Doctor calls `awareness::assess()` which reads
the filesystem but never modifies it.

**Proximity to catastrophe: VERY LOW.** No finding in this review is within two bugs of silent
data loss. The closest approach is the verify extraction (`collect_verify_output`), which touches
the same module that manages pin file reads — but the extraction is mechanical and the function
only reads, never writes.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | `monthly = 0` (unlimited) omitted from preview; otherwise math is solid and well-tested |
| **Security** | 5/5 | Read-only commands, no new privilege escalation, no new I/O paths |
| **Architectural Excellence** | 4/5 | Clean module boundaries, pure functions, good reuse of existing checks |
| **Systems Design** | 4/5 | Daemon mode works, graceful degradation on missing DB, sentinel PID check is correct |
| **Rust Idioms** | 4/5 | `#[must_use]`, proper enum matching, good use of `Option` chains |
| **Code Quality** | 4/5 | Clear structure, good test coverage, /simplify findings already addressed |

**Overall: 17/20 (approve) — one correctness fix required before shipping.**

---

## 4. Design Tensions

### 4.1 Preview math replication vs. sharing with `graduated_retention()`

`compute_recovery_windows()` replicates the cascading window offset logic from
`graduated_retention()` (lines 50-62). The alternative would be to extract the window
boundary computation into a shared function called by both.

**Why replication was probably chosen:** The preview needs days-as-floats for human formatting;
the retention engine needs `NaiveDateTime` cutoffs for snapshot comparison. The inputs and
outputs diverge enough that a shared function would need generics or a callback, adding
complexity to the most safety-critical code in the system.

**Evaluation:** Correct trade-off. The retention engine is load-bearing and battle-tested —
adding abstraction to serve a display feature is the wrong direction. The risk is drift
(the two implementations diverging), but there's a test (`preview_cumulative_math_matches_retention`)
that partially guards against this. The `monthly = 0` (unlimited) divergence found below
demonstrates this drift risk is real, not hypothetical.

### 4.2 Doctor as compositor vs. new diagnostic engine

Doctor composes existing modules (`preflight`, `init`, `awareness`, `sentinel_runner`, `verify`)
rather than building a new diagnostic framework. The sentinel status check is the one new
diagnostic — reading the state file and checking PID liveness.

**Evaluation:** Correct. A diagnostic framework would be speculative complexity. The composition
approach means doctor automatically benefits from improvements to the underlying checks. The
sentinel check is the right exception — it's a genuine gap that no existing command covers.

### 4.3 `DoctorCheckStatus` vs reusing `InitStatus`

Doctor introduces a new `DoctorCheckStatus` enum that is semantically identical to `InitStatus`
(Ok/Warn/Error). The mapping between them is trivial (lines 55-63 of doctor.rs).

**Why a new type:** `InitStatus` is defined in `output.rs` as part of `InitOutput` — reusing it
would mean doctor's output type depends on init's output type, which is a coupling direction
that doesn't match the composition hierarchy.

**Evaluation:** Acceptable. The cost is a 5-line mapping function. The alternative (shared enum)
would be cleaner but risks coupling init and doctor evolution. Not worth changing.

---

## 5. Findings

### S1: `monthly = 0` (unlimited) silently omitted from preview (Significant)

**What:** `compute_recovery_windows()` at line 343 checks `if config.monthly > 0` and skips the
monthly window when the count is zero. But in the actual retention engine (line 53), `monthly = 0`
means **unlimited** — keep all monthly snapshots indefinitely. The example config uses exactly
this: `monthly = 0  # Keep all monthly snapshots (0 = unlimited, space permitting)`.

**Consequence:** A user running `urd retention-preview` with the default config sees three recovery
windows (hourly/daily/weekly) and no monthly window. Their actual retention keeps monthly snapshots
forever. The preview understates their coverage — they think they have 7 months of history when
they actually have unlimited monthly snapshots beyond that. This is the safe direction (understating
rather than overstating), but it's misleading and will confuse users who compare the preview to
their actual snapshot list.

Similarly, `total_snapshot_count()` sums all four fields including a zero monthly, producing a
count that excludes the unlimited monthly snapshots. The disk estimate is therefore also an
underestimate.

**Fix:** When `config.monthly == 0`, add a recovery window with a description like
`"monthly snapshots kept indefinitely"` and handle the count/estimate as "unbounded." The
`format_graduated_policy()` function (line 404) has the same gap — it omits `monthly` entirely
when zero, where it should show `monthly = unlimited`.

**Distance from catastrophe:** Not applicable — this is a display-only issue. But it undermines
the feature's core value proposition (helping users understand their retention).

---

### S2: Preview and retention engine use different month approximations (Significant)

**What:** The preview uses `30.44` days/month (line 347), which is the standard average. The
actual retention engine uses `checked_sub_months(Months::new(n))` (line 59), which performs
**calendar month subtraction** — meaning the monthly window varies by ~3 days depending on
which months are involved (February vs July).

This means the preview's "7 months" might be off by a week compared to what the retention engine
actually does. For the weekly window, the divergence is zero (both use exact `7 * weeks`). For
the daily window, also zero (both use exact days). Only the monthly window diverges.

**Consequence:** A user sees "monthly snapshots back 19 months" in the preview, but the actual
retention cutoff is `weekly_cutoff.checked_sub_months(12)`, which for a December run date would
be January 1 of the previous year — potentially 2-3 days different from what `12 * 30.44 = 365`
computes. This is within the rounding tolerance of `format_cumulative_days()` and won't produce
a different month count in practice.

**Evaluation:** Acceptable divergence. Documenting the approximation (as the design doc's review
Finding 1 already noted) is sufficient. No code change needed — just noting that this is a
known, bounded approximation.

---

### M1: Sentinel not running is a warning, not an error (Moderate)

**What:** When sentinel is not running, doctor increments `warn_count` (line 156). But the
sentinel is supposed to be the continuous monitoring layer — if it's down, the user has no
sub-hourly monitoring, no drive detection, no backup overdue alerts. This could reasonably be
an error on systems where sentinel is deployed.

**Consequence:** Doctor reports "1 warning" when the monitoring daemon is down. A user running
`urd doctor` after a system reboot sees "Warnings(1)" rather than "Issues(1)" for a
non-functioning sentinel.

**Counter-argument:** Not all deployments use sentinel. Guarded-level subvolumes (local only)
don't need it. Making it an error would cause false positives for users who intentionally don't
run sentinel.

**Fix:** Check whether any configured subvolume has sentinel-relevant settings (e.g.,
`run_frequency: Sentinel`, or any offsite drives). If yes, sentinel not running is an error.
If no sentinel-relevant config exists, it's informational (not even a warning). This is a
refinement for later — the current behavior is defensible as a first pass.

---

### M2: `collect_verify_output` creates a new `RealFileSystemState` without state DB (Moderate)

**What:** The extracted `collect_verify_output()` at verify.rs line 24 creates
`RealFileSystemState { state: None }` — no state DB. But doctor.rs at line 76-82 already
opens the state DB for the awareness assessment. When doctor calls `collect_verify_output()`,
the verify function creates a second, DB-less filesystem state.

**Consequence:** Verify checks (pin files, orphan detection) don't use calibrated sizes or
send history, so the missing DB doesn't affect correctness. But it's a wasted opportunity —
if verify ever gains DB-dependent checks, the doctor path would silently miss them. Pre-existing
issue (verify has always been DB-less), but the extraction makes it more visible.

**Fix:** Consider accepting an optional `&StateDb` in `collect_verify_output()` in a future
pass. Not urgent — no current check needs it.

---

### M3: `DoctorVerdict` serialization may surprise API consumers (Moderate)

**What:** `DoctorVerdict` derives `Serialize` with `#[serde(rename_all = "snake_case")]`.
The enum variants serialize as:
- `Healthy` → `"healthy"`
- `Warnings(3)` → `{"warnings": 3}`
- `Issues(2)` → `{"issues": 2}`

This is an externally tagged enum — `Healthy` produces a string, while `Warnings` and `Issues`
produce objects. The heterogeneous JSON types (string vs. object depending on health) make
downstream parsing awkward. Consumers need `if typeof verdict === "string"` logic.

**Consequence:** Any script consuming `urd doctor` JSON output needs to handle two different
shapes for the verdict field. This is a minor API ergonomics issue.

**Fix:** Consider an internally tagged representation, or a struct with `{ status: "healthy"|"warnings"|"issues", count: Option<usize> }`. Not urgent — the API isn't stabilized pre-1.0.

---

### C1: Pure function placement is exemplary (Commendation)

`compute_retention_preview()`, `compute_recovery_windows()`, `compute_transient_comparison()`,
`retention_summary()`, and all helpers in `retention.rs` are genuinely pure — no I/O, no clock,
no global state. The `#[must_use]` annotations are correct. The preview replicates the retention
math rather than abstracting it, which keeps the safety-critical `graduated_retention()` function
untouched. This is the right instinct for a backup tool.

### C2: Doctor composition is clean and extensible (Commendation)

Doctor doesn't invent new diagnostic infrastructure — it calls existing functions
(`preflight_checks`, `collect_infrastructure_checks`, `assess`, `read_sentinel_state_file`,
`collect_verify_output`) and maps their results into a unified output. Adding a new diagnostic
section requires adding ~15 lines to doctor.rs and a new output type. The sentinel status check
is the one new diagnostic, and it's the right one — it fills a genuine observability gap.

### C3: Verify extraction is minimal and correct (Commendation)

The `collect_verify_output()` extraction from `verify::run()` is mechanical — it moves the
data collection into a separate function and makes `run()` a thin wrapper that renders and
exits. No logic changed. This is the correct way to enable reuse without risking the existing
command's behavior.

---

## 6. The Simplicity Question

**What could be removed?** Very little. Both features are lean for what they deliver.

- `EstimateMethod` enum now has a single variant (`Calibrated`). It exists for future
  extensibility. Single-variant enums are usually premature, but here the design doc explicitly
  deferred `UserProvided` until the wizard is built. The enum earns its existence as a reminder
  of that intent. Marginal — could be just a bool, but the enum is fine.

- `DoctorSentinelStatus.uptime` is pre-formatted as a String. This prevents the voice layer
  from choosing its own format. In practice, "3h 12m" is the only reasonable format, so this
  is acceptable.

**What's earning its keep?**

- The `cumulative_days` field on `RecoveryWindow` was added during /simplify to eliminate
  string re-parsing. It's paying for itself — `compact_window()` is now 6 lines of arithmetic
  instead of 12 lines of string parsing.

- The `--compare` flag on `retention-preview` earns its keep immediately: comparing graduated
  vs. transient is the key decision point the feature was designed to support.

- The `--thorough` flag on `doctor` is the right progressive disclosure: fast by default,
  complete when asked. The verify pass scans every pin file on every mounted drive — that's
  real I/O that shouldn't happen on every `urd doctor` invocation.

---

## 7. For the Dev Team

**Priority 1 — Fix before shipping:**

1. **`monthly = 0` unlimited handling** (`src/retention.rs`, `compute_recovery_windows()` and
   `total_snapshot_count()` and `format_graduated_policy()`):
   - When `config.monthly == 0`: add a RecoveryWindow with granularity "monthly", count 0,
     cumulative_days `f64::INFINITY` (or a sentinel), and description
     `"monthly snapshots kept indefinitely"`.
   - In `total_snapshot_count()`: document that the returned count excludes unlimited monthly.
   - In `format_graduated_policy()`: show `"monthly = unlimited"` instead of omitting.
   - In `compact_window()`: handle the `count == 0` / unlimited case → `"\u{221e}"` or `"all"`.
   - Add a test: `preview_monthly_unlimited()` with `monthly = 0`.

**Priority 2 — Address soon:**

2. **`DoctorVerdict` serialization** (`src/output.rs`): Consider whether the heterogeneous JSON
   (string vs. object) is intentional. If scripting consumers are a priority, flatten to a struct.
   If not, document the serialization format in a comment.

**Priority 3 — Track for later:**

3. **Sentinel warning vs. error** (`src/commands/doctor.rs`, line 155-162): The current "always
   warn" behavior is acceptable for v1. Revisit when sentinel becomes more central to the
   protection model.

4. **Month approximation documentation**: Add a comment at `retention.rs` line 347 noting
   the `30.44` approximation vs. calendar months in `graduated_retention()`, and that the
   divergence is bounded to ~3 days.

---

## 8. Open Questions

1. **Should `retention-preview` show the hourly bucket count when suppressed?** Currently,
   if `snapshot_interval >= 1d`, the hourly bucket is completely invisible — the user doesn't
   know it exists in their config. Should the policy description still show `hourly = 24`
   (greyed out or annotated) so the user knows it's configured but not applicable?

2. **Should `urd doctor` check redundancy advisories?** The 6-I advisory system was just
   merged. Doctor currently doesn't surface "no offsite protection" or "single point of failure"
   advisories. These seem like natural additions to the data safety section.

3. **What should `urd doctor` return as exit code?** Currently it always returns `Ok(())`.
   `urd verify` uses `std::process::exit(1)` on failures. Should doctor follow the same pattern
   for Issues? This matters for scripting: `urd doctor && echo "healthy"`.
