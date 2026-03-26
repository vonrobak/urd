# Arch-Adversary Review: Protection Promise ADR Design
**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-03-26
**Scope:** Design proposal -- `docs/95-ideas/2026-03-26-design-protection-promises.md`
**Review type:** Design review (pre-implementation)

---

## 1. Executive Summary

This is a well-structured design that correctly identifies the semantic gap between operational config and user intent. The core approach -- promise levels as derivation inputs to existing pure functions, with `custom` as a first-class migration path -- respects the planner/executor separation and avoids architectural damage. The primary risk is that achievability-as-warning creates a zone where the system promises more than it delivers, and the user learns to ignore the warnings until data loss occurs.

## 2. What Kills You (Catastrophic Failure Proximity)

The catastrophic failure mode for Urd is silent data loss: deleting snapshots that should be kept, or failing to back up when it should.

**Proximity to catastrophe: Moderate.** The design introduces a new indirection layer between user intent and retention policy. The retention algorithm itself is unchanged, which is good -- the danger is in the derivation, not the execution. Specifically:

- A user sets `protection_level = "resilient"`, trusts the label, but the derived retention is `daily=30, weekly=26, monthly=12`. If they previously had `monthly=0` (unlimited), switching to promises *silently reduces retention depth*. The label says "resilient" but the outcome is fewer snapshots kept than before.
- A user overrides `send_enabled = false` on a `protected` subvolume. The system warns but proceeds. The user now has a subvolume labeled "protected" with no external copies. The status output says "PROTECTED" if local snapshots are fresh, even though the promise semantics require external copies.
- Changing `protection_level` on an existing subvolume with hundreds of existing snapshots: the next retention pass uses the new derived policy, which may be tighter than the old explicit policy. Snapshots that survived under the old policy get deleted.

None of these are bugs -- they are design consequences that must be addressed explicitly.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 -- Solid | Core derivation logic is well-defined and pure. Edge cases around level transitions and override interactions need tightening. |
| Security | 3 -- Adequate | Achievability-as-warning creates a gap between stated promise and actual behavior. No mechanism prevents override combinations that void the promise semantics entirely. |
| Architectural Excellence | 4 -- Solid | Respects planner/executor separation, keeps awareness model unchanged, derivation is a pure function. Integration points are well-scoped. |
| Systems Design | 3 -- Adequate | `run_frequency` as manual config is fragile. The interaction between promise labels in status output and actual override-weakened behavior risks user confusion. |

## 4. Design Tensions

### Tension 1: Promise Labels vs. Override Freedom

The design allows overrides that can completely contradict the promise level (`protection_level = "protected"` with `send_enabled = false`). This is defended by ADR-107 (fail open) and "the user may know what they're doing." But it creates a system where the promise label in `urd status` can be actively misleading. The label says "protected"; the behavior is "guarded." The user sees the label, not the warning that fired once during `urd backup` preflight.

**The question:** Is a promise label that doesn't match actual behavior better or worse than no promise label at all?

### Tension 2: Explicit run_frequency vs. Operational Reality

The user declares `run_frequency = "daily"` in config, but then changes their systemd timer to 6h. Or they forget to update the config when they deploy the Sentinel. The derivation produces intervals based on the declared frequency, not the actual frequency. The awareness model catches the mismatch eventually (snapshots become stale), but the root cause is a config lie, not an operational failure.

### Tension 3: Simplicity of Named Levels vs. Adequacy of Fixed Derivations

The appeal of `protected` is that the user doesn't think about numbers. But the fixed derivation (`daily=30, weekly=26, monthly=12`) is a policy claim: "this is what protected means." For a user with 500GB of recordings changing hourly, this retention depth may be fine. For a user with 50GB of documents changing monthly, keeping 30 daily snapshots of identical content is waste. The promise level flattens different use cases into one policy.

### Tension 4: Advisory Achievability vs. Promise Integrity

If a promise is unachievable and the system only warns, then promises are aspirational, not contractual. The user sets `resilient` with one drive, gets a warning, and the system silently operates as `protected`. The status output could show "resilient" next to "AT RISK" without explaining that the promise itself is structurally impossible, not just temporarily degraded.

## 5. Findings

### Critical

*None.* The design does not introduce a direct path to silent data loss that bypasses existing safeguards. The retention algorithm, pin protection, and planner/executor separation remain intact.

### Significant

**S1. Protection level change on existing subvolumes can trigger retroactive snapshot deletion.**

When a user changes from explicit `local_retention = { monthly = 0 }` to `protection_level = "protected"` (which derives `monthly = 12`), the next retention pass will delete all monthly snapshots older than 12 months. This is correct behavior for the new policy, but the user may not realize that adopting a promise level can *reduce* their retention depth. The migration path section says "zero breaking changes" but this scenario shows that switching to promises can cause data loss relative to the user's previous configuration.

*Recommendation:* Add a migration safety check. When `urd init` or `urd verify` detects a subvolume switching from explicit retention to promise-derived retention, compare the two policies and warn if the derived policy is strictly tighter in any dimension. Consider a one-time `--confirm-retention-change` flag or a "retention floor" that preserves existing snapshots through at least one full retention cycle.

**S2. Override combinations can void promise semantics with no persistent signal.**

A user can set `protection_level = "protected"` and `send_enabled = false`. The preflight warning fires during `urd backup`, is logged, and forgotten. The status output shows the promise level "protected" alongside the awareness state. If local snapshots are fresh, the status shows "PROTECTED" -- even though the subvolume has zero external copies and structurally cannot meet the `protected` contract (which requires `min_external_drives = 1`).

*Recommendation:* When an override structurally voids the promise (not merely weakens it), the status display should reflect this. Options: (a) downgrade the displayed promise level to match actual behavior, (b) add a persistent "DEGRADED" qualifier (e.g., "protected*" with footnote), (c) refuse to display the promise level when structural overrides contradict it.

**S3. `run_frequency` as manual config will drift from reality.**

The design argues for explicit over inferred because it is "deterministic" and "validatable at config time." But the cost is that config lies are invisible. A user sets `run_frequency = "daily"`, deploys a 6h timer, and the derived intervals assume 24h runs. The awareness model will eventually flag staleness, but the root cause -- a config/reality mismatch -- is never surfaced. Worse: when the Sentinel launches, the user must remember to update `run_frequency = "sentinel"` in the config file. The design acknowledges this ("the Sentinel design determines the exact mechanism") but defers the answer.

*Recommendation:* At minimum, add a preflight check that compares `run_frequency` against the heartbeat history (when available). If the last N runs show a pattern inconsistent with the declared frequency, warn. This doesn't require inference -- it uses the explicit config as the expectation and heartbeat as the reality check.

### Moderate

**M1. The `custom` derivation path for `derive_policy(Custom, _)` is underspecified.**

The design says `custom` means "the user's config is the policy." But `derive_policy(Custom, run_frequency)` must return *something* -- what? The test plan includes `test_derive_custom_returns_none`, suggesting it returns no derivation. But the `resolve_subvolume` pseudocode calls `derive_policy(ProtectionLevel::Custom, run_frequency)` as the fallback when no `protection_level` is set. If `derive_policy` returns `None` for `custom`, what populates the `base` variable in the resolution logic? The existing `resolved()` method on `SubvolumeConfig` falls back to `DefaultsConfig`. The proposed `resolve_subvolume()` must produce identical results for configs with no `protection_level` field.

*Recommendation:* Specify explicitly: for `custom`, `derive_policy` returns a sentinel value that causes `resolve_subvolume` to fall through to the existing `defaults`-based resolution. Write a property test: for every existing subvolume config (no `protection_level`), the resolved output from the new `resolve_subvolume` must be byte-identical to the output from the existing `SubvolumeConfig::resolved()`.

**M2. Drive mapping does not interact with the planner's existing drive loop.**

The planner (`plan.rs:164-198`) iterates over `config.drives` for every subvolume. The design adds `drives: Option<Vec<String>>` to `SubvolumeConfig` as a filter. But the planner currently receives `ResolvedSubvolume`, which has no `drives` field -- it loops over `config.drives` directly. The design's integration table says plan.rs has "no changes," but filtering sends by the `drives` field requires the planner to know which drives are mapped to which subvolume.

*Recommendation:* Either (a) add a `drives` filter to `ResolvedSubvolume` and update the planner's drive loop to skip unmapped drives, or (b) restructure so drive mapping is resolved at config time and the drives list in `Config` is already filtered per-subvolume. Option (a) is a planner change that the integration table currently denies. Be honest about the scope.

**M3. The outcome targets (24h/48h) are derived from awareness multipliers, not from operational data.**

The design says: "Timer 24h + local threshold 2x = PROTECTED up to 48h = guarded max age." This is mathematically consistent with the awareness model's constants, but it means the "outcome targets" are reverse-engineered from implementation constants, not from user research or operational requirements. The 48h external max age for `protected` means a user's external copy can be nearly two days stale and still show "PROTECTED." For irreplaceable recordings that change hourly, this may not match user expectations of what "protected" means.

*Recommendation:* Document the outcome targets as explicit policy decisions, not as derivations from awareness constants. If the awareness multipliers change (which they might, as they are module-level constants, not config), the promise semantics would silently shift. Consider making the outcome targets the primary definition, and derive the awareness multipliers from them (or at least validate consistency between the two).

**M4. Retention is identical for `protected` and `resilient`, which weakens the semantic distinction.**

Both levels derive `hourly=24, daily=30, weekly=26, monthly=12` for local retention and `daily=30, weekly=26, monthly=0` for external. The only difference is `min_external_drives` (1 vs. 2). This means `resilient` is strictly "protected + one more drive." The name "resilient" implies something stronger -- more history, faster recovery, deeper safety net. A user choosing between `protected` and `resilient` might expect the latter to keep snapshots longer, not just on more drives.

*Recommendation:* This is a minor semantic issue now but will become a support burden. Either (a) accept and document that resilient = protected + redundancy (the design's position), or (b) give resilient deeper retention (e.g., `monthly=24` or `monthly=0`). The design can defer this, but should explicitly acknowledge the user-expectation risk.

### Minor

**N1. The `short_name` field on `SubvolumeConfig` is absent from the design's config schema examples.**

The proposed config examples show `name` and `source` but omit `short_name`, which is a required field in the current `SubvolumeConfig`. This is likely just an omission in the design document, but if the design intends to make `short_name` optional (derived from `name`), that should be stated.

**N2. Display convention for promise levels in status output is unresolved.**

Open Question 4 asks whether to show "resilient" or "RESILIENT." The design leans toward lowercase but flags noise concerns. This is minor but should be decided before implementation to avoid a format change later. Recommendation: lowercase, since the PROTECTED/AT RISK/UNPROTECTED status already uses uppercase and two uppercase columns would indeed be noisy.

### Commendation

**C1. Promise derivation as a pure function at config resolution time is the right architectural choice.**

By placing derivation between config parsing and planner input, the design avoids touching the planner, awareness model, or executor. The planner still receives `ResolvedSubvolume` with concrete intervals. This means the entire promise system can be tested in isolation with unit tests on the derivation function. This is textbook adherence to ADR-100 and ADR-108.

**C2. Dropping `archival` is the right call.**

The rationale is sound: archival is about retention depth, not protection freshness. Conflating them creates ambiguous status semantics. Deferring to a separate `retention_profile` concept keeps the promise model clean. The design correctly identifies that a subvolume can be `protected` (current copies exist) AND have deep retention (keep monthly forever) -- these are orthogonal concerns.

**C3. The design addresses every item in the Phase 5 gate checklist.**

Each gate requirement from `status.md` lines 221-229 has a corresponding section in the design. The timer frequency issue from the 2026-03-26 operational incident is directly addressed. The drive topology constraint (subvol3-opptak vs. 2TB-backup) is handled by the achievability validation. This is thorough preparation.

## 6. The Simplicity Question

The design adds three new config fields (`protection_level`, `drives`, `run_frequency`), one new enum (`ProtectionLevel`), one new struct (`DerivedPolicy`), and one new pure function (`derive_policy`). It modifies six existing files and adds zero new modules. This is restrained.

The complexity risk is not in the implementation but in the *conceptual model*. Users now have two mental models for configuring a subvolume: promise-based (set a level, trust the derivation) and operation-based (set intervals and retention explicitly). The `custom` level bridges them, but a user reading the config file must understand both systems to reason about behavior. The override mechanism adds a third mode: promise-with-tweaks.

**Verdict:** The design earns its complexity. The promise abstraction directly serves the project's north star ("does it reduce the attention the user needs to spend on backups?"). The risk is manageable if the override interaction is tightened per findings S1 and S2.

## 7. For the Dev Team (Prioritized Action Items)

1. **Define the protection-level transition behavior.** What happens to existing snapshots when `protection_level` changes? Write a test that switches from `monthly=0` to `protected` (which derives `monthly=12`) and verify no unexpected deletions occur on the first retention pass. Consider a grace period or confirmation. (Addresses S1)

2. **Decide: can overrides void a promise, or only weaken it?** If `send_enabled = false` on a `protected` subvolume is allowed, the status output must not display "protected" without qualification. Define which overrides are "weakening" (longer intervals) vs. "voiding" (disabling sends on a level that requires them). Handle them differently. (Addresses S2)

3. **Add heartbeat-vs-run_frequency consistency check to preflight.** When heartbeat history exists, compare actual run cadence against declared `run_frequency`. Warn on mismatch. This is a natural extension of the existing preflight module. (Addresses S3)

4. **Specify `derive_policy(Custom, _)` behavior precisely.** Write the property test: existing configs without `protection_level` must produce identical `ResolvedSubvolume` output through both the old and new resolution paths. (Addresses M1)

5. **Acknowledge the planner drive-loop change.** The `drives` field on subvolumes requires the planner to filter its drive iteration. Update the integration table to reflect this. It is a small change but it is a planner change. (Addresses M2)

6. **Document outcome targets as primary policy, not as awareness-multiplier derivations.** The numbers should be defensible on their own terms. If someone changes the awareness multipliers, promise semantics should not silently shift. (Addresses M3)

## 8. Open Questions

1. **What is the retention behavior during a protection-level transition?** If a subvolume moves from explicit `monthly=0` to `protected` (deriving `monthly=12`), should there be a grace period before the tighter retention takes effect? Or should the user be required to confirm the change via `urd init --confirm-retention-change`?

2. **Should `urd status` show the effective promise level or the configured promise level?** If overrides void the promise, showing the configured level is misleading. Showing the effective level requires computing what the overrides actually amount to. This is non-trivial but important for trust.

3. **How does `run_frequency` interact with mixed-mode operation?** A user might run `urd backup` manually between timer fires, or have both a timer and occasional manual runs. The derivation assumes a single cadence. Is this adequate, or does it need a "minimum guaranteed frequency" semantic?

4. **Is `monthly=12` the right floor for `protected`?** The current example config uses `monthly=12` in defaults and `monthly=0` in external retention. A user switching from defaults to `protected` gets the same local retention but loses unlimited external monthly retention (external derives `monthly=0`, which is actually unlimited -- but this inconsistency between the table in the design and the retention derivation code block needs to be reconciled: the table says "monthly=12" in the retention floor column, but the derivation block says `monthly = 0` for external).

5. **Does the `drives` field validation happen at config load time or at plan time?** If validated at load time, a typo in a drive name prevents Urd from starting entirely. If validated at plan time, the error surfaces late. The design says "all listed drives must exist in `[[drives]]`. Error if not" -- but when?

---

*Review conducted as architectural adversary. Findings reflect design-phase analysis; implementation may resolve some concerns through testing and iteration.*
