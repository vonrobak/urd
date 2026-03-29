# Implementation Plan: HSD-A — Drive Session Tokens + Chain Health as Awareness Input

**Date:** 2026-03-29
**Scope:** Session A of Hardware Swap Defenses
**Design:** `docs/95-ideas/2026-03-28-design-hardware-swap-defenses.md`
**Review:** `docs/99-reports/2026-03-28-hardware-swap-defenses-design-review.md`

---

## Design Decisions Resolved

**Q1: Where does `verify_drive_token()` live?**
Standalone function in `drives.rs` that takes `(&DriveConfig, &StateDb)`. It reads the token file from the drive (filesystem I/O) and looks up the stored reference in SQLite. This is I/O code that belongs with the other drive I/O functions. The function returns `DriveAvailability` so callers get a single enum to match on.

**Q2: New `DriveChainHealth` type or reuse `output::ChainHealth`?**
New `DriveChainHealth` struct in `awareness.rs`. It carries richer data (`intact: bool`, `reason: String`, `pin_parent: Option<String>`) needed by the sentinel in Session B. The `output::ChainHealth` becomes a presentation format derived from `DriveChainHealth`. This avoids coupling the awareness model to the output layer.

**Q3: Token generation — `uuid` crate?**
The `uuid` crate is NOT in `Cargo.toml`. Two options:
- **Option A:** Add `uuid = { version = "1", features = ["v4"] }` as a dependency (~clean, standard).
- **Option B:** Use `format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", ...)` with bytes from `/dev/urandom` or `getrandom` crate.

**Recommendation: Option A.** `uuid` is a small, well-maintained crate. The token format should be a standard UUID for recognizability. Add to `[dependencies]` in step 1.

---

## Implementation Steps (ordered for compilation)

### Step 1: Add `uuid` crate dependency

**File:** `Cargo.toml`

Add under `[dependencies]`:
```toml
uuid = { version = "1", features = ["v4"] }
```

This unblocks token generation. Cargo will resolve on next build.

---

### Step 2: Add `drive_tokens` table to `state.rs`

**File:** `src/state.rs`

**Changes:**

1. Add the table creation to `init_schema()` — append to the `execute_batch` SQL:
```sql
CREATE TABLE IF NOT EXISTS drive_tokens (
    drive_label TEXT PRIMARY KEY,
    token TEXT NOT NULL,
    first_seen TEXT NOT NULL,
    last_verified TEXT NOT NULL
);
```

2. Add three methods to `impl StateDb`:

```rust
/// Store a drive session token (insert or replace).
pub fn store_drive_token(&self, label: &str, token: &str, now: &str) -> crate::error::Result<()>
```
Uses `INSERT OR REPLACE` to handle both first-write and re-write (self-healing per ADR-102).

```rust
/// Look up a stored drive session token by drive label.
pub fn get_drive_token(&self, label: &str) -> crate::error::Result<Option<String>>
```
Returns `None` if no token stored for this drive.

```rust
/// Update the last_verified timestamp for a drive token.
pub fn touch_drive_token(&self, label: &str, now: &str) -> crate::error::Result<()>
```

All three follow the existing pattern: `rusqlite::params!`, map errors to `UrdError::State(String)`.

3. Add tests (in existing `#[cfg(test)] mod tests`):
   - `store_and_get_drive_token` — roundtrip
   - `get_drive_token_returns_none_for_unknown` — no row
   - `store_drive_token_overwrites` — upsert behavior
   - `touch_drive_token_updates_timestamp` — verify last_verified changes
   - `schema_is_idempotent` — existing test already covers this via `init_schema()` re-call

**Why first:** No dependencies on other changes. Compiles independently. All other steps depend on this.

---

### Step 3: Add token functions to `drives.rs`

**File:** `src/drives.rs`

**Changes:**

1. Add `TokenMismatch` and `TokenMissing` variants to `DriveAvailability`:

```rust
pub enum DriveAvailability {
    Available,
    NotMounted,
    UuidMismatch { expected: String, found: String },
    UuidCheckFailed(String),
    /// Drive is mounted and UUID matches, but the session token does not
    /// match the stored reference. The physical media may have changed.
    /// NOTE: This is an identity signal, not a security control.
    /// A copied token file defeats verification. Threat model: accidental swaps.
    TokenMismatch { expected: String, found: String },
    /// Drive is mounted and UUID matches, but no token file exists on the drive.
    /// Normal for drives that have not completed their first Urd send.
    TokenMissing,
}
```

Note: `DriveAvailability` already derives `Clone`, so the new variants with `String` fields work.

2. Add `TOKEN_FILENAME` constant:
```rust
const TOKEN_FILENAME: &str = ".urd-drive-token";
```

3. Add three public functions:

```rust
/// Path to the token file on a drive's snapshot root.
fn token_file_path(drive: &DriveConfig) -> PathBuf {
    drive.mount_path.join(&drive.snapshot_root).join(TOKEN_FILENAME)
}

/// Read the drive session token from the drive's snapshot root.
/// Returns Ok(None) if the file does not exist.
/// Returns Ok(Some(token)) if found and parsed.
/// Returns Err on I/O errors other than NotFound.
pub fn read_drive_token(drive: &DriveConfig) -> crate::error::Result<Option<String>>
```
Reads the file, skips comment lines (starting with `#`), parses `token=VALUE`. Returns the value.

```rust
/// Write a drive session token to the drive's snapshot root.
/// Uses atomic write (write to temp file, then rename) for crash safety.
/// The file includes human-readable comments explaining its purpose.
pub fn write_drive_token(drive: &DriveConfig, token: &str) -> crate::error::Result<()>
```
Writes the file format from the design doc. Uses `std::fs::write` to a temp file in the same directory, then `std::fs::rename`. If write fails (read-only drive), returns error — caller handles gracefully.

```rust
/// Generate a new random drive session token.
#[must_use]
pub fn generate_drive_token() -> String {
    uuid::Uuid::new_v4().to_string()
}
```

4. Add `verify_drive_token()` — the key function per design decision Option 2:

```rust
/// Verify the drive session token against the stored reference.
///
/// Call this AFTER `drive_availability()` returns `Available`.
/// This is a separate check because it requires `StateDb` access,
/// which the planner does not have.
///
/// PROTOCOL OBLIGATION: Any code path that sends to a drive should call
/// both `drive_availability()` and `verify_drive_token()`. Callers that
/// skip token verification send to an unverified drive.
///
/// Returns:
/// - `Available` if tokens match (or no stored token — self-healing path).
/// - `TokenMissing` if no token file on drive (benign, sends proceed).
/// - `TokenMismatch` if tokens differ (sends blocked).
#[must_use]
pub fn verify_drive_token(drive: &DriveConfig, state: &StateDb) -> DriveAvailability
```

Logic:
1. `read_drive_token(drive)` — get token from filesystem.
2. `state.get_drive_token(&drive.label)` — get stored reference.
3. Match:
   - Both exist and match → `Available` (+ `state.touch_drive_token()`)
   - Both exist and differ → `TokenMismatch { expected, found }`
   - Drive has token, SQLite has none → `Available` (first verification after migration; store the drive's token in SQLite as the reference — self-healing)
   - Drive has no token, SQLite has one → `TokenMissing` (suspicious but benign — first send will re-establish)
   - Neither has token → `TokenMissing` (normal: pre-token drive)
   - Read errors → log warning, return `Available` (fail-open per ADR-107)

5. Update existing `drive_availability()` — NO CHANGES needed. It stays as-is. Token verification is separate per Option 2.

6. Add tests:
   - `read_drive_token_roundtrip` — write then read (uses tempdir)
   - `read_drive_token_missing_file` — returns None
   - `read_drive_token_ignores_comments` — file with comments and blank lines
   - `write_drive_token_creates_file` — verify file contents
   - `generate_drive_token_is_valid_uuid` — parse result as UUID
   - `verify_drive_token_match` — both exist and match → Available
   - `verify_drive_token_mismatch` — different tokens → TokenMismatch
   - `verify_drive_token_no_file` — no file on drive → TokenMissing
   - `verify_drive_token_no_stored` — drive has token, SQLite empty → Available (self-healing)
   - `verify_drive_token_neither` — no token anywhere → TokenMissing

Note: `verify_drive_token` tests need both tempdir (for token file) and in-memory SQLite. The test helper creates a `DriveConfig` pointing at the tempdir.

---

### Step 4: Handle new `DriveAvailability` variants in `plan.rs`

**File:** `src/plan.rs`

**Changes:**

1. In `plan()` function, the match on `fs.drive_availability(drive)` (around line 174) needs two new arms:

```rust
DriveAvailability::TokenMismatch { expected, found } => {
    skipped.push((
        subvol.name.clone(),
        format!(
            "drive {} token mismatch (expected {}, found {}) — possible drive swap",
            drive.label, expected, found
        ),
    ));
    continue;
}
DriveAvailability::TokenMissing => {
    // Benign: first use or pre-token drive. Proceed with send.
    // Token will be written by executor on successful send.
}
```

`TokenMissing` falls through to the send planning — it does NOT skip. This is the backward-compatibility path.

2. In `MockFileSystemState::drive_availability()` — no changes needed. The `drive_availability_overrides` HashMap already supports arbitrary `DriveAvailability` values including the new variants.

3. Add tests:
   - `token_mismatch_skips_send` — drive with `TokenMismatch` override → send skipped
   - `token_missing_allows_send` — drive with `TokenMissing` override → send proceeds

---

### Step 5: Add `DriveChainHealth` to `awareness.rs`

**File:** `src/awareness.rs`

**Changes:**

1. Add the new type:

```rust
/// Chain health for a single subvolume/drive pair.
/// Richer than `output::ChainHealth` — carries data needed by sentinel (Session B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveChainHealth {
    pub drive_label: String,
    /// true if incremental chain is intact (pin exists, parent found locally and on drive).
    pub intact: bool,
    /// Why the chain is not intact. Empty if intact.
    pub reason: String,
    /// The pin parent name, if a pin file exists.
    pub pin_parent: Option<String>,
}
```

2. Add `chain_health: Vec<DriveChainHealth>` field to `SubvolAssessment`:

```rust
pub struct SubvolAssessment {
    pub name: String,
    pub status: PromiseStatus,
    pub local: LocalAssessment,
    pub external: Vec<DriveAssessment>,
    pub advisories: Vec<String>,
    pub errors: Vec<String>,
    /// Chain health per mounted drive (only for send-enabled subvolumes).
    pub chain_health: Vec<DriveChainHealth>,
}
```

3. Add a pure helper function to compute chain health for one subvolume/drive pair:

```rust
/// Compute chain health for a subvolume on a specific drive.
/// Pure function: uses FileSystemState trait for all I/O.
fn compute_chain_health(
    fs: &dyn FileSystemState,
    local_dir: &std::path::Path,
    drive: &DriveConfig,
    subvol_name: &str,
    local_snaps: &[SnapshotName],
    ext_snaps: &[SnapshotName],
) -> DriveChainHealth
```

Logic:
1. If `ext_snaps` is empty → `DriveChainHealth { intact: false, reason: "no drive data", pin_parent: None }`
2. Read pin: `fs.read_pin_file(local_dir, &drive.label)`
3. If no pin → `intact: false, reason: "no pin file"`
4. If pin exists:
   - Check if pin parent is in `local_snaps` (by name comparison)
   - Check if pin parent is in `ext_snaps` (by name comparison)
   - Both present → `intact: true, reason: "", pin_parent: Some(pin_name)`
   - Missing locally → `intact: false, reason: "pin missing locally"`
   - Missing on drive → `intact: false, reason: "pin missing on drive"`

This is the same logic as `commands/status.rs::compute_chain_health()` but uses snapshot lists (already fetched) instead of filesystem `exists()` calls. This is the pure-function version.

4. In `assess()`, after computing drive assessments for each subvolume (the `for drive in &config.drives` loop), compute chain health:

```rust
// ── Chain health ───────────────────────────────────────────
let mut chain_health = Vec::new();
if subvol.send_enabled {
    for drive in &config.drives {
        if !fs.is_drive_mounted(drive) {
            continue;
        }
        let ext_snaps = fs.external_snapshots(drive, &subvol.name)
            .unwrap_or_default();
        let local_snaps_for_chain = fs.local_snapshots(&snapshot_root, &subvol.name)
            .unwrap_or_default();
        let health = compute_chain_health(
            fs, &local_dir, drive, &subvol.name,
            &local_snaps_for_chain, &ext_snaps,
        );
        chain_health.push(health);
    }
}
```

Note: `local_snapshots` is already fetched earlier in `assess()` for the local assessment. To avoid a redundant call, capture the local snapshots result and reuse it. The `local_snaps` from the local assessment section can be stored in a variable before the drive loop.

5. Update all `SubvolAssessment` construction sites to include `chain_health: Vec::new()` (for error/early-return paths) or the computed vector.

6. Add tests:
   - `chain_health_incremental_when_pin_exists_and_parent_found` — pin points to snapshot present in both local and external lists
   - `chain_health_broken_when_pin_parent_missing_on_drive` — pin exists, parent in local but not external
   - `chain_health_broken_when_pin_parent_missing_locally` — pin exists, parent in external but not local
   - `chain_health_no_pin` — no pin file → not intact
   - `chain_health_no_drive_data` — empty external snapshots
   - `chain_health_empty_for_unmounted_drive` — unmounted drives excluded
   - `chain_health_empty_when_send_disabled` — send_enabled=false → no chain health entries

---

### Step 6: Update `commands/status.rs` to use assessment chain health

**File:** `src/commands/status.rs`

**Changes:**

1. Remove the `compute_chain_health()` private function (lines 148-174). It is replaced by the awareness-level computation.

2. Remove the chain health computation loop (lines 29-66) that iterates mounted drives and subvolumes.

3. Replace with a derivation from the assessment's `chain_health` field:

```rust
// ── Chain health per subvolume (from awareness assessment) ──
let mut chain_health_entries: Vec<ChainHealthEntry> = Vec::new();
for assessment in &assessments {
    if assessment.chain_health.is_empty() {
        continue;
    }
    // Take worst health across drives (same as before)
    let worst = assessment.chain_health.iter()
        .map(|ch| {
            if ch.intact {
                ChainHealth::Incremental(ch.pin_parent.clone().unwrap_or_default())
            } else if ch.reason == "no drive data" {
                ChainHealth::NoDriveData
            } else {
                ChainHealth::Full(ch.reason.clone())
            }
        })
        .min();
    if let Some(health) = worst {
        chain_health_entries.push(ChainHealthEntry {
            subvolume: assessment.name.clone(),
            health,
        });
    }
}
```

This converts `DriveChainHealth` (awareness type) to `ChainHealth` (output type) for presentation. The conversion is straightforward and keeps the output contract stable.

4. Remove unused imports: `chain` module import can be removed from the chain health path (still needed for `find_pinned_snapshots` for pin count).

5. No new tests needed — the existing `urd status` integration behavior is preserved. The conversion logic is simple enough to validate by inspection. The underlying chain health computation is tested in `awareness.rs`.

---

### Step 7: Update `output.rs` — `StatusAssessment::from_assessment` 

**File:** `src/output.rs`

**Changes:**

The `StatusAssessment::from_assessment()` method constructs a `StatusAssessment` from a `SubvolAssessment`. Check if it needs updating for the new `chain_health` field. Since `StatusAssessment` is the output presentation type and chain health is already carried separately in `StatusOutput.chain_health`, no structural change is needed here — the new field is consumed by the status command's conversion in Step 6.

However, verify that adding the `chain_health` field to `SubvolAssessment` doesn't break any existing `from_assessment()` calls. Since `SubvolAssessment` is not `Deserialize` (it's constructed programmatically), adding a field only affects construction sites, which are all in `awareness.rs` (updated in Step 5).

---

### Step 8: Executor — write token on first successful send

**File:** `src/executor.rs`

**Changes:**

1. In `execute_send()`, after the successful send path (where pin_on_success is written, around line 438-447), add token write logic:

```rust
// Token-on-success: write drive session token if not already present.
// Same pattern as pin-on-success: failure is logged, not fatal.
self.maybe_write_drive_token(drive_label);
```

2. Add a private method to `Executor`:

```rust
/// Write a drive session token if one does not already exist on the drive.
/// Called after a successful send. Failures are logged but not fatal.
fn maybe_write_drive_token(&self, drive_label: &str) {
    let Some(drive) = self.config.drives.iter().find(|d| d.label == drive_label) else {
        return;
    };

    // Check if token already exists on drive
    match drives::read_drive_token(drive) {
        Ok(Some(_)) => return, // Token already present, nothing to do
        Ok(None) => {}          // No token — write one
        Err(e) => {
            log::warn!("Failed to read drive token for {drive_label}: {e}");
            return;
        }
    }

    let token = drives::generate_drive_token();
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    if let Err(e) = drives::write_drive_token(drive, &token) {
        log::warn!("Failed to write drive token for {drive_label}: {e}");
        return;
    }

    // Store in SQLite (if available)
    if let Some(state) = self.state {
        if let Err(e) = state.store_drive_token(drive_label, &token, &now) {
            log::warn!("Token written to drive but failed to store in SQLite for {drive_label}: {e}");
            // Not fatal: next verification will self-heal by reading from drive
        }
    }

    log::info!("Drive session token written for {drive_label}");
}
```

3. The `execute_send` function receives `drive_label: &str` — it already has this parameter. The `Executor` struct already carries `config` and `state`. No signature changes needed.

4. Add tests:
   - `executor_writes_token_on_first_send` — mock send succeeds, verify token file created in tempdir and stored in in-memory SQLite
   - `executor_skips_token_write_if_exists` — pre-existing token file, verify no overwrite
   - `executor_handles_token_write_failure_gracefully` — read-only tempdir, verify send still succeeds

Note: These tests require tempdir for the drive path. The existing executor test infrastructure uses `MockBtrfs` but real filesystem paths. Token write tests need the `DriveConfig.mount_path` to point at a tempdir with a `snapshot_root` subdirectory.

---

### Step 9: Update `from_assessment` and any remaining compilation fixes

**Files:** Various — sweep for compilation errors.

Expected issues:
1. Any place that constructs `SubvolAssessment` manually (tests in `awareness.rs`, possibly `output.rs` tests) needs the new `chain_health` field.
2. Pattern matches on `DriveAvailability` that are exhaustive (in `plan.rs`, `commands/backup.rs`, voice display, etc.) need the two new variants. Search for all match sites.

**Grep targets:**
- `DriveAvailability::` — find all match arms
- `SubvolAssessment {` — find all construction sites

---

## File Change Summary

| File | Change Type | Estimated Lines |
|------|------------|-----------------|
| `Cargo.toml` | Add uuid dependency | +1 |
| `src/state.rs` | New table + 3 methods + 4 tests | +60 |
| `src/drives.rs` | 2 new enum variants, 4 new functions, ~10 tests | +120 |
| `src/awareness.rs` | New `DriveChainHealth` type, chain health computation, 7 tests | +100 |
| `src/plan.rs` | 2 new match arms for `DriveAvailability`, 2 tests | +20 |
| `src/commands/status.rs` | Remove old chain health, derive from assessment | -30/+15 (net -15) |
| `src/executor.rs` | `maybe_write_drive_token()`, 3 tests | +60 |
| `src/output.rs` | Possible minor adjustment to `from_assessment` | +2 |

**Total:** ~380 lines new code, ~26 tests.

---

## Compilation Order

Build and test incrementally in this order to minimize red time:

1. `Cargo.toml` (uuid dep) → `cargo check`
2. `state.rs` (new table + methods) → `cargo test -p urd state::tests`
3. `drives.rs` (new variants + functions, NO changes to `drive_availability`) → `cargo test -p urd drives::tests`
4. `plan.rs` (new match arms) → `cargo test -p urd plan::tests`
5. `awareness.rs` (DriveChainHealth + computation) → `cargo test -p urd awareness::tests`
6. `commands/status.rs` (derive chain health from assessment) → `cargo test`
7. `executor.rs` (token write on send) → `cargo test -p urd executor::tests`
8. Full `cargo test` + `cargo clippy`

---

## Risks and Mitigations

**Risk 1: Exhaustive match breakage.** Adding two variants to `DriveAvailability` will break all existing match arms. The compiler enforces this — it is a feature, not a bug. Every match site must explicitly handle the new variants.

**Mitigation:** Grep for `DriveAvailability` matches before starting. Known sites: `plan.rs` (line 174), `plan.rs` `MockFileSystemState::drive_availability`, possibly `commands/backup.rs`, `sentinel_runner.rs`, `voice.rs`.

**Risk 2: `awareness.rs` double-fetch of local snapshots.** The local assessment already fetches snapshots. Chain health needs the same list. If we call `fs.local_snapshots()` twice, the mock returns the same data but real FS does redundant I/O.

**Mitigation:** Capture the local snapshots result in a variable at the top of the per-subvolume loop and reuse it for both local assessment and chain health.

**Risk 3: Token file atomic write on non-local filesystems.** `std::fs::rename` across filesystems fails. The temp file and final file are in the same directory (the drive's snapshot root), so rename should work. But network-mounted drives might have issues.

**Mitigation:** Write to a temp file in the same directory (`{snapshot_root}/.urd-drive-token.tmp`), then rename. Same-directory rename is atomic on all POSIX filesystems including BTRFS and ext4 over USB.

**Risk 4: Test infrastructure for executor token tests.** Executor tests currently use `MockBtrfs` but the token write goes through real filesystem calls in `drives.rs`. Tests need a real tempdir with the expected directory structure.

**Mitigation:** Create helper that builds a tempdir with `{mount_path}/{snapshot_root}/` structure and returns a matching `DriveConfig`.
