# Architectural Adversary Review: Can status.md's Vision Be Realized?

> **TL;DR:** The existing planner/executor architecture is genuinely strong and can support
> protection promises and heartbeat without redesign. But the roadmap buries three hard
> problems under optimistic labels: (1) the promise-to-retention mapping is a policy design
> problem disguised as a coding task, (2) the Sentinel is two distinct systems — a state
> machine and a process supervisor — conflated into one priority, and (3) the mythic voice
> requires a content design discipline that no amount of Rust code can substitute for. The
> architecture won't block the vision. Unclear thinking about what the features actually
> *are* will.

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Architectural review of `docs/96-project-supervisor/status.md` priorities
against current codebase at commit `b66be6f`, focused on what must be true architecturally
for the vision to succeed.
**Reviewer:** Claude (arch-adversary)

---

## What Kills You

**Catastrophic failure mode for this review:** The roadmap creates a false sense of
architectural readiness. The team spends months building features that don't compose,
or discovers mid-implementation that a "Medium effort" item actually requires rethinking
a core abstraction. The architectural equivalent of "it compiled, ship it."

**Distance:** Moderate. The planner/executor separation is genuinely solid and won't
need redesign. But several roadmap items assume clean boundaries that don't yet exist
in the design — particularly around what a "promise" actually *means* at the system level
and how the Sentinel relates to the existing systemd timer.

---

## Scorecard: Architectural Readiness by Priority

| Priority | Readiness | Key Constraint |
|----------|-----------|----------------|
| P1: Operational Cutover | 5 — Ready | No code changes. Just do it. |
| P2: Safety & Foundation | 4 — Solid | UUID needs config migration story. Structured errors need a pattern catalog. |
| P3: Protection Promises | 3 — Needs design | Promise-to-retention mapping is a policy problem, not a code problem. |
| P3b: Heartbeat | 5 — Ready | Trivial. Reuse metrics pattern. |
| P3c: Promise State Model | 3 — Needs design | "Continuously computed" has performance and consistency implications. |
| P4: Sentinel | 2 — Needs decomposition | Two distinct systems (state machine + process supervisor) conflated. |
| P5: Core Expansion | 3 — Needs design | `find`/`get` performance on large snapshot trees is an unsolved problem. |
| P6: Mythic Experience | 2 — Needs content design | Architecture can't help here. This is a writing problem. |

---

## Design Tensions

### 1. Promises Are a Policy Design Problem, Not a Config Extension

The roadmap says: "Add `protection_level` field to config. Planner derives intervals
and retention from promise level." This frames promises as a config refactor. It isn't.

The hard question is: **what does "protected" mean, quantitatively?**

```toml
protection_level = "protected"
```

What operations does this produce? The brainstorm suggests:

| Level | Snapshot Interval | Local Retention | External Freshness | Min Copies |
|-------|-------------------|-----------------|-------------------|------------|
| guarded | 1h | 7d | — | 0 |
| protected | 1h | 30d | 48h | 1 |
| resilient | 15m | 90d | 24h | 2 |
| archival | 1d | 1y | 7d | 1 |

But these numbers are *policy decisions* that depend on:
- How much local disk space is available (15-minute snapshots of a 3TB subvolume eat
  space fast)
- How often external drives are connected (48h freshness is meaningless if the drive
  visits monthly)
- What the user's actual RPO is (a photographer with today's unsaved edits has a
  different need than someone with a music collection)

**The risk:** If the promise-to-retention mapping is wrong, the user gets a false sense
of security. They set "protected" and trust it, but the derived retention doesn't match
their actual connection pattern. This is worse than manual config — at least with manual
config the user *knows* they're responsible for the numbers.

**Architectural requirement:** The promise model needs a **validation layer** that checks
whether the derived policy is achievable given the user's actual drive connection history.
"You set 'protected' with max_age 48h, but your offsite drive has only been connected
3 times in the last 30 days. This promise cannot be kept." This is the attention budget
at work: surface the problem before it becomes silent data loss.

**Concrete criteria:**
- [ ] Promise levels have documented, specific retention/interval derivations
- [ ] Validation compares derived policy against historical drive connection frequency
- [ ] Status shows whether each promise is achievable, not just whether it's currently met
- [ ] `custom` level is not a second-class citizen — power users must not feel punished

### 2. The Sentinel Is Two Systems, Not One

Status.md describes the Sentinel as: "An event-driven state machine holding the awareness
model. Reacts to events (drive plug, timer, backup result), updates promise states, drives
notifications."

This is actually two distinct systems with different failure modes:

**System A: The Awareness Model** — a pure function that computes promise states from
filesystem state + history + config. Input: current snapshots, last send times, drive
connection frequency. Output: PROTECTED / AT RISK / UNPROTECTED per subvolume. This is
a *query*, not a daemon. It can run as part of `urd status`, as part of the backup
post-run summary, or as a standalone check.

**System B: The Event Reactor** — a long-running daemon that watches for udev events,
manages timers, decides when to trigger backups, and sends notifications. This is a
*process* with lifecycle concerns: startup, shutdown, crash recovery, lock contention
with manual `urd backup` runs, systemd integration.

The roadmap conflates these because the brainstorm document described them as one thing.
But building them as one thing creates two problems:

1. **Testing.** The awareness model is pure and testable. The event reactor is I/O-heavy
   and requires integration tests with udev, timers, and process management. Coupling
   them means the testable part gets dragged into the untestable part.

2. **Deployment flexibility.** The "three deployment levels" (none, passive, active) are
   really about whether System B exists. System A should *always* be available — it's
   just a function that computes promise states. A user who doesn't want the Sentinel
   daemon should still see promise states in `urd status`.

**Architectural requirement:** Separate the awareness model from the event reactor.

- [ ] `awareness.rs` — pure function: `(config, filesystem_state, history) -> Vec<PromiseState>`
- [ ] Available to all commands (`status`, `backup` summary, `verify`, heartbeat)
- [ ] No dependency on Sentinel being running
- [ ] Sentinel *uses* the awareness model but doesn't *own* it
- [ ] Sentinel can be killed without affecting promise state computation

### 3. "Continuously Computed" Promise States Have a Consistency Problem

The roadmap says promise states are "continuously computed." But computed from what?

- **Local snapshots:** Enumerated by reading a directory. Fast, consistent.
- **External snapshots:** Only visible when the drive is mounted. When the drive is
  disconnected, the last-known state is stale. How stale is acceptable?
- **Last send time:** In SQLite. Accurate, but only updated on backup runs.
- **Drive connection frequency:** Not currently tracked. Would need a new table or
  heartbeat history.

The consistency problem: when a drive is unplugged, the promise state should transition
from PROTECTED to AT RISK after some threshold. But who computes this transition?

- If computed on-demand (in `urd status`), the state is fresh but transient — it's not
  recorded anywhere.
- If computed by the Sentinel, it's recorded but requires the daemon to be running.
- If computed after each backup run, it's stale between runs (potentially 24 hours).

**Architectural requirement:** Define the *source of truth* for promise state.

- [ ] Promise state is computed on demand by the awareness model (pure function)
- [ ] Heartbeat file is the *cache*, not the *source* — it's written after computation
- [ ] Drive connection history is tracked (new `drive_connections` table in SQLite or
  timestamps in heartbeat history)
- [ ] Promise state computation has explicit staleness rules: "external snapshot freshness
  based on last known state; drive connection freshness based on mount check + history"

### 4. The Mythic Voice Is a Content Design Problem

The roadmap lists "mythic voice" features across Priorities 6a-6e. The architectural
support for this is trivial — it's string formatting. The hard part is **content design**:

- What does Urd actually *say*? "Your recordings are woven into the well" sounds evocative
  in a brainstorm document. Does it sound right at 3am when the user needs to know if their
  thesis backup succeeded? The voice must be specific enough to communicate real information
  while maintaining the mythic register.

- Where is the line between evocative and obscure? "The threads of your fate are fraying"
  for AT RISK is atmospheric. But does the user immediately understand that their external
  drive hasn't been connected in 12 days? The technical detail must coexist with the mythic
  framing.

- Consistency is harder than creation. Writing one mythic status message is fun. Writing
  mythic versions of every error message, every config validation warning, every restoration
  prompt, every notification — and keeping them consistent in tone, helpful in content, and
  non-repetitive across daily use — is a sustained content design effort.

**Architectural requirement:** The voice is a *layer*, not embedded in logic.

- [ ] All user-facing text goes through a voice/presentation module (`voice.rs` or similar)
- [ ] The module receives structured data (promise state, error type, operation result) and
  renders it into text — mythic for interactive, terse for daemon
- [ ] Voice strings are testable: given this state, assert this output contains these facts
- [ ] Technical details are always accessible (e.g., `--verbose` or expandable sections)
- [ ] The voice module is the *only* place that formats user-facing text — commands don't
  hardcode strings

### 5. `urd find` / `urd get` Performance Is an Unsolved Problem

The roadmap says `urd find thesis.md` searches across all snapshots. With 47 local
snapshots of htpc-home (77 GB each), that's potentially scanning 47 directory trees for
a filename. Even with short-circuit optimizations, this is slow.

BTRFS doesn't index filenames across snapshots. Each snapshot is an independent directory
tree. There is no "find this file across all snapshots" primitive in BTRFS. The options:

1. **Walk each snapshot directory tree.** O(snapshots × files). Unacceptably slow for
   large subvolumes.
2. **Use `find` subprocess per snapshot.** Same complexity, slightly faster I/O due to
   kernel-level readdir optimization.
3. **Build a cross-snapshot index.** Fast queries but requires building and maintaining
   an index. The index itself is a storage and consistency concern.
4. **Diff-based approach.** Use `btrfs subvolume find-new` or `btrfs send --no-data` to
   identify which snapshots changed a given path. Much faster for "which snapshots touched
   this file" but can't answer "does this file exist in snapshot X."
5. **Restrict to known paths.** `urd get ~/documents/thesis.md@yesterday` doesn't need
   to search — it constructs the path directly:
   `<snapshot_root>/htpc-home/20260322-0200-htpc-home/documents/thesis.md`. O(1).

**Architectural requirement:** Separate the two use cases.

- [ ] **`urd get file@date`** — direct path construction, O(1). The common case. No search
  needed. Check existence, copy file. This should be built first and can work today.
- [ ] **`urd find pattern`** — cross-snapshot search. Hard problem. Defer until there's a
  clear solution for performance. Don't ship a slow `find` that teaches users not to use it.
- [ ] If `find` is built, it needs a progress indicator and a timeout. A command that hangs
  for 5 minutes with no output violates "clear when interactive."

### 6. The Heartbeat Is Easy — the Contract Around It Is Not

Writing a JSON file after each run is trivial. The hard part is defining the contract:

- **Who reads it?** Shell prompts, tray icons, monitoring scripts, the Sentinel. Each has
  different freshness requirements and different tolerance for schema changes.
- **Schema stability.** The moment external tools depend on `heartbeat.json`, its schema
  becomes a public API. Adding a field is fine. Renaming or removing one breaks downstream.
- **Freshness semantics.** If the heartbeat was written 6 hours ago and says "PROTECTED,"
  is that still true? The heartbeat is a cache, not a live query. External consumers need
  to know this — the timestamp is critical, and there should be a documented staleness model.
- **Atomic writes.** A half-written JSON file will break every consumer. Must use the
  temp-file-then-rename pattern (already used in `state.rs` for SQLite).

**Architectural requirement:**

- [ ] Heartbeat schema is versioned (`schema_version` field) from day one
- [ ] Written atomically (temp file + rename)
- [ ] Includes computation timestamp and explicit staleness advisory
  (e.g., `"stale_after": "2026-03-23T04:00:00Z"`)
- [ ] Schema documented in a reference doc (not just in code)
- [ ] First iteration is minimal — add fields later rather than guessing what consumers need

---

## Findings

### Critical: Phase 5a Needs a Design Document Before Code

**Severity: Critical** (wrong abstractions here cascade through everything downstream)

Protection promises touch config, planner, state, status, heartbeat, and eventually the
Sentinel. Getting the abstraction wrong means reworking all of those. The roadmap jumps
from "brainstorm scored 10/10" to "build it." There's a missing step: a design document
that answers:

1. What are the exact retention/interval derivations for each promise level?
2. How does promise validation work when the derived policy is unachievable?
3. How do promises interact with manual overrides (the `custom` level)?
4. What's the migration path for existing configs? Auto-assign promise levels, or leave
   as implicit `custom`?
5. How does the planner route between promise-based and operation-based logic?

This design document should be written, reviewed, and agreed upon before writing code.
An ADR is appropriate here — this is the biggest architectural decision since the
planner/executor separation.

### Significant: Sentinel Must Be Decomposed Before Design

**Severity: Significant**

The Sentinel as described in status.md is at least three concerns:

1. **Awareness model** — computing promise states (pure function, no daemon)
2. **Event reactor** — watching udev events, managing timers (daemon, I/O-heavy)
3. **Notification dispatcher** — routing alerts to desktop/webhook (I/O, depends on #1)

Building these as one monolithic `sentinel.rs` will produce a module that's hard to test,
hard to deploy incrementally, and hard to reason about.

**Architectural criteria:**
- [ ] Awareness model is a standalone module usable without the Sentinel daemon
- [ ] Event reactor depends on awareness model, not the other way around
- [ ] Notification dispatcher depends on awareness model, not on event reactor
- [ ] Each can be tested independently with mocked inputs
- [ ] Passive mode = awareness model + notification dispatcher (no event reactor)
- [ ] Active mode = all three

### Significant: Config Migration Path Needs Explicit Design

**Severity: Significant**

Adding `protection_level` to config seems backward-compatible (it's optional). But the
interaction with existing fields is subtle:

```toml
# What happens here? Promise says 48h freshness, but send_interval says 7d.
protection_level = "protected"
send_interval = "7d"
```

Options:
- **Promise wins:** Override operation-level settings. Breaks user expectations if they
  set send_interval deliberately.
- **Operation wins:** Promise is aspirational, not enforced. Breaks the promise contract.
- **Conflict is an error:** Reject configs where both are set. Annoying but safe.
- **Custom only:** Operation-level settings only valid with `protection_level = "custom"`.
  Clean but requires migration.

This needs a decision *before* implementation. An ADR.

### Moderate: Voice Module Architecture Matters More Than Content

**Severity: Moderate**

The risk isn't "bad mythic text" — that can be iterated. The risk is that user-facing
strings are scattered across command handlers, making the voice inconsistent and hard to
maintain. Today, `commands/status.rs` hardcodes output formatting. `commands/backup.rs`
hardcodes progress and summary text. Each command is its own island.

If the mythic voice is applied by editing strings in 7 different command files, consistency
will degrade with every new feature. The voice needs to be a centralized module that
commands call with structured data.

**Architectural criteria:**
- [ ] Commands produce structured output data (types, not strings)
- [ ] A presentation/voice layer renders structured data into text
- [ ] Two renderers: interactive (mythic voice, color) and daemon (JSON/terse)
- [ ] Renderer selection based on TTY detection (already partially implemented)
- [ ] All user-facing text is testable: "given this state, output contains these keywords"

### Moderate: `urd get` Direct Path Construction Should Be Phase 5 Not Phase 6

**Severity: Moderate** (sequencing, not architecture)

`urd get ~/documents/thesis.md@yesterday` via direct path construction is:
- O(1) performance (no search needed)
- ~100 lines of code (path parsing, snapshot lookup by date, file copy)
- Zero new dependencies
- Immediately useful
- Does not require solving the hard `find` problem

The roadmap places both `find` and `get` in Priority 5 together. But `get`-by-direct-path
is dramatically simpler than `find`-by-search and should be separated. Ship `get` early
(it's the "I deleted a file and want it back" use case — the most common restore need).
Defer `find` until there's a performance solution.

### Commendation: Planner/Executor Separation Pays Dividends

The decision to make the planner a pure function is paying off exactly as intended. Every
proposed feature — promises, Sentinel, heartbeat — can be built *on top of* the planner
without modifying it. The planner's inputs (config, filesystem state) can be extended.
Its outputs (planned operations) can be generated from new logic. But its core contract
(pure function, no side effects) remains untouched.

This is the rare architectural decision that genuinely enables the project to evolve
without rework. It was the right call, and the vision roadmap validates it.

### Commendation: Heartbeat as Universal Interface Bridge

The heartbeat concept is architecturally clean because it decouples Urd from its consumers.
Shell prompts, tray icons, monitoring scripts, and the Sentinel all read the same file.
Urd doesn't need to know about any of them. This is the right pattern — publish state to
a well-known location and let consumers self-serve.

The only risk is schema stability, addressed above. With versioning and atomic writes,
this is a sound design that enables the tray icon, the prompt integration, and future
integrations that haven't been imagined yet.

---

## The Simplicity Question

The roadmap has 7 priorities containing ~20 features. That's ambitious. The simplicity
check: what's the *minimum* set of changes that delivers the vision's core value?

**The minimum viable vision:**

1. **Heartbeat file** after every backup run (Priority 3b, trivial)
2. **Awareness model** as a pure function computing promise states (part of 3c)
3. **`urd status` speaks promises** — opens with confidence statement (part of 6c)
4. **`urd get file@date`** — direct path construction restore (part of 5a)

These four changes, totaling maybe 500 lines of code, would deliver:
- "Is my data safe?" answered in human terms
- A file external tools can read for Urd's state
- The ability to restore a file in one command
- The foundation for everything else

Everything else — Sentinel, mythic voice, smart defaults, setup wizard, drive replacement —
builds on this foundation. The question is whether to build the foundation first and
iterate, or to try to build the full vision at once.

The answer should be obvious, but roadmaps have a way of making "build everything" look
like a plan.

---

## Architectural Criteria Checklist

These must be met for the vision to succeed. Use this as a pre-implementation gate for
each phase.

### Before Building Protection Promises

- [ ] ADR written: promise-to-retention mapping with exact numbers
- [ ] ADR written: config conflict resolution (promise + manual = ?)
- [ ] Migration path documented for existing configs
- [ ] Promise validation designed: "this promise is unachievable given your drive pattern"
- [ ] `custom` level designed as first-class, not afterthought
- [ ] Awareness model designed as standalone module (not inside Sentinel)

### Before Building Sentinel

- [ ] Awareness model is built and working independently
- [ ] Heartbeat file is built and working independently
- [ ] Event/action types defined as enums (testable state machine)
- [ ] Lock contention with manual `urd backup` resolved (what if both run simultaneously?)
- [ ] Circuit breaker designed (what stops cascade-triggering?)
- [ ] Passive mode works before active mode is attempted
- [ ] Process lifecycle designed: startup, crash, restart, shutdown, signal handling

### Before Building Mythic Voice

- [ ] Voice module architecture in place (structured data → renderer → text)
- [ ] At least 10 sample messages written and reviewed for tone/clarity balance
- [ ] Technical details accessible behind `--verbose` or similar
- [ ] Daemon persona output defined (JSON? structured log? plain terse?)
- [ ] Voice is testable: assert output contains specific facts given specific state

### Before Building `urd find`

- [ ] `urd get file@date` works via direct path construction (no search)
- [ ] Performance analysis on largest subvolume (subvol3-opptak, 2.8 TB)
- [ ] Index strategy decided: build index, diff-based, or restrict to `get`-only
- [ ] Progress indication designed for long-running searches
- [ ] Timeout behavior defined

---

## Open Questions

1. **What does "protected" mean for a subvolume that's too large for any single external
   drive?** Today, subvol3-opptak (2.8 TB) can only go to WD-18TB (16 TB). If WD-18TB
   fills up, the "resilient" promise (2+ copies) is unachievable. Should the promise model
   account for physical constraints, or is that a validation warning?

2. **How does the Sentinel interact with systemd?** If the Sentinel is a user service that
   triggers backups, it competes with the existing urd-backup.timer. Does the Sentinel
   *replace* the timer, or coexist? If both trigger backups, what prevents duplicate runs?

3. **Is the mythic voice localizable?** The brainstorm uses English poetic register. If
   Urd ever reaches non-English users, does the mythic voice translate? This doesn't need
   an answer now, but the voice module architecture should not hardcode English strings in
   logic.

4. **Where does the attention budget live — in config or in code?** The attention budget
   (what urgency level triggers what notification channel) feels like it should be
   configurable. But making it configurable adds complexity that conflicts with "flexibility
   only earns its keep if it's easy to operate." Sensible defaults with zero config may be
   the right call.

5. **Who writes the mythic voice text?** This is a creative writing task, not a programming
   task. A Rust engineer and a poet have different skills. The quality of the voice will
   depend on sustained creative attention, not just initial inspiration. Plan for iteration.
