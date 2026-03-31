# Design: Guided Setup Wizard (`urd setup`)

> **TL;DR:** A conversational config generator framed around failure scenarios. The norn
> asks what fate you would defy. From your answers, she derives protection levels, drive
> assignments, and retention policies. You never think about 3-2-1. You think about what
> matters and what you would lose.

**Date:** 2026-03-31
**Status:** Reviewed
**Origin:** Idea H from 2026-03-30 brainstorm (transient workflow & redundancy guidance)
**Score:** 10/10 -- but conditional on execution quality. A standard wizard is 7/10.
The voice is what elevates or sinks this feature.

---

## Review findings incorporated

Review: `docs/99-reports/2026-03-31-design-h-review.md`

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | HIGH | Config replacement orphaning snapshots | New "Migration safety" section added. Wizard defaults to evaluate/adjust mode when config exists. |
| 2 | HIGH | Config lacks Serialize | Added as prerequisite refactor in effort estimate (+1 session). |
| 3 | MEDIUM | Subvolume discovery does not exist | Tiered discovery approach added to Phase 1 with filtering strategy. |
| 4 | MEDIUM | --evaluate creates divergent code path | Evaluate mode MUST reuse `awareness.rs assess()`. No independent reimplementation. |
| 5 | MEDIUM | Privilege requirements underspecified | Tiered privilege approach documented in Phase 1. |
| 6 | LOW | Recovery depth + run_frequency | Retention preview (idea N) handles this; wizard delegates to N's computation. |
| -- | Voice | Phase 2 intro is fortune-cookie territory | Noted for rework. Consider merging Phases 2+4 to avoid two passes over subvolumes. |

---

## The problem

Users configure backups by thinking about operations -- intervals, retention counts, drive
labels. But they *want* their data to be safe. The gap between what users want (survival)
and what they configure (TOML fields) is where data loss hides.

A user who sets `protection_level = "protected"` may not realize their only external drive
sits next to their machine. A user who sets `daily = 30` may not know whether 30 days of
local retention actually matters for their use case. A user who skips offsite because it
sounds complicated may lose irreplaceable photos to a house fire.

The config file is honest about *what* it does. It says nothing about *why*.

**Urd's config system asks the wrong question.** It asks "how do you want your backups
configured?" when it should ask "what would you lose if the worst happened?"

## The solution

`urd setup` -- an interactive command that generates `urd.toml` by asking about intent,
not implementation.

The encounter is structured as a consultation. Urd discovers the filesystem, asks what
matters and what disasters to survive, and derives a complete config. The user approves
or adjusts. The output is a standard `urd.toml` -- no special format, no lock-in.

---

## Design philosophy: the encounter with Urd

This is Urd's primary speaking role. The mythological voice must serve the UX, not
decorate it. If the voice makes the experience worse -- more confusing, more precious,
slower -- it has failed.

### Principles

1. **Urd asks about fate, not features.** "What would you preserve if fire took this
   place?" -- not "How many external drives do you want?" The questions are about loss
   and survival. The config fields are Urd's problem, not the user's.

2. **She is direct and unflinching.** She names the disasters plainly. Drive failure.
   Fire. Theft. She does not soften the question. The user should feel the weight of
   what is at stake -- because the weight is real.

3. **She is wise, not theatrical.** Short sentences. No flourishes for their own sake.
   Every word earns its place. The tone is closer to a surgeon explaining a procedure
   than a fortune teller reading cards.

4. **She derives, she does not ask.** From answers about what matters and what you fear,
   she computes the config. She shows what she carved and why. You approve or adjust.
   She never asks "what snapshot_interval do you want?" -- she tells you what interval
   your answers require.

5. **The relief comes from clarity.** After the encounter, you know exactly what is
   protected, what is at risk, and what you chose to leave behind. That clarity is the
   gift. Not the voice. Not the mythology. The knowing.

### Anti-patterns (from user feedback)

- **Do not assume specific user behaviors.** No "when you visit the bank" or "every
  Monday morning." Urd is eternal, not prescriptive. She knows what the user declared;
  she does not know their schedule.
- **Do not overexplain.** If a sentence adds no insight, delete it. Elegance is brevity
  that loses nothing.
- **Do not be cheesy.** "The runes whisper of your home directory" is embarrassing. The
  mythology must earn its presence through genuine insight. If a plain sentence works
  better, use the plain sentence.
- **Do not be a personality.** Urd is not chatty. She does not make small talk. She does
  not congratulate you for good answers. She acknowledges and moves on.

---

## Conversation flow

Five phases. Each phase has a clear purpose and a defined output. The conversation is
linear -- no branching trees, no going back. Adjustments happen at the end.

**Open design question:** Phases 2 and 4 both iterate over subvolumes. Consider merging
them to avoid two passes over the same list. Per-subvolume: ask importance, then
immediately ask recovery depth if importance warrants it. This reduces classification
fatigue for users with many subvolumes. Decision deferred to implementation.

### Phase 1: Discovery

Urd detects the filesystem state automatically. No questions.

**Actions:**
- Discover BTRFS subvolumes using tiered approach (see below)
- Scan for mounted external drives (reuse `drives.rs` detection)
- Check for existing `urd.toml` and existing snapshots (see "Migration safety" section)
- Identify snapshot roots (existing `.snapshots` directories)

**Tiered subvolume discovery:**

Discovery proceeds through tiers, stopping when sufficient information is gathered.
Each tier is attempted in order; higher tiers require more privilege.

1. **`/etc/fstab` parsing** (unprivileged). Read `subvol=` mount options to find
   user-declared subvolumes. This covers the common case where subvolumes are
   explicitly mounted.
2. **`findmnt -t btrfs`** (unprivileged). Enumerate currently mounted BTRFS
   filesystems and their subvolume mount points. Catches subvolumes not in fstab
   (e.g., manually mounted).
3. **`btrfs subvolume list`** (requires sudo). Full enumeration of all subvolumes.
   The wizard asks for consent before escalating: "I can see more with root access.
   May I?" If declined, proceed with what tiers 1-2 found.
4. **Manual entry fallback.** If no subvolumes are discovered (or the user wants to
   add unlisted ones): "I could not see your volumes. Tell me what to protect."

**Subvolume filtering:** Raw discovery will surface subvolumes the user does not care
about. The wizard filters aggressively before presenting results:

- Docker per-layer subvolumes (`/var/lib/docker/btrfs/subvolumes/*`)
- Snapper snapshot subvolumes (`.snapshots/*/snapshot`)
- Timeshift snapshot subvolumes
- Urd's own snapshot subvolumes (anything under configured snapshot roots)
- Nested subvolumes that are children of already-listed subvolumes (present the
  parent, not the internal structure)

Filtered subvolumes are not hidden -- the wizard notes them: "I also see 34 Docker
layers and 12 existing snapshots. These are not shown." This prevents confusion if
the user expects to see them.

**Output to user:**

```
I see what you have.

  Volumes:  @home (118 GB, NVMe)
            @docs (2.1 TB, btrfs-pool)
            @pics (1.8 TB, btrfs-pool)
            @root (118 GB, NVMe)

  Drives:   WD-18TB (18 TB, mounted)
            WD-18TB1 (18 TB, not mounted)

  (Also found: 34 Docker layers, 12 Urd snapshots — not shown.)
```

Short. Factual. No embellishment. The user confirms Urd sees what they expect.

**Design note:** The discovery output uses plain names derived from subvolume paths, not
internal identifiers. If a subvolume is mounted at `/home`, the user sees `@home`, not
`subvol_id=258`. The `@` prefix follows BTRFS convention for subvolume names.

### Phase 2: What matters

For each subvolume (or logical group), Urd asks one question: how precious is this data?

**Classification:**

| Answer | Meaning | Maps to |
|--------|---------|---------|
| Irreplaceable | Cannot be recreated. Photos, recordings, writings. | resilient candidate |
| Important | Can be recreated but at significant cost. Configs, projects. | protected candidate |
| Replaceable | Can be reinstalled or regenerated. OS, caches, media libraries. | guarded candidate |
| Expendable | Loss is acceptable. Temp files, build artifacts. | guarded or excluded |

**DRAFT voice** (subject to iteration -- Phase 2 intro needs rework per review; the
current phrasing is fortune-cookie territory. Consider dropping the preamble entirely
and letting the classification options speak for themselves):

```
Tell me what each of these is worth.

  @home     — irreplaceable / important / replaceable / expendable?
  @docs     — irreplaceable / important / replaceable / expendable?
  @pics     — irreplaceable / important / replaceable / expendable?
  @root     — irreplaceable / important / replaceable / expendable?
```

**Design note:** The question groups subvolumes by mount point device when possible. All
subvolumes on `btrfs-pool` appear together. NVMe subvolumes appear together. This helps
the user think in terms of "what lives where" rather than arbitrary list order.

**UX consideration:** For users with many subvolumes (10+), offer to classify in bulk:
"Most of your volumes live on btrfs-pool. Are they broadly the same importance, or should
I ask about each?" This prevents fatigue without losing precision.

### Phase 3: What you fear

One question about disaster scope. This determines the geographic distribution of backups.

**DRAFT voice:**

```
What ruin would you guard against?

  1. Drive failure — a disk dies, but the machine survives
  2. Site loss    — fire, theft, flood. Everything here is gone.
```

If the user chooses site loss and has no offsite-capable drive, Urd notes the gap plainly:

```
You have no drive kept away from this place.
Without one, I cannot protect against site loss.

I will configure for drive failure. When you have an offsite drive,
run `urd setup --evaluate` to strengthen the weave.
```

She does not lecture. She does not suggest buying a drive. She states the constraint and
adapts. The user feels respected, not nagged.

**Design note:** If the user has drives with `role = "offsite"` (or Urd can infer offsite
from connection patterns -- drives seen infrequently), this changes the derivation:
irreplaceable + site loss = resilient with offsite drive assignment.

### Phase 4: How deep

For subvolumes marked important or irreplaceable, Urd asks about recovery depth. This
determines retention policy.

**DRAFT voice:**

```
How far back would you need to reach?

  @home  — a week / a month / a season / a year
  @pics  — a week / a month / a season / a year
```

Urd then shows what that means concretely (integrating idea N, retention preview):

```
  @home, one month:
    Local:     30 daily snapshots, then weekly for 6 months
    WD-18TB:   30 daily, then weekly thinning
    Recovery:  You can reach any day in the last month.
               Older than that, you can reach any week for 6 months.

  @pics, one season:
    Local:     Same graduated retention
    WD-18TB:   90 daily, then monthly
    Recovery:  Any day in the last 3 months.
```

For replaceable/expendable subvolumes, Urd skips this question and assigns minimal
retention automatically. She mentions it but does not ask:

```
  @root:  transient — sent to external, no local history.
  @tmp:   guarded, 7-day local retention. No external copies.
```

**Note on run_frequency interaction:** The retention preview (idea N) accounts for
`run_frequency` when computing actual snapshot counts and space estimates. The wizard
delegates retention computation to N's functions rather than implementing its own.
This ensures the wizard's retention preview matches what `urd plan` will actually do.

### Phase 5: The runestone

Urd presents the derived config as a summary, then shows the TOML.

**DRAFT voice:**

```
This is what I would carve.

  PROTECTION
    @home     RESILIENT   local + WD-18TB + WD-18TB1    1 month depth
    @pics     RESILIENT   local + WD-18TB + WD-18TB1    3 month depth
    @docs     PROTECTED   local + WD-18TB               1 month depth
    @root     TRANSIENT   WD-18TB1 only                 latest only
    @tmp      GUARDED     local only                    7 days

  GAPS
    No offsite rotation detected. If WD-18TB1 stays beside WD-18TB,
    site loss protection is nominal, not real.

  Shall I write this to ~/.config/urd/urd.toml?
```

The GAPS section is critical. This is where Urd earns the mythological framing -- she
names what the config *cannot* protect against. The user chose site loss protection but
both drives are in the same room? She says so. Not as an error. As a fact the user must
sit with.

**After approval**, Urd writes the config and shows next steps:

```
Written.

  Next:  urd plan        — see what the first run will do
         urd backup      — begin
         urd status      — check promise states after the first run
```

---

## Migration safety

**This section addresses the risk of config replacement orphaning existing snapshots.**

When `urd setup` detects an existing `urd.toml`, the wizard MUST behave differently
than on a fresh system. The project has a history of snapshot congestion causing
catastrophic storage failure -- orphaning snapshots is not a cosmetic problem.

### Default behavior with existing config

If `urd.toml` exists, the wizard runs in **evaluate/adjust mode** by default, not
replace mode. The user sees:

```
You already have a configuration. I can:

  1. Evaluate  — assess your current config against disaster scenarios
  2. Adjust    — walk through the wizard, keeping existing names and roots
  3. Replace   — start fresh (requires explicit confirmation)
```

Option 1 is the default (just pressing enter). Option 3 requires the user to type
the word "replace" -- not just a number. This friction is intentional.

### Name and root conflict detection

If the wizard generates new subvolume names that differ from existing ones (e.g., the
wizard derives `@home` but the existing config uses `htpc-home`), it MUST:

1. Load the existing config and enumerate all configured subvolume names and
   snapshot roots
2. Scan for existing snapshots on disk under those names
3. Compare the wizard's proposed names against existing names
4. If any names differ, warn explicitly:

```
  WARNING: Name changes detected.

    Existing: htpc-home  (47 snapshots on disk)
    Proposed: @home

  Renaming will make 47 existing snapshots invisible to Urd.
  They will not be deleted, but they will consume space and their
  incremental chains will break. Your next backup will do a full send.

  Continue with new names? (type 'yes' to confirm)
```

The same detection applies to snapshot root changes and drive assignment changes.

### Post-write guidance

After writing a replacement config, the wizard always recommends:

```
  Config written. Because this replaces an existing configuration:

    urd plan        — review what the first run will do
    urd backup --dry-run  — verify before committing
```

---

## The `--evaluate` mode

`urd setup --evaluate` runs against an existing config. It does not generate a new config.
Instead, it maps the existing config to the disaster framework and surfaces gaps.

This is the "second consultation" -- the user already has a config and wants to know if
it is sufficient.

**Architectural constraint:** The evaluate mode MUST reuse the `awareness.rs` `assess()`
function for computing promise states and coverage. It does not implement its own analysis.
The evaluate mode calls `assess()` and presents results through `setup_voice.rs` with the
disaster framing. This ensures a single source of truth about what survives what.

The flow is: `assess()` produces promise states per subvolume -> evaluate mode maps those
states to disaster scenarios (drive failure, site loss) -> `setup_voice.rs` renders the
result with the wizard's voice.

**Output:**

```
I read your current weave.

  WHAT SURVIVES
    Drive failure:    @home, @pics, @docs, @root (all have external copies)
    Site loss:        @home, @pics (on WD-18TB1, if kept offsite)

  WHAT DOES NOT SURVIVE
    Site loss:        @docs (only on WD-18TB, same location as machine)
                      @root (only on WD-18TB1, same location as machine)

  GAPS
    WD-18TB1 last seen 14 days ago. If it is offsite, your offsite
    copies are 14 days stale. If it is in a drawer beside you,
    it provides no site loss protection.

  Run `urd setup` to reconfigure, or adjust urd.toml directly.
```

This integrates directly with idea G (coverage map). The evaluate mode is essentially
the coverage map with a disaster framing. If G is implemented as a standalone pure
function, the evaluate mode calls it. If G does not exist yet, the evaluate mode still
uses `assess()` as its foundation -- never a parallel reimplementation.

---

## Module decomposition

| Module | Responsibility |
|--------|---------------|
| `commands/setup.rs` | CLI handler, conversation state machine, user input loop |
| `setup_voice.rs` | All wizard text -- questions, transitions, summaries. Pure functions: phase state in, text out. |
| `setup.rs` (lib) | Config derivation from wizard answers. Pure function: `WizardAnswers -> Config`. |
| `drives.rs` | Drive discovery (extend existing `detect_mounted_drives`) |
| `btrfs.rs` | Subvolume discovery via `btrfs subvolume list` (new `BtrfsOps` method) |
| `config.rs` | Config serialization (write `urd.toml` from `Config` struct -- requires `Serialize` on all config types) |

### Why `setup_voice.rs` instead of extending `voice.rs`

The setup wizard has a fundamentally different voice register than status/plan output.
Status output is terse and pragmatic ("OK", "aging", "gap"). The wizard is conversational
and carries the mythological tone. Mixing them in one file would create a split
personality. `setup_voice.rs` is still the presentation layer -- it renders structured
data as text -- but its vocabulary is distinct.

If the codebase later develops a unified voice register, the two can merge. For now,
separation protects both from contaminating each other.

### Answer types

```rust
/// What the user told Urd during setup.
pub struct WizardAnswers {
    pub subvolumes: Vec<SubvolumeIntent>,
    pub disaster_scope: DisasterScope,
    pub drives: Vec<DiscoveredDrive>,
}

pub struct SubvolumeIntent {
    pub path: PathBuf,
    pub name: String,
    pub importance: Importance,
    pub recovery_depth: Option<RecoveryDepth>,
}

pub enum Importance {
    Irreplaceable,
    Important,
    Replaceable,
    Expendable,
}

pub enum DisasterScope {
    DriveFailure,
    SiteLoss,
}

pub enum RecoveryDepth {
    Week,
    Month,
    Season,
    Year,
}
```

The derivation function `derive_config(answers: &WizardAnswers) -> Config` is pure. It
maps:

| Importance | Disaster scope | Protection level |
|-----------|----------------|-----------------|
| Irreplaceable | Site loss | Resilient (offsite drive required) |
| Irreplaceable | Drive failure | Protected (any external drive) |
| Important | Site loss | Protected (offsite if available, else any) |
| Important | Drive failure | Protected |
| Replaceable | Either | Guarded |
| Expendable | Either | Guarded (minimal) or excluded |

Recovery depth maps to retention:

| Depth | Local retention | External retention |
|-------|----------------|-------------------|
| Week | daily=7, weekly=4 | daily=7 |
| Month | daily=30, weekly=13 | daily=30, weekly=4 |
| Season | daily=30, weekly=26, monthly=3 | daily=30, weekly=13 |
| Year | daily=30, weekly=52, monthly=12 | daily=30, weekly=26, monthly=12 |

Space-constrained volumes (NVMe with < 256 GB) get transient local retention
automatically for replaceable subvolumes. Urd mentions this in the summary.

---

## Prerequisites

### Config Serialize support (implementation blocker)

The `Config` struct in `config.rs` currently derives only `Deserialize`, not `Serialize`.
The wizard's core output is writing a `Config` to TOML. Adding `Serialize` to `Config`
and all nested types is a prerequisite refactor that must happen before the wizard can
write TOML.

**Types requiring `Serialize`:** `Config`, `Interval`, `ByteSize`, `GraduatedRetention`,
`LocalRetentionConfig`, `ProtectionLevel`, `DriveRole`, `RunFrequency`,
`NotificationConfig`, and all other types nested in `Config`.

**Estimated effort:** ~1 session.

**Side benefits:** This refactor also enables future features: config migration tooling,
`urd config show`, config round-tripping, and the round-trip test in this design's test
strategy.

This is not optional -- the round-trip test (derive config from answers, serialize to
TOML, parse back, verify equivalence) depends on it, and string-templated TOML generation
would be fragile, hard to test, and would violate the pure-function design.

---

## Integration with other ideas

### Idea E: Promise levels that encode redundancy

The wizard's importance/disaster mapping directly produces the promise levels from idea E.
If E is implemented first, the wizard generates configs that use the richer promise
taxonomy. If the wizard ships first, it generates configs using the current taxonomy
(guarded/protected/resilient/custom) and gains richer semantics when E lands.

No hard dependency in either direction.

### Idea G: Coverage map

The `--evaluate` mode is the coverage map with a disaster framing. If G is implemented as
a standalone pure function (`fn coverage_map(config, awareness_state) -> CoverageReport`),
the evaluate mode calls it and renders through `setup_voice.rs`. If G does not exist yet,
the evaluate mode builds on `assess()` from `awareness.rs` -- never a parallel
reimplementation.

### Idea N: Retention preview

Phase 4 shows retention implications inline. If N is implemented as
`fn retention_preview(policy) -> RetentionSummary`, Phase 4 calls it. Otherwise, Phase 4
computes a simplified version directly. The retention preview function handles
`run_frequency` interaction, so the wizard does not need to account for it independently.

---

## ADR gate

**Does `urd setup` need an ADR?**

No. It is a new CLI command that generates standard `urd.toml`. It does not change:
- On-disk data formats (ADR-105)
- Config schema (it generates configs in the existing schema)
- Module boundaries (new modules follow existing patterns)
- Architectural invariants (the wizard is a presentation concern)

If the wizard needs to *extend* the config schema (e.g., storing wizard metadata in the
TOML), that would require an ADR-111 amendment. The current design avoids this -- the
output is a plain `urd.toml` with no wizard-specific fields.

---

## Test strategy

### The hard problem: testing a conversational UI

The wizard is an interactive readline loop. Testing the full loop end-to-end requires
simulating user input, which is fragile and slow. The architecture avoids this by making
every interesting function pure.

### What to test

1. **Config derivation (exhaustive).** `derive_config()` is a pure function. Test every
   combination of importance x disaster_scope x recovery_depth x drive availability.
   This is where correctness matters most -- wrong derivation means wrong protection.
   ~30-50 tests.

2. **Discovery parsing.** `btrfs subvolume list` and `findmnt` output parsing. Test with
   real output samples. Edge cases: nested subvolumes, unmounted subvolumes, subvolumes
   on multiple devices. Include filtering tests: Docker layers, snapper snapshots, and
   Urd's own snapshots must be filtered out.

3. **Voice rendering.** `setup_voice.rs` functions take structured data and return strings.
   Test that output contains expected content (subvolume names, protection levels, gap
   warnings). Do not test exact wording -- the voice will iterate.

4. **Evaluate mode.** Given a config and awareness state, verify that the coverage
   analysis correctly identifies gaps. Test: single drive, no offsite, stale offsite,
   transient with no external. Verify that evaluate mode produces results consistent
   with `awareness.rs assess()` -- no divergence.

5. **Config serialization.** Round-trip test: derive config from answers, serialize to
   TOML, parse back, verify equivalence. (Depends on Serialize prerequisite.)

6. **Migration safety.** Test name conflict detection: existing config with `htpc-home`,
   wizard proposes `@home`, verify warning is generated. Test snapshot scanning: existing
   snapshots on disk are counted and reported.

### What not to test

- The interactive input loop itself (test the functions it calls instead)
- Exact voice text (it will change; test structure and content presence)
- Terminal rendering (colors, alignment)

---

## Risks

### The cheesy risk

This is the primary risk and it must be named plainly.

The mythological voice is one bad sentence away from cringe. "The runes have spoken" after
generating a TOML file would undermine the entire feature. The voice must be earned
through genuine insight -- naming real disasters, surfacing real gaps, providing real
clarity.

**Mitigation:** The voice lives in `setup_voice.rs`, fully separated from logic. It can
be rewritten without touching any other module. Plan for at least one full rewrite of the
voice text. Draft voice in this document is explicitly marked DRAFT. The voice needs
user testing -- read it aloud, show it to someone who does not know Urd.

**Litmus test for every line:** "Would I be embarrassed if a senior engineer saw this on
my screen?" If yes, rewrite.

### The scope risk

This is a large feature. The conversation framework, discovery, derivation, voice, and
evaluate mode could easily expand to fill any amount of time. The voice iteration alone
could consume multiple sessions.

**Mitigation:** Build in vertical slices.
- Slice 1: Discovery + hardcoded voice + derivation for a single subvolume. End-to-end
  working, ugly text.
- Slice 2: Full conversation flow with placeholder voice. All phases work.
- Slice 3: Voice iteration. This is where the feature lives or dies.
- Slice 4: Evaluate mode, edge cases, polish.

Ship after slice 2 if the voice is not ready. A working wizard with plain text is better
than no wizard.

### The assumption risk

The wizard assumes users think in terms of "what matters" and "what disasters." Some users
think in terms of operations and will be frustrated by indirect questions. The wizard
should never *prevent* direct config editing -- it is an alternative entry point, not a
gate.

**Mitigation:** `urd setup` is optional. The config file remains the primary interface.
The wizard writes a standard `urd.toml` that can be edited directly afterward.

### The discovery risk

BTRFS subvolume discovery is system-dependent. Nested subvolumes, bind mounts, non-standard
layouts, and permissions issues can all produce confusing or incomplete discovery results.

**Mitigation:** Tiered discovery approach (see Phase 1) degrades gracefully. If
unprivileged detection finds nothing, ask consent for sudo. If sudo is declined or
fails, fall back to manual entry. The wizard never refuses to run because discovery
failed.

---

## Effort estimate

4-5 sessions, including the Serialize prerequisite refactor.

| Session | Deliverable |
|---------|------------|
| 0 | **Prerequisite:** Add `Serialize` to `Config` and all nested types. Round-trip test. |
| 1 | Discovery + conversation framework + answer types + derivation |
| 2 | Full conversation flow + config serialization + retention preview integration |
| 3 | Voice layer -- the hardest session. Iterate on tone, read aloud, revise. |
| 4 | `--evaluate` mode (using `assess()`), migration safety, edge cases, polish, tests |

---

## Open questions for review

1. **Is the conversation flow right?** Five phases, linear, no branching. Is this too
   rigid? Should any phase be skippable? Consider merging Phases 2+4 -- decision
   deferred to implementation.

2. **Is the voice earned?** Read the DRAFT text samples above. Do they add clarity or
   just decoration? Where does the voice help, and where does it get in the way?
   Phase 2 intro needs rework before implementation.

3. **Grouping strategy.** Should subvolumes be presented individually or grouped by
   device/mount? Grouping reduces fatigue for large setups but might hide important
   distinctions.

4. **Should the wizard detect existing snapshots?** Yes -- for migration safety (see
   new section). The wizard reports existing snapshot counts to inform the user but
   does not attempt to adopt or reconfigure existing snapshot state.

5. **Recovery depth granularity.** Four options (week/month/season/year) -- is this enough?
   Too many? "Forever" is not offered -- it implies unbounded retention, which conflicts
   with space management. "Year" is the practical maximum. Users who need longer can
   edit the TOML directly.
