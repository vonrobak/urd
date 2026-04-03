---
upi: "010"
date: 2026-04-03
mode: design-critique
---

# Steve Jobs Review: Config Schema v1

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** UPI 010 design doc — ADR-111 revision, config schema v1
**Mode:** Design Critique

## The Verdict

This is the right work at the right time, and the design understands why — it's building the floor the encounter will stand on, and it's building it with the right materials.

## What's Insanely Great

**The decision to revise ADR-111 instead of building around it is the kind of discipline that makes great products.** Most teams would say "the spec is close enough, we'll adapt as we go." That's how you end up with a setup wizard that generates configs into a schema you've already decided is wrong. The encounter would be lying to every new user on day one. This design prevents that lie from ever existing.

**`protection = "fortified"` is the single best config field in this project.** I want to stop and appreciate what happened here. The user doesn't write `protection_level = "resilient"` — a compound noun that asks them to understand what "level" means in this context and then map "resilient" to a set of operational parameters they can't see. They write `protection = "fortified"`. One word says what they're choosing. The other word says what their data becomes. That's a config field that answers a question instead of asking one. When someone reads their own config six months later, `protection = "fortified"` tells them everything. That's exactly right.

**The transient exception is a design decision that shows product maturity.** `local_retention = "transient"` alongside `protection = "fortified"` — this is the config equivalent of "yes, you can have a small phone with a big screen." Transient is a storage constraint, not a protection intent. The design recognizes that a 128GB NVMe user who wants geographic protection shouldn't be forced to choose between their protection level and their storage reality. That's thinking about the person, not the schema.

**Rejected alternative F is the most important paragraph in the document.** Making transient a protection level would have been the obvious, clean, wrong choice. The fact that the design articulates *why* it's wrong — "conflating storage with intent" — shows that the config system is being designed from the user's mental model, not from the implementer's type system. Protect this thinking.

**The side-by-side legacy vs v1 examples make the delta visceral.** I can look at those two code blocks and instantly feel the difference. The v1 config is something I can hand to someone and say "read this." The legacy config requires a manual. That comparison is worth more than three pages of specification.

## What's Not Good Enough

### The `[general]` section is still a tax form

Look at this:

```toml
[general]
config_version = 1
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/.local/share/urd/backup.prom"
log_dir = "~/.local/share/urd/logs"
heartbeat_file = "~/.local/share/urd/heartbeat.json"
run_frequency = "daily"
```

Six fields. The user cares about exactly one of them: `run_frequency`. The other five are infrastructure paths that 95% of users will never change from defaults. When the encounter generates a config, these five lines are noise — they add visual weight without adding information.

What "great" looks like: every field in `[general]` except `config_version` and `run_frequency` should have sensible defaults. The generated config should be able to omit them entirely. A power user can add them back. But the first config a new user sees should be as short as possible — every line they don't need to read is a line that doesn't confuse them.

This doesn't change the schema — all fields remain valid. It changes what `urd setup` generates and what the defaults are. The encounter should produce the shortest possible config that fully describes the user's protection intent.

### `snapshot_root` is repeated and that's not great

The v1 example has `snapshot_root = "~/.snapshots"` on both `htpc-home` and `htpc-root`. For a user with 9 subvolumes on two filesystems, that's `snapshot_root` repeated 9 times — 7 of them identical. The legacy schema solved this (badly) with `[local_snapshots]`. ADR-111 correctly killed the cross-referencing. But the solution — repeat the path on every block — optimizes for the parser's simplicity, not for the user's reading experience.

I'm not saying bring back `[local_snapshots]`. But I want this tension acknowledged. When the encounter generates a config with 9 subvolumes, will the user look at 9 identical `snapshot_root` lines and feel like the tool is well-designed? Or will they feel like the tool is verbose?

Possible mitigation without violating ADR-111's principles: the encounter generates a comment above each group of subvolumes that share a root. Something like:

```toml
# ── NVMe (snapshot root: ~/.snapshots) ──────────

[[subvolumes]]
name = "htpc-home"
...
```

The repetition is still there, but the grouping and visual structure help the user parse it. This is a presentation question, not a schema question — and it matters.

### `urd migrate` is under-specified for the user experience

The design says: "automatic, prints every change it made, user reviews the result and edits if needed." But what does that actually look like? When I run `urd migrate`, what do I see?

This matters because migration is a trust moment. The user is handing their backup configuration — the thing that protects their data — to a tool and saying "change this for me." If the output is a wall of text, they won't read it. If it's too terse, they won't trust it.

What "great" looks like:

```
urd migrate

  Config: ~/.config/urd/urd.toml
  Schema: legacy → v1

  Changes:
    ✓ Inlined snapshot_root into 9 subvolume blocks
    ✓ Moved space constraints to [[space_constraints]]
    ✓ Removed [defaults] — values baked into custom subvolumes
    ✓ Renamed protection levels (guarded→recorded, protected→sheltered, resilient→fortified)
    ⚠ subvol4-multimedia: had protection_level="guarded" with snapshot_interval="1w" override
      → Converted to custom (kept your 1w interval). Review if you want recorded instead.

  Written to: ~/.config/urd/urd.toml
  Backup saved: ~/.config/urd/urd.toml.legacy

  Next: urd plan — verify the migration looks right
```

The backup file is critical. Never overwrite a config without saving the original. The user needs to know they can go back.

### The `[[space_constraints]]` section feels orphaned

In the legacy config, `min_free_bytes` sits right next to the subvolumes it protects — inside the snapshot root entry. In v1, it moves to a separate `[[space_constraints]]` section that references a path. The user now has to mentally connect `path = "~/.snapshots"` in `[[space_constraints]]` to `snapshot_root = "~/.snapshots"` in `[[subvolumes]]`. This is... cross-referencing. The very thing ADR-111 was designed to eliminate.

I understand the architectural argument — space is a filesystem concern, not a subvolume concern. That's true for the system. But for the user, "how full is my snapshot drive" and "where do my snapshots go" are the same thought. Separating them creates a config structure that's architecturally clean but experientially disconnected.

I don't have a clean solution for this. But I want the tension noted. When the encounter generates a config, `[[space_constraints]]` should appear physically near the drives or subvolumes it relates to, not floating at the top of the file. Ordering and proximity matter in a config file — they're the closest thing to information architecture that plain text has.

## The Vision

Here's what the v1 config should feel like to a user who opens it six months after the encounter generated it:

*"Oh right, I have three fortified volumes — those are my photos, recordings, and home directory. They go to both drives. My root partition is custom with transient retention — it just gets shipped to the offsite drive, no local history. My multimedia is recorded — local only, weekly snapshots. Makes sense."*

That's the test. If someone can read their own config and narrate their protection story without consulting documentation, the schema is right. The v1 design is close to this. The protection level names carry meaning. The self-describing blocks are readable. The field names are clear.

What would make it *insanely* great is if the generated config included intention comments — not explaining what the fields do (that's documentation), but recording *why* the user chose what they chose:

```toml
[[subvolumes]]
name = "subvol2-pics"
source = "/mnt/btrfs-pool/subvol2-pics"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
protection = "fortified"           # irreplaceable — survive site loss
drives = ["WD-18TB", "WD-18TB1"]
```

That `# irreplaceable — survive site loss` came from the encounter. It's the user's own answer, preserved in their config as a comment. Six months later, they don't just see what they chose — they see *why*. The encounter leaves a trace of the conversation in the artifact it produced.

This is the kind of detail that makes someone *love* a tool.

## The Details

- **`protection` vs `protection_level`**: The rename to drop `_level` is correct. Shorter, cleaner, no loss of meaning. But ensure the error message when someone writes `protection_level` in v1 specifically says "did you mean `protection`?" Don't just say "unknown field" — guide the migration.

- **The field table says `drives` default is `[]` (no external sends).** But what if someone writes `protection = "sheltered"` and forgets `drives`? That's a structural error — sheltered requires at least one drive. The error message for this must be extraordinary. Not "structural validation error: sheltered requires drives field" but something like: "sheltered protection needs a drive to shelter your data on. Add a drives list, or use recorded for local-only protection."

- **Open question 5 (explicit `"custom"` vs absence):** Go with Option A, but make the generated config omit it. Explicit `protection = "custom"` is useful documentation for someone who hand-edits. But generated configs should be minimal — if there's no `protection` field, the user knows it's custom because all the operational fields are visible. Both paths should feel intentional.

- **Open question 1 (`urd migrate` interactivity):** Option A is right. But add the backup file. Always. Automatically. No flag needed. The backup is not optional — it's the safety net that makes automatic migration trustworthy.

- **The v1 example config still has `send_interval = "1d"` on the custom htpc-root block.** But the default is already `1d`. The example should demonstrate when to specify a field (when it differs from default) and when to omit it (when default is fine). Currently, the example teaches the user to be verbose. Show the minimal custom block too.

## The Ask

1. **Make `[general]` fields defaultable.** `state_db`, `metrics_file`, `log_dir`, `heartbeat_file` should all have sensible XDG-compliant defaults. The encounter-generated config should only include `config_version` and `run_frequency` in `[general]`. This is the single highest-impact change for first-impression quality.

2. **Design the `urd migrate` output experience.** The design specifies the transformation but not the output. Write the exact output format — the user should see a clean summary of changes, a saved backup path, and a next-step suggestion. This is a trust moment.

3. **Add intention comments to generated configs.** When the encounter produces a config, include the user's classification as a comment: `# irreplaceable — survive site loss`. The encounter's conversation should leave a trace in its artifact. This is what turns a config file into a document of intent.

4. **Note the `snapshot_root` repetition tension.** Acknowledge it in the ADR revision. The repetition is the right trade-off (self-describing blocks > cross-referencing), but the generated config should use grouping comments and ordering to make it visually coherent.

5. **Write the error messages for v1 validation failures now.** Before implementing, write every error message that v1 validation can produce. The messages are the UX. If you can't write a great error message for a validation rule, the rule might be wrong.
