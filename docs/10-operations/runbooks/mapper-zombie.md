# Runbook: "File exists" on Re-unlocking a LUKS Backup Drive

> **TL;DR:** When a removable LUKS drive is unplugged and reconnected, the
> re-unlock can fail with `Failed to activate device: File exists`. The
> cause is almost never Urd or BTRFS — it's that monitoring containers
> running on the host have recursively bind-mounted the host's mount tree
> into their namespaces, so the device-mapper node from the previous
> session is still pinned. Restart the offending container(s) and unlock
> normally.

**Audience:** Operators on hosts that run monitoring/observability
containers (Prometheus node_exporter, cAdvisor, similar) which bind-mount
the host filesystem.

**Severity:** Operator-blocked, not data-at-risk. The drive's content is
intact; the problem is purely that the kernel can't activate the LUKS
mapper because something still holds a reference.

---

## Symptom

After reconnecting a LUKS-encrypted backup drive, attempting to unlock it
(via Files / `udisksctl` / `cryptsetup luksOpen`) fails with:

```
Failed to activate device: File exists
```

`lsof` and `fuser` against the underlying block device return nothing.
`cryptsetup close luks-<uuid>` and `dmsetup remove --force` both fail with
`Device or resource busy`. Re-inserting the drive does not help.

---

## Root cause

Monitoring containers (the canonical examples are `node_exporter` and
`cAdvisor`, but anything with a recursive `/host` or `/rootfs` bind-mount
qualifies) inherited the LUKS mapper's mount when the drive was first
unlocked. When the drive was unplugged, the host's `udisks` cleanly
unmounted its view, but the container's namespace still holds the mount
because:

1. The container was started with the host filesystem bind-mounted
   recursively (`--volume /:/host` or `--mount type=bind,source=/,target=/rootfs`).
2. Mount events that happen *after* container start propagate into the
   container only if the source mount has `shared` or `rshared`
   propagation. Unmount events do **not** propagate the same way — the
   container retains the mount in its private namespace.
3. The kernel's device-mapper subsystem refuses to activate a new mapper
   with a UUID that still has open holders, even if those holders are in
   another mount namespace.

The `cryptsetup`/`dmsetup` tools running on the host see "no holders" via
the host's `/proc` views and report success or `EBUSY` confusingly — they
can't see the container's namespace.

This is not a Urd issue or a LUKS issue. It is a side effect of the
monitoring stack's mount topology.

---

## Diagnosis (optional — confirm before acting)

```bash
# Identify processes pinning the mapper across all namespaces
sudo grep -lE "luks-<uuid>|<dm-major>:<dm-minor>" /proc/*/mountinfo 2>/dev/null
```

Where `<uuid>` is the LUKS UUID from the failure message and
`<dm-major>:<dm-minor>` (e.g. `252:0`) is its device-mapper number. The
PIDs that match are the holders. In practice they are the monitoring
containers' main processes.

This step is purely confirmatory — the fix is the same whether you
confirm or skip.

---

## Fix

Restart the container(s) that pin the mapper. For a typical Docker /
Podman setup:

```bash
# Replace with the actual container names on this host.
docker restart node_exporter cadvisor
# or:
podman restart node_exporter cadvisor
```

The host's deferred-removal logic fires once the holders release, and the
mapper auto-cleans. If it doesn't (rare), close it manually:

```bash
sudo cryptsetup close luks-<uuid>
```

Then unlock the drive normally (Files, `udisksctl`, etc.).

---

## What does NOT work

- **`umount -l` inside the container's namespace.** Most monitoring
  containers drop mount capabilities or run with a user-namespace mapping
  that refuses unmount. Returns `Permission denied`.
- **`nsenter -m -- umount`** from the host into the container's namespace.
  Same permission problem.
- **`cryptsetup close --force` on the host.** It can't see the
  cross-namespace holder; it either reports `EBUSY` or appears to succeed
  but the mapper persists.
- **Re-unlocking with a slightly different command.** This is a kernel
  refcount, not a userspace state issue.

Don't burn time on these. Restart the container.

---

## Prevention

Long-term, restrict the monitoring containers' mount propagation so host
mount events don't leak into them:

1. Mount the host bind with `slave` (not `shared`/`rshared`) propagation,
   so container mounts don't propagate back to the host but host mounts
   *also* don't propagate into the container's view of removable media.
2. Or, exclude `/run/media` (and any other removable-mount path) from the
   bind, so mounts under it never appear inside the container at all.

Either change is owned by the monitoring stack's configuration, not Urd.

---

## When to escalate

If the symptom appears without monitoring containers in the picture
(server-class system, no Prometheus stack, no recursive host bind-mounts),
the root cause is something else — a stuck filesystem-level holder, a
genuine kernel bug, or hardware. In that case:

- `dmesg | tail -100` — look for I/O errors, mapper warnings.
- `lsblk -f` — confirm the device topology.
- `systemctl status systemd-udevd` — udev sometimes wedges on rapid
  plug/unplug cycles; restarting it can help.

---

## Related

- [Drive rotation runbook](drive-rotation.md) — the normal connect/disconnect flow this runbook recovers from
- [Doctor walk runbook](doctor-walk.md) — for verifying everything is well after recovery
