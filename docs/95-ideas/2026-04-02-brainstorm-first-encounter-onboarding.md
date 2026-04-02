# Brainstorm: The First Encounter — Interactive Onboarding

**Date:** 2026-04-02
**Status:** raw
**Origin:** User prompt — how to make the first encounter with Urd an interesting,
conversational experience that discovers the user's setup and proposes protection levels.

## Context

Today `urd init` is a diagnostic check — it validates an existing config. There is no
guided path from "I just installed Urd" to "my data is protected." The user must read
an example config, understand subvolumes, drives, retention, and protection levels,
then write TOML by hand. This is the wrong first experience for a tool whose north star
is reducing the attention users spend on backups.

The vision: Urd's first encounter is a conversation. She discovers the user's world
(disks, subvolumes, data types), explores what the user fears losing, and proposes
the strongest promise she can keep. The user walks away with a working config and
the understanding that Urd knows what matters.

## Relationship to existing work

- **Design O (progressive disclosure):** Teaches over time *after* setup. This brainstorm
  is about the *initial* setup. They're complementary — onboarding establishes the
  relationship, progressive disclosure deepens it.
- **ADR-111 (config architecture):** The target config schema. Onboarding should generate
  configs in the ADR-111 format, making it an early adopter of the new schema.
- **`urd init` (current):** Diagnostic check for existing configs. The onboarding flow
  would be a separate command or a mode of `urd init` when no config exists.

---

## Ideas

### 1. The Encounter: `urd` with no config triggers onboarding

When a user runs any `urd` command and no config file exists at the expected path,
Urd doesn't error out — she introduces herself and offers to begin the encounter.
No need for a separate `urd setup` command. The absence of config *is* the trigger.

"You have no threads woven yet. Shall I examine your world and propose what to protect?"

This follows the UX principle of guiding through affordances. The user doesn't need to
know that `urd setup` exists — Urd notices they need help and offers it.

### 2. Drive Discovery: "Show me the looms you have"

Urd scans for BTRFS filesystems and mounted drives automatically using `btrfs filesystem show`
and `findmnt`. She presents what she finds and asks the user to assign roles:

- "I see a BTRFS filesystem on your NVMe at `/`. This is where your system lives."
- "I see a BTRFS volume at `/mnt/btrfs-pool` with 8TB. What kind of data lives here?"
- "There's an external drive mounted at `/run/media/<user>/WD-18TB`. Is this for backups?"

For each drive, Urd asks: **What role does this play?**
- Internal (always-on, primary backup target)
- External (connected regularly, primary backup)
- Offsite (carried away for geographic separation)
- Cloud (future — rclone/restic target)

The technical implementation: `drives.rs` already has `is_drive_mounted()` and filesystem
detection. Extend with `discover_btrfs_filesystems()` that wraps `btrfs filesystem show`
to find all BTRFS volumes, and `discover_subvolumes()` that lists subvolumes on each.

### 3. Subvolume Discovery: "What threads already exist?"

Urd lists all BTRFS subvolumes on discovered filesystems using `btrfs subvolume list`.
For each, she asks what kind of data it holds:

- **Irreplaceable** — photos, personal documents, recordings, creative work
- **Important** — configuration, code, containers, project files
- **Reproducible** — downloads, media libraries, package caches
- **Ephemeral** — temp files, build artifacts, browser caches

This maps directly to protection levels: irreplaceable → resilient,
important → protected, reproducible → guarded, ephemeral → skip or guarded with
minimal retention.

### 4. The Fate Conversation: "Let us speak of what could go wrong"

This is the soul of the encounter. Instead of asking abstract questions about "redundancy
levels," Urd walks through concrete disaster scenarios and asks: "Would your data survive?"

The scenarios escalate:

1. **"Your disk fails."** — The most common disaster. If you have no backup, everything
   on that disk is gone. → Tests: do you have at least one external copy?

2. **"Your computer loses power during a write."** — BTRFS handles this via CoW, but
   unfinished transfers are real. → Tests: do you have snapshot integrity?

3. **"Your computer is stolen / destroyed."** — Fire, theft, hardware death. Everything
   in your home is gone. → Tests: do you have an external drive you can disconnect?

4. **"Your house floods / burns."** — Regional disaster. Every drive in the building is
   gone. → Tests: do you have offsite storage? A drive at work, at a friend's, in a
   safe deposit box?

5. **"You have to leave everything behind."** — War, natural disaster, forced migration.
   You take only what you can carry. → Tests: do you have cloud storage or a small
   portable drive? Is your data encrypted for travel?

6. **"Someone deletes your files maliciously."** — Ransomware, angry ex, rogue process.
   → Tests: are your backups read-only (BTRFS snapshots are)? Are they on a separate
   filesystem the attacker can't reach?

7. **"You make a mistake and don't notice for weeks."** — Silent data corruption, accidental
   deletion noticed late. → Tests: do you have retention history going back far enough?

After each scenario, Urd reflects on what the user's current setup would mean:

- "With what you've shown me: if your NVMe fails tomorrow, your photos survive on WD-18TB.
  But your system configuration would be gone."
- "If your house floods, nothing survives. You have no thread that reaches beyond these walls."

The conversation isn't a lecture — it's a mirror. Urd shows the user their own risk profile.

### 5. The Promise Proposal: "This is what I can weave for you"

Based on what Urd learned (drives, subvolumes, data types, disaster tolerance), she
proposes protection levels for each subvolume:

```
Based on what you've told me, here is what I can promise:

  RESILIENT    htpc-home        Your home directory, on WD-18TB + WD-18TB1
  RESILIENT    subvol3-opptak   Your recordings, on WD-18TB + WD-18TB1
  PROTECTED    subvol1-docs     Your documents, on WD-18TB
  GUARDED      subvol4-multi    Your media library, local snapshots only
  SKIP         subvol6-tmp      Temporary files, no protection needed

You lack offsite storage for documents and containers.
If misfortune strikes this home, those threads will break.
```

The proposal shows exactly what survives each disaster class. It's not "you should have
more drives" — it's "this is the consequence of your current setup."

### 6. Two Exits: "Are you satisfied, or do you wish to delve deeper?"

After the proposal, two paths:

**Path A: "Set and forget"** — User accepts the proposal. Urd generates the config,
writes it to `~/.config/urd/urd.toml`, runs `urd init` to validate, and sets up the
systemd timer. Done. The user's data is protected from this moment forward.

"Your threads are woven. I will watch over them. You need not think of this again."

**Path B: "Delve deeper"** — User wants fine-grained control. Urd presents config
options in a structured, progressive disclosure UI:

1. **Retention** — how long to keep snapshots (with sane defaults pre-filled)
2. **Intervals** — how often to snapshot and send (with defaults from run_frequency)
3. **Drive assignments** — which drives serve which subvolumes
4. **Notifications** — how Urd should alert on problems
5. **Advanced** — metrics, heartbeat, custom overrides

Each section shows the default with a "this is what I'd recommend" note. The user
only changes what they care about.

### 7. Scenario Simulator: Show the consequences live

As the user adjusts settings in "delve deeper" mode, Urd dynamically shows the impact:

- "If you reduce retention to 7 days, you cannot recover from mistakes older than a week."
- "If you remove WD-18TB1 from htpc-home, your recordings lose offsite protection."
- "With these settings, your data survives: disk failure ✓, theft ✓, house fire ✗"

This is `awareness.rs` and `preflight.rs` running in real-time during config editing.
Pure functions, no I/O — perfect for interactive feedback.

### 8. The Loom Metaphor: Threads, not drives

Throughout the encounter, Urd uses "threads" for backup chains and "weave" for the
overall protection fabric. Drives are "looms" — the instruments that hold the threads.

- "I see three looms where I could weave your threads."
- "Your photos are woven across two looms. If one breaks, the thread holds."
- "This thread reaches beyond your walls — your offsite loom carries it forward."

This is consistent with the existing voice vocabulary (thread for backup chains,
per the vocabulary decisions). The encounter establishes this language so that future
`urd status` output and progressive disclosure milestones speak the same way.

### 9. Filesystem Fingerprinting: Urd remembers drives by identity

During onboarding, Urd records each drive's UUID via `findmnt -o UUID`. When a drive
reconnects, Urd recognizes it by identity, not mount path. This is already in `DriveConfig`
as the optional `uuid` field — onboarding should populate it automatically.

"I've marked WD-18TB by its true name, not just where it sits. Even if it mounts
elsewhere, I'll know it's the same loom."

### 10. Cloud Thread: Acknowledge the gap honestly

Urd doesn't support cloud targets yet. But the fate conversation will naturally surface
the desire. Urd should acknowledge this honestly:

"You asked about protection beyond your walls. I cannot yet weave threads through the
cloud — that is a path I have not learned. For now, an offsite drive is your strongest
thread against regional disaster."

This sets expectations without overpromising. When cloud support eventually arrives,
the onboarding conversation naturally incorporates it.

### 11. Multi-machine awareness: "Do other machines share these looms?"

If Urd detects snapshots on external drives from unknown subvolume names (not in the
current config), she could ask:

"I see threads on WD-18TB from a machine I don't know — subvolumes named `laptop-home`
and `laptop-docs`. Do you have another machine that also weaves to this loom?"

This opens the door to multi-machine awareness without building it yet. Just
acknowledging the existence of other machines' data prevents the user from accidentally
running retention that deletes another machine's snapshots.

### 12. The Summary Scroll: A printed record of the encounter

After onboarding completes, Urd generates a readable summary:

```
╭─────────────────────────────────────────────────╮
│  The Threads of Your Data                       │
│                                                 │
│  3 looms discovered                             │
│  9 subvolumes examined                          │
│  7 threads woven                                │
│                                                 │
│  Survives:                                      │
│    Disk failure ........... yes                  │
│    Theft / fire ........... partially            │
│    Regional disaster ...... no                   │
│                                                 │
│  Config: ~/.config/urd/urd.toml                 │
│  Timer:  urd-backup.timer (daily at 04:00)      │
│                                                 │
│  Run `urd status` to see your protection state. │
╰─────────────────────────────────────────────────╯
```

### 13. Reversibility guarantee: "You can always return"

Onboarding should make clear that every choice is reversible:

"Nothing I do now is permanent. You can change any thread, add looms, or adjust your
promises at any time. Run `urd calibrate` to revisit these choices."

### 14. Dry-run first backup: Prove it works before trusting it

After config generation, Urd offers to run a dry-run backup immediately:

"Shall I show you what I would do on my first night watch? This will not change
anything — only plan."

Then runs `urd plan` and shows the user exactly what operations would happen.
If the user approves, offer to run the actual first backup right now (since waiting
until 04:00 means hours of unprotected time).

### 15. Guided sudoers setup

BTRFS operations need sudo. Onboarding should detect whether sudo is configured for
passwordless btrfs access and, if not, guide the user through setting it up:

"I need permission to work with your filesystems. Here is the sudoers line I need:"

```
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs *
```

"Add this to `/etc/sudoers.d/urd` and I can work without disturbing you."

### 16. Data Classification Intelligence: Smart defaults from filesystem analysis

Instead of asking "what kind of data is here?" for every subvolume, Urd could
make educated guesses by peeking at top-level directories:

- Contains `.git/`, `Cargo.toml`, `package.json` → code/project files → protected
- Contains `DCIM/`, `*.CR2`, `*.NEF`, `*.jpg` → photos → resilient
- Contains `*.mp4`, `*.mkv` → video → depends on replaceability
- Contains `.config/`, `.local/` → user config → protected
- Contains `node_modules/`, `target/`, `.cache/` → reproducible → guarded

"I see photos and raw camera files in subvol2-pics. These are likely irreplaceable.
Shall I weave them with my strongest thread?"

This is guidance, not assumption — Urd proposes based on evidence, the user confirms.

### 17. The Uncomfortable Ideas

**17a. Threat modeling as a service.** Urd could maintain a persistent threat model
document (`~/.config/urd/threats.md`) that maps the user's specific geographic,
infrastructure, and lifestyle risks to protection recommendations. Updated when the
user adds drives or changes their setup. Way beyond a backup tool's scope — but it's
what users actually need to think about.

**17b. Urd as the first conversation in a new relationship.** The encounter isn't just
functional — it's emotional. The user is confronting their mortality (data mortality,
but still). The scenarios about house fires and wars are genuinely uncomfortable.
Urd could lean into this: the conversation is a small act of care. "You've thought
about this now. Most people never do." This walks the line between backup tool and
existential counselor, but the mythic voice earns a little of this weight.

**17c. Witness mode.** After onboarding, Urd could offer to "witness" the user's
most precious files — letting the user explicitly name the 5-10 files or directories
they care about most. Not for technical reasons (protection levels handle it), but
for psychological ones. "Show me what matters most, and I will guard it with
particular care." This could tie into future restore UX: `urd get` could prioritize
witnessed files in suggestions.

### 18. TUI vs Pure CLI: The Interface Question

Should the encounter be:

**A. Pure CLI conversation** — stdin/stdout, question/answer, like a text adventure.
Simple to implement, works over SSH, accessible. But limited in showing dynamic
feedback (scenario 7) and multi-option selection.

**B. TUI with ratatui/crossterm** — Full terminal UI with panels, interactive selection,
real-time feedback. Beautiful but heavy dependency, complex to maintain, breaks in
some terminal environments.

**C. Hybrid** — Conversational flow for the narrative parts (fate conversation, scenario
exploration), TUI widgets for the mechanical parts (drive selection, retention tuning).
Best of both but complex to build.

**D. Web-based** — Generate a local HTML page with the encounter, serve it on localhost.
Rich UI capabilities but wildly out of scope for a CLI backup tool.

Recommendation: Start with pure CLI (A). The fate conversation is inherently linear
and conversational — it doesn't need a TUI. The "delve deeper" config editing *could*
benefit from a TUI later, but `$EDITOR` with a well-commented TOML file is honestly fine.

### 19. Config Generation as Pure Function

The encounter collects structured data (drives, subvolumes, roles, data classifications).
Config generation from this data should be a pure function in a new `onboard.rs` module:

```rust
pub fn generate_config(encounter: &EncounterResult) -> Config
pub fn generate_toml(config: &Config) -> String
```

This follows ADR-108 (pure function modules). The interactive conversation is I/O;
the config derivation is pure logic. Testable without a terminal.

### 20. Skip-Fast for Experts

Power users who know exactly what they want shouldn't be forced through the
fate conversation:

"I can examine your world and propose protections through a conversation,
or you can point me to a config file you've already prepared."

Options:
- `urd --config path/to/existing.toml init` — skip onboarding entirely
- Answer "I know what I'm doing" at the start → minimal discovery, no scenarios,
  just drive/subvolume detection and direct config editing

### 21. Re-encounter: "Things have changed"

When Urd detects significant changes (new BTRFS filesystem, new drives, subvolumes
removed), she could offer a partial re-encounter:

"Your world has changed since we last spoke. I see a new drive at `/run/media/...`.
Shall we discuss what role it plays?"

This is a lighter version of onboarding — discovery of delta, not full conversation.
Could be triggered by `urd calibrate` or automatically by the sentinel.

---

## Handoff to Architecture

The 5 most promising ideas for deeper analysis:

1. **The Fate Conversation (#4)** — This is the differentiator. No backup tool walks users
   through disaster scenarios to derive protection levels. It turns an abstract config
   decision into a concrete, personal risk assessment. Needs careful voice design.

2. **Drive/Subvolume Discovery (#2, #3)** — The foundation everything else depends on.
   Wrapping `btrfs subvolume list` and `btrfs filesystem show` in the discovery phase
   eliminates the hardest part of first-time setup. Mostly existing module extensions.

3. **Config Generation as Pure Function (#19)** — The architectural spine. Without this,
   onboarding is a bespoke script. With it, onboarding is a front-end to a testable
   config generator that can be consumed by future UIs (Spindle, web).

4. **Two Exits with Scenario Simulator (#6, #7)** — "Set and forget" vs "delve deeper"
   with live consequence feedback is the UX pattern that scales from beginners to experts.
   Leverages existing pure functions (awareness, preflight) during config editing.

5. **Auto-trigger from missing config (#1)** — The best onboarding is the one the user
   doesn't have to find. Running `urd status` with no config and being guided into setup
   is fundamentally better than a separate `urd setup` command in the README.
