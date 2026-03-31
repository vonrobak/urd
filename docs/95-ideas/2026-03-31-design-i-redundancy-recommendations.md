# Design: Redundancy Recommendations (Idea I)

> **TL;DR:** A pure function in awareness.rs detects redundancy gaps from config and
> assessment state, producing structured advisories that flow through voice.rs (status
> output), sentinel state file (Spindle tray icon), and notifications. Gentle guidance,
> not gates.

**Date:** 2026-03-31
**Status:** Reviewed
**Origin:** Idea I from [2026-03-30 brainstorm](2026-03-30-brainstorm-transient-workflow-and-redundancy-guidance.md), scored 9/10.
**User feedback:** "This system should have some way of communicating with the upcoming
tray icon build (which is another idea document, but not yet designed)."
**Review:** [2026-03-31 design review](../../99-reports/2026-03-31-design-i-review.md) — approved with findings (8/10). All findings incorporated below.

## Problem

Users may have redundancy gaps they don't know about. A subvolume marked "resilient" with
all drives on the same shelf doesn't survive a house fire. A "protected" subvolume backed
to a single drive has a single point of failure. Urd already knows this -- config declares
drive roles, awareness computes per-subvolume state -- but the system doesn't connect
these facts into proactive guidance.

The user shouldn't need to reason about failure scenarios. Urd sees the full picture. When
a gap exists, Urd should name it clearly and suggest a path forward.

## Non-goals

- **Not enforcement.** Advisories don't block backups or degrade promise states. That
  belongs to idea E (promise redundancy), which enforces drive-role requirements. I advises;
  E enforces. They compose but are independent.
- **Not a coverage map.** Idea G (disaster coverage map) answers "what survives what?" --
  a richer, per-scenario analysis. Redundancy recommendations are simpler: detect structural
  gaps in the setup itself, not runtime loss projections.
- **Not config validation.** Preflight catches structural errors (missing paths, invalid
  intervals). Redundancy advisories are opinions about sufficiency -- they belong in the
  awareness layer, not the config boundary.

## Advisory taxonomy

Four advisory types, each detected from config + assessment state. Ordered by severity
(most concerning first), though all are guidance, not alarms.

### 1. All drives local -- no offsite protection

**Trigger:** A subvolume with `protection_level = "resilient"` where no configured drive
has `role = "offsite"`.

**Signal:** The user declared maximum protection intent but the physical setup cannot
survive site-level disasters (fire, flood, theft, power surge).

**Overlap with idea E:** E's preflight check (`resilient-without-offsite`) detects the
same condition at config validation time -- once, at startup. This advisory surfaces the
same fact persistently in ongoing monitoring (`urd status`, sentinel state, notifications).
This is intentional overlap at different layers: E catches it early before any backup runs;
I surfaces it persistently so a user who ignores E's preflight warning still sees the gap.
They must not be consolidated -- doing so would lose either the early-catch or
persistent-surfacing property.

**Example voice:**
```
htpc-home seeks resilience, but all drives share the same fate.
Consider designating a drive as offsite to protect against site loss.
```

### 2. Offsite drive stale

**Trigger:** A drive with `role = "offsite"` has not been seen (no successful send) in
more than a configurable threshold. Default: 30 days.

**Signal:** The offsite copy is aging. The older it gets, the more data is at risk in a
site-level disaster.

**Migration note:** awareness.rs currently emits a stringly-typed "consider cycling"
advisory at 7 days for unmounted drives (`SubvolAssessment.advisories: Vec<String>`). As
part of this work, that existing advisory should be migrated into the structured
`RedundancyAdvisory` system. The 7-day "consider cycling" message becomes an
`OffsiteDriveStale` advisory with the 30-day threshold (the 7-day variant was too
aggressive for an offsite rotation pattern). Non-redundancy advisories (clock skew,
send_enabled without drives) remain stringly-typed since they are operational, not
redundancy-related. This eliminates the confusing overlap where users would see the old
stringly-typed advisory between days 7-30 and both advisories after day 30.

**Example voice:**
```
The offsite copy on WD-18TB1 has aged 23 days.
The longer it ages, the more is lost if fate visits your door.
```

### 3. Single point of failure

**Trigger:** A subvolume with `protection_level = "protected"` or `"resilient"` that sends
to only one drive (regardless of role).

**Signal:** "Protected" implies redundancy beyond local snapshots. With only one external
drive, a simultaneous NVMe + drive failure loses everything. Two drives (even both local)
meaningfully reduce risk.

**Example voice:**
```
htpc-home rests on a single external drive.
A second drive would guard against the failure of one.
```

### 4. Transient with no local recovery (informational)

**Trigger:** A subvolume with `local_retention = "transient"` and all drives unmounted.

**Signal:** There are currently zero accessible copies. This isn't a warning -- transient
is a deliberate choice, and the external copies exist on unmounted drives. But it's worth
surfacing as information, not alarm. The user should know recovery requires connecting a
drive.

**Example voice:**
```
htpc-root lives only on external drives while local copies are transient.
Recovery requires a connected drive.
```

**Severity:** Informational. Different visual treatment from the other three. Does not
contribute to advisory counts that affect Spindle badge state.

## Data flow

```
config + assessments
       |
       v
compute_redundancy_advisories()   <-- pure function, awareness.rs
       |
       v
Vec<RedundancyAdvisory>           <-- structured type, output.rs
       |
       +---> voice.rs             renders in `urd status` output
       |
       +---> sentinel state file  advisory_summary field for Spindle
       |
       +---> notify.rs            RedundancyChanged event on state transitions
```

**Recomputation timing:** `compute_redundancy_advisories()` runs on every sentinel tick
using fresh assessment data (not cached heartbeat). This means advisories resolve promptly
when a drive is connected or conditions change, consistent with the sentinel's existing
behavior of re-assessing on each tick.

### Module changes

#### output.rs -- RedundancyAdvisory type

```rust
/// A structured redundancy advisory. Not stringly-typed -- the type carries
/// the detection reason and enough context for voice rendering and Spindle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedundancyAdvisory {
    pub kind: RedundancyAdvisoryKind,
    pub subvolume: String,
    /// Affected drive label (for offsite-stale and single-point advisories).
    pub drive: Option<String>,
    /// Human context (e.g., "23 days since last offsite send").
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyAdvisoryKind {
    /// All drives are local for a resilient subvolume.
    NoOffsiteProtection,
    /// Offsite drive not seen in > threshold days.
    OffsiteDriveStale,
    /// Single external drive for a protected/resilient subvolume.
    SinglePointOfFailure,
    /// Informational only -- does not affect advisory counts.
    TransientNoLocalRecovery,
}
```

The enum is declared worst-first so that derived `Ord` makes `min()` yield the worst
advisory kind, consistent with `PromiseStatus`, `OperationalHealth`, and `ChainHealth`
elsewhere in the codebase. Use `min()` for worst, never `max()`.

#### awareness.rs -- compute_redundancy_advisories()

```rust
/// Compute redundancy advisories from config and assessment state.
///
/// Pure function: config + assessments in, advisories out. No I/O.
/// Called after `assess()` produces SubvolAssessments.
#[must_use]
pub fn compute_redundancy_advisories(
    config: &Config,
    assessments: &[SubvolAssessment],
    now: NaiveDateTime,
) -> Vec<RedundancyAdvisory>
```

The function iterates subvolumes and checks:

1. For each resilient subvolume: are any drives `role = "offsite"`? If not, emit
   `NoOffsiteProtection`.
2. For each offsite drive: when was the last successful send to any subvolume? If older
   than threshold (30 days default, future: configurable), emit `OffsiteDriveStale` for
   each affected subvolume.
3. For each protected/resilient subvolume: count configured, send-enabled drives. If
   exactly one, emit `SinglePointOfFailure`.
4. For each transient subvolume: if all configured drives are unmounted (from drive
   assessments), emit `TransientNoLocalRecovery`.

The function needs `now` for the offsite staleness check. It reads last-send ages from
the `DriveAssessment` entries already computed by `assess()`.

#### voice.rs -- rendering

Advisories render as a distinct section in `urd status`, visually separated from the
per-subvolume promise table. The tone is Urd's wisdom -- observational, not alarmist.

```
REDUNDANCY

  htpc-home seeks resilience, but all drives share the same fate.
  Consider designating a drive as offsite to protect against site loss.

  htpc-home rests on a single external drive.
  A second drive would guard against the failure of one.
```

Design principles for the voice:
- Two lines per advisory: observation + suggestion.
- No "WARNING:" prefix. No urgency markers. Urd states what it sees.
- Informational advisories (transient-no-recovery) use lighter treatment -- perhaps
  indented differently or grouped under a separate "NOTES" heading.
- When no advisories exist, the section is omitted entirely. Silence is approval.

#### sentinel state file -- advisory summary

Add an `advisory_summary` field to `SentinelStateFile`:

```rust
/// Redundancy advisory summary for tray icon consumers (schema v3+).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub advisory_summary: Option<AdvisorySummary>,
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisorySummary {
    /// Count of non-informational advisories.
    pub count: usize,
    /// Worst advisory kind (for badge/icon decisions).
    pub worst: Option<String>,
}
```

Schema version bumps to 3. Backward compatible: v2 readers ignore the field (serde
`skip_serializing_if` + `default`).

**Absent means unknown, not zero.** When `advisory_summary` is `None` (e.g., a v2
state file from an older Urd version), consumers must treat this as "advisories not
computed" rather than "zero advisories." Spindle should show no badge (not a zero-count
badge) when the field is absent. This prevents an older Urd from appearing advisory-free
when it simply does not compute advisories yet.

#### notify.rs -- state change notifications

Notifications fire when the advisory set changes between sentinel ticks -- not on every
tick. The sentinel tracks the previous advisory set and diffs:

- **New gap appears:** Notification with the new advisory text.
- **Gap resolves:** Notification that the gap has been addressed.

This uses the existing notification infrastructure. A new event variant
`RedundancyAdvisoryChanged` (or folded into the existing `HealthChanged` event) carries
the diff.

Notification frequency: at most once per gap appearance/resolution. The sentinel deduplicates
by tracking the set of active `(kind, subvolume)` pairs.

**Cooldown for cyclic advisories.** Offsite drive rotation creates a predictable cycle:
drive departs, advisory appears at 30 days, drive returns, advisory resolves, repeat.
To prevent notification fatigue from this expected pattern:

- **Suppress resolution notifications for quickly-resolved gaps.** If an `OffsiteDriveStale`
  advisory resolves within 48 hours of detection, suppress the `RedundancyGapResolved`
  notification. The user just connected the drive -- they know.
- **Suppress re-detection after recent resolution.** After a `RedundancyGapResolved` event,
  suppress `RedundancyGapDetected` for the same `(kind, subvolume)` pair for 7 days. This
  handles the case where the drive is disconnected again immediately after cycling.
- **Always notify on first detection.** The first time an advisory kind appears for a
  subvolume (no prior resolution in history), always notify. The user needs to know about
  a new gap.

This keeps notifications meaningful for new problems while silencing the expected monthly
rhythm of drive cycling.

## Spindle integration

Spindle (the tray icon, brainstormed but not yet designed) reads `sentinel-state.json`.
The advisory summary provides everything Spindle needs without parsing individual advisories:

1. **Badge.** If `advisory_summary.count > 0`, Spindle can show a small indicator on the
   tray icon (e.g., a subtle dot or number). The icon itself (`VisualIcon`) is unchanged --
   advisories don't override the safety/health-driven icon state. They're a secondary signal.

2. **Tooltip.** Spindle's tooltip can include "N redundancy suggestions" when advisories
   exist. Clicking could open `urd status` in a terminal, or (future) a Spindle detail view.

3. **No advisory detail in state file.** The state file carries counts, not full advisory
   text. Spindle that wants detail can shell out to `urd status --json` (which includes
   the full `RedundancyAdvisory` list). This keeps the state file small and avoids
   duplicating the voice layer's rendering logic in Spindle.

**Design decision:** Advisories do NOT influence `VisualIcon`. The icon reflects data
safety and operational health -- hard facts. Advisories are opinions about setup quality.
Mixing them would dilute the icon's signal. A user with all-PROTECTED, all-healthy
subvolumes should see the green icon even if they have no offsite drive. The advisory badge
is a separate, softer signal.

## Relationship to other ideas

| Idea | Relationship |
|------|-------------|
| **E (promise redundancy)** | E enforces drive-role requirements in promise levels. I advises. They compose: E could use I's detection logic to decide when to degrade a promise. Independent implementations. The `NoOffsiteProtection` advisory and E's `resilient-without-offsite` preflight check detect the same condition at different layers (config-time vs runtime) -- this is intentional overlap, not redundancy. See advisory type 1 above for details. |
| **G (coverage map)** | G is "what survives what disaster?" -- runtime loss projection. I is "your setup has structural gaps." G subsumes I's information but requires more data and computation. I ships first as the simpler win. |
| **D (redundancy scorecard)** | D is the rendered output of I's data. I computes; D (via voice.rs) displays. They're the same feature at different layers. |
| **O (progressive disclosure)** | O teaches redundancy through contextual messages over time. I surfaces current gaps. O could use I's advisories as triggers for its educational messages. |
| **Spindle (tray icon)** | Spindle consumes the advisory summary from sentinel state. I provides the data; Spindle renders the badge. |

## Test strategy

~15 tests in awareness.rs, covering:

1. **No advisories when setup is sound.** Resilient subvolume with offsite drive, multiple
   drives, recent sends. Expect empty advisory list.

2. **NoOffsiteProtection.** Resilient subvolume, two drives both `role = "primary"`. Expect
   one advisory of kind `NoOffsiteProtection`.

3. **OffsiteDriveStale.** Offsite drive with last send 35 days ago. Expect advisory. At
   29 days, expect none (under threshold).

4. **SinglePointOfFailure.** Protected subvolume with exactly one configured drive. Expect
   advisory. With two drives, expect none.

5. **TransientNoLocalRecovery.** Transient subvolume, all drives unmounted. Expect
   informational advisory. With one drive mounted, expect none.

6. **Multiple advisories.** A single subvolume can have multiple advisory types
   simultaneously (e.g., single drive + no offsite). Verify all are emitted.

7. **Guarded subvolumes excluded.** Guarded subvolumes (local-only by design) should not
   trigger SinglePointOfFailure or NoOffsiteProtection. The user explicitly chose minimal
   protection.

8. **Advisory ordering.** Verify `Vec<RedundancyAdvisory>` is sorted worst-first for
   consistent rendering. Verify `min()` yields the worst kind (`NoOffsiteProtection`).

9. **Informational advisories excluded from count.** `AdvisorySummary.count` should not
   include `TransientNoLocalRecovery` advisories.

10. **Sentinel diff detection.** Given previous and current advisory sets, verify correct
    new/resolved detection for notification triggers.

11. **Notification cooldown.** Verify that resolution notifications are suppressed when
    an `OffsiteDriveStale` advisory resolves within 48 hours of detection. Verify
    re-detection suppression within the 7-day cooldown window.

Output types (`RedundancyAdvisory`, `AdvisorySummary`) get serde round-trip tests in
output.rs.

## Effort estimate

~80-100 lines across awareness + output + voice + sentinel. 10-15 tests. 1-2 sessions.

| Module | Lines (est.) | Tests |
|--------|-------------|-------|
| output.rs | ~20 (types) | 2 (serde round-trip) |
| awareness.rs | ~40 (compute function) | 10 |
| voice.rs | ~15 (rendering) | 2 |
| sentinel.rs | ~15 (summary + diff) | 3 |
| notify.rs | ~10 (event variant + cooldown) | 2 |

## Open questions

1. **Offsite staleness threshold.** Hardcoded 30 days or configurable? Starting hardcoded
   aligns with the "opaque levels" principle -- if it needs tuning, that's a signal the
   threshold is wrong, not that users should pick their own. Revisit if 30 days proves
   wrong in practice.

2. ~~**Interaction with existing stringly-typed advisories.**~~ Resolved: migrate the
   existing offsite cycling advisory into `RedundancyAdvisory` as part of this work. See
   "Migration note" in advisory type 2 above.

3. ~~**Schema version bump.**~~ Resolved: absent `advisory_summary` means "unknown, not
   zero." See sentinel state file section above.

## Review findings incorporated

Findings from the [architectural review](../../99-reports/2026-03-31-design-i-review.md)
(2026-03-31, verdict: approve with findings, 8/10):

| Finding | Severity | Resolution | Location in doc |
|---------|----------|------------|-----------------|
| S1: Ord direction inconsistent with codebase | Medium | Fixed. Enum declaration reversed to worst-first so `min()` yields worst, matching `PromiseStatus` et al. | output.rs type definition |
| S2: Parallel advisory systems without migration | Medium | Fixed. Existing 7-day "consider cycling" advisory migrated into structured `RedundancyAdvisory` system. Non-redundancy advisories remain stringly-typed. | Advisory type 2 migration note |
| S3: Notification fatigue for drive cycling | Medium | Added. Cooldown/suppression for cyclic advisories: suppress resolution within 48h, suppress re-detection within 7 days of resolution. | notify.rs section |
| S4: Overlap between I and E on no-offsite | Low | Documented. Intentional overlap at different layers (config-time vs runtime). | Advisory type 1, relationship table |
| S5: Schema version without Spindle guidance | Low | Documented. Absent `advisory_summary` means "unknown, not zero." Spindle shows no badge when field is missing. | Sentinel state file section |
| S6: Recomputation timing unclear | Low | Documented. Advisories recompute on every sentinel tick using fresh assessment data. | Data flow section |
