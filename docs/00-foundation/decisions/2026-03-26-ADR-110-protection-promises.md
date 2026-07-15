---
type: ADR
title: Protection Promises
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-26'
timestamp: '2026-05-30T21:25:09+02:00'
---
# ADR-110: Protection Promises

> **TL;DR:** Protection promises are named levels that map to concrete operational policies.
> The user declares intent; Urd derives operations. Named levels are opaque — no per-field
> overrides. `Custom` means the user manages all parameters explicitly. Named levels must
> earn opaque status through operational track record. Current taxonomy:
> recorded/sheltered/fortified (renamed 2026-04-03 from guarded/protected/resilient).

**Date:** 2026-03-26 (revised 2026-03-27, addendum 2026-03-31, vocabulary 2026-04-03, amendments 2026-05-09 / 2026-05-15 / 2026-05-30)
**Status:** Accepted (taxonomy renamed 2026-04-03 — see Maturity Model; recommendation-layer amendment 2026-05-09 — see [Amendment 2026-05-09](#amendment-2026-05-09-recommendation-layer-as-graduation-evidence-path-adr-115); AT-RISK cap at Critical overturns R4 2026-05-30 — see [Amendment 2026-05-30](#amendment-2026-05-30-at-risk-cap-at-the-critical-tier-upi-031-b--overturns-r4))
**Depends on:** ADR-100 (planner/executor separation), ADR-108 (pure function modules),
ADR-109 (config boundary validation), ADR-111 (config system architecture)
**Amended by:** ADR-115 (Retention shape symmetry and the recommendation layer)
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
preflight warnings. The review also found the original three-level taxonomy insufficiently
mature — "guarded" vs "protected" were near-synonyms that didn't communicate the operational
axis (local-only vs external copies). These findings led to the revised design below.

A vocabulary session (2026-04-03) renamed the levels to communicate the operational axis:
recorded (data is recorded locally), sheltered (data is sheltered on external drive),
fortified (data is fortified across geography). The names describe what the data *becomes*,
not the mechanism.

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

### Current taxonomy

The current named levels are:

```rust
pub enum ProtectionLevel {
    Recorded,   // Local snapshots only. For: temp data, caches, build artifacts.
    Sheltered,  // Local + at least one external drive current. For: documents, photos.
    Fortified,  // Local + multiple external drives + offsite. For: irreplaceable data.
    Custom,     // User manages all parameters manually.
}
```

The names communicate the operational axis — what the data *becomes*:
- **Recorded:** data is recorded in history (local snapshots exist on this machine)
- **Sheltered:** data is sheltered from hardware failure (survives drive loss)
- **Fortified:** data is fortified against site loss (survives fire, theft, flood)

Until named levels earn opaque status through operational evidence (see Maturity Model),
`custom` with explicit parameters remains the recommended approach for production configs.

### Outcome targets per level

Each level defines **maximum acceptable age** for local and external copies — outcomes, not
frequencies. These targets are primary policy, defensible independent of awareness multipliers.

| Level | Local max age | External max age | Min external drives | Retention floor |
|-------|--------------|------------------|--------------------|-----------------------|
| `recorded` | 48h | — (no external) | 0 | daily=7, weekly=4 |
| `sheltered` | 24h | 48h | 1 | daily=30, weekly=26, monthly=12 |
| `fortified` | 24h | 48h | 2 | daily=30, weekly=26, monthly=12 |
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
protection_level = "fortified"
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
impossible to fulfill the promise. Examples: a `fortified` subvolume with only 1 drive
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

**Context:** Design 6-E (Promise Redundancy Encoding) makes the fortified level's geographic
requirement explicit. This addendum documents the offsite freshness contract and confirms
that it preserves the invariants above.

### Fortified requires offsite

The fortified level encodes geographic redundancy: at least one configured drive must have
`role = "offsite"`. A fortified subvolume with no offsite-role drive triggers a preflight
advisory (`fortified-without-offsite`). This is an achievability gap, not a structural
error — backups proceed but the promise cannot be fully met (ADR-109).

### Offsite freshness thresholds

For fortified subvolumes, the newest successful send to any offsite-role drive determines
an offsite freshness status:

| Offsite age (days) | Offsite freshness status |
|--------------------|--------------------------|
| 0–30               | PROTECTED                |
| 31–90              | AT RISK                  |
| > 90 (or no send)  | UNPROTECTED              |

The overall subvolume status is `min(local_status, best_external_status, offsite_freshness_status)`.
Offsite freshness is an additional constraint — it does not replace per-drive freshness
assessment.

These thresholds define what "fortified" means operationally: **monthly-or-better offsite
rotation.** Users with longer rotation cycles must use custom protection.
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

- Only fortified subvolumes are affected. Sheltered, recorded, and custom are unchanged.
- The existing 7-day "consider cycling" advisory is replaced by structured offsite freshness
  degradation for fortified subvolumes. Sheltered subvolumes keep the existing advisory.
- See Design 6-E (`docs/95-ideas/2026-03-31-design-e-promise-redundancy-encoding.md`) for
  full rationale and review findings.

## Amendment 2026-05-09: Recommendation Layer as Graduation Evidence Path (ADR-115)

**Context:** Phase D-2 (UPI 041) introduces a recommendation layer over
the symmetric data-cost model — see ADR-115. The recommendation layer
surfaces, per subvolume, a retention shape that fits the observed churn,
and tells the user what the current shape costs. This amendment names
the recommendation layer as the **path by which named levels accumulate
the operational evidence this ADR's Maturity Model required for
graduation to opaque status**.

### How named levels graduate under the amended model

Phase 1 (Custom-first, current) is unchanged. Custom remains the
recommended default until named levels earn opaque status.

The amended graduation criteria (replacing the original four) are:

1. **Operational track record** — run as a template-based custom policy
   in production for a meaningful period without operator intervention.
   *(Unchanged from original.)*
2. **Distinct operational identity** — parameters meaningfully different
   from every other level. *(Unchanged from original.)*
3. **Self-explanatory name** — operator can infer what the level does
   from its name without reading documentation. *(Unchanged from
   original.)*
4. **ADR documentation** — rationale for the level's specific parameter
   choices, grounded in operational evidence from Phase 1. *(Unchanged
   from original.)*
5. **Alignment with the recommendation engine across enough hosts to
   constitute a track record (NEW).** A named level is calibrated when
   the recommendation engine, given representative real-world drift
   signals, produces shapes that match the level's derived retention
   for the kind of subvolume the level is supposed to fit. Persistent
   divergence — the engine consistently recommending tighter or looser
   shapes than the named level produces — is evidence that the level's
   parameters are miscalibrated and not yet ready for opaque status.

### Why this amendment

ADR-110 originally framed graduation as "operational track record +
ADR documentation." It did not specify *what evidence track-record
generates*. The recommendation layer fills that gap: the engine's
output **is** the evidence. Named levels graduate when their derived
shapes consistently match what the engine recommends for representative
data. Until then, named levels remain provisional templates — useful
scaffolding for new operators, not yet sealed policies.

This is consistent with ADR-110's original intent ("you can't design a
battle-tested level at a desk") — it just makes the evidence channel
concrete.

### What this amendment does NOT change

- **Opacity of named levels remains absolute.** When `protection_level`
  is set to a named level, derived parameters are final. No per-field
  overrides. This is enforced by config validation (ADR-111).
- **The recommendation engine never mutates `derive_policy()`** (ADR-115
  invariant 2). Recommendations are advisory; if a recommendation
  differs from a named-level shape, the user must switch to `custom`
  to apply it. Voice surfaces this transition explicitly per the X1
  design.
- **The maturity model remains evidence-based, not process-based.**
  Graduation is not a vote or a committee decision; it is a finding
  that the recommendation engine and the named level converge on
  representative data.

### Phase 2 trigger

Phase 2 (Named levels graduate) is triggered when, across a population
of hosts running Urd in production, the recommendation engine's outputs
align with at least one named level's derived shapes for the
characteristic subvolumes that level is meant to serve. The arc's
done-ness criterion 6 (post-X4 evidence checkpoint) is the first
opportunity to evaluate this on a single host; broader graduation
requires multi-host evidence as Urd matures toward v1.0.

If the evidence shows persistent divergence — recommendations
consistently differ from named-level shapes — the named-level taxonomy
itself may be miscalibrated and warrant rework rather than graduation.
Either outcome is honest: graduation on alignment, taxonomy revision on
divergence, both grounded in evidence.

## Implementation Gates

This ADR is considered implemented when:

- [x] `ProtectionLevel` enum and `derive_policy()` exist in `types.rs`
- [x] `protection_level`, `drives`, `run_frequency` config fields are parsed and validated
- [ ] Config validation rejects operational fields alongside named protection levels (v1 schema, ADR-111)
- [x] `resolve_subvolume()` branches on protection level with custom fallthrough
- [x] Achievability preflight checks are active
- [x] `--confirm-retention-change` flag gates retention tightening
- [x] `urd status` shows promise level column
- [x] Pin-protection safety tests pass with derived retention
- [ ] Recommendation layer ships and surfaces per-subvolume shape advice (UPI 041 / ADR-115)

## Amendment 2026-05-15: `recorded_external_retention.monthly` correction

UPI 042 corrects an asymmetry in `derive_policy()` that was an oversight, not a designed
behavior. Under v1 semantics, `monthly = 0` meant "unlimited"; under v2, internal
construction of `MonthlyCount::Count(0)` means "no monthly retention." The four
`ResolvedGraduatedRetention` literals in `derive_policy()` are corrected to express their
*intent* in the new type system:

| Field                                  | v1 literal | v2 type                       | Semantic       |
|----------------------------------------|------------|-------------------------------|----------------|
| `recorded_retention.monthly`           | `0`        | `MonthlyCount::Count(0)`      | no monthly     |
| `full_retention.monthly`               | `12`       | `MonthlyCount::Count(12)`     | 12 months      |
| `full_external_retention.monthly`      | `0`        | `MonthlyCount::Unlimited`     | unbounded      |
| `recorded_external_retention.monthly`  | `0`        | `MonthlyCount::Count(0)`      | no monthly     |

The fourth row is the **Branch E correction**. Under v1 semantics, this field rendered as
"unlimited monthly," matching the third row by accident — not by design. The intent for
`recorded` (the Recorded protection level) is "no monthly retention" both locally and
externally; the v1 type system couldn't express that distinction, so the literal was forced
to a value (`0`) that happened to mean "unlimited" externally.

### Behavior-neutral today

Verified at `plan.rs:470, 511`: when `send_enabled = false` (always true for `recorded`
under any timer/sentinel mode), `plan_external_retention` is never invoked. The current
value is dead code in steady state, so the correction is behavior-neutral today.

### Forward-looking safety

If any future variant ever gives Recorded an external send, the *correct* default lands
("no monthly retention" matching local) rather than the historical accident ("unlimited
monthly retention" via the v1 type ambiguity). Locking in the correction now avoids
re-litigating it later under pressure.

### Visible effect

The only user-visible surface affected is `urd doctor --thorough`'s display of a Recorded
subvolume's external policy shape: it now reads "no monthly" instead of "unlimited monthly."
Display-only; no retention behavior changes.

## Amendment 2026-05-30: AT-RISK cap at the Critical tier (UPI 031-b — overturns R4)

The Do-No-Harm arc decision **R4** (recorded in the 031-a journal and the arc-regrill
doc) held that *"the promise degrades only via honest staleness"* — i.e. storage posture
(`TightnessTier`) was a presentation axis strictly **separate** from `PromiseStatus`, and
a tight pool could never, by itself, move the promise. UPI 031-b **overturns R4,
eyes-open.**

**The change.** At the **Critical** tier, the tier-graded ephemeral spine deliberately
slows Urd's send cadence (clear-all + a weekly interval floor) to bound Urd's local
footprint on a dangerously tight pool. Judged honestly, the subvolume *is* less protected
than its declared cadence promised — a fresh-but-weekly external copy is genuinely a
weaker guarantee than a fresh-but-daily one. So `awareness::assess` now **caps the promise
at AT RISK while the pool is Critical**: `overall = overall.min(AtRisk)` (never PROTECTED
at Critical; AT RISK / UNPROTECTED are unchanged).

**Why this is honest, not synthetic.** The cap is not an alarm bolted onto a healthy
subvolume. It records a real reduction in protection that Urd chose on the host's behalf.
The distinction between *this deliberate cap* and a *genuine failure* is carried by a new
`cadence_adapted` signal (`true` only when the pre-cap status was PROTECTED), which the
voice layer reads to lead with adaptation prose ("tight drive — backing up weekly to spare
it … reads AT RISK by design, not a failure") rather than a failure line. The status **word
stays `AT RISK`** — no new sub-state token (AB3.1), preserving the R4-era trim of promise
proliferation.

**Scope.** The cap fires **only at Critical**. **Tight** lengthens the cadence but does
**not** cap the promise (it is lengthened-but-honest). Roomy is unchanged. This is a
one-notch, bounded, recorded override of R4 — "less protected than declared," surfaced in
plain language, not a synthetic alarm.

## Related

- ADR-104: Graduated retention (Amendment 2026-05-15 — yearly window, `MonthlyCount` semantics)
- ADR-105: Backward-compatibility contracts (Amendment 2026-05-15 — `monthly = 0` semantic shift)
- ADR-111: Config system architecture (governs config structure, versioning, validation)
- Design: `docs/95-ideas/2026-03-26-design-protection-promises.md`
- Design review: `docs/99-reports/2026-03-26-protection-promises-design-review.md`
- Config design review: `docs/98-journals/2026-03-27-config-design-review.md`
- Test strategy review: `docs/99-reports/2026-03-26-test-strategy-review.md`
