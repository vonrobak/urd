# ADR-104: Graduated Retention Model

> **TL;DR:** Urd uses Time Machine-style graduated retention — keep everything recent, thin
> progressively with age — instead of fixed snapshot counts. Local retention has four time
> windows (hourly, daily, weekly, monthly). External retention uses count-based limits with
> space-governed cleanup. Space pressure mode aggressively thins when the filesystem is low.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** Roadmap's original fixed-count retention (`daily_keep`/`weekly_keep`/`monthly_keep`)

## Context

The bash script used flat retention: keep the last N snapshots, delete the rest. With daily
snapshots this was adequate (keep 15 = 15 days of history). But with interval-based
scheduling producing snapshots every 15 minutes to every hour, flat retention would either
keep too few (losing history depth) or too many (filling the disk).

The NVMe system drive (~128GB) hosts `htpc-home` and `htpc-root` snapshots. The btrfs-pool
(multi-TB) hosts 7 subvolumes. These have very different space constraints, and the
retention model must handle both.

## Decision

### Local retention: graduated time windows

A typical graduated retention policy:

```toml
# Per-subvolume (custom policy) or derived from a named protection level
local_retention = { hourly = 24, daily = 30, weekly = 26, monthly = 12 }
```

- `hourly = 24` — keep 24 hourly snapshots (1 day of hourly granularity)
- `daily = 30` — then 30 daily (1 per day, newest in each day)
- `weekly = 26` — then 26 weekly (1 per ISO week)
- `monthly = 12` — then 12 monthly (1 per calendar month)

Within each window, keep the *newest* snapshot per time period. This produces ~92 snapshots
covering ~18 months, with fine granularity for recent data and coarse granularity for old.

Per-subvolume retention comes from either a named protection level (opaque — see ADR-110)
or explicit values on the subvolume (custom policy). There is no `[defaults]` merge — configs
are self-describing artifacts (ADR-111). Custom subvolumes specify their full retention;
omitted fields use hardcoded fallbacks in the binary.

### Space pressure mode

When a snapshot root's filesystem drops below `min_free_bytes`, the retention engine enters
space pressure mode: the hourly window is thinned to 1 per hour instead of keeping
everything. This is the first line of defense for the NVMe drive.

### External retention: count-based + space-governed

External drives use simpler count-based retention (e.g., keep last 14 per subvolume) with
space-governed cleanup. The executor deletes oldest-first and re-checks free space after
each deletion, stopping when the space threshold is met. This fills the drive intelligently
without requiring the planner to know exact snapshot sizes.

### Monthly retention uses calendar month subtraction

Not `days * 30`. A snapshot from January 31 is "1 month old" on February 28, not on
March 2. This prevents the slow drift that accumulates with day-based month approximation.

## Consequences

### Positive

- Recovery window spans months with manageable snapshot counts (~92 per subvolume locally)
- Fine granularity when most useful (recent) and coarse when acceptable (old)
- Space pressure prevents the NVMe from filling — critical for system health
- Offsite drive pin parents survive for ~5 months under graduated retention, supporting
  quarterly drive rotation without forcing full sends (see ADR-020)
- External space-governed cleanup adapts to actual snapshot sizes without estimation

### Negative

- Graduated retention is more complex to implement and reason about than flat counts
- Space pressure mode can delete snapshots the user expects to keep — this is intentional
  (disk full is worse) but may surprise users
- The planner's retention proposals and the executor's space-governed reality can diverge
  — the executor logs skipped deletions so the operator can see the difference

### Constraints

- Retention must never delete a snapshot that is the current pin parent for any drive.
  This is enforced at both the planner level (exclusion) and executor level
  (defense-in-depth re-check before deletion).
- When `send_enabled` is true, snapshots newer than the oldest pin are protected from
  local retention — they may not have been sent to all drives yet.

## Amendment 2026-05-15: Yearly window

UPI 042 (config schema `v2`) extends the graduated retention stack with a `yearly` tier:

```
hourly → daily → weekly → monthly → yearly → beyond-window
```

### Slot key

Yearly's slot key is the calendar year `(year)`. One snapshot survives per calendar year for
`yearly` years past the monthly cutoff. Calendar-year semantics were chosen for symmetry
with the calendar-month arithmetic already in `retention.rs` (`Months::new` in the monthly
cutoff calculation). A snapshot from January 31, 2024 is "1 year old" on January 1, 2025 —
not on January 31, 2025 — matching how a January-31 snapshot is "1 month old" on February 1.

### Cutoff

The yearly cutoff is computed from the monthly cutoff, subtracting `yearly` calendar years
(implemented as `Months::new(yearly * 12)`), saturating to `NaiveDateTime::MIN` on overflow.

When the monthly tier is `Unlimited` (see Amendment 2026-05-15 in ADR-105 and the
`MonthlyCount` enum), the yearly window is **logically subsumed**: every snapshot newer than
the (non-existent) monthly cutoff is already retained, so a yearly thinning rule has nothing
left to thin. The retention engine treats yearly as `0` in that case, and `preflight` emits
an advisory `redundant-yearly` warning so the user sees the redundancy explicitly. The
recovery-window display likewise suppresses the yearly row when monthly is `Unlimited`, so
the displayed shape agrees with the engine's behavior.

### Bounded type

`yearly` is `Option<u32>` with no `"unlimited"` variant. The asymmetry with `monthly` is
deliberate: `monthly = "unlimited"` exists only to preserve a v1 contract (`monthly = 0`
meant "unlimited" under v1 semantics) via migration. `yearly` is new in v2 and starts
bounded. A `YearlyCount` enum can be added in a future UPI if evidence demands; today there
is no v1 contract to preserve and no operational data to justify unbounded yearly retention.

### Recommender scope

The retention shape recommender (`policy::recommend_shape`, UPI 041 / ADR-115) stays
**4-slot** in this amendment. Yearly is a v2 user opt-in only; the recommender does not
emit yearly suggestions. A future UPI can expand the recommender to 5 slots with measured
`RoleParams` once there is evidence of demand. This avoids silently changing UPI 041's
already-shipped recommendation outputs.

## Related

- ADR-020: Daily external backups (graduated retention enables quarterly offsite rotation)
- ADR-103: Interval-based scheduling (frequent snapshots require graduated retention)
- ADR-105: Backward-compatibility contracts (Amendment 2026-05-15 — `monthly = 0` semantic shift)
- ADR-111: Config system architecture (Amendment 2026-05-15 — `config_version = 2`)
- ADR-115: Retention shape symmetry and the recommendation layer (4-slot recommender scope)
- Phase 1 journal (`docs/98-journals/2026-03-22-urd-phase01.md`) — retention redesign
- Roadmap (`docs/96-project-supervisor/roadmap.md`) — original flat retention specification
