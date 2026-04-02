# Design: Phase 2 — Independent UX Commands

> **TL;DR:** Three independent capabilities built in parallel: (2a) `urd` with no subcommand
> shows a live one-sentence status, (2b) `urd doctor` composes existing diagnostics into a
> unified health check, (2c) `urd completions` generates shell tab-completion scripts. All
> three are high-scored UX features that require Phase 1 vocabulary to be landed.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** Phase 1 (vocabulary landing)

---

## 2a: `urd` Default One-Sentence Status

### Problem

When a user types `urd` with no arguments, clap prints help text. The most common question
is "is my data safe?" and the most natural invocation is the bare command. This should
answer that question with a single, fresh sentence — not cached sentinel state.

### Proposed Design

**CLI change:** Make subcommand optional.

```rust
// src/cli.rs
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,  // was: Commands (required)
    // ...
}
```

**Main dispatch:** `src/main.rs` — add `None` arm.

```rust
match cli.command {
    None => {
        // First-time path: detect missing config
        let config = match config::Config::load(config_path) {
            Ok(c) => c,
            Err(_) => {
                println!("{}", voice::render_first_time(mode));
                return Ok(());
            }
        };
        commands::default::run(config, mode)?;
    }
    Some(cmd) => { /* existing dispatch */ }
}
```

**New file:** `src/commands/default.rs`

```rust
pub fn run(config: Config, mode: OutputMode) -> anyhow::Result<()> {
    let state = StateDb::open(&config.state_db_path).ok();
    let fs_state = RealFileSystemState::new(/* ... */);
    let assessments = awareness::assess(&config, now, &fs_state);
    // Apply offsite overlay if applicable
    let last_run = state.as_ref().and_then(|s| s.last_run().ok().flatten());
    let output = DefaultStatusOutput::from(assessments, last_run);
    print!("{}", voice::render_default_status(&output, mode));
    Ok(())
}
```

**New type:** `src/output.rs`

```rust
pub struct DefaultStatusOutput {
    pub total: usize,
    pub sealed_count: usize,
    pub waning_names: Vec<String>,
    pub exposed_names: Vec<String>,
    pub last_backup: Option<LastBackupInfo>,
    pub health_issues: Vec<String>,
}

pub struct LastBackupInfo {
    pub age_secs: i64,
    pub result: String,
}
```

**Voice rendering:** `src/voice.rs`

Interactive mode:
```
All sealed. Last backup 7 hours ago.
Run `urd status` for details, `urd --help` for commands.
```

```
3 of 9 sealed. htpc-root, subvol1-docs exposed.
Run `urd status` for details.
```

First-time (no config):
```
Urd is not configured yet.
Run `urd init` to get started, or see `urd --help`.
```

Daemon mode: JSON serialization of `DefaultStatusOutput`.

### Module Mapping

| File | Change |
|------|--------|
| `src/cli.rs` | `command: Commands` → `command: Option<Commands>` |
| `src/main.rs` | Add `None` dispatch arm with first-time detection |
| `src/output.rs` | Add `DefaultStatusOutput`, `LastBackupInfo` |
| `src/voice.rs` | Add `render_default_status()`, `render_first_time()` |
| `src/commands/default.rs` | New file — wire awareness to voice |
| `src/commands/mod.rs` | Add `pub mod default;` |

### Test Strategy (~10 new tests)

- `default_all_sealed()` — all PROTECTED → `"All sealed."`
- `default_some_exposed()` — mixed states → `"{N} of {M} sealed. {names} exposed."`
- `default_with_last_backup()` — includes `"Last backup N hours ago."`
- `default_no_last_backup()` — omits last backup line
- `default_health_issues()` — appends health summary
- `default_daemon_json()` — valid JSON
- `first_time_no_config()` — includes `urd init` guidance
- CLI parsing: bare `urd` parses to `None` subcommand

### Invariants

- Config loading for subcommands must not regress (the `Option<Commands>` change is structural)
- First-time path must not attempt awareness assessment (no config = nothing to assess)
- Default output must be fast — no extra I/O beyond what `awareness::assess()` already does

### Effort: 1 session

---

## 2b: `urd doctor`

### Problem

Diagnosing Urd's health requires running multiple commands: `urd init` (infrastructure),
`urd verify` (threads), `urd status` (promises). No single command says "check everything
and tell me what's wrong." `doctor` composes existing checks into a unified diagnostic.

### Proposed Design

**New command:** `urd doctor [--thorough]`

Fast by default (config + preflight + awareness). `--thorough` adds the verify pass
(filesystem scanning for pin files, orphan detection).

**New file:** `src/commands/doctor.rs`

```rust
pub fn run(config: Config, args: DoctorArgs, mode: OutputMode) -> anyhow::Result<()> {
    // 1. Config + preflight (pure, instant)
    let preflight = preflight::preflight_checks(&config);

    // 2. Infrastructure checks (I/O: path existence, drive mounts)
    let infra = init::collect_infrastructure_checks(&config, &btrfs);

    // 3. Awareness (pure + fs reads for snapshot discovery)
    let assessments = awareness::assess(&config, now, &fs_state);

    // 4. Verify (optional, --thorough only)
    let verify = if args.thorough {
        Some(verify::collect_verify_data(&config, &btrfs, &fs_state)?)
    } else {
        None
    };

    // 5. Assemble and render
    let output = DoctorOutput { preflight, infra, assessments, verify };
    print!("{}", voice::render_doctor(&output, mode));
    Ok(())
}
```

**Prerequisite refactor:** `init::collect_infrastructure_checks()` must be extracted from
`init::run()` as a `pub(crate)` function. Similarly, verify's data collection logic should
be extractable from `verify::run()`.

**New types:** `src/output.rs`

```rust
pub struct DoctorOutput {
    pub config_checks: Vec<DoctorCheck>,
    pub infra_checks: Vec<DoctorCheck>,
    pub assessments: Vec<StatusAssessment>,
    pub verify: Option<VerifyOutput>,
    pub verdict: DoctorVerdict,
}

pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,  // Ok, Warn, Error
    pub detail: Option<String>,
    pub suggestion: Option<String>,
}

pub enum DoctorVerdict {
    Healthy,
    Warnings(usize),
    Issues(usize),
}
```

**Voice rendering:** `src/voice.rs`

```
Checking Urd health...

  Config
    ✓ 9 subvolumes, 3 drives
    ✓ All protection levels achievable
    ⚠ retention window shorter than send interval for htpc-root

  Infrastructure
    ✓ State DB accessible
    ✓ Sentinel running (PID 366735)
    ✓ sudo btrfs available
    ✓ All snapshot roots writable

  Data safety
    ✓ 8 of 9 sealed
    ✗ htpc-root exposed — last backup 3 days ago
      → Connect a drive and run `urd backup`

  [Threads — run with --thorough]

1 warning, 1 issue. Run suggested commands to resolve.
```

With `--thorough`, the `[Threads]` section shows verify results inline.

### Module Mapping

| File | Change |
|------|--------|
| `src/cli.rs` | Add `Doctor(DoctorArgs)` variant, `DoctorArgs { thorough: bool }` |
| `src/main.rs` | Add dispatch arm |
| `src/output.rs` | Add `DoctorOutput`, `DoctorCheck`, `DoctorVerdict` |
| `src/voice.rs` | Add `render_doctor()` |
| `src/commands/doctor.rs` | New file |
| `src/commands/mod.rs` | Add `pub mod doctor;` |
| `src/commands/init.rs` | Extract `collect_infrastructure_checks()` as `pub(crate)` |
| `src/commands/verify.rs` | Extract data collection as `pub(crate)` if needed |

### Test Strategy (~12 new tests)

- `doctor_all_healthy()` — clean config, all sealed, no warnings
- `doctor_config_warnings()` — preflight issues surface
- `doctor_promise_issues()` — waning/exposed appear
- `doctor_with_thorough()` — verify section present
- `doctor_without_thorough()` — verify section absent, placeholder shown
- `doctor_verdict_healthy/warnings/issues()` — verdict logic
- `doctor_daemon_json()` — valid JSON
- Integration: init's infrastructure checks callable from doctor

### Invariants

- Doctor never modifies anything (diagnostic-only, ADR-100 spirit)
- `--thorough` verify logic must not duplicate `commands/verify.rs` — share extraction
- `init::collect_infrastructure_checks()` remains pure-ish (I/O for checks, structured output)

### Effort: 1-2 sessions

---

## 2c: Shell Completions

### Problem

Tab completion for `urd` subcommands and flags requires shell integration scripts.
`clap_complete` generates these from the CLI definition.

### Proposed Design

**New command:** `urd completions <shell>`

```rust
// src/cli.rs
/// Generate shell completion scripts
Completions(CompletionsArgs),

// CompletionsArgs
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: clap_complete::Shell,  // bash, zsh, fish, elvish, powershell
}
```

**New file:** `src/commands/completions.rs`

```rust
pub fn run(args: CompletionsArgs) {
    let mut cmd = Cli::command();
    clap_complete::generate(args.shell, &mut cmd, "urd", &mut std::io::stdout());
}
```

**Usage:**
```bash
# Bash
urd completions bash > ~/.local/share/bash-completion/completions/urd

# Zsh
urd completions zsh > ~/.zfunc/_urd

# Fish
urd completions fish > ~/.config/fish/completions/urd.fish
```

**Dynamic completions (stretch goal, deferred):** For `--subvolume` and `--drive` arguments,
`clap_complete` 4.x supports custom value hints. This would read config at completion time
to suggest subvolume names and drive labels. Deferred because:
- Config loading at tab-time adds latency
- Missing/invalid config must degrade gracefully
- Static completions cover the high-value case (subcommands + flags)

### Module Mapping

| File | Change |
|------|--------|
| `Cargo.toml` | Add `clap_complete = "4"` dependency |
| `src/cli.rs` | Add `Completions(CompletionsArgs)` variant |
| `src/main.rs` | Add dispatch arm (runs before config load) |
| `src/commands/completions.rs` | New file |
| `src/commands/mod.rs` | Add `pub mod completions;` |

### Test Strategy (~4 new tests)

- Static completions generate non-empty output for bash/zsh/fish
- Generated script contains expected subcommand names
- No config required (completions command works without urd.toml)

### Invariants

- Completions must work without a valid config file
- `clap_complete` version must match `clap` version (both 4.x)
- Completions command runs before config loading in main.rs dispatch

### Effort: 0.5 session

---

## Phase 2 Overall

**Total effort: 2-3 sessions.** All three features are independent and can be built in any
order. 2a and 2c both modify `cli.rs`'s `Commands` enum — if built in parallel, merge
carefully.

---

## Ready for Review

Focus areas for arch-adversary:

1. **2a: `Option<Commands>` in clap.** Verify that `--help` still works correctly when
   subcommand is optional. Clap may show a different help layout. Test: `urd --help` should
   still list all subcommands.

2. **2a: First-time detection.** The config load error is used to detect first-time users.
   This conflates "no config" with "broken config." The first-time path should only trigger
   on file-not-found, not on parse errors. Parse errors should still surface as errors.

3. **2b: Doctor I/O characteristics.** `collect_infrastructure_checks()` may spawn btrfs
   subprocess calls. Document what I/O doctor performs so users know it's not just reading
   state files.

4. **2b: Verify extraction.** If verify's data collection is tightly coupled to its
   rendering, extracting it cleanly may require a larger refactor. Assess the coupling
   before committing to the extraction approach.

5. **2c: Completions dispatch order.** Completions must run before config loading
   (`urd completions bash` shouldn't fail because no config exists). This means the
   main.rs dispatch needs to handle completions before the config load block.
