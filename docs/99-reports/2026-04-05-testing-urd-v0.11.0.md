# Urd v0.11.0 Comprehensive Test Report

> **Date:** 2026-04-05
> **Version:** urd 0.11.0 (confirmed via `urd --version`)
> **Context:** First nightly run after v0.11.0 deployment (run #29, 2026-04-05T04:01:11).
> Phase E features: compressed sends (013), external-only runtime (018), context-aware
> suggestions (020), skip unchanged subvolumes (014), emergency space response (016),
> sentinel fixes (021).
>
> **Method:** All commands run from development build (`cargo run --`), interpreting
> JSON output. No TTY formatting visible — all observations are from structured data.

---

## Phase 0: Post-Nightly Baseline

### T0.1 — Version confirmed

```
urd --version → urd 0.11.0
```

**Result:** PASS.

### T0.2 — Status baseline

All 9 subvolumes report `PROTECTED`. WD-18TB connected (4.2TB free). WD-18TB1 absent.
2TB-backup absent.

| Subvolume | Status | Health | Promise Level | Local Snaps | External (WD-18TB) |
|-----------|--------|--------|---------------|-------------|-------------------|
| subvol3-opptak | PROTECTED | degraded | fortified | 31 | 7 (1d ago) |
| htpc-home | PROTECTED | healthy | fortified | 9 | 8 (7h ago) |
| subvol2-pics | PROTECTED | healthy | fortified | 17 | 9 (1d ago) |
| subvol1-docs | PROTECTED | healthy | sheltered | 31 | 9 (1d ago) |
| subvol7-containers | PROTECTED | healthy | sheltered | 32 | 9 (1d ago) |
| subvol5-music | PROTECTED | degraded | sheltered | 16 | 9 (1d ago) |
| subvol4-multimedia | PROTECTED | healthy | recorded | 6 | — |
| subvol6-tmp | PROTECTED | healthy | (custom) | 13 | — |
| htpc-root | PROTECTED | degraded | (custom) | 2 | 5 (7h ago) |

**Degraded reasons:**
- subvol3-opptak: WD-18TB1 away for 8 days
- subvol5-music: WD-18TB1 away for 8 days
- htpc-root: WD-18TB1 away for 13 days, 2TB-backup away for 13 days

**Chain health:** All 7 send-enabled subvolumes show "Incremental" — all chains intact.
23 total pins.

**Result:** PASS. System healthy after first v0.11.0 nightly.

### T0.3 — History review

Last 5 runs:

| Run | Time | Mode | Result | Duration |
|-----|------|------|--------|----------|
| 29 | 2026-04-05 04:01 | full | success | 9m 31s |
| 28 | 2026-04-04 04:00 | full | success | 31s |
| 27 | 2026-04-03 20:25 | full | success | 12s |
| 26 | 2026-04-03 04:00 | full | success | 2s |
| 25 | 2026-04-02 22:20 | full | success | 23s |

**Observation:** Run #29 (9m 31s) was dramatically longer than run #28 (31s). This is
because run #29 was the first run on v0.11.0 with compressed sends and new chain-break
behavior. The log reveals two full sends: htpc-home (27GB, 350s) and htpc-root (22GB, 215s).
Both were chain-break full sends that v0.11.0 allowed because drive identity was verified.

**Result:** PASS. Nightly timer firing consistently at ~04:00.

### T0.4 — Nightly run #29 detailed analysis

From journalctl:

1. **Compressed data pass-through detected** — `btrfs send: compressed data pass-through available`.
   UPI 013 feature working.

2. **Chain-break full sends auto-proceeded:**
   - htpc-home → WD-18TB: full send, 27.1GB, 350s. "Chain-break full send for htpc-home to WD-18TB: proceeding (drive identity verified)"
   - htpc-root → WD-18TB: full send, 22.0GB, 215s. Same pattern.
   - Both used `--compressed-data` flag. ✓

3. **Skip-unchanged working:** subvol2-pics, subvol1-docs, subvol7-containers, subvol5-music
   all show "already on WD-18TB" in skipped reasons — no redundant sends.

4. **Post-delete sync issue:** Two `btrfs subvolume sync` failures with "sudo: a terminal
   is required to read the password". The sync command for post-delete space reclamation
   requires sudo but the sudoers config doesn't cover `btrfs subvolume sync`. The warning
   message "space check may be pessimistic" is accurate — sync failure doesn't block
   operations, just means free-space readings may lag.

5. **Space-aware deletion skipping:** Executor correctly stopped further deletions once
   free space exceeded thresholds: "Free space on WD-18TB is now 4.2TB (>= 500.0GB),
   stopping further deletions" — avoiding unnecessary retention cleanup.

6. **Sentinel notification deferral:** "Sentinel is running — deferring notification dispatch".
   Backup correctly delegates notification to the sentinel daemon. ✓

**Result:** PASS with one issue (sync sudo).

---

## Phase 1: Plan and Dry-Run Testing

### T1.1 — Manual plan

`urd plan` shows: 4 snapshots, 6 sends, 25 deletions, 19 skipped.
Estimated total: ~1.0GB.

Key observations:
- All sends are incremental to WD-18TB. ✓
- **Skip-unchanged (UPI 014):** subvol2-pics, subvol1-docs, subvol5-music, subvol4-multimedia,
  subvol6-tmp all show `"unchanged — no changes since last snapshot"`. Feature working correctly.
- Retention thinning: graduated daily/weekly deletions for subvol6-tmp and others.
- htpc-root plans a send with `estimated_bytes: 72407457` (~69MB incremental).

**Result:** PASS. Plan is coherent and all new features visible.

### T1.2 — Auto plan

`urd plan --auto` shows: 0 snapshots, 6 sends, 25 deletions, 23 skipped.

Difference from manual: No new snapshots planned (interval not elapsed — next in ~16h33m).
But sends are still planned because unsent snapshots from the nightly exist.

**Observation:** The auto plan correctly gates snapshot creation on intervals but still
plans sends for snapshots that haven't been sent yet. This is correct behavior — the
nightly created snapshots but many subvolumes had their sends skipped (already on drive).

**Result:** PASS.

### T1.3 — Backup dry-run

`urd backup --dry-run` shows: 4 snapshots, 6 sends, 6 deletions, 19 skipped.

**Plan vs dry-run divergence (carried from v0.8 F0.4):** Plan shows 25 deletions,
dry-run shows only 6. The 19-deletion difference is retention thinning that plan includes
but backup dry-run omits. This divergence still exists from v0.8.

**UX note:** For a user trying to understand "what will happen if I run backup?", the
dry-run answer differs from the plan answer. Both are called "plan" views but show
different scopes of work.

**Result:** PASS (functional). UX divergence noted.

### T1.4 — Skip categories in JSON

The skip reasons include rich categories:
- `"unchanged"` — UPI 014 skip-unchanged working
- `"drive_not_mounted"` — clear
- `"local_only"` — replaces the misleading `[OFF] Disabled` from v0.8 (F0.3)
- `"interval_not_elapsed"` — with countdown ("next in ~16h33m")
- `"other"` — "20260403-2025-music already on WD-18TB" (already-sent skip)

**Improvement from v0.8:** The `local_only` category resolves F0.3 from v0.8 testing.
The old `[OFF] Disabled` label was misleading for subvolumes that are actively snapshotted
locally but not sent anywhere. Now clearly labeled as `"send disabled"` with `local_only`
category.

**Remaining issue:** The `"send disabled"` reason text could be more descriptive —
`"local-only — no external drives configured"` would better match the vocabulary design.

**Result:** PASS. F0.3 from v0.8 resolved in JSON output.

---

## Phase 2: Feature Verification (Phase E)

### T2.1 — Compressed sends (UPI 013)

Nightly log confirms:
- Probe: `btrfs send: compressed data pass-through available`
- Usage: `btrfs send --compressed-data` for both full sends

htpc-home full send: 27.1GB in 350s ≈ 77MB/s.
htpc-root full send: 22.0GB in 215s ≈ 102MB/s.

**Result:** PASS. Compressed sends operational.

### T2.2 — Skip unchanged subvolumes (UPI 014)

Plan output shows 5 subvolumes skipped with `"unchanged"` category:
- subvol2-pics: "unchanged — no changes since last snapshot (7h26m ago)"
- subvol1-docs: "unchanged — no changes since last snapshot (7h26m ago)"
- subvol5-music: "unchanged — no changes since last snapshot (1d ago)"
- subvol4-multimedia: "unchanged — no changes since last snapshot (1d ago)"
- subvol6-tmp: "unchanged — no changes since last snapshot (7h26m ago)"

This is generation-based detection. The "(Xh ago)" suffix tells the user when the
last snapshot was taken — useful context.

**Result:** PASS. Skip-unchanged operational and informative.

### T2.3 — Emergency space response (UPI 016)

`urd emergency` exists as a command. Help shows: "Guided emergency space recovery".
No `--dry-run` flag — this is an interactive guided command.

Cannot fully test without TTY (interactive prompts), but the command exists and is
reachable. The nightly log shows no emergency triggers, which is correct — space is
healthy.

**Result:** PARTIAL (command exists, cannot exercise interactive flow).

### T2.4 — External-only runtime (UPI 018)

Config confirms `htpc-root` has `local_snapshots = false`. Status shows:
- `"external_only": true` in htpc-root assessment
- `"retention_summary": "none (transient)"`
- `"local_snapshot_count": 2`

**CRITICAL FINDING F2.4:** htpc-root has `local_snapshots = false` in config but
**2 local snapshots exist** (`20260404-0400-htpc-root`, `20260405-0401-htpc-root`).
The nightly log confirms: "Creating snapshot: / -> ~/.snapshots/htpc-root/20260405-0401-htpc-root".

**Urd is still creating local htpc-root snapshots despite `local_snapshots = false`.**

The snapshots are on the NVMe root drive (118GB, 79% used, 26GB free). Each htpc-root
snapshot is a full root filesystem snapshot. This is the exact pattern that caused
the catastrophic storage failure — snapshot accumulation on a space-constrained system drive.

Currently only 2 snapshots exist (retention thinning appears to be working), but:
1. The config says `local_snapshots = false` — the user's explicit intent is zero local snapshots
2. The snapshot_root is `~/.snapshots` which lives on the NVMe
3. The retention_summary says "none (transient)" but snapshots are being created and kept
4. With 26GB free and each root snapshot potentially large, this is a live risk

**Root cause hypothesis:** The executor creates snapshots as a prerequisite for sends.
Even with `local_snapshots = false`, htpc-root needs a local snapshot to send to
WD-18TB. The snapshots are created for the send pipeline, and "transient" retention
keeps the minimum needed for chain continuity. But the user's expectation from
`local_snapshots = false` is **zero persistent local copies** — "create, send, delete"
should be the pattern.

**Risk assessment:**
- 2 snapshots × ~20GB = ~40GB potential on 118GB drive with 26GB free
- If retention fails or gets behind, snapshots accumulate → catastrophic failure
- The current 2-snapshot state may be correct (pin + latest), but the config contract
  says "false" and the user expects zero

**Result:** FAIL. Config contract violated. See Findings section.

### T2.5 — Context-aware suggestions (UPI 020)

Status output includes an `"advice"` array:
```json
{
  "subvolume": "subvol3-opptak",
  "issue": "degraded — WD-18TB1 away",
  "reason": "Consider connecting WD-18TB1"
}
```

Three advisories, all for degraded subvolumes suggesting WD-18TB1 connection.
Clear, actionable, relevant.

**Result:** PASS. Context-aware suggestions present and useful.

### T2.6 — Sentinel health (UPI 021)

Sentinel running since 17h21m. PID 929060. Tick interval: 900s (15m).
Circuit breaker: closed, 0 failures. Visual state icon: "warning" (due to degraded subvolumes).

Promise states match status output exactly. 6 healthy, 3 degraded, 0 blocked.

**Sentinel log anomaly:** Two entries from 2026-04-02:
- "Drive anomaly: all 0 chains broke on 2TB-backup simultaneously"
- "Drive anomaly: all 0 chains broke on WD-18TB simultaneously"

Both say "all 0 chains broke" — this is a spurious warning. When a drive has 0 chains,
reporting that "all 0 broke simultaneously" is vacuously true but misleading. Should be
suppressed when chain count is 0.

**Result:** PASS (functional). Spurious "0 chains broke" warning noted.

---

## Phase 3: Drive and Infrastructure Checks

### T3.1 — Drive status

```json
WD-18TB:    connected, token verified, 4.2TB free, role: primary
WD-18TB1:   absent, token unknown, role: offsite
2TB-backup: absent, token recorded (last seen 2026-04-02), role: test
```

Token states are differentiated: "verified" (seen and confirmed on mounted drive),
"recorded" (in SQLite but drive not mounted), "unknown" (never tokened).

**Result:** PASS. Drive state clearly communicated.

### T3.2 — Doctor thorough

35 OK, 14 warnings, 0 failures. All WD-18TB threads pass all 5 checks (pin-file,
pin-exists-local, pin-exists-drive, orphans, stale-pin). All 14 warnings are "Drive
not mounted — skipping" for WD-18TB1 and 2TB-backup.

No preflight warnings. No issues. Sentinel confirmed running.

**Improvement from v0.8:** No more "UUID contradiction" warning (WD-18TB UUID issue).
No htpc-root chain break issues on WD-18TB.

**Result:** PASS.

### T3.3 — Verify thread integrity

Same as doctor verify section: 35 OK, 14 warnings, 0 failures. All send-enabled
subvolumes have intact chains on WD-18TB.

**Result:** PASS.

### T3.4 — Heartbeat

```json
{
  "schema_version": 2,
  "timestamp": "2026-04-05T04:10:42",
  "stale_after": "2026-04-07T04:10:42",
  "run_result": "success",
  "run_id": 29,
  "notifications_dispatched": true
}
```

All subvolumes show `backup_success: true`. htpc-home and htpc-root show
`send_completed: true` (the two full sends). Others show `send_completed: false`
(skipped — already sent or unchanged).

Stale threshold: 48h from last run. Appropriate for a daily schedule.

**Result:** PASS. Heartbeat schema clean and informative.

### T3.5 — File restore (urd get)

```
urd get /etc/hostname --at yesterday
```

Output:
```json
{
  "subvolume": "htpc-root",
  "snapshot": "20260404-0400-htpc-root",
  "snapshot_date": "2026-04-04 04:00",
  "file_path": "etc/hostname",
  "file_size": 12
}
fedora-htpc
```

**Observation:** The JSON metadata is printed followed immediately by the file content
(`fedora-htpc`). This is a mixed-mode output — structured metadata concatenated with
raw file content. For piping (`urd get ... | cat`), the JSON preamble would corrupt
the restored file. The `-o` flag writes to a file instead, which avoids this issue.

**UX finding F3.5:** `urd get` without `-o` mixes JSON metadata with file content on
stdout. This makes the default restore path unusable for piping. Either:
- (a) Print metadata to stderr, content to stdout (pipe-safe default)
- (b) Only print metadata when `--verbose` is set
- (c) Always require `-o` for output (break pipe workflow entirely)

**Result:** PASS (functional). UX issue with mixed stdout.

### T3.6 — Retention preview

`urd retention-preview --all` shows graduated policies for all subvolumes.
htpc-root shows `"transient"` with empty recovery windows — consistent with config.

**Observation:** subvol3-opptak shows calibrated disk usage estimate of 315TB total
for 92 snapshots at 3.4TB per snapshot. subvol5-music shows 102TB total for 92 snapshots
at 1.1TB per snapshot. These numbers are per-snapshot full sizes from calibration, not
deltas — the actual disk usage with CoW deduplication will be far lower. The presentation
doesn't clarify this distinction.

**UX finding F3.6:** Retention preview shows raw calibrated sizes that massively overstate
actual disk usage due to BTRFS CoW. "315TB estimated for subvol3-opptak" is alarming but
misleading. Should note these are pre-dedup estimates or use delta-based estimates if available.

**Result:** PASS (functional). Size estimates misleading.

### T3.7 — Lock file

Lock file from run #29: `{"pid":2115074,"started":"2026-04-05T04:01:09","trigger":"auto"}`.
Trigger correctly shows "auto" for the nightly timer run.

**Result:** PASS.

---

## Phase 4: UX Assessment

### Overall JSON-first experience

Every command outputs structured JSON. This is excellent for programmatic consumption
(monitoring, dashboards, scripting) but means the CLI has **no human-readable presentation
layer** for interactive use. The v0.8 testing showed human-formatted tables and status
lines — the current output is pure JSON.

**Key question:** Has the presentation layer (voice.rs) been bypassed? Or does the
current binary always output JSON? If this is intentional (machine-first output with
a future TUI layer on top), it should be documented. If it's a regression, the mythic
voice that was part of Urd's identity has been lost.

**Finding F4.1 — JSON-only output.** All tested commands produce raw JSON:
- `urd status` — JSON object (no table, no promise-state summary)
- `urd plan` — JSON operations array (no briefing, no human summary)
- `urd doctor` — JSON checks (no severity summary)
- `urd history` — JSON runs (no formatted table)
- `urd verify` — JSON checks
- `urd drives` — JSON
- `urd get` — JSON metadata + raw content

This may be because we're running `cargo run --` (debug build) rather than the installed
binary, or because the output format changed. But in v0.8 testing, the same `cargo run --`
approach showed human-formatted output.

**Impact:** Without a presentation layer, the user cannot quickly answer "is my data
safe?" by glancing at status output. They must parse JSON mentally. The mythic voice,
protection summaries, and status tables that define Urd's character are absent.

### What works well

1. **Skip-unchanged messaging** — "unchanged — no changes since last snapshot (7h26m ago)"
   is informative and saves work.
2. **Context-aware advice** — Targeted suggestions in status ("Consider connecting WD-18TB1")
   are actionable.
3. **Drive token states** — "verified" / "recorded" / "unknown" differentiation is clear.
4. **Chain health reporting** — All-incremental status is easy to verify.
5. **Space-aware behavior** — Executor stops deletions when space is sufficient, saving
   unnecessary operations.
6. **Sentinel notification deferral** — Backup correctly hands off to sentinel.

### What needs attention

1. **htpc-root local snapshots** (CRITICAL) — Config says false, snapshots exist
2. **JSON-only output** — No human-readable layer visible
3. **Mixed stdout in `urd get`** — JSON + content concatenated
4. **Retention preview sizes** — Pre-dedup estimates are alarming
5. **Post-delete sync requires sudo** — `btrfs subvolume sync` not in sudoers
6. **Sentinel "0 chains broke" warning** — Vacuously true, should be suppressed

---

## Phase 5: htpc-root Deep Dive

This is the most critical finding. The user has been explicit that htpc-root local
snapshot accumulation caused a catastrophic storage failure.

### Current state

- Config: `local_snapshots = false`
- Actual local snapshots: 2 (`20260404-0400`, `20260405-0401`)
- Snapshot root: `~/.snapshots/htpc-root/` on NVMe (118GB, 79% used, 26GB free)
- Pin files: 3 (WD-18TB, WD-18TB1, 2TB-backup)
- Retention summary: "none (transient)"
- Status: `external_only: true`

### Nightly behavior

Run #29 log shows:
1. Created snapshot: `/ -> ~/.snapshots/htpc-root/20260405-0401-htpc-root`
2. Chain-break full send to WD-18TB: 22GB, 215s
3. Pin updated to `20260404-0400-htpc-root`

The snapshot was created, sent, and the previous snapshot is retained (pinned for
chain continuity). So htpc-root always has 2 local snapshots: the current pin parent
and the latest.

### Risk calculation

| Scenario | Snapshots | Est. disk use | NVMe free after |
|----------|-----------|---------------|-----------------|
| Normal (2 snaps) | 2 | ~10-20GB shared | 6-16GB |
| Missed cleanup (3 snaps) | 3 | ~15-30GB shared | 0-11GB |
| Chain break + retry (4 snaps) | 4 | ~20-40GB shared | 0-6GB |

The NVMe has 26GB free. With CoW sharing between snapshots, 2 snapshots may only
consume a few GB of exclusive data. But a chain break or failed cleanup could push
this over the edge.

### What `local_snapshots = false` should mean

The user's intent: htpc-root lives on external drives. Local copies are temporary —
create, send, delete. The "transient" retention and `external_only: true` flags
suggest the system understands this intent, but the execution still accumulates
snapshots.

**Recommendation:** After a successful send, the previous local snapshot (the old
pin parent) should be immediately deleted. Only the current pin parent should survive.
If `local_snapshots = false`, the lifecycle should be: create → send → update pin →
delete old pin parent → result: exactly 1 local snapshot at any time.

---

## Summary

| Test | Status | Notes |
|------|--------|-------|
| T0.1 | PASS | v0.11.0 confirmed |
| T0.2 | PASS | Baseline: 9 PROTECTED, 3 degraded, all chains intact |
| T0.3 | PASS | History clean, nightly consistent |
| T0.4 | PASS* | Nightly successful, compressed sends working, sync sudo issue |
| T1.1 | PASS | Plan coherent, all Phase E features visible |
| T1.2 | PASS | Auto plan correctly gates snapshots |
| T1.3 | PASS | Dry-run functional, plan divergence persists from v0.8 |
| T1.4 | PASS | Skip categories improved from v0.8 (F0.3 resolved) |
| T2.1 | PASS | Compressed sends operational |
| T2.2 | PASS | Skip-unchanged operational and informative |
| T2.3 | PARTIAL | Emergency command exists, interactive flow untestable |
| T2.4 | **FAIL** | htpc-root creating local snapshots despite `local_snapshots = false` |
| T2.5 | PASS | Context-aware suggestions present and useful |
| T2.6 | PASS | Sentinel healthy, spurious "0 chains broke" warning |
| T3.1 | PASS | Drive states clear |
| T3.2 | PASS | Doctor thorough clean |
| T3.3 | PASS | All threads intact |
| T3.4 | PASS | Heartbeat healthy |
| T3.5 | PASS* | Restore works, mixed stdout issue |
| T3.6 | PASS* | Retention preview functional, sizes misleading |
| T3.7 | PASS | Lock trigger correct |

## Findings

### Severity: Critical

**F2.4 — htpc-root local snapshots created despite `local_snapshots = false`.** Config
explicitly sets `local_snapshots = false` but nightly run #29 created a local snapshot
on the 118GB NVMe (26GB free). Two local htpc-root snapshots currently exist. This is
the exact pattern that caused the catastrophic storage failure. The "transient" retention
keeps snapshots for chain continuity, but the user's expectation is zero persistent local
copies. Either:
1. The send pipeline must delete the old pin parent immediately after successful send
2. Or `local_snapshots = false` needs redefinition — "false means at-most-1, not zero"
   — and this needs to be communicated to the user

### Severity: Moderate

**F4.1 — JSON-only output.** All commands produce raw JSON with no human-readable
presentation layer. The mythic voice and status tables from Urd's design are absent.
Users cannot quickly assess "is my data safe?" from the raw output. This may be a
regression or an intentional machine-first approach — either way, the invoked-norn
experience described in the design vision is not present.

**F3.5 — Mixed stdout in `urd get`.** JSON metadata concatenated with raw file content
on stdout makes the default restore path unpipeable. Metadata should go to stderr or
be suppressed without `--verbose`.

**F0.4 (carried) — Plan vs dry-run divergence.** `urd plan` shows 25 deletions while
`urd backup --dry-run` shows 6. Different scopes of "what will happen" with no
indication to the user.

### Severity: Low

**F3.6 — Retention preview sizes misleading.** Shows pre-deduplication full-snapshot
sizes (315TB for opptak, 102TB for music) that massively overstate actual disk usage.
Should clarify these are worst-case estimates.

**F2.6a — Sentinel "0 chains broke" warning.** "Drive anomaly: all 0 chains broke on
2TB-backup simultaneously" — vacuously true, should be suppressed when chain count is 0.

**F0.4a — Post-delete sync requires sudo.** `btrfs subvolume sync` fails in nightly
context because it's not in sudoers. Logged as warning, doesn't block operations, but
means space readings may be pessimistic after deletions.

### Regression check vs v0.8

| v0.8 Finding | v0.11 Status |
|-------------|-------------|
| F0.3 — `[OFF] Disabled` misleading | **RESOLVED** — now `local_only` category |
| F0.0 — UUID contradiction for cloned drives | Not observed in current doctor output |
| F1.1 — Silent drive reconnection | Not testable (no drive events during test) |
| F2.3 — Cloned drive identity crisis | Not testable (WD-18TB1 not available) |
| T3.3 — assess() scoping bug | **Appears improved** — htpc-home and subvol2-pics show healthy despite 2TB-backup absent (they have explicit `drives` config excluding 2TB-backup). However, htpc-root still degrades for WD-18TB1 and 2TB-backup (no explicit drives scoping — sends to all) |
| T0.4 — Plan vs dry-run divergence | **Still present** |
