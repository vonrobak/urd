---
upi: "000"
date: 2026-04-02
mode: product-review
---

# Steve Jobs Review: v0.8.0 Test Results

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Complete test execution results from `docs/99-reports/2026-04-02-testing-urd-v0.8.0.md` — 30 tests across 4 phases, all drive configurations
**Mode:** Product Review

## The Verdict

Urd v0.8.0 is a competent backup engine that doesn't know which drive it's talking to, and doesn't tell you when something important happens.

## What's Insanely Great

The chain-break full send gate saved this user's data. When the 2TB-backup came back with a broken htpc-root chain, Urd didn't silently blast 31.8GB onto the drive — it stopped and said "this requires a full send, opt in explicitly." That's the kind of safety that earns trust. The user didn't have to be an expert; Urd was the expert for them. Protect this pattern with your life. Every destructive-ish operation should have this level of thoughtfulness.

The space-aware skip for subvol5-music is exactly right. "Estimated ~1.3TB exceeds 1.0TB available" — Urd looked at the drive, looked at the data, and said no. Not an error, not a failure — a skip with a clear reason. The user knows what happened, knows why, and knows it wasn't a mistake. That's what "fail open, fail closed" looks like in practice and it's beautiful.

The two-backup test (T3.1) revealed something lovely: the second backup finished in 22 seconds. The user ran it, Urd did the work, and the delta was tiny because almost nothing changed. No complaint, no "you just did this," no nag. Just: here's what happened, here's how small it was. That's the invisible worker at its best — responsive without being chatty.

The redundancy warning when all drives disconnected — "htpc-root lives only on external drives while local copies are transient. Recovery requires a connected drive" — that's Urd thinking about what the user needs to know, not what the system's state is. More of this. Much more of this.

## What's Not Good Enough

### 1. The drive identity crisis is an existential threat to Urd's promise.

WD-18TB1 plugged in. It mounted at WD-18TB's path. Urd said "WD-18TB connected" and planned 1.3TB of full sends to the wrong drive. No warning. No hesitation. No "wait, something's different about this drive."

If that backup had run — and there was nothing in the UX preventing it except the user's own instinct — every pin file would have pointed at snapshots on the wrong physical disk. When the real WD-18TB came back, every chain would be broken. Every subsequent backup would be full. The user would lose weeks of incremental history because Urd couldn't tell two drives apart.

This isn't a bug. This is a broken promise. Urd says "all sealed" — that means "I know where your data is and it's safe." But Urd doesn't actually know where the data is. It knows where a mount path is. Those are not the same thing.

The drive token system *exists*. It's written to disk. It's stored in SQLite. But nobody reads it when it matters. The token should be the first thing Urd checks when it sees a drive, and a mismatch should be a hard stop — not a warning, a stop. "This drive says it's WD-18TB but I don't recognize it. Run `urd drives identify` to resolve this."

Here's what makes me angry: the user smelled danger. A human being looked at the output and thought "something's wrong, I'm not going to run this." The tool should have been the one smelling danger. That's what it's for.

### 2. Drives come and go in silence. That's wrong.

2TB-backup was absent for 10 days. The status page said "protection degrading" — good. Then the drive came back. What happened? Nothing. Silence. The user had to run `urd sentinel status` to discover that Urd had noticed.

Think about this from the user's perspective. For 10 days, every time they checked status, Urd told them their protection was degrading. That creates low-grade anxiety. Then the fix happens — the drive comes back — and Urd says nothing. The anxiety doesn't resolve because the resolution was invisible.

When I built the iPod, one of the non-negotiable details was the click sound. You could have navigated the scroll wheel without it. But the click told you something happened. It closed the loop.

Drive reconnection needs to close the loop. A desktop notification: "2TB-backup is back. Your protection will be restored on the next backup." Or even just a different first line in `urd status`: "2TB-backup reconnected after 10 days — run `urd backup` to restore full protection." Something. Anything that acknowledges the moment.

The same applies to WD-18TB1 — the *offsite* drive. When your offsite copy returns from wherever it's been, that should be an event. Not a row in a table. An event.

### 3. "All sealed. 3 degraded." — Pick one.

The baseline status line was: "All sealed. 3 degraded — 2TB-backup away for 8 days."

I raised this in my test plan review and it showed up in practice exactly as predicted. All sealed means "your data is safe." Three degraded means "three things are unhealthy." To someone who isn't thinking in Urd's two-axis model, this reads as: "everything is fine but also three things are broken."

The test results make this worse. The three degraded subvolumes were htpc-home, subvol2-pics, and htpc-root. Two of those (htpc-home and subvol2-pics) were false degradation from the assess() scoping bug — they don't even send to 2TB-backup. The third (htpc-root) was legitimate. So the status line conflated two fake problems with one real one, presented them all the same way, and expected the user to figure out which was which.

The summary line needs to answer one question: "Is my data safe, and what should I do?" Not "here are two orthogonal axes that interact in ways you'll need to think about."

### 4. `[OFF] Disabled` is a lie.

subvol4-multimedia and subvol6-tmp show as `[OFF] Disabled` in every plan and backup view. They are not disabled. They are actively snapshotted. They just don't send anywhere because they're local-only by design.

"Disabled" tells the user: "you turned this off." That's wrong. The user didn't turn anything off. They chose a protection level that means local snapshots only. The skip reason should reflect their choice, not imply a mistake: `[LOCAL]` or `[LOCAL ONLY]` or even just omit them from the skip list entirely — they're doing exactly what they're supposed to do.

This showed up in T0.3, T0.4, T1.3. Every single time the user looked at a plan, they saw a label that mischaracterized their own configuration. That's not a cosmetic issue. It erodes trust in the tool's understanding of what you told it to do.

### 5. The doctor contradicts the user's informed decision.

Doctor says: "Add uuid = '647693ed-...' to WD-18TB." The user tries. Config rejects it: "duplicate drive uuid." The user has two drives that share a UUID from cloning — a real-world scenario, not an error. Doctor gives advice that the system itself prevents you from following.

This is the product equivalent of a form that tells you your input is wrong but doesn't accept any alternative. It makes the user feel stupid for having a legitimate situation.

Doctor should either: (a) detect that the UUID is already configured on another drive and suppress the suggestion, or (b) say something useful: "WD-18TB's UUID matches WD-18TB1 — this is expected for cloned drives. Run `btrfstune -u` on one drive to separate their identities."

## The Vision

Here's what this test session revealed: Urd v0.8.0 is an excellent *engine* with a mediocre *relationship* with its user.

The engine is impressive. Incremental sends work. Space estimation works. Chain-break safety gates work. Retention thinning works. The planner is smart, the executor is careful, the state management is solid. Run #23 through #25 showed a system that handles real drives, real data, and real failures with mechanical competence.

But a backup tool isn't an engine. A backup tool is a *promise*. And a promise requires a relationship — the tool has to know you, know your situation, and communicate in a way that makes you feel your data is in good hands.

Three things would transform Urd from a good engine into a great product:

**First: know your drives.** The token system exists but isn't enforced. Make it load-bearing. When a drive appears, verify its identity before doing anything. When the identity doesn't match, stop and explain. When a cloned drive appears at the wrong path, say so. This isn't a feature — it's a prerequisite for the word "sealed" to mean anything.

**Second: close the loop.** Every state change the user has been told about needs a resolution. "Protection degrading" needs a matching "protection restored." "Chain broken" needs a matching "chain mended." The Sentinel watches everything — it just doesn't say anything. Give it a voice for the moments that matter.

**Third: the summary line tells you what to do, not what to think about.** "All sealed" is the answer to "is my data safe?" Good. But when things aren't all sealed, the user needs: what's wrong, why, and what to do about it — not a count of degraded subvolumes they need to investigate themselves. "htpc-root needs a full send to 2TB-backup — run `urd backup --force-full` when ready" is a thousand times better than "1 degraded."

The test session ended with the user having run 25 backups, exercised three drives, and avoided a catastrophic mis-send through human instinct. The instinct was right. The tool should have gotten there first.

## The Details

- When the user ran `urd get` without arguments, the error was: "the following required arguments were not provided: --at <AT> <PATH>". That's cargo-clap boilerplate. This is the most important command in the entire tool — the moment of rescue. The bare `urd get` should show a guided prompt or at least a human-written message: "To restore a file, tell Urd what you're looking for and when: `urd get ~/documents/important.txt --at yesterday`".

- The backup summary says "partial" for run #23 because htpc-root's full send was gated. But the *word* "partial" sounds like something went wrong. The gate was a *safety feature working correctly*. Consider: "success (1 gated)" or "complete — htpc-root full send deferred (see `urd backup --force-full`)".

- "Pinned snapshots: 21 across subvolumes" appeared in every status output. It never changed the user's behavior. It never informed a decision. It's information for the developer, not the user. Either make it meaningful ("21 snapshots pinned for incremental chains — safe to ignore") or remove it from the default view.

- The `[SPACE]` skip message is good but could be great: "subvol5-music: send to 2TB-backup skipped: estimated ~1.3TB exceeds 1.0TB available" — this tells you what happened but not what to do. Add: "free up space on 2TB-backup or increase min_free to proceed."

- The backup pre-action briefing — "Backing up everything to 2TB-backup and WD-18TB. 9 snapshots, 10 sends, ~39.8GB" — is good. The follow-up "WD-18TB1 is away — copies will update when it returns" is *great*. That's the voice of a tool that understands your situation. More of exactly this.

- `urd doctor --thorough` with 2TB-backup present showed: "Pin file is 8 day(s) old (threshold: 2 day(s)) — sends may be failing". The pin is 8 days old because the *drive* was absent for 8 days, not because sends failed. The diagnostic is technically accurate but practically misleading. Doctor should correlate pin age with drive absence: "Pin file is 8 days old — expected, drive was absent for that period."

## The Ask

1. **Make drive tokens load-bearing.** Check the on-disk token against SQLite on every drive mount detection. Mismatch = hard stop with clear guidance. This is the single highest-impact change for data safety. It directly addresses F2.3 and transforms the token from a bookkeeping artifact into a safety system.

2. **Add drive reconnection notifications to Sentinel.** When a drive transitions from absent to connected, emit a desktop notification. The notification should name the drive, say how long it was gone, and tell the user what to do: "2TB-backup reconnected after 10 days — run `urd backup` to catch up." This closes the anxiety loop.

3. **Fix assess() scoping.** False degradation erodes trust in the status display. If "all sealed, 3 degraded" includes two false positives, the user learns to ignore degradation warnings — and then misses the real one.

4. **Replace `[OFF] Disabled` with `[LOCAL]` for local-only subvolumes.** Five-minute fix. Immediate trust improvement. The label should reflect the user's intention, not imply a problem.

5. **Rewrite the bare `urd get` message.** This is the rescue command. When someone types it without arguments, they're looking for help. Give them a human example, not a usage() dump.

6. **Improve the "partial" backup result label.** "Partial" implies failure. When the cause is a safety gate working correctly, the language should reflect that. "Complete with 1 deferred" or similar.

7. **Correlate doctor pin-age warnings with drive absence.** Don't tell the user sends "may be failing" when the pin is old because the drive was on a shelf. Doctor has the data to distinguish these cases — use it.
