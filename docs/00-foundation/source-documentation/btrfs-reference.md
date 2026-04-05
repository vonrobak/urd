# BTRFS Reference for Backup Tooling

> Based on btrfs-progs documentation through v6.12+ (training data).
> BTRFS is a stable, slow-moving interface — commands used by Urd have been
> unchanged for years.

## Snapshot Operations

### Create Read-Only Snapshot

```bash
sudo btrfs subvolume snapshot -r <source> <dest>
```

- `-r` creates read-only snapshot (required for send)
- Snapshot is instantaneous (COW metadata operation)
- Snapshot appears as a regular directory at `<dest>`

### Delete Snapshot/Subvolume

```bash
sudo btrfs subvolume delete <path>
```

- Deletion is asynchronous — space reclamation happens in background
- `sync` or `btrfs filesystem sync` to force cleanup
- `btrfs subvolume delete -c` commits after deletion (ensures space is freed)

### Show Subvolume Info

```bash
sudo btrfs subvolume show <path>
```

Returns: UUID, parent UUID, creation time, send/receive status, flags.

## Send/Receive Pipeline

### Full Send

```bash
sudo btrfs send <snapshot> | sudo btrfs receive <dest_dir>
```

### Incremental Send (with parent)

```bash
sudo btrfs send -p <parent_snapshot> <snapshot> | sudo btrfs receive <dest_dir>
```

- Parent must exist at destination (or a clone of it)
- Incremental sends transfer only the delta — dramatically faster and smaller
- **The parent snapshot must not be deleted** until the child is successfully received

### Best Practices

- Always use `-p` (parent) for incremental sends when a common parent exists
- Capture stderr from both `send` and `receive` — either side can fail
- Check exit codes of both sides of the pipe
- Clean up partial snapshots on receive failure (incomplete receive leaves a
  directory that isn't a valid subvolume)
- Pipe through `pv` for progress monitoring on large sends (optional)

### Common Failure Modes

| Failure | Cause | Recovery |
|---------|-------|----------|
| "cannot find parent subvolume" | Parent deleted or not present at dest | Full send required |
| Partial receive | Interrupted pipe, disk full, I/O error | Delete partial dir, retry |
| "received UUID already exists" | Duplicate snapshot at destination | Delete existing, re-receive |
| Permission denied | Missing sudo or capability | Check sudoers configuration |

## Space Management

### Check Filesystem Usage

```bash
sudo btrfs filesystem usage <mount>
sudo btrfs filesystem df <mount>
sudo btrfs filesystem show <mount>
```

- `usage` gives the most complete picture (data, metadata, system, unallocated)
- `df` shows allocated/used per profile (single, DUP, RAID1, etc.)
- `show` lists devices and total/used per device

### Space Considerations for Backup Tools

- Snapshots share data blocks via COW — initial space cost is near zero
- Space grows as source diverges from snapshot (modified blocks are unique)
- Many snapshots of a rapidly changing subvolume can consume significant space
  (each snapshot pins its unique blocks from garbage collection)
- `btrfs filesystem sync` after deletions to ensure space is reclaimed
- Monitor unallocated space, not just used space — BTRFS allocates in chunks
- **Metadata space exhaustion** is harder to recover from than data space exhaustion

### Quota Groups (qgroups)

```bash
sudo btrfs qgroup show <mount>
sudo btrfs quota enable <mount>
```

- Can track per-subvolume space usage
- Significant performance overhead — not recommended for general use
- Most backup tools avoid qgroups and estimate sizes via send dry-run or
  `btrfs filesystem du`

## Urd-Specific Notes

Urd wraps these commands via the `BtrfsOps` trait in `btrfs.rs`. Key conventions:
- All paths passed as `&Path` to `Command::arg()` — never stringified
- Stderr captured from both sides of send|receive pipe
- Both exit codes checked
- Partial snapshots cleaned up on receive failure
- Pin files (`.last-external-parent-{DRIVE}`) protect incremental chain parents
