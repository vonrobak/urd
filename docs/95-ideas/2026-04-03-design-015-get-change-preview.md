---
upi: "015"
status: proposed
date: 2026-04-03
---

# Design: Change Preview for `urd get`

> **TL;DR:** Before restoring a file, show the user whether it actually changed in the
> selected snapshot and by how much — replacing the current blind leap of faith with
> grounded, actionable context. Add an optional `--diff` flag for text files to show
> a unified diff against the live version.

## Problem

`urd get` currently presents three facts before restoring a file: filename, snapshot date,
and file size. The user has no way to know whether the file they are about to restore
actually changed in that snapshot, or how it changed.

This is precisely the moment of maximum anxiety. The user suspects something is wrong —
a file was overwritten, corrupted, accidentally deleted, or changed to an unknown state.
They are restoring blind. A snapshot two days old may have the right version or the wrong
one. They have no signal.

The information to answer this question already exists in BTRFS: `btrfs subvolume find-new`
lists all files modified since a given generation. The generation comes from the previous
snapshot via `btrfs subvolume show`. The user should see this before committing to a restore.

## Mechanism

BTRFS tracks changes via a monotonically increasing generation counter on each subvolume.
Two commands expose this:

```
btrfs subvolume show <path>        # includes "Generation:" field
btrfs subvolume find-new <path> <gen>  # lists files modified since that generation
```

Given two adjacent snapshots A (older) and B (selected), the files modified in B are
those that appear in `find-new B <generation_of_A>`. If the target file's relative path
appears in that list, it changed in this snapshot. If not, it was unchanged — and the
user may want an older snapshot.

## Proposed Design

### Step-by-step flow in `commands/get.rs`

After the current step 5 (snapshot selection, line 80), and before building `GetOutput`:

1. Find the previous snapshot in the sorted list (the one immediately before the selected
   snapshot). This is a pure in-memory operation on the already-sorted `Vec<SnapshotName>`.
2. If a previous snapshot exists, call `btrfs_ops.subvolume_generation(prev_snapshot_path)`
   to retrieve its generation number.
3. Call `btrfs_ops.find_new(selected_snapshot_path, prev_generation)` to get the list of
   modified paths.
4. Check whether `relative_path` (the file the user requested) appears in the output.
5. Populate `GetOutput.change_summary` with the result.

If there is no previous snapshot (selected is the oldest), report "earliest available
snapshot — no prior version to compare against."

If either btrfs call fails, degrade gracefully: set `change_summary` to `None` and proceed
with restore. Change preview is advisory; it must never block a restore.

### `ChangeSummary` type (in `output.rs`)

```rust
/// Why the file changed (or did not) relative to the previous snapshot.
#[derive(Debug, Serialize)]
pub enum ChangeSummary {
    /// File was modified in this snapshot. Size delta compared to live version.
    Modified { size_delta_bytes: i64 },
    /// File was not modified in this snapshot. It last changed in an earlier one.
    Unchanged { last_changed_snapshot: String },
    /// No previous snapshot exists for comparison.
    EarliestSnapshot,
    /// Generation comparison could not be performed (btrfs call failed).
    Unavailable,
}
```

`GetOutput` gains:

```rust
pub change_summary: Option<ChangeSummary>,
```

### Rendering in `voice.rs`

The change summary is rendered as part of the existing get metadata block, immediately
after the snapshot date line:

```
  Snapshot    20260401-0200-htpc-home  (2026-04-01 02:00)
  Changed     yes — 3.2 KB larger than previous snapshot
  File size   18.4 KB
```

Or when unchanged:

```
  Snapshot    20260401-0200-htpc-home  (2026-04-01 02:00)
  Changed     no — last modified in 20260329-0200-htpc-home
  File size   18.4 KB
```

Or when earliest:

```
  Snapshot    20260318-0200-htpc-home  (2026-03-18 02:00)
  Changed     earliest available snapshot
  File size   18.4 KB
```

The word "Changed" is plain and direct. The mythic voice here is in restraint — the user
is anxious, not browsing. No flourishes.

### `--diff` flag for text files

Add `--diff` to `GetArgs`. When set:

- Only activates for files under a size threshold (64 KB by default, configurable via
  a compile-time constant — no config surface needed).
- Runs `diff -u <snapshot_file> <live_file>` via `std::process::Command`.
- Prints the unified diff to stderr before the restore confirmation (or before content
  output for stdout mode).
- If the live file does not exist (the file was deleted), note that explicitly.
- If the file exceeds the threshold, print a single-line notice and skip the diff.
- Binary files: detect via the same heuristic `diff` uses (NUL bytes in first 8 KB).

`--diff` is additive and never blocks. If `diff` is not installed, warn and continue.

This is intentionally simple: no custom diff logic, no in-process parsing. Shell out to
the system `diff`. The output format is what operators and developers already know.

## Architecture

### New `BtrfsOps` methods

```rust
pub trait BtrfsOps {
    // ... existing methods ...

    /// Return the current generation of a subvolume.
    /// Used by `urd get` (change preview) and potentially UPI 014 (snapshot history).
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64>;

    /// Return paths of files modified since `since_generation` in the given snapshot.
    /// Wraps `btrfs subvolume find-new <path> <generation>`.
    fn find_new(&self, path: &Path, since_generation: u64) -> crate::error::Result<Vec<String>>;
}
```

Both methods are added to `RealBtrfs` (parsing `btrfs subvolume show` and
`btrfs subvolume find-new` output respectively) and `MockBtrfs` (configurable return
values for testing).

**Shared with UPI 014.** If `urd snapshot-history` (UPI 014) is implemented in the same
session, `subvolume_generation` is shared between the two features. Implement once, test
once. If UPI 014 is deferred, UPI 015 still adds both methods — `find_new` is not useful
without `subvolume_generation`.

### Parsing `btrfs subvolume show` output

The output contains a line of the form:

```
	Generation:		12345
```

Parse with a line scan: find the line starting with `Generation:`, split on whitespace,
take the last token, parse as `u64`. Fail explicitly if the field is absent or
unparseable — do not silently return 0 (a generation of 0 would cause `find-new` to
return all files ever written, which is wrong).

### Parsing `btrfs subvolume find-new` output

Each line is a path relative to the subvolume root, preceded by metadata:

```
inode 12345 file offset 0 len 4096 disk start 0 offset 0 gen 12346 flags INLINE                 ./path/to/file.txt
```

Extract the path component: split on whitespace, take the last token, strip leading `./`.
Collect into `Vec<String>`. The check against `relative_path` is a simple string
comparison after normalization.

### Module responsibilities

| Module | Change |
|--------|--------|
| `btrfs.rs` | Add `subvolume_generation` and `find_new` to trait and `RealBtrfs` impl |
| `output.rs` | Add `ChangeSummary` enum and `change_summary` field to `GetOutput` |
| `voice.rs` | Render `change_summary` in `render_get()` |
| `commands/get.rs` | Orchestrate: find prev snapshot, call new btrfs methods, populate output |
| `cli.rs` | Add `--diff` flag to `GetArgs` |

No new modules. No changes to `plan.rs`, `retention.rs`, `awareness.rs`, or config.

## Data Flow

```
commands/get.rs
  ├── select_snapshot()               (existing — pure, in-memory)
  ├── find_prev_snapshot()            (new — pure, in-memory)
  ├── btrfs.subvolume_generation()    (new I/O — prev snapshot path)
  ├── btrfs.find_new()                (new I/O — selected snapshot, prev generation)
  ├── check_file_in_find_new()        (new — pure string comparison)
  └── GetOutput { change_summary }    (extended struct)
        └── voice::render_get()       (extended renderer)
```

## Error Handling

- `subvolume_generation` failure → `change_summary: None`, log at debug, proceed.
- `find_new` failure → `change_summary: None`, log at debug, proceed.
- Path not in `find-new` output when it clearly should be → treat as Unchanged (conservative).
- `--diff` with missing `diff` binary → warn once, skip diff, proceed with restore.
- `--diff` with binary file → print "binary file — diff not shown", skip.
- `--diff` with missing live file → print "live file not found — may have been deleted".

All failures degrade to less information, never to blocked restores.

## Testing

### Unit tests

- `find_prev_snapshot()` with zero, one, and multiple snapshots
- `check_file_in_find_new()` with various `find-new` output formats, including leading
  `./`, missing files, and paths with spaces
- `parse_generation()` with valid output, missing `Generation:` line, non-numeric value
- `parse_find_new_paths()` with real-looking btrfs output, empty output, malformed lines

### Mock tests (MockBtrfs)

- `subvolume_generation` returns configured generation
- `find_new` returns configured path list
- `GetOutput.change_summary` is `Modified` when file appears in find-new output
- `GetOutput.change_summary` is `Unchanged` when file absent, with correct last-changed name
- `GetOutput.change_summary` is `EarliestSnapshot` when no previous snapshot
- `GetOutput.change_summary` is `None` when btrfs call fails

### voice.rs rendering tests

- Each `ChangeSummary` variant renders the expected text
- `None` change_summary renders no "Changed" line (backward compatible)

## What This Does Not Do

- Does not show which other files changed in the snapshot (that is UPI 014 scope).
- Does not produce interactive navigation between snapshots.
- Does not track renames (BTRFS find-new reports by inode path, not inode number).
- Does not support `--diff` for binary files or files over the threshold.
- Does not add a config surface — the 64 KB threshold is a compile-time constant.

## Effort

~0.5 session.

- `BtrfsOps` additions + `RealBtrfs` parsing: ~1 hour
- `MockBtrfs` additions + unit tests: ~30 minutes
- `output.rs` / `voice.rs` changes: ~30 minutes
- `commands/get.rs` orchestration: ~30 minutes
- `--diff` flag: ~30 minutes (most of it is edge cases)

If UPI 014 is built in the same session, `subvolume_generation` is implemented once and
shared — saves ~30 minutes total.

## Open Questions

1. **Size delta source.** `find-new` confirms a file changed but does not report size
   directly. The size delta ("3.2 KB larger") comes from comparing `metadata.len()` on the
   snapshot file vs. `metadata.len()` on the live file. If the live file is absent, the
   delta is the snapshot file size with a note that the live file is gone. Is the live-vs-
   snapshot delta what the user wants, or should it be snapshot-vs-previous-snapshot? The
   latter requires reading the previous snapshot's file, which may not exist (file could
   have been absent). **Proposed:** report snapshot-vs-live as primary, add prev-snapshot
   comparison only if the previous snapshot also contains the file.

2. **`find-new` output format stability.** The `btrfs subvolume find-new` output format
   is not formally documented and has changed across kernel/btrfs-progs versions. The
   parser should be tolerant of lines it cannot parse (skip them, do not fail) and log
   the raw line at debug level for diagnostics. A test corpus of real output from multiple
   btrfs-progs versions would be valuable before shipping.

3. **Performance.** For subvolumes with thousands of changed files, `find-new` output
   may be large. Since we only care whether one specific file appears, the check can
   short-circuit on first match. The `BtrfsOps::find_new` signature returns
   `Vec<String>` (all paths), which is fine for testing but may be worth revisiting as
   a streaming predicate `fn file_changed_since(path, file, gen) -> bool` if performance
   proves to be a problem in practice.
