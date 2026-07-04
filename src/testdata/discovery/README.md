# Discovery fixtures (UPI 070)

Real command captures from the Encounter staging lab host (Fedora Workstation 44,
native btrfs default layout, one external USB LUKS+btrfs drive), taken 2026-07-04
during encounter field test 01. Golden source for `src/discovery.rs` parser tests.

## Files

| file | command | scenario |
|------|---------|----------|
| `lsblk-locked.json` / `lsblk-unlocked.json` | `lsblk -J` | default columns (tolerance family) |
| `lsblk-full-locked.json` / `lsblk-full-unlocked.json` | `lsblk -J -o NAME,FSTYPE,LABEL,UUID,MOUNTPOINTS,RM,HOTPLUG,TRAN,SIZE` | **the production column set** |
| `lsblk-f-locked.json` / `lsblk-f-unlocked.json` | `lsblk -f -J` | filesystem view (tolerance family) |
| `findmnt-locked.json` / `findmnt-unlocked.json` | `findmnt -J` | full mount tree (tolerance family) |
| `findmnt-btrfs-unlocked.json` | `findmnt -t btrfs -J` | **the production invocation** |

`-locked` vs `-unlocked`: whether the external USB drive's LUKS container is open.
Locked: the `crypto_LUKS` partition has no children and no mountpoints. Unlocked: it
gains a `luks-*` mapper child (`fstype: btrfs`, label `urd-test`) mounted under
`/run/media/`.

## Sanitization

These files are sanitized copies — do not edit by hand; do not add new captures
without the same pass. Policy (the mapping itself is not recorded anywhere):

- every filesystem/LUKS UUID and volume serial replaced with a visibly-patterned
  synthetic value (`11111111-1111-4111-…`, `AAAA0000AAAA0001`, …), applied as raw
  string substitution across **all files at once** so the lsblk↔findmnt coupling
  survives (`uuid` field ↔ `luks-<uuid>` node name ↔ `/dev/mapper/luks-<uuid>`
  source ↔ systemd `\x2d`-escaped unit paths);
- the username in `/run/media/<user>/…` replaced with `user`;
- no hostnames appeared in the captures (verified); formatting is byte-identical to
  the original `lsblk`/`findmnt` output otherwise — the as-captured layout is part
  of what the tolerance tests prove.

The `discovery.rs` end-to-end test joins `lsblk-full-unlocked.json` with
`findmnt-btrfs-unlocked.json`; a fixture edit that breaks the UUID coupling fails
that test loudly.
