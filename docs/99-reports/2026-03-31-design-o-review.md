# Design Review: Progressive Disclosure of Redundancy Concepts (Idea O)

**Design under review:** `docs/95-ideas/2026-03-31-design-o-progressive-disclosure.md`
**Reviewer:** arch-adversary
**Date:** 2026-03-31

---

## Scores

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Core invariants well-defined; duplicate delivery path and streak reset semantics need tightening |
| Security | 9/10 | No privilege escalation, no I/O in pure module, INSERT OR IGNORE is sound |
| Architectural Excellence | 9/10 | Clean separation of concerns, follows ADR-108 faithfully, voice in voice.rs |
| Systems Design | 7/10 | Streak table arguably overengineered; config-change identity problem unaddressed; dual delivery path under-specified |

---

## Finding 1: Duplicate delivery via sentinel + backup command (Severity: Medium)

The design specifies insights fire in both `sentinel_runner.rs` (during Assess) and at the end of `urd backup`. The INSERT OR IGNORE prevents double-recording in the DB, but it does not prevent double-dispatch.

Consider the sequence:
1. `urd backup` completes at 04:02. It computes insights, dispatches "FirstBackup" via desktop notification, and records the milestone.
2. The sentinel's next Assess tick fires at 04:03 (triggered by heartbeat mtime change). It reads milestone history, sees FirstBackup already recorded, and correctly does not re-fire.

This works. But the reverse order is the problem:
1. Sentinel ticks at 04:01, sees run_count == 0. No insight.
2. `urd backup` completes at 04:02. Records FirstBackup milestone, dispatches notification.
3. Sentinel ticks at 04:03. Sees new heartbeat. Reads milestone history -- FirstBackup already present. Correct.

Actually, the real race is: what if `urd backup` and sentinel are both computing insights at the exact same moment, before either has written to the DB? Both see empty milestone history, both compute FirstBackup, both dispatch, both INSERT OR IGNORE (one wins, one is ignored). The user sees two desktop notifications.

**Recommendation:** The design should specify which path is authoritative. The simplest fix: the backup command records the milestone but does not dispatch -- it sets `delivered_at = NULL`. The sentinel is the sole dispatcher. If the sentinel is not running, the insight is recorded but not shown until next sentinel tick (or next backup that checks for undelivered milestones). Alternatively, the backup command dispatches AND records, and the sentinel skips insights entirely, treating the backup command as the primary source. Pick one path. Do not have two independent dispatchers for the same event type.

---

## Finding 2: The `insight_streaks` table is justified but the reset semantics are fragile (Severity: Medium)

The design correctly separates streaks from milestones -- milestones are immutable (INSERT OR IGNORE), streaks are mutable (reset on degradation). A single table cannot serve both invariants. The separate table is the right call.

However, the reset semantics are under-specified. "When any subvolume drops below PROTECTED, the row is deleted" raises questions:

- **Who deletes it?** The sentinel during Assess? The backup command? Both? This is the same dual-path problem as Finding 1.
- **Granularity of "drops below PROTECTED."** A single AT_RISK subvolume at 04:00 that recovers by 04:15 resets the streak. Was the streak really broken? The design doesn't specify whether the streak tracks continuous PROTECTED across every assessment, or whether it uses daily granularity (all assessments within a calendar day were PROTECTED).
- **Startup gap.** If the sentinel restarts, there's no assessment history. The streak in the DB says started 20 days ago, but the sentinel can't verify that the last 20 days were actually all-PROTECTED -- it only sees the current state. Should it trust the DB?

**Recommendation:** Define "streak broken" as: any assessment where at least one subvolume is not PROTECTED. The sentinel is the sole writer (same as Finding 1). On startup, if a streak exists in the DB and the first assessment shows all-PROTECTED, trust the DB's `started_at` -- the backup command's heartbeat file provides continuity evidence. If the first assessment shows degradation, reset the streak. Document this explicitly.

---

## Finding 3: Config changes and the at-most-once identity problem (Severity: Medium)

The at-most-once invariant is enforced by `insight_type TEXT PRIMARY KEY`. This works for singletons (FirstBackup, AllProtected, HealthyStreak) but creates identity ambiguity for parameterized insights:

- **NewDrive:** Keyed by what? If the key is `"NewDrive"`, you can only ever acknowledge one new drive. The design says "first send to a drive label not previously seen in milestone history," implying the key should be `"NewDrive:{drive_label}"`. This needs to be explicit.
- **RecoveryFromUnprotected:** Same issue. The key should include the subvolume name, otherwise only the first-ever recovery across all subvolumes fires.
- **Drive removal and re-addition:** If a user removes `WD-18TB1` from config, adds a new drive `WD-18TB2`, later re-adds `WD-18TB1` -- should "NewDrive:WD-18TB1" fire again? The milestone table says no (row exists). This is probably correct (the drive isn't new), but the design should state this explicitly.
- **FirstOffsite:** If the user's only offsite drive is removed and a different drive is later designated offsite, should FirstOffsite fire again? The current design says no (singleton key). This seems wrong -- the new offsite drive is a genuinely different achievement.

**Recommendation:** Define the primary key format explicitly for each InsightType. Singletons use the variant name. Parameterized insights use `"{VariantName}:{parameter}"`. Consider whether FirstOffsite should be a singleton or parameterized by drive label.

---

## Finding 4: Info urgency vs. the default min_urgency filter (Severity: High)

This is the most operationally significant issue.

The notification config defaults to `min_urgency: Warning`. The design specifies all insights carry `Urgency::Info`. This means **no user will ever see an insight through notification channels unless they have explicitly lowered their min_urgency to Info.**

The existing `PromiseRecovered` notification is also Info-level and suffers the same problem -- but recovery notifications are arguably less important than milestone insights. For progressive disclosure to fulfill its purpose (the user's "really cool -- keep it subtle and smart"), the insights need to reach the user.

Options:
1. **Accept this.** Insights are a bonus for users who opt into Info-level notifications. Most users never see them. This undermines the feature's purpose.
2. **Add a separate insight delivery path** that bypasses the min_urgency filter. Insights are not alerts -- they're a different category. The notification system could have an `insight_enabled: bool` flag independent of min_urgency.
3. **Deliver insights through a different channel entirely.** For example, only through the sentinel state file (Spindle tooltip) and `urd status` output, never through desktop notifications. This avoids the urgency conflict entirely.

**Recommendation:** Option 3 is most aligned with the design philosophy. Insights are observational, not alerting. Routing them through the notification pipeline (which is designed for alerts) creates a category mismatch. Display insights in `urd status` output and the Spindle tooltip. Log them. Do not push them as desktop notifications. This also eliminates the duplicate delivery problem from Finding 1.

---

## Finding 5: Overlap between insight 9 (OffsiteFreshnessStale) and I's offsite-stale advisory (Severity: Low)

Design I's `OffsiteDriveStale` advisory fires on every `urd status` invocation when the offsite drive exceeds 30 days. Design O's insight 9 fires once on the first crossing of the AT_RISK threshold (which may be a different duration).

The design acknowledges this overlap but the distinction is muddled:
- I uses a hardcoded 30-day threshold. O uses "the configured AT_RISK threshold" (which comes from awareness.rs and may differ).
- I is a Warning-level advisory (displayed in status). O is an Info-level insight (one-time notification). But the I design doesn't assign urgency to advisories -- it uses a separate `RedundancyAdvisoryKind` ordering.
- Both ultimately tell the user the same thing: your offsite drive is stale.

**Recommendation:** Drop insight 9 entirely. Design I handles offsite staleness comprehensively as a recurring advisory. A one-time "first time your offsite goes stale" insight adds noise without adding understanding. The user doesn't need to be taught that staleness is bad -- Design I's advisory makes that clear every time they check status. This reduces the catalog from 9 to 8 insights, which is cleaner.

---

## Finding 6: Voice evaluation -- message by message

The user's scoring feedback: "keep it subtle and smart. Urd is very wise about it." The anti-pattern from the brainstorm scoring: "Don't be cheesy -- the mythological framing must earn its keep through genuine insight."

**1. First backup** -- "Your data now rests in two places. If one thread frays, the other holds."
Earned. Concrete, brief, maps to the actual state change. The "thread" metaphor is load-bearing (Urd spins fate). No excess.

**2. First offsite** -- "Your data has crossed a threshold -- it now survives beyond these walls. Fire, flood, theft: the thread endures elsewhere."
Mixed. The first sentence is earned. The second sentence ("Fire, flood, theft") is instructional rather than observational -- it explains *why* offsite matters rather than observing what happened. Tighten to one sentence: "Your data has crossed a threshold -- it endures beyond these walls."

**3. All protected** -- "Every thread is woven tight. All your data rests within its promised bounds."
Earned but redundant. Both sentences say the same thing. Pick one: "Every thread is woven tight."

**4. Healthy streak** -- "Thirty days, unbroken. Your data rests well."
Excellent. Quiet confidence. The brevity is the voice.

**5. New drive** -- "A new thread joins the weave. {drive_label} now carries your data forward."
Solid. Specific (uses drive label). The metaphor is consistent.

**6. Recovery from UNPROTECTED** -- "The weave is restored. {subvolume_name} is whole again."
Earned. Brief, specific, marks a genuine return from danger.

**7. First transient cleanup** -- "Local threads released -- your data lives safely on {drive_label}. The loom is lighter; nothing was lost."
Over-written. "The loom is lighter" introduces a new metaphor (loom) that doesn't appear elsewhere. The second sentence also feels like it's pre-empting anxiety rather than observing state. Tighten: "Local threads released -- your data lives safely on {drive_label}."

**8. Chain break recovered** -- "The chain mended. Incremental sends resume -- swift where they were labored."
Solid. The chain metaphor is literal (incremental chain), not mythological, which is appropriate for an operational insight. The "swift where they were labored" is concrete.

**9. Offsite freshness stale** -- see Finding 5 (recommend dropping).

**Summary:** 4 excellent (1, 4, 5, 6), 2 good with minor tightening (2, 3), 1 needs rework (7), 1 recommend dropping (9). The overall register is well-calibrated. The design's own anti-pattern section ("short is wise; long is preachy") is the right lens -- apply it more aggressively to messages 2, 3, and 7.

---

## Finding 7: Missing milestone -- first successful `urd get` / `urd restore` (Severity: Low)

The catalog covers the backup journey but not the recovery journey. The first time a user successfully restores a file is arguably the most emotionally significant milestone -- it's the moment backups prove their worth. This is the "you will be glad you had her acquaintance" moment from the user's voice philosophy.

**Recommendation:** Consider adding a `FirstRestore` milestone. Trigger: first successful file retrieval via `urd get` or `urd restore`. Voice: something that acknowledges the well (Urd's Well of fate) returning what was asked for. This would require the restore command to record the event in state.rs, which is a small addition. Do not over-invest if restore tracking doesn't exist yet -- note it as a future milestone.

---

## Finding 8: "No insight fires on first assessment" needs careful definition (Severity: Low)

The test strategy says "No insight fires on first assessment (no previous state to compare)." But several insights don't require previous state -- FirstBackup fires when `run_count == 1`, which is knowable from the current state alone.

The real invariant is: **insights that detect state transitions require a previous state to compare against.** Insights that detect absolute thresholds (FirstBackup, AllProtected, HealthyStreak) can fire on any assessment.

What the design probably means: "No insight fires on the sentinel's first assessment after startup, to avoid a burst of insights when the sentinel restarts." This is the same suppression pattern used for promise notifications (`has_initial_assessment`). If so, state this explicitly and implement it the same way -- skip insight computation until `has_initial_assessment` is true.

But this creates a problem: if the backup command is also a delivery path (Finding 1), it doesn't have a `has_initial_assessment` guard. The backup command always fires insights on its first and only assessment.

**Recommendation:** Clarify the suppression rule. If insights are only delivered through the sentinel (per Finding 4's recommendation), the existing `has_initial_assessment` guard is sufficient.

---

## Finding 9: One-per-run limit is too conservative for parameterized insights (Severity: Low)

The anti-pattern section says "no more than one insight per run." The open question asks if this is too conservative. It is, but only for a narrow case: when a user connects a new offsite drive and sends to multiple subvolumes for the first time, both `FirstOffsite` and `NewDrive` could fire simultaneously. Delivering only the "highest-priority" one means the other is silently dropped (it's already recorded as a milestone, so it never fires).

**Recommendation:** Change the rule to: at most one insight per assessment tick, but if a milestone was recorded without delivery (due to the one-per-run limit), it can be delivered on the next tick. This requires the `delivered_at` column from the design. Alternatively, allow up to two insights per tick if they reference different milestone categories (journey vs. operational). The simpler fix: just allow multiple insights. The design's own channel (Spindle tooltip, `urd status`) can show them all without overwhelming the user.

---

## Summary

The design is architecturally sound and well-aligned with Urd's character. The pure-function module pattern, the at-most-once invariant via PRIMARY KEY, and the separation of computation (insight.rs) from rendering (voice.rs) all follow established project patterns correctly.

The three issues that need resolution before implementation:

1. **Dual delivery path** (Finding 1 + Finding 4): Pick one authoritative dispatcher. Recommend the sentinel as sole dispatcher, with backup command only recording milestones. Or better: route insights through status/Spindle only, not through the notification pipeline at all.
2. **Parameterized insight identity** (Finding 3): Define the PRIMARY KEY format for each InsightType variant explicitly. NewDrive and RecoveryFromUnprotected must include their parameter in the key.
3. **Info urgency vs. min_urgency default** (Finding 4): Insights at Info level will be filtered by the default Warning threshold. Either bypass the filter, use a separate delivery channel, or accept that most users won't see insights via notifications.

The milestone catalog is strong. Consider dropping insight 9 (offsite freshness stale) as redundant with Design I, and adding FirstRestore as a future milestone. The voice register is mostly well-calibrated -- tighten messages 2, 3, and 7 per Finding 6.
