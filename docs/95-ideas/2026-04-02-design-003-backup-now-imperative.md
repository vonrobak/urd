---
upi: "003"
status: proposed
date: 2026-04-02
---

# Design: Backup-Now Imperative (UPI 003)

> **TL;DR:** `urd backup` typed by a human takes immediate action — fresh snapshots for
> all enabled subvolumes, sends to all connected drives, regardless of interval timers.
> Automated runs (systemd timer, cron) pass `--auto` to preserve current interval-gated
> behavior. The planner becomes mode-aware through `skip_intervals` in `PlanFilters`.

## Problem

When a user types `urd backup` in a terminal, the planner checks snapshot and send
intervals and often responds "nothing to do" because the nightly timer already ran.
This violates the Time Machine mental model: the user asked for a backup and got refused.

The interval logic exists to throttle *automated* runs, not to refuse *manual* ones.
Urd's two modes of existence (invisible worker / invoked norn) should have different
invocation semantics, but currently both share the same interval-gated path.

The current empty-plan response — `"Nothing to do."` in dim text — is dismissive when
a human explicitly asked for action. The invoked norn doesn't shrug.

## Proposed Design

### 1. CLI layer — `--auto` flag

Add an `--auto` flag to `BackupArgs` and `PlanArgs`. When present, the planner applies
interval checks (current behavior). When absent, intervals are skipped.

**Why `--auto`:** The flag appears in every systemd unit file, every cron job, every
automation script — it's Urd's most-read CLI argument. `--auto` reads naturally
(`ExecStart=urd backup --auto`) and describes the behavior it enables: "I'm an automated
run, apply the automated-run rules." Rejected alternatives:
- `--scheduled` — implementation concept, describes *when* not *what*
- `--unattended` — accurate but too long for the most common flag

**Why not TTY detection:** TTY detection (`stdout.is_terminal()`) is already used for
output mode selection (`OutputMode::detect()`). Reusing it to control *behavior* (not
just *presentation*) creates a coupling where piping output (`urd backup | tee log`)
silently changes what gets backed up. `--auto` is explicit, auditable in the systemd
unit, and visible in `ps` output. TTY detection stays in the presentation layer.

**Edge case — bare `urd backup` from a cron job:** Gets manual-mode semantics. This is
correct: a custom cron job is an intentional invocation. If interval gating is wanted,
add `--auto`.

**Module:** `src/cli.rs` — add `auto: bool` to `BackupArgs` and `PlanArgs`.

**Tests:** None needed — clap struct fields.

### 2. Planner — `skip_intervals` in `PlanFilters`

Add `skip_intervals: bool` to the existing `PlanFilters` struct. When true,
`plan_local_snapshot()` and `plan_external_send()` bypass their interval checks,
behaving as if the interval has always elapsed.

**Why in `PlanFilters`:** The `plan()` function signature stays unchanged. `PlanFilters`
already carries all "how should this plan be shaped" knobs (`priority`, `subvolume`,
`local_only`, `external_only`). `skip_intervals` is the same kind of shaping parameter.

**Why a bool, not `PlanMode` enum:** The behavioral difference is exactly one boolean
condition in two places. An enum adds a type without clarity. If a third mode emerges,
the refactor from bool to enum is trivial. YAGNI.

**Composition with `force`:** Both `plan_local_snapshot()` and `plan_external_send()`
already have a `force: bool` parameter (from `PlanFilters.subvolume` — forces a specific
subvolume regardless of interval). `skip_intervals` is semantically different: `force`
targets one subvolume, `skip_intervals` affects all. They compose naturally:
`if force || skip_intervals { true }`. When `--subvolume` is set, `force` already skips
the interval for the targeted subvolume, so `skip_intervals` is redundant but harmless.

**Safety invariant:** `skip_intervals` must NOT override the local space guard. The space
check happens before the interval check in both functions (plan.rs:262-278 for snapshots),
so `skip_intervals` slots in after the space guard — same as `force`.

**Module:** `src/plan.rs` — add field to `PlanFilters`, thread to `plan_local_snapshot()`
and `plan_external_send()`.

**Tests:**
- Existing tests continue to pass with `skip_intervals: false` (no behavior change)
- New: `skip_intervals_creates_snapshot_despite_recent_one` — snapshot 5min ago, interval
  1h, `skip_intervals: true` → snapshot planned
- New: `skip_intervals_sends_despite_recent_send` — send 1h ago, interval 4h,
  `skip_intervals: true` → send planned
- New: `skip_intervals_still_respects_space_guard` — even with skip_intervals, space
  guard must NOT be bypassed (load-bearing)
- New: `skip_intervals_still_runs_retention` — manual mode doesn't skip cleanup
- New: `skip_intervals_composes_with_filters` — skip_intervals + local_only still
  filters out external sends

### 3. Backup command — wire flag to planner

`src/commands/backup.rs` maps `args.auto` to `skip_intervals`:

```rust
let skip_intervals = !args.auto;
```

The `PlanFilters` construction adds `skip_intervals`. No other changes to backup command
logic — retention, locking, metrics, heartbeat, notifications all work unchanged.

**Lock trigger source:** Currently hardcoded to `"timer"` (line 84). Change to:

```rust
let trigger = if args.auto { "auto" } else { "manual" };
let _lock = lock::acquire_lock(&lock_path, trigger)?;
```

This makes lock metadata honest. Lock contention messages (lock.rs:63) already print the
trigger, so conflicts will now show `trigger: auto` or `trigger: manual`.

**Module:** `src/commands/backup.rs`

**Tests:** None needed at this layer — planner tests cover the logic, lock tests already
verify trigger strings.

### 4. Plan command — wire `--auto` flag

`src/commands/plan_cmd.rs` maps `args.auto` to `skip_intervals` in `PlanFilters`,
matching backup command semantics:

- `urd plan` (bare) → shows what a manual backup would do (skip intervals)
- `urd plan --auto` → shows what the timer would do (apply intervals)

This is a behavior change from today's `urd plan` (which currently shows the scheduled
view). The consistency argument is decisive: if `urd plan` says "nothing to do" but
`urd backup` takes action, that's confusing. `urd plan` in scripts is unlikely — it's a
preview command.

**Module:** `src/commands/plan_cmd.rs`

**Tests:** None needed — planner tests cover the logic.

### 5. Pre-action feedback (manual+TTY only)

When `!args.auto && !args.dry_run` and stdout is a terminal, print a brief summary of
what's about to happen *before* execution begins. This is the first beat of the narrative
arc: "Here's what I'm about to do → Here's what I'm doing → Here's what I did."

**Output structure (new types in `output.rs`):**

```rust
pub struct PreActionSummary {
    pub snapshot_count: usize,
    pub send_plan: Vec<PreActionDriveSummary>,
    pub disconnected_drives: Vec<DisconnectedDrive>,
    pub filters: PreActionFilters,
}

pub struct PreActionDriveSummary {
    pub drive_label: String,
    pub subvolume_count: usize,
    pub estimated_bytes: Option<u64>,
}

pub struct DisconnectedDrive {
    pub label: String,
    pub role: DriveRole,
}

pub struct PreActionFilters {
    pub local_only: bool,
    pub external_only: bool,
    pub subvolume: Option<String>,
}
```

**Rendering (in `voice.rs`):** Compact, natural language. Lead with the action and
destination, not the counts. Adapt the opening line to the operation shape:

**Full backup, one drive connected:**
```
Backing up everything to WD-18TB.
  7 snapshots, ~9.2GB to send

  WD-18TB1 is away — copies will update when it returns.
  2TB-backup not connected.
```

**Full backup, multiple drives connected:**
```
Backing up everything to WD-18TB and 2TB-backup.
  7 snapshots, ~18.4GB to send
```

**Local-only:**
```
Snapshotting 7 subvolumes.
```

**Single subvolume filter:**
```
Backing up htpc-home to WD-18TB.
  1 snapshot, ~1.2GB to send
```

**External-only:**
```
Sending to WD-18TB.
  7 subvolumes, ~9.2GB to send
```

**Drive role distinction:** Disconnected offsite drives show as "away — copies will
update when it returns" (expected lifecycle state). Disconnected primary drives show as
"not connected" (might warrant plugging in). The `DriveRole` enum provides this data.
In `--local-only` mode, disconnected drives are not shown (sends aren't part of the plan).

**Construction:** Built from the `BackupPlan` after planning, before execution. Count
`CreateSnapshot` operations, group `Send*` operations by drive, sum estimates.
Disconnected drives from the plan's skipped list (category = DriveNotMounted), enriched
with `DriveRole` from config.

**Module:** `src/output.rs` (types), `src/voice.rs` (rendering), `src/commands/backup.rs`
(construction and printing).

**Tests:**
- `voice.rs`: render tests for each operation shape (full/local/external/filtered/
  multi-drive), disconnected drive role variants, no-estimates case
- `output.rs`: `PreActionSummary` construction test from a mock `BackupPlan`

### 6. Mode-aware empty-plan messaging

When `!args.auto` and the plan is empty, explain why instead of printing "Nothing to do."
The explanation is derived from the skipped reasons already in the plan.

**Possible empty-plan causes in manual mode** (intervals can't cause it):
- All subvolumes disabled
- `--external-only` with no drives connected
- `--subvolume foo` where `foo` doesn't exist or is disabled
- All subvolumes hit the space guard (filesystem full)

**Output structure (new type in `output.rs`):**

```rust
pub struct EmptyPlanExplanation {
    pub reasons: Vec<String>,
    pub suggestion: Option<String>,
}
```

**Rendering example:**
```
Nothing to back up — all subvolumes are disabled in config.
  Enable subvolumes in ~/.config/urd/urd.toml
```

**Module:** `src/output.rs` (type), `src/voice.rs` (rendering), `src/commands/backup.rs`
(construction — replaces current `"Nothing to do."` path for manual mode).

**Tests:**
- `voice.rs`: render test for each cause
- Planner tests verify skip reasons are present for each case

### 7. Dry-run behavior

`--dry-run` inherits mode from `--auto`. `urd backup --dry-run` shows the manual plan
(skip intervals). `urd backup --auto --dry-run` shows the scheduled plan. No special
handling — `skip_intervals = !args.auto` is computed before the dry-run check, and the
plan rendering path uses whatever plan was computed.

### 8. Systemd timer migration

The systemd timer unit is not shipped in the repo — deployed by the user. Migration:

1. **CHANGELOG entry:** Note that `urd backup` now runs unconditionally; systemd timer
   units should add `--auto`.
2. **`urd init` guidance:** Updated output mentions `--auto` for timer units.

No `urd doctor` check for systemd unit files — scanning unit file paths is fragile
(non-standard paths, multiple units). The CHANGELOG + init guidance is sufficient.

### 9. Backward compatibility (ADR-105)

**No on-disk format changes.** Snapshots, pin files, metrics, heartbeat — all unchanged.
The `--auto` flag is additive CLI surface. Existing `urd backup` invocations get new
behavior (manual mode), which is the *intended* semantic change — the old behavior
(interval-gated manual runs) was a bug, not a feature.

**Existing scripts that call `urd backup`:** Will now produce more snapshots. This is
safe — retention cleans them up. If a script needs the old behavior, add `--auto`.

### 10. Architectural question: `INVOCATION_ID` check

**For arch-adversary review.** The existing `INVOCATION_ID` check at backup.rs:125 gates
chain-break full sends (`FullSendPolicy::SkipAndNotify`) in systemd contexts. This serves
a related but distinct purpose to `--auto` — gating expensive operations in unattended
contexts.

**Question:** Should `args.auto` replace the `INVOCATION_ID` check for consistency? The
two signals overlap but aren't identical:
- `--auto` means "apply automated-run rules" (user-declared intent)
- `INVOCATION_ID` means "running inside systemd" (environmental fact)

A manual `urd backup` inside a systemd oneshot would see different behavior depending on
which signal is used. Replacing `INVOCATION_ID` with `args.auto` is cleaner (one signal
for "automated") but loses the environmental safety net. Keeping both means two detection
mechanisms for overlapping concepts.

**Current recommendation:** Do not change in this PR. Flag for arch-adversary review.

## Module Map

| Module | Changes | Test strategy |
|--------|---------|---------------|
| `src/cli.rs` | Add `auto: bool` to `BackupArgs` and `PlanArgs` | Covered by integration |
| `src/plan.rs` | Add `skip_intervals: bool` to `PlanFilters`, thread to `plan_local_snapshot()` and `plan_external_send()` | 5 new unit tests |
| `src/commands/backup.rs` | Map `!args.auto` → `skip_intervals`, fix lock trigger, build + print `PreActionSummary`, mode-aware empty plan | Existing tests + manual verification |
| `src/commands/plan_cmd.rs` | Map `!args.auto` → `skip_intervals` in `PlanFilters` | Existing tests |
| `src/output.rs` | Add `PreActionSummary`, `PreActionDriveSummary`, `DisconnectedDrive`, `PreActionFilters`, `EmptyPlanExplanation` types | Construction tests |
| `src/voice.rs` | Add `render_pre_action()` and `render_empty_plan()` functions | 6-8 render tests |

## Effort Estimate

**~1 session.** Calibration:
- UUID fingerprinting (1 module, 10 tests): 1 session
- `urd get` (1 new command, 19 tests): 1 session

Comparable scope — core planner change is mechanical, pre-action summary follows
established output.rs/voice.rs patterns, empty-plan messaging is small. The plan command
wiring is a few lines.

## Sequencing

1. **Planner `skip_intervals` in `PlanFilters`** — core behavior change, all planner
   tests. Risk: none, pure function with existing test patterns.
2. **CLI `--auto` flag + backup/plan command wiring + lock trigger fix** — connects
   planner to user. Risk: low, verify existing tests still pass.
3. **Pre-action summary** — output.rs types, voice.rs rendering, backup.rs construction.
   Risk: low, follows established patterns. Build last because it depends on the plan.
4. **Empty-plan messaging** — small addition, depends on the mode being wired.

## Architectural Gates

**None for this PR.** The `INVOCATION_ID` question (section 10) is flagged for
arch-adversary review but does not block implementation.

## Rejected Alternatives

### TTY detection for mode selection

Rejected: couples *behavior* to *presentation*. Piping `urd backup | tee backup.log`
would silently switch to interval-gated mode. TTY detection stays in the presentation
layer (`OutputMode::detect()`).

### `PlanMode` enum instead of `skip_intervals: bool`

Rejected: one boolean checked in two places doesn't warrant an enum. YAGNI. Refactor is
trivial if a third mode emerges.

### Confirmation prompt before manual backup

Rejected: user already typed `urd backup`. Pre-action summary provides visibility without
blocking. `--dry-run` exists for preview-before-commit.

### `--force` flag instead of `--auto`

Rejected: inverts the mental model. The human at the terminal is the default case, the
timer is the special case. The special case gets the qualifier.

### `--scheduled` flag name

Rejected: implementation concept, describes *when* not *what*. `--auto` reads like
English in unit files and describes the behavior it enables.

### Doctor check for systemd unit files

Rejected: scanning unit file paths is fragile (non-standard paths, multiple units).
CHANGELOG + init guidance is sufficient.

## Assumptions

1. **The systemd timer is the primary automated caller.** Custom cron jobs calling
   `urd backup` without `--auto` get manual semantics — intentional invocation.

2. **`force` and `skip_intervals` compose without conflict.** `force` is a subset of
   `skip_intervals`. When both are true, behavior equals `skip_intervals` alone.

3. **Pre-action summary can be built from `BackupPlan` alone.** Plan contains all
   operations and skipped reasons. Drive roles come from config (already available in
   backup command scope). No additional filesystem queries needed.

4. **Retention behavior is identical in both modes.** Manual runs that create extra
   snapshots see those snapshots subject to normal retention on the next run.

5. **Space guard is never bypassed.** Space check precedes interval check in the planner.
   `skip_intervals` slots in after the space guard, same as `force`.

## Open Questions

**All resolved.** See Resolved Decisions below.

## Resolved Decisions

Decisions resolved during `/grill-me` session (2026-04-02), incorporating Steve Jobs
review findings from `docs/99-reports/2026-04-02-steve-jobs-003-backup-now-almost-right.md`.

| # | Decision | Resolution | Rationale |
|---|----------|------------|-----------|
| 1 | Flag name | `--auto` | Reads naturally in unit files, describes behavior not scheduling |
| 2 | Lock trigger source | Include: `"auto"` vs `"manual"` | One-line fix, directly adjacent, makes metadata honest |
| 3 | `urd plan` gains `--auto` | Yes, same PR | Consistency — plan should preview what backup would do |
| 4 | Pre-action summary tone | Lead with action+destination, adapt to operation shape | Narrative arc: briefing from authority, not spreadsheet |
| 5 | Empty-plan messaging | Include mode-aware explanation | Core pain point; dismissive response undermines the feature |
| 6 | Parameter location | `skip_intervals: bool` in `PlanFilters` | Keeps `plan()` signature stable, groups with plan-shaping knobs |
| 7 | Pre-action in auto mode | Manual+TTY only | Daemon path has backup summary; pre-action serves watching humans |
| 8 | `INVOCATION_ID` replacement | Flag for arch-adversary, don't change | Overlapping but distinct signals; needs architectural scrutiny |
| 9 | Dry-run behavior | Inherits mode from `--auto` | Falls out naturally, no special handling |
| 10 | `--subvolume` composition | No action, composes via `force` | Existing mechanism handles it |
| 11 | Vocabulary: "away" | Use "away" for offsite drives | Load-bearing vocabulary for off-site lifecycle state |
| 12 | Drive role in pre-action | Distinguish by `DriveRole` | Offsite="away", primary="not connected"; data exists |
| 13 | Scope boundary | Confirmed | See Architectural Gates and section 10 |
