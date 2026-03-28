# Design: Hardware Swap Defenses — Drive Identity, Chain Break Detection, and Full-Send Gates

> **TL;DR:** Three layered defenses against the hardware swap blind spot: (1) drive
> session tokens as a second identity factor in `drives.rs`, (2) simultaneous chain
> break detection in `sentinel.rs` with notifications, and (3) a full-send confirmation
> gate in the planner/executor for unexpected chain-break-driven full sends. Each layer
> is independently valuable and independently deployable.

**Date:** 2026-03-28
**Status:** proposed
**Depends on:** Sentinel Sessions 1-2 (complete), chain.rs (complete), drives.rs (complete)
**Inputs:**
- [Hardware swap test journal](../98-journals/2026-03-28-sentinel-hardware-swap-test.md)
- [Brainstorm: hardware swap solutions](2026-03-28-brainstorm-hardware-swap-solutions.md)
- [Visual feedback model design](2026-03-28-design-visual-feedback-model.md)

---

## Problem

After swapping a cloned BTRFS drive (same UUID, different content), Urd:

1. **Did not detect the swap.** UUID matched because both drives were clones.
2. **Did not warn about broken chains.** All pin files were missing on the swapped
   drive, but this was reported only as a neutral table cell.
3. **Planned four full sends silently.** Two were blocked by space guards; two would
   have proceeded. A full send on a chain-break is qualitatively different from a
   first send, and should be treated differently.

Space guards prevented catastrophe, but only by accident (the drives happened to be
different sizes). The root causes — identity, detection, and gating — were unaddressed.

---

## Design: Three Layers

### Layer 1: Drive Session Token (`drives.rs`)

**What:** On first successful send to a drive, Urd writes a random token file to the
drive root. On subsequent mounts, `drives.rs` reads the token and verifies it against
a stored value.

**Why this over alternatives:**
- BTRFS generation numbers: only detect *backward* jumps reliably, not clone siblings
  that diverged forward. Would need `sudo btrfs subvolume show` (heavier).
- LUKS UUID: only works for encrypted drives. Not all users encrypt externals.
- Snapshot-set fingerprint: fragile if user manually adds/removes snapshots.
- Hardware serial: unreliable across USB enclosures.
- Session token: simple, deterministic, Urd controls it, diverges immediately on clone.

#### On-disk contract

**Token file:** `.urd-drive-token` in the drive's snapshot root directory.

```
# Urd drive session token — do not edit
# Written: 2026-03-28T14:30:00
# Drive label: WD-18TB1
token=a3f8c2d1-7e4b-4a2f-9c8d-1234567890ab
```

Human-readable with comments so a user who finds the file understands what it is.
The `token=` line is the only parsed line. Format is deliberately simple — no JSON,
no TOML, just `key=value` for maximum robustness.

**Gate: ADR needed?** This writes a new file to external drives — a new on-disk
contract per ADR-105. However, it's a *new* file that Urd controls, not a change
to an existing contract. The file is non-critical (missing token = first-time
setup, not an error). **Recommendation: document in ADR-105 addendum, not a new ADR.**

#### Stored reference

The token reference is stored in SQLite (`state.rs`):

```sql
CREATE TABLE drive_tokens (
    drive_label TEXT PRIMARY KEY,
    token TEXT NOT NULL,
    first_seen TEXT NOT NULL,       -- ISO 8601 timestamp
    last_verified TEXT NOT NULL     -- ISO 8601 timestamp
);
```

**Why SQLite, not config?** The token is operational state, not user intent. It's
written by Urd, verified by Urd, and the user never needs to see or edit it. Config
is for user intent (ADR-111). SQLite is for "what has been" (ADR-102).

**Why not the sentinel state file?** The state file is ephemeral — rewritten every
tick, lost on restart. Tokens must survive restarts.

#### Module changes

**`drives.rs` — new `DriveAvailability` variant + token functions:**

```rust
pub enum DriveAvailability {
    Available,
    NotMounted,
    UuidMismatch { expected: String, found: String },
    UuidCheckFailed(String),
    /// Drive is mounted and UUID matches, but the session token doesn't match
    /// the stored reference. The physical media may have changed.
    TokenMismatch {
        expected: String,
        found: String,
    },
    /// Drive is mounted and UUID matches, but no token file exists on the drive.
    /// This is normal for drives that haven't had their first Urd send yet.
    TokenMissing,
}
```

New functions:

```rust
/// Read the drive session token from the drive's snapshot root.
pub fn read_drive_token(drive: &DriveConfig) -> Result<Option<String>>

/// Write a drive session token to the drive's snapshot root.
/// Atomic write (temp + rename). Called by the executor after first successful send.
pub fn write_drive_token(drive: &DriveConfig, token: &str) -> Result<()>

/// Generate a new random drive session token.
pub fn generate_drive_token() -> String
```

**`drive_availability()` changes:**

After UUID passes, check the token:
1. Read token from drive filesystem.
2. Look up stored token in SQLite.
3. If no stored token → `TokenMissing` (benign: first use or pre-token drive).
4. If stored token exists but drive has no token file → `TokenMissing` (suspicious
   if sends have happened before — the drive may have been replaced).
5. If both exist and match → `Available`.
6. If both exist and don't match → `TokenMismatch`.

**Important:** `TokenMissing` is not a blocking state. It's informational. A drive
with `TokenMissing` is still `Available` for sends — the token will be written on
the next successful send. This preserves backward compatibility: existing drives
without tokens continue to work. The first send after this feature ships writes
the token, and all subsequent mounts verify it.

`TokenMismatch` is a blocking state for sends but not for reads (the user can
still browse snapshots on the drive). The planner skips sends to `TokenMismatch`
drives and surfaces an advisory.

**`state.rs` — new table + methods:**

```rust
/// Store a drive session token.
pub fn store_drive_token(&self, label: &str, token: &str, now: &str) -> Result<()>

/// Look up a stored drive session token.
pub fn get_drive_token(&self, label: &str) -> Result<Option<String>>

/// Update the last_verified timestamp for a token.
pub fn touch_drive_token(&self, label: &str, now: &str) -> Result<()>
```

**`executor.rs` — token write on first send:**

After a successful send, the executor checks:
1. Does a token file exist on the drive? If yes, done.
2. If no, generate a token, write it to the drive, and store it in SQLite.

This happens once per drive lifetime (until the drive is replaced). The executor
already writes pin files after successful sends — this is the same pattern.

**Architectural invariant compliance:**
- ADR-102 (filesystem truth, SQLite history): The token on the drive is truth;
  SQLite stores the reference for verification. If SQLite loses the token, the
  next send re-writes it (self-healing).
- ADR-107 (fail-open backups): `TokenMissing` allows sends. Only `TokenMismatch`
  blocks, and that's because sending to the wrong drive is dangerous.
- ADR-108 (pure functions): `drive_availability()` is I/O (reads filesystem), which
  is already the case. No pure module is affected.

#### Testing

| Test | What |
|------|------|
| Token write + read roundtrip | Write token, read it back, verify match |
| Token missing on fresh drive | No token file → `TokenMissing`, drive still usable |
| Token mismatch detection | Write token A to drive, store token B in SQLite → `TokenMismatch` |
| Token survives remount | Write, "unmount" (just re-check), verify still matches |
| SQLite token CRUD | Store, get, touch operations |
| Executor writes token on first send | MockBtrfs send succeeds, verify token written |
| Executor skips token write if exists | Token already on drive, verify no write |
| `drive_availability` full flow | UUID match + token match → Available |

~10 tests. Uses tempdir for token files, in-memory SQLite for state.

---

### Layer 2: Simultaneous Chain Break Detection (`sentinel.rs`)

**What:** The sentinel detects when all chains on a drive break simultaneously and
emits a `DriveAnomalyDetected` notification. This is the strongest heuristic signal
for a drive swap even without token verification.

**Why:** The journal identified four signals that correlate with a swap: space delta,
snapshot set change, all chains breaking, and no unmount event. Of these, "all chains
breaking simultaneously" is the most reliable — it requires zero additional I/O (the
chain health is already computed in assessments) and has virtually no false positives
(manual deletion of all pin files simultaneously is unlikely).

#### Module changes

**`sentinel.rs` — chain health tracking in state:**

```rust
pub struct SentinelState {
    pub mounted_drives: BTreeSet<String>,
    pub last_promise_states: Vec<PromiseSnapshot>,
    pub has_initial_assessment: bool,
    pub circuit_breaker: CircuitBreaker,
    // NEW: chain health per (subvolume, drive) from the last assessment.
    // Used to detect simultaneous chain breaks.
    pub last_chain_health: Vec<ChainSnapshot>,
}

/// Chain health from a single assessment, for delta comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSnapshot {
    pub subvolume: String,
    pub drive_label: String,
    /// true = incremental chain intact (pin exists, parent found)
    pub chain_intact: bool,
}
```

New pure function:

```rust
/// Detect simultaneous chain breaks on a drive.
///
/// Returns a list of (drive_label, broken_count, total_count) tuples for
/// drives where ALL chains broke since the last assessment.
///
/// "All chains broke" means: in the previous assessment, at least 2 chains
/// were intact on this drive, and in the current assessment, zero chains are
/// intact. Single chain breaks are not flagged (normal operational events).
#[must_use]
pub fn detect_simultaneous_chain_breaks(
    previous: &[ChainSnapshot],
    current: &[ChainSnapshot],
) -> Vec<DriveAnomaly>

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveAnomaly {
    pub drive_label: String,
    pub broken_count: usize,
    pub detail: String,
}
```

**Logic:**
1. Group `previous` by drive_label. Count chains that were intact.
2. Group `current` by drive_label. Count chains that are intact.
3. For each drive: if previous had ≥2 intact chains and current has 0 → anomaly.

The ≥2 threshold prevents false positives from a drive that only has one subvolume
sending to it (a single chain break is normal).

**`sentinel_runner.rs` — detect and notify:**

In `execute_assess()`, after computing assessments and before writing the state file:

```rust
// Build current chain snapshots from assessments
let current_chains = build_chain_snapshots(&assessments, &self.state.mounted_drives);

// Compare with previous
if self.state.has_initial_assessment {
    let anomalies = sentinel::detect_simultaneous_chain_breaks(
        &self.state.last_chain_health,
        &current_chains,
    );
    for anomaly in &anomalies {
        notifications.push(build_anomaly_notification(anomaly));
    }
}

// Update state
self.state.last_chain_health = current_chains;
```

**`notify.rs` — new event variant:**

```rust
pub enum NotificationEvent {
    // ... existing variants
    /// All incremental chains on a drive broke simultaneously.
    /// Strong signal for drive swap or mass pin file loss.
    DriveAnomalyDetected {
        drive_label: String,
        broken_count: usize,
    },
}
```

Urgency: `Warning`. Title: "Drive anomaly on {label}". Body (mythic voice via
`voice.rs`): "All threads to {label} have frayed at once — {N} chains broken.
The well may hold a different stone. Verify with `urd drive verify {label}`."

#### Where does chain health come from?

The `SubvolAssessment` currently doesn't include chain health — that's in `output.rs`
`StatusAssessment`. Chain health is computed in `commands/status.rs` when building
the status output, using `fs.read_pin_file()` and comparing against external snapshots.

**This is a gap.** To detect chain breaks in the sentinel, we need chain health
in the assessment, not just in the status command's presentation layer.

**Options:**
1. Move chain health computation into `awareness.rs` (extends `SubvolAssessment`).
2. Compute chain health separately in the sentinel runner (duplicates logic).
3. Use the `output.rs` `ChainHealth` type as a shared computation.

**Recommendation: Option 1.** This is the same direction as the visual feedback model
design (which adds `OperationalHealth` to awareness). Chain health is a natural input
to operational health. The awareness module already receives `FileSystemState` which
has `read_pin_file()` and `external_snapshots()` — everything needed.

Add to `SubvolAssessment`:

```rust
pub struct SubvolAssessment {
    pub name: String,
    pub status: PromiseStatus,
    pub local: LocalAssessment,
    pub external: Vec<DriveAssessment>,
    pub advisories: Vec<String>,
    pub errors: Vec<String>,
    // NEW: chain health per drive (only for mounted, send-enabled drives)
    pub chain_health: Vec<DriveChainHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveChainHealth {
    pub drive_label: String,
    pub intact: bool,
    /// If not intact, why. Empty if intact.
    pub reason: String,
    /// The pin parent name, if a pin file exists.
    pub pin_parent: Option<String>,
}
```

Computing chain health in awareness means `commands/status.rs` can use it directly
instead of recomputing it. The `output.rs` `ChainHealth` type becomes a presentation
format derived from `DriveChainHealth`.

#### Testing

| Test | What |
|------|------|
| All chains intact → no anomaly | Previous all intact, current all intact → empty |
| Single chain break → no anomaly | One of 4 chains breaks → no anomaly (normal) |
| All chains break → anomaly | Previous 4 intact, current 0 intact → anomaly |
| Drive with 1 subvolume → no anomaly | Single chain can't trigger (threshold ≥2) |
| New drive (no previous) → no anomaly | First assessment, no previous state |
| Chain health in assessment | Pin exists + parent on drive → intact; pin missing → not intact |

~8 tests. All pure function tests, no I/O.

---

### Layer 3: Full-Send Confirmation Gate (`plan.rs`, `executor.rs`)

**What:** When the planner generates a full send for a subvolume that previously had
an incremental chain (pin file existed but parent is missing), it marks the send with
a `FullSendReason::ChainBroken`. In interactive mode, the executor prompts for
confirmation on large chain-break full sends. In autonomous mode (systemd timer),
it skips and notifies.

**Why:** A full send after a chain break is the proximate danger. Even without
detecting the swap, gating the dangerous operation prevents ENOSPC.

#### `types.rs` — full send reason

```rust
/// Why a full send was planned instead of an incremental send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullSendReason {
    /// First send to this drive for this subvolume. Normal.
    FirstSend,
    /// Pin file exists but the parent snapshot is missing on the drive.
    /// The chain broke — this is a red flag that warrants attention.
    ChainBroken,
    /// Pin file doesn't exist. Could be first send or pin was lost.
    NoPinFile,
    /// User explicitly requested --force-full.
    UserForced,
}
```

#### `plan.rs` — annotate full sends

Currently, `plan_external_send()` determines `is_incremental` and emits either
`SendIncremental` or `SendFull`. The change: when emitting `SendFull`, include
the reason.

```rust
pub enum PlannedOperation {
    // ... existing variants
    SendFull {
        snapshot: PathBuf,
        dest_dir: PathBuf,
        drive_label: String,
        subvolume_name: String,
        pin_on_success: Option<(PathBuf, SnapshotName)>,
        // NEW
        reason: FullSendReason,
    },
}
```

Logic in `plan_external_send()`:

```rust
let reason = if pin.is_some() {
    // Pin exists but parent not found on drive or locally → chain broke
    FullSendReason::ChainBroken
} else if ext_snaps.is_empty() {
    FullSendReason::FirstSend
} else {
    FullSendReason::NoPinFile
};
```

**`ChainBroken` vs `NoPinFile`:** A pin file that exists but points to a missing
snapshot is the specific signal for "chain was intact and broke." No pin file at
all is ambiguous (could be first send, could be pin loss). This distinction
matters for gating: `ChainBroken` is gated, `NoPinFile` is advisory only.

#### `executor.rs` — gate on chain-break full sends

```rust
/// Policy for handling unexpected full sends.
pub enum FullSendPolicy {
    /// Always proceed (legacy behavior).
    Allow,
    /// Skip and return an advisory (autonomous mode).
    SkipAndNotify,
    /// Prompt the user for confirmation (interactive mode, future).
    Confirm,
}
```

The executor checks `FullSendReason` before executing a full send:

```rust
PlannedOperation::SendFull { reason: FullSendReason::ChainBroken, .. } => {
    match self.full_send_policy {
        FullSendPolicy::Allow => { /* proceed */ }
        FullSendPolicy::SkipAndNotify => {
            log::warn!("Skipping chain-break full send for {} to {}: \
                        use --force-full to override", subvolume_name, drive_label);
            skipped.push(/* ... */);
            continue;
        }
        FullSendPolicy::Confirm => { /* future: interactive prompt */ }
    }
}
```

**Autonomous mode (systemd timer):** Uses `SkipAndNotify`. The nightly run skips
chain-break full sends and emits a notification. The user runs
`urd backup --force-full --subvolume htpc-home` to explicitly approve.

**Interactive mode (`urd backup` from terminal):** For now, uses `Allow` with a
prominent warning. Future: `Confirm` with a size estimate and y/N prompt.

**`--force-full` flag:** Already suggested by planner advisories. When set,
overrides `FullSendPolicy` to `Allow` for all sends.

#### Size-based threshold (from brainstorm idea 4.2)

Not designed here. A `max_unconfirmed_full_send_bytes` config option that auto-allows
small full sends (<500MB) while gating large ones is a good refinement, but it adds
config complexity. Ship the binary gate first (all chain-break full sends are gated
in autonomous mode), then add the threshold if the binary gate proves too restrictive.

#### `voice.rs` — plan display changes

When showing the plan, chain-break full sends get a distinct annotation:

```
SEND (full — chain broken):  htpc-home → WD-18TB1  (~2.1 TB estimated)
  NOTE: Previously incremental. Pin file points to 20260327-1430-htpc-home
  which is missing on WD-18TB1. Use --force-full to proceed.
```

vs. a normal first send:

```
SEND (full — first send):  subvol7-containers → WD-18TB1  (~12 MB estimated)
```

The distinction is visible in both `urd plan` and `urd backup --dry-run`.

#### Testing

| Test | What |
|------|------|
| First send → `FirstSend` reason | No pin, no external snapshots → FirstSend |
| Chain break → `ChainBroken` reason | Pin exists, parent missing from external → ChainBroken |
| No pin file → `NoPinFile` reason | No pin, external snapshots exist → NoPinFile |
| Forced → `UserForced` reason | --force-full flag → UserForced |
| SkipAndNotify skips chain-break sends | Policy=SkipAndNotify, ChainBroken → skipped |
| SkipAndNotify allows FirstSend | Policy=SkipAndNotify, FirstSend → proceeds |
| Allow proceeds on all reasons | Policy=Allow → all sends proceed |
| force-full overrides policy | --force-full + SkipAndNotify → proceeds |
| Plan display shows reason | ChainBroken send shows "chain broken" annotation |

~10 tests.

---

## Data flow

```
                   drives.rs
                      |
              drive_availability()
              (UUID + token check)
                      |
              ┌───────┴────────┐
              |                |
          Available         TokenMismatch
              |                |
         awareness.rs      planner skips
         (assess:            drive, surfaces
          chain health       advisory
          per drive)
              |
         ┌────┴─────┐
         |          |
    plan.rs     sentinel.rs
    (annotates  (detects simultaneous
     full sends  chain breaks,
     with reason) notifies)
         |
    executor.rs
    (gates chain-break
     full sends in
     autonomous mode)
```

---

## Module impact summary

| Module | Change | Size |
|--------|--------|------|
| `drives.rs` | New `TokenMismatch`/`TokenMissing` variants, token read/write/generate | ~60 lines |
| `state.rs` | New `drive_tokens` table, store/get/touch methods | ~40 lines |
| `executor.rs` | Token write on first send, full-send policy gate | ~30 lines |
| `awareness.rs` | Add `DriveChainHealth` to `SubvolAssessment`, compute in `assess()` | ~40 lines |
| `sentinel.rs` | `ChainSnapshot` tracking, `detect_simultaneous_chain_breaks()` | ~50 lines |
| `sentinel_runner.rs` | Build chain snapshots from assessments, detect and notify | ~20 lines |
| `notify.rs` | `DriveAnomalyDetected` variant | ~10 lines |
| `types.rs` | `FullSendReason` enum | ~15 lines |
| `plan.rs` | Annotate `SendFull` with reason | ~15 lines (changed, not new) |
| `voice.rs` | Chain-break annotation in plan display | ~20 lines |
| `commands/status.rs` | Use chain health from assessment instead of recomputing | ~-10 lines (simplification) |

**Total:** ~290 lines new code, ~28 new tests.

**Effort calibration:**
- UUID fingerprinting (completed): 1 module, 10 tests, 1 session → this is ~3x that scope
- Awareness model (completed): 1 module, 24 tests, 1 session → this is comparable in test count
- Estimate: 2-3 sessions. Session A: drive tokens + chain health in awareness. Session B: sentinel
  chain break detection + full-send gates.

---

## Sequencing

**Session A: Drive tokens + chain health in awareness**
1. Add `drive_tokens` table to `state.rs` with store/get/touch
2. Add token read/write/generate to `drives.rs`
3. Add `TokenMismatch`/`TokenMissing` to `DriveAvailability`
4. Extend `drive_availability()` to check tokens (requires `StateDb` access — see
   design decision below)
5. Add `DriveChainHealth` to awareness assessment
6. Update `commands/status.rs` to use assessment chain health
7. Executor: write token on first successful send
8. Tests for all of the above
9. `/check` quality gate

**Session B: Sentinel chain break detection + full-send gates**
1. Add `ChainSnapshot` and `last_chain_health` to `SentinelState`
2. Add `detect_simultaneous_chain_breaks()` pure function
3. Extend sentinel runner to build chain snapshots and detect anomalies
4. Add `DriveAnomalyDetected` to `notify.rs`
5. Add `FullSendReason` to `types.rs` and `SendFull` variant
6. Annotate full sends in `plan_external_send()`
7. Add `FullSendPolicy` to executor, gate chain-break full sends
8. Update `voice.rs` for plan display
9. Tests for all of the above
10. `/check` quality gate

Session A can be implemented independently. Session B depends on Session A for
chain health in assessments but the full-send gate (steps 5-8) is independent of
the sentinel work (steps 1-4) and could be split if needed.

---

## Design decisions

### `drive_availability()` needs `StateDb` access for token verification

Currently `drive_availability()` takes only a `&DriveConfig` and does I/O (reads
`/proc/mounts`, runs `findmnt`). Adding token verification requires reading from
SQLite.

**Options:**
1. **Pass `Option<&StateDb>` to `drive_availability()`.** Direct but changes the
   function signature everywhere it's called (planner, sentinel, status command).
2. **Separate function `verify_drive_token()`.** Keep `drive_availability()` as-is,
   call `verify_drive_token()` separately in the executor/sentinel where StateDb
   is available. Token verification becomes an additional check, not part of the
   core availability check.
3. **Add token to `FileSystemState` trait.** The trait already abstracts filesystem
   state for testing. Add `fn drive_token(&self, drive: &DriveConfig) -> Option<String>`.

**Recommendation: Option 2.** Drive availability (mounted + UUID) is a prerequisite
for token verification (you can't read a token from an unmounted drive). Keeping them
separate preserves `drive_availability()` as a lightweight check and avoids threading
SQLite through the planner's pure-function layer.

The sentinel runner calls `drive_availability()` first, then if `Available`,
calls `verify_drive_token()`. The planner doesn't verify tokens — it trusts the
drive list filtered by the caller (already the case: `commands/backup.rs` filters
drives before calling `plan()`).

### Should `TokenMismatch` block sends?

**Yes.** Sending to a drive with a token mismatch means sending to a drive that
isn't the one Urd previously sent to. This could be:
- A clone with divergent content (the test scenario)
- A completely different drive mounted at the same path

Both are dangerous. The user can resolve by running `urd drive verify WD-18TB1`
(future command from brainstorm) or by adding `--force-full --trust-drive` to
explicitly override.

**Fail-open exception:** If SQLite is unavailable (file missing, corrupt), token
verification is skipped with a warning. This preserves ADR-107 (fail-open backups).

### Where to store the token on the drive

**Option A:** Drive root (e.g., `/run/media/user/WD-18TB1/.urd-drive-token`).
**Option B:** Drive's snapshot root (e.g., `/run/media/user/WD-18TB1/.snapshots/.urd-drive-token`).

**Recommendation: Option B.** The snapshot root is Urd's working directory on the
drive. Other files Urd writes (snapshots, pin files) are there. Placing the token
alongside them is consistent. It also avoids polluting the drive root if the user
browses the drive in a file manager.

### Interaction with the visual feedback model design

The visual feedback model design proposes `OperationalHealth` in `awareness.rs`.
Chain health in assessments (this design) is a prerequisite for that — the
`OperationalHealth` computation needs chain health as an input.

**Sequencing:** This design's Session A (chain health in awareness) should be
implemented before the visual feedback model's Session A (two-axis awareness model).
The visual feedback model can then consume `DriveChainHealth` from the assessment
instead of re-deriving it.

---

## What this design does NOT cover

1. **`urd drive verify` command.** A manual verification command that runs all
   identity checks. Separate design, builds on the token infrastructure from Layer 1.

2. **Incremental chain rebuild from remote snapshots.** Finding common ancestors
   without pin files to avoid full sends. Separate design — requires investigating
   `btrfs send -p` behavior with arbitrary common ancestors.

3. **New drive onboarding (`urd drive add`).** Wizard for adding unconfigured BTRFS
   drives. Independent of swap detection.

4. **Space delta detection.** Tracking free space between ticks. Can be added to
   the sentinel independently.

5. **`--trust-drive` override flag.** Mechanism to bypass `TokenMismatch`. Design
   when the interactive UX for token mismatch recovery is addressed.

---

## Rejected alternatives

**1. Composite identity score (brainstorm 1.5).**
Multiple weak signals combined into a confidence score. Rejected because:
- Explaining "confidence 3/5" to users is harder than "token mismatch."
- Threshold tuning is a maintenance burden.
- A single strong signal (token) is more actionable than a composite of weak ones.
The simultaneous chain break detection (Layer 2) provides a complementary signal
without the complexity of scoring.

**2. Blocking sends on `TokenMissing`.**
Would break all existing drives that have never had a token written. Backward
compatibility is sacred (ADR-105). The migration path is transparent: first send
post-upgrade writes the token silently.

**3. Putting the token in config instead of SQLite.**
The config is user-edited intent. Auto-writing tokens to config creates merge
conflicts, surprises users who version-control their config, and blurs the line
between "what the user wants" and "what Urd knows." SQLite is for operational state.

**4. LUKS UUID as the identity factor.**
Only works for encrypted drives. Making identity dependent on the encryption layer
means unencrypted drives get no protection. Session tokens work on all drives.

---

## Assumptions

1. **External drives are writable when mounted.** Token write requires write access.
   If a drive is mounted read-only, token write fails gracefully (logged, not fatal).

2. **`StateDb` is available in the executor.** The executor already uses `StateDb`
   for recording operations — no new dependency.

3. **Pin file existence is a reliable chain health signal.** If the pin file exists
   but points to a snapshot that's missing, the chain is broken. If no pin file
   exists, the chain state is ambiguous (could be first send). This is already how
   `plan_external_send()` works.

4. **Two or more subvolumes per drive is normal.** The simultaneous chain break
   threshold of ≥2 assumes most drives receive sends for multiple subvolumes.
   A drive with a single subvolume can't trigger the anomaly detection. This is
   acceptable — a single chain break is a normal event that doesn't warrant an
   anomaly alert.

---

## Ready for Review

Focus areas for the arch-adversary:

1. **Token as on-disk contract.** Is a new file on external drives a manageable
   contract? What happens if the user copies the token file during a manual clone
   (intentionally or not)? Should the token include a machine identifier to prevent
   cross-machine token confusion?

2. **`drive_availability()` signature stability.** Option 2 (separate
   `verify_drive_token()`) keeps the signature stable but means callers must
   remember to call both functions. Is there a risk of a new caller checking
   availability but forgetting to verify the token?

3. **Chain health in `awareness.rs` scope creep.** The awareness module currently
   computes promise states from freshness. Adding chain health computation expands
   its scope. Is this a natural extension ("compute backup health") or does it
   violate the module's focused responsibility?

4. **`FullSendPolicy::SkipAndNotify` in autonomous mode.** This means the nightly
   backup silently skips chain-break full sends. If the user doesn't check
   notifications, those subvolumes never get sent. Is there a time limit after
   which the gate should auto-resolve? (e.g., skip for 7 days, then proceed
   with a full send and a louder notification.)

5. **Session sequencing risk.** Session A includes both drive tokens and chain
   health in awareness — two independent features in one session. Should these be
   split into separate sessions to reduce per-session risk?
