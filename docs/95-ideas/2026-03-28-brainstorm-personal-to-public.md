# Idea: Transitioning Urd from Personal Tool to Public Release

> **TL;DR:** Brainstorm on strategies for moving Urd from a homelab-tailored tool to a
> general-purpose BTRFS backup tool, without losing the homelab as a first-class deployment.

**Date:** 2026-03-28
**Status:** raw

## The tension

Urd is currently shaped by one deployment: 9 subvolumes, 3 drives, specific paths, specific
retention needs. Every design decision has been validated against that deployment. This is a
strength — the tool was forged in real use, not hypothetical requirements. But it means some
assumptions are baked in that won't generalize:

- Config example (`urd.toml.example`) is literally the homelab config with `<user>` placeholders
- Drive names like "WD-18TB" and subvolume names like "htpc-home" are hardcoded in examples
- sudoers setup assumes a specific path structure
- The protection level taxonomy (guarded/protected/resilient) was designed for one user's
  mental model of their data
- systemd units assume a specific Linux desktop setup (user units, not system)
- `urd init` imports state from the old bash script — only useful for this homelab

The risk is two-fold: (1) generalizing too early, breaking the homelab deployment to serve
hypothetical users, or (2) never generalizing, remaining a one-person tool with public ambitions.

---

## Ideas

### 1. The "example gallery" pattern

Replace the single `urd.toml.example` with a gallery of commented examples for common
deployment patterns:

- `examples/laptop-single-drive.toml` — `/home` only, one external drive, daily backups
- `examples/workstation-multi-drive.toml` — multiple subvolumes, 2+ drives, hourly snapshots
- `examples/server-data-volumes.toml` — production data, conservative retention, no Sentinel
- `examples/nas-large-media.toml` — large subvolumes (photos/video), space-constrained drives
- `examples/homelab-full.toml` — the actual homelab config (sanitized), showing all features

Each example is self-documenting and demonstrates a different slice of the feature surface.
New users find the example closest to their setup and modify it. The current `urd.toml.example`
becomes one entry in the gallery rather than the only reference.

**Data safety lens:** Makes data safer — users start from a tested, realistic config instead
of constructing one from scratch with the risk of misconfiguring retention.

### 2. `urd setup` as the generalization gateway

Build the conversational setup wizard (already Priority 6c in roadmap) as the primary
onboarding path. The wizard:

- Auto-discovers BTRFS subvolumes on the system
- Asks which ones to protect and at what level
- Detects connected external drives and offers to configure them
- Generates a complete `urd.toml` tailored to *this* system
- Installs systemd units (user or system, detected from context)
- Runs `urd plan` to preview what the config means in practice

This is the key architectural decision: **`urd setup` is how Urd stops being personal.**
The current homelab config was written by hand because the author understood every parameter.
A new user running `urd setup` should arrive at a config equally correct for *their* system
without understanding the parameter space.

**Data safety lens:** Significantly safer — guided setup prevents the most dangerous
misconfiguration (wrong retention, wrong paths, missing pin protection).

### 3. Compile-time feature flags for homelab vs. general

Use Cargo feature flags to separate homelab-specific code paths:

```toml
[features]
default = []
homelab-compat = []  # Legacy bash script import, old metric names, etc.
```

`urd init --import-bash-state` only compiles with `homelab-compat`. The default build is
clean of migration-specific code. The homelab builds with the flag; public releases don't.

**Data safety lens:** Neutral — this is a code organization choice, not a safety choice.

### 4. The "personality layer" separation

Urd's mythic voice is a distinctive feature, but the *specific* voice might not resonate with
everyone. Separate the voice into a personality system:

- `voice.rs` becomes a trait-based renderer with a default personality
- The default personality is the mythic norn voice
- Alternative personalities could exist: minimalist/technical, emoji-friendly, plain English
- The personality is a config option, not a compile-time choice

This is an uncomfortable idea — the mythic voice is part of Urd's identity. But "personality
as a feature" means the voice enhances the tool for those who appreciate it without alienating
those who find it distracting.

**Data safety lens:** Neutral for safety, but relevant to the design north star "reduce
attention on backups" — a voice that annoys a user makes them pay *more* attention to
backups, not less.

### 5. Protection levels as the universal language

The current taxonomy (guarded/protected/resilient) is acknowledged as provisional. For public
release, the level names need to communicate intent without explanation:

- What if levels were named by what they protect *against*, not what they *provide*?
  - "local" (protects against accidental deletion)
  - "offsite" (protects against hardware failure)
  - "distributed" (protects against site-level events)
- Or named by the user's relationship to the data?
  - "replaceable" (convenience backups, minimal retention)
  - "important" (would cost time/effort to recreate)
  - "irreplaceable" (photos, personal documents, cannot be recreated)
- Or simply numbered tiers with descriptions?
  - Level 1: local snapshots only
  - Level 2: local + 1 external drive
  - Level 3: local + 2+ external drives

The names must be self-evident to someone who has never read an ADR. The current names
require explanation — "what does 'guarded' mean vs 'protected'?" This is the single biggest
UX barrier to public release.

**Data safety lens:** Directly impacts safety — if users choose the wrong level because the
names are confusing, their data is less safe than they believe.

### 6. Test the config against real-world patterns via `urd doctor`

A diagnostic command that evaluates the running system against best practices:

```
$ urd doctor
✓ Config syntax valid
✓ All snapshot roots exist and are BTRFS subvolumes
✓ 2 external drives configured (meets resilient requirements)
⚠ subvol3-opptak (3.4TB) exceeds 2TB-backup capacity — drive excluded automatically
⚠ No UUID configured for WD-18TB1 — run `urd doctor --fix-uuids` to add
✗ sudoers entry missing for /run/media/<user>/WD-18TB1/.snapshots/*
  → Suggested fix: echo '<user> ALL=...' | sudo tee -a /etc/sudoers.d/urd
```

`urd doctor` is the "is my installation healthy?" command. It replaces the need for
documentation about common problems — the tool diagnoses them itself.

**Data safety lens:** Makes data significantly safer — catches misconfigurations that would
silently reduce protection (missing sudoers = failed sends = AT RISK status without obvious
cause).

### 7. Sudoers generation and validation

The sudoers setup is the single most system-specific aspect of Urd. Every user has different
paths, different usernames, different drive mount points. Currently this requires manual
editing of `/etc/sudoers.d/urd`, which is error-prone and intimidating.

`urd setup` or `urd doctor` could generate and validate sudoers entries:

```
$ urd doctor --sudoers
# Urd requires these sudoers entries for configured operations:
# (generated from ~/.config/urd/urd.toml)

<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs subvolume snapshot -r *
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs send *
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs subvolume delete /run/media/<user>/WD-18TB/.snapshots/*
<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs receive /run/media/<user>/WD-18TB/.snapshots/*
# ... for each drive

$ urd doctor --sudoers --install
[sudo] password for <user>:
Written to /etc/sudoers.d/urd (validated with visudo -c)
```

**Data safety lens:** Makes data safer — correct sudoers means sends actually work.
Incorrect sudoers is the #1 reason sends fail silently on a new installation.

### 8. Package-manager-ready build system

For public release, users shouldn't need a Rust toolchain. Distribution paths:

- **GitHub Releases with prebuilt binaries** (Linux x86_64, aarch64)
- **AUR package** (Arch Linux — the BTRFS power user demographic)
- **Fedora COPR** (RPM-based distros)
- **Flatpak** (universal, but sandboxing conflicts with sudo/btrfs — may not be viable)
- **Nix flake** (NixOS users are disproportionately BTRFS users)
- **cargo install urd** (Rust users)

Each distribution path has different expectations for where files go (`/usr/bin` vs
`~/.cargo/bin`), where config lives (`/etc/urd/` vs `~/.config/urd/`), and how systemd
units are installed (system vs user).

**Data safety lens:** Neutral directly, but safer indirectly — easier installation means
more people actually set it up properly.

### 9. The "homelab-as-integration-test" pattern

Instead of separating the homelab from public development, *lean into it* as the
authoritative integration test. The homelab config becomes a test fixture:

- CI runs the test suite against a sanitized version of the homelab config
- The homelab's real backup runs generate metrics that feed back into development
- New features are validated on the homelab before release
- The homelab's operational experience (catastrophic failures, NVMe exhaustion, chain
  breaks) directly shapes the test suite

This reframes the tension: the homelab isn't a legacy to escape, it's the proving ground
that makes public releases trustworthy. "Battle-tested on a real system for N months" is a
feature, not a limitation.

**Data safety lens:** Strongly safety-positive — real operational experience catches bugs
that synthetic tests miss.

### 10. Config schema as the generalization boundary

ADR-111 already defines the target config architecture. The migration from legacy to
ADR-111 schema is the natural generalization point:

- Legacy schema = homelab-tailored (`[defaults]`, `[local_snapshots]`, inheritance)
- ADR-111 schema = general-purpose (self-describing subvolume blocks, no inheritance,
  explicit everything)

The `urd migrate` command that transforms legacy → ADR-111 is also the command that
transforms "personal config" → "portable config." After migration, a config file is
readable in isolation — you can share it, discuss it, or use it as an example without
needing to know the system it came from.

**Data safety lens:** Neutral for safety, but critical for the "reduce attention" north
star — self-describing configs are less likely to be misunderstood.

### 11. Progressive disclosure in the config format (uncomfortable)

What if the config format itself had tiers of complexity?

**Minimal config (3 lines):**
```toml
[[subvolumes]]
source = "/home"
```
Urd discovers the snapshot root, drive, and retention automatically. Protection level
defaults to "protected." This is the "I just want backups" user.

**Standard config (current ~30 lines per subvolume):**
Named protection levels, explicit drive routing, custom retention.

**Expert config (ADR-111 full):**
Every parameter explicit, no derivation, full control.

The minimal config requires `urd setup` or auto-discovery to fill in what's missing. The
standard config is what `urd setup` generates. The expert config is what power users write
by hand.

**Data safety lens:** The minimal config could be *more* safe than the standard one — fewer
parameters means fewer ways to misconfigure. But it requires good defaults, and Urd's
defaults would need to be validated across diverse systems, not just one homelab.

### 12. Run Urd in "audit mode" before committing

A new user's first interaction with Urd shouldn't be `urd backup`. It should be a read-only
evaluation that shows what Urd *would* do:

```
$ urd audit
Found 4 BTRFS subvolumes:
  /home          — 45 GB, 3 existing snapshots
  /var/lib/data  — 200 GB, no snapshots
  /srv/media     — 2.1 TB, no snapshots
  /              — 15 GB, 1 existing snapshot

Found 1 external drive:
  /run/media/user/Backup-1TB — BTRFS, 800 GB free

Recommended protection:
  /home          → resilient (irreplaceable data, but only 1 drive available → protected)
  /var/lib/data  → protected (important, 1 external copy)
  /srv/media     → protected (large, but fits on 1TB drive)
  /              → guarded (system root, replaceable)

Estimated first run: ~2.3 TB to send (full sends, no incremental chain yet)
Estimated time: ~6 hours at 100 MB/s USB 3.0

Would you like to generate this config? [y/n]
```

This is `urd setup` plus `urd plan` in one zero-commitment experience. The user sees exactly
what Urd thinks about their system before any config file exists.

**Data safety lens:** Safety-positive — users understand what will happen before it happens.
The "estimated first run" warning prevents the surprise of a 6-hour backup blocking their
system.

### 13. Strip PII from snapshot names (uncomfortable)

Current snapshot names contain `short_name` which is user-chosen: `20260328-1430-opptak`.
For a public tool, should snapshot names contain user-identifiable information?

Most backup tools use opaque IDs or timestamps only. Urd's human-readable names are a
genuine feature (you can `ls` a snapshot directory and know what's what), but they mean
snapshot directories on external drives reveal the naming scheme to anyone who mounts the
drive.

Probably not worth changing — the names are already an on-disk contract (ADR-105), and
human-readability is a core UX principle. But worth noting as a tension.

**Data safety lens:** Neutral — names don't affect safety.

### 14. Polkit instead of sudoers

sudoers is powerful but fragile and distribution-specific. Polkit is the modern Linux
approach to privilege escalation:

- Write a polkit policy file that grants Urd's btrfs operations
- `urd setup` installs the policy
- No manual sudoers editing
- Works consistently across Fedora, Ubuntu, Arch

This is a significant architectural change to `btrfs.rs` — instead of `sudo btrfs`, it
would use `pkexec btrfs` or a custom privilege helper. The `BtrfsOps` trait abstracts this
cleanly, so the change is isolated.

**Data safety lens:** Slightly safer — polkit policies are more structured and harder to
misconfigure than sudoers entries. But pkexec has different interactive behavior (may prompt
for password on each call) that needs careful handling for batch operations.

### 15. The "graduated public release" (uncomfortable)

Instead of one big "v1.0 public release," release publicly in stages that match readiness:

- **v0.4: "Works for me"** — public repo (already done), no install docs, no packaging.
  Adventurous users can build from source. The homelab is the only supported deployment.
- **v0.5: "Works for you too"** — `urd setup` wizard, example gallery, `urd doctor`,
  sudoers generation. The first version where a non-author can set it up.
- **v0.6: "Trustworthy"** — config schema migration (ADR-111), protection level taxonomy
  rework, `urd audit` mode. The version where the tool explains itself to strangers.
- **v0.7: "Installable"** — AUR package, GitHub Releases with binaries, man pages.
- **v1.0: "Recommended"** — Stable config schema, stable CLI, stable on-disk formats.
  The version you'd recommend to a friend protecting their photos.

Each stage has a clear quality bar and a clear audience expansion. The homelab deployment
tracks the latest version; public users adopt at whatever stage matches their risk tolerance.

**Data safety lens:** Strongly safety-positive — staged release means each audience gets
a version appropriate to their willingness to debug issues.

---

## Handoff to Architecture

1. **`urd setup` wizard (idea 2+12)** — The single highest-leverage generalization action.
   Every other idea is easier once `urd setup` exists, because it becomes the canonical
   onboarding path that eliminates system-specific assumptions.

2. **Protection level taxonomy rework (idea 5)** — The names are the UX. If a new user
   can't choose the right level without reading an ADR, the promise model — Urd's core
   differentiator — is inaccessible.

3. **`urd doctor` (idea 6+7)** — Catches the long tail of installation problems that
   documentation can never fully address. Sudoers validation alone would prevent the most
   common failure mode for new users.

4. **Example gallery (idea 1)** — Low effort, high signal. Shows Urd working in contexts
   beyond the homelab. Validates that the config format actually generalizes.

5. **Graduated public release (idea 15)** — Frames the transition as a series of
   audience expansions rather than one big bet. Each release stage has clear criteria
   and clear scope.
