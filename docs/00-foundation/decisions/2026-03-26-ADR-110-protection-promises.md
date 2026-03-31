# ADR-110: Protection Promises

> **TL;DR:** Protection promises are named levels that map to concrete operational policies.
> The user declares intent; Urd derives operations. Named levels are opaque — no per-field
> overrides. `Custom` means the user manages all parameters explicitly. Named levels must
> earn opaque status through operational track record; the current taxonomy
> (guarded/protected/resilient) is provisional and subject to rework.

**Date:** 2026-03-26 (revised 2026-03-27, addendum 2026-03-31)
**Status:** Accepted (taxonomy provisional — see Maturity Model)
**Depends on:** ADR-100 (planner/executor separation), ADR-108 (pure function modules),
ADR-109 (config boundary validation), ADR-111 (config system architecture)
**Ownership:** This ADR is authoritative for protection promise *semantics* — what levels
mean, how they derive policy, the maturity model, and the opacity rule. ADR-111 is
authoritative for config *structure* — what fields exist, how validation works, versioning.

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

A subsequent design review (2026-03-27) found that the original override semantics
(voiding/weakening overrides on named levels) created contradictory configs and noisy
preflight warnings. The review also found the current three-level taxonomy insufficiently
mature — "guarded" vs "protected" are near-synonyms that don't communicate the operational
axis (local-only vs external copies). These findings led to the revised design below.

## Decision

### Named levels are opaque

When `protection_level` is set to a named level, the level's derived parameters are the
final values. There are no per-field overrides. The level is a sealed policy — the user
trusts Urd to deliver it.

If the user needs different parameters from what a named level provides, they omit
`protection_level` and specify all values explicitly (custom policy). There is no middle
ground. This eliminates the override resolution complexity, the voiding/weakening
distinction, and preflight warnings from intentional deviations.

### Custom is first-class

`Custom` is not a fallback or inferior mode. It means the operator owns the full policy.
No code path treats it as lesser. It is the appropriate — and currently recommended —
choice when named levels haven't earned their keep through operational evidence.

### Current taxonomy (provisional)

The current named levels are:

```rust
pub enum ProtectionLevel {
    Guarded,    // Local snapshots only. For: temp data, caches, build artifacts.
    Protected,  // Local + at least one external drive current. For: documents, photos.
    Resilient,  // Local + multiple external drives current. For: irreplaceable data.
    Custom,     // User manages all parameters manually.
}
```

**This taxonomy is provisional.** The design review identified that:

- "Guarded" and "protected" are near-synonyms that don't communicate the actual
  operational difference (local-only vs external copies).
- "Protected" and "resilient" are operationally identical except for `min_external_drives`.
  This may be insufficient to justify two distinct opaque levels.
- Level names should communicate operational meaning to the user — the naming axis should
  make the promise legible without reading documentation.

The taxonomy will be reworked in a future design session once more operational experience
exists. Until then, `custom` with explicit parameters is the recommended approach for
production configs.

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
pub fn derive_policy(level: ProtectionLevel, run_frequency: RunFrequency) -> Option<DerivedPolicy>
```

This is a pure function (ADR-108) that maps a promise level + run frequency to concrete
operational parameters: snapshot_interval, send_interval, send_enabled, local_retention,
external_retention, min_external_drives. Returns `None` for `Custom`.

Run frequency is an explicit config field, not inferred:

```rust
pub enum RunFrequency {
    Timer { interval: Interval },  // systemd timer, typically daily
    Sentinel,                      // Sentinel daemon, sub-hourly checks
}
```

### Config interaction

Named levels produce all operational parameters. The subvolume config for a named level
contains only identity and the level itself:

```toml
[[subvolumes]]
name = "subvol3-opptak"
source = "/mnt/btrfs-pool/subvol3-opptak"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
protection_level = "resilient"
drives = ["WD-18TB", "WD-18TB1"]
```

Operational fields (`snapshot_interval`, `send_interval`, `local_retention`,
`external_retention`) are **not permitted** alongside a named protection level. If present,
config validation rejects the file as a structural error (ADR-111). This is the enforcement
mechanism for opacity.

For custom policies, all operational fields are specified explicitly:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
snapshot_interval = "1w"
send_enabled = false
local_retention = { daily = 3, weekly = 2 }
```

### Transition safety

Changing `protection_level` on an existing subvolume can cause retroactive snapshot deletion
if the derived retention is tighter than the previous explicit retention. Mitigation:
`--confirm-retention-change` flag required for the first run after retention tightening.
Without it, backups proceed but retention is skipped for affected subvolumes (fail open,
ADR-107).

### Achievability validation

A promise is achievable if the derived policy can be fulfilled given run frequency, drive
topology, and retention alignment. Achievability has two tiers:

**Structural unachievability (hard error — refuse to start):** The config makes it
impossible to fulfill the promise. Examples: a `resilient` subvolume with only 1 drive
in its `drives` list (needs 2), or a named level with `drives` omitted when the level
requires external sends. These are authoring mistakes, not runtime conditions — the
config is wrong. Caught by `Config::validate()` (ADR-109, ADR-111).

**Runtime unachievability (advisory warning — fail open):** The config is correct but the
world isn't ready. Examples: a drive is configured but not mounted, or a filesystem is
temporarily below `min_free_bytes`. The backup runs what it can and reports what was
skipped (ADR-107, ADR-111 structural vs runtime distinction).

## Maturity Model

Named levels earn opaque status through a two-phase trajectory:

### Phase 1: Custom-first (current)

Custom is the recommended default. Named levels exist in the codebase but are understood
to be provisional. Templates based on named level parameters help operators scaffold
custom configs — the template is guidance, the resulting custom config is the policy.

### Phase 2: Named levels graduate

A named level graduates to production-ready opaque status when it has:

1. **Operational track record** — run as a template-based custom policy in production for
   a meaningful period without the operator needing to intervene or override.
2. **Distinct operational identity** — parameters that are meaningfully different from
   every other level. If two levels are identical except for one field, they may not
   justify separate names.
3. **Self-explanatory name** — the operator can infer what the level does from its name
   without reading documentation. The naming axis should communicate operational meaning
   (where copies exist, what failures the data survives).
4. **ADR documentation** — rationale for the level's specific parameter choices, grounded
   in operational evidence from Phase 1.

Design completeness alone (tests, docs, ADR) is necessary but insufficient. Graduation
requires data and operational understanding — you can't design a battle-tested level at a
desk.

## Consequences

### Positive

- Users express intent, not operations — "protect my recordings" instead of interval math
- No override complexity — levels are sealed, custom is explicit
- Config validation catches structural errors (operational fields mixed with named levels)
- Status output answers "is my data safe?" in promise-level terms
- The awareness model is completely unchanged — promises affect inputs, not evaluation
- The maturity model prevents premature promotion of untested levels

### Negative

- Operators cannot make small adjustments to named levels — they must go fully custom for
  any deviation. Templates mitigate this by providing a starting point.
- The provisional taxonomy means named levels are not yet recommended for production use.
  This is honest but means the promise model's full value is deferred.
- Taxonomy rework will require a config schema version bump and `urd migrate` (ADR-111).

### Risks

- **Retroactive deletion on level change** — mitigated by `--confirm-retention-change` flag
  and fail-open retention skip.
- **Promise-derived retention bypassing pin protection** — one bug from silent data loss.
  The three-layer pin protection (ADR-106) is the safety net.
- **Permanent deferral** — if graduation criteria are too strict, named levels never mature
  and custom remains the permanent reality. Mitigation: the criteria are evidence-based
  (operational track record), not process-based (committee approval).

## Invariants

1. **Named levels are opaque.** When set, derived parameters are final. No overrides.
   Operational fields alongside a named level are a config validation error. (ADR-111)
2. **Custom is first-class.** No code path treats it as inferior. It means "the operator's
   config is the policy." (ADR-111)
3. **Promises derive operations; they don't bypass the planner.** Config resolution happens
   before the planner runs. The planner receives `ResolvedSubvolume` with concrete values,
   never `ProtectionLevel`. (ADR-100)
4. **Promise derivation is a pure function.** `derive_policy()` has no I/O, no state, no
   side effects. (ADR-108)
5. **Achievability is advisory, not blocking.** Warnings, not errors. The user may be in
   transition. (ADR-107)
6. **The awareness model is unchanged.** Promise levels affect what intervals are configured,
   not how evaluation works. (ADR-108)

## Addendum: Offsite Freshness Contract (2026-03-31)

**Context:** Design 6-E (Promise Redundancy Encoding) makes the resilient level's geographic
requirement explicit. This addendum documents the offsite freshness contract and confirms
that it preserves the invariants above.

### Resilient requires offsite

The resilient level encodes geographic redundancy: at least one configured drive must have
`role = "offsite"`. A resilient subvolume with no offsite-role drive triggers a preflight
advisory (`resilient-without-offsite`). This is an achievability gap, not a structural
error — backups proceed but the promise cannot be fully met (ADR-109).

### Offsite freshness thresholds

For resilient subvolumes, the newest successful send to any offsite-role drive determines
an offsite freshness status:

| Offsite age (days) | Offsite freshness status |
|--------------------|--------------------------|
| 0–30               | PROTECTED                |
| 31–90              | AT RISK                  |
| > 90 (or no send)  | UNPROTECTED              |

The overall subvolume status is `min(local_status, best_external_status, offsite_freshness_status)`.
Offsite freshness is an additional constraint — it does not replace per-drive freshness
assessment.

These thresholds define what "resilient" means operationally: **monthly-or-better offsite
rotation.** Users with longer rotation cycles must use `protection_level = "custom"`.
The thresholds are fixed (not user-configurable), consistent with the opacity principle
for named levels.

### Invariant 6 preserved

The offsite freshness computation is a **post-processing overlay** (`overlay_offsite_freshness()`)
that runs after `assess()` returns. The awareness model itself remains protection-level-blind.
It does not know whether a subvolume is resilient, protected, or guarded. The overlay
operates on assessment results plus config, outside the awareness loop.

This preserves Invariant 6: promise levels affect what intervals are configured, not how
evaluation works. The overlay is a separate pure function (ADR-108) that applies an
additional constraint based on protection level.

### Scope

- Only resilient subvolumes are affected. Protected, guarded, and custom are unchanged.
- The existing 7-day "consider cycling" advisory is replaced by structured offsite freshness
  degradation for resilient subvolumes. Protected subvolumes keep the existing advisory.
- See Design 6-E (`docs/95-ideas/2026-03-31-design-e-promise-redundancy-encoding.md`) for
  full rationale and review findings.

## Implementation Gates

This ADR is considered implemented when:

- [ ] `ProtectionLevel` enum and `derive_policy()` exist in `types.rs`
- [ ] `protection_level`, `drives`, `run_frequency` config fields are parsed and validated
- [ ] Config validation rejects operational fields alongside named protection levels
- [ ] `resolve_subvolume()` branches on protection level with custom fallthrough
- [ ] Achievability preflight checks are active
- [ ] `--confirm-retention-change` flag gates retention tightening
- [ ] `urd status` shows promise level column
- [ ] Pin-protection safety tests pass with derived retention

## Related

- ADR-111: Config system architecture (governs config structure, versioning, validation)
- Design: `docs/95-ideas/2026-03-26-design-protection-promises.md`
- Design review: `docs/99-reports/2026-03-26-protection-promises-design-review.md`
- Config design review: `docs/98-journals/2026-03-27-config-design-review.md`
- Test strategy review: `docs/99-reports/2026-03-26-test-strategy-review.md`
