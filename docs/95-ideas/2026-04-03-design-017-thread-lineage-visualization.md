---
upi: "017"
status: proposed
date: 2026-04-03
---

# Design: Thread Lineage Visualization in `urd doctor --thorough`

> **TL;DR:** Add a "Thread details" section to `urd doctor --thorough` that shows, for each
> subvolume, how many local snapshots exist, which snapshot is pinned for each drive, whether
> each drive's chain is intact or broken, and how stale an absent drive's last known snapshot
> is. No new btrfs calls, no new command — all data already collected by existing modules.

## Problem

When a thread breaks, `urd status` reports "broken — full send (pin missing locally)" but
gives no chain structure. The user cannot answer three natural follow-up questions:

1. **Where exactly is the break?** Is the pin file missing? Does the pinned snapshot exist on
   the drive but not locally? Is the pin pointing at a snapshot that was pruned by retention?
2. **How far behind is an absent drive?** If WD-18TB1 has been away for 11 days, what was the
   last snapshot it received and how many local snapshots have accumulated since?
3. **What is the shape of each drive's chain?** How many snapshots exist locally vs. externally,
   and are the counts diverging in a way that suggests a problem?

`urd verify` checks chain health mechanically (pin file readable, pinned snapshot exists
locally and on drive, no orphans, pin not stale) but renders flat pass/fail checks with no
visual summary of the chain structure. The "Threads" section in `urd doctor --thorough`
currently only shows verify failures — it has no visualization when chains are healthy.

## Proposed Design

### User-facing output

Extend the "Threads" section in `urd doctor --thorough` with per-subvolume chain detail:

```
  Thread details
  ──────────────────────────────────────────────────────────
  htpc-home:
    Local: 18 snapshots, newest 21h ago
      pin → 20260402-2220-htpc-home  (for WD-18TB)
      pin → 20260323-0400-htpc-home  (for WD-18TB1)
    WD-18TB:   6 snapshots, newest 21h ago — thread intact
    WD-18TB1:  absent 11d, last: 20260323-0400-htpc-home — thread stale

  htpc-root  (local_snapshots = false):
    Local: 0 snapshots
    WD-18TB1:  5 snapshots, newest 21h ago — thread broken (pin missing locally)
```

Key design choices in this format:

- **Local section always appears first.** The local snapshot set is the spine of every chain;
  drives are downstream. Showing it first matches the causal order.
- **Each drive's pin is shown under the local section**, not under the drive section, because
  pins live in the local snapshot directory and their existence is a local-side fact.
- **Drive sections summarize count + age + verdict.** One line per drive, enough to answer
  "is this drive current?" without drowning in snapshot lists.
- **`local_snapshots = false` is annotated.** Clarifies why local count is 0 and why no pins
  appear under the local section (no local chain to anchor them).
- **Thread verdict uses a small controlled vocabulary:** "intact", "stale", "broken". These
  map to existing chain health concepts; stale means the pin is old but the chain is
  structurally sound, broken means a send would be forced full.

### Output types (`output.rs`)

New types alongside the existing `DoctorOutput`:

```rust
/// Per-drive chain summary for a subvolume thread.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadDriveSummary {
    pub label: String,
    /// Drive is currently mounted.
    pub mounted: bool,
    /// Number of snapshots on this drive for this subvolume.
    pub snapshot_count: usize,
    /// Age of the newest snapshot on this drive, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_age_secs: Option<i64>,
    /// Snapshot name recorded in the pin file for this drive, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// Whether the pin came from a legacy (non-drive-specific) pin file.
    pub pin_is_legacy: bool,
    /// Structural verdict for this drive's chain.
    pub thread_status: ThreadStatus,
}

/// Structural verdict for a drive's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Chain is intact: pin file exists, pinned snapshot present locally and on drive.
    Intact,
    /// Chain is structurally sound but the pin is older than `send_interval * threshold`.
    Stale,
    /// Chain is broken: next send will be forced full.
    Broken,
    /// Drive not mounted; chain status cannot be determined.
    Absent,
    /// No snapshots on drive yet; first send will be full (not a break).
    NeverSent,
}

/// Per-subvolume thread lineage summary for doctor --thorough.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadDetail {
    pub subvolume: String,
    /// True when local_snapshots is disabled.
    pub local_snapshots_disabled: bool,
    pub local_snapshot_count: usize,
    /// Age of newest local snapshot, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_newest_age_secs: Option<i64>,
    /// Per-drive chain summaries, in config order.
    pub drives: Vec<ThreadDriveSummary>,
}
```

Add `thread_details` to `DoctorOutput`:

```rust
pub struct DoctorOutput {
    // ... existing fields ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_details: Option<Vec<ThreadDetail>>,
}
```

### Pure collection function (`awareness.rs` or `chain.rs`)

A pure function `collect_thread_details` takes the config and `FileSystemState` trait and
returns `Vec<ThreadDetail>`. No new I/O patterns — it calls the same functions already used
by `verify.rs` and `awareness.rs`:

- `plan::read_snapshot_dir()` for local snapshot lists
- `chain::read_pin_file()` for per-drive pins
- `fs_state.external_snapshots()` for drive snapshot lists (via `FileSystemState` trait)
- `drives::is_drive_mounted()` for mount status

The function is pure: all filesystem access goes through `FileSystemState`. This makes it
testable with `MockFileSystemState` and keeps it architecturally consistent with
`compute_subvol_assessments()` in `awareness.rs`.

`ThreadStatus` computation rules:

| Condition | Status |
|-----------|--------|
| Drive not mounted | `Absent` |
| No snapshots on drive, no pin | `NeverSent` |
| No pin file but snapshots exist | `Broken` |
| Pin file exists, pinned snapshot missing locally | `Broken` |
| Pin file exists, pinned snapshot missing on drive | `Broken` |
| Pin file exists, both present, pin age > `send_interval * 3` | `Stale` |
| Pin file exists, both present, pin age within threshold | `Intact` |

The stale threshold reuses the `stale_threshold_secs()` logic already in `verify.rs`. If
that function is private, it can be made `pub(crate)` or duplicated with a TODO to
consolidate.

### Placement in doctor flow (`commands/doctor.rs`)

After the existing verify step (step 5), add a thread detail collection step:

```rust
// ── 6. Thread details (--thorough only) ───────────────────
let thread_details = if args.thorough {
    let fs_state = RealFileSystemState { state: None };
    Some(collect_thread_details(&config, &fs_state))
} else {
    None
};
```

Then include `thread_details` in the `DoctorOutput` construction.

### Voice rendering (`voice.rs`)

A new `render_thread_details()` helper called from `render_doctor_interactive()` after the
verify section. The rendering loops over `Vec<ThreadDetail>` and applies the format shown
above. The verdict string for each drive derives from `ThreadStatus`:

- `Intact` → "thread intact" (no color or green)
- `Stale` → "thread stale" (yellow)
- `Broken` → "thread broken" (red)
- `Absent` → "absent Nd, last: {pin}" (dimmed, with pin name if available)
- `NeverSent` → "no snapshots yet" (dimmed)

Age formatting reuses whatever `format_age()` helper already exists in `voice.rs`.

In JSON/daemon mode, `thread_details` is included verbatim as a structured field in the
`DoctorOutput` JSON object. Spindle can consume it without further processing.

## Architecture

### Module touch map

| Module | Change |
|--------|--------|
| `output.rs` | Add `ThreadDetail`, `ThreadDriveSummary`, `ThreadStatus`; add `thread_details` field to `DoctorOutput` |
| `awareness.rs` | Add `pub fn collect_thread_details(config, fs_state) -> Vec<ThreadDetail>` |
| `commands/doctor.rs` | Call `collect_thread_details` when `--thorough`; populate `DoctorOutput` |
| `voice.rs` | Add `render_thread_details()` helper; call from `render_doctor_interactive()` |

No changes to: `chain.rs`, `verify.rs`, `plan.rs`, `drives.rs`. All called as-is.

### No new btrfs calls

`FileSystemState::external_snapshots()` and `local_snapshots()` are the only filesystem
reads needed. Both are already in use by `verify.rs` via `RealFileSystemState`. The chain
check reads pin files from disk via `chain::read_pin_file()` — standard file I/O, no btrfs.

### Invariants respected

- **ADR-108 (pure function pattern):** `collect_thread_details` is pure; no I/O escapes.
- **ADR-100 (planner never modifies):** This is a reader, not a planner. No writes.
- **ADR-109 (validate at load, isolate at runtime):** Missing pin files and unreadable
  snapshot dirs produce `NeverSent` or `Broken` rather than errors that abort collection.
- Thread details are advisory information, not a gate on backup execution.

## Data Sources (all existing)

| Data | Source |
|------|--------|
| Local snapshot list | `plan::read_snapshot_dir(snapshot_root/name/)` |
| Per-drive pin name + source | `chain::read_pin_file(local_dir, drive_label)` |
| External snapshot list | `FileSystemState::external_snapshots(drive, name)` |
| Drive mount status | `drives::is_drive_mounted(drive)` |
| Pin file mtime (for stale check) | `std::fs::metadata(pin_path).modified()` |
| `local_snapshots` flag | `subvol.local_snapshots` from resolved config |

## Testing

### Unit tests in `awareness.rs`

| Test | What it covers |
|------|----------------|
| `thread_detail_intact` | Drive mounted, pin present, snapshots match — `Intact` |
| `thread_detail_broken_pin_missing_locally` | Pin file exists, pinned snapshot gone from local dir — `Broken` |
| `thread_detail_broken_pin_missing_on_drive` | Pin file exists, pinned snapshot gone from drive — `Broken` |
| `thread_detail_absent_drive` | Drive not mounted — `Absent`, pin name preserved |
| `thread_detail_never_sent` | No pin, no external snapshots — `NeverSent` |
| `thread_detail_stale` | Pin present, both snapshots exist, pin mtime old — `Stale` |
| `thread_detail_local_snapshots_disabled` | `local_snapshots = false`, external-only subvolume |
| `thread_detail_legacy_pin` | Legacy pin file fallback; `pin_is_legacy = true` |
| `thread_detail_multiple_drives` | Two drives with different statuses in same subvolume |

### Unit tests in `voice.rs`

| Test | What it covers |
|------|----------------|
| `render_thread_details_all_intact` | All drives intact; minimal output |
| `render_thread_details_mixed` | One intact, one absent, one broken — correct colors |
| `render_thread_details_local_disabled` | `local_snapshots_disabled = true` annotation present |
| `render_thread_details_never_sent` | `NeverSent` renders as "no snapshots yet", not "broken" |

All tests use `MockFileSystemState` — no real filesystem or btrfs calls.

## Effort Estimate

~0.5 session:

1. Add output types to `output.rs` — 20 min
2. Implement `collect_thread_details()` in `awareness.rs` — 40 min
3. Wire into `commands/doctor.rs` — 10 min
4. Voice rendering in `voice.rs` — 30 min
5. Unit tests (9 + 4) — 40 min

No migration needed. No on-disk format changes. No new btrfs calls. The feature is entirely
additive and gated behind `--thorough`.

## Open Questions

1. **Snapshot list display threshold.** When a drive has many snapshots, should the output
   show all names or just count + newest? The current design shows only count + age at the
   drive level. A future flag (`--verbose`) could list individual names if needed.

2. **Stale threshold source.** `stale_threshold_secs()` is currently private in `verify.rs`.
   It should be promoted to `pub(crate)` and shared with `collect_thread_details`, or the
   logic should be moved to `types.rs` on `Interval` as `stale_send_threshold_secs()`.

3. **JSON field naming.** `thread_details` is proposed for the `DoctorOutput` JSON field.
   This is consistent with `verify` (which uses the same pattern). No strong reason to
   change, but naming feedback welcome before implementation.

4. **Relation to UPI-008 (doctor pin age correlation).** UPI-008 already addresses stale
   pin detection in doctor. This design adds the structural chain view. They can be built
   independently; thread_details subsumes the stale-pin concern in --thorough mode.
