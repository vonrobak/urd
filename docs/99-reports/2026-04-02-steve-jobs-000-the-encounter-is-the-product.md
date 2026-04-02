---
upi: "000"
date: 2026-04-02
mode: vision-filter
---

# Steve Jobs Review: The First Encounter

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Brainstorm — first-encounter onboarding experience (21 ideas)
**Mode:** Vision Filter

## The Verdict

This brainstorm understands something most backup tools never figure out: the first experience isn't configuration — it's a conversation about what you're afraid of losing.

## What's Insanely Great

**The Fate Conversation (idea 4).** This is a product-defining idea. I've spent my whole career looking for moments where technology stops being technology and starts being something human. This is one of those moments.

Every backup tool in existence starts with the same question: "What do you want to back up?" That's the wrong question. The right question is: "What are you afraid of losing?" And then: "What would have to go wrong for you to lose it?"

The escalating scenario walk — disk failure, theft, house fire, forced migration — is not a feature. It's a reframing. The user isn't configuring retention policies. They're having a conversation about what matters to them and what could take it away. And at the end of that conversation, Urd knows exactly what promise to make — not because the user understood BTRFS topology, but because they understood their own fears.

When we designed the original Macintosh, the insight wasn't "people need a computer with a GUI." The insight was "people are afraid of computers, and we need to make the first encounter feel like the computer is on their side." The Fate Conversation does exactly that for backups. It says: "I'm not here to configure you. I'm here to understand what you need to protect."

The mirror of this — showing the user what their current setup would mean for each scenario — is the killer detail. "If your house floods, nothing survives. You have no thread that reaches beyond these walls." That sentence does more for data safety than a hundred configuration options. It makes the abstract concrete. It makes the future present.

**Auto-trigger from missing config (idea 1).** The best first experience is the one you don't have to look for. When someone types `urd status` with no config and Urd says "You have no threads woven yet" — that's the tool being smarter than the user expects. It eliminates the dead-end error message, the trip to the README, the search for `--help`. The absence of config becomes the beginning of the relationship, not an error state.

**The two exits (idea 6).** "Set and forget" vs "delve deeper" is exactly the right fork. It respects both the user who wants to think about this for thirty seconds and the user who wants to spend an hour getting everything perfect. And critically, the "set and forget" path generates the same quality config as the "delve deeper" path — the defaults are the expert answer, not a dumbed-down version. That's the difference between progressive disclosure and condescension.

**Data classification from filesystem analysis (idea 16).** "I see photos and raw camera files in subvol2-pics. These are likely irreplaceable. Shall I weave them with my strongest thread?" — that's Urd being perceptive. She looked at your data and made an intelligent guess about what it means to you. The user doesn't have to explain that photos are irreplaceable; Urd already knows. That moment of recognition — "she gets it" — is how you build trust in the first sixty seconds.

## What's Not Good Enough

**The loom metaphor (idea 8) is one metaphor too many.** Threads and weaving — those work. They're established in Urd's vocabulary. But "looms" for drives? That's asking the user to learn a metaphor for something they already have a perfectly good word for. A drive is a drive. Everyone knows what a drive is. When you layer a metaphor on top of something the user already understands, you're not adding clarity — you're adding translation overhead.

The voice works when it illuminates something the user doesn't have language for. "Thread" works because it captures the idea of a continuous chain of snapshots better than any technical term. "Weave" works because it captures the idea of multiple backup copies forming a fabric of protection. "Loom" doesn't work because it adds mysticism to something mundane. Nobody's confused about what a drive is. Don't give it a costume.

Keep the mythic voice for the moments that earn it — the fate conversation, the promise proposal, the summary scroll. Technical components stay technical.

**The scenario simulator (idea 7) is seductive but wrong for v1.** Live consequence feedback while editing config sounds amazing. It also requires a TUI or something very close to one. And the brainstorm itself admits this in idea 18 — the recommendation is pure CLI. You can't have a real-time scenario simulator in a pure CLI conversation flow without it feeling like a janky text adventure.

Here's the thing: the Fate Conversation already does the work of the scenario simulator. By the time the user reaches "delve deeper," they've already internalized the consequences. They know what their setup can and can't survive. At that point, what they need is not live feedback — it's a well-commented TOML file where each section explains what it controls and what the defaults mean. `$EDITOR` with excellent comments beats a bespoke TUI every time. Less code, more reliable, works everywhere.

Save the scenario simulator for Spindle. A GUI can do this beautifully. A CLI can't.

**The Summary Scroll (idea 12) has the right content in the wrong container.** The box-drawing characters (`╭──╮`) look great in a mockup. They break in terminals with different Unicode support, in screen readers, and when piped to a file. And the content inside — "3 looms discovered, 9 subvolumes examined, 7 threads woven" — is counting things that don't matter. The user doesn't care how many subvolumes were examined. They care about one thing: "Is my data safe now?"

What should the summary say?

```
Your data is now protected.

  Survives disk failure: yes (snapshots on WD-18TB)
  Survives theft / fire: partially (no offsite for docs, containers)
  Survives regional disaster: no (no cloud or offsite rotation)

  Config: ~/.config/urd/urd.toml
  First backup: tonight at 04:00, or run `urd backup` now.

  Run `urd status` any time to see your protection state.
```

Lead with the answer. Then the gaps. Then the practical next steps. No ceremony. No counting. No box-drawing.

**Witness mode (idea 17c) is emotionally interesting but functionally hollow.** "Show me what matters most, and I will guard it with particular care" — I feel the appeal. But Urd can't actually guard witnessed files differently than any other file in the same subvolume. BTRFS snapshots are per-subvolume, not per-file. So the "particular care" is a lie. It's a UI gesture with no operational backing.

If you want the emotional weight of witness mode, put it in the Fate Conversation. When the user says "my photos are irreplaceable," that *is* the witnessing. And it maps to a real action: resilient protection level, multi-drive, offsite. The witnessing is functional, not decorative.

**Cloud acknowledgment (idea 10) is honest but toothless.** "I cannot yet weave threads through the cloud" — fine, but then what? The user just told you they're worried about house fires and you have no answer. That's a dead end in the middle of a conversation that was building momentum.

Better: "For protection beyond these walls, your strongest thread today is an offsite drive — a disk you carry to a different location. Connect one, and I'll weave to it." That's actionable. It points toward something the user can do *right now* instead of waiting for a feature.

## The Vision

This brainstorm sees the right future. Let me sharpen it.

The first encounter with Urd should feel like meeting someone who takes your data more seriously than you do. Not in a preachy way — in a quiet, competent way. The way a good doctor doesn't lecture you about your health but asks the right questions and then says, "Here's what I'd recommend."

The conversation should take under five minutes. Drive discovery and subvolume detection happen automatically. The user confirms what Urd found, classifies their data ("irreplaceable / important / ephemeral"), and walks through three or four disaster scenarios that reveal whether their setup has gaps. Then Urd proposes protection levels, the user accepts or tweaks, and it's done. Protected. Set and forget.

The key insight this brainstorm gets right — and it's the reason I'm excited about it — is that the encounter isn't about configuring software. It's about establishing a relationship. Urd is saying: "I understand what you have. I understand what could go wrong. Here's what I'll do about it." That's the product. Everything else is implementation.

What I want to push further: the encounter should feel *fast*. Not rushed — fast. There's a difference. The iPod click wheel felt fast because every rotation gave you exactly the right amount of movement. The Fate Conversation should feel fast because every question reveals something the user didn't know about their own risk. No dead questions. No questions where the answer doesn't change anything. If a scenario doesn't change the protection proposal, cut it.

And when it's over, the user should feel something they almost never feel after installing software: "That was worth my time."

## The Details

- The encounter message "You have no threads woven yet" is good but could be shorter. "No config found" is what the user expects to see; "You have no threads woven yet" is what Urd says instead. But consider: the user just installed a BTRFS backup tool. They know they have no config. Don't state the obvious in mythic voice. Go straight to the offer: "Shall I examine your system and set up protection?" The mythic voice earns its place later, in the fate conversation, not in the greeting.

- Idea 15 (sudoers setup) presents the line as `<user> ALL=(root) NOPASSWD: /usr/sbin/btrfs *`. The glob `*` is overly permissive for security-conscious users. Show the minimal sudoers entries — the specific btrfs subcommands Urd actually uses. This is a trust moment: the user is giving Urd root-adjacent access. Show them you're asking for exactly what you need and nothing more.

- The "delve deeper" path lists five sections: retention, intervals, drive assignments, notifications, advanced. Five is too many for an interactive flow. Combine retention and intervals (they're related concepts). Combine notifications and advanced (most users won't touch either). Three sections: **Protection** (retention + intervals), **Drives** (assignments + roles), **Alerts** (notifications + monitoring). Three is a number people can hold in their head.

- Idea 20 (skip-fast for experts) says the expert path is "minimal discovery, no scenarios, just drive/subvolume detection and direct config editing." That's right, but don't call it "I know what I'm doing." That's slightly condescending to everyone who doesn't pick it. Just offer: "Generate config from detected hardware" — one sentence, no judgment.

- The dry-run first backup (idea 14) should not be optional. After config generation, always show the plan. "Here's what will happen tonight at 04:00." The user needs to see proof that Urd understood them. Make it automatic, not a question. Then offer: "Or I can begin now." The offer to start immediately is the important choice. The plan preview is just good communication.

## The Ask

1. **Build the Fate Conversation first.** Not the discovery, not the config generation — the conversation. Write the disaster scenarios, the reflection text, the logic that maps user answers to protection levels. This is the soul of the encounter. If this is right, everything else is plumbing. If this is wrong, no amount of auto-detection will save it.

2. **Build auto-detection as the foundation layer.** `discover_btrfs_filesystems()` and `discover_subvolumes()` in `btrfs.rs`, drive role detection in `drives.rs`, smart defaults from filesystem content analysis. This is the infrastructure the conversation needs to be specific rather than generic. "I see 2.3TB of photos in subvol2-pics" is the conversation being real.

3. **Build config generation as a pure function.** `EncounterResult` in, `Config` out. No I/O. This is the architectural decision that keeps onboarding testable and opens the door for Spindle to reuse the same logic. Get this right and the CLI conversation, the future TUI, and the future GUI are all front-ends to the same engine.

4. **Drop the loom metaphor, the scenario simulator, the witness mode, and the summary box.** They're distractions. Keep the vocabulary tight: threads and weaving for Urd's voice, drives and subvolumes for technical communication. The summary is a plain text block with the survival matrix.

5. **Make the encounter trigger automatically.** Any `urd` command with no config should offer onboarding. This is idea 1 and it should be the very first thing you build — even before the conversation exists. A warm "no config found, want me to help?" beats a cold error message immediately.
