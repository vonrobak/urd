# Design: Post-Backup Structured Summary (2b + 2d)

> **TL;DR:** Unify skip surfacing (2b) and post-backup summary (2d) into a single
> `BackupSummary` output type rendered by voice.rs. The backup command produces structured
> data; the presentation layer answers "is my data safer now?" in one screen. 2b is a
> subset of 2d — skip reasons are one section of the summary.

**Date:** 2026-03-26
**Status:** proposed
**Depends on:** Awareness model (3a, complete), Presentation layer (3c, complete), Heartbeat (3b, complete)

## Problem

After a backup run, the user must run three commands (`status`, `verify`, `history`) to
answer "is my data safe?" The backup command currently uses direct `println!` with ad-hoc
formatting. Most nightly runs are all-skips (drives not mounted during daily timer), but
skip reasons are buried among other output — the most important information is the least
visible.

The returning-from-travel experience (2026-03-26 operational evaluation) proved this: coming
back to a system that had been running for days, the user needed three commands to understand
what had happened. The backup output itself didn't answer the question.

Priority 2b (surface skipped sends loudly) and 2d (post-backup structured summary) are
the same feature at different scopes. 2b is "make skip reasons prominent." 2d is "make
the entire post-backup output structured and answerable." Building 2d subsumes 2b.

## Proposed Design

### Data Structure: `BackupSummary` (in `output.rs`)

A new structured output type following the established pattern (`StatusOutput`, `GetOutput`):

```rust
/// Structured output for the post-backup summary.
#[derive(Debug, Serialize)]
pub struct BackupSummary {
    /// Overall run result.
    pub result: String,                     // "success" | "partial" | "failure"
    /// Run ID from SQLite (if available).
    pub run_id: Option<i64>,
    /// Total wall-clock duration of the run.
    pub duration_secs: f64,

    /// Per-subvolume execution results.
    pub subvolumes: Vec<SubvolumeSummary>,
    /// Subvolumes skipped by the planner (name, reason).
    pub skipped: Vec<SkippedSubvolume>,

    /// Per-subvolume promise status AFTER the run (from awareness model).
    pub assessments: Vec<StatusAssessment>,  // reuse existing type

    /// Summary warnings (pin failures, skipped deletions, etc.)
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SubvolumeSummary {
    pub name: String,
    pub success: bool,
    pub duration_secs: f64,
    pub send_type: String,       // "full" | "incremental" | ""
    pub send_drive: Option<String>,
    pub bytes_transferred: Option<u64>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SkippedSubvolume {
    pub name: String,
    pub reason: String,
}
```

### Data Flow

```
plan::plan()           → BackupPlan { operations, skipped }
executor.execute()     → ExecutionResult { overall, subvolume_results, run_id }
awareness::assess()    → Vec<SubvolAssessment>    [called post-execution, fresh timestamp]
                         ↓
backup.rs: build_backup_summary(plan, result, assessments)  →  BackupSummary
                         ↓
voice::render_backup_summary(summary, mode)  →  String
```

The `build_backup_summary()` function lives in `commands/backup.rs` — it's the command
layer assembling the output type from business logic results. This follows the pattern
established by the status command: the command function builds the structured data, then
calls voice to render it.

### Rendering: `voice::render_backup_summary()` (in `voice.rs`)

**Interactive mode** — designed around Priority 2b's insight that skip reasons are the
most important output of a no-op run:

```
── Urd backup: success ── [run #47, 12.3s] ──────────────

  OK   htpc-home       [2.1s]
  OK   htpc-docs       [0.3s] (incremental → WD-18TB)
  OK   htpc-root       [0.1s]

  SKIP htpc-home       drive 2TB-backup not mounted
  SKIP htpc-docs       drive 2TB-backup not mounted
  SKIP subvol3-opptak  drive WD-18TB not mounted
  SKIP subvol3-opptak  drive 2TB-backup not mounted
  SKIP subvol3-opptak  send to 2TB-backup skipped: estimated ~3.4 TB exceeds 1.0 TB available

  STATUS     SUBVOLUME       LOCAL  WD-18TB
  PROTECTED  htpc-home       12     47
  AT RISK    htpc-docs       5      3
  PROTECTED  htpc-root       3      —
  AT RISK    subvol3-opptak  8      —

  NOTE htpc-docs: offsite drive not connected in 14 days
```

Key rendering decisions:
1. **Header line** with result, run ID, and total duration — answers "did it work?"
2. **Executed subvolumes first** — what changed this run.
3. **Skipped subvolumes block** — prominent, not dimmed. The 2b requirement: most runs are
   all-skips, so this block IS the output. Group by subvolume to reduce repetition.
4. **Awareness table** — the status table, reusing the existing `render_subvolume_table()`
   helper from voice.rs. Answers "is my data safe NOW?" without running a second command.
5. **Advisories/warnings** last — pin failures, skipped deletions, etc.

**Daemon mode** — JSON serialization of `BackupSummary` (same as StatusOutput pattern).

### Skip Reason Grouping (2b specifics)

The planner produces per-subvolume-per-drive skips. A 7-subvolume config with 2 unmounted
drives generates 14 skip entries, most saying "drive X not mounted." The raw list is noise.

The renderer groups skips by reason when displaying:

```
  Drives not mounted: 2TB-backup, WD-18TB
    → 5 sends skipped (htpc-home, htpc-docs, htpc-root, subvol3-opptak, subvol5-music)

  SKIP subvol3-opptak  estimated ~3.4 TB exceeds 1.0 TB available on 2TB-backup
```

Common patterns ("drive X not mounted") are collapsed. Unique reasons (space, UUID mismatch,
disabled, low local space) are shown individually. This is a rendering decision — the
structured data retains per-subvolume-per-reason granularity for daemon mode consumers.

### Module Changes

| Module | Change | Scope |
|--------|--------|-------|
| `output.rs` | Add `BackupSummary`, `SubvolumeSummary`, `SkippedSubvolume` types | New types only |
| `voice.rs` | Add `render_backup_summary()` + helpers | ~120 lines |
| `commands/backup.rs` | Replace `println!` block with `build_backup_summary()` + voice render | Refactor existing ~90 lines |

No changes to: plan.rs, executor.rs, awareness.rs, types.rs, heartbeat.rs.

### What Does NOT Change

- **Planner skip format** — `Vec<(String, String)>` stays. The `(name, reason)` tuples are
  adequate. Structured skip reasons (typed enum) would be nice but is scope creep — the
  string reasons are human-readable and the renderer can pattern-match on known prefixes
  for grouping.
- **ExecutionResult** — no fields added. The summary is assembled from existing data.
- **Heartbeat** — still written separately, already has awareness data. The summary is a
  display concern, not a persistence concern.
- **Metrics** — untouched. Already correct.

## Invariants

1. **ADR-100 (planner/executor separation)** — Preserved. The planner produces skip reasons
   as data. The executor produces results as data. The command layer assembles. The voice
   layer renders. No module crosses its boundary.

2. **ADR-108 (pure function modules)** — `build_backup_summary()` is a pure function: plan +
   result + assessments in, `BackupSummary` out. `render_backup_summary()` is a pure function:
   summary + mode in, string out.

3. **No new public contracts** — `BackupSummary` is consumed only by voice.rs. Not written
   to disk, not part of the heartbeat schema, not exposed as API. It can evolve freely.

4. **Backward compatibility** — The only observable change is the backup command's stdout
   output format. No external contracts (metrics, pin files, snapshot names) are affected.

## Integration Points

- **`StatusAssessment` reuse** — the summary embeds the same assessment type used by
  `urd status`. The awareness table in the backup output and `urd status` show the same
  data from the same computation, rendered by the same table helper.

- **`render_subvolume_table()` extraction** — currently a private function in voice.rs called
  by `render_status_interactive()`. Needs to become a shared helper within voice.rs (not
  `pub` — just used by both `render_status_interactive` and `render_backup_interactive`).

- **Empty run path** — the `backup_plan.is_empty() && backup_plan.skipped.is_empty()` early
  return in backup.rs currently prints "Nothing to do." and writes heartbeat. This path
  should also use the summary (a summary with no subvolumes and no skips renders as the
  "nothing to do" message). Unifies the output path.

- **Awareness call already exists** — backup.rs already calls `awareness::assess()` for the
  heartbeat. The summary reuses the same assessment result. No additional filesystem queries.

## Implementation Sequence

**Step 1: Output types** (~15 min)
Add `BackupSummary`, `SubvolumeSummary`, `SkippedSubvolume` to `output.rs`.
Test: types derive Serialize, can construct from test data.

**Step 2: Build function** (~30 min)
Write `build_backup_summary()` in `commands/backup.rs`. Pure function taking
`&BackupPlan`, `&ExecutionResult`, `&[SubvolAssessment]`, total duration, and returning
`BackupSummary`.
Test: unit test with mock plan/results verifying field mapping.

**Step 3: Interactive renderer** (~45 min)
Write `render_backup_summary()` in `voice.rs`. Extract `render_subvolume_table()` as
shared helper. Implement skip grouping logic.
Test: 6–8 tests covering normal run, all-skips run, partial failure, empty run, skip
grouping, daemon JSON.

**Step 4: Wire into backup command** (~30 min)
Replace the `println!` block in `backup.rs:run()` (lines 114–204) with:
1. `build_backup_summary()` call
2. `render_backup_summary()` call
3. `println!("{}", rendered)`
Preserve: metrics writing, heartbeat writing, exit code logic.
Also handle the empty-run early return path.

**Effort estimate:** ~2 hours. Comparable to presentation layer (output.rs + voice.rs)
which was 15 voice tests + 4 output tests in one session. The infrastructure exists —
this is adding a new consumer of established patterns.

## Rejected Alternatives

### 1. Typed skip reason enum

Instead of `Vec<(String, String)>`, define `SkipReason::DriveNotMounted { drive }`,
`SkipReason::SpaceInsufficient { .. }`, etc. This would make grouping trivial.

**Rejected because:** Requires changing `BackupPlan.skipped` (a types.rs public type used by
planner, executor, backup, plan_cmd, and metrics). The grouping logic in the renderer is
~20 lines of prefix matching. The typed enum buys cleanliness at the cost of a larger change
surface. Can be revisited if the renderer's grouping becomes fragile.

### 2. Separate 2b and 2d as independent features

Build skip surfacing first (just change the `println!` for skips), then build the full
summary later.

**Rejected because:** 2b's "prominent warning block" IS part of the summary. Building 2b
standalone would create temporary formatting code that gets replaced by 2d. The effort
difference is small (~30 min for 2b alone vs. ~2h for 2d-which-includes-2b), and 2d's
infrastructure (the output type) immediately pays off when other commands migrate to voice.

### 3. BackupSummary in a new module (e.g., `summary.rs`)

**Rejected because:** Output types live in `output.rs`, rendering lives in `voice.rs`,
command logic lives in `commands/`. The existing module structure handles this cleanly.
A new module for one type and one builder function doesn't earn its keep.

### 4. Include chain health in the summary

The status command shows chain health. Should the backup summary also show it?

**Rejected for v1:** Chain health requires reading pin files and external snapshots, which
is already done by the awareness model (indirectly). But the status command does its own
chain health computation. Adding it to the summary would either duplicate the computation
or require refactoring chain health into a shared function. Not worth it for v1 — the
awareness model's promise status already tells the user if something is wrong.

## Open Questions

1. **Skip grouping heuristics** — The design proposes grouping "drive X not mounted" skips.
   Should this be by drive (group all subvolumes skipped for the same drive) or by reason
   text (exact string match)? Drive-based grouping is more semantic but requires parsing
   the reason string. Recommendation: group by exact reason prefix match on
   `"drive {label} not mounted"`, which is stable and covers the dominant case.

2. **Empty run rendering** — When `is_empty() && skipped.is_empty()`, the current code
   prints "Nothing to do." Should this flow through the summary path (producing a summary
   with zero subvolumes and zero skips, rendered as a brief message) or remain as a
   special case? Recommendation: flow through the summary path for consistency, but keep
   the rendered output minimal (one line).

3. **Awareness table verbosity** — In a 7-subvolume config, the awareness table adds 9 lines
   to every backup output. For the daemon (systemd journal), this might be excessive.
   Options: (a) always show, (b) show only if any subvolume is AT RISK or UNPROTECTED,
   (c) configurable. Recommendation: (b) — show the table only when there's something worth
   noting, with a one-line "all subvolumes PROTECTED" summary otherwise. This aligns with
   the "invisible worker" principle: silence is a good sign.

## Ready for Review

Focus areas for the arch-adversary:

1. **Is reusing `StatusAssessment` the right call?** It was designed for `urd status`. The
   backup summary embeds the same type. If `StatusAssessment` evolves for status-specific
   needs, the backup summary inherits those changes. Is this coupling acceptable or should
   the summary have its own assessment type?

2. **Skip grouping in the renderer vs. structured data.** The design keeps string-based skip
   reasons in the data layer and does grouping in the renderer. The alternative (typed enum)
   would be cleaner but touches more files. Is the current approach fragile enough to warrant
   the larger change?

3. **The empty run path.** Currently a special case with early return. Should it flow through
   the summary, or is the early return better for clarity?

4. **Awareness table in daemon mode.** The daemon JSON includes the full assessment array.
   Is this the right default, or should daemon mode omit it (the heartbeat already persists
   this data)?

5. **Backward compatibility of stdout format.** The backup command's stdout will change
   format. Any downstream consumers (scripts, monitoring) that parse the old format will
   break. Is this a concern worth an ADR, or is stdout format explicitly not a contract?
