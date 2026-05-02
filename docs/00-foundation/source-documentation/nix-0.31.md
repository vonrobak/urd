# nix 0.31 Reference

> Urd dependency: `nix = { version = "0.31", features = ["fs"] }`
> Previous: `nix = "0.29"`

## Breaking Changes (0.29 -> 0.31)

### 0.30.0 — Major I/O Safety Migration

The biggest change. Public interfaces across nearly every module switched from
`RawFd` to I/O-safe types (`AsFd`/`OwnedFd`/`BorrowedFd`):

- `fcntl.rs` — all functions now take `AsFd` instead of `RawFd`
- `dir.rs` — I/O-safe types
- `sys/signal`, `sys/stat`, `unistd`, `sys/fanotify` — all migrated

**If code passes raw file descriptors to nix functions, wrap them in `OwnedFd`/
`BorrowedFd` or use types that implement `AsFd` (like `std::fs::File`).**

Other 0.30 breaking changes:
- `IpTos` renamed to `Ipv4Tos`
- `EventFlag` renamed to `EvFlags`
- `MemFdCreateFlag` renamed to `MFdFlags`
- `recvmsg` takes slice for `cmsg_buffer` instead of `Vec`
- `Copy` removed from `PollFd`
- `EventFd::defuse()` removed

### 0.31.0

- `Eq`/`PartialEq` removed from `SigHandler`
- `EpollEvent` methods are now `const`
- libc bumped to 0.2.180

## File Locking API (fcntl module)

Urd uses nix for file locking in `lock.rs`. Two mechanisms:

### Flock (RAII — recommended)

```rust
use nix::fcntl::{Flock, FlockArg};
use std::fs::File;

let file = File::open("lockfile")?;
let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock)
    .map_err(|(_, errno)| errno)?;

// Lock held until dropped
lock.relock(FlockArg::LockShared)?;   // upgrade/downgrade (since 0.29)
let file = lock.unlock()?;             // explicit unlock
```

`Flockable` is implemented for `std::fs::File` and `OwnedFd`.

`FlockArg` variants: `LockShared`, `LockExclusive`, `Unlock`,
`LockSharedNonblock`, `LockExclusiveNonblock`.

### POSIX Record Locks (fcntl)

```rust
use nix::fcntl::{fcntl, FcntlArg};

// FcntlArg variants for locking:
// F_SETLK(&libc::flock)      — set/clear, non-blocking
// F_SETLKW(&libc::flock)     — set/clear, blocking
// F_GETLK(&mut libc::flock)  — query locks
// F_OFD_SETLK, F_OFD_SETLKW, F_OFD_GETLK — open file description locks (Linux)
```

## Deprecations

- `flock()` function — deprecated since 0.28, use `Flock` struct instead
- `FlockArg::UnlockNonblock` — deprecated since 0.28, use `FlockArg::Unlock`

## Urd-Specific Notes

Urd uses nix only for file locking (`nix::fcntl` via the `fs` feature).
The I/O safety migration in 0.30 is the main concern — `std::fs::File`
implements `AsFd`, so if Urd passes `File` objects to nix functions (which it
should), the migration is transparent. If any code passes `RawFd` integers,
those calls need updating to use `AsFd`-implementing types.
