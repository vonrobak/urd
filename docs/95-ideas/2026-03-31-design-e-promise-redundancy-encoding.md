# Design: Promise Redundancy Encoding — Resilient Requires Offsite

> **TL;DR:** Make the existing promise taxonomy explicitly encode geographic redundancy.
> Resilient requires at least one drive with `role = "offsite"`; if the offsite copy goes
> stale, the promise degrades to AT RISK. This makes 3-2-1 a first-class concept without
> naming it, using machinery that already exists in preflight and awareness.

**Date:** 2026-03-31
**Status:** reviewed
**Depends on:** awareness.rs (complete), preflight.rs (complete), DriveRole enum (complete)
**Inputs:**
- [Brainstorm: transient workflow and redundancy guidance](2026-03-30-brainstorm-transient-workflow-and-redundancy-guidance.md) (idea E, scored 9-10)
- ADR-110 (protection promises)
- ADR-108 (pure-function module pattern)

---

## Review findings incorporated

Reviewed 2026-03-31 by architectural adversary. Full report:
`docs/99-reports/2026-03-31-design-e-review.md`

| Finding | Severity | Resolution |
|---------|----------|------------|
| C1: Threading protection_level into awareness breaks ADR-110 Invariant 6 | Critical | Adopted option (b): offsite freshness computed as post-processing step outside awareness. New `overlay_offsite_freshness()` function takes `Vec<SubvolAssessment>` + config, keeps awareness protection-level-blind. |
| I1: Double-counting risk with existing external assessment | Important | Added explicit justification: `best_external_status` uses `max()` across drives, so a healthy primary masks a stale offsite. The overlay exists precisely to catch this masking. |
| I2: 30-day threshold assumes monthly rotation | Important | Stated explicitly: resilient means monthly-or-better offsite rotation. Quarterly rotators must use custom. |
| I3: DriveAssessment.role plumbing risk | Important | Noted: role must come from config lookup, not a default value. |
| I4: Advisory overlap with 7-day "consider cycling" | Important | Resolved: the 7-day advisory is replaced by the structured offsite freshness system for resilient subvolumes. Protected subvolumes keep the existing advisory unchanged. |
| M1: ADR gate too conservative | Minor | Accepted: will add ADR-110 addendum documenting offsite freshness contract and thresholds. |
| M2: Boundary condition on num_days() | Minor | Accepted: threshold uses integer days via `num_days()`, not seconds. Test updated accordingly. |

---

## Problem

Urd's promise levels implicitly map to redundancy tiers, but the mapping has a gap:

| Level | Current enforcement | What the user expects |
|-------|--------------------|-----------------------|
| guarded | No external sends | Local snapshots only |
| protected | >= 1 drive, sends enabled | At least one external copy |
| resilient | >= 2 drives, sends enabled | Survives a site disaster |

A user can set `protection_level = "resilient"` with two drives on the same shelf. The
drive count check passes, but the resilience intent — geographic redundancy — is not
enforced. The word "resilient" promises something the system does not verify.

Meanwhile, awareness already generates an advisory when an unmounted drive's last send
exceeds 7 days ("offsite drive X last sent Y days ago — consider cycling"). But this
advisory is cosmetic — it does not affect the promise status. A resilient subvolume
whose offsite drive has not been seen in 60 days still shows PROTECTED.

The gap: **promise levels describe redundancy in spirit but not in structure.** The
DriveRole enum (`Primary`, `Offsite`, `Test`) already exists but is unused by both
preflight and awareness.

## North star

When a user sets `protection_level = "resilient"`, Urd should:

1. Refuse to accept the config if no drive has `role = "offsite"` (preflight).
2. Track whether the offsite copy is fresh enough to survive a site disaster (awareness + post-processing overlay).
3. Degrade the promise visibly when the offsite copy goes stale (voice).

The user never needs to learn "3-2-1." They set "resilient," and Urd guides them toward
the behavior that makes resilience real.

---

## Proposed Design

### Redundancy mapping

The promise levels encode redundancy expectations through drive roles:

| Level | Drive requirement | Offsite requirement | Freshness contract |
|-------|------------------|--------------------|--------------------|
| guarded | None | None | Local interval only |
| protected | >= 1 drive (any role) | None | External send interval |
| resilient | >= 2 drives | >= 1 with `role = "offsite"` | External send interval + offsite freshness |

Protected does NOT require offsite. It means "I have external copies." The distinction
between protected and resilient is precisely the geographic dimension.

### Offsite freshness threshold

For resilient subvolumes, the offsite freshness overlay tracks the newest successful send
to any offsite-role drive. If that age exceeds 30 days, the promise degrades to AT RISK.
If it exceeds 90 days, it degrades to UNPROTECTED.

**This threshold defines what "resilient" means operationally: monthly-or-better offsite
rotation.** Users with longer rotation cycles (e.g., quarterly) must use
`protection_level = "custom"` and manage their own freshness expectations. This is a
conscious design choice — the word "resilient" claims geographic redundancy, and a
monthly cadence is the minimum frequency at which that claim remains credible.

These thresholds are **not user-configurable**. They are derived from what "resilient"
means: geographic redundancy implies the offsite copy must be periodically refreshed.
Thirty days is generous enough for monthly drive rotation; ninety days means the offsite
copy is so stale that "resilient" is a lie. This aligns with ADR-110's principle that
named levels are opaque.

**Why a separate overlay is needed:** The existing per-drive freshness assessment feeds
into `best_external_status`, which uses `max()` across all drives. This means a healthy
primary drive masks a stale offsite drive. A resilient subvolume with a primary drive
sent 2 hours ago and an offsite drive last sent 35 days ago would show PROTECTED based
on external assessment alone — the primary's freshness wins. The offsite freshness
overlay exists precisely to catch this masking. Without it, the "resilient" label provides
no geographic guarantee.

The existing per-drive freshness assessment (based on `send_interval` multipliers)
continues to operate independently. The offsite freshness check is an additional
constraint that applies only to resilient subvolumes. The overall status is:

```
overall = min(local_status, best_external_status, offsite_freshness_status)
```

where `offsite_freshness_status` is only computed for resilient subvolumes.

### Module changes

#### preflight.rs — new check: `resilient-without-offsite`

Add a check in `check_promise_achievability()` after the existing drive-count check:

```rust
// Resilient requires at least one offsite drive
if level == ProtectionLevel::Resilient {
    let has_offsite = config.drives.iter().any(|d| d.role == DriveRole::Offsite);
    if !has_offsite {
        checks.push(PreflightCheck {
            name: "resilient-without-offsite",
            message: format!(
                "{}: resilient promise requires at least one drive with role = \"offsite\"",
                subvol.name,
            ),
        });
    }
}
```

This fires at config validation time. The user sees the issue before any backup runs.
Preflight is advisory (ADR-109), so it does not block backups — it surfaces the gap.

When the subvolume has explicit drive assignments (`drives = ["WD-18TB"]`), the check
scopes to those drives only, not the global drive list.

#### awareness.rs — unchanged (ADR-110 Invariant 6 preserved)

**Awareness remains protection-level-blind.** Per review finding C1, threading
`protection_level` into the awareness loop would break ADR-110 Invariant 6, which states
that the awareness model is unchanged by promise levels. Promise levels affect what
intervals are configured, not how evaluation works.

Instead, `assess()` continues to produce `Vec<SubvolAssessment>` based purely on
operational reality: configured intervals, send history, and drive states. It does not
know or care whether a subvolume is resilient, protected, or guarded.

#### awareness.rs — DriveAssessment gains `role`

```rust
pub struct DriveAssessment {
    pub drive_label: String,
    pub status: PromiseStatus,
    pub mounted: bool,
    pub snapshot_count: Option<usize>,
    pub last_send_age: Option<Duration>,
    pub configured_interval: Interval,
    pub role: DriveRole,  // NEW
}
```

The role is populated from `DriveConfig::role` when building drive assessments. This is
a straightforward plumbing change — the config already carries the role. **The role must
come from the config lookup, not from a default value.** A missed plumbing path that
falls back to `DriveRole::Primary` would silently disable offsite freshness tracking
for drives that are configured as offsite.

#### New: `overlay_offsite_freshness()` — post-processing step

The offsite freshness computation lives in a new pure function alongside `assess()` in
awareness.rs (or a companion module). It runs *after* `assess()` returns, taking the
assessment results plus config as input:

```rust
/// Post-processing overlay: degrade resilient subvolumes with stale offsite copies.
/// This is NOT part of assess() — awareness remains protection-level-blind per ADR-110.
pub fn overlay_offsite_freshness(
    assessments: &mut [SubvolAssessment],
    config: &ResolvedConfig,
) {
    for assessment in assessments.iter_mut() {
        let protection_level = config.protection_level_for(&assessment.name);
        if protection_level != Some(ProtectionLevel::Resilient) {
            continue;
        }

        let offsite_freshness = compute_offsite_freshness(&assessment.drive_assessments);
        if offsite_freshness < assessment.status {
            assessment.status = offsite_freshness;
            assessment.advisories.push(format!(
                "offsite copy stale — resilient promise degraded",
            ));
        }
    }
}
```

Where `compute_offsite_freshness` finds the best (newest) send age among offsite-role
drives and maps it to a promise status:

```rust
fn compute_offsite_freshness(drives: &[DriveAssessment]) -> PromiseStatus {
    let best_offsite_age = drives
        .iter()
        .filter(|d| d.role == DriveRole::Offsite)
        .filter_map(|d| d.last_send_age)
        .min(); // shortest age = freshest

    match best_offsite_age {
        None => PromiseStatus::Unprotected, // no offsite send ever
        Some(age) => {
            let days = age.num_days();
            if days <= 30 {
                PromiseStatus::Protected
            } else if days <= 90 {
                PromiseStatus::AtRisk
            } else {
                PromiseStatus::Unprotected
            }
        }
    }
}
```

This is a pure function (ADR-108). It does not modify DriveAssessment status values —
it produces a separate constraint that feeds into the overall minimum. The threshold
uses integer days via `num_days()` (which truncates), so 30 days and 23 hours is still
PROTECTED. The actual boundary is at 31 full days.

The call sites (status command, sentinel) call `assess()` then
`overlay_offsite_freshness()`:

```rust
let mut assessments = assess(&config, &fs_state, &state_db, now);
overlay_offsite_freshness(&mut assessments, &config);
```

This keeps awareness itself protection-level-blind while still producing a final
assessment that reflects the resilient contract.

#### awareness.rs — 7-day advisory replaced for resilient subvolumes

The existing advisory ("offsite drive X last sent Y days ago — consider cycling") is
**replaced** by the structured offsite freshness degradation for resilient subvolumes.
For protected subvolumes, the existing advisory continues unchanged — it remains
informational guidance without promise impact.

The advisory text moves to voice.rs for rendering; the advisory list in the assessment
carries the raw fact.

#### voice.rs — surface offsite degradation

When rendering status for a resilient subvolume whose promise is degraded due to offsite
staleness, voice.rs adds a line explaining why:

```
htpc-home         AT RISK
  local: 24 snapshots, newest 42 min ago
  WD-18TB: 847 snapshots, last sent 18 hours ago
  WD-18TB1 (offsite): last sent 34 days ago — resilient promise degraded
```

The offsite drive line carries the degradation context. The user sees exactly what to do:
connect the offsite drive and run a backup.

#### types.rs — DerivedPolicy (no change needed)

`derive_policy()` already sets `min_external_drives = 2` for resilient. The offsite
requirement is orthogonal to drive count — it is a role constraint, not a quantity
constraint. Encoding it in `DerivedPolicy` would conflate two concerns. Preflight
checks the role requirement directly from config.

---

## Invariants

1. **Preflight is advisory.** The resilient-without-offsite check warns but does not
   block backups (ADR-109). A user who ignores it gets backups to their local drives;
   they just see degraded promise status.

2. **Awareness is pure and protection-level-blind.** The `assess()` function takes
   operational inputs and returns operational assessments. It does not know about
   protection levels (ADR-110 Invariant 6, ADR-108). The offsite freshness overlay
   is a separate post-processing step.

3. **Offsite freshness is additive.** It does not replace the existing per-drive
   freshness assessment. Both constraints apply independently. The overall status is
   the minimum of all constraints.

4. **Protected is not affected.** Only resilient subvolumes have the offsite freshness
   constraint. Protected subvolumes with offsite drives keep the existing 7-day
   "consider cycling" advisory but receive no promise degradation.

5. **Custom is not affected.** Custom subvolumes bypass all promise-level logic, as today.

6. **Thresholds are fixed.** 30-day AT RISK and 90-day UNPROTECTED are part of the
   resilient level's semantics, not user configuration. This aligns with ADR-110's
   principle that named levels are opaque.

---

## Integration Points

| Module | Change | Risk |
|--------|--------|------|
| `preflight.rs` | New `resilient-without-offsite` check | Low — additive, advisory only |
| `awareness.rs` | `DriveAssessment.role` field (plumbing only, no logic change) | Low — data-only addition |
| `awareness.rs` (or companion) | New `overlay_offsite_freshness()` post-processing function | Medium — new logic, but isolated and testable |
| `commands/status.rs` | Call `overlay_offsite_freshness()` after `assess()` | Low — one-line addition |
| `sentinel.rs` / `sentinel_runner.rs` | Call `overlay_offsite_freshness()` after `assess()` | Low — one-line addition |
| `voice.rs` | Offsite degradation line in status display | Low — presentation only |
| `types.rs` | None | None |
| `config.rs` | None | None |
| `notify.rs` | Offsite degradation may trigger notifications via existing promise-change logic | Low — automatic if assessment is correct |

The post-processing approach distributes the call to `overlay_offsite_freshness()` across
two call sites (status command and sentinel), but each is a single line. The tradeoff is
worth it to preserve awareness's protection-level-blind contract.

---

## ADR Gate

**Does extending promise-level semantics require an ADR-110 update?**

ADR-110 defines the promise maturity model and the guarded/protected/resilient taxonomy.
This design makes the resilient level's geographic requirement explicit rather than
implicit. It does not add new levels or change what the levels mean — it enforces what
"resilient" already claims to mean.

**Resolution: ADR-110 addendum required.** Per review finding M1, ADR-110 explicitly
calls the taxonomy "provisional" and says the levels need rework. This design defines
resilient more precisely than ADR-110 ever did. An addendum documenting the offsite
freshness contract (30/90-day thresholds) and confirming that Invariant 6 is preserved
(offsite freshness is post-processing, not inside awareness) ensures future taxonomy
rework knows these thresholds exist.

---

## Test Strategy

### Preflight tests (~4 tests)

1. Resilient subvolume with no offsite drive produces `resilient-without-offsite` warning.
2. Resilient subvolume with one offsite drive passes cleanly.
3. Protected subvolume with no offsite drive does NOT produce the warning.
4. Resilient subvolume with explicit drive list scoped to offsite drive passes.

### Overlay tests (~6-8 tests)

5. Resilient subvolume with fresh offsite send (< 30 days) stays PROTECTED.
6. Resilient subvolume with stale offsite send (31 days) degrades to AT RISK.
7. Resilient subvolume with very stale offsite send (91 days) degrades to UNPROTECTED.
8. Resilient subvolume with no offsite send history is UNPROTECTED.
9. Protected subvolume with stale offsite send does NOT degrade (overlay
   skips non-resilient subvolumes).
10. Resilient subvolume where primary drive is AT RISK but offsite is fresh: overall is
    AT RISK (independent constraints, minimum wins).
11. Resilient subvolume with two offsite drives: best offsite freshness wins.
12. Boundary test: exactly 30 `num_days()` is PROTECTED, 31 `num_days()` is AT RISK.

### Voice tests (~2 tests)

13. Status rendering includes offsite degradation line when applicable.
14. Status rendering omits offsite degradation line for protected subvolumes.

Total: ~14 tests. Consistent with the ~60-80 line estimate for implementation.

---

## Rejected Alternatives

### User-configurable offsite threshold

Rejected because it undermines the opaque-level principle (ADR-110). If users can set
`offsite_max_age = "7d"` on a resilient level, the level is no longer opaque — it
becomes a partially-customized template. Users who need custom thresholds should use
`protection_level = "custom"` and manage their own expectations.

### Encode offsite requirement in DerivedPolicy

Rejected because `DerivedPolicy` is about operational parameters (intervals, retention,
drive counts). Role requirements are structural constraints, not operational parameters.
Mixing them creates a type that serves two purposes.

### New "geographic" or "offsite" protection level

Rejected because the existing taxonomy already encodes this. Resilient *means* geographic
redundancy. Adding a fourth level fragments the model without adding information.

### Hard gate in preflight (refuse to start)

Rejected per ADR-109. Structural config errors refuse to start; achievability gaps are
advisory. Missing offsite is an achievability gap — the config is structurally valid,
just aspirational.

### Degrade from existing advisory (7-day threshold)

The current advisory fires at 7 days, which is appropriate for "consider cycling" but
too aggressive for promise degradation. A user who rotates drives monthly would see
constant AT RISK status. The 30-day threshold matches common rotation patterns while
still catching genuine neglect.

### Thread protection_level into awareness (original design)

The original design threaded `protection_level` into the `assess()` loop to compute
offsite freshness inline. Review finding C1 identified this as a violation of ADR-110
Invariant 6 ("the awareness model is unchanged; promise levels affect what intervals are
configured, not how evaluation works"). The post-processing approach preserves the
invariant while achieving the same outcome.

---

## Open Questions

1. **Should the offsite freshness advisory in awareness carry structured data?**
   Currently advisories are `Vec<String>`. A structured variant (enum with fields)
   would let voice.rs render more precisely. This is a broader advisory-system question
   that affects other features too.

2. **Should `urd plan` surface the offsite gap?** Currently plan output focuses on
   operations. If the offsite drive is not mounted, plan cannot include a send to it.
   Should plan say "offsite drive WD-18TB1 not mounted — resilient promise at risk"?
   This is a UX question, not an architectural one.

3. **What about transient + resilient?** Preflight already blocks this combination
   (transient is incompatible with named protection levels other than custom). No
   interaction to design.
