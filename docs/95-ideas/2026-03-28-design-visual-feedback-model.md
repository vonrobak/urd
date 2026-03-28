# Design: Visual Feedback Model — Two-Axis Status and Tray Icon Architecture

> **TL;DR:** Redesign Urd's status communication across all surfaces (CLI, sentinel
> state file, tray icon, notifications) by separating data safety from operational
> health. The current single-axis model (PROTECTED/AT RISK/UNPROTECTED) conflates
> whether backups exist with whether the next backup can succeed, leading to false
> reassurance. This design introduces a two-axis assessment, extends the sentinel
> state file as the universal source of truth for all visual surfaces, and defines
> the spindle tray icon as a state-driven static icon set.

**Date:** 2026-03-28
**Status:** proposed
**Depends on:** Sentinel Sessions 1-2 (complete), awareness model (complete)
**Inputs:**
- [Hardware swap test journal](../../docs/98-journals/2026-03-28-sentinel-hardware-swap-test.md) — evidence of false reassurance
- [Spindle tray icon brainstorm](2026-03-28-brainstorm-tray-icon-spindle.md) — visual metaphor exploration
- [UX Norman principles brainstorm](2026-03-23-brainstorm-ux-norman-principles.md) — design principles

---

## Problem

After swapping a cloned drive (same UUID, different content), `urd status` reported
all subvolumes as PROTECTED. Every incremental chain was broken, four full sends were
planned (two blocked only by space guards), free space dropped by 1.1TB, and the
physical drive had silently changed. The user saw a wall of green.

The single word "PROTECTED" answered a question the user didn't ask ("is the last
send's freshness within the multiplier threshold?") and failed to answer the question
they did ask ("is my data safe and will the next backup work?").

### Specific failures

1. **PROTECTED conflates data safety with operational health.** Data was safe (recent
   copies existed). Operations were degraded (all chains broken, full sends pending).
   Same word for both.

2. **UNPROTECTED conflates absence with danger.** A drive that's unmounted and sitting
   in a safe shows the same status as a drive that has never received a snapshot.

3. **Chain health is invisible.** `full (pin missing on drive)` in the CHAIN column
   is the most important signal after the swap, but it's a neutral table cell with no
   escalation path to the status badge, the sentinel state, or notifications.

4. **No temporal context.** "PROTECTED" gives no sense of whether the user is
   comfortably within the threshold or one hour from AT RISK.

5. **No surface for tray/GUI consumers.** The sentinel state file (`sentinel-state.json`)
   carries only promise states — not enough for a tray icon to show operational health.

---

## Proposed Design

### The two-axis model

Replace the single `PromiseStatus` with two independent assessments per subvolume:

**Axis 1: Data Safety** — "Do recent copies of my data exist?"
- Measures what has already happened. Snapshots exist or they don't.
- Changes slowly (on the timescale of backup intervals).
- Current `PromiseStatus` freshness logic is correct for this axis.

**Axis 2: Operational Health** — "Can the next backup succeed efficiently?"
- Measures readiness for the next operation.
- Changes quickly (drive mount, chain break, space change).
- Currently not computed at all — scattered across chain health, drive status,
  space checks, with no aggregation.

After the drive swap:
- Data safety: **OK** (recent snapshots exist locally and externally)
- Operational health: **Degraded** (all chains broken, full sends pending, space tight)

The current model collapsed both into "PROTECTED" and lost the warning.

### Module changes

#### `awareness.rs` — add operational health computation

The awareness module remains a pure function. It already receives `FileSystemState`
which provides everything needed to compute operational health.

```rust
/// Operational health for a subvolume — can the next backup succeed efficiently?
/// Ordered worst-to-best so `min()` yields the worst health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationalHealth {
    /// Something will prevent or severely impair the next backup.
    /// Examples: no drives connected for a send-enabled subvolume,
    /// space critically low.
    Blocked,
    /// Next backup will work but suboptimally.
    /// Examples: chain broken (full send required), space getting tight,
    /// drive away for extended period.
    Degraded,
    /// Everything normal — incremental chains healthy, space adequate.
    Healthy,
}

/// Extended assessment — the two-axis model.
pub struct SubvolAssessment {
    pub name: String,
    // Axis 1: data safety (renamed from `status` for clarity)
    pub safety: PromiseStatus,
    // Axis 2: operational health (new)
    pub health: OperationalHealth,
    // Reasons for non-Healthy operational health
    pub health_reasons: Vec<String>,
    // ... existing fields unchanged
    pub local: LocalAssessment,
    pub external: Vec<DriveAssessment>,
    pub advisories: Vec<String>,
    pub errors: Vec<String>,
}
```

**Operational health computation** (new function in `awareness.rs`):

Inputs (all available through `FileSystemState`):
- Chain health per drive (pin file present? matching snapshot exists?)
- Drive mount state
- Available space vs estimated next send size
- Number of connected drives vs configured drives

Operational health rules:
- **Blocked:** No configured drives mounted AND subvolume has send_enabled.
  Or: estimated next send exceeds available space on all connected drives.
- **Degraded:** Chain broken on any connected drive (full send required). Or:
  space within 20% of min_free_bytes. Or: a configured drive has been unmounted
  for >7 days (already an advisory, promote to health signal).
- **Healthy:** Chains intact on all connected drives, space adequate.

**Design decision:** OperationalHealth is computed per-subvolume, not per-drive,
because the user cares about "will my next backup of htpc-home work?" not "is
drive WD-18TB1 operationally healthy?" Per-drive details go into `health_reasons`.

#### `output.rs` — extend structured output types

```rust
pub struct StatusAssessment {
    pub name: String,
    pub safety: String,          // renamed from `status`
    pub health: String,          // new: "healthy", "degraded", "blocked"
    pub health_reasons: Vec<String>,  // new: why health is not "healthy"
    // ... remaining fields unchanged
}
```

**Backward compatibility concern:** The `status` field is consumed by:
- `voice.rs` (interactive rendering)
- JSON output (daemon mode, piped)
- `sentinel-state.json` (sentinel state file)
- Prometheus metrics (`urd_promise_status` gauge)

**Gate: ADR needed** for renaming `status` → `safety` in JSON output and metrics.
The sentinel state file schema_version can absorb this (bump to 2). Prometheus
metrics are a backward-compatibility contract (ADR-105) — the existing
`urd_promise_status` metric must be preserved; a new `urd_operational_health`
metric is added alongside it.

**Alternative considered:** Keep `status` as the field name and add `health`
alongside it. Less disruptive, but perpetuates the naming confusion. The word
"status" doesn't tell the reader which axis it represents.

**Recommendation:** Keep `status` in JSON output for backward compatibility (it
still carries the same freshness-based value). Add `safety` as an alias that
eventually replaces it. Add `health` as a new field. The CLI interactive output
uses the new vocabulary immediately. JSON consumers get a deprecation period.

#### `voice.rs` — richer interactive rendering

**Summary line** (replaces "All subvolumes PROTECTED"):

```
All data safe. 3 subvolumes need attention — chain broken on WD-18TB1.
```

or:

```
7 of 8 safe. htpc-root has no backup in 5 days. Chain issues on WD-18TB1.
```

The summary line answers two questions in order:
1. Is my data safe? (safety axis — the existential question)
2. Is anything off? (health axis — the operational question)

**Per-subvolume table changes:**

Current columns: `STATUS  PROMISE  SUBVOLUME  LOCAL  [DRIVE]  CHAIN`

Proposed columns: `SAFETY  HEALTH  SUBVOLUME  LOCAL  [DRIVE]  CHAIN`

- `SAFETY` column: `OK` (green), `aging` (yellow), `gap` (red)
  - "OK" replaces "PROTECTED" — shorter, less absolutist, carries less false weight
  - "aging" replaces "AT RISK" — descriptive, implies a clock is ticking
  - "gap" replaces "UNPROTECTED" — there's a gap in protection, concrete
- `HEALTH` column: `good` (green/dim), `degraded` (yellow), `blocked` (red)
  - Only shown when at least one subvolume is non-healthy (avoid noise)

**Unmounted drive columns:**

Currently: shows em-dash with no distinction between "away" and "never sent."

Proposed: `away` (dimmed) when drive is unmounted but has send history.
Em-dash only when no snapshot has ever been sent to that drive.

**Temporal context:**

Add age of newest backup to the LOCAL column: `10 (2h)` means 10 snapshots,
newest is 2 hours old. For external drives: `7 (18h)` means 7 snapshots on
that drive, newest send was 18 hours ago. This is the single most valuable
addition — it lets the user assess comfort without understanding multiplier
thresholds.

**Chain health escalation:**

When chain health is `Full` on a drive that previously had `Incremental` chains,
the CHAIN column shows in yellow. When ALL chains break simultaneously (the
drive-swap pattern), an advisory line appears below the table:

```
  NOTE: All chains on WD-18TB1 broke simultaneously — verify drive identity.
```

#### `output.rs` / `sentinel-state.json` — extend for tray consumers

The sentinel state file becomes the single source of truth for all visual
surfaces. Currently it carries only promise states. Extend it:

```json
{
  "schema_version": 2,
  "pid": 500258,
  "started": "2026-03-27T23:27:04",
  "last_assessment": "2026-03-28T21:29:54",
  "mounted_drives": ["WD-18TB1"],
  "tick_interval_secs": 120,

  "visual_state": {
    "icon": "degraded",
    "summary": "All data safe. Chains broken on WD-18TB1.",
    "tooltip": "Protected — chains need attention on WD-18TB1"
  },

  "promise_states": [
    {
      "name": "htpc-home",
      "safety": "OK",
      "health": "degraded",
      "health_reasons": ["chain broken on WD-18TB1 — next send will be full"]
    }
  ],

  "circuit_breaker": {
    "state": "closed",
    "failure_count": 0
  }
}
```

**The `visual_state` block** is the tray icon's API. The sentinel computes it
from the full assessment. The tray applet reads the file, extracts `visual_state`,
and sets its icon + tooltip accordingly. This decouples the visual design from the
tray implementation — any frontend (GTK, Qt, web) reads the same file.

**`visual_state.icon` values** (the icon selector):

| Value | Meaning | Tray icon |
|-------|---------|-----------|
| `"ok"` | All safe, all healthy | Spindle with intact thread (green/gold) |
| `"ok_limited"` | All safe, no external drives connected | Spindle dimmed (grey-green) |
| `"degraded"` | All safe, health issues | Spindle with thread knot (yellow) |
| `"at_risk"` | Some data aging toward threshold | Spindle with fraying thread (amber) |
| `"unprotected"` | Data gap exists | Spindle with broken thread (red) |
| `"active"` | Backup currently running | Spindle spinning (animated or overlay) |
| `"error"` | Sentinel/system error | Spindle frozen/tilted (red) |

Seven states. Each maps to a static SVG icon file. The tray applet selects the
file by name: `urd-icon-ok.svg`, `urd-icon-degraded.svg`, etc.

**`visual_state.summary`** — One sentence for the context menu header.
**`visual_state.tooltip`** — Shorter version for hover tooltip.

Both are pre-computed by the sentinel so the tray applet doesn't need to
understand Urd's domain model.

#### `sentinel.rs` / `sentinel_runner.rs` — compute visual state

The pure state machine (`sentinel.rs`) gains a function:

```rust
/// Compute the visual state from the current assessment.
/// Pure function: assessments in, visual state out.
pub fn compute_visual_state(
    assessments: &[SubvolAssessment],
    mounted_drives: &BTreeSet<String>,
    configured_drive_count: usize,
    backup_running: bool,
) -> VisualState {
    // ...
}
```

Logic:
1. If `backup_running` → `"active"`
2. If any assessment has `safety == Unprotected` → `"unprotected"`
3. If any assessment has `safety == AtRisk` → `"at_risk"`
4. If any assessment has `health == Blocked` or `health == Degraded` → `"degraded"`
5. If `mounted_drives.len() < configured_drive_count` and all drives are expected
   (not offsite-role) → `"ok_limited"`
6. Otherwise → `"ok"`

The summary and tooltip are built by `voice.rs` (mythic voice for tooltip,
clinical for summary), keeping the pure/presentation split.

#### `notify.rs` — operational health notifications

Currently notifications fire only on promise state transitions (PromiseDegraded,
PromiseRecovered). Add:

```rust
pub enum NotificationEvent {
    // ... existing variants
    /// Operational health degraded (new)
    HealthDegraded {
        subvolume: String,
        reasons: Vec<String>,
    },
    /// All chains broke simultaneously — possible drive swap (new)
    DriveAnomalyDetected {
        drive_label: String,
        detail: String,
    },
}
```

**HealthDegraded** fires when a subvolume transitions from Healthy to Degraded
or Blocked. Separate from promise degradation — the user needs to know that
operations need attention even when data is safe.

**DriveAnomalyDetected** fires when the sentinel detects the simultaneous-chain-
break pattern from the hardware swap test. This is the detection mechanism for
the clone-swap blind spot until proper drive identity is implemented.

Notification urgency: HealthDegraded = INFO (not alarming — data is safe).
DriveAnomalyDetected = WARNING (unusual, warrants investigation).

---

## What this design does NOT cover

These are related concerns identified in the journal but deliberately out of scope:

1. **Drive identity beyond UUID.** Session token, LUKS UUID, or snapshot-set
   fingerprinting. Separate design needed — different module (`drives.rs`),
   different ADR gate (changes on-disk contract if writing tokens).

2. **Full-send confirmation gate.** Blocking a full send when chains were
   previously incremental. Affects `executor.rs` and the planner — separate
   concern from visual feedback.

3. **New drive onboarding workflow.** Guided setup when unconfigured BTRFS drives
   appear. Requires interactive CLI design, separate from status rendering.

4. **Tray applet implementation.** This design defines the contract (state file
   schema, icon values); the actual GTK/Qt/Electron tray applet is a separate
   project. The icon SVG artwork is also out of scope.

5. **Promise level naming.** The journal noted that guarded/protected/resilient
   are internal jargon. ADR-110 owns this vocabulary — renaming is a config
   contract change, not a visual feedback change.

---

## Data flow

```
config + filesystem state
          |
    awareness.rs          (pure: computes safety + health per subvolume)
          |
    output.rs             (structures data for consumers)
          |
    ┌─────┴──────┐
    |            |
voice.rs    sentinel_runner.rs
(CLI text)     |
            sentinel.rs   (pure: computes visual_state)
               |
          sentinel-state.json
               |
         tray applet      (reads file, sets icon + tooltip)
```

All pure modules remain pure. The sentinel runner is the only module that
performs I/O (writes the state file). The tray applet is a completely separate
process that watches the state file for changes.

---

## Module impact summary

| Module | Change | Size |
|--------|--------|------|
| `awareness.rs` | Add `OperationalHealth` enum, compute per-subvolume | ~60 lines new code, ~15 tests |
| `output.rs` | Add `health`, `health_reasons`, `visual_state` to output types | ~40 lines |
| `voice.rs` | New summary line, column renames, temporal context, chain escalation | ~80 lines changed |
| `sentinel.rs` | Add `compute_visual_state()` pure function | ~30 lines, ~10 tests |
| `sentinel_runner.rs` | Write extended state file, detect simultaneous chain break | ~20 lines |
| `notify.rs` | Add `HealthDegraded`, `DriveAnomalyDetected` variants | ~30 lines |

**Estimated effort:** ~280 lines of code, ~25 new tests. Comparable to the
awareness model session (1 new concept, pure function, test-heavy). Two sessions:
one for the two-axis model (awareness + output + voice), one for sentinel
extension and notification triggers.

---

## ADR gates

1. **JSON output schema change.** Adding `health` and `health_reasons` fields to
   StatusAssessment and SentinelPromiseState. Adding `visual_state` block to
   sentinel state file. Schema version bump (1 → 2). This is additive (new
   fields), not breaking (existing fields preserved). Likely does not need a new
   ADR — covered by existing ADR-105 (backward compatibility) with schema
   versioning.

2. **Prometheus metric addition.** New `urd_operational_health` gauge alongside
   existing `urd_promise_status`. Additive, no breakage. ADR-105 requires
   preserving existing metric names/labels/semantics — this is satisfied.

3. **CLI output vocabulary change.** Renaming PROTECTED→OK, AT RISK→aging,
   UNPROTECTED→gap in interactive output, and STATUS→SAFETY column header. This
   is a CLI presentation change, not a data contract change. **No ADR needed** —
   interactive output is not a backward-compatibility contract (only JSON, metrics,
   and on-disk formats are).

---

## Sequencing

**Session A: Two-axis awareness model + CLI rendering**
1. Add `OperationalHealth` enum to `awareness.rs`
2. Add health computation logic (chain health + space + drive mount state)
3. Extend `SubvolAssessment` with `health` and `health_reasons`
4. Update `output.rs` types
5. Update `voice.rs` interactive rendering (summary line, columns, temporal context)
6. Tests for all health computation paths
7. `/check` quality gate

**Session B: Sentinel visual state + notifications**
1. Add `VisualState` type and `compute_visual_state()` to `sentinel.rs`
2. Extend `SentinelStateFile` with `visual_state` block (schema version 2)
3. Add simultaneous-chain-break detection to `sentinel_runner.rs`
4. Add `HealthDegraded` and `DriveAnomalyDetected` to `notify.rs`
5. Tests for visual state computation and chain-break detection
6. `/check` quality gate

Session A can be implemented independently. Session B depends on Session A
(needs `OperationalHealth` from awareness).

---

## Rejected alternatives

**1. Four-state promise model (add "GUARDED" to the traffic light).**
Adding a fourth state to the existing single axis doesn't solve the fundamental
problem — it still conflates safety with health. More granularity on the wrong
axis is not helpful.

**2. Per-drive operational health instead of per-subvolume.**
The user asks "is htpc-home okay?" not "is WD-18TB1 okay?" Per-drive detail
belongs in `health_reasons`, not the top-level health status. This also avoids
an N×M explosion (subvolumes × drives) in the status table.

**3. Animated tray icon from day one.**
Animation requires a runtime framework (GTK main loop, Qt event loop) and
platform-specific tray APIs. Static icon swapping works everywhere, including
minimal tray implementations. Animation can be added later as a progressive
enhancement without changing the state file contract.

**4. Embedding voice/tooltip text in awareness.rs.**
The awareness module is a pure computation module (ADR-108). Generating
human-readable text is `voice.rs`'s job. The sentinel runner calls `voice.rs`
to generate tooltip/summary text from the assessment, then writes it to the
state file. This preserves the pure/presentation boundary.

---

## Assumptions

1. The sentinel state file is the right integration point for tray consumers.
   Assumption: file-watching is sufficient latency (sub-second with inotify).
   If tray consumers need push updates, a Unix socket would be needed instead.

2. Chain health information is available to `awareness.rs` through `FileSystemState`.
   Need to verify: the current `FileSystemState` trait may not expose pin file
   state directly. If not, the trait needs extension (affects `plan.rs` mock).

3. The `visual_state.icon` values are stable enough to be an implicit contract
   between the sentinel and tray applets. If icon states proliferate, this should
   become a versioned enum rather than string values.

4. Users will understand "OK/aging/gap" better than "PROTECTED/AT RISK/UNPROTECTED."
   This is a UX hypothesis, not a certainty. The words should be tested with
   real users before committing to the JSON output rename.

---

## Ready for Review

Focus areas for the arch-adversary:

1. **Two-axis coherence.** Does separating safety from health actually reduce
   confusion, or does it introduce a new conceptual burden? The user now has
   two things to check instead of one.

2. **OperationalHealth computation in awareness.rs.** The awareness module
   currently only needs snapshot timestamps and drive mount state. Adding chain
   health computation requires pin file state — does this violate the module's
   responsibility boundary? Or is "compute promise states" naturally extended
   to "compute promise states and operational readiness"?

3. **Visual state in the sentinel state file.** Is pre-computing tooltip text
   the right layering? Alternative: the tray applet imports `voice.rs` (or a
   shared library) and renders its own text. Pre-computation is simpler but
   couples the state file schema to the presentation layer.

4. **Vocabulary choice.** OK/aging/gap for safety. healthy/degraded/blocked for
   health. Are these the right words? "Gap" for UNPROTECTED is concrete but might
   not convey urgency. "Aging" is descriptive but might sound benign.

5. **Backward compatibility.** The design preserves `status` in JSON output and
   adds new fields alongside it. Is this sufficient? Should the old field be
   deprecated explicitly (with a timeline) or kept indefinitely?
