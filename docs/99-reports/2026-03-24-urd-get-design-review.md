# `urd get file@date` — Architectural Adversary Design Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 3d design review (pre-implementation)
**Reviewer:** Architectural Adversary (Claude)
**Commit:** 3450d90 (master)

---

## Executive Summary

`urd get` is a small, well-scoped feature with a clear insertion point into the existing
architecture. The hardest design problems are not in the code — they're in the UX: how to
specify the file, how to resolve ambiguous dates, and where to put the output. Get these
wrong and the command is confusing enough that nobody uses it. Get them right and it's the
fastest path from "I need that file back" to having it. The main architectural risk is
subvolume resolution from a user-provided file path — the mapping between source paths and
snapshot directories is indirect, and getting it wrong means silently looking in the wrong
snapshot tree.

## What Kills You

**Catastrophic failure mode for a restore command:** Returning the wrong file version
silently. The user thinks they recovered yesterday's draft, but they got last week's. Or
the file is from the wrong subvolume entirely (e.g., the root subvolume's `/home` instead
of the home subvolume).

Distance to catastrophe: one wrong subvolume match + one wrong snapshot selection = silent
data misrecovery. This is not data loss (the snapshots are still there), but it undermines
trust in the tool at the moment trust matters most — when the user is trying to recover
something they lost.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | Subvolume resolution and snapshot date matching are subtle; wrong answers are silent |
| Security | 4 | Read-only, no sudo, but path traversal from snapshot boundary needs care |
| Architecture | 4 | Clean insertion point, follows established patterns, minimal new machinery |
| Systems Design | 4 | Simple read-only operation; edge cases are UX problems, not systems problems |
| Rust Idioms | N/A | Pre-implementation review |
| Code Quality | N/A | Pre-implementation review |

## Design Tensions

### Tension 1: `@` Syntax vs. Separate Arguments

Two possible syntaxes:

- **`urd get /home/docs/file.txt@yesterday`** — single argument, `@` delimiter. Ergonomic,
  memorable, echoes `git show HEAD:file`. But `@` is legal in filenames. Ambiguous: does the
  user mean the file `file.txt@yesterday` or "file.txt at yesterday"?

- **`urd get /home/docs/file.txt --at yesterday`** — separate arguments. Unambiguous, standard
  CLI pattern, plays well with clap. Less memorable, more typing.

**Resolution:** Use separate arguments: `urd get <path> --at <date>`. Reasons:

1. `@` in filenames is uncommon but legal. The ambiguity is not theoretical — a user who has a
   file named `report@2026.txt` hits it immediately. Parsing `@` from the right helps but
   doesn't eliminate ambiguity (what about `report@2026@yesterday`?).
2. Clap handles `--at` natively with validation, help text, and shell completions. The `@`
   syntax would require custom parsing before clap sees it.
3. The command name `urd get` already signals "retrieve a file." The `--at` flag names the
   time dimension explicitly. This is clearer for new users.
4. Future extension is easier: `--at yesterday --subvolume home` vs. trying to pack more
   modifiers into a single string.

Keep `@date` in documentation as the conceptual shorthand, but implement `--at`.

### Tension 2: Subvolume Resolution — Automatic vs. Explicit

The user provides a file path like `/home/documents/report.txt`. Urd needs to determine
which subvolume (and therefore which snapshot directory) contains this file. Two approaches:

- **Automatic:** Match the path against all configured `source` paths. Longest prefix wins.
  `/home/documents/report.txt` matches source `/home` (subvolume `htpc-home`), not source `/`
  (subvolume `htpc-root`).

- **Explicit:** Require `--subvolume htpc-home`. No guessing.

**Resolution:** Automatic with longest-prefix matching, plus `--subvolume` override for
disambiguation. Reasons:

1. The user thinks in file paths, not subvolume names. "I want my report back" → they know
   the path. Forcing `--subvolume` adds a lookup step that breaks flow.
2. Longest-prefix is deterministic and correct for the existing config (no overlapping sources
   except `/` which is always the shortest match).
3. The `--subvolume` override handles the rare case where automatic matching is wrong or the
   user wants to check a specific subvolume.
4. When automatic matching is ambiguous or fails, emit a clear error listing the matched
   subvolumes and asking the user to specify.

**Critical invariant:** Root subvolume (`source = "/"`) must never win over a more specific
match. The algorithm is: collect all subvolumes whose `source` is a prefix of the input path,
select the one with the longest `source` path. If none match, error. If only `/` matches,
use it (it's the right answer — the file is on the root subvolume).

### Tension 3: Snapshot Date Resolution — Nearest Before vs. Nearest Overall

Given `--at 2026-03-20`, which snapshot to pick?

- **Nearest before:** The most recent snapshot whose datetime is ≤ the target date. This is
  the "time travel" semantic — "show me what the file looked like at that time." It can never
  return a version that didn't exist yet at the requested time.

- **Nearest overall:** The snapshot closest to the target date in either direction. More
  forgiving of imprecise dates but potentially confusing — "I asked for yesterday's version
  and got today's."

**Resolution:** Nearest before (or equal). This is the correct semantic for a backup tool:

1. "What did the file look like on March 20th?" means "the state as of March 20th" — not
   a state that came into existence on March 21st.
2. It matches `git log --before` and macOS Time Machine behavior. Users have existing
   mental models.
3. For relative dates like "yesterday," the target resolves to yesterday at 23:59:59 (end
   of day), so the most recent snapshot from yesterday is selected.
4. If no snapshot exists before the target date, error clearly: "no snapshot found before
   {date} for subvolume {name}. Earliest available: {date}."

**Edge case — time-of-day precision:** When the user says `--at 2026-03-20`, do they mean
midnight (start of day) or end of day? End of day is more useful: "show me the March 20th
version" almost always means "the latest version from that day." When a specific time is
given (`--at "2026-03-20 14:30"`), use it exactly.

### Tension 4: Output Destination — stdout vs. File Copy

- **stdout:** Pipe-friendly (`urd get path --at yesterday | diff - path`). Simple. But
  binary files corrupt the terminal, and large files flood it.

- **File copy to current directory:** Safe for all file types. But overwrites risk and
  naming collisions.

- **Explicit `--output`:** Flexible but more typing for the common case.

**Resolution:** stdout by default, with `--output <path>` for file copy. Reasons:

1. stdout is the Unix default for read-only retrieval commands (`cat`, `git show`). Users
   expect it.
2. The common case is "show me what this file looked like" — piping to `less`, `diff`, or
   another command. stdout enables this.
3. Binary files on stdout are the user's problem (same as `cat binary`). Urd is not an
   interactive file browser.
4. `--output` handles the "save to disk" case explicitly, without the risk of accidental
   overwrites that a "copy to current dir" default would create.
5. For directories, stdout doesn't make sense — error clearly and suggest `--output`.

## Findings

### Finding 1: Subvolume Source-Path Matching Needs Canonicalization (Significant)

**What:** The user might provide `/home/documents/../documents/report.txt` or a path with
symlinks. The config source is `/home`. Prefix matching on raw strings would fail or produce
wrong results.

**Consequence:** The command silently fails to find the right subvolume, or worse, matches
the wrong one. This is one step from the catastrophic failure mode.

**Recommendation:** Canonicalize the user-provided path before matching:

1. Expand `~` using the existing `expand_tilde()` from `config.rs`.
2. Use `std::fs::canonicalize()` to resolve symlinks and `..` — but only if the file
   exists on the live filesystem. For deleted files, use `std::path::Path::components()`
   to normalize without filesystem access (strip `..` and `.` components manually).
3. The config's `source` paths are already validated as absolute with no `..` (via
   `validate_path_safe`), so they're safe to compare against.
4. Compare using `std::path::Path::starts_with()`, which handles path components correctly
   (won't match `/home2` against `/home`).

**Critical:** `Path::starts_with()` is component-aware in Rust — `/home/foo` starts with
`/home` but `/home2/foo` does not. This is the right primitive. Do not use string prefix
matching.

### Finding 2: Relative Path Input Needs Resolution (Significant)

**What:** The user might type `urd get documents/report.txt --at yesterday` (relative path).
This needs to resolve against the current working directory before subvolume matching.

**Consequence:** A relative path won't match any subvolume source (all sources are absolute).
The command errors with "no subvolume found" — confusing when the file clearly exists.

**Recommendation:** Resolve relative paths to absolute before matching:

```rust
let path = if path.is_relative() {
    std::env::current_dir()?.join(&path)
} else {
    path.to_path_buf()
};
```

Then normalize (strip `.` and `..` components). This is standard CLI behavior — `git`,
`find`, and `rsync` all resolve relative paths against cwd.

### Finding 3: Date Parsing Scope — Keep It Small (Moderate)

**What:** The spec says "smart date matching: yesterday, last week, 2026-03-15." The
temptation is to build a natural language date parser. This is a rabbit hole.

**Consequence:** Over-engineering date parsing delays the feature and introduces edge cases
(does "last week" mean 7 days ago? Monday of last week? Sunday?). Every relative term needs
a documented definition. Timezone handling compounds the problem.

**Recommendation:** Support a minimal, unambiguous set:

| Format | Example | Resolves to |
|--------|---------|-------------|
| `YYYY-MM-DD` | `2026-03-15` | End of day (23:59:59) |
| `YYYY-MM-DD HH:MM` | `2026-03-15 14:30` | Exact time |
| `YYYYMMDD` | `20260315` | End of day (matches snapshot name prefix) |
| `yesterday` | | End of yesterday |
| `today` | | Now |

That's it. Five formats. No "last week," no "3 days ago," no "last Tuesday." Each of these
has one unambiguous interpretation. The snapshot name prefix `YYYYMMDD` format is included
because users will copy-paste from `urd status` output.

If natural language dates are wanted later, add them later. The parsing function is a
single match point — extending it doesn't require architectural changes.

### Finding 4: `read_snapshot_dir` Reuse (Commendation)

**What:** The `read_snapshot_dir()` function in `plan.rs` already reads a snapshot directory,
skips hidden files, and returns `Vec<SnapshotName>` with parsed datetimes. This is exactly
what `urd get` needs for snapshot resolution.

**Recommendation:** This function is currently file-private in `plan.rs`. Make it `pub(crate)`
or move it to a shared location (it's a utility, not planner logic). `urd get` calls it to
list snapshots for a subvolume, then selects by date.

**Why this is good:** The function is already tested, handles both snapshot name formats, and
filters hidden files correctly. No new snapshot-reading code needed.

### Finding 5: Path Traversal from Snapshot Boundary (Moderate)

**What:** After constructing the snapshot path
`<snapshot_root>/<subvol_name>/<snapshot_name>/<relative_path>`, the `relative_path` must
not escape the snapshot directory via `..`.

**Consequence:** Unlike the backup path validation (which protects against untrusted config),
this protects against user input. An attacker scenario is unlikely (the user is restoring
their own files), but a confused user typing `urd get ../../etc/shadow --at yesterday` could
read unexpected files from the snapshot.

**Recommendation:** After computing `relative_path` (the user's path minus the subvolume
source prefix), validate it contains no `..` components using the same
`Component::ParentDir` check used in `validate_path_safe()`. This is already a proven
pattern in `config.rs`.

Additionally: after joining `snapshot_base.join(relative_path)`, verify the result
`starts_with(snapshot_base)`. This is defense-in-depth — the component check should suffice,
but the starts_with check catches edge cases in path normalization.

### Finding 6: File Existence Check — Live vs. Snapshot (Minor)

**What:** The user provides a path that exists on the live filesystem. But the file might
not exist in the selected snapshot (it was created after the snapshot, or was deleted and
re-created).

**Consequence:** A clear "file not found in snapshot" error is needed. Without it, the user
sees a cryptic OS error.

**Recommendation:** After constructing the full snapshot path, check existence with
`Path::exists()`. If absent, provide a helpful error:

```
File not found in snapshot 20260320-1430-htpc-home.
The file may not have existed at that time.
Try an earlier or later date with --at.
```

If the file exists in the snapshot but the user's path doesn't exist on the live filesystem
(deleted file), that's fine — the user knows the path from memory or history. Don't require
the file to exist on the live filesystem.

### Finding 7: Presentation Layer Integration (Minor)

**What:** Should `urd get` use the presentation layer (`output.rs` + `voice.rs`)?

**Recommendation:** Minimally. The file content goes to stdout raw — no rendering, no
formatting, no mythic voice. But metadata messages (which snapshot was selected, warnings
about multiple matches) should go to stderr and use the voice layer in interactive mode.
In daemon mode, metadata goes to stderr as JSON.

This matches `git show` behavior: content on stdout, messages on stderr. It keeps the
content pipe-safe while allowing the voice to speak on the diagnostic channel.

Define a small `GetOutput` struct:

```rust
struct GetOutput {
    subvolume: String,
    snapshot: String,
    snapshot_date: NaiveDateTime,
    file_path: PathBuf,
    file_size: u64,
}
```

Render to stderr in interactive mode: "Retrieving report.txt from the well — woven on
2026-03-20 at 14:30." Daemon mode: JSON on stderr, content on stdout.

### Finding 8: Large File Handling (Minor)

**What:** `urd get` on a 50GB video file sends 50GB to stdout. This is technically correct
(same as `cat`) but potentially surprising.

**Recommendation:** No special handling needed. The user chose to pipe a large file — that's
their decision. But if `--output` is provided, use `std::fs::copy()` which is efficient
(sendfile/copy_file_range on Linux). Don't read the entire file into memory.

For stdout: use `std::io::copy()` from a `BufReader` to stdout. This streams without
buffering the entire file.

### Finding 9: Directory Retrieval (Moderate)

**What:** The user might point `urd get` at a directory, not a file. `urd get /home/projects --at yesterday` — what should happen?

**Consequence:** Sending a directory to stdout is nonsensical. But copying a directory
tree is a legitimate restore operation.

**Recommendation:** For v1, refuse directories with a clear error:

```
/home/projects is a directory in snapshot 20260320-1430-htpc-home.
Use --output <path> to restore directories, or specify a file within the directory.
```

With `--output`, recursively copy the directory using `fs_extra::dir::copy` or a simple
recursive walk. But this adds a dependency and scope. For v1, only support files. Add
directory restore later if users need it. The architecture doesn't change — it's the same
path resolution, just a different copy operation at the end.

## The Simplicity Question

**What could be removed?** The feature is already small. The risks are in adding too much:

- Don't build a date parser beyond the minimal set (Finding 3). Natural language parsing
  is a time sink with diminishing returns.
- Don't support directory restore in v1 (Finding 9). File-level is the common case.
- Don't add `--list` or `--browse` modes — that's `urd find` (unsolved, deferred).
- Don't add `--diff` mode — the user can pipe to `diff` themselves.

**What's earning its keep?**

- Automatic subvolume resolution (Tension 2): This is the UX differentiator. Without it,
  the user needs to know Urd's internal naming scheme.
- `--at` with date parsing: The whole point of the command. Keep the format set minimal.
- stdout default: Unix convention, enables composition.
- `read_snapshot_dir` reuse: Zero new snapshot-reading code.

**Estimated scope:** ~150-200 lines of new code:
- `commands/get.rs`: ~80-100 lines (path resolution, snapshot selection, file copy)
- `cli.rs` additions: ~15 lines (GetArgs struct)
- `plan.rs` change: 1 line (make `read_snapshot_dir` pub(crate))
- `output.rs` + `voice.rs`: ~30-40 lines (GetOutput struct + render function)
- Tests: ~50-80 lines (subvolume matching, date parsing, path validation)

## Priority Action Items

1. **Implement longest-prefix subvolume matching with canonicalization** (Finding 1,
   Tension 2). This is the hardest part and the closest to the catastrophic failure mode.
   Test exhaustively: overlapping sources, root subvolume, trailing slashes, symlinks.

2. **Resolve relative paths against cwd** (Finding 2). One line of code, but critical for
   usability. Users will type relative paths.

3. **Keep date parsing minimal** (Finding 3). Five formats, no natural language beyond
   "yesterday" and "today". Extend later if needed.

4. **Validate relative_path has no `..` components** (Finding 5). Reuse the existing
   `Component::ParentDir` check from `config.rs`.

5. **Make `read_snapshot_dir` accessible** (Finding 4). Change visibility from file-private
   to `pub(crate)`.

6. **Use stdout for content, stderr for metadata** (Finding 7). Keep content pipe-safe.

7. **Stream, don't buffer** (Finding 8). Use `std::io::copy()` with `BufReader`.

## Open Questions

1. **Should `urd get` work with external drive snapshots?** The spec only mentions local
   snapshots. External snapshots use a different directory structure
   (`drive_mount/snapshot_root/subvol_name/snapshot_name/`). Adding `--drive <label>` is
   straightforward but expands scope. Recommendation: local-only in v1, add `--drive` later.

2. **What if the file was renamed between snapshots?** `urd get` can only find files by
   their path at the time of the snapshot. If the user renamed `report.txt` to
   `report-final.txt`, they need to know the old name. This is inherent to the path-based
   approach and not worth solving in v1 (it would require an index, which is `urd find`
   territory).

3. **Should disabled subvolumes be searchable?** A subvolume with `enabled = false` still
   has snapshots on disk. `urd get` should search them — the user doesn't care about backup
   configuration when restoring a file. Filter disabled subvolumes from `urd backup`, not
   from `urd get`.

4. **Should `urd get` require config?** The command could work with just a snapshot
   root path, without full config. But subvolume resolution needs the source-to-name
   mapping from config. Keep the config requirement — it's consistent with all other
   commands and the config is already loaded in `main.rs`.
