# Architectural Adversary Review: Urd Phase 4

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Phase 4 diff — 6 changes across 7 files (Cargo.toml, cli.rs, config.rs, init.rs, verify.rs, executor.rs)
**Base commit:** `542c8bf` (Phase 3.5)
**Tests:** 129 pass (128 prior + 1 new), 0 failures, clippy clean
**Prior review:** `docs/reviews/2026-03-22-arch-adversary-phase35.md`

---

## Executive Summary

Phase 4 is a clean, small changeset that closes every actionable item from the Phase 3.5 review. Each fix is proportional to its finding — no over-engineering, no scope creep. One finding warrants attention: the `sudo btrfs --version` check in `urd init` may prompt for a password interactively, which needs consideration for non-interactive contexts. Everything else is solid.

## What Kills You

Unchanged from the Phase 3.5 review: **silent data loss — a snapshot containing irreplaceable data is deleted before it reaches external storage.** The Phase 4 changes don't introduce new paths toward this failure mode. The `send_enabled` footgun warning in `urd verify` (new) *reduces* proximity by surfacing a previously invisible config risk.

## Scorecard

| Dimension | Score | Delta | Rationale |
|-----------|-------|-------|-----------|
| Correctness | **4** | = | No new logic paths; crash-recovery test fills a gap |
| Security | **4+** | +0.5 | `btrfs_path` validation closes the last unvalidated config path |
| Architectural Excellence | **5** | = | No structural changes; all fixes follow existing patterns |
| Systems Design | **3+** | +0.5 | `urd init` btrfs check improves fail-fast; `verify` footgun warning improves observability |
| Rust Idioms | **4** | = | No change |
| Code Quality | **4+** | +0.5 | New test covers previously untested crash-recovery path |

## Findings

### Significant — `sudo btrfs --version` in init may hang or prompt for password

`init.rs:77` runs `sudo btrfs --version` via `Command::new("sudo").arg(...).output()`. This call blocks. Two scenarios:

1. **sudo requires a password and no cached credential exists.** `sudo` will prompt on the terminal. If `urd init` runs in a non-TTY context (unlikely for init, but possible), `sudo` will fail with "no tty present." The current error handling catches this correctly — it prints `FAILED` and continues.

2. **sudo has `timestamp_timeout=0` or requires re-auth.** The user sees a password prompt in the middle of init output, after `"Checking sudo btrfs... "` has been printed (no newline). The password prompt appears on the same line, which looks broken.

**Consequence:** Bad UX, not data loss. The check is correct in intent — catching misconfigured sudoers before the first backup is valuable. But the presentation could confuse.

**Suggested fix:** Either flush stdout before the sudo call (already done implicitly by `print!` in a line-buffered terminal, but `print!` without newline may not flush in all contexts), or accept this as a known limitation and document that `urd init` requires an interactive terminal. The current behavior is acceptable — `urd init` is explicitly an interactive command (it already prompts for y/N on line 248).

**Verdict:** Not a blocker. The check catches the real failure mode (sudoers not configured) and the edge case (password prompt on same line) is cosmetic.

### Commendation — `send_enabled` footgun warning is exactly right

`verify.rs:27-49` addresses the most subtle finding from the Phase 3.5 review. When `send_enabled=false` but pin files exist, it surfaces a warning with actionable context: "Unsent snapshot protection is disabled — retention may delete snapshots not yet on all drives."

This is good for three reasons:
1. It catches the one-config-change-from-data-loss scenario.
2. It doesn't block anything — it warns and continues.
3. The message explains the *consequence* ("retention may delete unsent snapshots"), not just the symptom ("pin files exist").

### Commendation — Crash-recovery test is well-constructed

`executor.rs:1201-1247` tests the exact path that was identified as under-tested in the Phase 3.5 review: "snapshot exists at dest, not pinned, gets cleaned up before re-send." The test:

- Sets up the precondition correctly (existing subvolume in mock)
- Verifies the outcome (success)
- Verifies the *mechanism* (delete-then-send call sequence)

This is the right level of assertion — checking both what happened and how it happened, since the "how" is the crash-recovery contract.

### Minor — `btrfs_path` validation is consistent but slightly asymmetric

`config.rs:296-299` validates `btrfs_path` by converting the `String` to `&Path` inline: `std::path::Path::new(&self.general.btrfs_path)`. This works because `validate_path_safe` takes `&Path`. But `btrfs_path` is the only config field that's a `String` rather than `PathBuf` — all other validated paths are already `PathBuf`.

This is fine — `btrfs_path` is intentionally `String` because it's passed to `Command::new("sudo").arg(&self.btrfs_path)`, and `String` is more natural for that API. The asymmetry is a conscious trade-off. No action needed.

### Minor — `tabled` removal is clean

Removing `tabled` from `Cargo.toml` also cleaned up its transitive dependencies in `Cargo.lock` (papergrid, bytecount, fnv, unicode-width, proc-macro-error, proc-macro-error-attr2, tabled_derive, and an older syn 1.x). The dependency tree is meaningfully smaller. No code changes were needed because `tabled` was never imported anywhere.

### Minor — CLI help text improvements are adequate

The help strings for `Backup`, `Status`, `Verify`, and `Init` are improved. They now describe what the commands *do* rather than being generic. `"Show system status"` → `"Show snapshot counts, drive status, chain health"` is a good example — a user reading `--help` now knows what they'll see.

## The Simplicity Question

**What was added:** ~100 lines of code and tests. No new abstractions, no new modules, no new types.

**What was removed:** `tabled` dependency and ~100 lines of Cargo.lock.

**Net complexity change:** Approximately zero. The changes follow established patterns — `std::process::Command` in init (already used elsewhere in `btrfs.rs`), `chain::find_pinned_snapshots` in verify (already used in status.rs), `MockBtrfs` setup in the new test (identical pattern to existing tests).

This is how a polish phase should look: small, proportional fixes that close identified gaps without introducing new machinery.

## Priority Action Items

1. **Consider flushing stdout before sudo call in init** — `std::io::stdout().flush()` before `Command::new("sudo")` on line 77. Prevents the password prompt from appearing on the same line as "Checking sudo btrfs... ". Low priority, cosmetic. *(Minor)*

2. **No other action items.** The Phase 4 changes are clean and complete.

## Open Questions

1. **Interactive context for `urd init`:** Is there any scenario where `urd init` runs non-interactively? If so, the sudo check and the y/N cleanup prompt both need consideration. Currently appears to be interactive-only, which makes both fine.

2. **Phase 3.5 open questions still open:**
   - Parallel run metrics stomping: Do bash and Urd write to the same `backup.prom`? (Operational, not code)
   - `count_retention()`: Still appears unused outside tests. Confirm if Phase 5 needs it.
   - Legacy pin file cleanup: Deferred, per plan. No action needed yet.

---

*Reviewed by: Claude Opus 4.6 (arch-adversary skill)*
*Phase 3.5 review findings addressed: 6/6 (tabled removal, btrfs_path validation, btrfs check in init, crash-recovery test, send_enabled warning, CLI polish)*
