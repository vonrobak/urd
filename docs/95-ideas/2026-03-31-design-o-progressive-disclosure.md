# Design: Progressive Disclosure of Redundancy Concepts (Idea O)

> **TL;DR:** Urd observes your backup journey and shares contextual wisdom at natural
> milestones. Not a tutorial -- Urd notices what changed and reflects on what it means.
> Each insight fires at most once, is tied to your actual state, and is delivered through
> `urd status` output and the sentinel state file -- never through the notification pipeline.

**Date:** 2026-03-31
**Status:** Reviewed
**Origin:** Idea O from [2026-03-30 brainstorm](2026-03-30-brainstorm-transient-workflow-and-redundancy-guidance.md), scored 10/10.

## Problem

Users don't learn 3-2-1 backup strategy from documentation. They learn it from experience
-- from the day they wish they had a second copy, or the relief of finding data on an
offsite drive after a hardware failure.

Urd already tracks the state transitions that map to these lessons: first backup, first
offsite send, promise level upgrades, staleness degradation, recovery from UNPROTECTED.
Today, these transitions produce technical notifications ("PromiseRecovered: htpc-home
AT RISK -> PROTECTED"). The information is correct but doesn't help the user understand
*why* it matters or what it means for their data safety.

The opportunity: Urd accompanies the user over time. It can teach redundancy concepts
through contextual messages tied to the user's actual state transitions -- not by
explaining 3-2-1, but by making its principles visible through experience.

## Design philosophy

### State-triggered, not time-triggered

Insights are never scheduled ("day 7: tell user about offsite"). They fire when the user's
actual backup state crosses a meaningful threshold. A user who connects an offsite drive on
day 2 gets the offsite insight on day 2. A user who never uses offsite never sees it.

### Milestone-gated, not repeating

Each insight fires at most once. The first backup insight fires once, forever. No nagging,
no reminders, no "did you know?" tips. The milestone history records what was delivered
and when, so Urd never repeats itself.

### Observational, not instructional

Urd doesn't lecture. She observes and reflects. The difference:

- **Instructional** (wrong): "You should connect your offsite drive more often to maintain
  3-2-1 backup compliance."
- **Observational** (right): "The offsite thread grows thin. Twenty-one days since WD-18TB1
  last carried your words forward."

The insight should feel like it was triggered by *your* specific state, not like a generic
tip that could appear for anyone.

### Earned voice

The mythological register is earned by relevance. A first-backup message warrants a brief
observation. An extended healthy streak warrants quiet confidence. Nothing warrants a
paragraph. Short is wise; long is preachy.

## Relationship to Idea I (Recommendations)

Idea I (redundancy recommendations) and Idea O (progressive disclosure) address different
moments in the user's experience:

| Aspect | I: Recommendations | O: Progressive disclosure |
|--------|-------------------|--------------------------|
| Trigger | Gap detection (status query) | State transition (milestone) |
| Timing | Every `urd status` invocation | Once per milestone, ever |
| Content | "Your setup is missing X" | "Your data just gained Y" |
| Voice | Advisory, actionable | Observational, reflective |
| Repeats | Yes (while gap persists) | Never (milestone-gated) |

They complement each other: recommendations surface what's missing, insights acknowledge
what's been gained. Both are pure functions over the same awareness state.

## Insight catalog

Draft messages below are directional -- the voice needs iteration. The catalog is
intentionally small. Quality over quantity.

### Journey milestones (positive state transitions)

**1. First backup completes**

Trigger: First successful snapshot + send recorded in state DB (run count == 1).

> Your data now rests in two places. If one thread frays, the other holds.

Why this matters: The user just crossed from "no backup" to "one backup." The single most
important transition in data safety. Acknowledge it without overstating.

**2. First offsite drive receives data**

Trigger: First successful send to a drive with `role = "offsite"`. Parameterized by
drive_label -- fires independently for each offsite drive.

> Your data has crossed a threshold -- it endures beyond these walls.

Why: Geographic separation is the step most people skip. Acknowledge the significance.

**3. All subvolumes reach PROTECTED**

Trigger: First time every configured subvolume has `PromiseStatus::Protected`.

> Every thread is woven tight.

Why: Full coverage is a real achievement. Many users have at least one lagging subvolume.

**4. Extended healthy streak (30 days all PROTECTED)**

Trigger: 30 consecutive days where every assessment shows all subvolumes PROTECTED.
Tracked via `streak_days` counter in the `insight_milestones` table (see State tracking).

> Thirty days, unbroken. Your data rests well.

Why: Quiet confidence. Urd has been working silently for a month and everything is fine.
This is the "silence means data is safe" principle made briefly visible.

Streak reset: The streak resets (counter drops to zero) when ANY subvolume drops below
PROTECTED in any assessment. The sentinel is the sole writer of the streak counter. On
sentinel restart, if the first assessment shows all-PROTECTED, trust the existing counter
(the backup heartbeat provides continuity evidence). If the first assessment shows
degradation, reset to zero.

**5. New drive added and receives first send**

Trigger: First successful send to a drive label not previously seen in milestone history.
Parameterized by drive_label.

> A new thread joins the weave. {drive_label} now carries your data forward.

Why: Expanding storage infrastructure is a deliberate act worth acknowledging.

Note: If a drive is removed from config and later re-added, the milestone does NOT re-fire
(the row already exists). The drive is not new -- the user already knows about it.

### Recovery milestones (returning from degraded state)

**6. Recovery from UNPROTECTED to PROTECTED**

Trigger: Any subvolume transitions from UNPROTECTED to PROTECTED *and* this is the first
time this specific recovery has been observed (not the first-ever assessment).
Parameterized by subvolume name.

> The weave is restored. {subvolume_name} is whole again.

Why: The user (or Urd autonomously) fixed a real problem. Acknowledge the return to safety.
Note: this fires per-subvolume, not globally, because the user cares about specific data.

### Operational milestones

**7. First transient cleanup**

Trigger: First time transient retention deletes local snapshots after confirmed external send.

> Local threads released -- your data lives safely on {drive_label}.

Why: Transient retention is counterintuitive ("you deleted my backups?"). The first time it
happens, briefly confirm that the external copy exists and the local deletion was intentional.

Note: Voice text needs further rework -- the original "loom" metaphor introduced an
inconsistent image. Keep to the thread/weave vocabulary established elsewhere.

**8. Incremental chain break recovered**

Trigger: First successful incremental send after a full send was required (chain break
detected and resolved in the same or subsequent run).

> The chain mended. Incremental sends resume -- swift where they were labored.

Why: Chain breaks cause slow full sends. When the chain recovers, the user benefits from
fast incrementals again. Acknowledge the return to efficiency.

### Future milestones (noted, not yet implemented)

**9. First successful restore (FirstRestore)**

Trigger: First successful file retrieval via `urd get` or `urd restore`.

This is arguably the most emotionally significant milestone -- the moment backups prove
their worth. Not implemented yet because `urd get` does not currently track restore history
in state.rs. When restore tracking is added, this milestone should be among the first
additions to the catalog.

## State tracking

### Milestone history table

New table in state.rs (`StateDb`):

```sql
CREATE TABLE IF NOT EXISTS insight_milestones (
    insight_key   TEXT PRIMARY KEY,  -- see "Insight identity" below
    insight_type  TEXT NOT NULL,     -- enum variant name: "FirstBackup", "FirstOffsite", etc.
    observed_at   TEXT NOT NULL,     -- ISO 8601 timestamp of triggering state transition
    delivered     INTEGER NOT NULL DEFAULT 0,  -- 1 if shown to user, 0 if pending
    streak_days   INTEGER           -- only used for HealthyStreak, NULL for all others
);
```

Design notes:

- **PRIMARY KEY on insight_key** enforces the "at most once" invariant at the database
  level. `INSERT OR IGNORE` means concurrent processes can't double-fire.
- **Insight identity format:** Singleton milestones use the variant name as key (e.g.,
  `"FirstBackup"`, `"AllProtected"`, `"HealthyStreak"`). Parameterized milestones include
  the parameter: `"FirstOffsite:WD-18TB1"`, `"NewDrive:WD-18TB2"`,
  `"RecoveryFromUnprotected:htpc-home"`. This ensures each parameterized event fires
  independently per drive or subvolume.
- **delivered column** tracks whether the user has seen this insight (via `urd status` or
  sentinel state file). Milestones recorded but not yet delivered carry forward to the
  next assessment -- they are never silently lost.
- **streak_days column** is used only for the HealthyStreak row. Incremented by the
  sentinel on each daily assessment where all subvolumes are PROTECTED. Reset to 0 when
  any subvolume drops below PROTECTED. The HealthyStreak milestone fires when
  `streak_days >= 30`. This eliminates the need for a separate streak tracking table.
- **No separate `insight_streaks` table.** The streak counter lives in the milestones
  table as a column, keeping the schema simple. The HealthyStreak row is inserted early
  (when streak tracking begins) with `delivered = 0` and `streak_days = 0`, then updated
  in place as the streak progresses.

## Delivery channels

Insights are NOT delivered through the notification pipeline. The notification system is
designed for alerts with urgency filtering (`min_urgency: Warning` by default), and
insights are observational -- routing them through alert infrastructure creates a category
mismatch that would cause all insights to be silently filtered for most users.

Instead, insights reach the user through three channels:

### 1. `urd status` output (primary)

The latest unacknowledged insight is displayed in `urd status` output. When the user sees
it, the milestone is marked as delivered. This fits the "invoked norn" pattern -- when the
user consults Urd, Urd shares what it has observed.

### 2. Sentinel state file (for Spindle tooltip)

The sentinel state file (`sentinel-state.json`) gains a `latest_insight` field:

```json
{
    "latest_insight": {
        "type": "FirstBackup",
        "key": "FirstBackup",
        "body": "Your data now rests in two places...",
        "observed_at": "2026-03-31T04:02:15"
    }
}
```

Spindle (tray icon) can display the latest insight in the tooltip, providing a warm
confirmation rather than a cold status line. The field is null when no undelivered insight
exists. The sentinel reads the DB and populates this field -- one authoritative writer.

### 3. Log channel (always)

Insights are always logged at info level when first observed. This provides an audit trail
regardless of whether the user checks status or runs Spindle.

### Writer responsibilities

The **backup command** is the authoritative writer of milestones. It computes insights at
the end of `urd backup` (after the heartbeat is written) and records them in the DB with
`delivered = 0`. It does not dispatch or display them.

The **sentinel** reads the DB during Assess ticks and populates the `latest_insight` field
in the state file from undelivered milestones. It also manages the streak counter
(incrementing on healthy assessments, resetting on degradation).

The **status command** reads undelivered milestones from the DB, displays the latest one,
and marks it as delivered.

This gives each path a single responsibility: backup writes, sentinel surfaces to Spindle,
status surfaces to the terminal. No duplicate dispatchers.

## Module decomposition

### New: `insight.rs`

Pure function module following ADR-108. No I/O.

```rust
/// Insight types -- one variant per milestone in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsightType {
    FirstBackup,
    FirstOffsite,
    AllProtected,
    HealthyStreak,
    NewDrive,
    RecoveryFromUnprotected,
    FirstTransientCleanup,
    ChainBreakRecovered,
    // Future: FirstRestore (requires restore tracking in state.rs)
}

/// Compute which insights should fire given current state and milestone history.
///
/// Pure function: state + history in, insights out. The caller handles
/// persistence and delivery.
pub fn compute_insights(
    current_assessments: &[SubvolAssessment],
    run_history: &RunContext,
    milestone_history: &[MilestoneRecord],
    now: NaiveDateTime,
) -> Vec<InsightMessage>
```

Note: the `streak_state` parameter from the original design is removed. The streak is now
a column in `MilestoneRecord` for the HealthyStreak row, so it arrives with the rest of
the milestone history.

### Modified: `state.rs`

New methods on `StateDb`:

- `record_milestone(insight_key, insight_type, observed_at)` -- INSERT OR IGNORE
- `get_milestones() -> Vec<MilestoneRecord>` -- read all milestones
- `get_undelivered_milestones() -> Vec<MilestoneRecord>` -- read milestones with delivered = 0
- `mark_delivered(insight_key)` -- set delivered = 1
- `update_streak_days(days: i32)` -- update streak_days on the HealthyStreak row
- `reset_streak()` -- set streak_days = 0 on degradation

### Modified: `voice.rs`

New render function for insight messages. The voice module is where the mythological
register lives -- insight body text is generated here, not in `insight.rs`. The pure
computation module returns `InsightType` + context data; `voice.rs` renders the final text.

```rust
pub fn render_insight(insight_type: InsightType, context: &InsightContext) -> String
```

### Modified: `sentinel_runner.rs`

After the existing `execute_assess()`, read undelivered milestones from the DB and
populate the `latest_insight` field in the sentinel state file. Also manage the streak
counter: increment on all-PROTECTED assessments, reset on degradation.

### Modified: `commands/status.rs`

After rendering standard status output, check for undelivered milestones. Display the
latest one and mark it as delivered.

### Modified: `output.rs`

`InsightMessage` as a structured output type, plus `InsightContext` for voice rendering
parameters (drive labels, subvolume names, day counts).

## Anti-patterns

These are the failure modes to avoid:

1. **Tutorial voice.** "Did you know that 3-2-1 means three copies..." -- Urd doesn't
   explain concepts. She observes states.

2. **Generic tips.** "Consider connecting your offsite drive regularly" -- this could
   appear for anyone. Insights must reference specific drives, subvolumes, or dates.

3. **Repetition.** Any insight that fires more than once is a notification, not an insight.
   The milestone gate is the defining characteristic.

4. **Urgency escalation.** Insights are observational. If something is urgent, it's a
   notification (PromiseDegraded, BackupOverdue), not an insight.

5. **Blocking behavior.** Insights never prevent, delay, or gate any operation. They are
   purely observational output.

6. **Volume.** More than one insight per run is overwhelming. If multiple milestones are
   crossed simultaneously, deliver the highest-priority one. Undelivered milestones carry
   forward to the next assessment -- they are never silently lost.

7. **Premature voice.** The mythological register must be earned by relevance. If the
   message would sound just as good as a plain English sentence, use plain English. The
   voice is for moments that carry genuine weight.

## Test strategy

All tests target the pure `compute_insights()` function. No I/O in test code.

### Core invariant tests

- Insight fires exactly once: call `compute_insights()` twice with the same milestone in
  history -- second call returns empty.
- Multiple simultaneous milestones: only highest-priority returned; others remain
  undelivered and carry forward.

### Per-insight tests

- **FirstBackup:** fires when run_count transitions from 0 to 1; does not fire when
  milestone already recorded.
- **FirstOffsite:** fires on first send to offsite-role drive; fires independently per
  drive_label; does not fire for non-offsite drives.
- **AllProtected:** fires when last subvolume reaches PROTECTED; does not fire if any
  subvolume is below PROTECTED.
- **HealthyStreak:** fires when streak_days >= 30; resets when any subvolume degrades;
  does not fire if streak was already delivered.
- **NewDrive:** fires per drive_label; does not re-fire for a drive removed and re-added.
- **RecoveryFromUnprotected:** fires on UNPROTECTED -> PROTECTED transition per subvolume;
  does not fire on AT_RISK -> PROTECTED (that's improvement, not recovery from crisis).
- **FirstTransientCleanup:** fires when transient deletes occur with confirmed external
  copies; does not fire for non-transient subvolumes.

### Edge cases

- State DB unavailable: `compute_insights()` receives empty milestone history, which
  could cause all milestones to re-fire. Guard: the function checks the `run_context`
  (which comes from the heartbeat, not the DB) to avoid firing FirstBackup when the
  system clearly has backup history.
- Sentinel and backup command both running: INSERT OR IGNORE prevents double-recording at
  the DB level. Backup writes milestones; sentinel only reads -- no write contention.
- Sentinel restart: trust existing streak_days if first assessment is all-PROTECTED;
  reset if degraded.

## Effort estimate

Approximately 100-130 lines of new code across `insight.rs` (pure computation), `state.rs`
(milestone persistence), `voice.rs` (rendering), and `commands/status.rs` (display).
12-15 tests.

**Session 1:** InsightType enum, `compute_insights()` pure function, milestone table in
state.rs, voice rendering for the 8 insight types, tests for core invariants.

**Session 2:** Sentinel state file integration, status command integration, streak counter
management, edge case tests.

## Review findings incorporated

Review: `docs/99-reports/2026-03-31-design-o-review.md`

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | Duplicate delivery via sentinel + backup command | Resolved by removing insights from notification pipeline entirely. Backup writes to DB, sentinel reads DB for state file, status command reads DB for display. One writer per path, no duplicate dispatch. |
| 2 | Medium | Streak reset semantics fragile | Defined explicitly: streak resets when ANY subvolume drops below PROTECTED. Streak is a `streak_days` integer column in `insight_milestones`, not a separate table. Sentinel is sole writer. On restart, trust DB if first assessment is healthy; reset if degraded. |
| 3 | Medium | Parameterized insight identity ambiguous | PRIMARY KEY changed to `insight_key` which includes the parameter for parameterized types: `"NewDrive:WD-18TB1"`, `"FirstOffsite:WD-18TB1"`, `"RecoveryFromUnprotected:htpc-home"`. FirstOffsite is parameterized by drive_label so it fires for each offsite drive. |
| 4 | HIGH | Info urgency filtered by default min_urgency: Warning | Design changed: insights removed from notification pipeline entirely. Delivered through `urd status` (latest unacknowledged), sentinel state file (`latest_insight` field for Spindle), and log (always). No urgency filtering applies. |
| 5 | Low | Overlap with Design I's offsite-stale advisory | Insight 9 (OffsiteFreshnessStale) dropped. Design I handles offsite staleness comprehensively. Catalog reduced from 9 to 8 insights. |
| 6 | Voice | Message-by-message voice evaluation | Messages 2, 3 tightened per review. Message 7 noted as needing rework (loom metaphor inconsistent). |
| 7 | Low | Missing FirstRestore milestone | Added as future milestone (noted, not implemented -- requires restore tracking in state.rs). |
| 8 | Low | "No insight on first assessment" ambiguous | Clarified: with insights delivered only through status/Spindle (not notification pipeline), the sentinel's `has_initial_assessment` guard is sufficient. Backup command writes milestones but does not dispatch, so no suppression needed there. |
| 9 | Low | One-per-run limit causes silent loss | Relaxed: undelivered milestones carry forward to next assessment. Never silently lost. |

Structural changes from review:
- Eliminated `insight_streaks` table. Streak tracking uses `streak_days` column in `insight_milestones`.
- Removed `InsightMessage` from notification dispatch system entirely.
- Added `commands/status.rs` to modified modules (displays latest undelivered insight).
- `compute_insights()` signature simplified (no separate `streak_state` parameter).

## Open questions (remaining after review)

1. **Streak duration:** 30 days for HealthyStreak is arbitrary. Too short feels unearned;
   too long and most users never see it. Is 30 days right?

2. **Voice rendering location:** Design puts body text in voice.rs, with insight.rs
   returning type + context. Alternative: insight.rs returns the full rendered text,
   keeping voice.rs focused on status/plan/backup rendering. Which is more consistent
   with the existing architecture?
