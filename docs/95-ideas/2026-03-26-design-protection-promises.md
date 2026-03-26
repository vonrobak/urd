# Design: Protection Promise ADR (ADR-110)

> **TL;DR:** Protection promises are named levels (`guarded`, `protected`, `resilient`,
> `custom`) that map to concrete retention and interval policies. The planner derives
> operations from the promise level; the awareness model evaluates whether the promise
> is being kept. Existing configs continue to work as implicit `custom`. Timer frequency
> and drive topology are explicit inputs to achievability validation.

**Date:** 2026-03-26
**Status:** reviewed (all findings addressed)
**Depends on:** Awareness model (3a, complete), pre-flight checks (2c, complete),
voice migration (Session 2, in progress)

## Problem

Urd currently speaks in operations: snapshot intervals, send intervals, retention
windows. Users must reason backwards from these to answer "is my data safe?" The
awareness model (already built) computes PROTECTED / AT RISK / UNPROTECTED, but the
thresholds are derived from whatever intervals the user configured — there's no semantic
link between the user's *intent* ("keep my recordings safe") and the system's *behavior*
(1h snapshots, 2h sends).

Protection promises bridge this gap. The user declares what they want; Urd derives
what to do. This is the foundation for:
- Promise-anchored status ("your recordings are PROTECTED")
- Config validation ("this promise is unachievable with your drives")
- Sentinel triggers ("a drive appeared — promises need attention")
- Future: conversational setup ("what matters most to you?")

### Operational evidence

The config/timer mismatch incident (2026-03-26) demonstrated the problem: the user
configured 1h–4h intervals during a travel period, but the systemd timer fires daily.
The awareness model reported UNPROTECTED for 18h/day — technically correct (intervals
were violated) but misleading (data was fine). This happened because the config
describes *desired cadence* without reference to *actual run frequency*.

Promise levels must be defined in terms of outcomes ("snapshot no older than X"), not
frequencies ("snapshot every Y"), so they're evaluable regardless of how often Urd runs.

### Gate checklist from status.md

The Phase 5 gate requires the ADR to answer:
- [x] Exact retention/interval derivations for each promise level
- [x] Config conflict resolution: promise + manual intervals
- [x] Migration path for existing configs (implicit `custom`)
- [x] Promise validation: achievability given drive topology
- [x] `custom` designed as first-class
- [x] Timer frequency as input to achievability
- [x] Drive topology constraints (subvolume × drive capacity)
- [x] Awareness threshold mode (adapt to Sentinel vs. timer?)

## Proposed Design

### Promise levels

Four named levels plus `custom`. Each level defines outcome targets, from which the
planner derives operational parameters.

```rust
/// Protection promise level for a subvolume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtectionLevel {
    /// Local snapshots only. No external send requirement.
    /// For: temp data, caches, build artifacts, data you can re-derive.
    Guarded,
    /// Local snapshots + at least one external drive current.
    /// For: documents, photos, personal projects — things you'd hate to lose.
    Protected,
    /// Local snapshots + multiple external drives current.
    /// For: irreplaceable data — recordings, archives, creative work.
    Resilient,
    /// User manages all parameters manually. Default for migration.
    Custom,
}
```

**Why no `archival` level?** The original brainstorm included `archival` (long-term
retention, relaxed freshness). On reflection, archival is an orthogonal concern —
it's about *how long* to keep history, not *how safe* data is right now. A subvolume
can be `protected` (current copies exist) *and* have archival retention (keep monthly
snapshots indefinitely). Conflating them creates confusing semantics: is an archival
subvolume "less protected" because its freshness threshold is relaxed? No — it just
has a longer retention tail.

If archival becomes needed, it should be a `retention_profile` separate from
`protection_level`. Defer until there's a concrete use case.

### Outcome targets per level

Each promise level defines **maximum acceptable age** for local and external copies.
These are the thresholds the awareness model evaluates against.

| Level | Local max age | External max age | Min external drives | Retention floor |
|-------|--------------|------------------|--------------------|-----------------------|
| `guarded` | 48h | — (no external) | 0 | daily=7, weekly=4 |
| `protected` | 24h | 48h | 1 | daily=30, weekly=26, monthly=12 |
| `resilient` | 24h | 48h | 2 | daily=30, weekly=26, monthly=12 |
| `custom` | (from config) | (from config) | (from config) | (from config) |

**Key insight:** The max ages are *outcomes*, not intervals. A daily timer achieving
24h local max age means `snapshot_interval = 24h`. A Sentinel achieving 24h means
it could use `snapshot_interval = 1h` with more frequent runs. The promise level
doesn't change — the derived intervals adapt to the run frequency.

**These outcome targets are primary policy, not awareness-multiplier derivations
(review M3).** The targets define what each promise level *means* to users. The
awareness model's multiplier constants (2×/5× local, 1.5×/3× external) happen to
be consistent with these targets today, but the targets should be defensible on their
own terms. If the awareness multipliers change (module-level constants, not config),
promise semantics must not silently shift — add a consistency check between promise
outcome targets and awareness thresholds in the preflight module.

### Planner derivation

The planner derives operational parameters from the promise level + run frequency:

```rust
/// Derived operational parameters for a promise level.
#[derive(Debug, Clone)]
pub struct DerivedPolicy {
    pub snapshot_interval: Interval,
    pub send_interval: Interval,
    pub send_enabled: bool,
    pub local_retention: ResolvedGraduatedRetention,
    pub external_retention: ResolvedGraduatedRetention,
    pub min_external_drives: usize,
}

/// Derive operational parameters from a promise level.
/// Pure function — promise level + run frequency in, policy out.
pub fn derive_policy(
    level: ProtectionLevel,
    run_frequency: RunFrequency,
) -> DerivedPolicy
```

**Run frequency** is an explicit input, not inferred:

```rust
/// How often Urd runs. Determines interval derivation.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunFrequency {
    /// systemd timer, typically daily
    Timer { interval: Interval },
    /// Sentinel daemon, sub-hourly checks
    Sentinel,
}
```

**Derivation rules:**

For `Timer { interval: 24h }`:
- `guarded`: snapshot_interval=24h, send_enabled=false
- `protected`: snapshot_interval=24h, send_interval=24h (one send per run)
- `resilient`: snapshot_interval=24h, send_interval=24h (sends to all available drives)

For `Sentinel`:
- `guarded`: snapshot_interval=4h, send_enabled=false
- `protected`: snapshot_interval=1h, send_interval=4h
- `resilient`: snapshot_interval=1h, send_interval=2h

The derived intervals are chosen so that the awareness model's thresholds (2× local,
1.5× external) keep the subvolume PROTECTED under normal operation:
- Timer 24h + local threshold 2× = PROTECTED up to 48h = `guarded` max age ✓
- Timer 24h + external threshold 1.5× = PROTECTED up to 36h, AT RISK at 48h
  → fits `protected` max age of 48h ✓

**Retention derivation by level:**

```
guarded:
  local:    daily=7, weekly=4
  external: (none — send disabled)

protected:
  local:    hourly=24, daily=30, weekly=26, monthly=12
  external: daily=30, weekly=26, monthly=0

resilient:
  local:    hourly=24, daily=30, weekly=26, monthly=12
  external: daily=30, weekly=26, monthly=0
```

Retention is the same for `protected` and `resilient` — the distinction is about
*how many copies*, not *how long to keep them*. The name "resilient" implies
redundancy (more drives), not deeper history (review M4). If users expect `resilient`
to also mean longer retention, the levels can diverge later (e.g., `resilient` with
`monthly = 0` for unlimited monthly). Defer until user feedback confirms the need.
Document this explicitly: "resilient = protected + multi-drive redundancy."

### Config schema

```toml
# Option 1: Promise level (Urd derives everything)
[[subvolumes]]
name = "subvol3-opptak"
source = "/mnt/btrfs-pool/subvol3-opptak"
protection_level = "resilient"
drives = ["WD-18TB"]  # which drives serve this promise

# Option 2: Custom (user specifies everything, same as today)
[[subvolumes]]
name = "subvol6-tmp"
source = "/mnt/btrfs-pool/subvol6-tmp"
# No protection_level → implicit "custom"
snapshot_interval = "1d"
send_enabled = false
local_retention = { daily = 7 }

# Option 3: Promise with overrides (advanced)
[[subvolumes]]
name = "htpc-home"
source = "/home"
protection_level = "protected"
drives = ["WD-18TB", "2TB-backup"]
snapshot_interval = "15m"  # Override: more frequent than derived
# send_interval, retention: derived from "protected"
```

### Config conflict resolution

**Rule:** When `protection_level` is set, the promise derives default values for all
operational parameters. Explicit overrides replace the derived values. The preflight
check warns when an override weakens the promise.

```rust
/// Resolve a subvolume's operational parameters.
/// Promise-derived values are the baseline; explicit config overrides replace them.
///
/// For `custom` (or absent `protection_level`): derive_policy returns None,
/// and resolution falls through to the existing defaults-based path — identical
/// to today's SubvolumeConfig::resolved(&defaults). (Review M1: property test
/// must verify byte-identical output for configs without protection_level.)
fn resolve_subvolume(
    sv: &SubvolumeConfig,
    defaults: &DefaultsConfig,
    run_frequency: RunFrequency,
) -> ResolvedSubvolume {
    match sv.protection_level {
        Some(ProtectionLevel::Custom) | None => {
            // Existing resolution path — unchanged behavior
            sv.resolved(defaults)
        }
        Some(level) => {
            let derived = derive_policy(level, run_frequency);
            ResolvedSubvolume {
                snapshot_interval: sv.snapshot_interval.unwrap_or(derived.snapshot_interval),
                send_interval: sv.send_interval.unwrap_or(derived.send_interval),
                send_enabled: sv.send_enabled.unwrap_or(derived.send_enabled),
                local_retention: sv.local_retention
                    .map(|r| r.merged_with(&derived.local_retention).resolved())
                    .unwrap_or(derived.local_retention),
                // ... etc
            }
        }
    }
}
```

**Property test (review M1):** For every existing subvolume config (no `protection_level`),
`resolve_subvolume(sv, defaults, any_frequency)` must produce identical output to
`sv.resolved(defaults)`. This guarantees the migration path is truly zero-breaking-change.

**Weakening detection in preflight:**

```
For each subvolume with protection_level set:
  derived = derive_policy(level, run_frequency)
  actual = resolved values (after overrides)

  if actual.snapshot_interval > derived.snapshot_interval:
    warn("snapshot_interval override ({actual}) is less frequent than
          {level} promise requires ({derived})")

  if actual.send_interval > derived.send_interval:
    warn("send_interval override weakens {level} promise")
```

**Weakening vs. voiding (review S2):** Not all overrides are equal. Some weaken the
promise (longer intervals than derived — performance still degrades gracefully); others
structurally void it (disabling sends on a level that requires external copies).

**Voiding overrides** (status display must reflect):
- `send_enabled = false` on `protected` or `resilient` (requires external sends)
- `drives = []` (empty list) on any level requiring external copies

**Weakening overrides** (warning in preflight, status shows promise level normally):
- `snapshot_interval` longer than derived
- `send_interval` longer than derived
- `local_retention` tighter than derived

When a voiding override is detected:
- Preflight emits an **error** (not warning): "protected requires external sends,
  but send_enabled = false — promise cannot be met"
- Status output displays the promise level with a degradation marker:
  `protected (degraded — no external sends)` instead of bare `protected`
- The awareness model evaluates normally (it doesn't know about promises), so
  the STATUS column will show the actual protection state

This means a user sees both the promise they set *and* the reality:
```
  subvol3-opptak   UNPROTECTED   protected (degraded — no external sends)
```

This is a warning, not a config error. The user may know what they're doing (mid-migration,
drive not yet purchased). But it surfaces the tension persistently in status output, not
just in a one-time log message.

### Config field: `run_frequency`

New top-level config field:

```toml
[general]
# How often Urd runs. Affects promise-derived intervals.
# "daily" = systemd timer at ~24h intervals (default)
# "sentinel" = Sentinel daemon manages timing
# Custom: "6h", "12h", etc.
run_frequency = "daily"
```

```rust
// In config.rs
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    // ... existing fields ...
    #[serde(default = "default_run_frequency")]
    pub run_frequency: RunFrequency,
}

fn default_run_frequency() -> RunFrequency {
    RunFrequency::Timer { interval: Interval::days(1) }
}
```

**Why explicit, not inferred from heartbeat?** Heartbeat history can drift (missed
runs, clock changes). An explicit config field is: (a) deterministic — same config
always produces same derivation, (b) validatable at config time, (c) clear to the
user about expectations.

When the Sentinel is running, it sets `run_frequency = "sentinel"` effectively (or
the config is updated to reflect this). The Sentinel design (5b) determines the exact
mechanism.

**Run-frequency consistency check (review S3):** To detect drift between declared and
actual run frequency, add a preflight check that compares `run_frequency` against
heartbeat history (when available). If the last N run timestamps show a cadence
inconsistent with the declared frequency (e.g., `run_frequency = "daily"` but runs are
6h apart, or runs are 3 days apart), warn:

```
⚠ run_frequency is "daily" but actual runs average 2.8 days apart.
  Promise-derived intervals assume daily runs — promises may be unachievable.
  Update run_frequency or check your systemd timer.
```

This doesn't infer the "correct" frequency — it uses the explicit config as the
expectation and heartbeat as the reality check. The check requires ≥3 heartbeat
timestamps to compute a meaningful average.

### Subvolume-to-drive mapping

New optional field on `SubvolumeConfig`:

```toml
[[subvolumes]]
name = "subvol3-opptak"
protection_level = "resilient"
drives = ["WD-18TB"]  # Only send to these drives
```

```rust
// In config.rs
pub struct SubvolumeConfig {
    // ... existing fields ...
    /// Which drives serve this subvolume's external promise.
    /// None = all drives (current behavior).
    #[serde(default)]
    pub drives: Option<Vec<String>>,
}
```

**Behavior:**
- `drives = None` (default, backward compatible): Send to all configured drives.
  Current behavior unchanged.
- `drives = Some(["WD-18TB"])`: Send only to WD-18TB. Other drives are skipped for
  this subvolume.
- Validation: all listed drives must exist in `[[drives]]`. Error if not.

**Promise validation with drive mapping:**
- `resilient` requires `min_external_drives = 2`. If `drives = ["WD-18TB"]` (only 1),
  the preflight check warns: "resilient requires 2 external drives, but only 1
  configured for subvol3-opptak."
- Drive capacity check: if `subvol3-opptak` (~3.4TB) maps to `2TB-backup` (~1.1TB
  available), preflight warns: "2TB-backup has insufficient capacity for subvol3-opptak
  (estimated 3.4TB, available 1.1TB)."

### Achievability validation

A promise is **achievable** if the derived policy can be fulfilled given:
1. Run frequency (can the timer/Sentinel create snapshots often enough?)
2. Drive topology (are enough drives configured and large enough?)
3. Retention alignment (do retention windows keep snapshots long enough for sends?)

Achievability is checked by the preflight module (pure, config-only for topology
checks; with `FileSystemState` for capacity checks in `init`/`verify`).

```rust
/// Achievability check results for a subvolume's promise.
pub struct AchievabilityResult {
    pub subvolume: String,
    pub level: ProtectionLevel,
    pub achievable: bool,
    pub issues: Vec<AchievabilityIssue>,
}

pub enum AchievabilityIssue {
    InsufficientDrives { required: usize, configured: usize },
    DriveCapacityInsufficient { drive: String, estimated_size: u64, available: u64 },
    RunFrequencyInsufficient { required_interval: Interval, actual: Interval },
    RetentionOverrideWeakens { field: String, derived: String, actual: String },
}
```

**Where achievability is checked:**
- `urd init`: Full check including drive capacity (has I/O)
- `urd verify`: Full check including drive capacity (has I/O)
- `urd backup` preflight: Config-only subset (no I/O), logs warnings
- `urd status`: Shows achievability in promise display (computed from awareness model
  + config, no new I/O)

### Awareness model integration

The awareness model already computes PROTECTED / AT RISK / UNPROTECTED using
multiplier thresholds on configured intervals. With promises:

**No change to the core algorithm.** The awareness model evaluates the *resolved*
intervals (after promise derivation + overrides). The promise level doesn't change
how freshness is computed — it changes what intervals are configured.

**New: Promise-level status display.**

```
urd status

  subvol3-opptak   PROTECTED   (resilient — 1 of 1 required drives current)
  htpc-home        AT RISK     (protected — external send overdue by 6h)
  subvol6-tmp      PROTECTED   (guarded — local only)
  htpc-root        PROTECTED   (custom)
```

The promise level appears as context alongside the status. For `custom`, no
derived expectation is shown.

**Threshold adaptation question (resolved):** Should thresholds change based on
whether the Sentinel is running? **No.** The thresholds evaluate outcomes (is the
data fresh enough?), not process (how often does Urd run?). `run_frequency` feeds
into *interval derivation*, not *threshold computation*. The awareness model is
agnostic to how the intervals were chosen.

### Migration path

**Zero breaking changes.** Every existing config continues to work identically.

1. No `protection_level` field → implicit `custom`
2. `custom` means: "I manage intervals and retention manually; the planner uses my
   explicit values; the awareness model evaluates against my configured intervals."
3. No `drives` field on subvolume → send to all drives (current behavior)
4. No `run_frequency` field → default `daily` (24h timer)

**Protection-level transition safety (review S1):**

Changing `protection_level` on an existing subvolume can cause retroactive snapshot
deletion if the derived retention is tighter than the previous explicit retention.
Example: switching from `monthly = 0` (unlimited) to `protected` (which derives
`monthly = 12`) would delete all monthly snapshots older than 12 months on the next
retention pass.

**Mitigation:** When `urd init` or `urd verify` detects a subvolume where the resolved
retention policy is strictly tighter than what the previous run used (detectable by
comparing against the most recent heartbeat's config snapshot or a stored config hash),
it warns:

```
⚠ subvol3-opptak: switching to "resilient" would reduce monthly retention
  from unlimited to 12 months. 3 monthly snapshots older than 12 months
  would be eligible for deletion on the next backup run.
  Run `urd backup --confirm-retention-change` to acknowledge.
```

The `--confirm-retention-change` flag is required for the first run after a retention
tightening. Without it, the backup runs but skips retention for the affected subvolumes
(fail open — backups happen, deletions are deferred until confirmed). This flag is
needed once; subsequent runs proceed normally.

**Implementation:** Store the last-used retention config per subvolume in the state DB.
On backup, compare resolved retention against stored. If any dimension is tighter and
the flag is not set, log a warning and skip retention for that subvolume.

**Upgrade path for users who want promises:**

```diff
 [[subvolumes]]
 name = "subvol3-opptak"
 source = "/mnt/btrfs-pool/subvol3-opptak"
-priority = 1
-snapshot_interval = "1h"
-send_interval = "2h"
+protection_level = "resilient"
+drives = ["WD-18TB"]
```

The user removes explicit intervals and adds a promise level. `urd init` shows the
derived policy so they can verify it matches their expectations.

### `urd status` with promises

Status output changes to lead with the promise perspective:

```
Urd Status — 2026-03-26 04:05

  SUBVOLUME          STATUS       PROMISE      LOCAL    WD-18TB   WD-18TB1   2TB-backup
  subvol3-opptak     PROTECTED    resilient    12       8         —          —
  htpc-home          AT RISK      protected    47       23        15         —
  subvol1-docs       PROTECTED    protected    12       8         5          3
  htpc-root          PROTECTED    custom       7        3         3          —
  subvol6-tmp        PROTECTED    guarded      7        —         —          —

  ⚠ htpc-home: external send to WD-18TB overdue (last: 30h ago, threshold: 36h)
```

The PROMISE column shows the configured level. For `custom`, no promise expectations
are displayed in advisories.

## Invariants

1. **Promises derive operations; they don't bypass the planner.** The planner still
   receives `ResolvedSubvolume` with concrete intervals and retention. Promise
   derivation happens at config resolution time, before the planner runs. (ADR-100)
2. **`custom` is first-class.** No code path treats `custom` as inferior or special-cased.
   It's the default, and it means "the user's config is the policy." (ADR-109)
3. **Promise derivation is a pure function.** `derive_policy(level, run_frequency)`
   has no I/O, no state, no side effects. (ADR-108)
4. **Existing configs don't break.** All fields added by this ADR are optional with
   backward-compatible defaults. No migration script needed. (ADR-105)
5. **Achievability is advisory, not blocking.** An unachievable promise generates
   warnings, not errors. The user may be in transition (new drive arriving, config
   change pending). Blocking on unachievable promises would violate ADR-107 (fail open).
6. **The awareness model is unchanged.** Promise levels affect what intervals are
   configured, not how the awareness model evaluates them. The model remains a pure
   function of config + filesystem state. (ADR-108)

## Integration Points

| Module | Change | Scope |
|--------|--------|-------|
| `config.rs` | `ProtectionLevel` enum, `drives` field, `run_frequency` field, derivation in `resolve_subvolume()` | Medium — extends existing resolution logic |
| `types.rs` | `ProtectionLevel`, `RunFrequency`, `DerivedPolicy` types | New types |
| `preflight.rs` | Achievability checks (drive count, retention weakening) | Extension (~3 new checks) |
| `awareness.rs` | **No changes** | — |
| `plan.rs` | Filter drive iteration per subvolume's `drives` field (review M2) | Small — add `drives: Option<Vec<String>>` to `ResolvedSubvolume`, skip unmapped drives in send loop |
| `voice.rs` | Promise column in status, promise context in advisories | Extension |
| `output.rs` | Promise level in `StatusOutput`, achievability in output types | Extension |
| `commands/init.rs` | Show derived policy for promise subvolumes | Extension |

**Files affected:** ~6 (config, types, preflight, voice, output, init)
**New module:** No (extends existing modules)

## Effort Estimate

| Task | Effort |
|------|--------|
| `ProtectionLevel` enum + `derive_policy()` | Low (types + pure function) |
| Config extension (3 new fields) + validation | Medium |
| Preflight achievability checks | Medium |
| Voice/output integration (status display) | Medium |
| Tests | Medium (~20-25 tests) |
| **Total** | **~2 sessions** |

Calibration: UUID fingerprinting was 1 module, 10 tests, 1 session. This is 0 new
modules but touches more files and has more test scenarios.

## Test Strategy

```
// Promise derivation (pure function)
test_derive_guarded_timer_daily
test_derive_protected_timer_daily
test_derive_resilient_timer_daily
test_derive_protected_sentinel
test_derive_custom_returns_none (no derivation for custom)

// Config resolution with promises
test_promise_sets_default_intervals
test_explicit_override_replaces_derived
test_no_protection_level_is_custom
test_migration_existing_config_unchanged  // property test: byte-identical (M1)
test_voiding_override_detected            // send_enabled=false on protected (S2)
test_weakening_override_generates_warning // longer interval on protected

// Achievability validation
test_resilient_with_one_drive_warns
test_protected_with_no_drives_warns
test_drive_capacity_insufficient_warns
test_retention_override_weakens_warns
test_run_frequency_insufficient_warns
test_all_achievable_no_warnings
test_custom_skips_achievability (no expectations to validate)

// Drive mapping
test_drives_field_filters_send_targets
test_drives_field_none_sends_to_all
test_drives_field_invalid_drive_errors
test_drives_field_validated_at_config_load

// Transition safety (S1)
test_retention_tightening_detected
test_retention_loosening_no_flag_needed
test_confirm_flag_allows_tighter_retention
test_skip_retention_without_confirm_flag

// Integration with awareness model
test_promise_derived_intervals_evaluated_correctly
test_custom_intervals_evaluated_as_before

// Run frequency consistency (S3)
test_run_frequency_matches_heartbeat_history
test_run_frequency_mismatch_warns
test_run_frequency_check_skipped_insufficient_history

// Pin protection with derived retention (test-team: critical)
test_promise_derived_retention_preserves_pinned_snapshots
test_promise_derived_retention_with_space_pressure_preserves_pins

// Full pipeline: promise → planner → retention (test-team: critical)
test_promise_level_to_retention_decision_pipeline

// Protection level downgrade (test-team: high)
test_downgrade_resilient_to_guarded_disables_sends
test_downgrade_does_not_delete_external_snapshots

// Planner drive mapping integration (test-team: high)
test_planner_skips_unmapped_drives_for_subvolume
test_planner_sends_to_all_drives_when_no_mapping
test_planner_drive_mapping_with_uuid_mismatch

// Migration identity property (test-team: proptest candidate)
test_migration_identity_property  // proptest: all configs without protection_level
                                  // produce identical output through old and new paths

// Voice rendering
test_status_shows_promise_column
test_advisory_includes_promise_context
test_custom_shows_no_promise_advisory
test_status_shows_degraded_promise_when_sends_disabled
```

## Rejected Alternatives

### A. `archival` as a promise level

Archival is about retention depth, not protection freshness. A subvolume can be
`protected` (fresh copies exist) with archival retention (monthly snapshots kept
indefinitely). Conflating them creates confusing status: "archival + AT RISK" could
mean either "old snapshots are fine but recent ones are stale" or "the archival promise
itself is degraded." Keep them separate. Add `retention_profile` later if needed.

### B. Infer run frequency from heartbeat history

Heartbeat history can show actual run frequency, but: (a) new installs have no history,
(b) missed runs skew the average, (c) config should be deterministic — same config
always produces the same derivation. An explicit `run_frequency` field is simpler and
more reliable. The Sentinel can update this field (or effectively override it) when
it takes over scheduling.

### C. Promise levels as interval multipliers

Instead of named levels, define promises as multipliers: "2× the default interval" for
protected, "1× for resilient." This is elegant but opaque — users can't reason about
what "2× the default" means without knowing the default. Named levels with documented
outcome targets are more transparent.

### D. Require drive mapping for all promise levels

Making `drives = [...]` mandatory (not just for `resilient`) would be more explicit but
adds friction for simple cases. A user with one drive who sets `protection_level =
"protected"` shouldn't have to also list that drive. Default: send to all drives.
`drives` mapping becomes important for `resilient` (counting) and for topology
validation, but it's always optional.

### E. Dynamic thresholds based on promise level

Make the awareness model use tighter multipliers for `resilient` than `protected`. This
couples the promise level to the evaluation logic, creating two paths through the
awareness model. The current design keeps evaluation uniform (same multipliers for all)
and varies the *inputs* (intervals derived from promise level). Simpler, more testable.

### F. Config conflict as error (not warning)

If a user sets `protection_level = "protected"` and `send_enabled = false`, that's
contradictory. Should it be an error (refuse to load) or a warning (load with override)?
Errors are safer but hostile — the user might be mid-migration. Warnings respect user
intent while surfacing the tension. ADR-107 (fail open) supports warnings.

## Open Questions

1. **Should `priority` be derived from `protection_level`?** Currently, priority is a
   separate field (1-3) controlling execution order. It would be natural for `resilient`
   to imply priority 1 and `guarded` to imply priority 3. But this couples two concepts
   that might want independent control. Recommendation: derive default priority from
   promise level, allow override.

2. **Per-drive send intervals.** Should `protected` allow different send intervals per
   drive? ("Send to WD-18TB daily, to WD-18TB1 weekly.") This is drive-level policy,
   not subvolume-level. Defer — the current model sends to all mapped drives at the
   same interval. Per-drive intervals can come later if the use case materializes.

3. **How does `urd setup` (Priority 6c) use promises?** The conversational setup wizard
   would guide users to choose a promise level rather than configuring intervals. This
   is the ideal entry point for promises. But the wizard isn't built yet. Promises must
   work well in TOML config first.

4. **Display name for promise levels.** Should `urd status` show "resilient" or
   "RESILIENT" or "Resilient"? Recommendation: lowercase in config, UPPERCASE in status
   output (matching PROTECTED/AT RISK/UNPROTECTED convention). But this might look
   noisy with two uppercase fields per row.

## Review Findings Addressed

This design was reviewed by arch-adversary on 2026-03-26. All findings addressed:

| Finding | Severity | Resolution |
|---------|----------|------------|
| S1. Protection level change triggers retroactive deletion | Significant | Added transition safety: `--confirm-retention-change` flag required on first run after retention tightening. Backup proceeds but skips retention for affected subvolumes until confirmed. |
| S2. Override combinations void promise with no persistent signal | Significant | Distinguished weakening (warning) from voiding (degradation marker in status). Voiding overrides show `protected (degraded — reason)` in status output. |
| S3. run_frequency as manual config drifts from reality | Significant | Added preflight check comparing declared frequency against heartbeat history. Warns when actual cadence diverges (≥3 timestamps required). |
| M1. custom derivation path underspecified | Moderate | `Custom` / absent `protection_level` falls through to existing `sv.resolved(defaults)` path unchanged. Property test verifies byte-identical output. |
| M2. Drive mapping requires planner change | Moderate | Acknowledged: `ResolvedSubvolume` gains `drives: Option<Vec<String>>`, planner filters drive iteration. Integration table updated. |
| M3. Outcome targets derived from multipliers, not policy | Moderate | Documented targets as primary policy. Added consistency check between promise targets and awareness multipliers in preflight. |
| M4. Identical retention for protected/resilient | Moderate | Documented: resilient = protected + multi-drive redundancy. Retention divergence deferred until user feedback confirms need. |
| N1. short_name absent from examples | Minor | Noted. Config examples are illustrative; short_name remains required. |
| N2. Display convention unresolved | Minor | Decision: lowercase in config and status output. UPPERCASE reserved for STATUS column (PROTECTED/AT RISK/UNPROTECTED). |

[Review report](../99-reports/2026-03-26-protection-promises-design-review.md)
