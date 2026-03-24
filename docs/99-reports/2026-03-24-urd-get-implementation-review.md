# `urd get` — Architectural Adversary Implementation Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 3d implementation review — `urd get file --at date`
**Reviewer:** Architectural Adversary (Claude)
**Commit:** 3450d90 + uncommitted changes on master
**Files reviewed:** `commands/get.rs`, `cli.rs` (GetArgs), `output.rs` (GetOutput),
`voice.rs` (render_get), `main.rs` (wiring), `plan.rs` (read_snapshot_dir visibility)

---

## Executive Summary

Clean, focused implementation that follows established patterns and stays within scope.
The core data flow — resolve path → match subvolume → find snapshot → stream file — reads
top-to-bottom in 137 lines. The main concerns are around the snapshot filtering logic,
which silently matches on `short_name` rather than subvolume name, creating a subtle
correctness risk for configs where `short_name` overlaps between subvolumes. Path safety
is well-handled with defense-in-depth.

## What Kills You

**Catastrophic failure mode:** Returning the wrong file version silently. The user recovers
what they think is yesterday's draft but it's last week's — or from the wrong subvolume
entirely.

**Distance to catastrophe:** The implementation is two hops from this:
1. The `short_name` filter (line 46) could match snapshots from a different subvolume if
   short_names overlap (Finding 1).
2. If subvolume source paths aren't normalized consistently between config and user input,
   `strip_prefix` fails and the user gets a confusing error — not silent misrecovery, but a
   broken experience (moderate concern, well-handled by `resolve_path`).

The defense-in-depth `starts_with` check (line 79) prevents the worst outcome (escaping
snapshot boundaries). The architecture is sound.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | `short_name` filtering creates a subtle correctness risk; snapshot dir structure assumption is unverified |
| Security | 4 | Defense-in-depth path validation; read-only, no sudo; traversal prevention is solid |
| Architecture | 4 | Clean insertion into existing patterns; proper separation of concerns; reuses `read_snapshot_dir` |
| Systems Design | 4 | Streaming I/O, helpful error messages, stderr/stdout separation |
| Rust Idioms | 4 | Natural ownership, proper error handling with anyhow, no unnecessary allocations |
| Code Quality | 4 | 19 well-targeted tests, readable top-to-bottom, proportional coverage |

## Design Tensions

### Tension 1: Filtering by `short_name` vs. Snapshot Directory Structure

The implementation reads all snapshots from the snapshot directory
(`<root>/<subvol_name>/`) then filters by `short_name` (line 46). This assumes that the
snapshot directory already scopes to the right subvolume — which it does, because the
directory is `<root>/<subvol.name>/`. The `short_name` filter is therefore redundant in the
normal case (all snapshots in `htpc-home/` have short_name `htpc-home`).

But it's not entirely redundant — if a stray snapshot from another subvolume somehow lands
in the wrong directory (manual copy, migration error), the filter prevents selecting it.
This is a reasonable belt-and-suspenders approach.

**Verdict:** The filter is harmless and provides mild safety. The real question is whether
it's necessary at all (see Finding 1).

### Tension 2: `normalize_path` Without Filesystem Access vs. `canonicalize`

The implementation normalizes paths by walking `Component` values rather than calling
`std::fs::canonicalize()`. This was a conscious choice from the design review: `canonicalize`
requires the file to exist, but `urd get` needs to work on paths to deleted files.

**Verdict:** Correct trade-off. The component-based normalization handles `..` and `.` without
touching the filesystem. It doesn't resolve symlinks, which means a symlinked path won't match
the subvolume source — but that's the right behavior. The user should use the canonical path.
Documenting this limitation would be helpful but isn't blocking.

### Tension 3: Metadata on stderr vs. Silent Operation

The implementation always prints metadata to stderr via `eprint!` (line 115). This means
`urd get file --at yesterday > restored.txt` prints "Retrieving from snapshot..." to the
terminal while writing content to the file. This is the right call — stderr is the diagnostic
channel, and the user needs to know which snapshot was selected.

**Verdict:** Correct. Matches `git show`, `curl`, and other Unix tools that separate content
from diagnostics.

## Findings

### Finding 1: `short_name` Filter May Be Unnecessary and Is Subtly Fragile (Moderate)

**What:** Line 46 filters snapshots by `short_name`:

```rust
snapshots.retain(|s| s.short_name() == subvol.short_name);
```

The snapshot directory is already scoped to the subvolume (`<root>/<subvol.name>/`). Within
that directory, all snapshots should have a matching `short_name` — the planner creates
snapshots with the subvolume's `short_name` in the name.

The fragility: `short_name` is a config field set by the user. If two subvolumes share a
`short_name` (config allows this — there's no uniqueness validation), and a snapshot from
one ends up in the other's directory, the filter would do the wrong thing. More practically:
if a subvolume's `short_name` was ever changed in config without renaming existing snapshots,
this filter would hide all historical snapshots.

**Consequence:** After a `short_name` change in config, `urd get` silently returns "no
snapshots found" even though the directory is full of snapshots with the old name. This is
a confusing UX failure, not data loss, but it undermines trust at the moment the user needs
to recover a file.

**Recommendation:** Remove the `short_name` filter. The snapshot directory is already the
correct scope — `<root>/<subvol.name>/` contains only snapshots for that subvolume. If you
want to keep the filter as validation, make it a warning rather than a silent filter:

```rust
let mismatched: Vec<_> = snapshots
    .iter()
    .filter(|s| s.short_name() != subvol.short_name)
    .collect();
if !mismatched.is_empty() {
    log::warn!("{} snapshots in {} have unexpected short_name",
        mismatched.len(), snapshot_dir.display());
}
```

### Finding 2: `expect()` in Library-Adjacent Code (Minor)

**What:** `parse_date_reference` uses `.expect("valid HMS")` on `and_hms_opt(23, 59, 59)`
(lines 192, 201, 205). CLAUDE.md says: "No `unwrap()` / `expect()` in library code — only
in tests and `main.rs`."

These are in `commands/get.rs`, which is CLI-layer code — arguably the application boundary
where anyhow is appropriate. And `23, 59, 59` is a compile-time constant that can never fail.

**Consequence:** No runtime risk — these will never panic. But it sets a precedent that
`expect()` is acceptable in command code. Future contributors might use `expect()` on less
certain values.

**Recommendation:** This is borderline. The values are constant and provably valid.
If you want to be strict about the convention, replace with:

```rust
let end_of_day = |d: NaiveDate| -> NaiveDateTime {
    d.and_hms_opt(23, 59, 59)
        .unwrap_or_else(|| d.and_hms_opt(23, 59, 0).expect("23:59:00 is always valid"))
};
```

But honestly, `.expect("valid HMS")` on `(23, 59, 59)` is fine. The convention exists to
prevent panics on untrusted input, and this is trusted, constant input. No action needed
unless you want strict adherence.

### Finding 3: `--output` Overwrites Without Warning (Moderate)

**What:** `std::fs::copy()` (line 119) silently overwrites the destination file if it
exists. The user might accidentally overwrite a file they care about:

```bash
urd get /home/report.txt --at yesterday --output /home/report.txt
```

This replaces the current `report.txt` with yesterday's version — the current version is
gone.

**Consequence:** The user loses the current version of the file they were trying to restore
alongside. This is ironic for a backup tool. The damage is recoverable (the current version
is in a snapshot), but the user may not realize what happened.

**Recommendation:** Check if the output file exists and error:

```rust
if let Some(output_path) = &args.output {
    if output_path.exists() {
        bail!(
            "output file already exists: {}\n\
             Use a different path to avoid overwriting.",
            output_path.display(),
        );
    }
    // ... proceed with copy
}
```

This is a one-line safety check that prevents a confusing experience. If the user wants to
overwrite, they can delete the file first. A future `--force` flag could skip this check.

### Finding 4: Streaming I/O and stderr/stdout Separation (Commendation)

**What:** The implementation correctly separates content (stdout) from metadata (stderr),
uses `BufReader` + `std::io::copy()` for streaming without buffering the entire file, and
locks stdout once for the copy. This is textbook Unix tool behavior.

**Why this is good:** A user can do `urd get large-video.mkv --at yesterday > recovered.mkv`
and see the metadata message on the terminal while the file streams to disk. No memory
pressure regardless of file size. The `stdout.flush()` at the end ensures the last buffer
is written before the process exits.

### Finding 5: Defense-in-Depth Path Validation (Commendation)

**What:** Path safety is enforced at three layers:

1. `resolve_path` normalizes `..` and `.` components before matching (line 16)
2. `validate_no_traversal` rejects `..` in the relative path after `strip_prefix` (line 73)
3. `starts_with(&snapshot_dir)` on the final constructed path (line 79)

**Why this is good:** Any single layer could have a bug. Together, they form a defense-in-depth
chain where an attacker would need to defeat all three. For a read-only command this is
arguably over-engineered — but path validation is the kind of thing where over-engineering
is the right call. If `urd get` ever gains write capabilities (e.g., restoring into the live
filesystem), these checks are already in place.

### Finding 6: Error Messages Guide the User (Commendation)

**What:** Error messages throughout the command are specific and actionable:

- No subvolume match: lists all configured sources
- No snapshot before date: shows earliest available snapshot
- File not found: suggests trying a different date
- Directory encountered: suggests `--output` or specifying a file within

**Why this is good:** This follows the UX principle from CLAUDE.md: "Guide through affordances,
not error messages." These errors don't just report failure — they tell the user what to do
next. This is especially important for a restore command where the user is already stressed
about lost data.

### Finding 7: `read_snapshot_dir` Visibility Change Is Clean (Commendation)

**What:** Rather than duplicating snapshot-reading logic, the implementation makes
`read_snapshot_dir` `pub(crate)` with a one-word change. No new snapshot-reading code.

**Why this is good:** `read_snapshot_dir` is already tested and handles both snapshot name
formats, hidden file filtering, and missing directories. Sharing it eliminates a category
of bugs (inconsistent snapshot parsing between commands). The `pub(crate)` visibility is
precisely scoped — other crates can't depend on it, but all commands within Urd can.

## The Simplicity Question

**What could be removed?**

- The `short_name` filter (Finding 1) could be removed — the directory structure already
  scopes snapshots correctly. This removes a subtle fragility.
- The `GetOutput` struct and `render_get` voice function are minimal (5 fields, 10 lines of
  rendering). They could be inlined as a simple `eprintln!`. But following the presentation
  layer pattern is correct — it keeps daemon mode (JSON) working and maintains consistency
  for future commands.

**What's earning its keep?**

- `resolve_path` + `normalize_path`: Essential. Without them, relative paths and `..` break
  the subvolume matching.
- `find_subvolume_for_path`: The UX differentiator. Without it, users need to know subvolume
  names. 5 lines of code, well-tested.
- `select_snapshot`: 4 lines, does exactly one thing. Clean.
- `validate_no_traversal` + `starts_with` check: Defense-in-depth on path safety. Worth it.
- The 19 tests: Well-targeted at the functions that could go wrong. Each test is 3-8 lines.
  No test is testing an obvious tautology.

**What's the total cost?** ~200 lines of implementation + tests. The command touches 7 files
but only adds substantial code to one (`commands/get.rs`). The other changes are 1-5 lines
each. This is proportionate to the feature.

## Priority Action Items

1. **Remove or warn on `short_name` filter** (Finding 1). The directory structure already
   scopes correctly. The filter creates a subtle failure mode after `short_name` changes.

2. **Add overwrite protection for `--output`** (Finding 3). One existence check prevents
   accidental data loss — appropriate for a backup tool.

3. **Consider `--force` flag** for future use with `--output` overwrite. Not needed now,
   but note it as a natural extension point.

## Open Questions

1. **Should `urd get` work on disabled subvolumes?** The design review recommended yes
   (Open Question 3). The current implementation does — `find_subvolume_for_path` doesn't
   check `enabled`. This is correct: a user restoring a file doesn't care about backup
   configuration.

2. **Should the `--at` value support snapshot names directly?** A user looking at `urd status`
   output might want to copy-paste a snapshot name like `20260320-0200-htpc-home`. The current
   implementation supports `20260320` (YYYYMMDD format), which selects the end-of-day snapshot.
   Full snapshot name support would require parsing the name and doing exact match — a natural
   extension but not needed for v1.

3. **`local_snapshot_dir` has `#[allow(dead_code)]`** — this method was previously unused.
   Now that `urd get` calls it, the allow can be removed. Minor cleanup.
