# ADR-110: Protection Promises

> **TL;DR:** Protection promises are named levels (`guarded`, `protected`, `resilient`, `custom`)
> that map to concrete retention and interval policies. The user declares intent; Urd derives
> operations. Existing configs continue to work as implicit `custom` with zero breaking changes.
> Promise derivation is a pure function at config resolution time — the planner, executor, and
> awareness model are unchanged.

**Date:** 2026-03-26
**Status:** Accepted
**Depends on:** ADR-100 (planner/executor separation), ADR-108 (pure function modules),
ADR-109 (config boundary validation)

## Context

Urd currently speaks in operations: snapshot intervals, send intervals, retention windows.
Users must reason backwards from these to answer "is my data safe?" The awareness model
(already built) computes PROTECTED / AT RISK / UNPROTECTED, but the thresholds are derived
from whatever intervals the user configured — there's no semantic link between the user's
*intent* ("keep my recordings safe") and the system's *behavior* (1h snapshots, 2h sends).

The config/timer mismatch incident (2026-03-26) demonstrated the problem: intervals configured
for sub-hourly cadence, but the systemd timer fires daily. The awareness model reported
UNPROTECTED for 18h/day — technically correct but misleading. This happened because the config
describes *desired cadence* without reference to *actual run frequency*.

Protection promises bridge this gap: the user declares what they want; Urd derives what to do.

## Decision

### Four promise levels

```rust
pub enum ProtectionLevel {
    Guarded,    // Local snapshots only. For: temp data, caches, build artifacts.
    Protected,  // Local + at least one external drive current. For: documents, photos.
    Resilient,  // Local + multiple external drives current. For: irreplaceable data.
    Custom,     // User manages all parameters manually. Default for migration.
}
```

No `archival` level. Retention depth is orthogonal to protection freshness — a subvolume can
be `protected` (current copies exist) and have deep retention (keep monthly snapshots
indefinitely). Conflating them creates confusing semantics. If archival becomes needed, it
should be a separate `retention_profile`.

### Outcome targets per level

Each level defines **maximum acceptable age** for local and external copies — outcomes, not
frequencies. These targets are primary policy, defensible independent of awareness multipliers.

| Level | Local max age | External max age | Min external drives | Retention floor |
|-------|--------------|------------------|--------------------|-----------------------|
| `guarded` | 48h | — (no external) | 0 | daily=7, weekly=4 |
| `protected` | 24h | 48h | 1 | daily=30, weekly=26, monthly=12 |
| `resilient` | 24h | 48h | 2 | daily=30, weekly=26, monthly=12 |
| `custom` | (from config) | (from config) | (from config) | (from config) |

### Pure derivation function

```rust
pub fn derive_policy(level: ProtectionLevel, run_frequency: RunFrequency) -> DerivedPolicy
```

This is a pure function (ADR-108) that maps a promise level + run frequency to concrete
operational parameters: snapshot_interval, send_interval, send_enabled, local_retention,
external_retention, min_external_drives.

Run frequency is an explicit config field, not inferred:

```rust
pub enum RunFrequency {
    Timer { interval: Interval },  // systemd timer, typically daily
    Sentinel,                      // Sentinel daemon, sub-hourly checks
}
```

### Config schema

```toml
[general]
run_frequency = "daily"

[[subvolumes]]
name = "subvol3-opptak"
protection_level = "resilient"
drives = ["WD-18TB"]  # Which drives serve this promise

[[subvolumes]]
name = "subvol6-tmp"
# No protection_level -> implicit "custom"
snapshot_interval = "1d"
send_enabled = false
```

### Override resolution

When `protection_level` is set, the promise derives default values. Explicit overrides replace
the derived values. The preflight module warns when overrides weaken or void the promise.

**Voiding overrides** (structurally incompatible — status shows degradation marker):
- `send_enabled = false` on `protected` or `resilient`
- `drives = []` on levels requiring external copies

**Weakening overrides** (warning in preflight, status shows promise level normally):
- `snapshot_interval` longer than derived
- `send_interval` longer than derived
- `local_retention` tighter than derived

### Transition safety

Changing `protection_level` on an existing subvolume can cause retroactive snapshot deletion
if the derived retention is tighter than the previous explicit retention. Mitigation:
`--confirm-retention-change` flag required for the first run after retention tightening.
Without it, backups proceed but retention is skipped for affected subvolumes (fail open,
ADR-107).

### Migration path

Zero breaking changes. Every existing config continues to work identically:

1. No `protection_level` field -> implicit `custom`
2. `custom` means: user's explicit values, same resolution as today
3. No `drives` field on subvolume -> send to all drives (current behavior)
4. No `run_frequency` field -> default `daily` (24h timer)

Property test required: for all configs without `protection_level`, the new resolution path
must produce byte-identical output to the current `SubvolumeConfig::resolved()`.

### Achievability validation

A promise is achievable if the derived policy can be fulfilled given run frequency, drive
topology, and retention alignment. Achievability is checked by the preflight module — advisory
(warnings), not blocking (errors). ADR-107: fail open.

### Subvolume-to-drive mapping

New optional `drives` field on subvolume config. `None` = send to all drives (current
behavior). `Some(["WD-18TB"])` = send only to listed drives. Validated against configured
drives at config load time.

## Consequences

### Positive

- Users express intent, not operations — "protect my recordings" instead of interval math
- Config validation catches unachievable promises at load time
- Status output answers "is my data safe?" in promise-level terms
- The awareness model is completely unchanged — promises affect inputs, not evaluation
- Zero migration burden for existing users

### Negative

- Config resolution has two paths (`custom` vs named level) — must be tested for equivalence
- Override interactions create a combinatorial space for preflight warnings
- `--confirm-retention-change` is a speed bump for legitimate config changes

### Risks

- **Retroactive deletion on level change** — mitigated by `--confirm-retention-change` flag
  and fail-open retention skip. Two critical safety tests required:
  `test_promise_derived_retention_preserves_pinned_snapshots` and
  `test_promise_derived_retention_with_space_pressure_preserves_pins`.
- **Promise-derived retention bypassing pin protection** — one bug from silent data loss.
  The three-layer pin protection (ADR-106) is the safety net. Test required:
  `test_promise_level_to_retention_decision_pipeline`.

## Invariants

1. **Promises derive operations; they don't bypass the planner.** Config resolution happens
   before the planner runs. The planner receives `ResolvedSubvolume` with concrete values,
   never `ProtectionLevel`. (ADR-100)
2. **`custom` is first-class.** No code path treats it as inferior. It means "the user's
   config is the policy." (ADR-109)
3. **Promise derivation is a pure function.** `derive_policy()` has no I/O, no state, no
   side effects. (ADR-108)
4. **Existing configs don't break.** All new fields are optional with backward-compatible
   defaults. No migration script needed. (ADR-105)
5. **Achievability is advisory, not blocking.** Warnings, not errors. The user may be in
   transition. (ADR-107)
6. **The awareness model is unchanged.** Promise levels affect what intervals are configured,
   not how evaluation works. (ADR-108)

## Implementation Gates

This ADR is considered implemented when:

- [ ] `ProtectionLevel` enum and `derive_policy()` exist in `types.rs`
- [ ] `protection_level`, `drives`, `run_frequency` config fields are parsed and validated
- [ ] `resolve_subvolume()` branches on protection level with custom fallthrough
- [ ] Migration identity property test passes
- [ ] Achievability preflight checks are active
- [ ] `--confirm-retention-change` flag gates retention tightening
- [ ] `urd status` shows promise level column
- [ ] Pin-protection safety tests pass with derived retention

## Related

- Design: `docs/95-ideas/2026-03-26-design-protection-promises.md`
- Design review: `docs/99-reports/2026-03-26-protection-promises-design-review.md`
- Test strategy review: `docs/99-reports/2026-03-26-test-strategy-review.md`
