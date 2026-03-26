# Design: Next 2–3 Implementation Sessions

> **TL;DR:** Three sessions: (1) Pre-flight checks + interval tuning, (2) Voice migration
> for remaining commands, (3) Protection Promise ADR. Sessions are largely independent;
> verify voice migration (Session 2) integrates preflight results if Session 1 is complete,
> otherwise defers that integration. Sequence optimizes for operational safety first, then
> presentation consistency, then unlocking Phase 6.

**Date:** 2026-03-26
**Status:** reviewed (all findings addressed)
**Context:** All Priority 2a/2a+/2b/2d and Phase 5 foundations complete. Urd is sole backup
system since 2026-03-25, monitoring until 2026-04-01.

## Current Position

What's done:
- Safety hardening: UUID fingerprinting, local space guard, backup summary (2a, 2a+, 2b, 2d)
- Architectural foundations: awareness model, heartbeat, presentation layer, `urd get` (3a–3d)
- Operational cutover in progress (2 unattended nights, 5 total successful runs)

What remains before Phase 6 (protection promises):
- **2c:** Pre-flight checks (safety hardening, last open item)
- **2e:** Structured error messages (medium effort, can defer)
- **3c remaining:** Voice migration for `plan`, `history`, `verify`, `calibrate`
- **Operational:** Config intervals misaligned with daily timer
- **Gate:** Protection Promise ADR (blocks all of Phase 6)

## Session 1: Pre-Flight Checks + Interval Tuning

**Goal:** Close Priority 2 safety hardening. Fix the interval mismatch that causes false
UNPROTECTED status.

### 1a. Config Interval Tuning (operational, no code)

The config has send intervals of 1h–4h from the travel period, but Urd runs once daily via
systemd timer. The awareness model multiplies intervals by thresholds (local: 2×/5×,
external: 1.5×/3×), so a 4h send interval means UNPROTECTED after 12h — the system reports
broken promises 18h/day despite working correctly.

**Action:**

1. **Document the mismatch first.** Before changing anything, capture the current `urd status`
   output and awareness model readings in a journal entry. This preserves the operational
   evidence of the timer/interval mismatch for the Promise ADR (Session 3). The ADR needs
   real data showing how sub-daily intervals interact with a daily timer — once tuned, this
   evidence is gone.

2. **Update `urd.toml` intervals** to reflect daily reality:
   - `snapshot_interval`: 24h (or close to timer frequency)
   - `send_interval`: 24h per drive

This is a config change, not code. Do it first so that `urd status` gives honest readings
during the rest of the session.

### 1b. Pre-Flight Checks Module (`preflight.rs`)

**Problem:** `init.rs` performs 10+ validation checks that are useful before any backup run,
but they're locked inside the `init` command. The `backup` command doesn't call them.
Meanwhile, the htpc-root chain break showed that retention/send-interval incompatibility
isn't caught anywhere.

**Proposed design:**

#### New module: `src/preflight.rs`

A pure function (per ADR-108) that takes config and returns a list of check results.
Config-only — no I/O, no `FileSystemState`. Path-existence and drive-capacity checks
stay in `init.rs` where I/O is expected.

```rust
/// A single pre-flight check result.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightCheck {
    pub name: &'static str,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Severity {
    Ok,
    Warning,
    Error,
}

/// Run all pre-flight checks. Pure function — config only, no I/O.
pub fn preflight_checks(config: &Config) -> Vec<PreflightCheck>
```

#### Checks to extract/add:

**From `init.rs` (extract — these are currently I/O-dependent, so only the *logic* moves):**

The init command's checks involve I/O (testing sudo, writing test files, reading pin files).
These can't become pure functions. Instead, the approach is:

1. **Keep `init.rs` as-is** for its I/O-based environment validation
2. **Build `preflight.rs` as a new layer** for config-level consistency checks that don't
   need I/O — the kind of checks that catch misconfigurations before they cause operational
   problems

**Checks in `preflight.rs` (pure, config-only — no I/O):**

| Check | Severity | Logic |
|-------|----------|-------|
| Retention/send interval compatibility | Warning | For each subvolume where `send_enabled`: guaranteed retention floor < `send_interval`. See detailed model below. |
| Snapshot interval vs. timer frequency advisory | Warning | If `snapshot_interval` < reasonable minimum (e.g., 1h) and no Sentinel is running, warn that the timer can't honor the interval. (Preparatory for Promise ADR — timer frequency as input.) |
| Send enabled but no drives configured | Warning | Subvolume has `send_enabled = true` but no drives exist in config. |

**Checks that stay in `init.rs` (I/O-dependent):**

| Check | Reason |
|-------|--------|
| Subvolume source exists | Needs filesystem access |
| Snapshot root exists | Needs filesystem access |
| Drive too small for subvolume | Needs `calibrated_size()` from state DB and drive capacity from filesystem |

These are already implemented in `init.rs` and work well there. The preflight module's
value is config consistency checks — things you can detect from config alone without
touching disk.

**The key new check — retention/send interval compatibility:**

This is the htpc-root chain break pattern. The chain breaks when:
- Retention policy deletes snapshots faster than the send interval creates new pins
- Specifically: if `daily = 3` keeps only 3 days of daily snapshots, but `send_interval = 1w`,
  the pinned parent (which might be 4+ days old) gets deleted before the next send

The guaranteed survival floor for a snapshot is the sum of the hourly and daily windows.
Weekly/monthly windows provide only *probabilistic* survival — a pinned snapshot might
be the representative for its week, but that depends on which other snapshots exist at
runtime. Per ADR-107 (fail closed on deletions): when in doubt about whether a snapshot
survives, assume it won't.

Detection logic:
```
For each subvolume where send_enabled:
    // Guaranteed survival: hourly window + daily window
    // A snapshot survives `hourly` hours, then `daily` more days
    // in the daily window. After that, it enters weekly selection
    // where survival is not guaranteed.
    guaranteed_survival_hours = hourly + (daily * 24)
    send_interval_hours = send_interval.as_secs() / 3600

    if send_interval_hours > guaranteed_survival_hours:
        warn("retention guarantees snapshot survival for {guaranteed},
              but send interval is {send_interval} — pinned parent
              may be deleted before next send, forcing a full send")
```

**Test cases for the retention/send model:**

| Config | Guaranteed survival | Send interval | Result |
|--------|-------------------|---------------|--------|
| `hourly=24, daily=3, send=1w` (htpc-root) | 24h + 72h = 96h | 168h | WARN (168 > 96) |
| `hourly=24, daily=7, send=1w` | 24h + 168h = 192h | 168h | OK (168 < 192) |
| `hourly=168, daily=0, send=5d` | 168h + 0h = 168h | 120h | OK (120 < 168) |
| `hourly=0, daily=3, send=4d` | 0h + 72h = 72h | 96h | WARN (96 > 72) |
| `hourly=0, daily=10, send=12d` | 0h + 240h = 240h | 288h | WARN (288 > 240) |

#### Integration points:

1. **`backup` command:** Call `preflight_checks(&config)` before planning. Log warnings
   via `log::warn!`. Do NOT abort on warnings — backups fail open (ADR-107). Preflight
   warnings merge into the existing `BackupSummary.warnings: Vec<String>` (no new section
   needed — they're warnings like any other).
2. **`init` command:** Call `preflight_checks(&config)` as an additional section after
   existing I/O checks. Render results.
3. **`verify` command:** Call `preflight_checks(&config)` and include in `VerifyOutput`.

#### Module boundaries:

| Module | Responsibility |
|--------|---------------|
| `preflight.rs` | Pure config consistency checks (new) |
| `commands/init.rs` | I/O environment validation (unchanged) |
| `commands/verify.rs` | Chain integrity + preflight results (calls preflight) |
| `plan.rs` | Runtime pre-conditions during planning (unchanged) |

**Effort:** ~1 session. Comparable to UUID fingerprinting (1 new module, 3 checks, ~10-12
tests). The retention/send compatibility check is the novel logic; the rest is straightforward.

**Tests:**
- Retention/send compatibility: htpc-root case (`hourly=24, daily=3, send=1w`) → warning
- Retention/send compatibility: safe case (`hourly=24, daily=7, send=1w`) → no warning
- Retention/send: large hourly compensates for zero daily (`hourly=168, daily=0, send=5d`) → no warning
- Retention/send: zero hourly, small daily (`hourly=0, daily=3, send=4d`) → warning
- Send enabled with no drives → warning
- Timer/interval advisory: sub-hourly interval → warning
- All-clear config → empty results
- Disabled subvolumes skipped
- Send-disabled subvolumes skipped for retention/send check
- Edge cases: zero retention values (hourly=0, daily=0) → guaranteed survival is 0 → always warns if send enabled

### Session 1 deliverables:
- [ ] Journal entry documenting current awareness mismatch (before tuning)
- [ ] Config intervals tuned to daily timer
- [ ] `src/preflight.rs` with 3 pure config-only checks
- [ ] Integration into `backup`, `init`, and `verify` commands
- [ ] 10-12 tests
- [ ] Adversary review

---

## Session 2: Voice Migration for Remaining Commands

**Goal:** Complete Priority 3c — all commands produce structured output rendered by the
voice layer. Daemon mode (JSON) works for every command.

### Commands to migrate (in order):

#### 2a. `plan_cmd.rs` → `PlanView` (Low, ~1h)

**Current:** 11 `println!` with inline colors and formatting. `run_with_plan()` is already
shared between `urd plan` and `urd backup --dry-run` — the structured output must serve both.

**Design decision (from review Finding 3):** The existing `BackupPlan` and `PlannedOperation`
in `types.rs` contain internal details (`pin_on_success` paths, full filesystem paths) that
shouldn't be exposed to JSON consumers. Rather than adding `#[derive(Serialize)]` to core
types and leaking internals, build a thin **view adapter** that projects the domain model
into a presentation-safe shape.

**Structured type:**
```rust
/// Presentation view of a BackupPlan. Thin adapter — not a parallel domain model.
/// Built from BackupPlan by the plan command, consumed by voice::render_plan().
#[derive(Debug, Serialize)]
pub struct PlanView {
    pub timestamp: String,
    pub subvolumes: Vec<PlannedSubvolumeView>,
    pub summary: PlanSummaryView,
}

#[derive(Debug, Serialize)]
pub struct PlannedSubvolumeView {
    pub name: String,
    pub priority: u8,
    pub operations: Vec<PlannedOperationView>,
    pub skip_reasons: Vec<String>,
}

/// Flattened, presentation-safe view of a PlannedOperation.
/// Internal paths (pin files, full source paths) are stripped.
#[derive(Debug, Serialize)]
pub struct PlannedOperationView {
    pub op_type: String,       // "create", "send-incremental", "send-full", "delete"
    pub snapshot: String,      // snapshot name only, not full path
    pub drive: Option<String>, // drive label for sends
    pub detail: String,        // human-readable detail (parent name, delete reason, etc.)
}

#[derive(Debug, Serialize)]
pub struct PlanSummaryView {
    pub snapshots: usize,
    pub sends: usize,
    pub deletions: usize,
    pub skipped: usize,
}
```

**Conversion:** `impl From<&BackupPlan> for PlanView` (with config for subvolume grouping).
The adapter extracts file names from paths and drops `pin_on_success`. `PlanSummaryView`
mirrors the existing `PlanSummary` from `types.rs` but with `Serialize`.

**Voice rendering:** Reuse the colored tag pattern from current code (`[CREATE]`, `[SEND]`,
etc.) but through `voice::render_plan()`.

**Why first:** Simplest migration, establishes the adapter pattern for others.

#### 2b. `calibrate.rs` → `CalibrateOutput` (Low-Med, ~1.5h)

**Current:** 16 `println!` + 3 `eprintln!` with inline progress.

**Challenge:** Interactive progress (`print!` + `flush()` for "Measuring... done") happens
during execution, not after. The structured output captures final results only; progress
display stays in the command handler (it's inherently interactive).

**Structured type:**
```rust
#[derive(Debug, Serialize)]
pub struct CalibrateOutput {
    pub results: Vec<CalibrateResult>,
    pub summary: CalibrateSummary,
}

#[derive(Debug, Serialize)]
pub struct CalibrateResult {
    pub subvolume: String,
    pub snapshot: String,
    pub size_bytes: Option<u64>,
    pub error: Option<String>,
}
```

**Design decision:** Progress display during measurement remains as direct `print!` in
interactive mode (same as backup progress — it's inherently streaming). The final summary
goes through voice.

#### 2c. `history.rs` → `HistoryOutput` (Medium, ~2h)

**Current:** 14 `println!` with three mode-dependent table formats.

**Structured type:**
```rust
#[derive(Debug, Serialize)]
pub struct HistoryOutput {
    pub mode: HistoryMode,
    pub entries: Vec<HistoryEntry>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum HistoryMode {
    RecentRuns,
    SubvolumeHistory { subvolume: String },
    Failures,
}

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    pub run_id: i64,
    pub timestamp: String,
    pub result: String,
    pub subvolume: Option<String>,
    pub duration_secs: Option<f64>,
    pub bytes_transferred: Option<u64>,
    pub error: Option<String>,
}
```

**Voice rendering:** Table formatting using the existing column-width pattern. Move
`truncate_str()` and `color_result()` helpers to `voice.rs`.

#### 2d. `verify.rs` → `VerifyOutput` (Medium-High, ~3h)

**Current:** 29 `println!` with complex nested structure (subvolume → drive → checks).

**Most complex migration.** The verify command has hierarchical output with per-check
severity levels (OK/WARN/FAIL).

**Structured type:**
```rust
#[derive(Debug, Serialize)]
pub struct VerifyOutput {
    pub subvolumes: Vec<VerifySubvolume>,
    pub preflight: Vec<PreflightCheck>,  // from session 1
    pub summary: VerifySummary,
}

#[derive(Debug, Serialize)]
pub struct VerifySubvolume {
    pub name: String,
    pub drives: Vec<VerifyDrive>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyDrive {
    pub label: String,
    pub checks: Vec<VerifyCheck>,
}

#[derive(Debug, Serialize)]
pub struct VerifyCheck {
    pub name: String,
    pub status: String,       // "OK", "WARN", "FAIL"
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifySummary {
    pub ok_count: usize,
    pub warn_count: usize,
    pub fail_count: usize,
}

impl VerifyOutput {
    /// Derive exit code from verification results.
    /// Single source of truth — command handler calls this, never re-derives.
    pub fn exit_code(&self) -> i32 {
        if self.summary.fail_count > 0 { 1 } else { 0 }
    }
}
```

**Why last:** Benefits from patterns established in the earlier migrations. Also integrates
preflight checks from Session 1 (if complete; otherwise defers preflight integration).

### Session 2 deliverables:
- [ ] 4 new output types in `output.rs` (`PlanView`, `CalibrateOutput`, `HistoryOutput`, `VerifyOutput`)
- [ ] 4 new render functions in `voice.rs`
- [ ] All commands produce JSON in daemon mode
- [ ] ~20-25 voice tests (5-7 per command)
- [ ] Adversary review

**Effort:** One full session. Comparable to the presentation layer initial build (3c) which
created `StatusOutput` + `GetOutput` + `BackupSummary` with 15 voice tests.

---

## Session 3: Protection Promise ADR

**Goal:** Write the ADR that gates Phase 6. This is design work, not code. The ADR must
resolve the open questions listed in status.md's Phase 5 gate.

### Questions the ADR must answer:

1. **Promise levels and their derivations:**
   - `guarded`: local snapshots only, no external requirement
   - `protected`: local + at least one external drive current
   - `resilient`: local + multiple external drives current
   - `archival`: long-term retention, relaxed freshness
   - `custom`: existing operation-focused config (the default for migration)
   - For each level: exact `snapshot_interval`, `send_interval`, and retention policy

2. **Config conflict resolution:**
   - If `protection_level = "protected"` AND `send_interval = "6h"`, which wins?
   - Proposal: promise-derived values are defaults; explicit overrides win with a warning
   - Alternative: explicit overrides are errors when a promise is set

3. **Migration path:**
   - Existing configs have no `protection_level` → implicit `custom`
   - `custom` means "I manage intervals and retention manually"
   - Zero breaking changes — all existing configs continue to work identically

4. **Timer frequency as input:**
   - Operational data showed awareness reporting UNPROTECTED 18h/day
   - Promise achievability must account for actual run frequency
   - Option A: `timer_frequency` in config, planner uses it for achievability checks
   - Option B: Derive from heartbeat history (last N run timestamps)
   - Option C: Require Sentinel for sub-daily promises

5. **Drive topology constraints:**
   - subvol3-opptak (~3.4TB) cannot have external promises on 2TB-backup (~1.1TB available)
   - Per-subvolume `drives = [...]` mapping (Priority 4c) is prerequisite
   - Promise validation: "this promise is unachievable given your drive topology"

6. **Awareness threshold mode:**
   - Current: fixed multipliers (2×/5× local, 1.5×/3× external)
   - Should thresholds adapt based on whether Sentinel is running?
   - Recommendation: No — keep thresholds simple. Timer frequency feeds into *achievability
     validation*, not threshold computation.

### ADR structure:

Following the existing ADR format (ADR-100 through ADR-109):
- **Context:** Why promises, what operational data shows
- **Decision:** Promise levels, derivation rules, config schema, migration
- **Consequences:** What this enables (Phase 6), what it constrains
- **Alternatives considered:** Each open question's rejected alternatives

### Session 3 deliverables:
- [ ] ADR-110: Protection Promise Design
- [ ] Updated status.md Phase 5 gate checklist (all items resolved)
- [ ] Adversary review of ADR

---

## What's Deliberately Deferred

| Item | Why defer |
|------|-----------|
| **2e: Structured error messages** | Medium effort, lower operational impact than 2c. Build when a specific btrfs error pattern causes user confusion. The translation layer is well-scoped but not urgent. |
| **Journal persistence gap** | Operational concern, not code. `journalctl --vacuum-time` settings or a local log file complement. Address when it causes a real problem. |
| **NVMe snapshot accumulation** | Space guard prevents catastrophe. Gradual accumulation above threshold is a retention tuning issue, not a code issue. Revisit during Promise ADR when retention derivations are defined. |
| **`heartbeat::read()` Result upgrade** | Blocked on Sentinel design (the consumer). Build when 5a starts. |
| **`init` command voice migration** | `init` has interactive deletion prompts that don't fit the structured output pattern cleanly. Defer until the interaction model is clearer. |

## Sequencing Rationale

**Session 1 first** because:
- Pre-flight checks close the last safety hardening item (2c)
- Interval tuning fixes false UNPROTECTED status immediately
- Both are small, high-value changes during the monitoring period

**Session 2 second** because:
- Voice migration is independent of Session 1 (but benefits from preflight integration in verify)
- Gives presentation consistency before the Promise ADR (Session 3) introduces new promise-level
  status displays
- Daemon JSON output for all commands is prerequisite for Sentinel consumers

**Session 3 third** because:
- The ADR is a design exercise that benefits from operational experience
- More monitoring data (target: past 2026-04-01 cutover completion) informs timer frequency questions
- Sessions 1-2 ensure all existing features are polished before adding the next layer

## Review Findings Addressed

This design was reviewed by arch-adversary on 2026-03-26. All findings addressed:

| Finding | Severity | Resolution |
|---------|----------|------------|
| 1. Retention/send compatibility model error | Significant | Replaced `daily_count > send_interval_days` with guaranteed survival floor: `hourly + (daily * 24)`. Added 5 test cases covering the model. |
| 2. Path-existence checks aren't pure | Moderate | Dropped from preflight. Simplified signature to `preflight_checks(config: &Config)`. I/O checks stay in `init.rs`. |
| 3. PlanOutput duplicates planner types | Moderate | Renamed to `PlanView` — thin adapter that projects `BackupPlan` into presentation-safe shape. Internal paths stripped, `pin_on_success` dropped. |
| 4. VerifyOutput omits exit code semantics | Moderate | Added `exit_code(&self) -> i32` method. Single source of truth for severity → exit code. |
| 5. Interval tuning may mask mismatch evidence | Minor | Added journal documentation step before tuning. Preserves evidence for Promise ADR. |
| 6. Session independence claim overstated | Minor | Softened TL;DR to acknowledge verify/preflight soft dependency. |

**Open questions from review (resolved):**

1. **Should `preflight_checks` accept `FileSystemState`?** No. Pure config-only. Drive-too-small
   check stays in `init.rs`/`verify` where I/O is expected.
2. **Where do preflight warnings surface in backup summary?** Merged into existing
   `BackupSummary.warnings: Vec<String>`. No new section needed.
3. **Should `PlanView` support `--dry-run`?** Yes. `run_with_plan()` is already shared between
   `urd plan` and `urd backup --dry-run`. Both will build a `PlanView` from the `BackupPlan`.

[Review report](../99-reports/2026-03-26-next-sessions-design-review.md)
