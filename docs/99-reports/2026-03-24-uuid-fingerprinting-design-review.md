# UUID Drive Fingerprinting — Pre-Implementation Design Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 2a design — UUID drive fingerprinting
**Reviewer:** Architectural Adversary (Claude)
**Commit:** 56d25fc (master)

## Executive Summary

The proposed design is sound in premise and scope. UUID fingerprinting is a genuine safety
feature — it prevents silent data corruption when a different drive mounts at the same path.
However, the design as sketched has one critical gap (how to get the UUID given LUKS-encrypted
drives), one significant architectural question (where verification lives relative to the
planner purity invariant), and several moderate decisions that should be resolved before code.
The feature is appropriately scoped as "low effort" — but only if the design decisions below
are settled first.

## What Kills You

**Silent sends to the wrong drive.** If drive A's label is "WD-18TB" and drive B gets mounted
at `/run/media/user/WD-18TB` (e.g., after a drive swap, relabel, or automount glitch), Urd
will send incremental snapshots whose parent doesn't exist on drive B. `btrfs send -p` will
fail — but a full send would succeed silently, creating a new chain on the wrong drive and
wasting space while the user believes drive A is receiving backups. This feature exists to
prevent exactly that scenario. The distance from "no UUID check" to "data on wrong drive" is
one mount event.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Design addresses the right threat; edge cases below need resolution |
| Security | 4 | No new privilege escalation; `findmnt` is unprivileged |
| Architectural Excellence | 3 | Planner purity tension is real and must be resolved cleanly |
| Systems Design | 4 | `findmnt` approach handles LUKS, no new dependencies |
| Rust Idioms | n/a | Pre-implementation; no code to evaluate |
| Code Quality | n/a | Pre-implementation |

## Design Tensions

### Tension 1: Planner Purity vs. UUID Verification

**The trade-off:** The planner is a pure function — config + filesystem state in, plan out.
UUID verification is a filesystem query. Where does it live?

**Option A: Inside `FileSystemState` trait.** Add `fn filesystem_uuid(&self, mount_path: &Path) -> Option<String>` to the trait. The planner calls `fs.is_drive_mounted(drive)` and then
`fs.filesystem_uuid(&drive.mount_path)` to verify. This preserves planner purity — the
planner still doesn't do I/O, it asks the trait. `MockFileSystemState` can return
configurable UUIDs for testing.

**Option B: Separate pre-flight check.** UUID verification runs before planning, in the
command handler. Drives that fail verification are removed from the config (or marked
unmounted) before the planner sees them. The planner never knows about UUIDs.

**Recommendation: Option A.** It fits the existing pattern — the planner already queries
`is_drive_mounted()` through the trait. Adding UUID verification as another trait method
keeps the planner pure while making UUID mismatch a testable condition. Option B hides
information from the planner and makes the skip reason ("UUID mismatch") harder to surface
in the plan's `skipped` list.

**One refinement:** Don't add a separate `filesystem_uuid()` method. Instead, change the
semantics of the existing `is_drive_mounted()` to return a richer signal. A drive whose
UUID doesn't match is effectively "not the right drive" — it should not be treated as
mounted. This keeps the planner's logic unchanged (it already skips unmounted drives) while
gaining UUID safety.

### Tension 2: `is_drive_mounted()` Return Type — Bool vs. Enum

**The trade-off:** Currently `is_drive_mounted()` returns `bool`. The simplest change is
to keep `bool` and fold UUID mismatch into "not mounted." But then the skip reason is
"drive WD-18TB not mounted" when the real situation is "a drive is mounted at that path
but it's not WD-18TB."

**Option A: Keep `bool`, add logging.** `is_drive_mounted()` returns `false` on UUID
mismatch, but logs a warning. The skip message says "not mounted." Simple, minimal change.

**Option B: Return enum.** `DriveStatus { Mounted, NotMounted, WrongDrive { expected: String, found: String } }`. The planner can produce different skip reasons. More informative,
slightly more complex.

**Recommendation: Option B, but lightweight.** The skip reason matters — "not mounted" and
"wrong drive at mount point" require very different user responses. The first is normal
(drive unplugged), the second is an emergency. This is a backup safety tool; the distinction
is load-bearing. But keep it to a simple enum, not a struct with metadata.

```rust
pub enum DriveAvailability {
    Available,
    NotMounted,
    UuidMismatch { expected: String, found: String },
    UuidCheckFailed(String),  // e.g., findmnt not found
}
```

The planner maps `Available` → proceed, everything else → skip with appropriate reason.

### Tension 3: `Option<String>` UUID vs. Newtype

**The trade-off:** UUIDs are well-structured strings (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
Using `Option<String>` is simple but doesn't prevent typos or invalid formats in config.

**Recommendation: `Option<String>` is fine.** The UUID is verified against what the
filesystem reports — a typo will cause a mismatch, which is a safe failure mode (refuses to
send, doesn't send to wrong drive). Format validation in config parsing is nice-to-have but
not safety-critical. A newtype would be over-engineering for a single field that's validated
by use.

### Tension 4: How to Get the UUID

**Context from this system:** The external drives use LUKS encryption. In `/proc/mounts`,
the device appears as `/dev/mapper/luks-<outer-uuid>`. The filesystem UUID (inside the LUKS
container) is different from the partition UUID. `lsblk --json` shows nested `children`
with different UUIDs at each layer.

**Options:**
- `blkid <device>` — needs sudo or at least read access to the block device
- `lsblk -o UUID -n <device>` — works without sudo but requires parsing the device from
  `/proc/mounts` first, then handling LUKS nesting
- `findmnt -n -o UUID <mount_path>` — works without sudo, takes the mount path directly,
  returns the filesystem UUID. Handles LUKS transparently. Single clean output.
- `/dev/disk/by-uuid/` symlink resolution — works without sudo, but maps UUID → device,
  not mount_path → UUID. Would need to reverse-lookup, which is fragile.

**Recommendation: `findmnt`.** It's the right tool — takes a mount path (which we have),
returns the filesystem UUID (which we want), handles LUKS (which we need), requires no
sudo (which we prefer). It's part of `util-linux`, present on every Linux system Urd
targets. Parse the single-line output; no JSON needed.

```
findmnt -n -o UUID /run/media/patriark/WD-18TB1
647693ed-490e-4c09-8816-189ba2baf03f
```

## Findings

### Finding 1: Auto-populate UUID — Do Not Build (Moderate)

The proposal asks about "learn mode" — auto-populating UUID on first successful send.

**Why it's wrong:** This defeats the purpose. The threat model is "wrong drive mounts at the
expected path." If Urd auto-learns the UUID of whatever drive is there, it will happily learn
the wrong drive's UUID on the first run. The user must verify and set the UUID intentionally.

**What to do instead:** Provide a helper command or config guidance. When UUID is absent from
config, Urd should show the detected UUID in the warning message so the user can copy-paste
it into their config:

```
warning: drive "WD-18TB" has no UUID configured
  detected UUID at /run/media/user/WD-18TB: 647693ed-490e-4c09-8816-189ba2baf03f
  add `uuid = "647693ed-..."` to your [[drives]] config for safety
```

This is the "guide through affordances" UX principle in action — show the user what to do,
don't do it for them in a context where doing it wrong is dangerous.

### Finding 2: Graceful Degradation When `findmnt` Is Absent (Moderate)

`findmnt` should be present on all target systems, but a missing binary should not prevent
backups from running. If UUID is configured but `findmnt` fails:

- Log a warning (UUID verification could not be performed)
- **Still refuse to send** — the user explicitly configured a UUID, meaning they want
  verification. Silently skipping verification when the tool is missing would be a false
  sense of security.
- The `UuidCheckFailed` variant in the enum handles this.

If UUID is *not* configured and `findmnt` fails, there's nothing to check — proceed normally.

### Finding 3: UUID Validation Scope — Only External Drives (Minor)

UUID fingerprinting is about external drives — they get plugged/unplugged, swapped, mounted
at varying paths. The local snapshot roots are on the system drive and don't change. Don't
add UUID checking for local snapshot roots — it adds complexity for a threat that doesn't
exist in this system's deployment.

### Finding 4: Config Format Decision (Minor)

The TOML field should be `uuid` (not `filesystem_uuid` or `fs_uuid`). It's the only UUID
in the drive config, and its meaning is clear in context. Keep it short.

```toml
[[drives]]
label = "WD-18TB"
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"
mount_path = "/run/media/patriark/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
```

### Finding 5: Pin Files Are Unaffected (Commendation of Question)

Pin files use drive labels, not UUIDs. This is correct and should not change. The label is
the user's logical name for a drive; the UUID is a physical identity check. If a user
replaces a failed drive and formats it with a new UUID, they update the UUID in config but
keep the label. Pin files track the logical chain, not the physical medium.

### Finding 6: `FileSystemState` Trait Growth (Moderate)

The trait already has 9 methods (noted in Known Issues). Adding `drive_availability()` makes
it 10. This is acceptable for now but reinforces the suggestion to rename to `SystemState`
if another method is needed after this one. Don't rename as part of this PR — it's a separate
concern.

## The Simplicity Question

**What could be removed?** Nothing in the proposed design is unnecessary. UUID fingerprinting
is a targeted safety feature with a clear threat model.

**What earns its keep?**
- The `DriveAvailability` enum earns its keep because the skip reason distinction is
  safety-critical (wrong drive ≠ unplugged drive).
- The `findmnt` approach earns its keep by eliminating LUKS complexity, sudo requirements,
  and `/proc/mounts` device parsing.
- The `Option<String>` approach earns its keep by preserving backward compatibility without
  config migration code.

**What doesn't earn its keep?**
- Auto-learn mode
- UUID format validation beyond non-empty string
- UUID for local snapshot roots
- A `Uuid` newtype

## Priority Action Items

1. **Resolve Tension 1+2:** Add `drive_availability()` to `FileSystemState` trait returning
   `DriveAvailability` enum. Planner uses this instead of `is_drive_mounted()`. Keep
   `is_drive_mounted()` as a convenience method that returns `bool` (delegates to
   `drive_availability() == Available`).

2. **Use `findmnt -n -o UUID <mount_path>`** for UUID detection. Parse as single-line
   trimmed output. Handle missing binary, empty output, non-zero exit.

3. **Add `uuid: Option<String>` to `DriveConfig`** with `#[serde(default)]`. No config
   migration code — absent UUID means no verification (with warning).

4. **Surface UUID mismatch prominently** in skip reasons and plan output. This is not a
   normal skip — it's a safety event.

5. **Show detected UUID in the "no UUID configured" warning** so users can copy-paste it.

6. **Test with `MockFileSystemState`**: UUID match, UUID mismatch, UUID absent from config,
   `findmnt` failure, drive not mounted.

7. **Update example config** to show `uuid` field on one drive (commented out on others
   to show it's optional).

## Open Questions

1. **Should UUID mismatch be an error or a skip?** Currently proposed as skip (drive treated
   as unavailable). An alternative is to make the entire backup run fail-fast on UUID
   mismatch, since it indicates a potentially dangerous misconfiguration. Recommendation:
   skip with prominent warning, not fail-fast — other subvolumes on other drives should
   still be backed up.

2. **What if the same UUID appears on two config entries?** (Two `[[drives]]` with the same
   UUID but different labels.) This should be a config validation error — add to `validate()`.

3. **Interaction with `urd verify`:** Should `urd verify` check UUID consistency? It's a
   natural fit but can be a follow-up.
