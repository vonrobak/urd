# Runbook: Restarting the Sentinel

> **TL;DR:** The Sentinel is a stateless observer of the SQLite event log
> and the filesystem. It can be restarted at any time without losing data
> or backup history — every fact it cares about is persisted (filesystem
> is truth, SQLite is history; ADR-102). The only state that does not
> survive a restart is the in-memory circuit-breaker timer, which resets
> to closed.

**Audience:** Operators who suspect the Sentinel daemon has wedged, or
who need to apply a config change without a full reboot.

---

## When to restart

Restart the Sentinel when:

- `urd sentinel status` reports the daemon is not running (PID dead, state
  file stale).
- A `[notifications]` or `[sentinel]` config change needs to take effect
  (the Sentinel reads config at startup; live reload is not supported).
- Drive reconnect notifications stop firing, but `urd status` correctly
  reports `connected`. Suggests the Sentinel's mount-detection loop has
  stalled.
- The circuit breaker is stuck open and you want to give it a clean slate
  while you investigate the underlying cause.

Do **not** restart the Sentinel as a habit. If something feels wrong,
`urd doctor` first — see [doctor-walk runbook](doctor-walk.md).

---

## Procedure

### Diagnose first

```bash
urd sentinel status
systemctl --user status urd-sentinel.service    # if installed
journalctl --user -u urd-sentinel.service -n 50 --no-pager
```

The status output reports:

- Whether the daemon process is alive.
- Last activity timestamp.
- Circuit breaker state (closed / open with reason).
- Any pending actions queued.

If the process is dead, the systemd unit's `Restart=on-failure` should have
revived it. If it didn't, check `journalctl` for the crash reason — Urd's
philosophy is to fix the cause, not paper over it with a restart.

### Restart

If installed as a user service:

```bash
systemctl --user restart urd-sentinel.service
```

Then confirm:

```bash
sleep 2
urd sentinel status
journalctl --user -u urd-sentinel.service -n 20 --no-pager
```

You want to see lifecycle log lines (`warn`-level by convention so they
appear at default log levels) and the new PID matched in `urd sentinel status`.

If running ad-hoc (e.g., debugging in a terminal):

```bash
# Stop the running instance (Ctrl-C in its terminal, or kill its PID).
# Start fresh in foreground:
urd sentinel run --verbose
```

### Verify behavior

After restart:

1. `urd status` — should be unchanged. Promise states come from the
   filesystem and SQLite, not the Sentinel's memory.
2. Trigger a drive reconnect (or wait for the next idle poll) — the
   Sentinel should emit a notification on the next mount event.
3. Check the heartbeat freshness: `cat <heartbeat_file> | jq .timestamp`.
   The Sentinel does not write the heartbeat (the backup runner does), so
   this only changes on the next backup.

---

## What survives a restart

| State | Survives? | Source |
|-------|-----------|--------|
| Backup history | Yes | SQLite `runs`, `subvolume_results` |
| Promise states | Yes (recomputed from filesystem on next read) | `awareness.rs` |
| Pin files (chain parents) | Yes | Filesystem (`.last-external-parent-<LABEL>`) |
| Drive UUID adoption | Yes | SQLite `drive_identities` |
| Notification dispatch ledger | Yes | Heartbeat `notifications_dispatched` field |
| Structured event log | Yes | SQLite `events` |
| Circuit breaker state | **No** — resets to closed | In-memory only |
| Pending actions queue | **No** — recomputed from current state | In-memory only |

The circuit breaker's reset to closed is intentional. If the underlying
condition that tripped it persists, it will trip again immediately on the
next observation. A restart that "fixes" repeated tripping has masked a
real problem — investigate the structured event log:

```bash
urd events --kind sentinel --since 24h
```

---

## When restart does not help

If the symptom persists after restart, escalate to:

- `urd doctor --thorough` — full diagnostic battery, including thread
  verification ([doctor-walk runbook](doctor-walk.md)).
- Inspect the events log: `urd events --kind sentinel --limit 100`.
- Read the journals: `journalctl --user -u urd-sentinel.service --since "1 day ago"`.
- Check for filesystem-level problems (`btrfs filesystem show`, `dmesg | tail`).

The Sentinel is a thin observer; persistent misbehavior usually points at
something below it — a wedged mount, a permission change, a stale lock.

---

## Related

- [Drive rotation runbook](drive-rotation.md)
- [Doctor walk runbook](doctor-walk.md)
- [CLI reference](../../20-reference/cli.md) — `urd sentinel run`, `urd sentinel status`, `urd events`
- [ADR-102 — Filesystem is truth, SQLite is history](../../00-foundation/decisions/2026-03-24-ADR-102-filesystem-truth-sqlite-history.md)
