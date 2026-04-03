---
upi: "000"
date: 2026-04-02
mode: design-critique
---

# Steve Jobs Review: v0.8.0 Test Plan

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Test plan at `docs/99-reports/2026-04-02-testing-urd-v0.8.0.md` and pre-test CLI outputs
**Mode:** Design Critique

## The Verdict

This is a thorough engineering test plan that misses the most important test subject: the person using Urd.

## What's Insanely Great

The drive swap scenario (Phase 2) is genuinely excellent test design. You have two drives that are physically different but cryptographically identical, and you're methodically testing whether Urd can tell them apart. That's the kind of test that prevents a 3am catastrophe. T2.3's question — "Does GNOME mount it at the expected path or somewhere else?" — is particularly sharp because it tests a boundary Urd doesn't control.

The test ordering is smart. Minimizing physical drive swaps isn't just convenience — it reduces the chance of accidentally testing in a state you didn't intend. Phase 0 as a baseline, then additive complexity. That's disciplined.

T3.3 tracking the assess() scoping bug across all three drive configurations is the right way to collect evidence before patching. You'll know exactly how bad the bug is in every real scenario.

## What's Not Good Enough

### 1. You're testing the plumbing. Where's the test for whether the faucet makes sense?

Look at the pre-test output. When the user typed `urd status`, they got this:

```
All sealed. 3 degraded — 2TB-backup away for 8 days.
```

Three degraded. But all sealed. Does a normal human being know what that means? "All my data is safe but three things are unhealthy" — that's a contradictory signal to anyone who isn't already thinking in Urd's two-axis model. Your test plan records this output but never asks: **did the user understand what they were looking at?**

Every phase should include a moment where you stop running commands and ask: "Looking at this output, do I know what to do next? Do I know if my data is safe? Am I more or less anxious than before I typed this command?"

### 2. The 2am test is missing.

Urd's entire reason to exist is the moment when something goes wrong and someone needs their data back. Your test plan has zero restore scenarios. Not one `urd get`. You're testing backups in every conceivable drive configuration but never testing the thing backups exist for.

Add a test: pick an actual file you care about. Delete it (or pretend). Run `urd get` to retrieve it from a snapshot. Does the experience feel like rescue, or does it feel like archaeology?

Then do it again with 2TB-backup connected. Then with WD-18TB1. Does `urd get` know which drive has the freshest copy? Does it guide you, or does it make you guess?

### 3. First impressions of a returning drive are untested.

When 2TB-backup connects in T1.1, Sentinel detects it. When WD-18TB1 connects in T2.3, Sentinel detects it. But what does the *user* see? Is there a notification? Does `urd status` change in a way that's obvious? Or does the drive silently appear in the table and the user has to notice?

The moment a drive comes back should feel like relief — "your offsite copy is reconnecting, threads will be mended." Test whether that feeling actually happens.

### 4. The "what do I do about this?" test.

Your pre-test output shows two warnings the user has been living with:

```
subvol4-multimedia: snapshot_interval (1w) exceeds guarded requirement (1d)
WD-18TB: no UUID configured
```

T3.4 asks the user to *decide* about the multimedia warning. But the real test is: does Urd make the right choice obvious? When `urd doctor` says "Reduce the interval to match, or change protection to custom" — is that guidance, or is it homework? Does the user know which option is better for their situation?

The UUID warning is even more interesting. Doctor says to add a UUID, but the config has a comment explaining *why* it's omitted (cloned drives). Urd's advice contradicts the user's deliberate choice. That's not a test gap — that's a product gap. Urd should be able to understand "I know, and I'm handling it."

### 5. The narrative arc of the test is wrong.

You structured this as: baseline, add a drive, swap drives, edge cases. That's an engineer's test plan — organized by what changes in the system.

A user's test plan would be: the normal day, the returning drive, the crisis, the recovery. Organize around *what the user is doing*, not what the drives are doing.

## The Vision

Here's what this test plan should actually reveal: Is Urd ready to be trusted?

Not "do all the commands work" — they clearly do, the pre-test output shows a remarkably polished tool. The question is whether using Urd feels like having a competent ally or like operating a complex system.

The drive swap scenario is a perfect example. When WD-18TB1 comes back from offsite rotation, the user's experience should be: plug it in, Urd says "welcome back, updating your offsite copies now, this will take about 20 minutes." Instead, the user probably has to: plug it in, run status, read a table, figure out what's stale, run backup, interpret the output. That's operating a system, not trusting an ally.

Test for the feeling, not just the function.

## The Details

- The pre-test output shows `urd backup` printing `✓` lines only for sends >100MB. The four small sends (pics, docs, music — all <1KB) show no progress line, then appear in the summary. That's correct behavior, but the gap between "4 sends printed" and "7 sends in summary" creates a moment of doubt: did the other three fail? Test whether this ambiguity exists with the drives you're adding.

- `urd plan` output says `[OFF] Disabled: subvol4-multimedia, subvol6-tmp` — but these subvolumes are *enabled*, they just don't have send targets. "Disabled" is wrong. "Off" implies the user turned them off. They're guarded — local-only by design. This is a vocabulary bug worth catching in the test.

- The backup summary line `── Urd backup: success ── [run #22, 112.7s] ──` — is the run number meaningful to the user? Is `112.7s` what they care about? Or do they care about "everything is sealed"? Test what the user actually wants to hear at the end of a backup.

- `Pinned snapshots: 21 across subvolumes` — is this information or noise? Does the user know what a pinned snapshot is? Does this number going up or down mean anything to them? If you can't answer that, the line shouldn't be there.

## The Ask

1. **Add a restore test to every phase.** Pick one file. Restore it with `urd get` when only WD-18TB is connected, when 2TB-backup is added, when WD-18TB1 is swapped in. This tests the most important user journey and reveals whether drive configuration affects the restore experience.

2. **Add a "read-and-react" moment after every status check.** After each `urd status`, write one sentence: "Based on this output, the action I would take is ___." If you can't fill in the blank, that's a product finding more important than any token mismatch.

3. **Test the `[OFF]` label for guarded subvolumes.** Run `urd plan` and check whether "Disabled" accurately describes subvolumes that are enabled but local-only. If it doesn't, log it as a UX bug.

4. **Test whether the four small-send "silence" in backup progress creates confusion** when more drives are in play and more sends are happening.

5. **Move T3.4 (multimedia interval decision) earlier** — before Phase 1. It's a config decision that affects every subsequent test. Don't test against a config you know has a warning you haven't resolved.
