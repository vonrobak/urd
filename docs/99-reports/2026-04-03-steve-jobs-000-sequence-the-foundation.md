---
upi: "000"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Sequencing UPI 004-009 Against the Roadmap

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Integration of 6 new UPIs (004-009) into existing roadmap (assess() fix, 6-O progressive disclosure, 6-H the encounter)
**Mode:** Vision Filter

## The Verdict

These six fixes are not a detour from the roadmap — they are the roadmap's missing foundation, and the encounter will be better for having them done first.

## What's Insanely Great

The existing roadmap has a clear arc: fix assess() scoping → build progressive disclosure (6-O) → build the encounter (6-H). That arc is about making Urd trustworthy to new users. These six UPIs are about making Urd trustworthy to existing users. Both are necessary, and the testing session just proved which comes first.

The roadmap already had "assess() scoping fix" as the next item. The testing validated that decision and expanded its scope — the fix isn't just about htpc-root, it's about every subvolume with drive scoping. UPI 005 subsumes the existing roadmap item and adds the vocabulary fix. This isn't scope creep. This is the same work, now properly understood.

The separation between patch-tier fixes (004, 005, 007, 008) and standard-tier features (006, 009) is exactly right. The patches don't need design cycles — they have clear single-function-level changes. The features need more thought. Respect that distinction in sequencing.

## What's Not Good Enough

### The roadmap sequence assumed the foundation was solid

The current roadmap says:

```
assess() scoping fix (patch, ~0.5 session)
  │
Backup-now imperative ✓ (v0.8.0)
  │
6-O: Progressive disclosure (2 sessions)
  │
6-H: The Encounter (4-6 sessions)
```

This sequence jumps from one correctness fix to the encounter — as if the status display, the doctor, the drive identity system, and the backup communication are all ready for a new user's first impression. They're not. The test session proved it.

If someone goes through the encounter (6-H), sets up their drives, and then plugs in a cloned drive — Urd silently accepts it. If they run `urd status` and see "All sealed. 3 degraded" — they won't know what to do. If they run a backup and see "FAILED" for a safety gate that worked correctly — they'll think the tool is broken.

The encounter is Urd's first impression. You don't launch the store with broken display cases.

### The proposed build sequence doesn't account for the encounter's needs

The grill-me output proposed: 004 → 005 → 007 → 008 → 009 → 006. That's ordered by severity. But severity and strategic value aren't the same thing.

UPI 006 (reconnection notifications) is listed last because it's "standard tier." But for the encounter, it's essential. When a new user sets up Urd through the guided wizard and then plugs in their backup drive the next day, what happens? If the answer is "silence" — the encounter failed. The tool just taught them to set up backups, and then gave them no feedback when the backup infrastructure appeared.

## The Vision

Here's how I see the sequence. The organizing principle is: **what does the user experience at each stage, and is it honest?**

### Phase A: Make the promises true (v0.8.1)

```
UPI 004 — TokenMissing gate       (~0.5 session)
UPI 005 — assess() scoping + [LOCAL]  (~0.5 session)
```

These two are the foundation. After they ship:
- `urd status` stops lying about health (no false degradation)
- Urd refuses to send to an unrecognized drive (no silent data corruption)
- Local-only subvolumes are correctly labeled

These can be built in parallel. Ship as v0.8.1. This is the "stop lying" release.

### Phase B: Make the communication honest (v0.8.2)

```
UPI 007 — Safety gate communication  (~0.5 session)
UPI 008 — Doctor pin-age + UUID fix  (~0.25 session)
```

After they ship:
- Safety gates that work correctly say "DEFERRED," not "FAILED"
- Doctor stops accusing the user of failures caused by drive absence
- Doctor stops suggesting impossible UUID additions

These are also independent of each other. Ship as v0.8.2. This is the "stop confusing" release.

### Phase C: Give drives a face (v0.9.0)

```
UPI 009 — `urd drives` subcommand   (~0.5-1 session)
UPI 006 — Reconnection notifications (~0.5-1 session)
```

After they ship:
- Users can see and manage drive identity (`urd drives`)
- UPI 004's error message upgrades from "run `urd doctor`" to "run `urd drives adopt`"
- Drive reconnection closes the anxiety loop (notification)

These form a cohesive "drives are first-class citizens" release. Ship as v0.9.0 — it's a MINOR bump because `urd drives` is a new command (feature, not fix).

### Phase D: The encounter can begin

```
6-O — Progressive disclosure        (~2 sessions)
6-H — The Encounter                 (~4-6 sessions)
```

Now the encounter has a solid platform:
- Status tells the truth (Phase A)
- Communication is honest (Phase B)
- Drives have a user-facing identity layer (Phase C)
- The guided setup wizard can reference `urd drives` for drive management
- Reconnection notifications prove the invisible worker works
- A new user's first `urd status` after the encounter reflects reality

The encounter is 4-6 sessions. If the foundation is broken, that's 4-6 sessions building on lies. Fix the foundation first.

### Updated roadmap arc

```
Phase A: v0.8.1 — Make promises true
  UPI 004 (token gate) ─┐
  UPI 005 (assess + local) ─┘─→ v0.8.1 tag
                              │
Phase B: v0.8.2 — Make communication honest
  UPI 007 (deferred not failed) ─┐
  UPI 008 (doctor fixes) ────────┘─→ v0.8.2 tag
                                    │
Phase C: v0.9.0 — Give drives a face
  UPI 009 (urd drives) ─────┐
  UPI 006 (notifications) ──┘─→ v0.9.0 tag
                                │
                         Update 004 error message
                         (urd doctor → urd drives adopt)
                                │
Phase D: Progressive disclosure + The Encounter
  6-O ─→ 6-H ─→ v1.0 horizon
```

Estimated total: ~5-6 sessions for Phases A-C, then 6-8 sessions for Phase D. The existing roadmap estimated ~9-11 sessions from the assess() fix through 6-H. Adding Phases A-C adds ~4-5 sessions, but they're not overhead — they're the quality gate that makes the encounter trustworthy.

## The Details

- **Phase A is the only blocking prerequisite for everything else.** Without truthful status and drive identity, all downstream features build on lies. Phases B and C improve the experience but don't block the encounter architecturally.

- **The v0.8.1/v0.8.2/v0.9.0 versioning follows SemVer (ADR-112).** Patches for fixes, minor for new features. This matters: users tracking versions know that 0.8.x means "same features, fewer bugs" and 0.9.0 means "new capability."

- **Consider doing the `btrfstune -u` on WD-18TB1 during Phase A.** The software fix (UPI 004) is the right long-term answer, but the manual mitigation (unique UUID + label rename) eliminates the physical risk immediately. Do both: manual fix today, software fix in v0.8.1. Belt and suspenders.

- **P6a (enum rename) and P6b (config Serialize) are still deferred chores.** They don't need to sequence against these UPIs. Do them as quick PRs when convenient — the roadmap already says this. Don't let them drift into the encounter's critical path.

- **The test session itself should become a repeatable practice.** After v0.9.0, before starting 6-H, run another physical drive test session. The encounter needs to work with real drives, not just mocks. Schedule it.

## The Ask

1. **Ship Phase A (004 + 005) as v0.8.1 immediately.** These are the "stop lying" fixes. Every day they're not shipped is a day where `urd status` gives false degradation and cloned drives can corrupt pin state.

2. **Ship Phase B (007 + 008) as v0.8.2 within the same week.** Four changes across two tiny PRs. Should take one session total.

3. **Ship Phase C (009 + 006) as v0.9.0 in the following session.** This is the "drives have a face" release. It upgrades 004's interim message and adds reconnection notifications.

4. **Update roadmap.md to reflect the new arc.** The current roadmap shows assess() fix → 6-O → 6-H. The new roadmap is Phases A-C → 6-O → 6-H. Make the plan legible.

5. **Run `btrfstune -u` on WD-18TB1 at the next opportunity.** Don't wait for the software fix. The physical risk exists now.

6. **Schedule a second physical drive test session after v0.9.0 ships.** Verify the full arc before starting the encounter. The encounter is too important to build on untested ground.
