# Brainstorm Synthesis: Evaluating Urd's Future Directions

> **TL;DR:** Cross-evaluation of 40+ ideas from two brainstorming sessions — one exploring
> feature expansion (drives, restore, network, observability) and one applying Don Norman's
> UX design principles. Ideas are scored on user value, development effort, simplicity,
> UX impact, coolness, and ease of use. The highest-leverage work clusters around three
> themes: UX polish on what exists (shell completions, structured errors, better status),
> restore workflows (completing the backup story), and drive intelligence (UUID, per-subvolume
> targeting). Network features and TUI dashboards are high-value but premature before
> operational cutover.

**Date:** 2026-03-23
**Scope:** Synthesis of `docs/95-ideas/2026-03-23-brainstorm-urd-future.md` (feature
expansion) and `docs/95-ideas/2026-03-23-brainstorm-ux-norman-principles.md` (UX design
principles), evaluated against the current codebase at commit `b66be6f`.

## Executive Summary

The two brainstorming documents are complementary. The future-directions document asks
"what should Urd do?" while the Norman document asks "how should Urd feel?" The strongest
ideas live at the intersection: features that both expand capability and improve user
experience. The weakest ideas add capability without UX payoff, or add UX polish to
features that don't exist yet.

**Current codebase reality:** Urd has 7 CLI commands (plan, backup, status, history,
verify, init, calibrate), drives identified by mount path with labels, no restore, no
network sends, no shell completions, no JSON output, no notifications, no setup wizard,
and basic error messages that pass through raw btrfs stderr. The operational cutover from
bash has not started. This context is essential — ideas that build on a battle-tested
production system are premature when the system hasn't run production yet.

---

## Evaluation Criteria

Each idea is scored 1–5 on six dimensions:

| Dimension | What it means |
|-----------|---------------|
| **User Value** | How much does this improve the user's life? Does it prevent data loss, save time, or remove friction? |
| **Effort** | How much work to implement well? (1 = weeks/complex, 5 = hours/straightforward) |
| **Simplicity** | Does this keep the system simple, or add moving parts? (1 = significant complexity, 5 = minimal) |
| **UX Impact** | How much does the user *feel* the improvement in daily use? |
| **Coolness** | Would this make someone say "that's clever" or "I want that"? |
| **Ease of Use** | How intuitive is this for a new or returning user? |

**Composite score** = weighted average: User Value (3x) + Effort (2x) + Simplicity (1x) +
UX Impact (2x) + Coolness (1x) + Ease of Use (1x). Maximum 50. This weights impact and
feasibility over aesthetics.

---

## Tier 1: High-Value, Low-Effort — Do These First

These ideas have the best ratio of impact to effort. Most can be built independently
in a single session.

### Shell Completions (Norman §1.1)

Tab-completion for commands, flags, and configured subvolume/drive names.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Eliminates guessing subvolume/drive names |
| Effort | 5 | `clap_complete` generates from existing definitions; dynamic completers need more work |
| Simplicity | 5 | Additive — zero impact on existing code |
| UX Impact | 4 | Discoverability transforms daily use |
| Coolness | 2 | Expected, not novel |
| Ease of Use | 5 | The definition of ease — less typing, fewer typos |
| **Composite** | **41** | |

**Codebase fit:** clap already defines all commands and flags. Static completions are
near-trivial. Dynamic completions (subvolume names from config) require a custom completer
that reads the config file — moderate but well-bounded work. This is the single
highest-value-per-effort item across both documents.

### Help Text with Examples (Norman §1.2)

Add `before_help`/`after_help` with usage examples to each command.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Helps new users, less impactful for daily use |
| Effort | 5 | Pure text changes in cli.rs |
| Simplicity | 5 | No code changes |
| UX Impact | 4 | First impression of the tool; teaches workflows not just syntax |
| Coolness | 2 | Expected |
| Ease of Use | 5 | Self-documenting tool |
| **Composite** | **37** | |

**Codebase fit:** `cli.rs` uses clap derive macros. Adding `#[command(after_help = "...")]`
is mechanical. The Norman doc provides ready-to-use example text.

### Suggested Next Actions (Norman §1.3)

After each command, suggest what to do next.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Bridges the gulf of execution for new users |
| Effort | 4 | A few `eprintln!` calls at the end of each command handler |
| Simplicity | 5 | No structural changes |
| UX Impact | 4 | Turns a dead-end into a guided workflow |
| Coolness | 3 | Feels polished and thoughtful |
| Ease of Use | 5 | User never has to check `--help` to know the next step |
| **Composite** | **36** | |

### UUID Drive Fingerprinting (Future §1.2)

Identify drives by BTRFS filesystem UUID instead of mount path alone.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | Prevents sending to wrong drive — data safety feature |
| Effort | 4 | Read UUID from `btrfs filesystem show`, add to config, verify on mount |
| Simplicity | 4 | Adds one field to config; verification is a simple string compare |
| UX Impact | 3 | Invisible when working; loud when it prevents a mistake |
| Coolness | 2 | Expected for a safety-conscious tool |
| Ease of Use | 4 | First-time setup adds one field; thereafter invisible |
| **Composite** | **38** | |

**Codebase fit:** `drives.rs` already calls `btrfs filesystem show` for space info.
Parsing UUID from the same output is straightforward. Config validation can warn if
UUID is missing (backward-compatible). The mount-path check becomes a two-step verify:
path mounted AND UUID matches.

### Structured Error Messages (Norman §6.1)

Replace raw btrfs stderr with layered error messages: what happened, why, how to fix.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Users can self-diagnose instead of searching forums |
| Effort | 3 | Requires pattern-matching on btrfs stderr strings; ongoing maintenance |
| Simplicity | 3 | Adds an error translation layer; must stay in sync with btrfs versions |
| UX Impact | 5 | The moment a user hits an error is when UX matters most |
| Coolness | 3 | "The tool told me exactly how to fix it" |
| Ease of Use | 5 | Turns brick walls into signposts |
| **Composite** | **39** | |

**Codebase fit:** `error.rs` defines `UrdError::Btrfs { msg, bytes_transferred }`.
The msg field currently holds raw stderr. Adding a match on common patterns ("No space
left on device", "not a btrfs filesystem", "permission denied") with human-readable
wrappers is a bounded task. The technical detail can go behind `--verbose`.

### Sudoers Generator (Future §7.3)

`urd sudoers` outputs exact sudoers entries derived from current config.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Biggest barrier for new users after "what is BTRFS" |
| Effort | 4 | Read config, enumerate paths, format NOPASSWD lines |
| Simplicity | 5 | Pure output — no state changes |
| UX Impact | 3 | One-time use, but removes the scariest step in setup |
| Coolness | 3 | Thoughtful — tool helps configure its own prerequisites |
| Ease of Use | 5 | Copy-paste vs. hand-writing sudoers rules |
| **Composite** | **38** | |

---

## Tier 2: High-Value, Medium-Effort — Plan Carefully, Build After Cutover

These require design work and meaningful code changes. Worth building but not before
Urd is running production backups.

### Subvolume-to-Drive Mapping (Future §2.1)

Config support for targeting specific subvolumes to specific drives.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | Essential for multi-drive setups with different-sized drives |
| Effort | 3 | Config change + planner filter + backward compatibility |
| Simplicity | 3 | Adds a dimension to the planning matrix |
| UX Impact | 4 | Users stop sending 3TB subvolumes to 2TB drives |
| Coolness | 3 | Expected for a general-purpose tool |
| Ease of Use | 4 | Opt-in; omitting `drives` means "all" (backward compatible) |
| **Composite** | **39** | |

**Codebase fit:** The planner currently iterates all subvolumes × all mounted drives.
Adding a filter is a single `if` in the planning loop. The config change needs a new
optional field on `SubvolumeConfig`. Clean architectural fit.

### Post-Backup Structured Summary (Norman §3.1)

Answer "is my data safer now?" after every backup run.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | The single most Norman thing Urd could do (per the brainstorm) |
| Effort | 3 | Requires aggregating results across all operations; formatting |
| Simplicity | 4 | Adds output formatting, not new logic |
| UX Impact | 5 | Transforms backup completion from "done" to "safe" |
| Coolness | 4 | "All 7 subvolumes backed up to at least one external drive" |
| Ease of Use | 5 | User reads one summary instead of scanning per-subvolume output |
| **Composite** | **43** | |

**Codebase fit:** `backup.rs` already collects per-subvolume results. Aggregating into
a summary is straightforward. The "protection change" framing requires tracking
pre-backup state (last send timestamps from SQLite) and comparing against post-backup.
Moderate work but well-bounded.

### `urd restore` Command (Future §4.1)

Restore entire snapshots or individual files from local or external snapshots.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | Completes the backup story — a backup you can't restore is worthless |
| Effort | 2 | Multiple restore modes; browsing snapshots; error handling for partial restores |
| Simplicity | 3 | Adds a new major command; introduces receive-side logic |
| UX Impact | 5 | The feature users will be most grateful for when they need it |
| Coolness | 4 | "I restored my file from 3 days ago in one command" |
| Ease of Use | 4 | Simple cases easy; advanced restore from external drives more complex |
| **Composite** | **38** | |

**Codebase fit:** The `BtrfsOps` trait would need `receive` and possibly `subvolume_list`
methods. Single-file restore doesn't need btrfs receive at all — just copy from the
read-only snapshot path. Start with the simple case (local snapshot → cp) and build up.

### Pre-flight Checks (Norman §5.2)

Validate system readiness before starting a backup.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Prevents wasted time on doomed backup runs |
| Effort | 3 | `init.rs` already does most checks; need to run them before `backup` too |
| Simplicity | 4 | Reuses existing validation logic |
| UX Impact | 4 | Fail fast with diagnosis, not fail slow with raw errors |
| Coolness | 2 | Expected |
| Ease of Use | 5 | System tells you what's wrong before you wait 3 hours |
| **Composite** | **37** | |

**Codebase fit:** `init.rs` checks sudo, metrics dir, lock dir, DB. Extracting these into
a shared `preflight_checks()` and calling before backup execution is clean refactoring.

### `urd setup` Wizard (Future §3.1)

Interactive guided configuration for new users.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | Removes the biggest barrier to adoption |
| Effort | 2 | Interactive TUI, subvolume scanning, drive detection, config generation |
| Simplicity | 2 | Significant new code; interactive state machine |
| UX Impact | 5 | First-run experience defines tool perception |
| Coolness | 5 | "It found my drives and subvolumes automatically" |
| Ease of Use | 5 | The whole point is ease of use |
| **Composite** | **39** | |

**Codebase fit:** Would use `dialoguer` or similar for interactive prompts. Subvolume
discovery needs `btrfs subvolume list`. Drive discovery reuses `drives.rs`. Config
generation is templating. Significant but self-contained new code.

### Explicit Error Codes (Norman §6.2)

Granular exit codes for scripting and monitoring.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Enables monitoring scripts and systemd integration |
| Effort | 4 | Map existing error paths to defined exit codes |
| Simplicity | 5 | Changes return values, not logic |
| UX Impact | 2 | Invisible to interactive users; critical for automation |
| Coolness | 1 | Plumbing |
| Ease of Use | 3 | Allows `ExecCondition=` in systemd and `if` in scripts |
| **Composite** | **29** | |

### Growth Rate Prediction (Future §6.1)

Track subvolume growth and predict drive-full dates.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Turns surprise disk-full into planned upgrade |
| Effort | 3 | Needs historical data collection, trend calculation, display |
| Simplicity | 3 | Adds time-series tracking to state DB |
| UX Impact | 3 | Valuable in `urd status` but not daily-visible |
| Coolness | 4 | "WD-18TB full in 43 days" |
| Ease of Use | 4 | Passive — just shows up in status |
| **Composite** | **35** | |

**Codebase fit:** `state.rs` already records send sizes per run. Trend calculation
over historical data is a SQL query. Display in `status.rs` is formatting.

---

## Tier 3: High-Value, High-Effort — Vision Features

These are the features that would make Urd remarkable. Each requires significant
design and implementation work. Worth building, but only after the foundation is solid.

### SSH Remote Targets (Future §5.1)

Send snapshots to remote machines via `btrfs send | ssh | btrfs receive`.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | The killer feature for NAS/offsite backup |
| Effort | 1 | SSH auth, remote btrfs validation, network error handling, bandwidth throttling |
| Simplicity | 2 | Adds a new target type; networking introduces failure modes local sends don't have |
| UX Impact | 4 | "My laptop backs up to my NAS automatically" |
| Coolness | 5 | Transforms Urd from local tool to network backup system |
| Ease of Use | 3 | SSH key management adds friction |
| **Composite** | **33** | |

**Codebase fit:** The planner/executor separation helps here — SSH targets are just
another target type. But `RealBtrfs::send_receive` currently pipes two local processes.
SSH requires piping through `ssh`, handling remote errors, and potentially resuming
interrupted transfers. This is a substantial extension to `btrfs.rs`.

### Time-Travel File Browser (Future §4.2)

Interactive TUI for browsing snapshot history of specific files.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | "Show me every version of this file" |
| Effort | 1 | TUI framework (ratatui), snapshot navigation, file diffing |
| Simplicity | 1 | Adds a full TUI application within the CLI |
| UX Impact | 5 | The macOS Time Machine visual, in a terminal |
| Coolness | 5 | This is the feature people screenshot and share |
| Ease of Use | 4 | Visual navigation is inherently intuitive |
| **Composite** | **31** | |

**Codebase fit:** BTRFS snapshots are just directories — browsing them doesn't need
btrfs commands, just filesystem access. The challenge is the TUI, not the data.
Adds ratatui as a dependency. Self-contained module.

### Sentinel + Notifications (Future §1.1 + §6.3)

Auto-detect drive plug events and send notifications for backup lifecycle.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | "The Time Machine moment" — plug in drive, backup starts |
| Effort | 2 | udev rules, daemon process, dbus/notify-send, Discord webhook |
| Simplicity | 2 | Adds a persistent daemon; process management complexity |
| UX Impact | 5 | The feature that makes Urd feel magical |
| Coolness | 5 | "I plugged in my drive and it just backed up" |
| Ease of Use | 5 | Zero-interaction backup trigger |
| **Composite** | **38** | |

**Codebase fit:** `sentinel.rs` is already stubbed. Phase 5 in the roadmap. udev rules
are in the repo structure. This is planned work — the brainstorm confirms it's the right
priority after cutover.

### Zero-Config Mode (Future §7.1)

`urd backup /home --to /run/media/<user>/MyDrive` — no config file needed.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 5 | Ultimate onboarding experience |
| Effort | 2 | Must derive all config from arguments; sensible defaults for everything |
| Simplicity | 3 | Config becomes optional; two code paths (config-based, arg-based) |
| UX Impact | 5 | One command to start backing up |
| Coolness | 5 | "Install it, run one command, you're backed up" |
| Ease of Use | 5 | As easy as it gets |
| **Composite** | **40** | |

**Codebase fit:** Currently every code path starts with `Config::load()`. Zero-config
would need a `Config::from_args()` that constructs a synthetic config from CLI arguments.
The planner and executor don't need to change — they work on `Config` regardless of source.
Clean architectural fit, but the argument parsing and defaulting logic is substantial.

### FUSE Filesystem (Future §10.1)

Mount backup history as a virtual filesystem: `/timeline/htpc-home/2026-03-22/`.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Browse history with standard tools (ls, cp, diff) |
| Effort | 1 | FUSE implementation, snapshot enumeration, read-only passthrough |
| Simplicity | 1 | Adds a kernel-adjacent subsystem; FUSE has its own failure modes |
| UX Impact | 5 | Browse time like folders — universally intuitive |
| Coolness | 5 | The wow feature |
| Ease of Use | 5 | `ls` is the most known interface in computing |
| **Composite** | **32** | |

**Codebase fit:** BTRFS snapshots are already accessible as directories, so the FUSE
layer is mostly organizational (date hierarchy over flat snapshot names). But FUSE
adds `fuser` crate dependency and a daemon process. The snapshot mounting helper
(Future §4.3) gets most of the value for 10% of the effort.

---

## Tier 4: Nice-to-Have — Lower Priority

These are good ideas with either lower impact or niche applicability.

### Backup Health Score (Future §6.2)

Single 0-100 number representing backup health.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Gamifies backup status; useful for monitoring |
| Effort | 3 | Scoring rubric, Prometheus metric, display |
| Simplicity | 3 | Adds a derived metric; scoring rules need maintenance |
| UX Impact | 3 | "87/100" is instantly understandable |
| Coolness | 3 | Satisfying but not transformative |
| Ease of Use | 4 | Single number answers "am I OK?" |
| **Composite** | **30** | |

### Drive Trust Levels (Future §1.3)

Tag drives as onsite/offsite/portable with visit intervals.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Useful for multi-drive setups; overkill for simple ones |
| Effort | 4 | Config fields + status warnings |
| Simplicity | 3 | Adds classification scheme |
| UX Impact | 2 | Only visible when checking status |
| Coolness | 3 | Smart drive awareness |
| Ease of Use | 3 | Requires understanding the trust model |
| **Composite** | **30** | |

### Drive Health Monitoring (Future §1.4)

SMART checks and btrfs device stats on plug-in.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Early warning for failing drives |
| Effort | 3 | Parse smartctl, parse btrfs device stats |
| Simplicity | 3 | Adds external tool dependency (smartctl) |
| UX Impact | 2 | Rarely seen, very valuable when it triggers |
| Coolness | 3 | Proactive health awareness |
| Ease of Use | 4 | Passive — surfaces automatically |
| **Composite** | **29** | |

### `urd explain` / `urd why` (Norman §2.2, §8.5)

Explain planner decisions, either prospectively or retrospectively.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Debugging tool for understanding unexpected behavior |
| Effort | 3 | Planner already has the logic; needs to emit reasoning not just results |
| Simplicity | 3 | Adds explanation strings to plan output |
| UX Impact | 3 | High value when confused; zero value when things work |
| Coolness | 4 | "The tool explains its own decisions" |
| Ease of Use | 4 | Ask and receive |
| **Composite** | **31** | |

### JSON Output (Norman §9.2)

`--json` flag for machine-readable output.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Enables scripting and custom dashboards |
| Effort | 3 | Serde serialization of output structs; parallel output path |
| Simplicity | 3 | Dual output paths need maintenance |
| UX Impact | 1 | Invisible to interactive users |
| Coolness | 2 | Expected for modern CLI tools |
| Ease of Use | 3 | Enables downstream tooling |
| **Composite** | **25** | |

### Configuration Profiles (Future §10.4)

Pre-built configs for laptop, workstation, server, etc.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Useful onboarding shortcut |
| Effort | 4 | Config templates + init flag |
| Simplicity | 4 | Additive — just pre-authored TOML files |
| UX Impact | 3 | Only relevant during first setup |
| Coolness | 3 | Feels professional |
| Ease of Use | 4 | Pick a profile, customize later |
| **Composite** | **32** | |

### `urd` with No Arguments = Status (Norman §1.4)

Default to `urd status` when run without subcommand.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | "Am I OK?" is the most common question |
| Effort | 5 | One line in clap config |
| Simplicity | 5 | No code changes |
| UX Impact | 3 | Saves one word of typing |
| Coolness | 2 | Subtle |
| Ease of Use | 4 | Muscle memory rewarded |
| **Composite** | **33** | |

### Encrypted Sends (Future §9.1)

Pipe btrfs send through age/gpg before writing to external drive.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | Essential for offsite drives that might be lost |
| Effort | 2 | Encryption pipeline, key management, restore decryption, testing |
| Simplicity | 2 | Encrypted blobs aren't browsable; changes restore workflow |
| UX Impact | 2 | Security feature — invisible when working, critical when needed |
| Coolness | 3 | Responsible engineering |
| Ease of Use | 2 | Key management is inherently complex |
| **Composite** | **28** | |

### Disaster Recovery Playbook (Future §10.7)

`urd disaster-recovery` generates a step-by-step recovery guide.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 4 | The document you need when everything is on fire |
| Effort | 3 | Template generation from config + state |
| Simplicity | 4 | Pure output, no state changes |
| UX Impact | 2 | Only used in emergencies — but invaluable then |
| Coolness | 4 | "Print this and keep it with your offsite drive" |
| Ease of Use | 4 | Generated, not authored |
| **Composite** | **33** | |

---

## Tier 5: Defer Indefinitely

These ideas are interesting but either premature, over-engineered for the current user
base, or introduce complexity that isn't justified yet.

### Pull Mode / Mesh Networking (Future §5.2, §5.3)

Server-pulls-from-clients or peer-to-peer backup.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | Enterprise/family-NAS feature; single-user tool doesn't need it |
| Effort | 1 | Inverts the entire execution model; agent coordination |
| Simplicity | 1 | Adds distributed systems complexity |
| UX Impact | 2 | Only relevant for multi-machine setups |
| Coolness | 4 | Architecturally interesting |
| Ease of Use | 2 | Configuration complexity explodes |
| **Composite** | **21** | |

### Cloud Backup to S3/B2 (Future §10.2)

Encrypted btrfs streams to object storage.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 3 | True offsite, but ongoing cost and complexity |
| Effort | 1 | S3 API, multipart upload, chain tracking in metadata, cost management |
| Simplicity | 1 | Cloud APIs, billing, credentials — far from the btrfs-centric design |
| UX Impact | 2 | Set and forget, but setup is complex |
| Coolness | 3 | "My backups are in Glacier" |
| Ease of Use | 2 | Cloud credential management |
| **Composite** | **20** | |

### Audit Trail with Hash Chain (Future §9.3)

Cryptographically signed audit log.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 1 | Compliance feature; homelab user doesn't need this |
| Effort | 2 | Hash chain implementation, verification |
| Simplicity | 2 | Adds cryptographic complexity |
| UX Impact | 1 | Invisible |
| Coolness | 3 | "Mini blockchain" is fun to build |
| Ease of Use | 3 | Passive |
| **Composite** | **16** | |

### Multi-User / System-Wide Mode (Future §10.6)

Iterate users and run their configs centrally.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 2 | Niche — shared Linux desktops are rare |
| Effort | 2 | Privilege escalation, per-user isolation, conflict resolution |
| Simplicity | 1 | Major architectural shift |
| UX Impact | 1 | Only relevant for sysadmins |
| Coolness | 2 | Solves a problem few have |
| Ease of Use | 2 | Complex to configure |
| **Composite** | **18** | |

### Library Mode (Future §10.5)

Extract core logic into a library crate.

| Dimension | Score | Notes |
|-----------|-------|-------|
| User Value | 2 | Enables GUI frontends that don't exist yet |
| Effort | 2 | API surface design, stability guarantees, documentation |
| Simplicity | 2 | Public API is a maintenance commitment |
| UX Impact | 1 | Users don't see this |
| Coolness | 3 | Enables ecosystem |
| Ease of Use | 2 | Useful for developers, not end users |
| **Composite** | **19** | |

---

## Cross-Cutting Norman Improvements

Several Norman ideas don't map to discrete features — they're principles that should
inform all future work. These aren't scored individually but noted as ongoing guidance.

| Principle | Application |
|-----------|-------------|
| **Consistent color semantics** (§7.3) | Green=success, Red=failure, Yellow=warning, Blue=active. Document in codebase. Already mostly followed. |
| **Plan/backup output mirroring** (§4.3) | `urd backup` output should use same structure as `urd plan`, extended with results. Already partially true. |
| **`--dry-run` is sacred** (§7.1) | Currently correct — plan() is pure. Document as a guarantee. |
| **Graceful Ctrl+C messaging** (§7.2) | Currently works but could show more detail about cleanup and remaining work. |
| **`urd status` as evaluation surface** (§8.3) | Status already shows chain health. Adding "attention needed" section and drive-full predictions would close the gap. |
| **Flag consistency audit** (§4.1) | `--subvolume` and `--priority` should work uniformly across commands. |
| **NO_COLOR support** (§9.3) | Respect `NO_COLOR` env var. `colored` crate may handle this, but audit. |
| **"Did you mean?" for typos** (§6.3) | Levenshtein distance on subvolume/drive names. Small effort, high polish. |

---

## Recommended Sequencing

Given that the operational cutover hasn't started, the recommended build order respects
the project's actual state:

### Before Cutover (polish what exists)

1. **Shell completions** — immediate discoverability win
2. **Help text with examples** — first-impression quality
3. **`urd` bare = status** — one-line change
4. **Suggested next actions** — guided workflow
5. **Pre-flight checks** — reuse init.rs validation in backup

### During/After Cutover (battle-test, then extend)

6. **Structured error messages** — will be informed by real failure modes observed during cutover
7. **Post-backup summary** — needs real production runs to calibrate what matters
8. **UUID drive fingerprinting** — safety feature for production
9. **Sudoers generator** — prep for other users
10. **Explicit error codes** — systemd timer integration

### Post-Cutover Expansion (Phase 5+)

11. **Sentinel + notifications** — already planned as Phase 5
12. **Subvolume-to-drive mapping** — essential for generalization
13. **`urd restore`** — completes the backup story
14. **Zero-config mode** — the adoption play
15. **`urd setup` wizard** — the onboarding play

### Vision (when core is mature)

16. **SSH remote targets** — the killer feature
17. **Growth prediction** — passive intelligence
18. **Time-travel browser** — the wow feature
19. **Disaster recovery playbook** — the peace-of-mind feature

---

## Key Insight

The Norman brainstorm and the feature brainstorm converge on the same truth: **Urd's
core backup engine is solid, but the user-facing surfaces need work.** The planner/executor
architecture, the chain tracking, the retention logic — these are well-designed and
well-tested. What's missing is the layer between the engine and the human: discoverability,
error recovery guidance, completion feedback, and restore capability.

The highest-leverage work isn't building new backup features — it's making the existing
features communicate clearly. Shell completions, structured errors, and a post-backup
summary would transform Urd's usability without touching the core engine at all.
