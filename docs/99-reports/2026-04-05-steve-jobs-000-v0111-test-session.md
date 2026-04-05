---
upi: "000"
date: 2026-04-05
mode: product-review
---

# Steve Jobs Review: v0.11.1 Test Session

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-05
**Scope:** All human-facing CLI outputs from a live test session of urd v0.11.1
**Mode:** Product Review
**Commands reviewed:** bare invocation, status, verify, doctor, doctor --thorough, drives, history, retention-preview (error), drive (typo)

## The Verdict

Urd is a backup tool that genuinely respects the person using it. The bare invocation
alone — three lines that tell you whether your data is safe — is better than anything
I've seen from any backup tool on any platform. But it's not done. Several surfaces still
speak engineer, not user. And the tool has a habit that will hurt it: it answers questions
the user didn't ask while making them work to get the answer they wanted.

## What's Insanely Great

**The bare invocation is a masterpiece of information hierarchy.**

```
All connected drives are sealed. 2 degraded. Last backup 10h ago.
2 subvolumes need attention — run `urd status` for details.
```

Three lines. In those three lines, I know: my data is safe, two things need attention but
aren't urgent, and my last backup was recent. Then it tells me exactly what to do next.
This is what every backup tool should do and none of them do. Time Machine buries this
behind a GUI. Restic doesn't even have a concept of "are you safe." Borg makes you ask
five different questions to get one answer. Urd gives you the answer before you ask.

**The vocabulary is exactly right.** "Sealed," "degraded," "exposed," "waning," "unbroken" —
these words carry weight without being theatrical. They don't explain, they *communicate*.
When I read "sealed," I feel something that "PROTECTED" never gave me. When I see "waning,"
I understand the trajectory without needing a number. This is the mythic voice working
exactly as intended — in the vocabulary choices, not in dramatic prose.

**The status table is dense and honest.** Nine subvolumes, their snapshot counts, ages,
thread health — all in a compact table that doesn't waste a character. The `(10h)` and
`(1d)` age markers are perfect: they tell you the age of the *newest* snapshot without
cluttering the view. The thread column — `unbroken`, `ext-only` — is the kind of detail
that rewards the user who looks closer.

**The TTY/pipe split is good design instinct.** Interactive gets the human voice, pipes get
JSON. This is the correct separation. Most tools get this wrong — they either pipe ANSI
codes into files or dumb down the TTY output. Urd respects both audiences.

**`urd doctor` is calm and comprehensive.** The checklist format — Config, Infrastructure,
Data Safety, Sentinel — tells you what was checked, not just what failed. "All clear" at
the end is reassuring in a way that no output at all never is. The `--thorough` flag is
progressive disclosure done right: the casual check gives you the summary, the deep check
gives you everything.

## What's Not Good Enough

**The `urd status` summary line drops the "drives are sealed" framing.**

The bare invocation says: `All connected drives are sealed.`
The status command says: `All sealed. 2 degraded — WD-18TB1 away for 8 days.`

"All sealed" without context is weaker than "All connected drives are sealed." The status
command is the one place where the user deliberately asked "how am I doing?" — give them
the full sentence. Moreover, "2 degraded — WD-18TB1 away for 8 days" is good, but it only
names one drive while two are absent (WD-18TB1 and 2TB-backup). The drive summary below
covers both, but the summary line creates an incomplete first impression.

**The status table numbers are opaque.**

```
sealed    degraded  subvol3-opptak      31 (10h)  7 (1d)   unbroken
```

What does 31 mean? Snapshot count. What does 7 mean? Snapshot count on the drive. But
nowhere does the table say this. A new user sees `31 (10h)` under `LOCAL` and has to
*guess* that 31 is a count and 10h is an age. The column headers should be enough to decode
any row. `LOCAL` is not enough. `SNAPSHOTS` or even `LOCAL (count/age)` would make this
self-documenting. The numbers are useful — the framing makes them mysterious.

**`urd verify` is too verbose for the common case.**

When 6 of 7 subvolumes have identical results (all OK on WD-18TB, two drives not mounted),
the output is 60+ lines of repetition to surface one actual finding (htpc-root's broken
chain). The verify command should lead with what matters:

```
htpc-root/WD-18TB: Chain broken — pinned snapshot missing locally.
  Next send will be full.

6 subvolumes verified clean. 2 drives not mounted (WD-18TB1, 2TB-backup).
```

The detailed per-subvolume output should be behind a `--verbose` or `--detail` flag. The
user ran verify because they want to know if something is wrong, not to read 34 lines of
"OK."

**`doctor --thorough` buries its one real finding.**

```
    ✗ htpc-root/WD-18TB: Pinned snapshot missing locally...
```

This line — the only actual problem — appears on line 13 of 15, buried under 12 identical
"Drive not mounted — skipping" warnings. The user already knows those drives aren't mounted.
The thorough check should separate *findings* from *expected conditions*. A drive being
absent is a known state, not a finding. The broken chain is a finding. Don't make me read
past 12 non-events to find the one event that matters.

**`retention-preview` error is a wall of text.**

```
Error: specify a subvolume or use --all. Configured subvolumes: subvol3-opptak,
htpc-home, subvol2-pics, subvol1-docs, subvol7-containers, subvol5-music,
subvol4-multimedia, subvol6-tmp, htpc-root
```

This is a dump, not guidance. Nine subvolume names in a comma-separated list on one line.
The error should show the right usage pattern first, then list the names in a scannable
format:

```
Usage: urd retention-preview <subvolume> or urd retention-preview --all

Available subvolumes:
  subvol1-docs        subvol3-opptak      subvol5-music
  subvol2-pics        subvol4-multimedia  subvol6-tmp
  subvol7-containers  htpc-home           htpc-root
```

**`--verbose` doesn't exist, but users expect it.** Both `urd status --verbose` and
`urd plan --verbose` failed with clap's generic "unexpected argument" error. A tool this
thoughtful about UX should either support `--verbose` (even as an alias for existing detail
flags) or give a helpful error: "Try `urd status` — it already shows the detail view."

**`doctor` says "All clear" but `status` says "2 subvolumes need attention."** These are
not contradictory if you understand that "All clear" means infrastructure is fine and
"need attention" means degraded drive state. But the user doesn't know that. They ran
`doctor` because `status` told them to, and `doctor` said everything is fine. This is a
trust gap. Either `doctor` should acknowledge the degradation that `status` surfaced, or
the status advice text should say `urd doctor --thorough` instead of just `urd doctor`.

**`1 issue(s).` in doctor --thorough.** Don't pluralize with parenthetical `(s)`. Either
write `1 issue` or `3 issues`. This is a small thing that signals low attention to detail
in a tool that otherwise demonstrates exceptionally high attention to detail. The contrast
makes it worse, not better.

## The Vision

Here's what Urd is becoming, and it's rare: a tool that makes invisible infrastructure
feel *known*. Not monitored — known. The difference is the same as the difference between
a security camera and a trusted friend watching your house. Both detect problems. One makes
you feel surveilled, the other makes you feel safe.

The bare invocation is the proof of concept for this vision. In three lines, it transforms
"do I need to worry about backups?" from a question that requires expertise into one that
requires reading a sentence. That's the transformation.

Where it needs to go next: that same clarity needs to flow through every surface.
`verify` should be as scannable as `status`. `doctor` should answer the question `status`
raised, not a different question. The error messages should guide with the same confidence
that the success messages inform.

And the vocabulary — sealed, waning, exposed, unbroken — this is genuinely distinctive.
No other backup tool has a vocabulary that carries emotional weight while remaining
technically precise. Protect this. Don't let it get diluted by falling back to generic
terms in new features.

## The Details

1. **"Last backup 10h ago" vs "Last backup: 2026-04-05T04:01:11"** — The bare invocation
   uses the relative "10h ago" (perfect). The status command uses an ISO timestamp
   (functional but cold). Status should lead with the relative time and append the
   timestamp: `Last backup: 10h ago (04:01, success) [#29]`. Nobody's first question is
   "what ISO 8601 timestamp was my last backup?"

2. **`sealed_count()` says "9 of 9 sealed"** but it renders as "All sealed" — good. But if
   it were 8 of 9, it would say "8 of 9 sealed" which sounds like a test score, not a
   warning. Consider: "8 sealed. 1 exposed." — lead with the good news, name the problem.

3. **`ext-only` in the thread column** — what does this mean? It's technically accurate
   (htpc-root has no local snapshots, only external). But a user seeing `ext-only` next to
   a `sealed / healthy` row has no idea if this is good, bad, or neutral. Consider
   `drive-only` or even `—` with the explanation in the drive summary section.

4. **Drives table alignment** — `TOKEN` column has `✓` for WD-18TB but `—` and
   `recorded` for the others. The visual alignment breaks slightly because the checkmark
   character and the em-dash have different widths. This is the kind of detail that
   matters at 2am.

5. **History table: `0s` duration for run #20** — what does a 0-second backup mean? It
   probably means nothing needed doing (all skipped). But "0s" reads as "broken." Consider
   `<1s` or `—` with a tooltip concept, or append `(no-op)` when duration rounds to zero.

6. **"Run suggested commands to resolve"** at the end of `doctor --thorough` — but it
   didn't suggest any commands. The chain break message says what's wrong but not what to
   do. Either suggest the command (`urd backup` to trigger a full send) or don't promise
   suggested commands.

7. **The `urd drive` typo recovery** is clap's built-in "tip: a similar subcommand exists:
   'drives'" — functional but impersonal. Since Urd has a voice, consider intercepting
   this: `Did you mean 'urd drives'?` — one line, no boilerplate.

8. **"protection degrading"** in the drives section of status — degrading implies an
   active process, which is accurate (copies are getting staler). But "protection
   degrading" reads like something is *happening right now* that the user should stop.
   Consider "protection aging" or simply "stale" — conveys drift without urgency that
   can't be acted on (the user can't teleport the drive home).

## The Ask

1. **Fix the verify/doctor information hierarchy.** This is the most impactful change.
   Both commands bury findings under noise. Lead with what's wrong. Summarize what's fine.
   Put the detail behind a flag. This is the `urd status` principle applied to diagnostics.

2. **Reconcile doctor and status.** When status says "run doctor for details," doctor must
   surface those details. The trust gap between "2 need attention" → "All clear" will
   erode user confidence.

3. **Add `--verbose` as an alias** for whatever the existing detail flag is, or give a
   helpful redirect. Users will try it — don't punish them with a generic clap error.

4. **Fix the `1 issue(s)` pluralization** and the "Run suggested commands" promise that
   isn't fulfilled. These are 10-minute fixes that remove paper cuts.

5. **Humanize the status timestamp.** Lead with relative time, append the exact time.
   The bare invocation already knows the right format — extend that instinct to status.

6. **Improve the retention-preview error.** Format the subvolume list as a scannable
   column layout with a usage example above it.

7. **Consider a `--quiet` mode for verify** (or make the current output the `--verbose`
   mode and the summary the default). Nine subvolumes × three drives × five checks is a
   lot of output for "one thing is wrong."

8. **Add column hints to the status table** so the numbers are self-documenting. This
   doesn't need to be verbose — `LOCAL` → `LOCAL #` or a footnote line below the table.
