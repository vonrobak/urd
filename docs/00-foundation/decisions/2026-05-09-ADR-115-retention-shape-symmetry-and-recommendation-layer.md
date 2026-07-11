# ADR-115: Retention Shape Symmetry and the Recommendation Layer

> **TL;DR:** The cost of retaining a snapshot is **symmetric across source and
> destination pools** — the same arithmetic over the same drift signal projects
> pinned CoW delta on `/mnt/btrfs-pool` and projected delta on a destination
> drive. X1 of Phase D-2 (UPI 041) ships an **advisory** recommendation layer
> over this symmetric cost model: `urd doctor --thorough` surfaces a
> per-subvolume shape that fits the observed churn, and the user applies it
> by editing `urd.toml`. The engine never mutates `derive_policy()` or config.
> X1 models **data delta only** (inter-slot formula); metadata cost is
> deferred to UPI 043 (file-count telemetry) + UPI 044 (metadata cost model).
> Auto-tune — Urd applying recommendations on the user's behalf — is the
> documented long-term north star, scoped to a future arc gated on the
> evidence this layer generates.

**Date:** 2026-05-09
**Status:** Accepted
**Depends on:** ADR-108 (pure function modules), ADR-110 (protection promises),
ADR-113 (do-no-harm invariant), ADR-114 (structured event log)
**Complemented by:** UPI 030 (drift telemetry — the signal this layer consumes)
**Amends:** ADR-110 (see [amendment](2026-03-26-ADR-110-protection-promises.md#amendment-2026-05-09-recommendation-layer-as-graduation-evidence-path-adr-115))

## Context

Six weeks of autonomous operation produced the first evidence-based view of
per-subvolume cost
(report: `docs/99-reports/2026-05-09-retention-tuning-from-real-world-data.md`).
Two patterns emerged:

1. **Named-level retention shapes are decoupled from real cost.** The same
   shape applied to a 7 MB/day subvolume and a 2,700 MB/day subvolume costs
   ~400× more on the latter. On this host, `containers` alone pinned ~165 GB
   of CoW delta on `/mnt/btrfs-pool` — 94% of all source-side cost — purely
   because it inherited the same shape its quieter siblings use. A user has
   no way to discover this without manually digging through `drift_samples`
   and reasoning about retention shapes themselves.

2. **The cost is symmetric across pools.** Local snapshot pinning and
   external send retention share the same cost-projection math: a retained
   slot pins its inter-slot wire bytes, regardless of which pool it lives
   on. Yet `derive_policy()` happens to give the *more* generous shape to
   local retention — exactly the wrong asymmetry for a constrained source
   pool like NVMe. ADR-113 (Do-No-Harm) recognized this defensively for
   the source pool; this ADR closes the loop with **honest advice across
   both pools**.

The user has only two surfaces today that touch retention shape:
`derive_policy()` (named-level shapes, opaque/provisional per ADR-110) and
the user's `urd.toml` (Custom shapes, hand-edited). Neither tells the user
*what shape fits their data* or *what the current shape costs*.

UPI 030's drift telemetry made the underlying signal observable. This ADR
turns the signal into actionable advice without taking control of retention
out of the user's hands.

## Decision

### The symmetric data-cost claim

The cost of retaining a graduated snapshot shape `S = {hourly, daily, weekly, monthly}`
under an observed mean churn rate `r` (bytes/second) is:

```
data_bytes(S, r) = r × Σ_W W_total_seconds(S)
```

where `W_total_seconds(S)` is the **outer-edge span** of window W under shape `S`:

| Window  | `W_total_seconds(S)`             |
|---------|----------------------------------|
| hourly  | `S.hourly × 3600`                |
| daily   | `S.daily × 86400`                |
| weekly  | `S.weekly × 7 × 86400`           |
| monthly | `S.monthly × 30 × 86400`         |

The formula is **symmetric**: the same function projects pinned CoW delta on
the source pool and projected delta on a destination drive. Local and external
retention serve different recovery objectives (local = fast restore; external
= survives drive loss), but their **costs are governed by the same arithmetic
over the same drift signal**.

#### Why inter-slot, not age-midpoint

A retained slot pins **inter-slot delta** — the wire bytes between it and the
previous retained slot in the same window. The naive alternative,
`mean × W_avg_age × slot_count`, double-charges old slots: a 12-month window
contributes `mean × Σ(1..12) months` ≈ `mean × 78 months` instead of
`mean × 12 months`, a ~6× overcount on the monthly window alone. Inter-slot
matches the physical cost of pinned CoW delta on BTRFS.

The inter-slot formula has an elegant collapse: total data cost is
`mean × total_chain_span`. **Slot density inside a window does not affect
data cost** — only metadata cost (deferred to UPI 044). Window outer edges
drive data cost; slot density is a separate axis.

#### What this formula does *not* model

- **Variance / burstiness.** `r` is the time-weighted mean over a 7-day
  rolling window. For subvolumes whose churn is bimodal (long dormancy
  punctuated by active periods), `r` understates active-period cadence
  needs. The recommendation layer flags this case (`BurstyPattern` note)
  rather than committing to a stateful bimodal model.
- **Metadata cost.** Per-snapshot extent-tree overhead is real (the
  2026-05-09 report documented 98.98% metadata DUP on WD-18TB from 156
  snapshots' overhead) and roughly proportional to file count — not data
  delta. X1 does not model it. UPI 043 (file-count telemetry) + UPI 044
  (metadata cost model) introduce it. Until UPI 044 ships, X1 may
  over-recommend retention for cold-with-many-files subvolumes; no such
  subvolume exists on this host's evidence.
- **Chain integration.** The formula uses pre-window-stack arithmetic,
  not integration over the actual chain history. UPI 044 / future
  refinements may revisit if evidence shows the approximation is
  materially limiting.

The arc's reading-list report frames the data-delta math as accurate to
±30%, and the 350× spread between this host's hottest and coldest
subvolume dwarfs the modeling error.

### The recommendation-layer pattern

The recommendation layer is a **pure module** (`src/policy.rs`, ADR-108)
that:

1. **Reads** drift telemetry (`drift::ChurnEstimate`) and the user's
   resolved retention shape (`ResolvedGraduatedRetention`).
2. **Computes** a recommended shape under the symmetric cost model.
3. **Returns** the recommendation as data — no I/O, no persistence, no
   mutation of `derive_policy()`, no mutation of config.
4. **Surfaces** through `urd doctor --thorough` only. Heartbeat,
   Prometheus, and `urd status` are unchanged in X1.

The friction floor for users is **"ignore the suggestion."** Anyone who
disagrees with a recommendation does nothing and Urd continues with the
user's existing config. Anyone who agrees edits `urd.toml`.

#### Advisory only

- `derive_policy()` is unchanged.
- Config is unchanged.
- Snapshot creation, retention, and send behavior are unchanged.
- No event is recorded in the ADR-114 event log when a recommendation is
  computed (recommendations are not Urd decisions; routine recommendation
  emissions are not "non-trivial decisions worth recording"). When future
  auto-tune work *applies* a recommendation, that **is** a decision and
  becomes an event — but applying is out of scope for this ADR and Phase D-2.

#### Independence from UPI 031

The recommendation layer and the `storage_critical` bundle (UPI 031,
ADR-113 Layer 1) are **independent surfaces over the same drift
telemetry**. They serve different purposes — recommendations help the
user choose a better-fitting shape; the storage_critical bundle defends
against pressure incidents in real time — and they should not contradict
each other. Future coordination is its own UPI if contradiction emerges
in evidence.

### Internal model parameters

The recommendation algorithm has the following tunables, **fixed in code,
documented here, and not user-configurable**. Per arc decision F (no
user-tunable thresholds): friction floor is "ignore the suggestion," not
"edit the threshold." Tunable thresholds become backward-compat contracts
(ADR-105); fixed thresholds keep evolution cheap.

| Parameter             | Local        | External     |
|-----------------------|--------------|--------------|
| `data_budget_bytes`   | 50 GB        | 100 GB       |
| `slot_share` (h/d/w/m)| 0.05/0.30/0.40/0.25 | 0/0.30/0.40/0.30 |
| `clamp_min` (h/d/w/m) | 0/3/0/0      | 0/3/0/0      |
| `clamp_max` (h/d/w/m) | 24/60/52/24  | 0/60/52/24   |

#### Calibration rationale

`data_budget_bytes` is a per-pool target for total pinned data delta the
engine will recommend retaining. It directly translates to "how many
weeks/months back the chain reaches" via the symmetric formula:
`Σ_W W_total_seconds = data_budget_bytes / mean_bytes_per_second`.

Validated against the 2026-05-09 report's two host extremes:

- `containers` at 2,700 MB/day (~31,250 B/s mean): `50 GB / 31,250 B/s ≈
  18.5 days total chain` → engine recommends 24 h + ~17 d (clamps tight).
  Matches the report's intuition for a hot subvolume.
- `subvol1-docs` at 7 MB/day (~81 B/s): `50 GB / 81 B/s ≈ 19.6 years` →
  engine recommends MAX clamps everywhere (24 h + 60 d + 52 w + 24 m).
  Matches the report's intuition that docs is essentially free to retain.

`slot_share` distributes the data budget across windows (which determines
each window's outer edge under the inter-slot formula). The local
distribution emphasizes recent dailies and a long weekly tail; the
external distribution drops hourlies entirely (external retention is
about durability, not point-in-time recovery within the day).

`clamp_min` ensures degenerate inputs (e.g., extreme churn) still produce
a usable shape with at least 3 daily slots.

`clamp_max` keeps shapes within sane bounds at the cold end. 60 dailies +
52 weeks + 24 months caps the chain at ~5 years, which is enough for
any plausible recovery scenario and prevents runaway shapes from numeric
artifacts.

#### Anticipated revision

These constants are committed in this ADR but explicitly **soft**:
arc done-ness criterion 6 (post-X4 evidence checkpoint, ~30 days of
operation) is the anticipated revision point. Revising the constants
amends this ADR; revision is not a breaking change because the
recommendation surface is advisory.

### What this ADR does NOT decide

These are deliberately deferred to the arc's later UPIs or to future
ADRs:

- **Metadata cost model.** UPI 043 (file-count telemetry) + UPI 044
  (metadata cost model). When 044 ships, this ADR may amend with a
  metadata symmetry claim and a `metadata_budget_bytes` constant set.
- **Headroom-aware adjustments.** UPI X4 of the arc. The recommendation
  engine grows optional headroom inputs already; UPI X4 wires them.
- **Tier/classifier exposure** as a heartbeat or Prometheus field. Per
  arc decision F: internal-only. The recommendation surface is human-
  readable only.
- **Dedicated `urd recommend` command.** `urd doctor --thorough` is the
  right home — recommendations belong with health diagnostics.
- **Auto-apply** (`urd config apply-suggestion` or similar). The
  long-term north star, scoped to a future arc gated on evidence.
- **Drift signal from local snapshot deltas.** Today, send-disabled
  subvolumes have no churn signal. A future UPI may extend `drift.rs`
  to measure CoW deltas between consecutive local snapshots; out of
  scope for the Phase D-2 arc.

## Consequences

### Positive

- **Honest retention.** The user can see what their current shape costs
  and what shape fits their data. The decoupling between named-level
  shapes and real cost (the report's central finding) becomes
  user-visible.
- **Symmetric cost is now load-bearing.** ADR-113's Do-No-Harm invariant
  generalizes from "defend the source pool reactively" to "the same
  cost model governs both pools." This is the conceptual foundation for
  future arc work.
- **Auto-tune becomes designable.** The recommendation layer is the
  evidence-generating step that makes auto-tune feasible on safe,
  validated foundations rather than guessed ones. Phase D-3 (if it
  happens) builds on this evidence.
- **Named levels accumulate graduation evidence.** ADR-110's maturity
  model required operational track record for graduation. The
  recommendation layer surfaces that evidence as a side effect of normal
  operation. (See [ADR-110 amendment](2026-03-26-ADR-110-protection-promises.md#amendment-2026-05-09-recommendation-layer-as-graduation-evidence-path-adr-115).)
- **No new on-disk contracts.** Heartbeat schema, Prometheus metric set,
  and pin-file format are unchanged. ADR-105 risk is zero in X1.

### Negative

- **Metadata cost is unmodeled in X1.** Cold-with-many-files subvolumes
  may be over-recommended retention until UPI 044. Risk accepted; no
  such subvolume exists on this host's evidence.
- **Send-disabled subvolumes get no recommendation.** They fall through
  the existing "no churn observed yet" path. A future UPI for
  drift-from-local-snapshots is the proper fix.
- **Constants are calibrated on N=1 host's evidence.** The post-X4
  evidence checkpoint may revise. Until then, recommendations may be
  miscalibrated for hosts with very different cost distributions.
- **The user must hand-edit `urd.toml` to apply.** No automation. For
  named-level subvolumes, applying a recommendation requires switching
  to `custom`, which voice surfaces with a per-row hint but is still
  manual.

### Neutral

- **Does not replace `derive_policy()`.** Named levels remain
  opaque/provisional per ADR-110. The recommendation engine reads them
  via `derive_policy()` and the user's resolved config; it does not
  write to them.
- **Does not replace `storage_critical` (UPI 031).** Independent surface
  over the same drift telemetry; serves a different purpose.

## Invariants

1. **The cost-projection math is symmetric across source and destination
   pools.** The same arithmetic over the same drift signal projects
   pinned delta on either pool. (R1)
2. **Recommendations are advisory.** The recommendation layer never
   mutates `derive_policy()`, config, snapshot creation, retention, or
   send behavior. (Recommendation-layer pattern)
3. **Internal model parameters are fixed in code.** No
   `[recommendations]` config section, no per-host threshold overrides.
   Friction floor is "ignore the suggestion." (Arc decision F)
4. **Recommendation outputs are shapes, not labels.** No tier name
   ("hot", "warm", "cold") escapes the engine. The user sees a
   `ResolvedGraduatedRetention`-shaped suggestion with cost framing.
   (Arc decision F)
5. **Recommendation events are not logged in the ADR-114 event log.**
   Routine recommendation emissions are not Urd decisions. Future
   auto-tune *applying* a shape is the right Event hook.
6. **The recommendation layer and `storage_critical` (UPI 031) are
   independent.** Same telemetry, different purposes. They should not
   contradict each other; future coordination is its own UPI if
   contradiction emerges in evidence.

## Open concerns (to revisit on evidence)

1. **Constant calibration.** Post-X4 evidence checkpoint (~30 days of
   operation) may show the budgets are off. Anticipated revision via
   ADR-115 amendment.
2. **Metadata cost integration.** UPI 044 will introduce the metadata
   axis. Whether the symmetry claim extends to metadata (it should,
   structurally) becomes an ADR-115 amendment when 044 lands.
3. **Bursty subvolumes.** The `BurstyPattern` advisory note is the X1
   handling for bimodal churn. If the post-X4 checkpoint shows bursty
   patterns are common and the steady-state recommendation actively
   misleads users, a future ADR may introduce a stateful bimodal model
   (out of scope for Phase D-2).
4. **Recommendation/storage_critical coordination.** Independence is
   the X1 stance. If evidence shows the two surfaces contradict each
   other in user-visible ways, a coordination ADR follows.

## Related

- **ADR-108** — Pure function modules. The recommendation layer is a
  pure module (`policy.rs`).
- **ADR-110** — Protection promises. The recommendation layer is the
  path by which named levels accumulate graduation evidence (see
  amendment).
- **ADR-113** — Do-No-Harm invariant. Source-pool stewardship; this
  ADR generalizes the cost insight to both pools.
- **ADR-114** — Structured event log. Recommendation emissions are
  *not* events; future auto-tune *applies* are.
- **UPI 030** — Drift telemetry. The signal this layer consumes.
- **Arc proposal:** `docs/95-ideas/2026-05-09-arc-proposal-retention-symmetry.md`
- **X1 design:** `docs/95-ideas/2026-05-09-design-041-recommendation-mvp.md`
- **Evidence base:** `docs/99-reports/2026-05-09-retention-tuning-from-real-world-data.md`

## Amendment 2026-05-16 — Headroom-aware recommendations (UPI 044, X4)

UPI 044 ships X4 of the retention-symmetry arc: when storage signals
indicate the source pool is shrinking or a destination's metadata is
pressured, the recommendation engine surfaces an **adjustment note** —
and in the Pressure tier, also a **tightened shape** — alongside the
original churn-fit suggestion. The output stays advisory; UPI 031 owns
the imperative path. Per-subvolume per-role severity (Healthy / Caution
/ Pressure / Critical) replaces the X1 single-shape output.

### Scope clarification (D1)

UPI 044 is **headroom-only**. The earlier "metadata cost model" framing
in this ADR (lines 11, 106–107, 215–216, 308–309) is rescinded:

- Lines 11 / 106–107 / 215–216 / 308–309 each describe UPI 044 as the
  metadata-cost-model UPI. Replace with: "UPI 044 ships headroom-aware
  recommendations. A separate UPI (TBD) ships the metadata cost model."

The metadata cost model remains valuable future work — it just isn't UPI
044. Today's pressure signal uses observed metadata utilization ratio
directly, not a forward-projected metadata cost.

### Headroom substance (D2–D18)

**Severity classification (D7).** A pure function
`classify_headroom_severity(HeadroomContext) -> HeadroomSeverity` takes
three signals — source-pool free ratio, source-pool time-to-empty (from
the 7-day shrink trend), destination-metadata utilization ratio — and
returns the max-of-triggers severity. Healthy < Caution < Pressure <
Critical. Critical is **not** in the classify domain; doctor.rs injects
it externally (see D10/D14).

**Thresholds (D5).** Committed in code, not config (per Invariant 3).

| Signal                         | Caution     | Pressure    |
|--------------------------------|-------------|-------------|
| Source-pool free ratio         | `< 25%`     | `< 15%`     |
| Source-pool time-to-empty      | `< 90 days` | `< 30 days` |
| Destination metadata ratio     | `> 85%`     | `> 92%`     |

The boundary is strict (`<` / `>`); exact-threshold values map to the
lower tier.

**Tightening rule (D6).** When severity is Pressure, the engine
multiplies the recommended shape's hourly/daily/weekly/monthly slot
counts by `HEADROOM_TIGHTEN_MULTIPLIER = 0.7` (floor-rounded, re-clamped
to `[clamp_min, clamp_max]`). The result is exposed as the
recommendation's `adjusted` field. Monthly stays `Count(n)`; yearly
stays `0`. The tightened shape's cost projection (`adjusted_cost`) is
emitted alongside so the voice layer's "recover ~X" tail matches what
it renders (Rule 1).

**AdjustmentReason taxonomy + priority tiebreak (D8).** When multiple
signals fire at the same severity, the reason carried on the
recommendation row resolves by priority:

```
DestinationMetadataPressure  >  SourcePoolLow  >  SourcePoolShrinking
```

Each variant embeds the numeric value that drove it (free_ratio,
days_to_empty, ratio + drive_label). `StorageCritical` is its own
variant injected when `is_storage_critical(name)` fires.

**UPI 031 coordination contract (D10, refined D14).** UPI 044 ships
`src/storage_critical.rs` with a stub
`is_storage_critical(subvolume: &str) -> bool { false }`. UPI 031 will
later replace the body with its chosen truth source (sentinel state,
event log, or per-destination predicate). Doctor injects the predicate
as a closure into `build_doctor_recommendation_view_inner` so tests can
substitute, and so signature changes propagate to a single call site.
The stub takes no `state_db` argument and no other state — the simplest
possible shape that UPI 031 can widen without breaking callers.

**Per-role placement (D18).** Severity, reason, and adjusted shape live
on each `HeadroomAwareRecommendation` (one per role) — not on the row.
Local and External roles see different `HeadroomContext` inputs (Local
sees source-pool signals only; External sees both source-pool and
destination-metadata signals — D15), so per-role placement is correct
both modeling-wise and rendering-wise.

**Trend computation (D17).** A pure function
`drift::compute_pool_free_bytes_trend(samples, window, now, min_sample_days)`
performs a linear regression on `source_free_bytes` across all samples
from all subvolumes on a pool (the **union** is implicit — the caller
passes samples from every subvol on the pool). Returns `Some(slope
bytes/day)` when at least `min_sample_days` distinct calendar days are
covered; `None` otherwise. No per-subvolume dedup — the noise from
intra-day jitter is absorbed by the regression's slope estimator.

**Role-conditional context (D15).** `HeadroomContext` is built per
(subvolume, role) pair. Local row's context omits
`destination_metadata_ratio` (the row is about source-pool retention,
not destination). External row's context includes the max-of-mounted
destination metadata ratio plus the corresponding drive label. (R4: the
label is `DriveConfig.label` verbatim — matches what `urd status` and
notifications surface.)

**Silence interaction matrix (D16).** UPI 041 silenced rows where
`suggested == current` and there was no headroom signal. UPI 044
preserves that silence but escalates Pressure and Critical: even if the
shape recommendation is silent, a row appears for those tiers to carry
the adjustment note. Caution does not escalate (silence wins).

| Shape-quiet (UPI 041) | Headroom severity | Row emitted? |
|-----------------------|-------------------|--------------|
| Yes                   | Healthy           | No           |
| Yes                   | Caution           | No           |
| Yes                   | Pressure          | **Yes** (synth) |
| Yes                   | Critical          | **Yes** (synth) |
| No                    | any               | Yes          |

**`PoolSpace` helper (D13).** `pools.rs` exposes `PoolSpace { free_bytes,
capacity_bytes }` and `pool_space(&Path) -> Result<PoolSpace>` — one
statvfs call yielding both numbers. The pre-UPI-044 `pool_free_bytes()`
becomes a thin wrapper. Capacity is the new dependency: the
`source_pool_free_ratio` signal needs both numerator and denominator
from the same syscall.

### Critical / Pressure-at-MIN synth path (R1)

When the churn-fit engine returns `None` (no churn signal — cold/
transient subvolume), severity in `{Pressure, Critical}` requires a
row anyway to surface the headroom message. Doctor.rs synthesizes a
minimal `HeadroomAwareRecommendation` via
`policy::headroom_aware_pointer_only(...)`: `suggested == current`,
both `current_cost` and `suggested_cost` set to `CostProjection {
data_bytes: 0, snapshot_count: 0 }`, `adjusted = None`,
`adjusted_cost = None`. The voice renderer detects the synth shape
(both costs zero AND suggested == current) and renders only the
reason line — no numeric tail, no shape line. The same path handles
**Pressure-at-MIN** (severity is Pressure but the engine couldn't
tighten further because the shape was already at `clamp_min`): the
renderer says "shape already at minimum; consider expanding storage."

### Doctor JSON schema bump (R3)

`urd doctor [--thorough] --json` output gains a top-level
`schema_version: u32` field. v1 retroactively names the pre-UPI-044
shape (rows' `local`/`external` typed as `ShapeRecommendation`); v2
names the post-UPI-044 shape (typed as `HeadroomAwareRecommendation`
with nested `.recommendation` plus new `severity`, `reason`,
`adjusted`, `adjusted_cost` fields). Future breaking JSON shape
changes bump `schema_version` and are CHANGELOG-noted.

This formalizes `urd doctor` output alongside heartbeat
(`heartbeat.json`) and Prometheus textfile contracts (ADR-105 §
"Backward Compatibility"). `--json` consumers should read
`schema_version` and either branch on it or pin a supported value.

### Limitations and 30-day evidence checkpoint (R7)

The threshold numbers (25%/15% free, 90/30 days time-to-empty,
0.85/0.92 metadata, 0.7 tightening multiplier) are **N=1-calibrated**
from the 2026-05-09 retention-tuning report. The post-UPI-044 30-day
evidence checkpoint owns:

1. **Threshold flap monitoring.** If the Caution/Healthy boundary
   produces visible flapping under boundary conditions in
   `urd doctor --thorough` runs (e.g., free ratio oscillating around
   25%), the boundary needs hysteresis or widened spacing. Hysteresis
   is the more honest fix and the natural home is UPI 031's state
   machine, **not** `policy.rs` (policy.rs is pure; hysteresis is
   stateful). UPI 031's predicate eventually owns "is this subvolume
   currently in storage_critical?" — extending it to "is this
   subvolume currently in headroom_pressure?" is the right next step
   if flapping is observed.
2. **Pressure tightening multiplier.** If Pressure recommendations
   prove too aggressive (users dismiss them) or too conservative
   (users still hit storage_critical), revise `0.7` upward or
   downward respectively.
3. **Metadata threshold revision.** If the 0.85/0.92 numbers fire
   too often on healthy filesystems or too rarely on stressed ones,
   revise. Cross-check against the future metadata cost model when
   it ships.

The checkpoint deliverable is an ADR-115 amendment, not a new ADR.

### Not in scope for UPI 044

- **Verdict coupling.** `urd doctor` verdict (`Ok` / `Warn` / `Fail`)
  is unchanged by UPI 044. Headroom severity is presented per-row, not
  rolled up. If the user wants the verdict to surface storage pressure,
  the proper home is UPI 031's storage_critical bundle.
- **Per-drive recommended shapes.** A subvolume sent to multiple drives
  still gets one External recommendation, not one per drive. The
  metadata ratio is max-of-mounted-drives; the drive label embedded in
  the reason is the worst-offending drive.
- **User-tunable thresholds.** Per Invariant 3 (no `[recommendations]`
  config section). Friction floor remains "ignore the suggestion."
- **Auto-apply.** Long-term north star; future arc.
