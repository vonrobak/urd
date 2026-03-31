# Architectural Review: Phase 3 — Advisory System + Retention Preview

**Reviewer:** arch-adversary
**Date:** 2026-03-31
**Scope:** Design review (no code yet)
**Documents reviewed:**
- `docs/95-ideas/2026-03-31-design-phase3-advisory-retention.md` (orchestration)
- `docs/95-ideas/2026-03-31-design-i-redundancy-recommendations.md` (6-I)
- `docs/95-ideas/2026-03-31-design-n-retention-policy-preview.md` (6-N)
- Current code: `awareness.rs`, `output.rs`, `voice.rs`, `retention.rs`

---

## 1. Executive Summary

The Phase 3 design is well-structured, correctly separates advisory from enforcement, and keeps both features as pure functions consistent with Urd's architecture. The main risk is the `Vec<String>` to `Vec<RedundancyAdvisory>` migration on `StatusAssessment`, which the orchestration doc correctly identifies but whose blast radius is underestimated. The notification cooldown logic in 6-I introduces hidden state that needs careful design to avoid masking genuine new problems.

---

## 2. What Kills You (Catastrophic Failure Proximity)

**Verdict: No catastrophic proximity.** Neither feature modifies snapshots, drives retention decisions, or interacts with the deletion pipeline. Both are read-only advisory/preview functions. The design explicitly states advisories do not block backups or degrade promise states (enforcement belongs to 6-E, which is separate).

One indirect path to watch: the orchestration doc mentions that 6-E "could use I's detection logic to decide when to degrade a promise." If a future integration wires advisory detection into promise degradation, and a bug in advisory detection causes false "all clear," it could prevent degradation that should happen. This is not in the current design, but the relationship table plants the seed. Flagged as a design tension, not a finding.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4/5 | Cascading retention formula verified against code. Notification cooldown has an edge case (see S1). |
| Security | 5/5 | No attack surface. Read-only pure functions. No new privilege escalation paths. |
| Architectural Excellence | 4/5 | Clean module boundaries, proper pure-function pattern. Migration atomicity is the weak point. |
| Systems Design | 4/5 | Sentinel integration and Spindle contract are thoughtful. Cooldown state persistence is underspecified. |

**Overall: 17/20 — Approve with findings.**

---

## 4. Design Tensions

### T1: Advisory Migration Atomicity vs. Test Churn

The `Vec<String>` to `Vec<RedundancyAdvisory>` change on `StatusAssessment` breaks every test that constructs a `StatusAssessment` (the orchestration doc acknowledges this). The tension: doing it atomically is architecturally correct but creates a large diff that touches many test files. The alternative -- keeping both fields temporarily -- creates the parallel-systems problem the design explicitly wants to avoid. The atomic approach is right; the cost is accepted.

### T2: Notification Cooldown Complexity vs. User Experience

The cooldown/suppression logic (48h resolution suppression, 7-day re-detection suppression) adds hidden temporal state to the notification system. This is justified by a real UX problem (monthly drive cycling noise), but it introduces a class of bug where genuine new problems are silently suppressed. The tension is between notification hygiene and notification reliability.

### T3: Calibrated vs. Uncalibrated Estimation Honesty

The retention preview correctly refuses to show byte estimates when uncalibrated. But even calibrated estimates use `du -sb` which measures apparent size, not BTRFS exclusive usage. The design acknowledges this with a disclaimer, but the 5-10x gap between the displayed number and reality is large enough that users may still make poor decisions based on the "calibrated" upper bound. The tension is between showing something useful and showing something misleading.

---

## 5. Findings

### Significant

#### S1: Cooldown suppression can mask genuinely new offsite problems

**Location:** 6-I design, notify.rs section, cooldown for cyclic advisories.

The 7-day re-detection suppression after resolution works for the normal cycle: drive departs, ages past 30 days, returns, departs again. But consider: drive returns (resolving `OffsiteDriveStale`), user swaps the wrong drive back in (different drive, not the offsite one), and the original offsite drive is still away. The `(kind, subvolume)` key would suppress the re-detection because the same kind just resolved, even though the underlying condition is different (a different drive is now stale).

**Recommendation:** Key the suppression on `(kind, subvolume, drive_label)`, not `(kind, subvolume)`. This preserves the cycling-noise benefit while allowing detection of a different drive's staleness. The `RedundancyAdvisory` already carries `drive: Option<String>`, so the data is available.

**Severity:** Significant. Not catastrophic (advisories don't affect backups), but a user could miss a real gap for 7 days due to a suppression keyed too broadly.

#### S2: Cooldown state persistence across sentinel restarts

**Location:** 6-I design, notify.rs section.

The cooldown logic requires tracking when advisories were first detected and when they last resolved. The design does not specify where this state lives. If it is in-memory only, a sentinel restart (systemd restart, reboot) resets all cooldowns, causing a burst of notifications for all existing conditions. If it is persisted, where? The sentinel state file? A separate file? SQLite?

This is not a correctness issue -- the worst case is extra notifications after a restart -- but it should be specified at design time to avoid an implementation-time decision that might pick the wrong storage.

**Recommendation:** Persist cooldown state in the sentinel state file alongside `advisory_summary`. Add a `cooldowns: HashMap<(kind, subvolume, drive_label), CooldownEntry>` with detection/resolution timestamps. This is small, JSON-serializable, and lives where sentinel state already lives.

**Severity:** Significant. Unspecified state persistence is a design gap that will cause either notification spam (in-memory) or an ad-hoc storage decision (implementation time).

### Moderate

#### M1: The stringly-typed advisory migration boundary is unclear

**Location:** 6-I design, advisory type 2 migration note.

The design says: "Non-redundancy advisories (clock skew, send_enabled without drives) remain stringly-typed since they are operational, not redundancy-related." But `SubvolAssessment.advisories` is `Vec<String>`, and the migration replaces this field on `StatusAssessment` with `Vec<RedundancyAdvisory>`. What happens to the non-redundancy advisories?

Options: (a) `StatusAssessment` gets two fields (`redundancy_advisories: Vec<RedundancyAdvisory>` and `advisories: Vec<String>`), (b) the non-redundancy advisories are migrated to a separate typed enum at the same time, or (c) they are dropped. The design does not specify.

**Recommendation:** Keep both fields on `StatusAssessment` during this phase. The `advisories: Vec<String>` field continues to carry operational advisories. The new `redundancy_advisories: Vec<RedundancyAdvisory>` field carries the structured ones. This avoids scope creep (migrating all advisory types at once) while maintaining the clean separation. `voice.rs` renders both sections. The `SubvolAssessment` struct gets the same treatment: keep `advisories: Vec<String>` for operational, add `redundancy_advisories: Vec<RedundancyAdvisory>` for structured.

**Severity:** Moderate. If not resolved before implementation, the developer will face an ambiguous choice mid-build.

#### M2: `du -sb` in the command handler breaks pure-function chain

**Location:** 6-N design, module changes.

The design says `commands/retention_preview.rs` will "optionally measure snapshot sizes" via `du -sb`. This is I/O in the command handler, which is fine architecturally (command handlers are the I/O boundary). But the design should note that this measurement can be slow (seconds for large snapshot directories) and should have a timeout or skip mechanism. A user running `urd retention-preview --all` with 5 subvolumes and large snapshots could wait 30+ seconds for size measurement.

**Recommendation:** Add a `--no-size` flag (or make size measurement opt-in with `--estimate-size`) so users can get instant recovery-window output without waiting for `du -sb`. Default behavior: attempt measurement with a per-subvolume timeout (e.g., 5 seconds), fall back to `Unknown` if it takes too long.

**Severity:** Moderate. Not a correctness issue, but a UX gap that could make the command feel sluggish.

#### M3: Vocabulary mapping incomplete for existing advisories

**Location:** Orchestration doc, vocabulary adjustments section.

The orchestration doc lists vocabulary mappings (`"chain"` to `"thread"`, `"mounted"` to `"connected"`, etc.) but the existing `awareness.rs` code (line 295) uses `"consider cycling"` and `"offsite drive"` in its stringly-typed advisories. The migration from stringly-typed to `RedundancyAdvisory` will rewrite these messages, but the orchestration doc should explicitly list the old strings being retired and their structured replacements. Without this, the implementer must audit awareness.rs manually to find all the strings that need migration.

**Recommendation:** Add a migration table to the orchestration doc: old string pattern, new `RedundancyAdvisoryKind`, and new voice rendering. There are at least three: (1) `"offsite drive {} last sent {} days ago -- consider cycling"` becomes `OffsiteDriveStale`, (2) `"offsite copy stale -- resilient promise degraded"` (from `overlay_offsite_freshness`) stays as-is (it is an enforcement message from 6-E, not an advisory), (3) `"send_enabled but no drives configured"` stays stringly-typed (operational, not redundancy).

**Severity:** Moderate. Missing this mapping risks leaving orphaned string advisories or migrating the wrong ones.

### Minor

#### N1: `AdvisorySummary.worst` is `Option<String>` instead of typed

**Location:** 6-I design, sentinel state file section.

`worst: Option<String>` serializes the advisory kind as a string for Spindle consumption. This means Spindle (or any consumer) must parse the string back to understand severity ordering. If a new advisory kind is added later, old Spindle versions will see an unknown string. Using `Option<RedundancyAdvisoryKind>` with serde `rename_all = "snake_case"` would be type-safe internally while producing the same JSON output. The JSON contract is identical; the Rust side is stronger.

**Recommendation:** Use `Option<RedundancyAdvisoryKind>` instead of `Option<String>`. The serde output is the same (`"no_offsite_protection"`, etc.), but Rust code that reads the state file back gets type safety.

#### N2: Schema version bump to v3 lacks migration story

**Location:** 6-I design, sentinel state file section.

The design bumps schema to v3 and notes backward compatibility (v2 readers ignore the field). But what about v3 readers encountering a v2 file? The `skip_serializing_if` + `default` pattern handles deserialization, but the design should explicitly state: "v3 code reading a v2 state file sees `advisory_summary: None`, which means 'unknown, not zero' as specified." This is implied but worth stating explicitly for the implementer.

#### N3: Retention preview `--compare` default direction unstated

**Location:** 6-N design, CLI design section.

`--compare` shows transient vs. graduated comparison. For a graduated subvolume, it shows what switching to transient would save/lose. For a transient subvolume, it shows what switching to graduated would cost/gain. But the default graduated policy for comparison is not specified. Which graduated settings does it use? The subvolume's configured graduated policy (which does not exist for a transient subvolume)? A system default? The design should state: for transient subvolumes, `--compare` uses the default graduated retention values from `[defaults]` in config.

### Commendation

#### C1: Advisory/enforcement separation is exactly right

The design's relationship between 6-I (advise) and 6-E (enforce) is the correct architecture. The explicit documentation of intentional overlap at different layers (config-time vs. runtime) prevents future consolidation that would lose either the early-catch or persistent-surfacing property. This is mature architectural thinking.

#### C2: "Absent means unknown, not zero" for schema evolution

The sentinel state file design's explicit statement that `advisory_summary: None` means "not computed" rather than "no advisories" is the correct null-semantics decision. This prevents a v2 Urd from falsely appearing advisory-free to a v3 Spindle. More designs should include this kind of explicit null-semantics specification.

---

## 6. The Simplicity Question

**Is this design as simple as it could be?**

Mostly yes. The core of both features is pure functions with clean inputs and outputs. The complexity is concentrated in two areas:

1. **Notification cooldown** (6-I): The 48h/7d suppression logic adds temporal state and edge cases. Consider whether the simpler alternative -- deduplicate by only notifying on state transitions, with no cooldown -- is sufficient. The monthly cycling pattern would produce exactly two notifications per cycle (gap detected, gap resolved), which may be acceptable. The cooldown saves one notification per cycle at the cost of hidden state and suppression bugs.

2. **Transient comparison** (6-N): Computing both policies and diffing adds code but provides clear user value. The `--compare` flag keeps it opt-in. This complexity is justified.

**Verdict:** The cooldown logic is the one area where simplification should be seriously considered. Everything else is proportional to the problem.

---

## 7. For the Dev Team (Prioritized Action Items)

1. **Resolve M1 before building.** Decide whether `StatusAssessment` gets two advisory fields or one. The two-field approach (keep `advisories: Vec<String>` for operational, add `redundancy_advisories: Vec<RedundancyAdvisory>`) is recommended. This decision affects the migration blast radius.

2. **Key cooldown suppression on `(kind, subvolume, drive_label)`** (S1). Prevents masking genuinely new problems after a different drive's advisory resolves.

3. **Specify cooldown state persistence** (S2). Recommend sentinel state file. Decide before building.

4. **Add migration table for existing string advisories** (M3). List every stringly-typed advisory in `awareness.rs` and its fate: migrate to structured, keep as string, or remove.

5. **Consider dropping cooldown entirely** (simplicity question). Evaluate whether bare state-transition notifications (no suppression) are acceptable for the drive cycling use case. If two notifications per monthly cycle is tolerable, the cooldown can be deferred to "add if needed" rather than "build preemptively."

6. **Address `du -sb` performance for `--all`** (M2). Add timeout or opt-out flag.

7. **Use typed `Option<RedundancyAdvisoryKind>` for `worst`** (N1). Free type safety.

---

## 8. Open Questions

1. **Should the cooldown logic ship in Phase 3, or be deferred until notification fatigue is observed?** Building it preemptively adds ~20 lines of code and ~5 tests, plus persistent state. Deferring it means accepting 2 notifications per monthly drive cycle until the next release. The YAGNI argument is strong here.

2. **What graduated policy does `--compare` use for a transient subvolume?** The design needs to specify the source of the comparison policy. Options: `[defaults]` graduated values, a hardcoded "typical" policy, or require the user to specify via flags.

3. **Should `overlay_offsite_freshness()` advisory messages (line 739 of awareness.rs) be migrated into the structured system?** These are enforcement messages ("resilient promise degraded"), not advisory guidance. The design implies they stay stringly-typed, but this should be an explicit decision since they share vocabulary with the new `OffsiteDriveStale` advisory.
