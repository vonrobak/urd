---
upi: "000"
date: 2026-04-02
mode: vision-filter
---

# Steve Jobs Review: Project Trajectory

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Roadmap, priorities, release history, project direction
**Mode:** Vision Filter (project-level)

## The Verdict

Urd has world-class architecture and a real vision, but it's drifting toward becoming a beautifully engineered tool that nobody new can start using -- the onboarding gap is the existential risk, not the feature gap.

## What's Insanely Great

**The architecture is right.** Planner/executor separation, pure functions for core logic, fail-open backups, fail-closed deletions -- this is how you build a tool that protects data. I've seen entire companies that can't articulate their error philosophy this cleanly. The ten architectural invariants aren't decoration; they're load-bearing. Keeping them is non-negotiable.

**The catastrophic failure shaped the design in the right direction.** Most projects bury their disasters. Urd wears the scar tissue as policy: space guards, transient cleanup, NVMe accumulation tracking. The snapshot congestion incident made this a more cautious, more serious tool. That's the correct response.

**The two-mode mental model is profound.** "Invisible worker, invoked norn" -- this is the central insight that separates Urd from every other BTRFS snapshot script. Time Machine didn't succeed because it was the best backup technology. It succeeded because 95% of the time you forgot it existed, and the 5% you needed it, the experience was magical. Urd understands this at the architectural level.

**Velocity is extraordinary.** v0.1.0 to v0.7.0 in 10 days. 162 commits, 695 tests, 80+ reports. The development process -- design reviews, adversary reviews, brainstorms -- isn't bureaucratic overhead; it's producing better work faster. The archived 550-line roadmap was replaced with an 85-line one. That's discipline.

**The backup-now imperative idea is exactly right.** The insight in that sketch -- "the interval logic exists to throttle automated runs, not to refuse manual ones" -- shows someone who understands the user's mental model. When I press "Back Up Now" and the machine says "nothing to do," I've been disrespected. Fix this first.

## What's Not Good Enough

**The roadmap is pointed inward, not outward.** Progressive disclosure, enum renames, config Serialize refactors, guided setup wizard -- these are real items, but the sequencing says "I'm polishing the house before inviting guests." The problem is there's no clear gate where guests arrive. When does someone who isn't the author first use Urd?

**ADR-111 is a landmine being politely stepped around.** The config system is the largest deferred gate, and the roadmap says "the wizard proves the schema before migration code is written." That's reasonable engineering but terrible product thinking. The config is the first thing a new user touches. If the schema they set up today becomes legacy before v1.0, you've betrayed their trust. Either commit to the current schema or implement ADR-111. Straddling is the worst option.

**Too many vocabulary changes, not enough user-facing capability.** Between v0.5.0 and v0.7.0, there were three rounds of vocabulary work: sealed/waning/exposed, skip tag renames, notification mythology cleanup, column header renames, drive role terminology. Each one was thoughtful. Together, they signal a project that's still deciding what words to use rather than what problems to solve. Vocabulary should stabilize and stay stable. Ship it and live with it.

**The deferred list is where the product lives.** SSH remote targets, directory restore, `urd find`, drive replacement workflow -- these are the features that make someone say "I need Urd" instead of "that's a nice tool." They're all marked "no current timeline." That's honest, but it also means the most compelling user stories have no path to reality.

**Spindle is too far away.** The tray icon is described as depending on Sentinel active mode and visual state work. That's four dependency layers deep. But Spindle is the Time Machine experience. It's what makes Urd feel like a product instead of a CLI tool. Even a minimal Spindle -- icon that turns green/yellow/red based on promise states, click to see `urd status` output -- would transform the perception of the project. It doesn't need active mode. It doesn't need the full visual state framework. It needs to exist.

**The report archive is getting heavy.** 80+ design and adversary reviews in 10 days. Each one added value at the time. But this volume of meta-documentation is a maintenance burden and a signal of over-process. A new contributor would drown. Not every feature needs a design review, an adversary review, and an implementation review. The tiered workflow is good -- use it to actually reduce ceremony for small items, not just relabel it.

## The Vision

Here's what Urd looks like at v1.0, and here's the story someone tells:

"I plugged in a drive, ran one command, and Urd started protecting my data. I forget it's there most of the time. When my SSD died last month, I ran `urd get` and had my files back in minutes. There's a little icon in my tray -- green means safe. That's all I need to know."

That's four things: (1) setup takes one interaction, (2) daily operation is invisible, (3) restore is fast and obvious, (4) the status is always one glance away. Urd has #2 working. #3 is partially there (files only, no directories). #1 and #4 are in the roadmap but buried under prerequisites. The path from v0.7 to that story needs to be shorter and straighter.

v1.0 doesn't need SSH targets, cloud backup, mesh topology, or multi-user mode. It needs: guided setup, reliable invisible operation (already done), clear restore (directories + files), and a visual presence (Spindle). Everything else is v2.

## The Details

**Sequencing problem: 6-O before backup-now.** The roadmap puts progressive disclosure (6-O, 2 sessions) before the backup-now imperative. Invert this. Backup-now is a functional gap that violates the core mental model. Progressive disclosure is presentation polish. Functional correctness before presentation polish, always.

**The 7.5 session estimate for the active arc is optimistic.** The guided setup wizard alone is estimated at 4 sessions, and it depends on three prerequisites. Complex interactive features with config generation, validation, and file writing always take longer than estimated. Budget 10-12 sessions for the arc.

**P6a (enum rename) should be done as tech debt, not as a roadmap milestone.** It's a search-and-replace with test updates. Treating it as a session-sized item adds ceremony that doesn't match the scope. Same for P6b.

**Test count (695) is healthy but test coverage tells a different story.** The known issues list includes `assess()` not respecting per-subvolume drive scoping, status strings matched as raw strings, and parallel notification builders. These aren't just tech debt -- they're correctness gaps in core promise evaluation. A user whose htpc-root subvolume is only configured for one drive is getting false degradation warnings. That's the promise model lying to them.

**The `urd get` limitation (files only, no directories) is more important than the roadmap suggests.** When someone's recovering from a failure, they almost never need a single file. They need a directory -- their project folder, their config directory, their photos from last week. This should be on the horizon, not buried in a deferred tech debt note.

## The Ask

1. **Ship backup-now next.** It's the highest-impact, lowest-risk change on the board. The idea sketch is clean. Design it, build it, release it. Before progressive disclosure, before enum renames, before everything else.

2. **Fix the assess() scoping bug.** The promise model is the soul of Urd. If it's lying about subvolume states because it ignores drive scoping, fix it now. Not as tech debt. As a correctness fix. This is a data safety question -- north-star test #1.

3. **Commit to a config schema or migrate it.** Stop building features on a config system you've already decided is wrong. Either accept the current schema as good enough for v1.0, or implement ADR-111 before the wizard. The wizard is the wrong place to "prove" a schema -- by the time the wizard exists, you have users on the old schema who need migration anyway.

4. **Build minimal Spindle after the setup wizard, not after Sentinel active mode.** A read-only tray icon that shows promise state and last backup time. No active mode dependency. No notification integration. Just presence. The icon turns red when data is at risk. That's enough for v0.9.

5. **Freeze vocabulary.** The current terms (sealed/waning/exposed, thread, connected/away) are good. Stop changing them. Every rename breaks muscle memory and documentation. Ship what you have and iterate on semantics only if user feedback demands it.

6. **Add directory restore to `urd get` before v1.0.** This is table stakes for a backup tool. A user who can restore files but not directories will not recommend Urd.

7. **Reduce ceremony for patch-tier work.** Enum renames, string constants, trait renames -- these don't need design docs and adversary reviews. They need a branch, a PR, and tests passing. The tiered workflow exists; use the lightest tier more aggressively.
