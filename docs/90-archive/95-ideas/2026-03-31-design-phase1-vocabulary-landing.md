# Design: Phase 1 — Vocabulary Landing

> **TL;DR:** Land all resolved vocabulary decisions from the brainstorm/grill-me sessions
> into presentation-layer code. No data structures change. No new computation. Every
> user-facing string speaks the final language so all subsequent phases build on stable
> vocabulary. Purely mechanical — the editorial work is already done.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** 6-E merged (ready now)

---

## Problem

Urd's presentation layer carries inconsistent vocabulary across four dimensions:

1. **Safety labels.** Awareness model says `PROTECTED`/`AT RISK`/`UNPROTECTED`. Voice.rs
   maps to `OK`/`aging`/`gap`. The brainstorm resolved: **sealed/waning/exposed**.

2. **Chain terminology.** `notify.rs` says "thread." `voice.rs` says "chain." `cli.rs`
   says "chain." The brainstorm resolved: **thread** everywhere in presentation.

3. **Drive status.** `voice.rs` says "mounted"/"not mounted" regardless of drive role.
   The brainstorm resolved: **connected/disconnected/away** (role-aware).

4. **Mythic inconsistency.** `notify.rs` uses "loom" and "weave" — the norns spin at
   a well, they don't weave on a loom. The brainstorm resolved: remove loom/weave/woven,
   keep thread/fray/well/spindle, add mend.

Every subsequent phase produces user-facing text through `voice.rs`. Building features on
unstable vocabulary means either shipping text that will change, or deferring vocabulary
until all features exist (causing a massive rewrite). Landing vocabulary first is cheap
(presentation-only) and unblocks everything.

---

## Proposed Design

### Change 1: Safety labels → Exposure triad

**File:** `src/voice.rs` — `safety_label()` (line 244)

```rust
// Before
fn safety_label(status: &str) -> String {
    match status {
        "PROTECTED"   => "OK".to_string(),
        "AT RISK"     => "aging".to_string(),
        "UNPROTECTED" => "gap".to_string(),
        other => other.to_string(),
    }
}

// After
fn exposure_label(status: &str) -> String {
    match status {
        "PROTECTED"   => "sealed".to_string(),
        "AT RISK"     => "waning".to_string(),
        "UNPROTECTED" => "exposed".to_string(),
        other => other.to_string(),
    }
}
```

Rename function `safety_label` → `exposure_label` throughout voice.rs.

**Column header:** `"SAFETY"` → `"EXPOSURE"` in `render_subvolume_table()` (line ~165)
and `render_assessment_table()` (line ~680).

**Summary line:** `render_summary_line()` (lines 70-135):
- `"All data safe."` → `"All sealed."`
- `"{N} safe"` → `"{N} sealed"`
- `"need(s) attention"` → `"exposed"`
- Pattern: `"{N} of {M} sealed. {names} exposed."`

### Change 2: Chain → Thread

**Column header:** `"CHAIN"` → `"THREAD"` in `render_subvolume_table()`.

**Thread column rendering** (interactive mode only):
- `"incremental (pin_name)"` → `"unbroken"`
- `"full (reason)"` → `"broken — full send (~size)"` (include estimated bytes if available)
- `"none"` → `"—"` (no external sends configured)

Implementation: voice.rs intercepts `ChainHealth` rendering for interactive mode instead
of calling `.to_string()` (which feeds daemon JSON and must not change).

```rust
fn render_thread_status(health: &ChainHealth) -> String {
    match health {
        ChainHealth::NoDriveData => "—".to_string(),
        ChainHealth::Incremental(_) => "unbroken".to_string(),
        ChainHealth::Full(reason) => format!("broken — full send ({})", reason),
    }
}
```

**Error messages:** `src/error.rs` (line 237):
- `"Incremental parent missing (chain broken)"` → `"Incremental parent missing (thread broken)"`
- `"Check \`urd verify\` for chain health"` → `"Check \`urd verify\` for thread health"`
- `UrdError::Chain` error message: `"Chain error: {0}"` → `"Thread error: {0}"`
  (variant name stays `Chain` — internal, not user-facing)

### Change 3: Drive status → Role-aware vocabulary

**File:** `src/voice.rs` — `render_drive_summary()` (lines 297-327)

- `"mounted"` → `"connected"`
- `"not mounted"` → role-aware:
  - `DriveRole::Offsite` → `"away"` + `"last seen N days ago"` if age available
  - All other roles → `"disconnected"`

**Subvolume table drive cells** (line ~222): Currently shows `"away"` for unmounted drives
with send history. Make this role-aware: offsite drives show `"away"`, primary/test drives
show `"—"` (disconnected state is implicit from the column being empty).

**Grouped skip labels:**
- `"Not mounted:"` → `"Disconnected:"` in `render_drive_not_mounted_group()` (line 932)
- `"Drives not mounted:"` → `"Drives disconnected:"` in `render_skipped_block()` (line 632)

**Sentinel status** (line ~1538): `"Mounted"` → `"Connected"`

**Init output** (lines ~1370-1392): `"MOUNTED"` → `"CONNECTED"`, `"NOT MOUNTED"` →
`"DISCONNECTED"` or `"AWAY"` (role-aware)

### Change 4: Skip tag differentiation

**File:** `src/voice.rs` — `render_individual_skips()` (lines 964-977)

Current: `[SPACE]` for SpaceExceeded, `[SKIP]` for Other.

New: per-category tags in grouped renderers too:

| SkipCategory | Tag | Color |
|---|---|---|
| `IntervalNotElapsed` | `[WAIT]` | dimmed |
| `DriveNotMounted` | `[AWAY]` | dimmed |
| `SpaceExceeded` | `[SPACE]` | yellow |
| `Disabled` | `[OFF]` | dimmed |
| `Other` | `[SKIP]` | dimmed |

The grouped renderers (`render_drive_not_mounted_group`, `render_interval_group`,
`render_disabled_group`) already display their categories in prose. Adding the tag prefix
makes the vocabulary consistent with the individual skip renderer:

```
[AWAY]  Disconnected: WD-18TB1 (3 subvolumes), 2TB-backup (1 subvolume)
[WAIT]  Interval not elapsed: 5 subvolumes (next in ~2h30m)
[OFF]   Disabled: subvol6-tmp
```

### Change 5: CLI command descriptions

**File:** `src/cli.rs` (lines 22-39)

| Command | Current | New |
|---------|---------|-----|
| Plan | "Show planned backup operations without executing" | "Preview what Urd will do next" |
| Backup | "Create snapshots, send to external drives, run retention" | "Back up now — snapshot, send, clean up" |
| Status | "Show snapshot counts, drive status, chain health" | "Check whether your data is safe" |
| History | "Show backup history" | "Review past backup runs" |
| Verify | "Verify incremental chain integrity and pin file health" | "Diagnose thread integrity and pin health" |
| Init | "Initialize state database and validate system readiness" | "Set up Urd and verify the environment" |
| Calibrate | "Measure snapshot sizes for space estimation (run before first external send)" | "Measure snapshot sizes for send estimates" |
| Get | "Retrieve a file from a past snapshot" | "Restore a file from a past snapshot" |
| Sentinel | "Sentinel daemon — monitors backup health and drive connections" | "Sentinel — continuous health monitoring" |

### Change 6: Notification mythology cleanup

**File:** `src/notify.rs`

| Line | Current | New |
|------|---------|-----|
| ~202 | "...The well remembers, but the weave grows thin." | "...The well remembers, but the thread grows thin." |
| ~220 | "The thread of {} is rewoven" | "The thread of {} is mended" |
| ~274 | "The loom has seized — every weaving failed." | "The spindle has stopped — every thread snapped." |
| ~277 | "{N} of {total} threads could not be woven." | "{N} of {total} threads could not be spun." |

### Change 7: Column header rename

**File:** `src/voice.rs`

- `"PROMISE"` → `"PROTECTION"` in `render_subvolume_table()` and `render_assessment_table()`

This is a presentation-only change. The `ProtectionLevel` enum and config field name stay
unchanged until Phase 6.

---

## What does NOT change

- `PromiseStatus::Display` in awareness.rs — stays `"PROTECTED"`/`"AT RISK"`/`"UNPROTECTED"` (ADR-105: heartbeat contract)
- `ChainHealth` serde serialization in output.rs — daemon JSON consumers may parse these
- `SkipCategory::from_reason()` string matching in output.rs — matches planner reason strings
- `ProtectionLevel` enum names and Display — deferred to Phase 6
- `OperationalHealth` vocabulary — "healthy/degraded/blocked" stays (earned its place)
- Operation tags — `[CREATE]`/`[SEND]`/`[DELETE]` stay
- No new types, fields, enums, or modules

---

## Module Mapping

| File | Changes | Lines affected |
|------|---------|---------------|
| `src/voice.rs` | safety_label→exposure_label, column headers, summary line, thread rendering, drive vocabulary, skip tags | ~40 string literals, ~10 function calls |
| `src/cli.rs` | 9 command doc comments | 9 lines |
| `src/notify.rs` | 4 notification body strings | 4 lines |
| `src/error.rs` | 3 error message strings | 3 lines |

---

## Test Strategy

**~35 existing tests modified, ~10 new tests added.**

The bulk of the work is updating string assertions in voice.rs tests (67 tests) and
notify.rs tests (22 tests). Every test checking for `"OK"`, `"aging"`, `"gap"`, `"safe"`,
`"mounted"`, `"CHAIN"`, `"SAFETY"`, `"PROMISE"` needs updating.

New tests:
- `exposure_label()` mapping: sealed/waning/exposed for all three inputs
- Role-aware drive summary: primary → disconnected, offsite → away (new behavior)
- `render_thread_status()`: unbroken, broken, dash
- Skip tag variants: [WAIT], [AWAY], [SPACE], [OFF], [SKIP]

---

## Invariants

1. **Daemon JSON unchanged.** `OutputMode::Daemon` paths serialize structured types via
   serde. `ChainHealth::Display` feeds daemon JSON — must not change.
2. **Heartbeat strings unchanged.** `promise_status` in heartbeat.json stays
   `PROTECTED`/`AT RISK`/`UNPROTECTED`.
3. **Prometheus metrics unchanged.** Metric names and label values are on-disk contracts.
4. **No data structure changes.** Zero new fields, renamed fields, or new enum variants.
5. **DriveRole already available.** `DriveInfo` has `role: DriveRole` — no new plumbing
   needed for role-aware vocabulary.

---

## Integration Points

- **Downstream:** Prometheus metrics unaffected. Sentinel state file unaffected (uses
  structured types, not voice strings). Heartbeat unaffected.
- **Upstream:** All subsequent phases (2-6) produce text through voice.rs in the vocabulary
  landed here. This is the foundation.

---

## Effort Estimate

**1 session (3-4 hours).** Methodical find-and-replace with test updates. The risk is not
complexity but thoroughness — missing one string creates vocabulary inconsistency. The test
suite (67 voice.rs tests, 22 notify.rs tests) catches omissions.

Comparable: `SkipCategory` system (output.rs + voice.rs + 17 tests) was one session. This
phase touches more strings but has simpler logic (no new computation).

---

## Ready for Review

Focus areas for arch-adversary:

1. **Daemon JSON stability.** Verify no serde-derived serialization changes. The
   `ChainHealth::Display` impl is seductive to change but feeds daemon consumers.

2. **Role-aware "away" reconciliation.** Current `"away"` in the subvolume table (line 222)
   triggers on unmounted drives with send history, regardless of role. The new vocabulary
   says `"away"` is offsite-only. The table cell must also become role-aware — offsite
   drives show `"away"`, primary drives show `"—"` when disconnected.

3. **Skip tag in grouped renderers.** The grouped renderers already display their categories
   in prose. Adding `[AWAY]`/`[WAIT]`/`[OFF]` prefixes to the grouped output is a
   presentation choice — verify it doesn't make output too busy.

4. **Thread column data loss.** `"incremental (20260330-0404-htpc-home)"` carries the parent
   snapshot name. `"unbroken"` discards it. The parent name is useful for debugging. Decision:
   `"unbroken"` in default, `"unbroken (20260330-0404-htpc-home)"` in `--verbose`.
