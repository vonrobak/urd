# Architectural Review: 6-I Redundancy Recommendations Implementation

**Reviewer:** arch-adversary
**Date:** 2026-04-01
**Scope:** Implementation review (post-build, post-simplify)
**Commit range:** Feature branch diff for 6-I redundancy advisories
**Documents reviewed:**
- `docs/95-ideas/2026-03-31-design-i-redundancy-recommendations.md` (design)
- `docs/99-reports/2026-03-31-design-phase3-advisory-retention-review.md` (prior review)
- Source: `awareness.rs`, `output.rs`, `voice.rs`, `sentinel_runner.rs`, `commands/status.rs`
- Full diff of all changed files

---

## 1. Executive Summary

The implementation is clean, correctly scoped, and faithful to the design. The pure-function
pattern is maintained. The feature is entirely read-only advisory output with zero proximity
to the deletion pipeline. The /simplify pass caught one real logic bug (NoOffsiteProtection
global vs. per-subvolume scoping) and improved code quality.

The main finding is that `SinglePointOfFailure` and `OffsiteDriveStale` share a scoping
defect inherited from `assess()`: they operate on `assessment.external`, which contains ALL
config drives, not just the subvolume's effective drives when `subvol.drives` scopes the
subvolume to a subset. This means SPOF can undercount (not fire when it should) and
OffsiteDriveStale can fire for drives the subvolume does not use.

---

## 2. What Kills You (Catastrophic Failure Proximity)

**Verdict: No catastrophic proximity.** This feature produces advisory text from config and
assessment state. It does not:
- Touch the deletion pipeline
- Modify snapshots or pin files
- Influence retention decisions
- Degrade promise states
- Block backups

The `compute_redundancy_advisories()` function is pure, takes `&Config` and
`&[SubvolAssessment]` by reference, and returns `Vec<RedundancyAdvisory>`. No I/O, no
mutation. The sentinel state file change is additive (new optional field, `skip_serializing_if`).

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4/5 | One scoping defect (S1). Threshold alignment is intentional but underdocumented (M1). |
| Security | 5/5 | No attack surface. Read-only. No new privilege paths. |
| Architectural Excellence | 5/5 | Pure function, clean module boundaries, correct placement in awareness.rs. |
| Systems Design | 4/5 | State file evolution is handled well. Minor semantic question on AdvisorySummary (M2). |

**Overall: 18/20 -- Approve with findings.**

---

## 4. Design Tensions

### T1: Assessment Scoping vs. Redundancy Accuracy

`assess()` iterates `config.drives` for all subvolumes without filtering by `subvol.drives`.
This is the existing awareness model's behavior -- it computes drive assessments for all
drives regardless of per-subvolume scoping. The planner (plan.rs line 176) respects
`subvol.drives` by skipping non-allowed drives. The awareness model does not.

This creates a tension: `compute_redundancy_advisories()` correctly scoped
`NoOffsiteProtection` to the subvolume's effective drives (the /simplify fix), but
`SinglePointOfFailure` and `OffsiteDriveStale` still read from `assessment.external` which
reflects all drives, not the scoped set. Fixing this properly requires either (a) scoping
`assess()` itself to filter drives, which is a larger change with blast radius across the
awareness model, or (b) applying the same `subvol.drives` filter inside
`compute_redundancy_advisories()` for the SPOF and stale checks.

### T2: Threshold Alignment Between Enforcement and Advisory

`overlay_offsite_freshness()` uses `OFFSITE_AT_RISK_DAYS = 30` to degrade resilient
promises. `compute_redundancy_advisories()` uses `OFFSITE_STALE_ADVISORY_DAYS = 30` for
the advisory. These are the same value but serve different purposes (enforcement vs. gentle
guidance) and are defined as separate constants. This is currently fine but will diverge if
either threshold changes independently.

---

## 5. Findings

### Significant

#### S1: SinglePointOfFailure and OffsiteDriveStale ignore per-subvolume drive scoping

**Location:** `awareness.rs` lines 835-858 (SPOF) and 813-832 (stale).

Both checks operate on `assessment.external`, which contains `DriveAssessment` entries for
ALL config drives, not just the subvolume's effective drives. A subvolume with
`drives = ["drive-a"]` that has two global drives configured (`drive-a`, `drive-b`) will
show `assessment.external` with TWO entries. The SPOF check counts 2 non-test drives and
does not fire, even though the subvolume only uses 1 drive.

Similarly, `OffsiteDriveStale` could fire for an offsite drive that a subvolume is not
scoped to use, producing a misleading advisory.

The `/simplify` pass correctly fixed `NoOffsiteProtection` to use `subvol.drives` scoping
(line 789), but the same pattern was not applied to the other two checks.

**Impact:** False negatives for SPOF (advisory doesn't fire when it should). False positives
for OffsiteDriveStale (advisory fires for drives the subvolume doesn't use). Since both are
advisory-only, this cannot cause data loss, but it undermines user trust in the advisory
system.

**Recommendation:** Apply the same `subvol.drives` filtering to the SPOF and stale checks.
For SPOF, filter `assessment.external` by the subvolume's effective drives before counting.
For stale, only check offsite drives that belong to the subvolume's effective drive set.
Pattern:

```rust
let effective_drives: Vec<&DriveAssessment> = match &subvol.drives {
    Some(allowed) => assessment.external.iter()
        .filter(|d| allowed.iter().any(|a| a == &d.drive_label))
        .collect(),
    None => assessment.external.iter().collect(),
};
```

Apply this filter before both the SPOF count and the stale iteration.

**Severity:** Significant. Incorrect advisory output for users with per-subvolume drive
scoping. Not catastrophic (advisory-only), but the fix is straightforward and the scoping
pattern already exists in the NoOffsiteProtection check.

### Moderate

#### M1: Threshold gap between old and new offsite advisory (7-day to 30-day)

**Location:** `awareness.rs` line 760 (`OFFSITE_STALE_ADVISORY_DAYS = 30`), old code at
diff line 42 (7-day threshold).

The old "consider cycling" advisory fired at 7 days. The new `OffsiteDriveStale` fires at
30 days. The design explicitly chose this: "the 7-day variant was too aggressive for an
offsite rotation pattern." This is a correct decision.

However, `overlay_offsite_freshness()` degrades resilient promises to AtRisk at exactly
30 days (`OFFSITE_AT_RISK_DAYS = 30`). The advisory also fires at 30 days
(`OFFSITE_STALE_ADVISORY_DAYS = 30`). This means the advisory and the enforcement fire
simultaneously -- the user sees both the promise degradation and the stale advisory at the
same moment.

For the 8-30 day range, the user now gets no advisory at all (the old one would have fired
at 7 days). For non-resilient subvolumes with offsite drives, there is no enforcement layer
-- the advisory is the only signal, and it is silent for the first 30 days. Whether this is
a gap depends on expected drive cycling frequency. If the user cycles monthly, 30 days is
correct. If weekly, they lose the early reminder.

**Recommendation:** Add a brief comment near `OFFSITE_STALE_ADVISORY_DAYS` documenting the
intentional alignment with `OFFSITE_AT_RISK_DAYS` and the rationale for dropping the 7-day
threshold. This prevents a future developer from reintroducing a lower threshold without
understanding the design decision.

**Severity:** Moderate. Not a bug -- an intentional design change that should be documented
in code.

#### M2: AdvisorySummary semantics when only informational advisories exist

**Location:** `output.rs` lines 118-135 (`from_advisories()`).

When the advisory list contains only `TransientNoLocalRecovery` advisories,
`from_advisories()` returns `Some(AdvisorySummary { count: 0, worst: Some(TransientNoLocalRecovery) })`.

This means `count == 0` but `worst.is_some()`. A Spindle consumer checking
`summary.count > 0` for badge display would correctly show no badge. But a consumer checking
`summary.worst.is_some()` would think there is an advisory worth displaying. The semantics
are ambiguous: does `worst` represent "worst of the non-informational set" or "worst of all
advisories"?

The design says: "Informational advisories do not contribute to advisory counts that affect
Spindle badge state." The current implementation puts them in `worst` but not in `count`.

**Recommendation:** Either (a) exclude informational advisories from `worst` as well (filter
before `min()`), so `count == 0` implies `worst == None`, or (b) document explicitly that
`worst` includes informational advisories and consumers should check `count > 0` before
interpreting `worst`. Option (a) is cleaner -- it makes `None` mean "nothing actionable."

**Severity:** Moderate. Spindle does not exist yet, so no current consumer is affected. But
this is an API contract decision that should be made now, before consumers exist.

#### M3: `_now` parameter is unused

**Location:** `awareness.rs` line 773.

The `now` parameter is prefixed with `_` (unused). The function was designed to need `now`
for the offsite staleness check, but the implementation reads `last_send_age` from
`DriveAssessment`, which is already computed relative to `now` by `assess()`. So `now` is
redundant here.

Keeping the parameter for future use (e.g., configurable thresholds that depend on calendar
time) is defensible, but the `_` prefix signals "we don't need this" rather than "we'll need
this later."

**Recommendation:** Either remove the parameter now (YAGNI) or remove the `_` prefix and
add a comment explaining the forward reservation. The current state is ambiguous.

**Severity:** Moderate. Not a bug, but creates confusion about the function's contract.

### Minor

#### N1: No test for guarded subvolumes being excluded

**Location:** Test suite, `awareness.rs` lines 3017-3355.

The design specifies: "Guarded subvolumes (local-only by design) should not trigger
SinglePointOfFailure or NoOffsiteProtection." This is naturally handled because guarded
subvolumes have `send_enabled = false`, and all checks gate on `subvol.send_enabled`.
However, there is no explicit test verifying this exclusion.

**Recommendation:** Add a test with a guarded subvolume to document the exclusion as a
tested invariant, not an incidental property.

#### N2: `redundancy_advisories: Vec::new()` populated in `assess()` then overwritten

**Location:** `awareness.rs` lines 346 and `commands/status.rs` line 90.

`assess()` initializes `SubvolAssessment.redundancy_advisories` as `Vec::new()`. Then
`compute_redundancy_advisories()` produces a separate `Vec<RedundancyAdvisory>` that is
placed on `StatusOutput.redundancy_advisories` (the top-level field). The per-subvolume
`redundancy_advisories` field on `SubvolAssessment` is never populated by the advisory
system -- it stays empty unless manually filled.

This means the JSON output has `redundancy_advisories` at two levels: per-subvolume
(always empty from the advisory system) and top-level (populated). The per-subvolume field
exists on `StatusAssessment` (output.rs line 179) and is propagated by `from_assessment()`
(line 167), but is never filled by the advisory computation path.

**Recommendation:** Either populate the per-subvolume field by filtering the top-level
advisories by subvolume name, or remove the field from `SubvolAssessment` if per-subvolume
placement is not needed. The current state has a field that exists but is structurally
always empty from the production code path.

#### N3: Voice rendering does not use the `detail` field

**Location:** `voice.rs` lines 318-370.

The voice renderer generates its own text per advisory kind and ignores the `detail` field
on `RedundancyAdvisory`. For example, voice renders "The offsite copy on {drive} has aged."
while the detail field says "offsite drive {drive} last sent {days} days ago." The voice
version loses the specific day count that the detail carries.

The `detail` field is correctly documented as "for JSON daemon consumers" and the voice
layer independently renders the mythic text. This is the right separation per CLAUDE.md
("voice in presentation, precision in data"). But the `OffsiteDriveStale` voice text says
"has aged" without saying how long, while the design example says "has aged 23 days." The
interactive user gets less information than the JSON consumer.

**Recommendation:** Consider interpolating the age from the detail or adding an
`age_days: Option<i64>` field to `RedundancyAdvisory` so voice.rs can render "has aged 23
days" without parsing the detail string.

#### N4: Existing serialization roundtrip test uses schema_version: 2

**Location:** `sentinel_runner.rs` line 703.

The existing `state_file_serialization_roundtrip` test still uses `schema_version: 2` and
does not include `advisory_summary`. This is correct for backward compatibility testing
(v2 files must still parse), but there is no roundtrip test for a v3 file with
`advisory_summary: None`. The `state_file_v3_with_advisory_summary` test covers
`Some(...)`, but the `None` case (v3 writer, no advisories) is tested only via the v2
backward compat test.

**Recommendation:** Add a v3 roundtrip test with `advisory_summary: None` to verify the
`skip_serializing_if` behavior produces valid output that can be re-read.

---

## 6. Specific Stress-Test Answers

### Q1: NoOffsiteProtection per-subvolume scoping

**Correct.** The `/simplify` fix at line 789 properly handles `subvol.drives`:
- `Some(drive_list)`: checks only the listed drives for offsite role
- `None`: checks all config drives (meaning "use all drives")

This is the right behavior. The None/Some handling matches the config semantics.

### Q2: OffsiteDriveStale threshold consistency

**Intentionally aligned, underdocumented.** Both `OFFSITE_AT_RISK_DAYS` (enforcement) and
`OFFSITE_STALE_ADVISORY_DAYS` (advisory) are 30 days. The 8-30 day gap where the old
advisory would have fired is an intentional design change. See M1 above.

### Q3: Schema v3 backward compatibility

**Safe.** No code anywhere checks `schema_version >= N`. The schema version is a
documentation marker for consumers. The `serde(default)` + `skip_serializing_if` pattern
ensures v2 files deserialize cleanly (missing field becomes `None`). Tests cover this
(line 888-907).

### Q4: AdvisorySummary with only informational advisories

**Ambiguous semantics.** `count: 0, worst: Some(TransientNoLocalRecovery)` is technically
correct per the implementation but could confuse consumers. See M2 above.

### Q5: Voice rendering detail mismatch

**By design, but information-lossy for interactive users.** Voice generates its own mythic
text; `detail` carries terse facts for JSON. The `OffsiteDriveStale` voice text loses the
specific day count. See N3 above.

---

## 7. The Simplicity Question

**Is this implementation as simple as it could be?**

Yes. The /simplify pass already removed dead comments, combined iterations, extracted shared
test config, and moved `AdvisorySummary::from_advisories()` to a method. The remaining code
is proportional to the feature. Four advisory types, one pure function, one voice section,
one state file field. No over-engineering.

The cooldown/suppression logic was correctly deferred per YAGNI. Notifications on bare state
transitions were also deferred. Both are the right calls -- build them when the need is
observed, not preemptively.

---

## 8. Commendations

### C1: /simplify caught a real logic bug

The per-subvolume drive scoping fix for `NoOffsiteProtection` is exactly the kind of bug
that simplification passes catch. The original code checked `config.drives` globally; the
fix respects `subvol.drives`. This validates the /simplify step in the workflow.

### C2: Clean separation of advisory and enforcement layers

The advisory system does not influence promise states, retention, or backup decisions. The
design's "not enforcement" non-goal is perfectly implemented. `compute_redundancy_advisories()`
returns values; nothing consumes them for control flow.

### C3: Backward-compatible state file evolution

The `serde(default)` + `skip_serializing_if` pattern for `advisory_summary` is the correct
approach. The explicit "None means unknown, not zero" documentation is excellent. The v2
backward compatibility test verifies the contract.

---

## 9. For the Dev Team (Prioritized Action Items)

1. **Fix S1: Apply per-subvolume drive scoping to SPOF and OffsiteDriveStale.** The same
   `subvol.drives` pattern from NoOffsiteProtection should filter the drives considered by
   both checks. Without this, users with per-subvolume drive scoping get incorrect advisories.

2. **Decide M2: AdvisorySummary `worst` semantics.** Either exclude informational from
   `worst` or document the contract. Do this before Spindle exists.

3. **Document M1: Threshold alignment.** Add a comment near `OFFSITE_STALE_ADVISORY_DAYS`
   explaining the alignment with `OFFSITE_AT_RISK_DAYS` and the decision to drop the 7-day
   threshold.

4. **Resolve M3: `_now` parameter.** Remove it (YAGNI) or document the forward reservation.

5. **Consider N2: per-subvolume `redundancy_advisories` field.** Either populate it or
   remove it from `SubvolAssessment`.

6. **Consider N3: age information in voice rendering.** Add structured age data to
   `RedundancyAdvisory` for richer interactive output.

7. **Add N1: guarded exclusion test.** Quick win for test coverage.

---

## 10. Open Questions

1. **Should the SPOF and stale checks be fixed in this PR, or tracked for a follow-up?**
   The fix is mechanical (apply the same filter pattern), but adds ~10 lines per check plus
   tests. If the user does not currently use per-subvolume `drives` scoping, the bug is
   latent and can wait.

2. **Should the per-subvolume `redundancy_advisories` field on `SubvolAssessment` be
   populated?** The current top-level placement on `StatusOutput` is sufficient for `urd
   status`, but JSON consumers might expect per-subvolume advisory data. The field exists
   but is always empty -- this is confusing.
