# Architectural Adversary Review: Urd Phase 3.5

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** All source files (`src/`), post-Phase 3.5 hardening
**Commit:** `542c8bf`
**Coverage:** 128 unit tests, 0 failures, clippy clean
**Lines reviewed:** ~8,000 lines across 20 source files

---

## Executive Summary

Urd is a well-architected backup tool with strong fundamentals. The planner/executor separation is genuinely load-bearing — not a paper abstraction — and the defense-in-depth around pin protection means the catastrophic failure mode (silent data loss) requires multiple independent failures. The main risks ahead of cutover are in the spaces between what the code does and what the operator can observe when things go wrong.

## What Kills You

**Catastrophic failure mode: silent data loss — a snapshot containing irreplaceable data is deleted before it reaches external storage, and no one notices.**

Current distance: **3 independent failures required**. The system has three layers of protection:

1. **Unsent snapshot protection** (`plan.rs:246-268`) — snapshots newer than the oldest pin are protected from local retention when `send_enabled=true`. If no pins exist at all, *all* snapshots are protected.
2. **Pin-based deletion guard** in the planner — pinned snapshots are excluded from retention's delete list.
3. **Defense-in-depth pin check** in the executor (`executor.rs:460-484`) — even if the planner gets it wrong, the executor re-checks pins before deleting.

This is solid. The closest realistic scenario: if `send_enabled` is accidentally set to `false` for a subvolume that was previously `true`, unsent-protection vanishes and retention will immediately thin snapshots that haven't been sent. The config change is the only failure needed. **Severity: moderate** — this is a config-level footgun, not a code bug, but it's worth surfacing in `urd verify` or `urd status`.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | **4** | Retention logic is well-tested; edge cases handled. One gap: the `send_enabled` toggle scenario above. |
| Security | **4** | Path validation is thorough. TOCTOU gap in partial cleanup is acceptable given the trust model. |
| Architectural Excellence | **5** | Planner/executor separation is exemplary. `FileSystemState` trait makes the planner fully testable. Every module has one job. |
| Systems Design | **3** | Crash recovery is good for sends; weaker for local operations. Observability gaps (see findings). |
| Rust Idioms | **4** | Strong typing, proper error handling, good ownership model. Minor `RefCell` concern in mock (test-only, acceptable). |
| Code Quality | **4** | 128 tests with good coverage of the dangerous paths. Test design is behavior-oriented. Naming is clear. |

## Design Tensions

### 1. Flat operation list vs. dependency graph

The planner emits a flat `Vec<PlannedOperation>` with an implicit ordering invariant (create -> send -> delete). The executor relies on this ordering and uses `failed_creates` to handle cascading failures.

**Trade-off:** Simplicity (easy to iterate, easy to serialize for `urd plan`) vs. expressiveness (can't represent "delete external only after send succeeds" structurally).

**Verdict: right call for now.** The ordering invariant is documented in both `PLAN.md` and the code comments, and the executor handles the primary cascading failure (create fails -> skip send). The flat list is dead simple to test and display. A dependency graph would be warranted only if multi-drive sends need coordination (Phase 5), not before.

### 2. Pin files as source of truth vs. SQLite

The system uses pin files on the filesystem as the authoritative record of "last successful send" and SQLite only for history/observability. This means the critical chain state is co-located with the data it protects (in the snapshot directory) rather than in a separate database.

**Verdict: correct.** If SQLite were the source of truth, a corrupted or lost database would break the incremental chain system-wide. With pin files, each subvolume/drive pair is independently recoverable. The downside (no transactional update across multiple pins) is mitigated by the "last writer wins" property — the pin only advances forward.

### 3. Executor pin re-reading vs. trusting the plan

The executor re-reads pin files when checking crash recovery and pin protection, rather than trusting the plan's snapshot of state. This adds I/O during execution but protects against the plan being stale (e.g., if the bash script runs between planning and execution during parallel operation).

**Verdict: correct for Phase 3.5 parallel running.** Can be reconsidered post-cutover if performance matters, but the reads are trivially cheap.

### 4. Space-governed retention: planner proposes, executor re-checks

The planner proposes all deletions based on a point-in-time space check, but the executor re-checks free space after each external deletion and stops early. This means `urd plan` and `urd backup` can diverge.

**Verdict: correct trade-off, well-communicated.** The divergence is logged and surfaced in the backup output. The alternative (planner batches deletes until threshold) would require knowing snapshot sizes, which btrfs doesn't expose cheaply.

## Findings by Dimension

### Correctness

**Commendation: Unsent snapshot protection is the most important safety feature in the codebase.**

The logic in `plan.rs:246-268` that protects all snapshots newer than the oldest pin (or *all* snapshots when no pin exists) is the primary guard against silent data loss. It's well-tested with two dedicated tests (`unsent_snapshots_protected_from_retention` and `all_snapshots_protected_when_no_pin`). This is the kind of code that prevents the 3am page.

**Commendation: Dual-format snapshot name parsing is robust.**

`SnapshotName::parse()` cleanly handles both legacy `YYYYMMDD-shortname` and current `YYYYMMDD-HHMM-shortname` formats, with proper handling of compound names like `htpc-home`. The `let` chains for time parsing are clear. Ordering is by datetime, not string comparison — correct.

**Moderate — External retention uses local pins, not per-drive pins**

In `plan.rs:386-420`, `plan_external_retention` receives the `pinned` set from `plan()`, which is computed from *all* drives' pins (`plan.rs:106`). This means a snapshot pinned by drive A is also protected from deletion on drive B. This is conservative — it prevents deleting a snapshot on drive B that might be needed as a parent for drive A — but it also means external retention on a frequently-synced drive can be blocked by a stale pin on a rarely-connected offsite drive.

**Consequence:** If the offsite drive hasn't been connected in months, its stale pin protects old snapshots on the primary drive from retention, consuming space.

**Suggested fix:** Phase 4 could compute per-drive pin sets for external retention. For local retention, the current behavior (all-drives union) is correct because the local snapshot must exist for *any* drive's incremental chain.

**Minor — `SnapshotName::Ord` tie-breaking on `short_name`**

When two snapshots have the same datetime, ordering falls back to `short_name` string comparison (`types.rs:236`). This is fine for sorting, but could lead to surprising behavior if two different subvolumes' snapshots are accidentally mixed in the same list. Currently, all snapshot operations filter by short_name or subvolume_name, so this is not a real problem — just worth noting.

### Security

**Commendation: Path validation is thorough and positioned correctly.**

`validate_path_safe` (absolute, no `..`) and `validate_name_safe` (no `/`, `\`, `..`, `\0`) in `config.rs` run at config load time, before any path is used. This means every path that reaches `sudo btrfs` has been validated. The config file itself is trusted (owned by the user), which is the right trust model.

**Minor — `btrfs_path` from config is not validated**

`GeneralConfig::btrfs_path` defaults to `/usr/sbin/btrfs` but can be overridden in config. The value is passed to `Command::new("sudo").arg(&self.btrfs_path)`. Since the config file is user-owned (and readable only by them), this is not a privilege escalation vector — the user already has sudo for btrfs. But adding a `validate_path_safe` check would be consistent with the rest of the validation.

### Architectural Excellence

**Commendation: The planner/executor separation is the best thing about this codebase.**

The planner is a pure function of `(Config, NaiveDateTime, PlanFilters, &dyn FileSystemState) -> BackupPlan`. Every backup decision can be tested without touching the filesystem, without sudo, without btrfs. The `FileSystemState` trait abstraction is clean and the mock is complete. This is the design that makes the 67+ planner tests possible.

**Commendation: `FileSystemState` trait deserves specific praise.**

This is the right level of abstraction. It's not "mock everything" — it mocks the filesystem boundary specifically, letting the planner logic be tested in isolation. The mock is simple (HashMaps), the interface is small (6 methods), and the real implementation is thin. This is the testing seam that earns its keep.

### Systems Design

**Significant — No lock file for `urd plan` / `urd status` vs `urd backup` contention**

`urd backup` acquires an advisory lock (`backup.rs:184-205`), which prevents concurrent backup runs. But `urd status` and `urd verify` read the filesystem state without any coordination. If a backup is running while `urd status` reads snapshot directories, it may see inconsistent counts (a snapshot being created or deleted mid-read).

**Consequence:** Misleading status output, not data corruption. Low severity in practice since `urd status` is human-interactive and backups are typically seconds.

**Suggested approach:** Accept this as a known limitation. Document it. Don't add locking to read-only commands.

**Moderate — Crash between snapshot creation and pin update**

If the system crashes after `btrfs send|receive` completes but before `chain::write_pin_file`, the snapshot exists on the external drive but the pin still points to the old parent. The next run will either:
- Send the same snapshot again (detected by the crash recovery check in `executor.rs:330-379` — it sees the snapshot exists and is not pinned, deletes it as partial, and re-sends). This is **wasteful but safe**.

However, if the crash happens after a *successful* pin write but before the SQLite operation record, the history is incomplete but the system state is correct. This is the right failure mode — filesystem > database.

**Verdict:** The crash recovery is sound. The waste of re-sending is the cost of correctness, and it's acceptable.

**Moderate — No mechanism to detect bash/urd metric file stomping during parallel run**

During the parallel run phase, both bash and Urd write to the same `backup.prom` file (or different files — unclear from config). If they write to the same file, last writer wins. The bash script at 03:00 will overwrite Urd's 02:00 metrics, and Grafana will see the bash script's values.

**Suggested fix:** Verify that urd and bash write to different metrics files during parallel run, or accept that the 03:00 bash run's metrics overwrite the 02:00 urd run. Document this in the parallel run strategy.

**Minor — `urd init` doesn't verify btrfs binary exists**

`commands/init.rs` validates config paths and pin files but doesn't verify that `btrfs_path` is executable or that `sudo btrfs` works. A user could get through `urd init` and `urd plan` successfully, only to discover at `urd backup` time that sudo isn't configured.

### Rust Idioms

**Commendation: Error handling is textbook.**

`thiserror` for library errors in `error.rs`, `anyhow` at the CLI boundary in commands. No `unwrap()` in library code. SQLite failures are warned-and-continued, not fatal. The `UrdError` variants are specific enough to act on (you can tell `Btrfs` from `Chain` from `State`).

**Minor — `#[allow(dead_code)]` on `MockBtrfs` and several types**

The `#[allow(dead_code)]` on `MockBtrfs`, `MockBtrfsCall`, and some methods is because they're only used in tests but live in the main source. This is fine — the alternative (putting them in test modules) would require duplicating them across test files. The allows are justified.

**Minor — `tabled` crate is in dependencies but unused**

`Cargo.toml` lists `tabled = "0.17"` but `status.rs` uses a custom `print_table` function. Either use `tabled` or remove the dependency.

### Code Quality

**Commendation: Test design in `plan.rs` is excellent.**

The planner tests cover the dangerous scenarios: interval not elapsed, force override, incremental vs full send, missing parent on external, unsent protection, all-protected-when-no-pin, future-dated suppression. These are the scenarios that would cause data loss or confusion in production. The tests are behavior-focused (what operations appear in the plan) rather than implementation-focused.

**Moderate — Executor test coverage for crash recovery is thin**

The crash recovery path in `execute_send` (`executor.rs:330-379`) — detecting a partial snapshot at the destination and cleaning it up — is tested only via the `MockBtrfs.existing_subvolumes` mechanism. But the test `pin_on_success_writes_pin_file` doesn't exercise the partial-cleanup path. A dedicated test for "snapshot exists at dest but pin doesn't reference it -> delete and re-send" would be valuable. This is the crash recovery path — it should be tested as thoroughly as the happy path.

**Minor — Some string-typed fields could be enums**

`OperationRecord.operation` is a `String` ("snapshot", "send_incremental", "send_full", "delete") and `OperationRecord.result` is a `String` ("success", "failure", "skipped"). These could be enums for type safety within the Rust code, with `Display`/`FromStr` for SQLite serialization. Low priority — the strings are consistent and tested.

## The Simplicity Question

**What could be removed?**

- `tabled` dependency (unused)
- `count_retention()` in `retention.rs` (appears unused outside tests — check if Phase 4 needs it)

**What's earning its keep?**

- `FileSystemState` trait: enables 30+ planner tests without touching the filesystem
- `BtrfsOps` trait: enables 15+ executor tests without sudo
- `MockBtrfs`: simple, complete, used heavily
- Graduated retention with space pressure: handles real-world scenarios (full disk, hourly thinning)
- Pin-on-success in `PlannedOperation`: makes the send/pin dependency structural, not implicit

**What's proportional?**

The codebase is ~8,000 lines for a tool that replaces a 1,710-line bash script. The increase is justified: the Rust version has 128 tests, type-safe config parsing, proper error handling, crash recovery, defense-in-depth pin protection, and structured observability. The bash script had none of these.

## Priority Action Items

1. **Add executor test for crash recovery partial-cleanup path** — The code exists and is correct, but the test coverage for "snapshot exists at dest, not pinned, gets cleaned up before re-send" is missing. This is the path that handles power loss during send. (Moderate)

2. **Remove unused `tabled` dependency** — Dead dependency, clean it up. (Minor)

3. **Consider per-drive pin protection for external retention** — Currently all pins protect all drives. For Phase 4/post-cutover, compute per-drive pin sets for external retention to avoid stale offsite pins blocking primary drive cleanup. (Moderate, can defer)

4. **Verify metrics file strategy for parallel run** — Confirm bash and Urd don't stomp each other's `.prom` file, or document the expected behavior. (Moderate, before cutover)

5. **Add `send_enabled` change detection to `urd verify`** — Surface a warning if a subvolume has `send_enabled=false` but has existing pin files (suggesting it was previously enabled). This catches the config footgun. (Moderate)

6. **Add btrfs binary check to `urd init`** — Verify that `sudo btrfs --version` works before the user tries their first backup. (Minor)

## Open Questions

1. **Parallel run metrics:** Do bash and Urd write to the same `backup.prom` file? If so, which one does Grafana see after both have run?

2. **`count_retention`:** Is this function used anywhere outside tests? If it's for Phase 4 or later, it should stay. If not, it's dead code.

3. **Legacy pin file cleanup:** After cutover, should Urd clean up `.last-external-parent` (non-drive-specific) files, or leave them? They're read-only now but will accumulate as stale artifacts.

---

*Reviewed by: Claude Opus 4.6 (arch-adversary skill)*
