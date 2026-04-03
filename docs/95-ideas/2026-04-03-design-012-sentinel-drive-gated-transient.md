---
upi: "012"
status: proposed
date: 2026-04-03
---

# Design: Sentinel Drive-Gated Transient Backup + Space Pressure Monitoring (UPI 012)

> **TL;DR:** Two Sentinel enhancements for the horizon: (1) trigger transient subvolume
> backups when their configured drive appears, aligning backup timing with drive
> availability instead of a fixed schedule; (2) monitor filesystem free space on snapshot
> roots and emit early-warning notifications before the space guard kicks in. Both extend
> the Sentinel's event-driven model — no new daemons, no new processes.

## Problem

### Problem 1: Transient backups on a schedule are architecturally wrong

The nightly timer creates snapshots for all subvolumes at ~04:00, including transient
ones. For transient subvolumes (external-only by intent), this means:

- If the drive is mounted at 04:00: snapshot → send → cleanup. Works correctly.
- If the drive is NOT mounted at 04:00: snapshot created, can't send, unsent protection
  keeps it. Space wasted until the drive appears and the next backup run fires.

UPI 011 fixes the acute problem (don't create when drive isn't mounted). But the
schedule-driven approach is still conceptually wrong for transient subvolumes. The
optimal behavior is: **back up when the drive appears, not when the clock ticks.**

The Sentinel already detects drive mount events (`DriveMounted { label }`) and has
infrastructure for triggering backups on state changes (sentinel.rs:268-300). It checks
whether any subvolume needs a send to the newly mounted drive. But it currently treats
this as a generic "something might be overdue" trigger — it doesn't distinguish
transient subvolumes that specifically need the drive to be useful.

### Problem 2: No filesystem space monitoring

The fifth NVMe exhaustion was caught by an external Discord alert from the homelab
monitoring stack — not by Urd itself. Urd has a reactive space guard
(`min_free_bytes` prevents snapshot creation below threshold) but no proactive warning.
The user learns about space pressure only when the space guard activates and skips
a snapshot, which appears as a skip reason in backup output that nobody reads at 04:00.

The Sentinel monitors drive availability and promise states on a timer. It does not
monitor filesystem free space on snapshot roots. On a constrained volume like the 118GB
NVMe, the gap between "healthy" and "space guard firing" can be crossed in a single
backup cycle.

## Proposed Design

### Enhancement 1: Drive-gated transient backup trigger

**New Sentinel behavior:** When a `DriveMounted` event fires for a drive that is
configured for at least one transient subvolume, the Sentinel triggers a targeted
backup for those transient subvolumes immediately.

**Event flow:**

```
udev: drive WD-18TB1 appears
  → Sentinel: DriveMounted { label: "WD-18TB1" }
    → Sentinel checks: any transient subvolumes with this drive in their `drives` list?
      → yes: emit Action::TriggerTransientBackup { label: "WD-18TB1", subvolumes: ["htpc-root"] }
        → sentinel_runner: spawn `urd backup --auto --subvolume htpc-root`
      → no: standard drive reconnection flow (notification, assessment)
```

**Key design decisions:**

- **Targeted backup, not full backup.** Only back up the transient subvolumes that
  need this specific drive. Don't trigger a full backup on every drive plug — that
  would be noisy and could overwhelm the circuit breaker.

- **`--auto` mode.** The triggered backup runs in auto mode, which means interval
  checks apply. If the transient subvolume was already backed up recently (within its
  `send_interval`), the trigger is a no-op. This prevents rapid-fire backups if someone
  plugs and unplugs a drive repeatedly.

- **Circuit breaker applies.** The existing circuit breaker (15min initial, exponential
  backoff) prevents cascade failures. A failed drive-gated backup counts as a failure
  and extends the backoff window.

- **Coexists with timer-based backups.** The nightly timer still runs for all subvolumes.
  If the drive happens to be mounted at 04:00, the timer handles transient subvolumes
  normally. Drive-gated triggers are additive — they provide faster response when drives
  appear between scheduled runs.

**Sentinel state machine changes:**

```rust
// New action variant
enum Action {
    // ... existing variants ...
    TriggerTransientBackup {
        label: String,
        subvolumes: Vec<String>,
    },
}

// In process_event for DriveMounted:
// After existing logic, add:
let transient_subvols = config.resolved_subvolumes()
    .iter()
    .filter(|sv| sv.local_retention.is_transient())
    .filter(|sv| sv.drives.as_ref().map_or(false, |d| d.contains(&label)))
    .map(|sv| sv.name.clone())
    .collect::<Vec<_>>();

if !transient_subvols.is_empty() {
    actions.push(Action::TriggerTransientBackup {
        label,
        subvolumes: transient_subvols,
    });
}
```

**sentinel_runner.rs changes:**

```rust
// Handle the new action
Action::TriggerTransientBackup { label, subvolumes } => {
    for subvol in &subvolumes {
        log::info!(
            "Drive {label} mounted — triggering transient backup for {subvol}"
        );
    }
    // Spawn: urd backup --auto --subvolume <name>
    // (one per subvolume, or --subvolume flag could accept multiple)
    self.trigger_backup(Some(&subvolumes[0])).await?;
}
```

### Enhancement 2: Filesystem space pressure monitoring

**New Sentinel behavior:** On each `AssessmentTick`, check free space on each snapshot
root. If free space drops below a configurable warning threshold (higher than
`min_free_bytes`), emit a notification.

**Config addition (v1 schema, optional):**

```toml
[general]
space_warning_threshold = "15GB"    # Warn when any snapshot root drops below this
```

Or per snapshot root if different volumes have different sizes. For simplicity, start
with a single global threshold.

**Event flow:**

```
AssessmentTick fires
  → Sentinel checks free space on each unique snapshot root
    → snapshot_root "/.snapshots": 8.2 GB free, threshold 15 GB
      → Emit Action::NotifySpacePressure {
            snapshot_root: "/.snapshots",
            free_bytes: 8_800_000_000,
            threshold_bytes: 15_000_000_000,
            subvolumes: ["htpc-root", "htpc-home"],
        }
```

**Notification content (voice.rs):**

```
⚠ Storage pressure on /.snapshots
  8.2 GB free — below 15 GB warning threshold
  Subvolumes affected: htpc-root, htpc-home
  
  Consider: review retention, check for stale snapshots, 
  or run `urd plan` to see pending cleanup.
```

**Dedup:** The notification should fire once per assessment cycle where the condition
is true, but not repeat every tick. Use the same cooldown mechanism as other Sentinel
notifications. The notification should re-fire if free space drops further (e.g., at
50% of the warning threshold) as an escalation.

**Sentinel state machine changes:**

```rust
// New event type (or fold into AssessmentTick processing)
// New action variant
enum Action {
    // ... existing variants ...
    NotifySpacePressure {
        snapshot_root: PathBuf,
        free_bytes: u64,
        threshold_bytes: u64,
        subvolumes: Vec<String>,
    },
}
```

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `sentinel.rs` | New `TriggerTransientBackup` action; space pressure check in assessment tick; new `NotifySpacePressure` action | State machine tests: DriveMounted + transient subvol → trigger action; DriveMounted + non-transient → no trigger; space below threshold → notify; space above → no notify; dedup across ticks. ~8-10 tests. |
| `sentinel_runner.rs` | Handle `TriggerTransientBackup` (spawn targeted backup); handle `NotifySpacePressure` (format + dispatch notification) | Integration tests with mock runner: verify backup command spawned with correct args; verify notification dispatched. ~4-5 tests. |
| `notify.rs` | New notification template for space pressure | Template test: correct message formatting. ~2 tests. |
| `voice.rs` | Space pressure notification rendering | Voice rendering test. ~1-2 tests. |
| `config.rs` | `space_warning_threshold` field (optional, v1 only or both) | Parser tests. ~2 tests. |
| `types.rs` | `ByteSize` already exists; possibly new config field type | Minimal. |

**Modules NOT touched:** `plan.rs`, `executor.rs`, `chain.rs`, `retention.rs`,
`awareness.rs`. These are backup-path modules; this design extends monitoring only.

## Effort Estimate

**~1-1.5 sessions total.** Calibrated against:
- UPI 006 (drive reconnection notifications): 1 Sentinel action + notification template,
  ~0.5-1 session → Enhancement 1 is similar scope (1 new action, 1 runner handler)
- Enhancement 2 adds another action + config field + notification template → ~0.5 session

Could be split: Enhancement 1 first (higher impact), Enhancement 2 second.

## Sequencing

This is horizon work — not in the active arc.

**Prerequisites:**
- UPI 011 must ship first (transient behavioral fix). Without 011, drive-gated triggers
  would create snapshots that can't be safely managed.
- Sentinel active mode design should be reviewed. Enhancement 1 is effectively the first
  use case of "Sentinel triggers backups." If the active mode design has broader
  requirements (permission model, user opt-in, rate limiting), those should inform this.

**Recommended order:**
1. UPI 011 (emergency fix) — immediate
2. UPI 010 (config schema v1) — already in progress
3. UPI 012 Enhancement 1 (drive-gated transient) — after Sentinel active mode design
4. UPI 012 Enhancement 2 (space pressure) — can ship independently, lower priority

## Architectural Gates

### Enhancement 1: Sentinel active mode precedent

The Sentinel currently reacts to events by assessing state and sending notifications.
Enhancement 1 makes it trigger backups — a qualitative change from observer to actor.
This should be reviewed against the Sentinel's design philosophy:

- **Permission model:** Should the user explicitly opt in to Sentinel-triggered backups?
  Currently the Sentinel runs as a user service — it has the same permissions. But
  "monitoring" and "acting" are psychologically different to users.
- **Failure isolation:** If the triggered backup fails, how does it affect the Sentinel?
  The circuit breaker handles rate limiting, but error propagation needs design.
- **Observability:** Triggered backups should be visible in `urd status` or logs. The
  user should know "this backup was triggered by drive insertion at 14:23" vs "this
  backup ran at the scheduled 04:00."

**Recommendation:** This gate is advisory, not blocking. The existing circuit breaker
and `--auto` mode provide sufficient safety for a first implementation. But document the
precedent: Enhancement 1 is the first instance of Sentinel-as-actor, and future active
mode features should follow the same pattern.

### Enhancement 2: No gates

Space monitoring is read-only observation + notification. No new contracts, no new
actions beyond notification dispatch.

## Rejected Alternatives

### Dedicated transient backup daemon

A separate process that watches for drive events and triggers transient backups.
Rejected because:
- The Sentinel already watches for drive events
- Another daemon adds operational complexity (two services to manage, coordinate, debug)
- The Sentinel's circuit breaker and assessment infrastructure are needed anyway

### Polling-based space monitoring (cron)

A cron job or separate timer that checks space and alerts. Rejected because:
- The Sentinel already runs on a tick cycle
- Adding space checks to the existing assessment tick is zero additional infrastructure
- Cron-based monitoring doesn't integrate with Urd's notification system

### Space pressure triggers aggressive retention

When space is low, automatically thin snapshots more aggressively. Rejected because:
- "Deletions fail closed" (ADR-107) — automatic deletion under pressure is the opposite
  of fail-closed
- The space guard already prevents new snapshots; aggressive retention deletes existing
  ones, which could remove the user's recovery options
- Notification is the right response: inform the human, let them decide

## Assumptions

1. **Sentinel can spawn `urd backup` processes.** The runner already has infrastructure
   for spawning subprocesses (it runs assessments). Spawning a backup is architecturally
   the same — a subprocess with arguments.

2. **The circuit breaker is sufficient rate limiting for drive-gated triggers.** A user
   plugging and unplugging a drive rapidly should not trigger unlimited backups. The
   15-minute initial cooldown + exponential backoff handles this.

3. **Filesystem free space checks are cheap.** `statvfs()` is a constant-time operation.
   Adding it to the assessment tick (every ~5 minutes) has negligible performance impact.

4. **A single global `space_warning_threshold` is sufficient for v1.** Per-root thresholds
   are a future enhancement if users have volumes of wildly different sizes.

## Open Questions

### Q1: Should drive-gated triggers work for non-transient subvolumes too?

**Option A (transient only):** Only transient subvolumes benefit from drive-gated
triggers. Non-transient subvolumes have local snapshots ready to send whenever the
nightly timer runs.

**Option B (all subvolumes with pending sends):** Any subvolume with an unsent
snapshot gets triggered when its drive appears. This is closer to "back up when
possible" — the universal ideal.

**Recommendation: Start with Option A, design for Option B.** Transient subvolumes
have the strongest need (they can't create useful local snapshots without the drive).
Non-transient subvolumes benefit less (they already have local history). But the
implementation should be extensible — the trigger logic should check "does this
subvolume need a send to this drive?" not "is this subvolume transient?"

### Q2: What's the right space warning threshold default?

No default is universally right. Options:
- **Fixed default (10GB):** Simple but wrong for large volumes.
- **Percentage of volume (10%):** Scales but 10% of 10TB is 1TB, which isn't "pressure."
- **No default (opt-in):** Safest — no notification noise until configured.
- **Derived from `min_free_bytes`:** Warning at 2x the space guard threshold.

**Recommendation: Derived from `min_free_bytes` × 2, opt-in via config.** If the user
has set `min_free_bytes = 10GB`, warn at 20GB. If not set, no warning. This ties the
warning to the user's own sense of "how much space is too little" without inventing a
new threshold.

### Q3: Should triggered backups appear differently in output?

When `urd status` shows the last backup, should "triggered by drive WD-18TB1 at 14:23"
be distinguishable from "scheduled run at 04:00"? The trigger source is already captured
in the lock metadata (`lock.rs` tracks trigger source). Extending this to backup history
(state.rs) would provide observability.

**Recommendation: Yes, but defer.** Track the trigger source in state.db for future
use. Don't change the status output now — that's UPI 002 (output polish) territory.
