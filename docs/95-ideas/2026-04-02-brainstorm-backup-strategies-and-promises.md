# Brainstorm: Backup Strategies Tied to Protection Promises

> **Seed question:** How should Urd's promise/protection model map to recognized backup
> strategies (3-2-1-1-0, 4-3-2, GFS, CDP, tiered archival, mirror+snapshot)? How can Urd
> make setting up these strategies effortless and, once configured, completely autonomous?

**Date:** 2026-04-02
**Status:** raw
**Inputs:**
- User-provided strategy breakdown (3-2-1-1-0, 4-3-2, GFS, CDP, tiered/warm-cold, mirror+snapshot)
- Current promise model: `ProtectionLevel` enum (guarded/protected/resilient/custom)
- ADR-110 (protection promises), ADR-111 (config system)
- Design: promise redundancy encoding (offsite freshness overlay)
- `derive_policy()` in types.rs (pure function: level + frequency -> operational params)
- `awareness.rs` (promise status: PROTECTED/AT_RISK/UNPROTECTED, operational health)
- Existing `DriveRole` enum (Primary/Offsite/Test), `GraduatedRetention`

---

## Ideas

### 1. Strategy as the Top-Level Concept (Replace or Subsume Protection Levels)

Instead of the user choosing a protection *level*, they choose a backup *strategy*. The
strategy implies the level, the drive topology, the retention policy, and the verification
behavior. The current guarded/protected/resilient taxonomy becomes an internal
implementation detail — the user-facing concept is the strategy name.

```toml
[[subvolumes]]
name = "opptak"
strategy = "3-2-1"  # implies: protected, >= 1 offsite drive, GFS retention
```

Urd derives everything from the strategy declaration. The user never touches intervals,
retention numbers, or drive assignments directly (unless they want `strategy = "custom"`).

**Data safety lens:** Dramatically safer — users can't accidentally configure a weak
combination. The strategy name carries the full intent.

---

### 2. Strategy Templates as Scaffolding (ADR-111 Aligned)

Rather than strategies replacing protection levels, strategies are **templates** that
scaffold a complete config. `urd init --strategy 3-2-1` generates the right config stanza
with the right protection level, drive topology, and retention settings. The template
produces concrete config that the user can inspect and override.

This aligns with ADR-111's position that templates scaffold rather than govern. The
strategy is a one-time generation tool; the config is the ongoing truth.

```bash
$ urd init --strategy 3-2-1
Generated config for "opptak":
  protection_level = "protected"
  drives = ["WD-18TB"]       # detected: 1 external drive
  local_retention = { daily = 30, weekly = 26, monthly = 12 }
  # 3-2-1 note: consider adding a drive with role = "offsite" for geographic redundancy
```

**Data safety lens:** Slightly less safe than idea 1 (user can edit the scaffolded config
into incoherence) but respects the principle that the config file is the source of truth.

---

### 3. Strategy Verification Layer (`urd doctor --strategy`)

A diagnostic command that evaluates the current config + filesystem state against a named
strategy and reports compliance gaps. Not prescriptive — descriptive.

```
$ urd doctor --strategy 3-2-1-1-0
Strategy: 3-2-1-1-0 (3 copies, 2 media, 1 offsite, 1 immutable, 0 errors)

  ✓ 3 copies: local + WD-18TB + WD-18TB1
  ✓ 2 media types: NVMe (local) + HDD (external)
  ✗ 1 offsite: no drive with role = "offsite" configured
  ✗ 1 immutable: no immutable/air-gapped copy detected
  ✗ 0 errors: no verified restores in last 30 days

Recommendations:
  - Set role = "offsite" on WD-18TB1 and rotate it offsite monthly
  - Run `urd verify-restore --subvolume opptak` monthly to satisfy the "0 errors" leg
```

This is purely advisory. It maps the user's *actual* setup against a *desired* strategy
without changing any config. It's an audit tool.

**Data safety lens:** Very safe — it reveals gaps the user didn't know about. The "0
errors" leg is particularly powerful because it makes restore testing a visible obligation.

---

### 4. Drive Roles as Strategy Primitives

Extend `DriveRole` beyond Primary/Offsite/Test to encode strategy-relevant semantics:

```rust
pub enum DriveRole {
    Primary,
    Offsite,
    Immutable,    // air-gapped, write-once semantics
    Archive,      // cold storage tier
    Mirror,       // RAID-paired, not independent backup
    Test,
}
```

The strategy verification layer (idea 3) and the promise derivation logic (idea 1) can
use drive roles to reason about whether a configuration satisfies a strategy. `resilient`
+ `Immutable` drive = 3-2-1-1-0 candidate. `Protected` + two `Primary` drives in different
locations = approaching 4-3-2.

**Data safety lens:** Makes the system's understanding of hardware topology explicit.
Currently, a drive's role is a label; with richer roles, Urd can reason about what each
drive actually contributes to data safety.

---

### 5. GFS as First-Class Retention Model (Already Native)

The current `GraduatedRetention` (hourly/daily/weekly/monthly) is already GFS in
everything but name. Surface this explicitly:

- Rename or alias the retention fields to use GFS terminology in documentation and
  `urd status` output: "son" = hourly/daily, "father" = weekly, "grandfather" = monthly
- Add `yearly` to GraduatedRetention for true long-term archival
- Show the GFS shape in status: "7d / 4w / 12m / ∞y" alongside each subvolume

The user already has GFS. They just don't know it because it's called "graduated retention."

**Data safety lens:** Neutral technically (same behavior), but significantly safer in
practice because users understand their retention window and can make informed decisions.

---

### 6. Restore Verification as Promise Obligation ("0 Errors" Leg)

Add a `urd verify-restore` command that picks a random file from a recent snapshot,
restores it to a temp location, and verifies it against the live filesystem. Track the
last verification date per subvolume in the state DB.

The awareness model gains a new dimension: **verification freshness**. If no verified
restore has happened in N days, the promise status could degrade (or an advisory fires).

```rust
pub struct SubvolAssessment {
    // ... existing fields ...
    pub last_verified_restore: Option<NaiveDateTime>,
    pub verification_status: VerificationStatus,  // Verified / Stale / Never
}
```

For 3-2-1-1-0, the "0 errors" leg becomes a measurable, trackable property rather than
an honor system.

**Data safety lens:** This is potentially the highest-impact idea. Backup systems that
never test restores are theater. Making verification a visible, tracked obligation
transforms Urd from "I take snapshots" to "I guarantee recovery."

---

### 7. Immutability Tracking for Air-Gapped Drives

When a drive with `role = "immutable"` is connected, Urd sends snapshots but does NOT
delete old ones (retention is disabled for that drive). When disconnected, Urd tracks
the disconnect date and reports how long the air gap has been maintained.

```
WD-18TB1 (immutable): last connected 12 days ago, 47 snapshots preserved
  Air gap integrity: ✓ (no retention deletions since 2026-01-15)
```

For the "1 immutable" leg of 3-2-1-1-0, this gives the user confidence that the offsite
copy hasn't been tampered with or thinned. Urd guarantees it won't delete from immutable
drives — the user's only job is to physically protect the drive.

**Data safety lens:** Directly addresses ransomware resilience. Even if the local system
is compromised, the immutable drive has an untouched copy.

---

### 8. Tiered Lifecycle Policies (Warm -> Cold -> Archive)

Add a lifecycle dimension to retention that describes where snapshots should live at
different ages:

```toml
[[subvolumes]]
name = "photos"
strategy = "tiered"
tiers = [
  { age = "0-30d", location = "local+primary" },
  { age = "30d-6m", location = "primary" },     # drop local copies after 30d
  { age = "6m+", location = "archive" },          # move to cold storage
]
```

The planner gains lifecycle awareness: when a snapshot ages past a tier boundary, the
planner includes a "promote" operation (send to next tier) and a "demote" operation
(delete from current tier, if configured).

The awareness model tracks tier compliance: is the right number of snapshots in the
right tier?

**Data safety lens:** Addresses the cost vs. retention tradeoff directly. Users with
large media libraries (photos, recordings) can keep deep history without consuming
primary drive space.

---

### 9. Drive Topology Advisor (Auto-Detect Strategy Fit)

On `urd init` or `urd doctor`, scan the detected drives and suggest which strategy best
fits the user's hardware:

```
$ urd doctor
Detected topology:
  - NVMe 1TB (local, BTRFS RAID1)
  - WD-18TB (external HDD, connected)
  - WD-18TB1 (external HDD, disconnected, last seen 5 days ago)

This topology supports:
  ✓ Mirror + Snapshot (RAID1 handles hardware, snapshots handle logical)
  ✓ 3-2-1 (local + 2 external, need 1 offsite)
  ~ 3-2-1-1-0 (need immutable designation + restore verification)
  ✗ 4-3-2 (need 2 geographically separate offsite locations)

Suggested: 3-2-1 with WD-18TB1 as offsite rotation drive
```

Pure analysis, no config changes. The user sees what their hardware can support and
what's missing for higher-tier strategies.

**Data safety lens:** Removes the guesswork from strategy selection. Users discover
gaps before they matter.

---

### 10. Promise Composition (Multiple Strategies per Subvolume)

Instead of one strategy per subvolume, allow composed promises that layer strategies:

```toml
[[subvolumes]]
name = "opptak"
promises = ["3-2-1", "gfs", "verified"]
# Urd derives: protected + offsite + GFS retention + monthly restore verification
```

Each promise contributes requirements that merge into the final policy. This lets users
express exactly what they want: "I want geographic redundancy AND deep history AND
verified restores."

**Data safety lens:** Very safe — additive composition means more promises = more
protection. But complexity risk: which promise dominates when they conflict?

---

### 11. Strategy-Aware Notifications

Extend the notification system to speak in strategy terms:

```
⚠ Your 3-2-1 promise for "opptak" is incomplete.
  The offsite leg (WD-18TB1) hasn't been connected in 34 days.
  Connect WD-18TB1 and run `urd backup` to restore your geographic redundancy.
```

vs. the current:

```
⚠ opptak: AT RISK — external send overdue
```

The strategy framing tells the user *why* this matters (geographic redundancy), not just
*what* is overdue (a send).

**Data safety lens:** Makes notifications actionable and educational. The user learns
what their strategy protects against while being told how to fix it.

---

### 12. CDP-Adjacent: Application-Aware Snapshot Triggers

For the CDP strategy, Urd can't do filesystem-level continuous protection (BTRFS
limitation). But it can respond to application events:

- Hook into `inotifywait` on critical directories to trigger snapshots on significant
  change
- Accept signals from applications (e.g., a PostgreSQL post-checkpoint hook calls
  `urd snapshot --subvolume databases`)
- Sentinel watches for write bursts and triggers interim snapshots

```toml
[[subvolumes]]
name = "databases"
strategy = "cdp-adjacent"
trigger = "write-burst"   # Sentinel mode: snapshot when write activity exceeds threshold
# or
trigger = "signal"        # wait for external signal
```

**Data safety lens:** Gets closer to CDP's promise without requiring filesystem-level
journaling. The RPO for critical data drops from "hourly" to "minutes after significant
change."

---

### 13. Mirror-Awareness (Don't Count RAID as Backup)

When Urd detects BTRFS RAID1 (via `btrfs filesystem show`), it should explicitly NOT
count mirrored copies as separate backup copies. The status output should acknowledge
the mirror but explain its limitation:

```
Local: BTRFS RAID1 (hardware redundancy, NOT backup)
  ✓ Protects against: disk failure
  ✗ Does NOT protect against: ransomware, accidental deletion, logical corruption
  Need: at least 1 external copy for true backup
```

This prevents the "I have RAID so I'm backed up" misconception that leads to data loss.

**Data safety lens:** Extremely important. RAID-as-backup is one of the most common
data loss misconceptions. Making this explicit could save users from learning the hard way.

---

### 14. Strategy Maturity Ladder (Progressive Enhancement)

Define a maturity ladder that users climb naturally:

```
Level 0: No backup (Urd not configured)
Level 1: Local snapshots only (guarded)
Level 2: Local + 1 external (protected) — basic 3-2-1
Level 3: Local + offsite rotation (resilient) — geographic 3-2-1
Level 4: + immutable copy (3-2-1-1-0)
Level 5: + verified restores (3-2-1-1-0 complete)
Level 6: + tiered archival (long-term resilience)
```

`urd status` shows the user's current maturity level and what they'd need to reach the
next one. This turns backup strategy from a one-time decision into a progressive journey.

```
Strategy maturity: Level 3 of 6 (geographic redundancy)
  Next level: designate WD-18TB1 as immutable and rotate it offsite
```

**Data safety lens:** Makes improvement legible. Users who see "Level 3 of 6" naturally
want to reach Level 4. Gamification of data safety — but it actually works because each
level is genuinely more resilient.

---

### 15. Yearly Retention Tier for True Archival

Add `yearly` to `GraduatedRetention`:

```rust
pub struct GraduatedRetention {
    pub hourly: Option<u32>,
    pub daily: Option<u32>,
    pub weekly: Option<u32>,
    pub monthly: Option<u32>,
    pub yearly: Option<u32>,  // NEW: keep N snapshots per year
}
```

This enables true deep archival without monthly snapshot accumulation. A `yearly = 10`
setting keeps one snapshot per year for a decade. Combined with `monthly = 12`, you get
the full GFS grandfather tier.

**Data safety lens:** Directly enables long-term recovery windows. A photo from 2020
might not need daily granularity, but it needs to exist somewhere.

---

### 16. Strategy Presets in Config (Named, Not Numbered)

Instead of encoding strategy knowledge in code, allow strategy presets in a reference
config that ships with Urd:

```toml
# Shipped in /usr/share/urd/strategies/
[strategy.3-2-1]
protection_level = "protected"
min_offsite_drives = 1
retention = { daily = 30, weekly = 26, monthly = 12 }
verification_interval = "30d"

[strategy.3-2-1-1-0]
protection_level = "resilient"
min_offsite_drives = 1
min_immutable_drives = 1
retention = { daily = 30, weekly = 26, monthly = 12, yearly = 10 }
verification_interval = "7d"
```

Users reference these by name. Urd ships new strategies in updates without code changes.

**Data safety lens:** Keeps strategy definitions current as best practices evolve. A new
"NIST-recommended" strategy could ship as a preset without a code release.

---

### 17. Cross-Machine Strategy Awareness (Uncomfortable Idea)

Urd currently operates per-machine. But 3-2-1 and 4-3-2 are inherently multi-location
strategies. What if Urd instances could communicate?

- Urd on machine A sends a heartbeat to a shared location (S3 bucket, shared drive)
- Urd on machine B reads it and includes A's copy in its strategy assessment
- "Your 4-3-2 for 'databases' is complete: machine A (local), machine B (offsite-1),
  Backblaze (offsite-2)"

This is architecturally huge and probably out of scope for v1, but it's the logical
endpoint of multi-location strategies.

**Data safety lens:** The only way to truly verify multi-location strategies. Without it,
the user manually tracks which machine has which copy.

---

### 18. Retention as Derived from Recovery Objectives (RPO/RTO Framing)

Instead of configuring retention directly, let users express recovery objectives:

```toml
[[subvolumes]]
name = "databases"
recovery_point_objective = "1h"    # max acceptable data loss
recovery_time_objective = "15m"    # max acceptable restore time
```

Urd derives snapshot frequency from RPO and retention depth from RTO (deeper history =
more recovery points to choose from, but potentially slower to find the right one).

**Data safety lens:** Speaks the language of business continuity. Users who think in RPO/RTO
can express their actual requirements; Urd translates to BTRFS operations.

---

### 19. "Backup Score" as Strategy Completeness Metric

A single 0-100 score that aggregates strategy compliance across all subvolumes:

```
Urd Backup Score: 73/100

  opptak:     92 (resilient, GFS, offsite current)
  docs:       81 (protected, offsite stale by 5 days)
  tmp:        45 (guarded only — consider external backup)
  databases:  74 (protected, no verified restore in 60 days)
```

The score is composable: each strategy dimension (copies, offsite, immutability, verification,
retention depth) contributes to the total. Users see at a glance whether their data
protection is improving or degrading.

**Data safety lens:** Aggregates complex multi-dimensional state into a single actionable
number. The risk: score gamification could lead to chasing numbers rather than real safety.
Mitigation: weight the score toward the dimensions that matter most (copies > verification >
retention depth).

---

### 20. Strategy-Specific Sentinel Behaviors

The Sentinel daemon adapts its monitoring and alerting based on the declared strategy:

- **3-2-1:** Alert when offsite drive hasn't been seen in > 14 days
- **3-2-1-1-0:** Additionally alert when no verified restore in > 30 days
- **4-3-2:** Track two separate offsite locations; alert when either falls behind
- **Mirror+Snapshot:** Watch BTRFS device stats for early warning of mirror degradation
- **Tiered:** Alert when tier transitions are overdue (snapshot aging past warm tier
  without cold promotion)

The Sentinel already has a state machine (`sentinel.rs`). Strategy-aware behaviors
would be additional event handlers that fire based on the configured strategy.

**Data safety lens:** Makes the Sentinel's monitoring proportional to the user's stated
protection goals. A 3-2-1-1-0 user gets more aggressive monitoring than a guarded user.

---

### 21. One-Command Strategy Setup (The "Set and Forget" Dream)

The ultimate UX goal: a single command that goes from "I have drives" to "fully configured
and running":

```bash
$ urd setup
Urd has detected:
  - 3 subvolumes on /mnt/btrfs-pool
  - 2 external drives (WD-18TB, WD-18TB1)
  - systemd timer (daily at 04:00)

? What strategy do you want for your most important data?
  > 3-2-1 (recommended for your setup)
    3-2-1-1-0 (needs monthly offsite rotation)
    Custom (I'll configure manually)

? Which subvolumes contain irreplaceable data?
  [x] subvol3-opptak (recordings)
  [x] subvol1-docs (documents)
  [ ] subvol6-tmp (temporary)

Setting up:
  ✓ opptak: resilient, drives = [WD-18TB, WD-18TB1], GFS retention
  ✓ docs: protected, drives = [WD-18TB], standard retention
  ✓ tmp: guarded, local only, 7-day retention
  ✓ WD-18TB1 designated as offsite rotation drive
  ✓ Config written to ~/.config/urd/urd.toml
  ✓ First backup scheduled for next timer run

Your data is now protected. Urd will handle everything from here.
Run `urd status` any time to check your protection promises.
```

**Data safety lens:** The highest-leverage idea for adoption. Most data loss happens because
backups are never properly configured. If setup takes 30 seconds instead of 30 minutes, more
data gets protected.

---

### 22. Strategy Degradation Cascade

When a strategy's requirements are partially met, show the effective strategy rather than
just "AT RISK":

```
opptak: configured as 3-2-1-1-0, effectively running as 3-2-1
  ✓ 3 copies
  ✓ 2 media types
  ✓ 1 offsite
  ✗ immutable copy not configured → degraded from 3-2-1-1-0 to 3-2-1
  ✗ no verified restore → "0 errors" unmet
```

The user sees both what they asked for and what they have. The gap is the action item.

**Data safety lens:** Honest about the actual protection state. "3-2-1-1-0 degraded to
3-2-1" is more informative than "AT RISK."

---

### 23. External Send Pipeline Abstraction (Uncomfortable Idea)

Currently, Urd only does `btrfs send | btrfs receive`. To support tiered archival and
cloud targets, abstract the send pipeline:

```rust
pub trait SendTarget {
    fn send(&self, snapshot: &Path, parent: Option<&Path>) -> Result<()>;
    fn verify(&self, snapshot: &Path) -> Result<bool>;
    fn list_snapshots(&self) -> Result<Vec<SnapshotName>>;
}

// Implementations:
// BtrfsSendReceive — current behavior
// RcloneTarget — send to cloud (B2, S3, Glacier)
// RsyncTarget — send to non-BTRFS remote
```

This would let Urd's planner and executor work with heterogeneous targets. A `btrfs send`
to local drives + `rclone` to Backblaze = multi-tier strategy in one config.

**Data safety lens:** Enables strategies that currently require external scripts. But the
abstraction is large and breaks the "all btrfs calls through BtrfsOps" invariant. Would
need careful ADR work.

---

### 24. Strategy Compliance History (Trend Over Time)

Track strategy compliance over time in the state DB and show trends:

```
Strategy compliance (last 90 days):
  3-2-1 requirements met: 87% of days
  Offsite drive connected: 12 of 90 days (13%)
  Longest gap without offsite sync: 23 days
  Verified restores: 2 (target: 3)
```

This gives the user a historical view of how well they're actually following their
declared strategy. A user who set `resilient` but never rotates the offsite drive sees
the pattern.

**Data safety lens:** Accountability. The strategy promise is only as good as the user's
behavior. Showing the trend makes gaps visible before they become emergencies.

---

### 25. Pre-Configured Drive Labels by Strategy Role (Uncomfortable Idea)

What if Urd wrote strategy metadata to the drive itself?

When a drive is designated as `role = "offsite"` in Urd's config, Urd creates a small
metadata file on the drive root: `.urd-drive-metadata.json` containing the drive's role,
the machine it serves, and the last sync date. When the drive is connected to any machine,
Urd can read this metadata and understand the drive's purpose.

This enables multi-machine awareness (idea 17) and makes drive rotation self-documenting:
the drive itself knows it's an offsite rotation drive for machine X.

**Data safety lens:** Reduces the chance of misconfiguration when rotating drives. The
drive's purpose travels with the drive.

---

## Handoff to Architecture

The following ideas deserve deeper `/design` analysis:

1. **Strategy verification layer (`urd doctor --strategy`, idea 3)** — highest value for
   lowest implementation cost. Uses existing awareness model and drive roles. Could ship
   as a pure diagnostic without changing any core behavior. Natural complement to the
   existing `urd doctor` command.

2. **Restore verification as promise obligation (idea 6)** — addresses the most dangerous
   gap in current backup systems: untested restores. The "0 errors" concept from 3-2-1-1-0
   is the most underserved leg. Maps naturally to a new awareness dimension.

3. **Strategy maturity ladder (idea 14)** — the most natural UX for progressive disclosure.
   Users see where they are and what comes next. Works with the existing promise model
   without replacing it. Could be the foundation for the progressive disclosure feature
   (6-O on roadmap).

4. **One-command strategy setup (idea 21)** — the ultimate "set and forget" enabler.
   Combines drive detection, strategy recommendation, and config generation. Aligns with
   the guided setup wizard design (design-h). Most impactful for new users.

5. **GFS surfacing + yearly retention (ideas 5 + 15)** — the easiest architectural win.
   The current graduated retention IS GFS; naming it explicitly and adding `yearly` completes
   the model. Low risk, high clarity.
