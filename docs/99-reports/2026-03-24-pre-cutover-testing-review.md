# Pre-Cutover Testing Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Version bump (`0.1.0` → `0.2.2026-03-24`), mkdir fix in `executor.rs`, manual test
results from Tests 1–8, and the testing process itself.
**Commit:** `85af7bd` (base) + uncommitted changes (version bump + mkdir fix)
**Reviewer:** Architectural adversary

---

## Executive Summary

The test run uncovered a real bug (missing `mkdir` before `btrfs receive`) and fixed it
correctly — but the fix was applied reactively to unblock testing rather than derived from
a systematic analysis of the executor's precondition contract. The testing process itself
was thorough on read-only commands but stopped at the boundary where it matters most: actual
backup execution. The legacy pin file fallback is a ticking false-positive generator that
will erode trust in `urd verify` during the parallel-run phase — the exact moment when trust
matters most. Both issues are fixable before cutover.

## What Kills You

**Silent data loss.** For Urd, this means: (1) deleting a snapshot that's the last copy of
data, or (2) believing a backup succeeded when it didn't, or (3) sending data to the wrong
drive. The mkdir bug was one step from scenario (2) — sends fail, the error is logged, but
the heartbeat still writes, and the user may not check the journal. The legacy pin fallback
is zero steps from eroding trust in verify — the tool that's supposed to catch problems is
itself producing false positives.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | mkdir fix is correct but reveals a missing precondition contract; pin fallback produces false positives on real data |
| Security | 4 | No new attack surface; mkdir uses unprivileged `create_dir_all` on user-owned paths |
| Architecture | 3 | Fix is placed correctly but isn't derived from the architecture; no executor precondition system exists |
| Systems Design | 3 | Crash recovery interaction is sound; testing process skipped the most important tests |
| Rust Idioms | 4 | Let-chain guard is clean; error handling follows project conventions |
| Code Quality | 3 | No test for the new code path; testing journal mixes framework with results |

## Design Tensions

### Tension 1: Reactive fix vs. executor precondition contract

The mkdir fix was discovered by running `urd backup --dry-run`, noticing the plan included
full sends, then checking whether destination directories exist. This is good diagnostic
instinct — but the fix addresses the symptom (directory doesn't exist) without establishing
the principle (the executor should verify its preconditions before acting).

**The executor currently has zero precondition checks.** It receives a `BackupPlan` and
trusts that every path in it is valid. The planner checks drive availability, but between
plan and execution, drives can unmount, directories can be deleted, permissions can change.
The mkdir fix is the first precondition check — but it exists as a special case for one
specific failure, not as a systematic pattern.

**What should exist:** A precondition phase at the top of `execute_send()` that verifies
dest_dir's parent exists (drive is still mounted) before doing anything. The mkdir becomes
a natural consequence: parent exists + child missing = create child. Parent missing = drive
went away, skip with clear error. This is the same pattern as the `failed_creates` cascading
check that already exists at line 314.

**Verdict:** The fix is correct and should ship. But it should be understood as the first
instance of an executor precondition pattern, not a one-off. Document this intent in a
comment, and when Priority 2c (pre-flight checks) is built, fold it in.

**Severity: Moderate.** The fix works. The missing pattern is technical debt, not a bug.

### Tension 2: Legacy pin fallback vs. verify accuracy

**This is the most important finding in this review.**

The pin file system has a legacy fallback: if no drive-specific pin file exists
(`.last-external-parent-{DRIVE}`), it reads the legacy file (`.last-external-parent`).
This was designed for backward compatibility with the bash script.

On this real system, the bash script is **still running nightly at 02:00** and **still
writing legacy pin files**. The evidence:

```
subvol3-opptak/.last-external-parent       → "20260324-opptak"  (written 02:00 today by bash)
subvol3-opptak/.last-external-parent-WD-18TB1 → "20260324-opptak"  (written 02:00 today by bash)
subvol3-opptak/.last-external-parent-2TB-backup → DOES NOT EXIST
```

When `urd verify` checks subvol3-opptak against 2TB-backup:
1. Looks for `.last-external-parent-2TB-backup` — not found
2. Falls back to `.last-external-parent` — reads `20260324-opptak`
3. Checks if `20260324-opptak` exists on 2TB-backup — it doesn't (only on WD-18TB1)
4. Reports: **"FAIL Pinned snapshot missing from drive: 20260324-opptak"**

This is a **false positive**. The legacy file was written by a send to WD-18TB1, not to
2TB-backup. The verify command is checking the wrong pin for the wrong drive.

**Consequence during parallel run:** During the cutover parallel-run phase, both the bash
script and Urd will be running. The bash script writes legacy pins. Urd's verify reads them
and reports false chain breaks. The operator (you) will see `FAIL` lines in verify output
and have to mentally filter "is this a real failure or a legacy pin artifact?" — exactly
the kind of attention tax that Urd's design philosophy says the user should never have to pay.

**The fix is not to remove the fallback** — it's needed for subvolumes that have never had
a drive-specific pin written. The fix is:

1. **In verify:** When reporting a chain break, check whether the pin came from a legacy file
   or a drive-specific file. If legacy, downgrade from FAIL to WARN with a message like
   "legacy pin file — run a backup to create drive-specific pins."
2. **In the bash script (during parallel run):** Or simply accept that verify will be noisy
   until Urd has run enough backups to create drive-specific pins for all subvolume/drive
   pairs.
3. **Long-term:** After cutover, clean up legacy pin files (already in the cutover checklist
   Step 4).

**Severity: Significant.** Not a data-safety issue, but an operator-trust issue at the worst
possible time (parallel run). False positives in safety tools train people to ignore real
failures.

### Tension 3: Testing stopped before the tests that matter

Tests 1–8 (read-only) were executed systematically. Tests 9–11 (actual backups) were deferred
because they "need sudo" and "modify real filesystem state." This is the correct instinct for
safety — but it means the testing session validated that Urd can *describe* what it would do,
not that it can *do* it.

The mkdir bug was found by *inspecting* destination directories, not by *running* a backup
and observing the failure. That's good — but it means we don't know whether the fix actually
works on real drives. The fix has zero test coverage: no unit test, no manual verification on
real hardware.

**What a senior engineering team would do differently:**

1. **Test the fix on real hardware before declaring it fixed.** Run
   `urd backup --subvolume htpc-home` (a small, fast subvolume) and observe whether the
   mkdir log line appears. This is Test 9 and it should have been the immediate next step
   after applying the fix, not deferred.

2. **Add a unit test for the new code path.** The executor test suite uses `MockBtrfs` with
   fake paths. The mkdir fix is guarded by `parent.exists()`, which returns false for fake
   paths, so the fix is invisible to existing tests. A test using `tempfile::TempDir` (like
   `pin_on_success_writes_pin_file` already does) would exercise the real mkdir path.

3. **Test the failure mode, not just the fix.** What happens when `create_dir_all` fails?
   Permission denied on an external drive? The fix returns `OpResult::Failure` — but does
   the rest of the executor handle that correctly? Does the heartbeat still write? Does the
   summary show the failure clearly?

**Severity: Significant.** An untested fix to the executor — the component that runs
`sudo btrfs` — should not go to production without at least one real execution.

### Tension 4: Version scheme signals maturity the code hasn't proven

Bumping to `0.2.2026-03-24` communicates "this is a real build, not the prototype." But the
build has never completed a full backup run. The version will appear in heartbeat files and
systemd journal output. If the first real run fails, the version number creates a false
impression of tested readiness.

This is minor — version numbers are metadata, not code. But it's worth being honest about:
this is a development build being tested, not a release candidate that passed QA.

**Severity: Minor.** Cosmetic. Ship it, but don't let the version number substitute for the
validation that Tests 9–11 would provide.

## Findings

### Finding 1: mkdir fix has no test coverage (Significant)

**What:** The new code at `executor.rs:335-356` is not exercised by any unit test. The
`parent.exists()` guard returns false for all existing test fixtures (which use synthetic
paths like `/mnt/test/.snapshots/sv-a`), so the mkdir logic is dead code in the test suite.

**Consequence:** If someone refactors the guard condition (e.g., removes the `parent.exists()`
check), no test will catch the regression. The fix could silently break and reintroduce the
original March 23 failure.

**Fix:** Add a test using `tempfile::TempDir` that:
1. Creates a parent dir (simulating the drive's `.snapshots` root)
2. Does NOT create the subvolume subdir
3. Runs `execute_send()` with a `MockBtrfs`
4. Asserts the subdir was created
5. Asserts `MockBtrfs` received the `send_receive` call with the correct dest_dir

Also add a test where parent doesn't exist, verifying `OpResult::Failure` is returned
without calling `send_receive`.

### Finding 2: Legacy pin fallback produces false positives in verify (Significant)

**What:** `chain::read_pin_file()` falls back to the legacy `.last-external-parent` file
when no drive-specific pin exists. During the parallel run, the bash script writes legacy
pins for sends to WD-18TB1. When verify checks these against 2TB-backup, it reports broken
chains that aren't actually broken.

**Consequence:** Two false FAILs in the current verify output. During the parallel-run phase,
this will produce false alerts every time verify runs, training the operator to discount FAIL
messages — including real ones.

**Evidence:**
```
subvol3-opptak: legacy pin = "20260324-opptak" (bash sent to WD-18TB1)
                pin-2TB-backup = MISSING → falls back to legacy → checks 2TB-backup → FAIL
subvol2-pics:   legacy pin = "20260322-pics" (bash sent to WD-18TB1)
                pin-2TB-backup = MISSING → falls back to legacy → checks 2TB-backup → FAIL
```

**Fix options (pick one):**
- **(A) Best:** In verify, track whether the pin came from a legacy file. If legacy, emit
  WARN "chain status unknown for {drive} (using legacy pin from pre-Urd backup)" instead
  of FAIL.
- **(B) Quick:** After the first successful Urd backup run creates drive-specific pins for
  all subvolume/drive pairs, delete the legacy pin files. The false positives disappear.
- **(C) Minimal:** Document in the testing journal that these 2 FAILs are known false
  positives from legacy pins, and ignore them during parallel run. Risky — new false
  positives from the same cause won't be obvious.

### Finding 3: Executor has no systematic precondition checking (Moderate)

**What:** The executor trusts the plan completely. The only pre-execution check is
`failed_creates` (cascading failure from snapshot creation). The new mkdir is a second
precondition check, but it's ad-hoc — not part of a pattern.

**Consequence:** Other precondition failures will surface as btrfs subprocess errors rather
than clear Urd error messages. Examples: drive unmounted between plan and execute, snapshot
root permissions changed, disk full before send starts.

**This is not a cutover blocker.** But it's the natural home for the mkdir logic and for
Priority 2c (pre-flight checks). When 2c is built, the executor should gain a
`verify_preconditions()` step that runs before any btrfs call.

### Finding 4: The bash script is actively writing legacy pins (Moderate)

**What:** The bash timer runs at 02:00 and writes `.last-external-parent` (legacy) files.
These files are being updated *today*, not just leftover from the past.

**Consequence:** Even after Urd creates drive-specific pins, the bash script will keep
overwriting the legacy files on its nightly run. During the parallel run, pins from two
systems will coexist, and the legacy file's semantics (which drive?) are ambiguous.

**Fix:** This resolves itself when the bash timer is disabled (cutover Step 3). But during
the parallel run, be aware that legacy pin files are not static artifacts — they're being
actively mutated by the bash script. If Urd's planner uses `find_pinned_snapshots()` for
retention protection, it may over-protect snapshots that are pinned only by the legacy file,
preventing retention cleanup.

### Finding 5: Testing journal mixes framework with results (Minor)

**What:** The journal `2026-03-24-pre-cutover-testing.md` was written as a test plan, then
the results were appended in Part 6. The test plan sections still contain `[ ]` checkboxes
that weren't updated, the "Open Questions" section was partially answered but not revised,
and the mkdir fix description in Part 4 was written *before* the fix was applied.

**This is normal for a working document**, but it means someone reading the journal later
can't quickly tell which tests passed, which were skipped, and what the final state was.

**Fix:** Not worth reworking now. The results in Part 6 are clear. But for future test runs:
write the plan and results as separate documents, or update the checkboxes as tests are
executed.

### Finding 6: Planner space check uses dest_dir without existence guarantee (Minor)

**What:** In `plan.rs` line 527, `fs.filesystem_free_bytes(&ext_dir)` is called where
`ext_dir` is the same path the executor will receive as `dest_dir`. If the subvolume subdir
doesn't exist, `statvfs` will fail. However, `statvfs` on a non-existent path where the
*parent* exists will return the parent's filesystem stats — so this probably works in
practice for the first-send case (parent `.snapshots` exists, child `subvol-name` doesn't).

**Consequence:** Likely none — `statvfs` resolves to the mount point. But this is an
implicit assumption worth noting.

### Commendation: The mkdir guard is well-designed

The three-condition guard (`!dest_dir.exists() && parent.exists()`) is the correct design:

1. **Idempotent:** If dest_dir already exists, skips entirely. Safe to re-run.
2. **Fail-safe:** If parent doesn't exist (drive unmounted, wrong path), doesn't attempt
   mkdir and lets `btrfs receive` produce the real error. No masking of root causes.
3. **Test-compatible:** Fake paths in tests have no real parent, so the guard skips mkdir,
   preserving existing test behavior.
4. **Uses `create_dir_all`:** Handles intermediate directories if somehow the snapshot root
   has nested structure.

This is the right fix. It just needs a test and a comment connecting it to the future
precondition pattern.

### Commendation: Test ordering (least-invasive to most-invasive) is correct

The testing framework's ordering — status, plan, verify, history, get, init, dry-run, then
real backups — is exactly how a senior team would structure a validation pass. Each test
builds confidence for the next. The read-only tests verified that Urd's *model* of the
system is correct before trusting it to *act* on the system.

## The Simplicity Question

**What could be removed?** Nothing in the fix itself. The guard is minimal.

**What should be added?** One unit test. That's the minimum to make this fix reviewable and
maintainable.

**What's earning its keep?** The planner/executor separation. The mkdir bug exists because
the planner doesn't know about filesystem preconditions — that's the executor's job. The
architecture made it obvious where the fix belongs and where it doesn't. The MockBtrfs
infrastructure meant the fix could be validated against 214 tests in 0.02 seconds.

## Priority Action Items

Ordered by consequence, not effort:

1. **Run Test 9 (single-subvolume backup) on real hardware.** This is the most important
   remaining action. The mkdir fix, UUID detection, heartbeat writing, metrics output, and
   pin file creation all need one real execution to validate. htpc-home is small and fast.
   Do this before committing.

2. **Add a unit test for the mkdir code path.** Use `tempfile::TempDir`. Two cases: parent
   exists (mkdir succeeds, send proceeds), parent missing (mkdir skipped, returns failure).
   This takes 15 minutes and prevents regression forever.

3. **Decide on the legacy pin strategy before the parallel run.** Option A (downgrade to WARN
   in verify) is cleanest. Option B (delete legacy files after first Urd run) is fastest.
   Either way, don't start the parallel run with verify producing false FAILs — it will
   undermine confidence at the worst time.

4. **Add UUIDs to `urd.toml` for WD-18TB1 and 2TB-backup.** The UUIDs have been discovered.
   This is a 2-minute config edit that activates the safety feature you just built and
   shipped.

5. **Commit the version bump + mkdir fix as a single coherent change.** The version bump is
   a release marker; the mkdir fix is a bug fix. They could be separate commits, but they're
   small enough to bundle as "prepare v0.2 for cutover testing."

6. **Add a comment to the mkdir block connecting it to Priority 2c.** Something like:
   `// First executor precondition check. See Priority 2c for the systematic pattern.`
   This prevents the next developer from treating it as a one-off hack.

7. **Run Test 11 (full backup) before installing the systemd timer.** This is the final
   gate. Everything else is warm-up.

## Open Questions

- The bash script runs at 02:00 and Urd's timer is also set to 02:00. During the parallel
  run, bash shifts to 03:00. But what if they overlap due to `RandomizedDelaySec=300`? Is
  there a lock file that prevents concurrent btrfs operations, or could both systems try to
  snapshot the same subvolume simultaneously?

- `urd init` found an orphaned snapshot `20250422-multimedia` on WD-18TB1 from February 2025.
  This is a 13-month-old partial transfer. The interactive delete prompt appeared but wasn't
  answered. Should this be cleaned up before cutover, or left for Urd's crash recovery to
  handle on first run?

- The test run was executed via a non-TTY shell (Claude Code's Bash tool), so all output was
  daemon-mode JSON. The interactive (colored table) output path of `urd status` was not
  actually tested. Should be verified in a real terminal.

---

*Reviewed at commit `85af7bd` + uncommitted changes (version bump, mkdir fix).*
*214 unit tests passing. 0 integration tests (none exist). Manual tests 1–8 executed, 9–11 pending.*
