---
upi: "009"
status: proposed
date: 2026-04-02
---

# Design: `urd drives` Subcommand (UPI 009)

> **TL;DR:** A minimal `urd drives` command for drive identity management: list drive
> status with token state, and `urd drives adopt <label>` to reset a drive's token.
> Provides the user surface that UPI 004's error messages need to point at.

## Problem

Drive identity issues (UPI 004: token mismatch, token expected but missing) require
user action to resolve. Currently, the only resolution path is manual: edit SQLite,
delete token files, or hope the problem goes away.

UPI 004 introduces `TokenExpectedButMissing` — a safety gate that blocks sends when a
drive's token is missing but SQLite has a record. The error message needs to say "run
`urd drives adopt WD-18TB` to accept this drive." Without that command, the error
points at nothing.

Steve's review (score: 68/100, item #7): "I believe in the need but I want to adjust
the scope. `urd drives` should exist, but it needs to be minimal at first."

## Proposed Design

### Two subcommands only

**`urd drives`** (no subcommand) — List all configured drives with status:

```
DRIVE        STATUS       TOKEN    FREE     ROLE
WD-18TB      connected    ✓        4.2TB    primary
WD-18TB1     absent 10d   —        —        offsite
2TB-backup   connected    new      1.1TB    default
```

Column meanings:
- STATUS: connected / absent {duration} / identity suspect
- TOKEN: `✓` (verified) / `new` (no SQLite record, will be set on first send) /
  `✗ mismatch` / `✗ missing` (SQLite has record but drive doesn't) / `—` (drive not mounted)
- FREE: available space on drive
- ROLE: from `DriveRole` config (primary/offsite/default)

**`urd drives adopt <label>`** — Reset a drive's token relationship:

1. Verify the drive is mounted at its configured path
2. Read any existing on-disk token
3. If on-disk token exists: store it in SQLite (replacing old record)
4. If no on-disk token: generate a new one, write to drive, store in SQLite
5. Confirm: "Adopted WD-18TB — token verified, sends enabled."

This handles:
- Cloned drive with no token (generates new one)
- Replaced drive with no token (generates new one)
- Drive with mismatched token (re-adopts the on-disk token)
- Drive after `btrfstune -u` (token survives UUID change)

### Command structure

```
urd drives              # List drives
urd drives adopt <label>  # Accept/reset drive identity
```

No `identify`, `forget`, or other subcommands yet. Steve: "Minimal at first."

### Implementation in commands/

New file: `commands/drives.rs` with two handler functions:

```rust
pub fn run_drives_list(config: &Config) -> Result<()>
pub fn run_drives_adopt(config: &Config, label: &str) -> Result<()>
```

### Output types

Add to `output.rs`:

```rust
pub struct DrivesListOutput {
    pub drives: Vec<DriveListEntry>,
}

pub struct DriveListEntry {
    pub label: String,
    pub status: String,       // "connected", "absent 10d", etc.
    pub token_state: String,  // "verified", "new", "mismatch", "missing", "unknown"
    pub free_space: Option<ByteSize>,
    pub role: DriveRole,
}
```

### Voice rendering

Add to `voice.rs`:

```rust
pub fn render_drives_list(data: &DrivesListOutput, mode: OutputMode) -> String
```

Interactive mode: colored table (similar to status table style).
Daemon mode: JSON.

### CLI registration

Add `drives` subcommand to the CLI parser in `main.rs`:

```rust
Drives {
    #[command(subcommand)]
    action: Option<DrivesAction>,
},

enum DrivesAction {
    Adopt { label: String },
}
```

`urd drives` (no action) → list. `urd drives adopt <label>` → adopt.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `commands/drives.rs` | New file: list and adopt handlers | Unit test with MockBtrfs: list shows correct token states; adopt writes token and updates SQLite |
| `output.rs` | Add `DrivesListOutput`, `DriveListEntry` | Struct tests |
| `voice.rs` | Add `render_drives_list()` | Unit test: output format matches specification |
| `main.rs` | Register `drives` subcommand | Manual smoke test |
| `drives.rs` | No changes (uses existing `verify_drive_token`, `write_drive_token`, etc.) | Existing tests cover |
| `state.rs` | No changes (uses existing `store_drive_token`) | Existing tests cover |

## Effort Estimate

Standard tier. ~0.5-1 session. New command module, but the underlying functions
(token read/write/verify, drive availability) already exist. The main work is the
command wiring, output types, and voice rendering.

## Sequencing

1. `output.rs` — types (pure)
2. `commands/drives.rs` — handlers
3. `voice.rs` — rendering
4. `main.rs` — CLI registration
5. Tests

Build UPI 004 (TokenMissing gate) first or in parallel. UPI 009's `adopt` command is
what UPI 004's error message points to, but UPI 004 can ship with a generic message
("check drive identity") until UPI 009 is ready.

## Architectural Gates

None. This is a new read-mostly command. `adopt` writes to the drive's token file and
SQLite, but both are existing operations used by the executor.

**Consideration:** `urd drives adopt` is a user-initiated identity override. It should
require the drive to be physically mounted — never allow adopting an absent drive. This
prevents accidentally overwriting a valid token record for a drive that's temporarily
disconnected.

## Rejected Alternatives

**Full `urd drives` suite with identify, forget, replace, status.** Over-scoped for
current needs. Steve explicitly said "minimal at first." `list` and `adopt` cover the
two scenarios from testing: "what drives does Urd see?" and "accept this drive."

**Put drive management in `urd doctor`.** Doctor is diagnostic, not interactive. Drive
adoption is a deliberate user action, not a diagnostic suggestion.

**Inline `adopt` behavior into backup command with `--accept-drive` flag.** Flags that
bypass safety belong in dedicated commands, not as options on the primary backup path.
A flag is forgettable; a command is deliberate.

## Assumptions

1. `write_drive_token()` and `store_drive_token()` are safe to call from a command
   handler. (Verified: executor already calls them from non-backup contexts.)
2. `DriveRole` is available on `DriveConfig` and can be displayed. (Verified: types.rs.)
3. The CLI parser (clap) supports optional subcommands where no subcommand means "list."
   (Standard clap pattern.)

## Resolved Decisions (from /grill-me)

**009-Q1: `adopt` trusts on-disk token.** One rule: "on-disk wins, generate if absent."
If the drive has a token file, store that token in SQLite (replacing any old record).
If no token file, generate a fresh UUID-v4, write to drive, store in SQLite. This
matches the semantics of "adopt" — accepting what's there, not imposing identity.

**009-Q2: Unmounted drives show token record state.** TOKEN column shows `recorded`
(SQLite has a token for this drive) or `—` (no record). Helps diagnose UPI 004
scenarios: if a drive shows `recorded` but mounts without a token file, the user
understands why sends are blocked. Simple to implement — just query SQLite.

**Deferred: Confirmation prompt, re-adopt, snapshot counts.** No confirmation prompt
(command is already deliberate). Already-adopted drives are no-op with message.
Snapshot counts deferred to future `urd drives status <label>`.
