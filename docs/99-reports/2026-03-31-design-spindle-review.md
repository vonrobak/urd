# Design Review: Spindle Tray Icon (v1)

**Reviewed:** [docs/95-ideas/2026-03-31-design-spindle-tray-icon.md](../95-ideas/2026-03-31-design-spindle-tray-icon.md)
**Reviewer:** arch-adversary
**Date:** 2026-03-31

---

## Scores

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 7/10 | Core data flow is sound but Active icon override creates a lying-icon window; lock probe uses wrong nix API for current dependency features; inotify event type for atomic rename not verified |
| Security | 9/10 | No privilege escalation in Spindle; "Back Up Now" delegates to existing `urd backup`; no new attack surface beyond spawning a child process |
| Architectural Excellence | 8/10 | Clean separation via state file; Spindle-local stale detection is elegant; but Cargo dependency scoping has a real gap, and AnimationState is premature |
| Systems Design | 7/10 | Good process model and deployment story; but double-click on "Back Up Now" is unhandled, stale threshold math needs refinement, and GNOME tray viability has a hard external dependency |

**Overall: 7.75/10** -- Strong design with several findings that need resolution before implementation.

---

## Finding 1: Active icon override creates a lying-icon window (SEVERITY: HIGH)

**Focus area 2 from the design.**

The design says: "If backup is running, override icon to Active regardless of promise/health state." This is the single most dangerous decision in the proposal.

**Scenario:** A backup is running, but there is also a critical data gap (e.g., one subvolume is UNPROTECTED because its drive has been away for weeks). The user sees "Active" (blue, backup running) and thinks "everything is being handled." But the running backup may not fix the data gap -- the drive might still be unmounted for that subvolume. The Active icon suppresses the Critical signal for the entire duration of the backup.

**How close to catastrophic failure:** This IS the lying icon. A user who sees Active and assumes things are being resolved will skip the manual check that would reveal the data gap. The design's own principle 4 says "the icon must not lie."

**Recommendation:** Active should overlay, not override. Two options:

(a) **Composite state.** When `backup_active && worst_safety == Critical`, show Critical icon with a secondary "active" indicator (tooltip shows "Backup running -- 1 subvolume UNPROTECTED"). The worst safety signal always wins the icon color. Activity is communicated through tooltip text or a badge.

(b) **Priority inversion.** Active overrides Ok and Warning, but never overrides Critical. If `backup_active && icon == Critical`, the icon stays Critical and the tooltip adds "Backup in progress." This is simpler and preserves the safety signal.

Option (b) is the minimum viable fix. It adds one conditional to `compute_visual_state` and eliminates the lying-icon scenario entirely.

---

## Finding 2: Lock probe uses `nix::sys::signal::kill` but nix is compiled with only `features = ["fs"]` (SEVERITY: HIGH)

**Focus area from code verification.**

The design's lock probe code calls `nix::sys::signal::kill(Pid, None)` to check if a PID is alive. But `Cargo.toml` shows:

```toml
nix = { version = "0.29", features = ["fs"] }
```

The `kill` function requires the `signal` feature in nix 0.29. This code will not compile without adding `features = ["fs", "signal"]`.

However, the existing codebase already solves this problem differently. `sentinel_runner.rs` line 634 defines `is_pid_alive(pid)` using `/proc/{pid}` existence checks -- no signal feature needed. The design should use this existing pattern instead of introducing a new dependency on `nix::sys::signal`.

**Recommendation:** Replace the `nix::sys::signal::kill` approach in the lock probe with the existing `is_pid_alive()` function from `sentinel_runner.rs`:

```rust
fn is_backup_running(lock_path: &Path) -> bool {
    lock::read_lock_info(lock_path)
        .is_some_and(|info| is_pid_alive(info.pid))
}
```

This is simpler, already tested, and requires no dependency changes.

---

## Finding 3: Cargo does not scope dependencies per `[[bin]]` (SEVERITY: HIGH)

**Focus area 5 and 9 from the review instructions.**

The design proposes `src/bin/spindle.rs` as a second binary and says "Consider a cargo feature `spindle` to avoid pulling ksni/notify into the main `urd` binary. Or accept the dependency since they're small."

This needs a definitive answer, not a deferred consideration. Cargo dependencies in `[dependencies]` are linked into ALL binaries in the crate. Adding `ksni` and `notify` to `Cargo.toml` means every `cargo build` of `urd` will compile and link these crates, even though `urd` never uses them. This adds:

- D-Bus client libraries (ksni depends on zbus or dbus)
- inotify bindings (notify crate)
- Additional compile time for every build
- Additional binary size for the `urd` binary (unless LTO eliminates them, which is not guaranteed for all codepaths)

More critically, ksni pulls in an async runtime or D-Bus event loop. Adding this to a binary that runs as a headless systemd timer is architecturally wrong.

**Recommendation:** A cargo feature flag is not optional -- it is required. The feature flag should gate both the dependencies and the `[[bin]]` entry:

```toml
[features]
spindle = ["dep:ksni", "dep:notify"]

[dependencies]
ksni = { version = "0.3", optional = true }
notify = { version = "7", optional = true }
```

Alternatively, consider a workspace with two crates: `urd` (the existing binary) and `urd-spindle` (the tray icon). This is cleaner than feature flags for binary separation and avoids the `urd` binary ever seeing ksni in its dependency tree.

---

## Finding 4: Inotify event type for atomic rename needs verification (SEVERITY: MEDIUM)

**Focus area 7 from the review instructions.**

The design states: "Inotify `MOVED_TO` event should fire on rename. Need to verify." This is listed as an assumption (assumption 2) but left unverified.

The behavior depends on which inotify events the `notify` crate subscribes to. The `notify` crate (v7) uses `inotify` under the hood and typically watches for `CREATE`, `MODIFY`, `DELETE`, `MOVED_FROM`, `MOVED_TO`, and `CLOSE_WRITE`. An atomic rename (`std::fs::rename`) from `sentinel-state.json.tmp` to `sentinel-state.json` should produce a `MOVED_TO` event on the parent directory.

However, there is a subtlety: the watcher must be watching the *directory*, not the file itself. If Spindle watches `sentinel-state.json` directly, the inotify watch is on the inode. After rename, the old inode (the `.tmp` file) is now at the target path, and the original inode is gone. Depending on notify crate behavior, this may or may not work correctly.

**Recommendation:** Write a test before implementing. The test should:
1. Set up a `notify` watcher on a temp directory
2. Write a file via tmp+rename (matching sentinel_runner's pattern)
3. Assert that an event fires
4. Verify which event type fires and that the content is readable

If the notify crate requires watching the directory (not the file), the design's watcher.rs module must watch the parent directory and filter for the state file name.

---

## Finding 5: Stale detection threshold of 3x may be too lenient (SEVERITY: MEDIUM)

**Focus area 3 from the design.**

With the default tick interval of 120 seconds (2 minutes), `3 * tick_interval_secs` = 6 minutes. But the sentinel also writes on events (drive mount/unmount), so during normal operation the file updates more frequently than every 2 minutes.

The real question is: what is the maximum acceptable "lie time" -- how long can a green icon persist after the sentinel crashes?

- At 3x (6 minutes): if the sentinel crashes right after writing, the icon lies for up to 6 minutes. This seems acceptable for normal health states.
- At 2x (4 minutes): tighter, but may cause false stale during slow assessment ticks (assessment involves btrfs subvolume show + filesystem show for every subvolume, which can take 10-30 seconds on loaded systems).

**The real issue:** The threshold uses `tick_interval_secs` from the state file itself. If the sentinel crashes, the last-written `tick_interval_secs` is the value Spindle uses. This is correct. But if the sentinel *hangs* (stuck in a btrfs call), it never updates `last_assessment`, and Spindle correctly shows stale after 3x. Good.

**Recommendation:** 3x is defensible for v1. Add a configurable override in the Spindle CLI args (`--stale-threshold-multiplier`) so users with slow filesystems can tune it. Document that the threshold is `3 * tick_interval_secs` and why.

---

## Finding 6: Double-click on "Back Up Now" is unhandled (SEVERITY: MEDIUM)

**Focus area 6 from the review instructions.**

The design says: "Spawns `urd backup` as a child process (detached). Spindle does NOT track the child." And: "If a backup is already running (icon is Active), the menu item is greyed out."

Race condition: the user clicks "Back Up Now." Spindle spawns `urd backup`. But `urd backup` needs time to acquire the lock and for the sentinel to detect the lock and write Active to the state file. During this window (potentially 2+ minutes until next sentinel tick), the icon still shows the previous state (e.g., Ok). The user clicks "Back Up Now" again. Now two `urd backup` processes are racing for the lock.

The second `urd backup` will fail with "Another urd backup is already running" -- which is correct behavior from Urd's perspective. But from Spindle's perspective, the user gets an error from a process they can't see (Spindle spawned it detached and doesn't track it).

**Recommendation:** Spindle should implement a local cooldown after spawning `urd backup`:

- After clicking "Back Up Now," immediately grey out the menu item for 30 seconds (or until the icon changes to Active, whichever comes first).
- Display the menu item as "Backup starting..." during the cooldown.
- This is a UI-only guard -- the real concurrency protection remains in Urd's lock file.

---

## Finding 7: "Back Up Now" error reporting is invisible (SEVERITY: MEDIUM)

**Related to Finding 6.**

If `urd backup` fails (config error, permission error, disk full), the error goes to the detached process's stderr, which nobody sees. Spindle spawned it and forgot about it. The user clicked "Back Up Now," the icon never changed to Active, and they have no idea what happened.

**Recommendation:** Either:

(a) Track the child process and capture its exit code. If non-zero after a reasonable timeout, show a desktop notification: "Backup failed to start." This violates the "don't track the child" principle but provides basic error visibility.

(b) Accept the limitation for v1 and document it: "Back Up Now" is fire-and-forget. The sentinel's next assessment will surface any resulting state changes. Users who want feedback should run `urd backup` in a terminal.

Option (b) is acceptable for v1 if the design acknowledges the gap.

---

## Finding 8: AnimationState struct is premature (SEVERITY: LOW)

**Focus area 8 from the review instructions.**

The design defines `AnimationState` with fields (`frames`, `delays`, `current_frame`, `active`) and `KICK_DELAYS_MS` -- none of which are used in v1. The design says "architecture first, animation later" but then defines concrete types for the deferred feature.

This violates YAGNI. When v2 animation is built, the requirements may have changed (different frame count, different trigger model, different timing). The struct defined now may not match what's actually needed.

**Recommendation:** Remove `AnimationState` and `KICK_DELAYS_MS` from the v1 design. Keep the prose description of the animation architecture in the v2 roadmap section. When v2 is designed, define the types then.

---

## Finding 9: ksni + GNOME requires AppIndicator extension (SEVERITY: LOW, but blocking for some users)

**Focus area 4 from the review instructions.**

GNOME removed native tray icon support in GNOME Shell 3.26 (2017). The AppIndicator/KStatusNotifierItem extension (`appindicatorsupport`) re-adds it and ships as a bundled extension in most distros (Fedora, Ubuntu). However:

- It must be enabled by the user (GNOME Extensions app or `gnome-extensions enable appindicatorsupport@RaphaelRochet`)
- Some minimal GNOME installs may not include it
- GNOME's extension API breaks on major releases (GNOME 45 broke many extensions)

ksni speaks the StatusNotifierItem D-Bus protocol, which is what the extension listens for. This is the correct protocol. The dependency is on the GNOME extension, not on ksni.

**Recommendation:** This is acceptable -- the same dependency exists for every Linux tray application on GNOME. Document it clearly:

- Installation docs: "GNOME users need the AppIndicator extension enabled"
- Spindle startup: if D-Bus registration fails, log a clear error message naming the extension
- Don't try to auto-install the extension -- that's a user choice

---

## Finding 10: Sentinel state file lacks backup trigger source (SEVERITY: LOW)

**Focus area 1 from the design.**

The tooltip example shows "Started 3 minutes ago (timer)" -- but the state file has no field for backup trigger source. The `LockInfo` struct has a `trigger` field, but the design says Spindle reads only `sentinel-state.json`, never the lock file (principle 2).

The tooltip data sources table acknowledges this: "Backup trigger | Lock file info (from sentinel, if added to state file)." This is an honest gap, but it means the "Backup running" tooltip cannot show the trigger source in v1.

**Recommendation:** Either:

(a) Add a `backup_trigger: Option<String>` field to `SentinelStateFile` (populated when the sentinel detects `Active` state and reads the lock info). This keeps the single-interface principle intact.

(b) Accept the gap: show "Backup in progress" without the trigger source in v1. The trigger source is nice-to-have, not essential.

Option (a) is clean and small. The sentinel already has access to `read_lock_info()`.

---

## Finding 11: Systemd unit should NOT use `BindsTo=urd-sentinel.service` (SEVERITY: LOW)

The brainstorm document (idea 12) proposed `BindsTo=urd-sentinel.service`. The design document correctly changed this to `After=urd-sentinel.service` without `BindsTo`, and notes: "Does NOT bind to sentinel -- Spindle handles missing/stale state gracefully."

This is the right call. Confirmed: no issue here. The design correctly handles sentinel absence through stale detection rather than systemd coupling.

---

## Finding 12: SpindleIcon duplicates VisualIcon with one addition (SEVERITY: LOW)

The design defines `SpindleIcon` as a separate enum with the same four variants as `VisualIcon` plus `Stale`. This is fine for v1 but consider whether `VisualIcon` should gain a `Stale` variant in the future. If Spindle is the only consumer of `Stale`, keeping it Spindle-local is correct. If other consumers (future web dashboard, terminal status) also need stale detection, it belongs in `output.rs`.

**Recommendation:** Keep Spindle-local for v1. The stale concept is a consumer-side concern (derived from timestamp age), not a producer-side state.

---

## Summary of required changes before implementation

| # | Finding | Severity | Action |
|---|---------|----------|--------|
| 1 | Active overrides Critical = lying icon | HIGH | Active must not suppress Critical icon state |
| 2 | Lock probe uses unavailable nix feature | HIGH | Use existing `is_pid_alive()` instead |
| 3 | ksni pollutes headless binary | HIGH | Feature flag or workspace split is required, not optional |
| 4 | Inotify + atomic rename unverified | MEDIUM | Write verification test before relying on it |
| 6 | Double-click race on "Back Up Now" | MEDIUM | Add UI-side cooldown after spawn |
| 7 | Failed backup is invisible | MEDIUM | Acknowledge gap or track child exit code |
| 8 | AnimationState is premature | LOW | Remove from v1 design |

Findings 5, 9, 10, 11, 12 are informational or low-severity with clear paths forward.

---

## What the design gets right

- **Single-interface principle.** Spindle reads one file. This is a strong architectural boundary that will prevent coupling creep. The design correctly rejected "Spindle reads lock file directly" (alternative 4).

- **Process separation.** Keeping Spindle out of the sentinel is essential. UI crashes must never take down the backup monitoring daemon. The design correctly rejected embedding (alternative 2).

- **Stale detection as mandatory.** Many tray icon implementations skip this. The design treats it as a first-class concern, which directly addresses the lying-icon catastrophic failure mode.

- **Static icons first.** Deferring animation to v2 is the right call. The architecture doesn't preclude it, and v1 can ship and prove value without it.

- **Same repo, same identity.** Matches the Time Machine model and the user's UX philosophy. Spindle is Urd's face, not a companion app.
