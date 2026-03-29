# Design: P1+P2+P9 — Rich Progress Display

**Status:** proposed
**Date:** 2026-03-29
**Source:** [Progress and Plan Display Brainstorm](2026-03-29-progress-and-plan-display.md)

## Summary

Replace the anonymous byte counter progress display with a context-aware display showing
current subvolume name, "2/6 sends" counter, and permanent completion lines that accumulate
above the live progress line. The core mechanism is a new `Arc<Mutex<ProgressContext>>`
shared between the executor (writer) and the progress thread (reader), extending the
current `Arc<AtomicU64>` byte counter without replacing it.

Target display:
```
  ✓ htpc-home → WD-18TB: 53.2 GB in 4:58 (full)
  ✓ subvol3-opptak → WD-18TB: 3.8 TB in 1:42:00 (full)
  [3/6] subvol2-pics: 23.1 GB @ 178.3 MB/s  [2:10]
```

## Module Changes

### `src/commands/backup.rs` — primary changes

Define a module-private `ProgressContext` struct:

```rust
use crate::executor::SendType;

struct ProgressContext {
    subvolume_name: String,
    drive_label: String,
    send_type: SendType,        // Full or Incremental (from executor.rs)
    send_index: u32,            // 1-based, updated by executor before each send
    total_sends: u32,           // computed from plan before execution
}
```

**Note (arch-adversary P1 item 3):** Uses the existing `SendType` enum from `executor.rs`
(`Full`, `Incremental`, `NoSend`) instead of a raw `&'static str`. The `NoSend` variant
is never set in `ProgressContext` since the progress display only activates during sends.

Create `Arc<Mutex<ProgressContext>>` alongside existing `bytes_counter`. Pass clone to
progress thread and to `Executor`. Rewrite `progress_display_loop` to read from both
the atomic counter and the mutex-protected context.

**State machine in progress thread (four states):**
1. **Idle:** Counter is 0, no active send. Continue polling.
2. **Active:** Counter > 0. On 0→non-zero transition, read `ProgressContext` via mutex,
   store locally. Display live progress line with `\r` overwrite.
3. **Completing:** Counter resets to 0 after having been non-zero (next send starting).
   Print permanent completion line via `eprintln!`, then transition back to idle/active.
4. **Shutdown (arch-adversary M2):** Shutdown flag is set. If `last_display_bytes > 0`,
   immediately print the final completion line with last known context before thread exit.
   This handles the last send, whose counter never resets to 0. The executor signals
   shutdown after all sends complete; the progress thread must not wait for a counter
   reset that will never come.

The ~1s gap between sends (snapshot creation time) is acceptable: the progress thread
shows the previous send's final byte count as a stale line briefly, then detects the
reset when the next `send_receive()` starts.

Extract formatting into pure functions for testability:
- `format_progress_line(ctx, bytes, rate, elapsed) -> String`
- `format_completion_line(ctx, bytes, elapsed) -> String`

### `src/executor.rs` — light changes

Add `progress_context: Option<Arc<Mutex<ProgressContext>>>` to `Executor` struct.
Update `Executor::new()` to accept it. Before each `self.btrfs.send_receive()` call in
`execute_send()`, lock the mutex and update with current subvolume name, drive label,
send type, and incremented send index.

The `Option` wrapper means tests pass `None` — no behavior change for the executor's
core logic.

### No changes to

`types.rs`, `output.rs`, `voice.rs`, `plan.rs`, `btrfs.rs`. The progress display is
purely a runtime TTY concern.

## Data Flow

1. `backup.rs::run()` counts sends from `backup_plan.summary().sends`.
2. Creates `Arc<Mutex<ProgressContext>>` with `total_sends` set, other fields empty.
3. Passes `Arc` clone to progress thread and to `Executor::new()`.
4. Executor, in `execute_send()`, locks mutex, writes context, increments `send_index`.
5. `btrfs.rs::send_receive()` resets byte counter to 0, then starts counting (unchanged).
6. Progress thread polls at 250ms:
   - On 0→non-zero: reads `ProgressContext`, stores locally for duration of this send.
   - During transfer: displays live line using local context copy + current bytes.
   - On non-zero→0: prints permanent completion line, resets local state.
7. On shutdown: prints final completion line if send was active.

## Test Strategy

- **Formatting functions (pure, testable):** format_progress_line and
  format_completion_line — zero rate, hours-long elapsed, full vs incremental label,
  index/total rendering, long subvolume names. ~8 tests.
- **Send counting:** Verify `BackupPlan::summary().sends` (likely already covered). ~1 test.
- **Progress context sequencing:** Mock executor with 3 subvolumes, verify context
  updates before each send via `Option<Arc<Mutex<ProgressContext>>>`. ~3 tests.
- **No TTY tests.** The actual `eprint!` calls are untestable in unit tests.

**Estimated: 10-12 tests.**

## Effort Estimate

Similar to UUID fingerprinting: one module extended (`backup.rs`), one lightly changed
(`executor.rs`), ~12 tests. Progress thread rewrite is medium complexity (three-state
machine). **One session.**

## Dependencies

None. Can be implemented first. Feature 4 (ETA) extends this design.

## Risks

**Mutex contention:** Negligible. Executor locks once per send (minutes apart). Progress
thread reads once per 250ms poll on state transitions only.

**Mutex poisoning:** Use `lock().unwrap_or_else(|e| e.into_inner())` in progress thread
to recover from panics in executor thread.

**Last-send completion:** Must be handled in shutdown cleanup, not via counter reset.

## Alternatives Rejected

- **Channel-based:** `mpsc::channel` with `ProgressEvent` messages. Cleaner in theory
  but adds complexity: progress thread must manage receiver alongside timer-based poll
  (`select!` or `try_recv`). Mutex piggybacks on existing poll loop.
- **Multiple atomics:** Subvolume names don't fit in `AtomicU64`. An `AtomicUsize` index
  into a pre-built list was considered but is fragile.

## Ready for Review

Focus on:
1. **Completion detection:** Counter resets at START of next send, not end of current.
   Last send never resets — shutdown cleanup must handle it.
2. **Executor lifetime:** Adding `Arc<Mutex<>>` alongside borrowed references. `Arc` is
   `'static`, should be fine, but verify.
3. **Thread safety:** Progress thread must not hold mutex across sleep.
