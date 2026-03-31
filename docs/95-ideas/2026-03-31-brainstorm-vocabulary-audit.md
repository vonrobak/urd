# Brainstorm: Vocabulary Audit — Urd's User-Facing Language

> **TL;DR:** A systematic audit of every user-facing term in Urd, evaluating each
> against clarity, precision, consistency, and mythic resonance. The goal is not to
> rename everything — it's to identify where the current vocabulary is excellent,
> where it creates confusion, and where a better word exists that is both more precise
> and more evocative. The bar is high: technical accuracy must not be sacrificed for
> character.

**Date:** 2026-03-31
**Status:** raw

---

## Audit Method

Every user-facing string was extracted from `voice.rs`, `output.rs`, `cli.rs`,
`error.rs`, `notify.rs`, `awareness.rs`, and `types.rs`. Terms are grouped by
semantic domain and evaluated against four criteria:

1. **Clarity** — Does a new user understand this term without explanation?
2. **Precision** — Does it mean exactly one thing in Urd's domain?
3. **Consistency** — Is it used the same way everywhere?
4. **Resonance** — Does it carry weight? Does it sound intentional?

The design principle from the grill-me session: **Urd's voice is mythic because
it is precise, not despite it.** Precision IS the character. When in doubt, the
technically clearer term wins.

---

## Domain 1: Data Safety (The Promise System)

These are the most important words in Urd. They answer "is my data safe?"

### Current vocabulary

| Term | Where | Meaning |
|------|-------|---------|
| PROTECTED | awareness.rs | Promise is being honored |
| AT RISK | awareness.rs | Backups aging, safety window narrowing |
| UNPROTECTED | awareness.rs | No valid backup, data loss possible |
| OK | voice.rs (status table) | Mapped from PROTECTED |
| aging | voice.rs (status table) | Mapped from AT RISK |
| gap | voice.rs (status table) | Mapped from UNPROTECTED |

### Analysis

**PROTECTED / AT RISK / UNPROTECTED** — These are the awareness model's internal
states. They're precise and well-ordered. No change needed in the data layer.

**OK / aging / gap** — These are the voice layer's rendering of the same states.
This is where the vocabulary audit matters.

- **OK**: Functional but flat. Doesn't carry weight. "OK" is what you say about
  mediocre pizza. For a backup tool that just confirmed your irreplaceable data is
  safe, it undersells the achievement. Alternatives:
  - **safe** — direct, answers the user's question ("is my data safe?" → "safe")
  - **held** — mythic resonance (the thread is held), but might confuse
  - **sound** — technical precision (structurally sound), but obscure
  - Keep **OK** — it's universally understood, zero ambiguity

- **aging** — Good. Conveys temporal degradation without alarm. Alternatives:
  - **stale** — more technical, implies something is wrong
  - **fading** — more evocative, but imprecise (data doesn't fade)
  - Keep **aging** — it's the right word

- **gap** — Problematic. "Gap" suggests something is missing, but doesn't convey
  severity. A gap in a fence is different from a gap in your data protection.
  Alternatives:
  - **exposed** — strong, precise (your data is exposed to loss)
  - **bare** — mythic (the thread is bare), but might read as "empty"
  - **open** — simple, conveys vulnerability
  - **unguarded** — directly negates the lowest promise level ("guarded")
  - **at risk** — already the name of the middle state, can't reuse

### Ideas for the safety column

**Idea V-1: Replace OK/aging/gap with safe/aging/exposed**

"Safe" directly answers the user's question. "Aging" stays. "Exposed" conveys
urgency without panic — your data is exposed to potential loss.

```
SAFETY  SUBVOLUME
safe    htpc-home
safe    subvol3-opptak
aging   subvol1-docs
exposed htpc-root
```

**Idea V-2: Use the promise status names directly**

Drop the mapping entirely. Show PROTECTED / AT RISK / UNPROTECTED in the safety
column. More precise, no translation layer, but more verbose.

```
SAFETY       SUBVOLUME
protected    htpc-home
at risk      subvol1-docs
unprotected  htpc-root
```

**Idea V-3: Single-character severity indicators**

For dense table layouts, use symbols instead of words:

```
  ●  htpc-home           (green dot = safe)
  ◐  subvol1-docs        (half dot = aging)
  ○  htpc-root           (empty dot = exposed)
```

Works well in tables but fails the "grep the output" test and the accessibility
test (no-color mode needs text fallback).

---

## Domain 2: Operational Health

### Current vocabulary

| Term | Where | Meaning |
|------|-------|---------|
| healthy | awareness.rs | Everything normal |
| degraded | awareness.rs | Backup works but suboptimally |
| blocked | awareness.rs | Something prevents next backup |

### Analysis

**healthy / degraded / blocked** — This is excellent vocabulary. Each term is
immediately understood, precisely ordered, and used consistently. The graduated
severity is intuitive: healthy → degraded → blocked maps to fine → impaired → stopped.

No change recommended. This vocabulary earns its place.

---

## Domain 3: The Incremental Send Mechanism

### Current vocabulary (presentation layer)

| Term | Where | Meaning |
|------|-------|---------|
| chain | voice.rs, output.rs, cli.rs | The incremental send/receive chain |
| chain health | output.rs | Whether the chain is intact |
| chain broken | awareness.rs, voice.rs | Parent snapshot missing, next send is full |
| incremental | voice.rs, plan output | Send using parent diff |
| full | voice.rs, plan output | Send entire snapshot |
| pin / pin file | chain.rs, awareness.rs | Marker recording last sent snapshot |
| parent | chain.rs, plan.rs | The snapshot used as incremental base |

### Analysis

The grill-me session resolved: **"thread" replaces "chain" in presentation layer.**

- **chain → thread** — The norns spin threads of fate. An incremental send chain IS
  a thread: a continuous line connecting snapshots through time. "Thread broken" is
  instantly understood. "Thread repaired" is natural English.
  
  `notify.rs` already uses this vocabulary: "The thread of {subvolume} has frayed",
  "Every thread in the well has snapped." The CLI should match.

- **pin / pin file** — This is internal terminology that sometimes leaks to the user
  (e.g., "full (no pin)" in status, "pin file write failures" in notifications). The
  user doesn't need to know about pin files. Alternatives for user-facing text:
  - **marker** — less technical, but generic
  - **anchor** — evocative (the thread is anchored to a snapshot), precise
  - Keep **pin** in technical contexts (error messages, verbose output, verify)
  - Use **anchor** or avoid the concept entirely in non-technical contexts

**Idea V-4: Thread vocabulary in presentation**

```
Current:  CHAIN: incremental (20260330-0404-htpc-home)
Proposed: THREAD: incremental (20260330-0404-htpc-home)

Current:  chain broken on WD-18TB — next send will be full
Proposed: thread broken on WD-18TB — next send will be full

Current:  htpc-root chain established — future sends will be incremental
Proposed: htpc-root thread established — future sends will be incremental
```

**Idea V-5: Pin → anchor in user-facing text only**

```
Current:  full (no pin)
Proposed: full (no anchor)

Current:  Pin file write failures
Proposed: Thread anchor could not be written — next send will be full
```

Or simply avoid exposing the mechanism: "full (new thread)" instead of "full (no pin)."

**Idea V-6: Simplify full-send reasons to user intent**

The chain health reasons are currently mechanical: "no pin", "pin missing locally",
"pin missing on drive", "pin error", "no drive data". The user doesn't care *why*
the chain is broken — they care *what happens next*.

```
Current:  full (no pin)
          full (pin missing locally)
          full (pin missing on drive)

Proposed: full send (~31.8 GB)
          (all three reasons produce the same consequence)
```

The *reason* moves to `--verbose` or `urd verify`. The default shows the *consequence*.

---

## Domain 4: Protection Levels (Promise Names)

### Current vocabulary

| Term | Where | Meaning |
|------|-------|---------|
| guarded | types.rs | Local snapshots only |
| protected | types.rs | Local + one external drive current |
| resilient | types.rs | Local + multiple externals, including offsite |
| custom | types.rs | User-managed parameters |

### Analysis

These terms were chosen carefully (ADR-110) and encode a real hierarchy:
guarded < protected < resilient. The question is whether the words carry their
intended weight intuitively.

- **guarded** — Implies active protection, but in Urd it means *least* protected
  (local only). A user might expect "guarded" to be stronger than "protected."
  This is a known issue. Alternatives:
  - **local** — direct, no ambiguity about what it means
  - **watched** — implies monitoring without full protection
  - **cached** — too technical, wrong connotation
  - **sheltered** — implies some protection but not full
  - **mirrored** — wrong; snapshots aren't mirrors

- **protected** — Good. The middle ground. Everyone understands this.

- **resilient** — Excellent. Stronger than protected, implies surviving failures.
  The word carries the right weight for "your data survives a house fire."

- **custom** — Functional. Not evocative, but custom is inherently non-standard
  and shouldn't pretend to be a named level.

**Idea V-7: guarded → local**

"Local" is unambiguous: your data exists in local snapshots only. No external copies.
The hierarchy becomes: local < protected < resilient. Each word precisely describes
what protection exists.

Counterargument: "local" is purely descriptive, not aspirational. "Guarded" at least
implies the system is watching over the data. But if the word causes confusion about
its position in the hierarchy, clarity wins.

**Idea V-8: Keep guarded, improve its introduction**

Rather than renaming, make the hierarchy explicit in the first encounter. The guided
setup wizard (6-H) and progressive disclosure (6-O) are the right places to teach
the vocabulary. The word is fine if the user understands the ordering.

---

## Domain 5: Drive Status

### Current vocabulary

| Term | Where | Meaning |
|------|-------|---------|
| mounted | voice.rs | Drive connected and accessible |
| not mounted | voice.rs | Drive absent |
| away | voice.rs (status table) | Drive unmounted but has send history |
| primary | types.rs | Main backup destination |
| offsite | types.rs | Geographically separate copy |
| test | types.rs | Test/staging drive |

### Analysis

- **mounted / not mounted** — Technically precise (mount is a Linux concept), but
  "mounted" assumes the user understands Linux mount semantics. For a tool that
  aspires to general use, alternatives:
  - **connected / disconnected** — universal, no Linux knowledge needed
  - **available / unavailable** — even more general, but less physical
  - Keep **mounted** — Urd targets BTRFS Linux users, who understand mounts

- **away** — This is beautiful vocabulary. An offsite drive that's "away" perfectly
  communicates its intended state: it's supposed to be elsewhere. "Not mounted" for
  the primary drive means something is wrong. "Away" for the offsite drive means
  everything is working as designed. The semantic distinction matters.

  However, "away" is used for ALL unmounted drives in the status table, not just
  offsite ones. A primary drive that's "away" is confusing — it should be "not mounted"
  or "disconnected" to signal that this is unexpected.

**Idea V-9: "Away" only for offsite drives**

```
SUBVOLUME     WD-18TB     WD-18TB1 (offsite)
htpc-home     2 (5h)      away
htpc-root     1           disconnected    ← primary drive, unexpected absence
```

Offsite drives are "away" (expected). Primary drives are "disconnected" (unexpected,
investigate). The vocabulary encodes the drive's role in the single word used to
describe its absence.

**Idea V-10: Drive status summary in natural language**

```
Current:   Drives: WD-18TB mounted (4.3TB free)
           Drives: WD-18TB1 not mounted

Proposed:  WD-18TB connected (4.3 TB free)
           WD-18TB1 away — last seen 8 days ago
```

Drop the "Drives:" prefix (the context is obvious). Add temporal context to absent
drives. "Last seen" is natural language that carries history.

---

## Domain 6: Operations and Actions

### Current vocabulary

| Term | Where | Meaning |
|------|-------|---------|
| [CREATE] | voice.rs | Create local snapshot |
| [SEND] | voice.rs | Send snapshot to external drive |
| [DELETE] | voice.rs | Delete snapshot (retention) |
| [SKIP] | voice.rs | Operation skipped |
| [SPACE] | voice.rs | Skipped due to space |
| backup | cli.rs | Execute the full backup cycle |
| plan | cli.rs | Preview planned operations |
| snapshot | throughout | Point-in-time copy of a subvolume |
| retention | throughout | Policy for which snapshots to keep/delete |

### Analysis

- **[CREATE] / [SEND] / [DELETE]** — Clear action tags. Color-coded (green/blue/yellow).
  Consistent. No change needed.

- **[SKIP]** — Overloaded. `OpResult::Skipped` has four distinct semantics (noted in
  status.md known issues). The user sees [SKIP] and doesn't know if it means "not due
  yet" (fine), "drive not mounted" (expected), "no space" (problem), or "disabled"
  (intentional). The [SPACE] tag helps but only covers one case.

**Idea V-11: Differentiated skip tags**

```
[WAIT]    — interval not elapsed (expected, will happen later)
[AWAY]    — drive not mounted (expected for offsite)
[SPACE]   — insufficient space (needs attention)
[OFF]     — disabled in config (intentional)
[SKIP]    — catch-all for other reasons
```

Each tag communicates the *nature* of the skip, not just the fact. The user
immediately knows whether to act or ignore.

- **snapshot** — Universally understood in backup contexts. Keep.

- **retention** — Technical but standard in backup tools. The question is whether
  the user needs to encounter this word at all. Alternatives:
  - **cleanup** — what it does, not what it's called
  - **thinning** — implies keeping some, removing others (accurate)
  - Keep **retention** in config and verbose output
  - Use **cleanup** in casual output: "3 old snapshots cleaned up"

**Idea V-12: Retention → cleanup in casual output**

```
Current:  [DELETE]  20260310-htpc-home (monthly thinning)
Proposed: [DELETE]  20260310-htpc-home (monthly cleanup)

Current:  12 snapshots deleted (retention)
Proposed: 12 old snapshots cleaned up
```

"Retention" stays in config (`local_retention`, `external_retention`) and technical
docs. "Cleanup" in user-facing output. The user thinks "old snapshots were cleaned
up," not "the retention policy was executed."

---

## Domain 7: Error and Warning Language

### Current vocabulary

Error messages in `error.rs` follow a structured pattern: summary, cause, remediation.
This is excellent Norman-grade error design. The vocabulary audit focuses on the
summary layer.

| Summary | Source error |
|---------|-------------|
| "Destination drive is full" | No space on receive side |
| "Local filesystem is full" | No space for snapshot |
| "Insufficient permissions" | sudo/permission denied |
| "Drive is read-only (possible hardware failure)" | Read-only mount |
| "Snapshot not found at expected path" | Stale target |
| "Destination directory missing" | Snapshot root dir missing |
| "Incremental parent missing (chain broken)" | Parent not on drive |

### Analysis

These summaries are precise and actionable. The parenthetical in the last one should
update: "(thread broken)" per the vocabulary change.

**Idea V-13: Thread vocabulary in error messages**

```
Current:  "Incremental parent missing (chain broken)"
Proposed: "Incremental parent missing (thread broken)"
```

One simple substitution. The remediation text in the error already explains what to do.

---

## Domain 8: Notification Language

### Current vocabulary in notify.rs

Notifications already use mythic language:

```
"The thread of {subvolume} has frayed"
"The thread of {subvolume} is rewoven"
"Every thread in the well has snapped"
"The loom has seized — every weaving failed"
"{N} of {M} threads could not be woven"
```

### Analysis

This is the most voiced surface in Urd today. The vocabulary is strong but has some
inconsistencies with the rest of the system:

- **"The well"** — Refers to the Well of Urd (Urðarbrunnr). Beautiful reference but
  it only appears once, in the promise degradation notification. If "the well" is part
  of Urd's vocabulary, it should appear consistently or not at all.

- **"The loom has seized"** — Mixing metaphors. The norns work at a well, spinning
  threads of fate. A loom is a different tool. Weavers use looms; spinners use spindles
  (hence the tray icon name). The metaphor should be consistent: threads are spun and
  held, not woven on a loom.

- **"threads could not be woven"** — Same issue. Threads aren't woven; they're spun.
  A backup that fails is a thread that breaks or frays, not one that couldn't be woven.

**Idea V-14: Consistent spinning/thread metaphor in notifications**

The norns spin threads at the Well of Urd. The metaphor family:

| Concept | Metaphor | Technical meaning |
|---------|----------|-------------------|
| Backup succeeds | Thread holds / spins true | All operations completed |
| Backup degrades | Thread frays | Promise state worsened |
| Backup recovers | Thread is mended | Promise state improved |
| Backup fails | Thread snaps / breaks | Operations failed |
| All backups fail | The spindle stops | Complete backup failure |
| Chain intact | Thread unbroken | Incremental send possible |
| Chain broken | Thread severed | Full send required |

Remove: "loom", "weaving", "woven" — wrong tool for the mythology.
Keep: "thread", "fray", "well", "spin", "spindle".
Add: "mend" (for recovery), "sever" (for chain breaks), "hold" (for success).

**Idea V-15: Notification language that layers mythic + technical**

```
Current:
  "The thread of htpc-home has frayed — it was PROTECTED, now AT RISK.
   The well remembers, but the weave grows thin."

Proposed:
  "htpc-home: PROTECTED → AT RISK.
   The thread frays — offsite drive WD-18TB1 last seen 8 days ago."
```

Technical fact first (what changed). Mythic framing second (what it means). The
technical line is greppable, scriptable, unambiguous. The mythic line carries
emotional weight and provides context.

---

## Domain 9: Time and Duration

### Current vocabulary

| Format | Where | Example |
|--------|-------|---------|
| `{n}d` | voice.rs | "1d" (in table cells) |
| `{n}h` | voice.rs | "5h" (in table cells) |
| `{n}m` | voice.rs | "14m" (in table cells) |
| `{n} days ago` | various | "8 days ago" (in advisories) |
| ISO timestamps | various | "2026-03-31T04:00:34" |

### Analysis

Time vocabulary is consistent and well-formatted. The compact form (`1d`, `5h`) works
perfectly in tables. The natural form (`8 days ago`) works in advisory text.

One opportunity:

**Idea V-16: "Last seen" for drives, "freshness" for snapshots**

```
WD-18TB1 away — last seen 8 days ago
htpc-home: 12 snapshots, newest 1 day old
```

"Last seen" is natural for drives (they're physical objects that come and go).
"Freshness" framing for snapshots communicates how current the data is.

---

## Domain 10: CLI Command Descriptions

### Current vocabulary

| Command | Description |
|---------|-------------|
| plan | "Show planned backup operations without executing" |
| backup | "Create snapshots, send to external drives, run retention" |
| status | "Show snapshot counts, drive status, chain health" |
| history | "Show backup history" |
| verify | "Verify incremental chain integrity and pin file health" |
| init | "Initialize state database and validate system readiness" |
| calibrate | "Measure snapshot sizes for space estimation" |
| get | "Retrieve a file from a past snapshot" |

### Analysis

These are functional descriptions. They tell you what the command does, not why
you'd use it. Norman's principle: the signifier should communicate intent.

**Idea V-17: Intent-first command descriptions**

```
plan      Preview what Urd will do next
backup    Protect your data now
status    Is my data safe?
history   What has Urd done?
verify    Are the threads intact?
init      Set up Urd for the first time
calibrate Measure how much space sends will need
get       Recover a file from the past
```

The descriptions answer the *question* the user has when they reach for the command.
"Is my data safe?" is more useful than "Show snapshot counts, drive status, chain
health" because it tells you when to use the command, not what it outputs.

Note: these should still be precise enough that the user knows what the command
*does*. "Protect your data now" might be too vague for `backup`. A hybrid:

```
backup    Run the backup cycle — snapshots, sends, cleanup
status    Check data safety — promises, drives, threads
verify    Deep-check thread integrity and drive health
```

**Idea V-18: Command names — evaluate each**

The current command names are well-chosen. But two deserve scrutiny:

- **get** — Generic. Every CLI tool has a `get`. In Urd's context, "get" means
  "retrieve a file from a past snapshot." Alternatives:
  - **restore** — more specific to backup context
  - **recover** — implies something was lost
  - **fetch** — git-adjacent, might confuse
  - Keep **get** — it's simple and the `--at` flag makes the context clear

- **calibrate** — Precise but unusual for backup tools. The user might not think
  "I need to calibrate" before their first send. Alternatives:
  - **measure** — what it does
  - **estimate** — what the result is for
  - Keep **calibrate** — it's distinctive and the help text explains when to use it

---

## Domain 11: The Summary Line

### Current vocabulary

```
"All data safe."
"{N} of {M} safe. {names} {needs/need} attention."
" {N} blocked, {M} degraded — {reason}."
```

### Analysis

The summary line is the single most important piece of text Urd produces. It's the
one-sentence answer to "is my data safe?"

**"All data safe."** — Direct, unambiguous. Could be more authoritative:
  - "All data safe." (current — factual, flat)
  - "All safe." (shorter — the subject is obvious from context)
  - "All threads hold." (mythic — but might confuse on first encounter)

**"{N} of {M} safe"** — Clear. The fraction communicates both the problem (something
isn't safe) and the scale (how much).

**"{names} needs attention"** — Good. "Needs attention" is non-technical,
non-alarming, but communicates that action is required.

**Idea V-19: Graduated summary line**

```
All safe.                          (all protected, terse confidence)
All safe. 9 threads hold.          (verbose mode adds detail)
8 of 9 safe. htpc-root exposed.   (problem named, severity visible)
3 of 9 safe. Data at risk.        (majority unsafe, urgency escalates)
None safe. Immediate action needed. (everything broken, maximum urgency)
```

The tone escalates with severity. All-safe is terse (the norn speaks least when all
is well). Partial failure names the problem. Total failure demands action.

---

## Domain 12: Existing Mythic Vocabulary Audit

Urd already uses mythic language in `notify.rs`. Full inventory:

| Term | Where | Mythologically accurate? |
|------|-------|-------------------------|
| thread | notify.rs | Yes — norns spin threads of fate |
| fray | notify.rs | Yes — thread degradation |
| rewoven | notify.rs | Mixed — weaving is a loom operation, not spinning |
| well | notify.rs | Yes — Well of Urd (Urðarbrunnr) |
| weave | notify.rs | No — norns spin at the well, they don't weave at a loom |
| loom | notify.rs | No — wrong tool entirely |
| woven | notify.rs | No — see above |
| spindle | tray icon name | Yes — the tool used for spinning thread |

### Proposed consistent vocabulary

**Keep (mythologically sound):**
- **thread** — the continuous line of fate / incremental chain
- **fray** — degradation of a thread / promise worsening
- **the well** — the Well of Urd / the system's source of truth
- **spindle** — the spinning tool / the tray icon
- **spin** — creating thread / successful backup operation
- **hold** — thread holding / promise maintained
- **snap/break/sever** — thread broken / chain broken, backup failed

**Remove (mythologically wrong):**
- **loom** — norns don't use looms
- **weave/woven/weaving** — norns spin, they don't weave
- **rewoven** — replace with "mended" or "restored"

**Candidates to add:**
- **mend** — thread repaired / promise recovered
- **anchor** — the point a thread is tied to / pin file
- **sever** — thread cut / chain broken beyond repair
- **tend** — what the norns do at the well / what Urd does to your data

---

## Uncomfortable Ideas

**Idea V-20: Rename subvolumes in user-facing output**

BTRFS "subvolumes" are a technical concept. The user thinks in terms of "my home
directory" or "my documents." Could the status table show user-friendly names?

```
Current:   subvol1-docs
Proposed:  Documents (subvol1-docs)
```

This would require a `display_name` field in config. High effort for cosmetic gain.
But it shifts the conceptual model from "BTRFS subvolumes" to "the things I care
about protecting." Users already set `short_name` — this would be an even shorter
display name.

**Idea V-21: Remove all technical jargon from default output**

The most aggressive version: default `urd status` shows no BTRFS terminology at all.
No "subvolume", no "snapshot", no "send/receive". Just:

```
All safe.

  Home          protected   backed up 1 day ago   3 copies
  Documents     protected   backed up 1 day ago   3 copies
  Photos        resilient   backed up 1 day ago   4 copies
  htpc-root     exposed     needs backup           1 copy

  WD-18TB connected (4.3 TB free)
  WD-18TB1 away (8 days)

Last backup: yesterday at 04:00 (success)
```

"Snapshot" → "backed up." "Send" → "copy." "Subvolume" → display name. "Chain" →
invisible (only surfaced when broken, as "needs full backup").

This is the "explain it to my parents" version. It might be too far — Urd's users
are Linux/BTRFS users who understand the terminology. But it's worth naming as a
design direction.

**Idea V-22: Voice modes — "technical" and "conversational"**

A config option or flag that switches between vocabulary registers:

```toml
[general]
voice = "technical"   # or "conversational"
```

Technical mode: current vocabulary (snapshot, chain, retention, subvolume).
Conversational mode: simplified vocabulary (backup, thread, cleanup, [display name]).

This satisfies both audiences without forcing a choice. But it doubles the voice
maintenance surface. Every new feature needs two phrasings.

---

## Cross-Domain Consistency Issues Found

### Issue 1: "chain" vs "thread" inconsistency

`notify.rs` says "thread." `voice.rs` says "chain." `cli.rs` says "chain."
The grill-me resolved this: "thread" in all presentation. But the audit reveals the
scope: CHAIN column header, chain health display, verify command description, error
messages — all need updating in voice.rs.

### Issue 2: "safe" used ambiguously

The summary line says "All data safe" but the safety column says "OK". These are
different words for the same concept. Pick one and use it everywhere.

### Issue 3: Skip reason verbosity mismatch

Some skip reasons are terse ("disabled") and others are verbose ("send to WD-18TB
skipped: calibrated size ~1.3TB exceeds available 800GB"). The terse ones don't help;
the verbose ones are too long for a table. Need a consistent policy: one-line summary
in default output, full explanation in `--verbose`.

### Issue 4: Notification vs CLI vocabulary drift

Notifications use mythic voice ("The thread has frayed"). CLI uses technical voice
("chain broken"). These should converge — not to identical text, but to the same
underlying vocabulary. The user should hear "thread" in both places, not "thread" in
one and "chain" in the other.

### Issue 5: "send" is invisible to non-technical users

"Send" in BTRFS means `btrfs send | btrfs receive` — a pipeline that streams a
snapshot to another location. To the user, it means "copy my backup to the external
drive." The word "send" is accurate but doesn't communicate value. Alternatives:
- **copy** — what the user thinks is happening
- **transfer** — slightly more technical but clear
- **sync** — implies bidirectional, wrong
- Keep **send** in technical contexts, use **copy** or **back up to** in casual output

---

## Grill-Me Results (2026-03-31)

20 vocabulary domains evaluated one at a time. Every term tested against clarity,
precision, consistency, and mythic resonance. Full decisions saved to memory
(`project_vocabulary_decisions.md`).

### Complete Resolved Vocabulary

**Exposure triad** (column header: EXPOSURE, replacing SAFETY):

| State | Replaces | Concept |
|-------|----------|---------|
| **sealed** | OK | Promise honored. Data's fate is sealed. |
| **waning** | aging | Protection degrading over time. The seal wanes. |
| **exposed** | gap | No valid backup. Data is exposed to loss. |

Narrative arc: sealed → waning → exposed. Each word follows naturally from the last.

**Protection levels** (column header: PROTECTION, replacing PROMISE):

| Level | Replaces | Concept |
|-------|----------|---------|
| **recorded** | guarded | Local snapshots only. Urd noted your data. |
| **sheltered** | protected | Local + external drive. Data moved to safety. |
| **fortified** | resilient | Local + multiple externals + offsite. The 3-2-1. |
| **custom** | custom | Kept as opt-out. |

Narrative arc: record → shelter → fortify. Each is a verb the user did. Progression
nudges toward fortified. "Fortified" chosen for Viking cultural resonance and the
engineering/building connotation.

Config field becomes `protection = "fortified"`. The "promise" concept moves to
voice.rs only. Full restructuring deferred to **ADR-110 rework**, which should address:
enum rename, config field rename, whether named levels are the right abstraction vs.
explicit parameter bundles, and migration path from current config.

**Thread vocabulary** (column header: THREAD, replacing CHAIN):

| State | Display |
|-------|---------|
| **unbroken** | "unbroken" (terse when fine) |
| **broken** | "broken — full send (~31.8 GB)" (consequence when broken) |
| **—** | No external sends configured |

Asymmetric display: problems get more words than healthy state.

**Drive status:**

| State | When | Replaces |
|-------|------|----------|
| **connected** | Drive accessible | "mounted" |
| **disconnected** | Primary/test drive absent | "not mounted" |
| **away** | Offsite drive absent (expected) | "not mounted" / "away" for all |

Drop "Drives:" prefix. Add "last seen N days ago" for absent drives.

**Skip tags** (differentiated, replacing generic [SKIP]):

| Tag | Meaning | User action? |
|-----|---------|-------------|
| **[WAIT]** | Interval not elapsed | No — will happen later |
| **[AWAY]** | Drive not connected | Depends on role |
| **[SPACE]** | Insufficient space | Yes — needs attention |
| **[OFF]** | Disabled in config | No — intentional |
| **[SKIP]** | Catch-all | Read the reason |

**Operational health:** healthy / degraded / blocked — **kept unchanged**. Excellent
vocabulary that earned its place.

**Operation tags:** [CREATE] / [SEND] / [DELETE] — **kept unchanged**.

**Other kept terms:** send/sent, snapshot, SUBVOLUME header, "backup" as verb/noun,
retention (config) / cleanup (casual) / thinning (specific reason).

**Summary line:**
- All clear: `"All sealed."`
- Problem: `"{N} of {M} sealed. {names} exposed."`

**Last backup line:** Humanized dates ("today at 04:00"). Drop run number. Keep
result and duration.

**CLI descriptions:** Intent-first style.

| Command | Description |
|---------|-------------|
| plan | "Preview what Urd will do next" |
| backup | "Run the backup cycle — snapshots, sends, cleanup" |
| status | "Check data safety — exposure, drives, threads" |
| history | "Show past backup runs" |
| verify | "Deep-check thread integrity and drive health" |
| init | "Set up Urd for the first time" |
| calibrate | "Measure snapshot sizes for send estimation" |
| get | "Recover a file from a past snapshot" |

**Notification pattern:**
State change in Urd vocabulary + technical detail + actionable command.
Remove loom/weave/woven. Keep spindle/thread/hold/fray/break. Add mend.
"The spindle has stopped" for total failure.

Example:
```
htpc-home: sealed → waning.
Offsite drive WD-18TB1 last seen 8 days ago. Connect and run `urd backup`.
```

**Mythic voice principle:** Voice on events, data on queries. "Backup" kept as the
overall verb — the voice adds character through framing, not by renaming established
terms.

### Key Design Principles Confirmed

1. **Urd's voice is mythic because it is precise, not despite it.**
2. **Technical descriptions are the default and fallback.**
3. **Every term evaluated against: clarity, precision, consistency, resonance.**
4. **Presentation-layer changes only** — data structures retain internal naming.
5. **ADR-110 rework needed** for protection level restructuring (enum rename, config
   field rename, promise → protection in data layer).

### Handoff to Design

The vocabulary is resolved. The next artifact is a **unified voice design doc** that
implements these decisions across voice.rs, cli.rs, and notify.rs. The design doc
should cover:

1. **Vocabulary mapping table** — old term → new term, which files change
2. **Graduated language rules** — staleness escalation thresholds, next-action patterns
3. **Notification templates** — rewritten with correct mythology and technical detail
4. **Status table mockup** — the new output with all vocabulary applied
5. **ADR-110 addendum** — scope of the protection level restructuring
6. **Migration** — how existing users experience the vocabulary change (config field
   rename needs `urd migrate` support)
