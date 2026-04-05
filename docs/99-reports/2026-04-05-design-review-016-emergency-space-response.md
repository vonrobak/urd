---
upi: "016"
date: 2026-04-05
---

# Design Review: UPI 016 — Emergency Space Response (Implementation Plan)

**Scope:** `docs/97-plans/2026-04-05-plan-016-emergency-space-response.md`
**Design:** `docs/95-ideas/2026-04-03-design-016-emergency-space-response.md`
**Mode:** Design review (plan, pre-implementation)
**Reviewer:** arch-adversary
**Commit:** e744da4 (master)

---

## Executive Summary

The plan is well-structured and correctly sequences a feature that addresses a real
catastrophic failure the project has already experienced. The core retention function is
sound — simple, pure, and safe by construction. Two findings need attention before build:
the emergency command duplicates the executor's pin re-check logic (creating a second
code path that must stay in sync with the defense-in-depth contract), and the pre-flight
emergency path in backup.rs runs *before* the advisory lock, which means two concurrent
backup invocations could both run emergency retention simultaneously.

---

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be
deleted, particularly pinned snapshots that are the last reference for incremental send
chains.

**Proximity:** The plan is **two bugs away** from this. The `emergency_retention()` function
structurally prevents it (pin set is a mandatory argument). The two remaining paths to the
catastrophe are: (1) a caller passes an incorrect or incomplete pinned set, or (2) the
defense-in-depth re-check (layer 3) has a bug in its duplicated implementation. Finding F1
addresses the second path.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Core retention logic is sound; lock ordering issue (F2) and unsent protection gap (F3) need resolution |
| 2 | **Security** | 5 | No new privilege surface; all btrfs calls through `BtrfsOps`; no path construction from user input |
| 3 | **Architectural Excellence** | 4 | Clean separation of pure logic and I/O; pin re-check duplication (F1) is the main blemish |
| 4 | **Systems Design** | 4 | Handles crash, concurrent runs, and non-TTY correctly (with F2 fix); notification threading is clean |

---

## Design Tensions

### T1: Direct `BtrfsOps` calls vs. going through the executor

The plan chooses to call `BtrfsOps::delete_subvolume()` directly from the emergency command,
bypassing the executor. The rationale is sound: emergency is not a backup run, so the
executor's operation grouping, space-recovery tracking, and progress reporting are irrelevant.

**Trade-off evaluation:** This is the right call. The executor's `execute_delete()` also
contains the space-recovery early-exit logic (`space_recovered` HashMap) which would
*actively interfere* with emergency — we want to delete the full list, not stop after
freeing enough space. The cost is duplicating the pin re-check (addressed in F1).

### T2: Notification via backup flag vs. Sentinel action

The plan revises the design doc's approach (new `SentinelAction` variant) to instead use
a flag in the backup command that emits a `NotificationEvent` through the standard path.
This avoids adding state machine complexity to Sentinel for something that only happens
during backup runs.

**Trade-off evaluation:** Good call. The Sentinel state machine should remain minimal.
The backup command already dispatches notifications — adding one more event type is
natural. The design doc's `SentinelAction::NotifyEmergencyRetention` was solving a
problem that doesn't exist: the backup command already has the notification machinery.

### T3: Crisis threshold — `< min_free_bytes` (interactive) vs. `< 50%` (automatic)

The plan uses `free_bytes < min_free_bytes` for the interactive `urd emergency` command
(Step 3) and `free_bytes < min_free_bytes * 0.5` for the automatic pre-flight (Step 4).
Different thresholds for different contexts — the interactive path is more aggressive in
*offering* help, the automatic path is more conservative in *taking action*.

**Trade-off evaluation:** Correct asymmetry. The interactive command should detect and
offer recovery even at mild pressure (the user chose to run it), while automatic deletion
of months of history should only fire at genuine crisis levels. The 50% threshold is
conservative enough to avoid surprise data loss from routine space fluctuation.

---

## Findings

### F1: Pin re-check duplication creates a second defense-in-depth code path (Significant)

**What:** Step 3 says the emergency command will "reproduce executor defense-in-depth" by
calling `chain::find_pinned_snapshots()` before each `delete_subvolume()`. This creates a
second implementation of the pin re-check that must stay in sync with `executor.rs:714-740`.

**Consequence:** If the executor's pin re-check evolves (e.g., adds a new pin file format,
checks a different directory, or gains additional safety logic), the emergency command's
copy must be updated in lockstep. ADR-106 defense-in-depth is a three-layer system; having
two separate implementations of layer 3 means the layers can diverge silently.

**Suggested fix:** Extract the pin re-check into a shared helper function. Something like:

```
/// Defense-in-depth: re-check pin status immediately before deletion.
/// Returns true if the snapshot is pinned and must NOT be deleted.
pub fn is_pinned_at_delete_time(
    snapshot_path: &Path,
    subvolume_name: &str,
    config: &Config,
) -> bool
```

Place it in `chain.rs` (it already owns pin file logic) or as a free function in
`executor.rs` that the emergency command can also call. The executor's existing re-check
at line 714 becomes a call to this function. The emergency command calls the same function.
One implementation, one test, one place to update.

### F2: Emergency pre-flight runs before the advisory lock (Significant)

**What:** Step 4 places `run_emergency_preflight()` before `plan::plan()` at line 64 of
`commands/backup.rs`. But the advisory lock is acquired at line 91-93, *after* the plan
is computed. This means emergency retention — which performs actual btrfs deletions —
runs without the lock.

**Consequence:** Two concurrent `urd backup --auto` invocations (e.g., systemd timer fires
while a manual backup is running) could both run emergency retention simultaneously. Both
would enumerate the same snapshots, both would call `delete_subvolume()` on the same paths.
The second deletion would fail (snapshot already gone), which is *safe* (btrfs reports an
error, the command handles it), but it's noisy and wasteful. More subtly: if both read
pins and then both delete, there's a narrow window where the first deletes a snapshot that
the second was about to skip as pinned — but since both read pins *before* deleting, this
is safe in practice (both see the same pins).

**Suggested fix:** Move the emergency pre-flight to *after* lock acquisition (after line 93).
The lock already exists to prevent concurrent backup operations — emergency retention is
a destructive operation that belongs inside the lock scope. The pre-flight should go between
the lock acquisition (line 93) and the empty-plan check (line 95):

```
let _lock = lock::acquire_lock(&lock_path, trigger)?;
run_emergency_preflight(&config, &fs_state)?;  // After lock, before plan
let mut backup_plan = plan::plan(&config, now, &filters, &fs_state)?;
```

Wait — `plan::plan()` is currently at line 64, before the lock too. Reading more carefully:
the lock is acquired at line 91, after `plan()` and after `filter_promise_retention()`.
But the plan itself is a pure function (no I/O mutations), so running it without the lock
is fine. Emergency retention performs *deletions* — that's qualitatively different. The fix
is to restructure: acquire lock first, then emergency preflight, then plan.

Actually, looking at the code more carefully: the dry-run path exits before the lock (line
77-88). So the current structure is: plan → dry-run check → lock → execute. For emergency
preflight, the right place is: lock → emergency preflight → (re-plan if needed) → execute.
This means restructuring the early part of `backup.rs::run()`. Plan the restructure explicitly.

### F3: Emergency retention doesn't account for unsent snapshot protection (Significant)

**What:** The `emergency_retention()` function takes `pinned` (chain parents) and `latest`
(newest snapshot). It keeps those and deletes everything else. But `plan_local_retention()`
at `plan.rs:426-448` expands the pinned set to include *all unsent snapshots* — snapshots
newer than the oldest pin that haven't been sent to all drives yet. Emergency retention
skips this expansion.

**Consequence:** Consider: subvolume has 10 snapshots, pins at positions 3 and 7 (sent to
drive A and B respectively). Snapshots 8, 9, 10 haven't been sent to any drive yet.
Emergency retention keeps: latest (10) + pinned (3, 7). Deletes: 1, 2, 4, 5, 6, 8, 9.
Snapshots 8 and 9 are *unsent* — deleting them means they can never be sent to external
drives. The next backup will create snapshot 11 and send it, but the history between 7
and 10 is lost from external drives.

**Severity assessment:** This is *by design* — the design doc says "keep the latest and the
chain parents" and explicitly rejects keeping multiple recents. Emergency is an aggressive
thinning that sacrifices history for space recovery. The unsent snapshots between the oldest
pin and latest are exactly the kind of history that emergency mode trades away.

However, the plan should **explicitly acknowledge this trade-off** and ensure the voice
output communicates it: "Unsent snapshots will be deleted. The next backup will create a
fresh snapshot and send it." Users need to understand that emergency retention breaks the
incremental send chain for any drive that hasn't received recent sends. The `urd emergency`
preview should show which drives will need full sends after recovery.

**Suggested fix:** Not a code change — add to the voice rendering (Step 3b) a line in the
crisis preview: "Note: {N} unsent snapshots will be deleted. Next send to {drives} will
be a full send." This is information, not a gate. The emergency is still the right action.

### F4: Roots without `min_free_bytes` are invisible to emergency (Moderate)

**What:** Step 3 checks `free_bytes < min_free_bytes` and Step 4 checks
`free_bytes < min_free_bytes * 0.5`. But `SnapshotRoot.min_free_bytes` is `Option<ByteSize>`
— it can be `None` when the user hasn't configured a threshold.

**Consequence:** A snapshot root with no `min_free_bytes` configured will never trigger
emergency detection, even if it has 0 bytes free. The user runs `urd emergency`, sees
"No crisis detected. All snapshot roots are within their free-space thresholds" while their
disk is full. The "no crisis" output (Step 3, point 8) shows each root with "OK", but a
root with no threshold will show "OK" even at 0 bytes free.

**Suggested fix:** For the `urd emergency` command (interactive): still assess roots
without `min_free_bytes` but show them differently — "no threshold configured" rather than
"OK". If the filesystem is critically low (say, <1 GB free) regardless of config, flag it
as an advisory. For the automatic pre-flight (Step 4): skip roots without
`min_free_bytes` (consistent with existing space_pressure logic in `plan.rs:465` which uses
`unwrap_or(0)` — no threshold means no pressure).

### F5: `btrfs subvolume sync` after deletions (Moderate)

**What:** The executor's `execute_delete()` at line 748 calls `self.btrfs.sync_subvolumes()`
after each deletion so freed space is visible to subsequent space checks. The plan's
emergency command flow (Step 3) doesn't mention sync between deletions.

**Consequence:** Without sync, `drives::filesystem_free_bytes()` may not reflect freed space
after deletions. The post-deletion space check ("Freed 8.2 GB") would underreport. More
importantly, the backup pre-flight (Step 4) checks space after emergency retention — if it
doesn't sync, `plan()` may still see the old free space and enter space_pressure mode
unnecessarily.

**Suggested fix:** After each `delete_subvolume()` call (or after the batch), call
`btrfs.sync_subvolumes(snapshot_root)`. The executor already does this per-delete; the
emergency path should match. A batch sync after all deletions (rather than per-delete) is
acceptable for the emergency command since we always delete the full list — no early
termination based on freed space.

### F6: `EmergencyRootAssessment` aggregates across subvolumes but retention is per-subvolume (Minor)

**What:** The output type `EmergencyRootAssessment` has `snapshot_count`, `delete_count`,
`keep_count` at the root level, but `emergency_retention()` is called per-subvolume. The
`latest` snapshot is per-subvolume (each subvolume keeps its own latest), and the pinned
set is per-subvolume (pin files are in `{root}/{subvol_name}/`).

**Consequence:** The aggregation is fine for the summary view, but the user should also see
the per-subvolume breakdown. "39 snapshots across 2 subvolumes" is helpful; "deleting 39
snapshots" without showing the per-subvolume split might obscure that one subvolume is
keeping 8 while another is keeping 1.

**Suggested fix:** Add per-subvolume detail to the output type:

```rust
pub struct EmergencySubvolDetail {
    pub name: String,
    pub snapshot_count: usize,
    pub keep_count: usize,
    pub delete_count: usize,
    pub latest: String,
    pub pinned_count: usize,
}
```

Include `Vec<EmergencySubvolDetail>` in `EmergencyRootAssessment`. The voice rendering
can decide how much detail to show in interactive vs. daemon mode.

### C1: `emergency_retention()` signature enforces safety structurally (Commendation)

The function requires `latest: &SnapshotName` and `pinned: &HashSet<SnapshotName>` as
mandatory arguments. Callers *cannot* accidentally pass nothing — they must explicitly
construct the latest and the pinned set. Combined with the purity contract (no I/O, no
config), this makes the function safe by construction. The only way to delete a pinned
snapshot is to lie about what's pinned, and you have to lie explicitly. This is the right
design for a deletion function in a backup tool.

### C2: Revised notification approach (Commendation)

Choosing to route emergency notifications through the backup command's existing
`notify::dispatch()` path rather than adding a `SentinelAction` variant is architecturally
clean. It keeps the Sentinel state machine minimal (it doesn't need to know about emergency
internals) and leverages existing infrastructure. The design doc's `SentinelAction::
NotifyEmergencyRetention` would have added weight to the pure state machine for something
that only runs inside the backup command.

---

## The Simplicity Question

**What's earning its keep:** The `emergency_retention()` pure function, the per-root
iteration pattern, the direct `BtrfsOps` call bypass of the executor. All three are simple
and correct.

**What could be simpler:** The `EmergencyOutput` / `EmergencyRootAssessment` /
`EmergencyResult` type trilogy is heavier than needed for v1. The emergency command could
build and render inline (like the early `urd get` did) and extract types later when the
shape stabilizes. The design doc's interaction format is specific enough to render directly.
That said, the structured output pattern is established convention in this codebase (every
command does it), so following it is reasonable even if it's slightly over-engineered for
the initial build.

**What could be deleted:** The doctor space trend warning (Step 6) adds mild value. If the
session runs long, it's the first thing to cut — it's purely advisory and `urd emergency`
already handles the "no crisis" display. Could be added in a follow-up.

---

## For the Dev Team

Priority-ordered action items for `/post-review`:

1. **Extract pin re-check into shared helper** (F1) �� Create a function in `chain.rs` or
   `executor.rs` that both the executor and emergency command call. One implementation of
   ADR-106 layer 3. Files: `src/chain.rs` or `src/executor.rs`, `src/commands/emergency.rs`.

2. **Move emergency pre-flight after lock acquisition** (F2) — Restructure `backup.rs` to
   acquire the lock before running emergency retention. The plan currently places it before
   `plan::plan()` at line 64, but the lock isn't acquired until line 91. Emergency retention
   performs destructive btrfs operations that must be serialized. File: `src/commands/backup.rs`.

3. **Acknowledge unsent snapshot trade-off in voice output** (F3) — Add a line to the
   emergency preview showing that unsent snapshots will be deleted and listing drives that
   will need full sends after recovery. Not a code gate — informational. File: `src/voice.rs`.

4. **Handle roots without `min_free_bytes`** (F4) — In `urd emergency`, show unconfigured
   roots as "no threshold configured" rather than "OK". In automatic pre-flight, skip them
   (matches existing `plan.rs` behavior). File: `src/commands/emergency.rs`.

5. **Add `btrfs subvolume sync` after emergency deletions** (F5) — Call
   `btrfs.sync_subvolumes()` after deletions so freed space is visible. Batch sync after
   all deletions is acceptable. Files: `src/commands/emergency.rs`, `src/commands/backup.rs`.

6. **Add per-subvolume detail to output types** (F6) — Enrich `EmergencyRootAssessment`
   with per-subvolume breakdown so the user sees what each subvolume keeps/loses.
   File: `src/output.rs`.

---

## Open Questions

1. **Should `urd emergency` respect `local_snapshots = false` subvolumes?** These subvolumes
   use transient retention (delete everything not pinned). They may have snapshots on the
   same root as other subvolumes. The plan iterates `config.local_snapshots.roots` and their
   subvolumes — will it encounter `local_snapshots = false` subvolumes? If so, transient
   retention already handles them aggressively. Clarify whether the emergency path should
   skip them or include them.

2. **What happens if `read_snapshot_dir` fails for one subvolume?** The plan doesn't specify
   error handling per-subvolume during emergency enumeration. Should one unreadable subvolume
   directory skip that subvolume (consistent with ADR-109 isolate-and-report) or abort the
   entire emergency? Recommend: skip per-subvolume, report the error, continue with others.
