---
upi: "000"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Ship or Test at v0.9.0?

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Strategic direction after v0.9.0 — test session vs. roadmap progression
**Mode:** Vision Filter (project-level)

## The Verdict

Test first. But not aimlessly — test *as* the user who will walk through the encounter, and let what breaks tell you what to build next.

## What's Insanely Great

**The last test-then-fix cycle was the best thing that happened to this project.** The v0.8.0 comprehensive test surfaced six real issues. Phases A–C fixed all of them. The result: v0.9.0 is the most honest, most coherent version of Urd that has ever existed. Status tells the truth. Drives have faces. Communication doesn't gaslight the user. That cycle — real usage revealing real gaps — produced better prioritization than any amount of roadmap planning could have.

**You're asking the right question at the right time.** Most projects barrel forward on the roadmap because the roadmap exists and forward feels like progress. The fact that you're pausing at v0.9.0 to ask "should we validate before we build?" shows the kind of discipline that separates tools that *work* from tools that merely *ship*.

**Phases A–C created a qualitatively different product.** Go back and read the v0.8.0 test results. False degradation. Safety gates that looked like failures. Drives that Urd couldn't identify. Now read the v0.9.0 changelog. Every one of those lies has been replaced with truth. That's not incremental improvement — that's crossing a trust threshold. The tool went from "works but sometimes confuses me" to "I believe what it tells me."

## What's Not Good Enough

**Phase D is the single largest, riskiest feature Urd has attempted.** The Encounter (6-H) is estimated at 4–6 sessions. It involves auto-detection, a conversational wizard, config generation, a new config schema (ADR-111 adjacency), and integration with every module that Phases A–C just fixed. It is the feature where every other feature becomes visible. If something is still broken underneath, the encounter will expose it at the worst possible moment — during a new user's first impression.

Building the encounter on an untested v0.9.0 foundation is exactly the mistake that the v0.8.0 test session caught last time. Phases A–C changed awareness, output, voice, drives, notifications, and the executor. That's half the codebase. You cannot assume all of those changes compose correctly in real usage just because the unit tests pass. 763 tests are necessary. They are not sufficient.

**The roadmap's own sequencing report called for this.** The last Steve review literally said: "Schedule a second physical drive test session after v0.9.0 ships. Verify the full arc before starting the encounter." That wasn't a suggestion. It was item #6 in The Ask. The encounter is too important to build on untested ground. That judgment hasn't changed.

**Progressive disclosure (6-O) before testing is backwards.** 6-O changes how information is presented to the user. If the underlying information has bugs that only surface in real usage, 6-O will paper over them with progressive disclosure — hiding problems behind "show more" rather than fixing them. Test the current information layer first. Then decide what to progressively disclose.

## The Vision

Here's what I'd do. Not "maybe consider" — here's what I'd do:

### Week 1: The real-usage test session

Not a comprehensive 30-test marathon. A focused session with two goals:

**Goal 1: Live with v0.9.0 for a few days.** Let the nightly timer run. Let the Sentinel watch. Plug in a drive, unplug it. Check `urd status` in the morning. Check `urd drives` when a drive is connected and when it's away. Read the notifications. Do what a user does: glance at it, trust it, and occasionally invoke it. Write down every moment where you hesitate, squint, or feel uncertain.

**Goal 2: Simulate the encounter user's journey.** Pretend you've never used Urd. Run `urd` with no arguments. Run `urd status`. Run `urd drives`. Run `urd backup`. What's the experience? Not whether it *works* — whether it *makes sense* to someone who doesn't know the architecture. This is the encounter's dress rehearsal. The encounter can't be better than the tools it introduces.

The output of this session isn't a test matrix. It's a prioritized list: "here's what a new user would stumble on." That list becomes the scope for a targeted fix phase — maybe one session, maybe two. Then you start the encounter with a foundation you've actually stood on.

### Why not just push to Phase D?

Because Phase D is 6–8 sessions of building the most important feature in Urd's lifecycle, and the difference between building it on solid ground vs. building it on "probably fine" ground is the difference between an encounter that delights and an encounter that subtly disappoints.

The original Time Machine shipped late because we refused to ship it broken. The cost of delay was measured in weeks. The cost of shipping a bad first-run experience would have been measured in years of reputation. The encounter *is* Urd's first impression. It gets one chance.

### What about the tech debt items?

P6a (enum rename) and P6b (config Serialize) — do P6b during or right after the test session if there's energy. It's a prerequisite for the encounter's config generation. P6a is cosmetic and can wait forever. Neither is worth a dedicated session.

The known issues list (NVMe accumulation, FileSystemState naming, status string fragility, parallel notification builders, planner parameter limits) — these are real but none are user-facing enough to block the encounter. If the test session surfaces any of them as actual user pain, promote them. Otherwise, they wait.

## The Details

- **The `--auto` flag in the systemd timer is still pending since v0.8.0.** This is a deployment detail, but it means your nightly runs aren't using interval gating. Fix it before the test session so you're testing real autonomous behavior.

- **The CHANGELOG comparison links are stale.** `[Unreleased]` still points to `v0.7.0...HEAD`. Update them to reflect the actual tag progression through v0.9.0. A user browsing the changelog shouldn't see broken links.

- **Check the Sentinel state file schema.** v0.9.0 added reconnection notifications and drive adoption. Does the sentinel state file correctly reflect these new states? If Spindle ever reads this file, it needs to be right.

- **`urd get` for directories is still missing.** If the test session includes a restore scenario (it should), you'll feel this gap viscerally. Don't add it now — but let the test session confirm whether it needs to be pre-encounter or can remain horizon.

## The Ask

1. **Run a focused real-usage session before starting Phase D.** Live with v0.9.0 for a few days. Simulate the new-user journey. Document what feels wrong, not what fails.

2. **Fix the systemd timer `--auto` flag.** You've been running without interval gating since v0.8.0. That's not testing real behavior.

3. **Update CHANGELOG comparison links.** Small detail, but broken links in the changelog undermine the craft signal.

4. **Do P6b (config Serialize) as a quick patch during the test-session week.** It's a prerequisite for the encounter and low-risk. Get it out of the way.

5. **Start Phase D only after the test session's findings are addressed.** If the test session finds nothing — great, you've validated the foundation and can build with confidence. If it finds issues — great, you've caught them before they became the encounter's first impression. Either way, you win.
