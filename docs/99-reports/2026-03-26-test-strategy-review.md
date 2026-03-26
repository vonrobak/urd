# Test Strategy Review: Three Design Proposals

> **TL;DR:** The proposed test strategies cover the happy paths and primary edge cases well
> but share a common blind spot: they under-test the interaction between new features and
> existing safety mechanisms (pin protection, retention, executor error isolation). The
> highest-risk gap is in the Protection Promises design — no test verifies that promise-derived
> retention policies don't accidentally bypass the three-layer pin protection system. The
> Sentinel design needs circuit breaker state transition tests that would catch the
> partial-failure cascade. The Structured Errors design is well-covered for a presentation-only
> feature.

**Date:** 2026-03-26
**Scope:** Test strategies from three design proposals:
- Structured Error Messages (`docs/95-ideas/2026-03-26-design-structured-errors.md`)
- The Sentinel (`docs/95-ideas/2026-03-26-design-sentinel.md`)
- Protection Promises (`docs/95-ideas/2026-03-26-design-protection-promises.md`)

**Existing test suite:** 251 tests. Strong coverage in plan.rs (31), awareness.rs (24),
voice.rs (23), executor.rs (17), retention.rs (9). No property-based tests (no proptest).
MockBtrfs and MockFileSystemState provide excellent test isolation.

---

## Design 1: Structured Error Messages

**Proposed tests:** 15 (8 pattern matching, 3 composite errors, 3 rendering, 1 monitoring)
**Risk level:** Low — presentation only, no behavior change
**Verdict:** Adequate. Minor gaps only.

### Coverage Map

| Function | Proposed tests | Risk | Assessment |
|----------|---------------|------|-----------|
| `translate_btrfs_error()` | 8 pattern tests + 1 fallthrough | Moderate | Good — one per pattern + fallthrough |
| Composite send/receive | 3 tests | Moderate | Good — covers both sides |
| Voice rendering | 3 tests | Low | Adequate |
| `LC_ALL=C` enforcement | 0 | Moderate | **Gap** |

### Gaps

**Moderate gap: No test for `LC_ALL=C` on subprocess calls.**

The design adds `LC_ALL=C` to all btrfs subprocess calls as a hardening measure. No test
verifies this. If someone adds a new `Command::new()` for btrfs without the env var, patterns
break silently on non-English systems.

```
test_btrfs_commands_set_lc_all_c
  Setup: Read MockBtrfs call recording, or inspect Command construction
  Assert: All subprocess calls include LC_ALL=C in environment
  Why: Patterns depend on English stderr. This is the only guarantee.
```

Since MockBtrfs doesn't spawn real processes, this needs a code-level assertion — either
a `#[cfg(test)]` hook that records env vars, or a grep-based test that scans `btrfs.rs` for
`Command::new` and verifies each has `.env("LC_ALL", "C")`.

**Low gap: No test for `BtrfsErrorContext` round-trip through `Display`.**

The `Display` impl on `UrdError::Btrfs` renders a flat summary from `BtrfsErrorContext`.
Verify that the flat summary is human-readable and contains the key facts (operation, stderr
excerpt).

```
test_btrfs_error_display_contains_operation_and_stderr
  Setup: BtrfsErrorContext with operation=Send, stderr="No space left"
  Assert: format!("{}", error) contains "send" and "No space"
  Why: Logs and non-voice error paths use Display, not the translation layer
```

**Low gap: No test for `BtrfsOperation` enum parsing from executor's operation types.**

The design unifies `OperationOutcome.operation` as `BtrfsOperation` enum. Test the mapping
from executor operation types ("send_full", "send_incremental", etc.) to enum variants.

### Recommended additions: +3 tests (total: ~18)

---

## Design 2: The Sentinel

**Proposed tests:** ~50 (7 notification, 9 state machine, 3 integration, ~30 additional
implied by "~20 state machine" and "~15 active mode")
**Risk level:** High — introduces concurrent execution, lock contention, auto-triggering
**Verdict:** Significant gaps in circuit breaker edge cases and lock contention scenarios.

### Coverage Map

| Component | Proposed tests | Risk | Assessment |
|-----------|---------------|------|-----------|
| 5a: `compute_notifications()` | 7 pure + 3 integration | Moderate | Good |
| 5b: `sentinel_transition()` | 9 state machine | High | **Gaps below** |
| 5c: Circuit breaker | 0 explicit | Critical | **Major gap** |
| 5c: Lock file | 0 explicit | High | **Gap** |
| 5a: `BackupOverdue` | 0 | High | **Gap** |

### Gaps

**Critical gap: No circuit breaker state transition tests.**

The circuit breaker is the primary defense against cascade-triggering — the closest
proximity to Urd's catastrophic failure mode (snapshot congestion from repeated auto-triggers).
The design defines Closed → Open → HalfOpen transitions with partial-failure semantics,
but proposes zero tests for this logic.

```
test_circuit_breaker_closes_after_success
  Setup: CircuitBreaker in Closed state, failure_count=0
  Action: Record success
  Assert: State remains Closed, failure_count=0

test_circuit_breaker_opens_after_max_failures
  Setup: Closed, failure_count=2, max_failures=3
  Action: Record failure
  Assert: State transitions to Open

test_circuit_breaker_half_open_after_backoff
  Setup: Open, last_failure 2h ago, min_interval=1h
  Action: Check if trigger allowed
  Assert: State transitions to HalfOpen, one trigger allowed

test_circuit_breaker_half_open_to_closed_on_success
  Setup: HalfOpen
  Action: Record success
  Assert: State transitions to Closed, failure_count reset

test_circuit_breaker_half_open_to_open_on_failure
  Setup: HalfOpen
  Action: Record failure
  Assert: State transitions to Open, backoff doubles

test_circuit_breaker_partial_success_on_scheduled_run
  Setup: Closed
  Action: Record partial success for ScheduledRun trigger
  Assert: Counts as failure (per design decision)

test_circuit_breaker_partial_success_on_drive_mounted
  Setup: Closed, trigger=DriveMounted, some sends succeeded
  Action: Record partial resolution
  Assert: Counts as resolved, counter reset

test_circuit_breaker_manual_backup_ignores_open_circuit
  Setup: Open
  Action: Manual urd backup attempts
  Assert: Allowed regardless of circuit state

test_circuit_breaker_exponential_backoff_caps_at_24h
  Setup: Open, failure_count=10
  Action: Compute next allowed trigger time
  Assert: Backoff capped at 24h, not 2^10 hours
```

**These 9 tests are non-negotiable before shipping 5c.** The circuit breaker is the only
thing between the Sentinel and a repeat of the catastrophic storage failure.

**High gap: No lock file contention tests.**

The lock file prevents concurrent backup runs. No tests verify the contention behavior.

```
test_lock_acquired_successfully
  Setup: No existing lock
  Action: acquire_backup_lock()
  Assert: Returns Ok(LockGuard), lock file exists

test_lock_contention_returns_error
  Setup: Hold lock in a thread
  Action: Second acquire_backup_lock() from main thread
  Assert: Returns Err indicating lock held

test_lock_released_on_drop
  Setup: Acquire lock, drop guard
  Action: Try to acquire again
  Assert: Succeeds

test_lock_file_contains_pid_and_trigger
  Setup: Acquire lock with trigger="sentinel"
  Action: Read lock file contents
  Assert: Contains PID, timestamp, trigger source

test_lock_survives_process_crash (integration, #[ignore])
  Setup: Fork, acquire lock, kill child
  Action: Parent tries to acquire
  Assert: Succeeds (flock released on process death)
```

**High gap: No `BackupOverdue` notification tests.**

The `BackupOverdue` event catches hung backups — an important safety mechanism added in
the review. No test verifies that it fires correctly.

```
test_backup_overdue_fires_when_stale
  Setup: Heartbeat with stale_after 2h ago
  Action: compute_notifications() with current time past stale_after
  Assert: BackupOverdue event generated with correct age

test_backup_overdue_does_not_fire_when_fresh
  Setup: Heartbeat with stale_after 1h from now
  Action: compute_notifications()
  Assert: No BackupOverdue event

test_backup_overdue_urgency_is_critical
  Setup: Heartbeat stale by 24h
  Action: compute_notifications()
  Assert: BackupOverdue urgency is Critical
```

**Moderate gap: No adaptive tick interval tests.**

```
test_adaptive_tick_all_protected → 15 minutes
test_adaptive_tick_any_at_risk → 5 minutes
test_adaptive_tick_any_unprotected → 2 minutes
```

**Moderate gap: No notification deduplication tests.**

```
test_notifications_dispatched_flag_prevents_resend
test_notifications_dispatched_false_triggers_resend
test_sentinel_skips_dispatch_when_flag_true
```

### Recommended additions: +23 tests (total: ~73)

The Sentinel is the most test-hungry feature. The circuit breaker alone needs 9 tests.
This is proportional — it's the highest-risk new code in the roadmap.

---

## Design 3: Protection Promises

**Proposed tests:** ~37 (5 derivation, 6 resolution, 7 achievability, 4 drive mapping,
4 transition safety, 3 awareness integration, 3 run frequency, 3 voice, 2 additional)
**Risk level:** High — changes retention policy derivation, one step from snapshot deletion
**Verdict:** Good coverage of the happy path; critical gaps in interaction with existing
safety mechanisms.

### Coverage Map

| Function | Proposed tests | Risk | Assessment |
|----------|---------------|------|-----------|
| `derive_policy()` | 5 | Moderate | Good |
| `resolve_subvolume()` | 6 | Critical | **Gaps below** |
| Achievability checks | 7 | Moderate | Good |
| Drive mapping | 4 | High | Good |
| Transition safety | 4 | Critical | Good but incomplete |
| Awareness integration | 2 | Moderate | Thin |
| Run frequency check | 3 | Moderate | Good |

### Gaps

**Critical gap: No test that promise-derived retention interacts correctly with pin
protection.**

This is the highest-risk untested scenario in the entire design. The three-layer pin
protection system (unsent protection in planner, `is_pinned` in retention, re-check in
executor) is Urd's primary defense against deleting snapshots needed for incremental sends.
Promise-derived retention policies flow through the same `graduated_retention()` function,
but no test verifies that the derived policies respect pinned snapshots.

The danger: `derive_policy("guarded")` produces `daily=7, weekly=4`. If a pinned snapshot
is 8 days old (outside the daily window), does `graduated_retention()` still protect it?
Yes — the existing retention function has `pinned` as an explicit parameter. But no test
exercises this with promise-derived retention configs specifically.

```
test_promise_derived_retention_preserves_pinned_snapshots
  Setup: Subvolume with protection_level="guarded" (derives daily=7)
         Pinned snapshot 10 days old (outside daily window)
  Action: Run graduated_retention() with derived config and pinned set
  Assert: Pinned snapshot NOT in deletions list
  Why: Pin protection must survive the promise derivation path. This is one bug
       away from the catastrophic failure mode.

test_promise_derived_retention_with_space_pressure_preserves_pins
  Setup: Same as above but with space_pressure=true
  Action: Run graduated_retention()
  Assert: Pinned snapshot still NOT deleted even under pressure
  Why: Space pressure enables more aggressive thinning but must never
       override pin protection.
```

**Critical gap: No test for the full pipeline: promise level → derived retention →
planner → retention decision.**

The design proposes `test_promise_derived_intervals_evaluated_correctly` but this tests
awareness model evaluation, not the retention/planning pipeline. A full pipeline test:

```
test_promise_level_to_retention_decision_pipeline
  Setup: Config with protection_level="protected", active snapshots including
         some outside derived retention windows, one pinned
  Action: plan() with MockFileSystemState
  Assert: Planner's retention operations respect derived retention AND pins
          AND unsent protection simultaneously
  Why: The promise layer adds a new indirection between config and retention.
       Pipeline tests catch integration bugs that unit tests miss.
```

**High gap: No test for protection level downgrade (resilient → guarded).**

The transition safety tests cover tightening retention. But what about downgrading
the protection level itself? Switching from `resilient` to `guarded` means
`send_enabled` goes from `true` to `false`. On the next run:
- No external sends happen
- External snapshots become orphaned (no retention runs against them)
- Pin files reference snapshots that will never be updated

```
test_downgrade_resilient_to_guarded_disables_sends
  Setup: Subvolume with protection_level="resilient", existing pin files
  Action: Change to protection_level="guarded", resolve_subvolume()
  Assert: send_enabled=false, existing pin files are stale but harmless

test_downgrade_does_not_delete_external_snapshots
  Setup: External drive has snapshots from previous resilient config
  Action: Plan with guarded level (send_enabled=false)
  Assert: No external retention operations planned (we don't delete what
          we're no longer managing)
```

**High gap: No test for drives field interaction with planner drive loop.**

The design acknowledges (review M2) that the planner needs to filter drives per subvolume.
The test strategy includes `test_drives_field_filters_send_targets` but doesn't test
interaction with the existing planner logic.

```
test_planner_skips_unmapped_drives_for_subvolume
  Setup: Config with 3 drives, subvolume maps to ["WD-18TB"] only
  Action: plan() with all 3 drives mounted
  Assert: Send operations only for WD-18TB, not for other drives

test_planner_sends_to_all_drives_when_no_mapping
  Setup: Config with 3 drives, subvolume has drives=None
  Action: plan() with all 3 drives mounted
  Assert: Send operations for all 3 drives (backward compatible)

test_planner_drive_mapping_with_uuid_mismatch
  Setup: Mapped drive has UUID mismatch
  Action: plan() with MockFileSystemState returning UuidMismatch
  Assert: Skip with UuidMismatch reason, not "drive not in mapping"
```

**Moderate gap: No property test for migration path identity.**

The design proposes `test_migration_existing_config_unchanged` as a "property test" but
describes it as a single example. For a migration guarantee this important, use actual
property-based testing:

```
// Using proptest (or manual exhaustive iteration over real config)
test_migration_identity_property
  Setup: Generate SubvolumeConfig instances with no protection_level field,
         various combinations of explicit and default fields
  Action: Compare sv.resolved(defaults) vs resolve_subvolume(sv, defaults, any_freq)
  Assert: Identical for every input
  Why: The migration claim ("zero breaking changes") is load-bearing.
       A single counterexample invalidates it.
```

This is one of the best candidates for proptest in the entire codebase — the invariant
is well-defined, the input space is enumerable, and a violation is catastrophic.

**Moderate gap: No test for voiding override display in status.**

```
test_status_shows_degraded_promise_when_sends_disabled
  Setup: protection_level="protected", send_enabled=false
  Action: Render status
  Assert: Shows "protected (degraded — no external sends)", not bare "protected"
```

### Recommended additions: +12 tests (total: ~49)

---

## Cross-Design Test Gaps

### Gap 1: No integration test between Sentinel triggers and existing backup pipeline

When the Sentinel auto-triggers a backup (5c), it calls the same pipeline as manual
`urd backup`. But the test strategies for the Sentinel and the existing executor tests
are independent. A test that exercises: Sentinel trigger → lock acquisition → plan →
execute → heartbeat → notification would catch integration bugs.

**Recommendation:** Add 2 integration tests to the Sentinel design:

```
test_sentinel_triggered_backup_produces_heartbeat (#[ignore])
test_sentinel_triggered_backup_respects_lock (#[ignore])
```

### Gap 2: No test for structured errors rendered in backup summary

The structured errors design produces `BtrfsErrorDetail` for rendering. The voice layer
renders it in backup summaries. But the backup summary tests (21 existing) were written
before structured errors exist. When structured errors are implemented, the backup summary
tests need updating.

**Recommendation:** Add to the structured errors test plan:

```
test_backup_summary_renders_structured_error_instead_of_raw
```

### Gap 3: Proptest is absent from the codebase

Three designs would benefit from property-based testing:
1. Migration identity (promises): `resolve_subvolume` with no `protection_level` = `resolved()`
2. Pin protection under derived retention: `graduated_retention` never returns pinned snapshots
3. Translation completeness: `translate_btrfs_error` always produces non-empty summary

**Recommendation:** Add `proptest` to dev-dependencies. The migration identity property alone
justifies the dependency. Start with one proptest, expand as patterns prove useful.

---

## Summary

| Design | Proposed | Gaps found | Recommended total | Critical gaps |
|--------|----------|-----------|-------------------|---------------|
| Structured Errors | 15 | 3 (low) | ~18 | 0 |
| Sentinel | ~50 | 23 (9 critical) | ~73 | Circuit breaker transitions |
| Protection Promises | ~37 | 12 (4 critical) | ~49 | Pin protection with derived retention |

**Highest-priority tests to add before implementation:**

1. **Circuit breaker state transitions** (9 tests) — Sentinel's cascade defense
2. **Promise-derived retention preserves pinned snapshots** (2 tests) — one bug from data loss
3. **Full pipeline: promise → planner → retention** (1 test) — integration safety
4. **Lock file contention** (5 tests) — concurrent execution correctness
5. **Migration identity property** (1 proptest) — zero-breaking-change guarantee
