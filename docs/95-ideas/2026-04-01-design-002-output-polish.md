---
upi: "002"
status: promoted
date: 2026-04-01
---

# Design: Output Polish (UPI 002)

> Make v0.7.0's terminal output correct, clean, and honest.

## Context

User testing of v0.7.0 captured in `output-urd-v.0.7.0.md` revealed bugs in the
backup progress display, presentation noise in backup/status output, misleading
data in the status table, and log leakage into interactive commands. A detailed
decision session resolved all questions — this design encodes 16 decisions.

## Decision Record

Decisions made in conversation, referenced by ID below.

| ID | Decision |
|----|----------|
| D1a | Pipe btrfs receive stdout to `Stdio::null()` |
| D1b | Executor prints completions synchronously; progress thread only renders after >1s |
| D1c | Completion lines only for sends >1s |
| D2a | Default: "All connected drives are sealed." Exclude disabled subvolumes. Surface health. |
| D2b | No absent drive mentions in default |
| D3a | Group `[WAIT]` in backup output, same as plan |
| D3b | Suppress `[WAIT]` when `[OFF]` present for same subvolume |
| D3c | Only show skipped for absent drives. No tags. `Drives disconnected: X, Y` / `N send(s) skipped` |
| D4a | Hide PROTECTION column by default. Show when exposure conflicts with promise. |
| D4b | THREAD stays. Collapse disconnected drive columns (only show connected). |
| D4c | Hide RECOVERY column until it reports real snapshot depth from disk. |
| D5a | Doctor warnings include concrete numbers |
| D5b | Doctor suggests the fix with `→` pattern |
| D6a | Move UUID warning to `urd doctor` only |
| D6b | Suppress log output for interactive TTY commands |

## Module Map

Six modules affected, no new modules.

### 1. `btrfs.rs` — D1a

**Change:** Add `.stdout(Stdio::null())` to `btrfs receive` spawn (line ~167).

**Scope:** 1 line. The data pipeline flows through stdin; stdout is informational
only ("At snapshot ..."). Suppressing it removes terminal pollution during sends.

**Test:** Existing send/receive integration tests cover correctness. No new tests
needed — stdout was never consumed.

### 2. `commands/backup.rs` — D1b, D1c, D3a, D3b, D3c

**D1b/D1c: Progress and completion refactor.**

Current flow:
```
executor runs send → resets byte counter → updates ProgressContext mutex
progress thread polls counter every 250ms → reads context → prints progress/completion
```

New flow:
```
executor runs send → on completion, synchronously prints ✓ line (if duration >1s)
progress thread polls counter → only renders when send has been active >1s
```

Changes:
- `progress_display_loop`: remove completion-line logic. Only render progress when
  elapsed since send-start >1s. The thread becomes display-only, never announces
  completions.
- `format_completion_line` stays (reused by the synchronous path), `format_progress_line`
  stays.

**Completion lines print inside `executor.rs`**, in `execute_send()`, after
`send_receive()` returns successfully. This is where `ProgressContext` is already
updated and all needed data is available (name, drive, bytes, duration, send_type).
Gated on: `progress_context.is_some()` (opt-in via `set_progress()`) AND
`duration > 1s`. Tests and daemon mode see no change.

**Mutex protocol** (prevents interleave with progress thread):
1. Lock ProgressContext
2. Clear progress line (`\r\x1b[2K` on stderr)
3. Print completion line
4. Update context for the next send
5. Release lock

Document this protocol in a comment at the ProgressContext definition.

**D3a/D3b/D3c: Backup skipped section refactor.**

Current `render_skipped_block` groups drive-not-mounted entries but lists `[WAIT]`
and `[OFF]` individually. Changes:

- Only render absent-drive skips. Format:
  ```
  Drives disconnected: WD-18TB1, 2TB-backup
    11 send(s) skipped
  ```
  No `[AWAY]` tag. No arrow prefix.
- Suppress all `[WAIT]` lines entirely.
- Suppress `[OFF]` lines entirely.
- The summary line at the bottom still includes the total skipped count.

**Test strategy:** Update existing `render_skipped_block` tests. Add test for
`[WAIT]`+`[OFF]` same-subvolume suppression. Add test that sub-second sends produce
no completion line.

### 3. `commands/default.rs` + `output.rs` — D2a, D2b

**D2a: "All connected drives are sealed" + health.**

**Disabled subvolume filtering: deferred.** Design review and grill-me revealed that
the "disabled" concept is entangled with a known issue (`assess()` doesn't respect
per-subvolume `drives` scoping — see status.md). The user's htpc-root shows false
degradation because awareness checks all drives, not just configured ones. Until that
is fixed, filtering by health state in the default would hide real problems or surface
false ones. For now, include all subvolumes in the default count.

`DefaultStatusOutput` needs two new fields:
- `degraded_count: usize` — count of non-healthy subvolumes
- `blocked_count: usize` — count of blocked subvolumes

`voice.rs` `render_default_status_interactive` changes:
- "All sealed" → "All connected drives are sealed" when all are sealed.
- Append health clause: "1 degraded." / "1 blocked." when counts > 0.

**D2b:** No changes needed — absent drives already excluded from default.

**Test strategy:** Update existing `render_default_status_interactive` tests. Add test
for health degradation surfacing.

### 4. `commands/status.rs` + `voice.rs` — D4a, D4b, D4c

**D4a: Hide PROTECTION by default.**

In `render_subvolume_table`, change `has_promises` logic:
```rust
// Show PROTECTION only when exposure conflicts with promise
let show_protection = data.assessments.iter().any(|a| {
    a.promise_level.is_some() && a.status != "PROTECTED"
});
```

**D4b: Collapse disconnected drive columns.**

In `render_subvolume_table`, filter `data.drives` to only mounted drives:
```rust
let visible_drives: Vec<_> = data.drives.iter().filter(|d| d.mounted).collect();
```

Use `visible_drives` for both headers and row cells. The drive summary section below
the table already reports absent drives — no information lost.

**D4c: Hide RECOVERY.**

Remove the `has_retention` conditional and the RECOVERY column entirely. Remove
`retention_summary` field from `StatusAssessment` if desired, or just stop populating
it. Prefer: stop populating in `commands/status.rs` (set to `None`), leave the field
for future use.

**Test strategy:** Update table rendering tests. Add test for protection column
showing when exposure conflicts. Add test for disconnected drive column collapse.

### 5. `commands/doctor.rs` + `voice.rs` — D5a, D5b

**D5a/D5b: Concrete numbers + suggestion in doctor warnings.**

The preflight check that produces the "snapshot_interval is longer than guarded
baseline" warning lives in `preflight.rs`. The `DoctorCheck` struct has `name`,
`status`, and optional `detail`/`suggestion` fields.

Changes to `preflight.rs` or the doctor check builder:
- Include the actual interval and the required interval in the warning message.
- Add a suggestion: `→ reduce snapshot_interval to {required}, or change protection to custom`

Use current terminology (guarded/protected/resilient) — P6a will rename later.

**Test strategy:** Update existing preflight/doctor tests to assert concrete numbers
in output.

### 6. `drives.rs` + `commands/doctor.rs` + `main.rs` — D6a, D6b

**D6a: UUID warning → doctor only.**

- Remove `warn_missing_uuids()` calls from `commands/backup.rs` and
  `commands/plan_cmd.rs`.
- Add a doctor check in `commands/doctor.rs` that calls the same detection logic
  from `drives.rs` and produces a warning with the UUID and copy-paste config line.
- `warn_missing_uuids()` function can be repurposed or a new
  `check_missing_uuids() -> Vec<DoctorCheck>` created.

**D6b: Suppress logs for interactive TTY.**

In `main.rs`, change the log level for interactive (TTY) mode:
```rust
let log_level = if cli.verbose {
    log::LevelFilter::Debug
} else if std::io::stderr().is_terminal() {
    log::LevelFilter::Error  // suppress WARN on TTY
} else {
    log::LevelFilter::Warn   // daemon/pipe mode keeps WARN
};
```

This suppresses WARN-level log output when running interactively. Errors still
surface (they indicate real failures). `--verbose` overrides to Debug regardless.
`RUST_LOG` env var still overrides via `parse_default_env()`.

**Note:** This also suppresses sentinel lifecycle WARN logs (`"Sentinel starting"`,
`"Sentinel shutting down"`) when running `urd sentinel run` interactively. Accepted
trade-off: sentinel is a daemon, interactive use is a debugging scenario where
`--verbose` is natural. Add a comment in main.rs explaining the suppression rationale.

**Test strategy:** No unit test for log level (env_logger init is global). Manual
verification: `urd plan` should produce no log lines; `urd plan --verbose` should.

## Effort Estimate

- **D1 (progress/receive):** ~0.5 session. Mostly mechanical — pipe stdout, move
  completion to executor, simplify progress thread.
- **D2 (default command):** ~0.5 session. Small struct changes, voice text update,
  defining "disabled" precisely.
- **D3 (backup skipped):** ~0.25 session. Simplify existing render function.
- **D4 (status table):** ~0.25 session. Conditional logic changes in existing render.
- **D5 (doctor):** ~0.25 session. String formatting changes.
- **D6 (logs):** ~0.25 session. Remove calls, add doctor check, change log init.

**Total: ~2 sessions.** Comparable to the retention preview + doctor feature (P2b + 6-N).

## Sequencing

1. **D1 first.** The progress bugs are the most impactful and the fix is self-contained
   in btrfs.rs + backup.rs. Verifiable immediately with `urd backup`.
2. **D6 second.** Suppressing log noise makes it easier to evaluate subsequent output
   changes visually.
3. **D3 third.** Backup skipped cleanup — simple, no dependencies.
4. **D4 fourth.** Status table changes — independent of backup changes.
5. **D2 fifth.** Default command depends on understanding what "disabled" means in
   the resolved config, which may surface questions during D4 work.
6. **D5 last.** Doctor polish — lowest risk, smallest impact.

## Architectural Gates

None. All changes are within existing module boundaries. No new public contracts,
no on-disk format changes, no new ADRs needed. The only subtlety is D6b (log level
change) which affects daemon mode — but `parse_default_env()` preserves `RUST_LOG`
override, and the change only suppresses WARN on TTY, not in pipes/daemons.

## Rejected Alternatives

- **Channel-based progress (D1b option b):** More correct but significantly more
  complex. The executor would need to push typed events through a channel, the
  progress thread would need to handle event ordering, and the synchronization
  model changes from polling to message-passing. Not justified when the simpler
  fix (synchronous completions + suppress sub-second progress) handles all observed
  symptoms.

- **Show RECOVERY with policy labels (D4c option b):** Relabeling the column as
  "POLICY" and keeping it visible was considered. Rejected because the column
  occupied space for information the user already knows (they wrote the config).
  When it returns, it should show real depth — that's the version that earns space.

- **Absent drive mentions in default (D2b options b/c):** Adding drive absence
  to the one-liner turns it into a dashboard. "All connected drives are sealed"
  already scopes the claim honestly. Drive absence that causes waning/exposed
  states surfaces through the exposure counts.

## Assumptions

1. **Disabled subvolume filtering deferred.** The original assumption about what
   "disabled" means was wrong (multimedia and tmp have `protection_level = "guarded"`,
   not `None`). The deeper issue is that `assess()` doesn't respect per-subvolume
   `drives` scoping (status.md known issue), causing false health degradation. D2a
   proceeds without filtering — all subvolumes are included in the default count.

2. The executor has access to send duration at the point where it can print
   completion lines. Verified: `execute_send()` has `start.elapsed()`,
   `subvol_name`, `drive_label`, `send_type`, and `result.bytes_transferred`.

3. Suppressing WARN logs on TTY won't hide critical information. Reviewed all
   current WARN-level log sites: UUID missing (moved to doctor), preflight
   (already in backup summary), heartbeat write failures (would be ERROR if
   critical). Sentinel lifecycle logs suppressed on TTY — accepted, documented.

## Review Status

Design reviewed 2026-04-01. Report: `docs/99-reports/2026-04-01-design-review-002-output-polish.md`.
Grill-me resolved all four findings:

| Finding | Resolution |
|---------|-----------|
| F1: "Disabled" definition wrong | Deferred. Entangled with `assess()` drive scoping known issue. |
| F2: Executor API for completions | Print inside `execute_send()`, gated on progress context + >1s. |
| F3: Progress thread race variant | Mutex protocol documented. Lock covers full clear-print-update. |
| F4: Sentinel log suppression | Accepted. `--verbose` for interactive debugging. Documented. |

## Open Questions (post-review)

1. When all external drives are disconnected, the status table has zero drive
   columns. Is the drive summary section below sufficient context, or should
   there be a different rendering?

2. After D3c removes `[WAIT]`/`[OFF]` detail from backup output, should the
   summary line also drop the skipped count, or does the number serve as a
   "there's more going on" signal?
