# Arch-Adversary Review: Sentinel Session 1 — Lock Extraction + Pure State Machine

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-27
**Scope:** Implementation review — `src/lock.rs` (new), `src/sentinel.rs` (new), `src/commands/backup.rs` (modified), `src/main.rs` (modified)
**Review type:** Implementation review (post-implementation)
**Prior review:** `docs/99-reports/2026-03-27-sentinel-implementation-design-review.md`
**Commit:** uncommitted (working tree on master, 5dedec7)
**Reviewer:** Claude (arch-adversary)

---

## 1. Executive Summary

This is a clean Session 1 delivery. The lock extraction is correct and the state machine is well-structured. The circuit breaker has one logic gap that will manifest as a surprising behavioral asymmetry in production. The code addresses the prior design review's action items faithfully. The main risk is in lock.rs — a `File::create` call that truncates the lock file before attempting to acquire it, which can destroy metadata that a concurrent reader needs.

## 2. What Kills You (Catastrophic Failure Proximity)

The catastrophic failure mode for Urd is **silent data loss**. For Session 1 specifically:

**Can the Sentinel cause silent data loss?** No. Session 1 contains zero I/O paths to btrfs. The state machine is pure. The lock module wraps existing `flock(2)` behavior that was already in production. The Sentinel's active mode will spawn `urd backup` as a subprocess — it can't delete snapshots directly. **Distance: not reachable from this code.**

**Can the lock extraction break existing backups?** The behavioral change is small: the trigger string changes from absent to `"timer"`, and lock metadata is now written after acquisition. The lock mechanism itself is identical (`Flock::lock` with `LockExclusiveNonblock`). The only risk is the `File::create` truncation issue described in S1 below. **Distance: 2 bugs away** — requires concurrent lock-contention read during the create-truncation window AND the user to make a decision based on the corrupted metadata.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | State machine logic is sound. Circuit breaker has one asymmetry (S2). Lock extraction preserves existing behavior. 30 sentinel tests + 6 lock tests cover the important cases. |
| **Security** | 5 | No new attack surface. Lock module doesn't touch sudo paths. Sentinel types don't handle external input. No new dependencies. |
| **Architectural Excellence** | 4 | Clean pure/impure separation. Module boundaries follow established patterns. One concern: `sentinel_transition` clones the entire state on every call — fine now, could matter if state grows. |
| **Systems Design** | 3 | The `File::create` truncation race (S1) is a real systems-level issue. The circuit breaker's Open→Closed transition has no HalfOpen step (S2), which breaks the standard pattern. |
| **Rust Idioms** | 4 | Clean use of `BTreeSet`, `#[must_use]`, pattern matching. `chrono::Duration::from_std().unwrap_or(MAX)` is correct defensive coding. One structural concern: `CircuitBreakerConfig` is cloned into `CircuitBreaker` rather than referenced — fine for this size, but unusual. |
| **Code Quality** | 4 | Well-commented. Tests are readable and cover the stated design cases. Module header comments explain the architecture clearly. |

## 4. Design Tensions

### Tension 1: State cloning vs. mutation in sentinel_transition

`sentinel_transition` takes `&SentinelState` and returns a new `(SentinelState, Vec<SentinelAction>)`. This clones the entire state on every event. The alternative is `&mut SentinelState` with in-place mutation.

The clone approach is the right call for a pure state machine — it makes the function referentially transparent and enables the runner to compare old and new state without manual bookkeeping. The cost (cloning a `BTreeSet<String>` and a `Vec<PromiseSnapshot>`) is negligible for the event frequencies involved (seconds to minutes between events). This only becomes wrong if `SentinelState` grows to contain large data — unlikely given the design.

### Tension 2: `has_promise_changes` vs. reusing `notify::compute_notifications`

The Sentinel has its own promise-change detection (`has_promise_changes`) rather than reusing the existing `notify::compute_notifications()` from 5a. These do related but different things — `compute_notifications` takes heartbeats and produces `Notification` objects, while `has_promise_changes` takes promise snapshots and returns a bool.

This is the right split. The Sentinel's notification path will call `awareness::assess()` directly (not via heartbeat), so it needs its own comparison logic operating on assessments, not heartbeat structs. The two paths converge at `notify::dispatch()`, which is shared. But this means there are now **two independent implementations of "did promise states change?"** — one in `notify::compute_notifications` (heartbeat-based) and one in `sentinel::has_promise_changes` (assessment-based). They must agree. There's no test that exercises both paths with the same scenario to verify they produce consistent results. This is a Session 2 integration concern, not a Session 1 bug.

## 5. Findings

### Significant

**S1. `File::create` in `acquire_lock` and `try_acquire_lock` truncates the lock file before locking.**

```rust
let file = File::create(lock_path)?;  // ← truncates to zero bytes
match nix::fcntl::Flock::lock(file, ...) {
```

`File::create` opens with `O_CREAT | O_WRONLY | O_TRUNC`. The truncation happens *before* the `Flock::lock` call. This means:

1. Process A holds the lock with metadata `{"pid": 100, "trigger": "sentinel"}`
2. Process B calls `acquire_lock` → `File::create` truncates the file to 0 bytes
3. Process B calls `Flock::lock` → gets `EWOULDBLOCK`
4. Process B calls `read_lock_info(lock_path)` → reads the now-empty file → returns `None`
5. Process B reports "Another urd backup is already running (lock file: ...)" with no metadata

The metadata that Process A wrote is gone. Process A still holds the lock (flock is fd-based, not file-content-based), but any subsequent `read_lock_info` call by anyone will get `None` until Process A rewrites. But Process A never rewrites — it wrote once on acquisition.

*Consequence:* The lock contention error message falls back to the no-metadata format. Not dangerous — the lock still works — but the metadata feature (PID, trigger source, timestamp) is unreliable. Specifically, `urd backup` checking "is the holder the Sentinel?" via `read_lock_info` will get `None` instead of the Sentinel's metadata, breaking the review item M4 logic (exit 0 for Sentinel-held locks).

*Fix:* Use `OpenOptions::new().create(true).read(true).write(true).open(lock_path)` instead of `File::create`. This opens with `O_CREAT | O_RDWR` without `O_TRUNC`. The metadata write (`write_lock_metadata`) already truncates via `ftruncate` after acquiring the lock, so the initial truncation is unnecessary.

**S2. Circuit breaker has no explicit Open→HalfOpen transition — `allows_trigger` silently permits it.**

The standard circuit breaker pattern is: Open → (backoff elapses) → HalfOpen → (trial succeeds) → Closed, or (trial fails) → Open. In this implementation:

- `allows_trigger` returns `true` when Open + backoff elapsed (line 177), but it doesn't transition the state to HalfOpen.
- `evaluate_trigger_result` checks `circuit.state == CircuitState::HalfOpen` to decide whether to double backoff on failure.
- The runner must manually set `cb.state = CircuitState::HalfOpen` between calling `allows_trigger` (which says "go ahead") and `evaluate_trigger_result` (which checks the state).

This creates an implicit protocol: the runner must know to set HalfOpen between these two pure function calls. If it doesn't, a failure after an open-circuit backoff elapses will be treated as a closed-circuit failure (incrementing `failure_count` but not doubling backoff), because `evaluate_trigger_result` sees `Closed`, not `HalfOpen`.

The tests work around this by manually setting `cb.state = CircuitState::HalfOpen` (lines 855, 881), which proves the protocol exists but doesn't enforce it.

*Consequence:* If the runner forgets the intermediate HalfOpen set, the circuit breaker's backoff escalation is broken. The breaker still *opens* (failure_count accumulates), but it never doubles the backoff, so it retries every 15 minutes forever instead of backing off to 30m, 1h, 2h, etc.

*Fix:* Either (a) make `allows_trigger` return an enum (`Allowed`, `Blocked`, `HalfOpenTrial`) so the runner knows what state the trigger is in, or (b) add a `prepare_trigger(&mut self, now)` method that transitions Open→HalfOpen and returns whether the trigger is allowed. Option (a) is cleaner and stays pure:

```rust
pub enum TriggerPermission { Allowed, Blocked, HalfOpenTrial }
pub fn check_trigger(&self, now: NaiveDateTime) -> TriggerPermission
```

Then `evaluate_trigger_result` takes `TriggerPermission` instead of checking `circuit.state`.

### Moderate

**M1. `LockHeld` outcome updates `last_trigger` timestamp, which affects min_interval gating.**

```rust
TriggerOutcome::LockHeld => {
    // Not a real failure — ... Don't change circuit state or increment failure count.
}
```

But `new.last_trigger = Some(trigger.triggered_at)` is set unconditionally at line 432, *before* the match. So a `LockHeld` result still updates `last_trigger`. The next real trigger will be blocked by `min_interval` counting from the LockHeld event, even though no backup actually ran.

Scenario: Sentinel triggers at 10:00, lock is held → LockHeld. Drive unmounts and remounts at 10:15. Sentinel wants to trigger but `min_interval` (1h) blocks it because `last_trigger` is 10:00. The user unplugs the drive at 10:30. No backup was sent.

*Consequence:* `LockHeld` events eat the min_interval cooldown as if a real trigger occurred. This reduces responsiveness after timer/sentinel overlap.

*Fix:* Move `new.last_trigger = Some(trigger.triggered_at)` into the `Success` and `Failure` arms only.

**M2. `should_trigger_backup` for `DriveMounted` doesn't check `has_initial_assessment`.**

The `AssessmentTick` arm correctly returns `None` when `!state.has_initial_assessment`. But the `DriveMounted` arm doesn't check this flag. If a drive mounts before the first assessment completes, the function receives the just-computed assessments and may trigger a backup based on stale-by-default external drive status (every drive starts "unprotected" because no sends exist yet from the Sentinel's perspective).

*Consequence:* On Sentinel startup, if a drive is already mounted, the first `DriveMounted` event could trigger an unnecessary backup before the Sentinel has established baseline state. Not harmful (the backup itself is correct), but wastes resources and could trigger the circuit breaker if the backup "fails" for unrelated reasons.

*Fix:* Add `if !state.has_initial_assessment { return None; }` at the top of `should_trigger_backup`, before the match. Or check it in the `DriveMounted` arm.

**M3. `has_promise_changes` and `has_promise_degradation` are near-duplicates with subtly different semantics.**

Both iterate `previous` vs. `current` assessments. `has_promise_degradation` only checks one direction (current < previous). `has_promise_changes` checks both directions plus added/removed subvolumes. They share no code. If the comparison logic needs to change (e.g., to handle assessment errors), both must be updated independently.

*Consequence:* Maintenance burden. The divergence between "any change" (notification) and "degradation only" (trigger) is a real semantic distinction, but the implementation duplicates the iteration and lookup logic.

*Fix:* Extract the common comparison into a helper that returns a richer result (e.g., `Vec<(name, old_status, new_status)>`), then let each consumer filter. Not urgent — the current code is small enough that duplication is manageable.

### Commendation

**C1. The state machine is genuinely pure and well-tested.**

`sentinel_transition` takes `(&SentinelState, &SentinelEvent)` and returns `(SentinelState, Vec<SentinelAction>)` with no side effects. This follows the same pattern as the planner and awareness model, which have proven their value across 300+ tests. The 30 sentinel tests exercise every event type, drive tracking edge cases, circuit breaker state transitions, and trigger conditions. The `first_assessment_after_startup_no_notifications` test directly validates the prior review's M3 finding.

**C2. Lock extraction is minimal and correct.**

The diff to backup.rs is exactly 6 insertions and 27 deletions. The lock mechanism is identical — same `Flock::lock` call, same `EWOULDBLOCK` handling. The new metadata feature is additive and best-effort (failures don't affect the lock). The `read_lock_info` edge case handling (empty, corrupt, missing) is tested. This is a textbook extraction — same behavior, new capabilities, no regressions.

**C3. The trigger/circuit-breaker separation is clean.**

`should_trigger_backup` decides *whether* to trigger. `evaluate_trigger_result` decides *how the circuit breaker responds*. The runner sits between them. This separation means the trigger logic and the circuit breaker can be tested independently with different scenarios, which the tests demonstrate. The `TriggerOutcome::LockHeld` variant correctly distinguishes "couldn't trigger because of contention" from "triggered and failed", preventing false circuit-breaker opens.

## 6. The Simplicity Question

> **What could be removed?**

**`SentinelStateFile` and `CircuitBreakerState`** are defined but unused. They're serialization types for Session 2's state file. Having them here means they'll need to stay in sync with the actual state types as those evolve. If they drift, the Session 2 author will discover the mismatch only when wiring serialization. Consider removing them until Session 2 and generating them from the actual types then — or add a test that round-trips `SentinelState` → `SentinelStateFile` → fields to catch drift.

**`last_assessment` field on `SentinelState`** is declared but never read (compiler warns). It's intended for the runner to track timing, but `sentinel_transition` doesn't set it (it doesn't know the current time — correct for a pure function). Either remove it from `SentinelState` and let the runner track it separately, or accept that it's runner-managed state that happens to live in the state struct for serialization convenience.

> **What's earning its keep?**

**The adaptive tick function** (`compute_next_tick`) is three lines of pattern matching that encode a real operational insight: poll faster when things are worse. Simple, correct, and the tests document the exact intervals.

**The `has_promise_changes` first-assessment guard** (line 492-494) prevents a known failure mode (spurious notification flood on Sentinel restart) with two lines. Directly traces to a specific review finding.

## 7. For the Dev Team (Prioritized Action Items)

1. **Fix `File::create` truncation in lock.rs** (S1). Replace `File::create(lock_path)` with `OpenOptions::new().create(true).read(true).write(true).open(lock_path)` in both `acquire_lock` and `try_acquire_lock`. This preserves existing metadata during contention. The test `acquire_and_try_acquire_contention` should verify that metadata remains readable after a failed acquire attempt.

2. **Make the HalfOpen transition explicit** (S2). Change `allows_trigger` to return `TriggerPermission { Allowed, Blocked, HalfOpenTrial }`. Update `evaluate_trigger_result` to accept the permission rather than checking `circuit.state`. This eliminates the implicit protocol between the two functions. Update circuit breaker tests to use the new API.

3. **Move `last_trigger` update into Success/Failure arms only** (M1). Line 432 sets `new.last_trigger` unconditionally. Move it into the `Success` and `Failure` match arms so `LockHeld` doesn't consume the min_interval cooldown. Add test: `circuit_lock_held_does_not_consume_min_interval`.

4. **Guard `DriveMounted` trigger against pre-initial-assessment** (M2). Add `has_initial_assessment` check to the `DriveMounted` arm of `should_trigger_backup`. Add test: `no_trigger_on_drive_mount_before_initial_assessment`.

5. **Remove or test `SentinelStateFile`/`CircuitBreakerState`** (Simplicity). Either delete them (Session 2 will create them when needed) or add a round-trip test that catches structural drift.

## 8. Open Questions

1. **Should `lock::acquire_lock` return a richer error for "held by Sentinel" vs. "held by other"?** The current implementation puts the distinction in the error *message* string. Session 2 needs this distinction programmatically (to decide exit code 0 vs. 4). A structured error type (or a separate `is_sentinel_held(lock_path)` function) would be cleaner than string-matching.

2. **Who sets `has_initial_assessment = true`?** The state machine doesn't — it's a pure function that doesn't know which assessment is "first." The runner must set this flag after the first `Assess` action completes. This is implied by the design but not documented in the code. A comment on the field would help the Session 2 implementer.

---

*Review conducted as architectural adversary. Findings reflect implementation-phase analysis of Session 1 artifacts.*
