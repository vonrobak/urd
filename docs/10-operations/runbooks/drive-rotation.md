# Runbook: Drive Rotation

> **TL;DR:** Removable backup drives can be disconnected and reconnected
> freely. Urd treats drive absence as deferred work, not failure. While a
> drive is `away`, sends queue; when it reconnects, the next scheduled
> backup catches up. The Sentinel emits a notification on reconnect so the
> operator knows catch-up is possible.

**Audience:** Operators rotating an external backup drive (e.g., taking it
offsite or swapping in a sibling).

---

## Mental model

A configured drive lives in one of two states (see
[glossary](../../00-foundation/glossary.md#drive-states)):

- **`connected`** — mounted, Urd can read and write it now.
- **`away`** — not currently mounted. Urd defers operations targeting it.

`away` is *physical absence*, not data staleness. The duration shown by
`urd status` is time since last disconnection, not time since last
successful send.

---

## Procedure

### Disconnecting

1. Quiesce backups (optional). If a backup is currently writing to the drive,
   `urd status` will show the run in progress. Wait for it, or accept that
   the in-flight send will fail and be retried on the next run.

2. Unmount via your desktop / shell (`udisksctl unmount` or eject from the
   file manager). The drive cleanly detaches.

3. Confirm:

   ```bash
   urd status
   ```

   The drive's row will show `away` with a fresh "since" duration. Subvolumes
   that depended only on this drive may transition to `AT RISK` or
   `UNPROTECTED` — that is expected.

### While the drive is away

- **Backups continue locally.** Snapshot creation never depends on an external
  drive. Local promise states stay current.
- **Sends are deferred.** The planner records `send_type = 3` (deferred) for
  subvolumes whose target drive is away. `backup_send_type{subvolume="..."} 3`
  shows up in metrics.
- **No catch-up retries.** Urd does not poll for the drive between scheduled
  runs. The Sentinel notices reconnection in real time but does not trigger
  backups itself.

### Reconnecting

1. Plug the drive in and unlock / mount it the way you normally would
   (Files, `udisksctl`, `cryptsetup` for LUKS).

2. The Sentinel detects the mount and dispatches a "drive reconnected"
   notification (configurable via `[notifications]`).

3. Confirm:

   ```bash
   urd status
   ```

   The drive shows `connected`. Subvolumes that defer to this drive will
   show their pending send count and updated promise states.

4. Run a backup or wait for the scheduled timer:

   ```bash
   urd backup        # immediate; runs every subvolume that has work
   ```

   Or let the systemd timer pick it up at the next scheduled fire. Catch-up
   is incremental whenever the chain is intact (the pin file from the last
   successful send still points to a snapshot present on both sides).

### When the chain breaks

If the drive was away long enough that the local pin parent has been pruned
(or the drive's last received snapshot has been deleted offline), the
incremental chain is broken. The next send will be a **full send**.

In autonomous mode (`--auto`, used by the systemd timer), full sends due to
chain breaks are **skipped** to avoid surprising the operator with a
multi-TB transfer. Re-run with `--force-full` to allow them:

```bash
urd backup --force-full --subvolume <name>
```

`urd plan` previews the same decision without executing.

---

## What `urd status` shows

| Drive state | Subvolume promise | Likely meaning |
|-------------|-------------------|----------------|
| `connected`, recent send | `PROTECTED` | Normal. |
| `away`, recent send | depends on level | Drive is offsite or swapped; data on it is current. |
| `away`, stale send | `AT RISK` / `UNPROTECTED` | Drive has been away long enough that the protection level's freshness threshold lapsed. |
| `connected`, no recent send | `AT RISK` / `UNPROTECTED` | The drive is here but Urd hasn't sent yet (just reconnected; first send pending). |

---

## Common questions

**Q: Will Urd back up immediately when I plug a drive in?**
No. The Sentinel notifies you, but it does not run backups. Either run
`urd backup` manually or wait for the next timer fire.

**Q: Can I rotate between two drives with the same configured slot?**
Yes — give each drive a distinct label in `[[drives]]`. Each maintains its
own pin file (`.last-external-parent-<LABEL>`), so the chains are independent.

**Q: Is it safe to power-cycle the drive mid-send?**
Sends are atomic at the snapshot level: a partial receive is cleaned up by
the executor on failure (ADR-107). The chain stays intact because the pin
file is only updated after a confirmed-complete send.

**Q: How do I know when catch-up is done?**
`urd status` shows `PROTECTED` once every subvolume's send has caught up.
Metrics: `backup_last_success_timestamp{subvolume="...",location="external"}`
will reflect the post-reconnect run.

---

## Related

- [Sentinel restart runbook](sentinel-restart.md) — if the reconnect notification doesn't fire
- [Mapper zombie runbook](mapper-zombie.md) — when re-unlocking a LUKS drive fails with "File exists"
- [Doctor walk runbook](doctor-walk.md) — when something feels wrong after rotation
- [CLI reference](../../20-reference/cli.md) — `urd status`, `urd backup`, `urd plan`
