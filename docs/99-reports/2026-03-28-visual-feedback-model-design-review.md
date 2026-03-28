# Architectural Review: Visual Feedback Model Design

**Project:** Urd
**Date:** 2026-03-28
**Scope:** Design review of `docs/95-ideas/2026-03-28-design-visual-feedback-model.md`
**Mode:** Design review (4 dimensions)
**Base commit:** `faada42`

---

## Executive Summary

The two-axis model is the right idea. Separating data safety from operational health
solves a real problem demonstrated by the hardware swap test. But the design has a
structural flaw: it puts chain health computation inside `awareness.rs`, which
currently has no access to chain state and would need to duplicate logic already
living in `commands/status.rs`. The design also carries a phantom Prometheus contract
that doesn't exist yet, pre-computes presentation text in a state file (coupling the
persistence layer to `voice.rs`), and defines seven icon states before a single user
has seen two. The core concept is sound; the machinery around it needs trimming.

---

## What Kills You

The catastrophic failure mode for a visual feedback system is **false reassurance**
— the user sees green, closes the terminal, and discovers their data is gone at
restore time. The hardware swap test proved this already happens with the current
model. The design directly addresses this failure mode, which is the most important
thing it does.

Distance from catastrophe: the design itself doesn't touch backup execution, retention,
or pin protection — it's a read-only observation layer. The risk is not that it causes
data loss, but that it fails to *warn* about data loss. That's one layer removed from
catastrophic, but it's the layer that determines whether the user acts in time.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Two-axis concept is sound. The operational health rules have edge cases (see S1) but the decomposition is right. |
| 2 | **Security** | 5 | No new trust boundaries, no new sudo paths, no new I/O. Pure observation layer. |
| 3 | **Architectural Excellence** | 3 | Core idea is clean, but chain health in awareness.rs violates current module boundaries and duplicates existing logic. Pre-computed tooltip text in the state file couples persistence to presentation. |
| 4 | **Systems Design** | 3 | Seven icon states before any tray applet exists is speculative design. The `ok_limited` state has unclear semantics for offsite drives. The backward-compat strategy is sound but the phantom Prometheus contract needs correction. |

---

## Design Tensions

### 1. Awareness purity vs. operational health inputs

**Tension:** The design puts `OperationalHealth` computation in `awareness.rs`, but
`awareness.rs` currently operates only on snapshot timestamps and drive mount state
(via `FileSystemState`). Computing operational health requires chain state (pin files,
snapshot existence on drives) and space estimates — data that `awareness.rs` has never
touched.

**Why the design probably chose this:** `awareness.rs` is the "compute all the things
about subvolume health" module. Adding a second axis to the same module keeps the
assessment unified — one call, two axes out.

**Why this is the wrong call:** Chain health is already computed in `commands/status.rs`
lines 148–174, using `chain::read_pin_file()` and filesystem existence checks. Moving
this into `awareness.rs` would either duplicate that logic or force `awareness.rs` to
depend on `chain.rs`, breaking the clean layering where awareness only knows about
timestamps and counts.

**Better resolution:** Keep chain health computation where it is (or extract it into a
small pure function in `chain.rs`). The *aggregation* into `OperationalHealth` can happen
in `commands/status.rs` or a new thin function, taking both the awareness assessment and
chain health as inputs. This is composition, not absorption.

### 2. Pre-computed tooltip text vs. pure rendering

**Tension:** The design puts human-readable `summary` and `tooltip` strings in
`sentinel-state.json`. This means `voice.rs` is called during state file writing, and
the text is frozen into a file that any external consumer reads.

**Why the design chose this:** Decouples the tray applet from Urd's Rust code. The
applet just reads strings.

**Why this is concerning:** It couples the *state file schema* to the *presentation
layer*. If the voice changes ("The well remembers" → different phrasing), the state
file format effectively changes. If a consumer wants a different language, they can't
re-render from structured data. The state file becomes a snapshot of one particular
rendering.

**Better resolution:** The state file should carry structured data (`icon` enum value,
worst safety status, worst health status, counts). The tray applet renders its own
tooltip from this structured data — which is trivial for a tooltip. If the applet is
too dumb for even that, a helper binary (`urd tray-state`) can render the text on
demand from the state file. Keep the state file mechanical.

### 3. Vocabulary change scope

**Tension:** The design proposes renaming PROTECTED→OK, AT RISK→aging, UNPROTECTED→gap
in CLI output and adding a HEALTH column. It simultaneously proposes the same
vocabulary in the sentinel state file JSON, the tray icon, and the Prometheus metrics.

**Why this matters:** The CLI interactive output is the right place to experiment with
vocabulary. It's not a contract — users read it, they don't parse it. But the JSON
output and sentinel state file are machine-readable contracts. Changing vocabulary in
all layers simultaneously is unnecessary risk. Iterate on the CLI vocabulary first, let
it settle, then promote to contracts.

---

## Findings

### S1 — Significant: `OperationalHealth::Blocked` for unmounted drives is too aggressive

The design says: "**Blocked:** No configured drives mounted AND subvolume has send_enabled."

This means every time the user unplugs their external drive (the normal state — it's an
offsite drive), every send-enabled subvolume goes to `Blocked`. For the user in the
hardware swap test, WD-18TB is routinely unmounted (offsite). WD-18TB1 may be unmounted
for days between visits. This is the *expected* state, not an anomaly.

**Consequence:** The tray icon would show yellow (`degraded`) or worse as the default
state, training the user to ignore yellow — the same alert fatigue the design explicitly
warns against in the journal addendum.

**Fix:** `Blocked` should only apply when a subvolume is *due* for a send but *cannot*
complete one. "Drives unmounted" alone is not blocked — it's "away." A subvolume is
blocked when: send is due (interval elapsed) AND no drive is mounted AND the send is
pending (not completed within threshold). The temporal dimension matters. An offsite
drive being unmounted for 2 hours when the send interval is 48 hours is not a problem.

### S2 — Significant: Chain health computation in awareness.rs violates module boundaries

As discussed in the design tensions section. `awareness.rs` is pure and depends only on
`FileSystemState` for timestamp and count data. Computing chain health requires:
- Reading pin files (`chain::read_pin_file`)
- Checking snapshot existence on external drives (filesystem I/O path construction)
- Knowing about the chain/pin concept at all

This is not data that `FileSystemState` currently provides. Extending the trait with
chain health methods would make the trait even larger (already 10 methods, noted as a
known issue in status.md) and would blur the responsibility boundary.

**Fix:** Compute `OperationalHealth` as a composition of awareness output + chain health
output, not inside awareness. Either in `commands/status.rs` (where chain health is
already computed) or in a small aggregation function that takes both inputs. The design's
data flow diagram would change — `awareness.rs` still produces axis 1 (safety), chain
health logic produces chain state, and a small combiner produces axis 2 (health).

### S3 — Significant: The design references a Prometheus metric that doesn't exist

The design states: "Prometheus metrics (`urd_promise_status` gauge)" as part of the
backward compatibility concern. This metric does not exist. The current `metrics.rs`
writes `backup_success`, `backup_last_success_timestamp`, `backup_duration_seconds`,
`backup_local_snapshot_count`, `backup_external_snapshot_count`, `backup_send_type`,
`external_drive_mounted`, `external_drive_free_bytes`, and `script_last_run_timestamp`.
None of these carry promise status.

**Consequence:** The ADR gate analysis builds on a false premise. There is no Prometheus
contract to preserve for promise status, which simplifies the backward compatibility
picture. The new `urd_operational_health` metric would be the *first* promise-related
Prometheus metric, not a companion to an existing one.

**Fix:** Remove the phantom metric from the analysis. If promise status *should* be a
Prometheus metric (reasonable), design it fresh rather than claiming to add alongside
an existing one. This also means the ADR gate for "preserving existing metric semantics"
does not apply here.

### M1 — Moderate: Seven icon states before a tray applet exists is premature

The design defines `ok`, `ok_limited`, `degraded`, `at_risk`, `unprotected`, `active`,
and `error` — seven states — for a tray applet that doesn't exist yet. Each state needs
an SVG icon, a semantic definition, and a priority ordering.

`ok_limited` is particularly problematic: it means "all safe but not all drives connected."
For the user with an offsite drive, this is the *normal* state. The tray icon would show
a diminished state most of the time, which is either correct (the user should be reminded
their offsite leg is disconnected) or annoying (they know, it's by design). This requires
user testing to determine, and the design can't do user testing.

**Fix:** Start with four states: `ok`, `warning`, `critical`, `active`. These map
directly to the two-axis model: ok = both axes green, warning = safety ok but health
degraded, critical = safety degraded. Active = backup running. Add granularity later when
a real tray applet surfaces the need.

### M2 — Moderate: `DriveAnomalyDetected` notification is fragile as a detection mechanism

The design proposes detecting the clone-swap scenario by observing "all chains broke
simultaneously." But chains can also all break simultaneously for legitimate reasons:
- User deletes old snapshots from the drive
- Drive had a filesystem error and was reformatted
- Pin files were manually cleaned up
- First send to a fresh drive (all chains are "Full" from the start)

**Consequence:** False positive notifications for normal operations. The user gets a
"verify drive identity" warning when they've just cleaned up old snapshots.

**Fix:** The design correctly notes this is a stopgap until proper drive identity is
implemented. Accept the false positives but: (1) make it an advisory, not a notification
push, (2) only trigger when chains were *previously* incremental and *became* full, not
when they were always full, and (3) combine with the space-delta signal (simultaneous
chain break + significant space change = high confidence).

### M3 — Moderate: Backward compatibility strategy has an unnecessary alias

The design proposes keeping `status` in JSON output, adding `safety` as an alias that
"eventually replaces it," and adding `health` as a new field. The alias creates a
transition period where consumers might use either name for the same value, which is
a maintenance burden for zero benefit.

**Fix:** Keep `status` in JSON output (it means what it has always meant — freshness-
based promise status). Add `health` as a new field. Don't create a `safety` alias. If
the field should be renamed, do it in a single schema version bump with a clear
migration, not through aliasing. The CLI interactive output can use "SAFETY" as its
column header without changing the JSON field name.

### C1 — Commendation: The two-axis decomposition is exactly right

Separating "do recent copies exist?" from "can the next backup succeed?" is the
design's central insight and it holds up under scrutiny. The hardware swap test is
the perfect motivating example: data safety was fine, operational health was degraded,
and the current model couldn't express this.

The key is that these axes have different *urgency profiles*. Safety degrades slowly
(on the timescale of backup intervals). Health can degrade instantly (drive unmount,
chain break). They need different colors, different tick rates, and different
notification triggers. The design recognizes all of this.

### C2 — Commendation: Temporal context in status output

Adding age indicators (`10 (2h)` for "10 snapshots, newest 2 hours ago") is the
single highest-value UX change in the design and it requires almost no new computation.
The data already exists — `LocalAssessment.newest_age` and `DriveAssessment.last_send_age`
are computed and marked `#[allow(dead_code)]` with a comment "consumed by verbose
status display (future)." The future is now.

### C3 — Commendation: Scoping discipline

The "What this design does NOT cover" section is well-drawn. Drive identity, full-send
confirmation, drive onboarding, tray applet implementation, and promise level naming
are all correctly identified as adjacent concerns and deferred. This prevents the
design from becoming a kitchen-sink proposal.

---

## The Simplicity Question

**What could be removed:**

1. **The `visual_state` block in sentinel-state.json.** The sentinel state file should
   carry structured data, not pre-rendered text. Remove `summary` and `tooltip`. Keep
   `icon` as a computed enum value — that's the right abstraction level for a state file.

2. **The `safety` alias in JSON output.** Keep `status` (it works), add `health`. No
   aliasing, no deprecation timeline, no migration.

3. **Three of the seven icon states.** `ok_limited`, `at_risk`, and `error` can be
   collapsed into `ok`, `warning`, and `warning` respectively until a tray applet
   proves they need to be distinct.

4. **`OperationalHealth` computation inside `awareness.rs`.** Replace with composition
   in the command layer. Awareness stays pure and focused on freshness. Health is
   assembled from awareness output + chain health output + space data.

**What's earning its keep:**

- The two-axis model itself — this is the load-bearing insight.
- Temporal context in the CLI output — almost free, very high value.
- The `away` vs em-dash distinction for unmounted drives — small change, reduces
  confusion.
- Chain health escalation (yellow CHAIN column, advisory on simultaneous break) — makes
  invisible signals visible.

---

## For the Dev Team

Priority-ordered action items:

1. **Move `OperationalHealth` computation out of `awareness.rs`.** Instead, add a
   function (in `commands/status.rs` or a new `health.rs` if it grows) that takes
   `SubvolAssessment` + `ChainHealthEntry` + drive space data and returns
   `OperationalHealth` + reasons. This is composition, keeping each module's
   responsibilities clean.
   - File: new function, likely in `commands/status.rs` initially
   - Why: preserves ADR-108 purity of awareness module; avoids bloating `FileSystemState`

2. **Fix the `Blocked` threshold for unmounted drives.** A subvolume is only `Blocked`
   when a send is *due* (interval elapsed, no recent send within threshold) AND no
   capable drive is mounted. Unmounted-but-not-yet-due is `Healthy`, not `Blocked`.
   - File: wherever `OperationalHealth` computation lands
   - Why: prevents yellow being the default state for offsite-drive users (alert fatigue)

3. **Remove phantom Prometheus metric reference.** The design says `urd_promise_status`
   exists — it doesn't. Rewrite the ADR gate analysis without it. If promise status
   metrics are desired, design them fresh.
   - File: the design document itself
   - Why: downstream decisions built on false premises

4. **Remove pre-computed text from sentinel state file.** Keep `visual_state.icon` as a
   computed enum. Remove `summary` and `tooltip` strings. If a consumer needs text,
   provide `urd tray-state` or let the consumer render from structured data.
   - File: state file schema in the design
   - Why: decouples persistence from presentation; avoids schema changes on voice updates

5. **Reduce icon states to four.** `ok`, `warning`, `critical`, `active`. Expand when a
   real tray applet needs the granularity.
   - File: the design document
   - Why: don't design seven states when you haven't shipped one

6. **Drop the `safety` JSON alias.** Keep `status` field name in JSON, use "SAFETY" as
   CLI column header only. Add `health` as new JSON field. No aliasing.
   - File: `output.rs` schema in the design
   - Why: aliases create ambiguity; the field means the same thing regardless of name

---

## Open Questions

1. **Where does the `OperationalHealth` aggregation function live long-term?** The design
   placed it in `awareness.rs`. This review recommends against that. `commands/status.rs`
   works for now, but if the sentinel also needs it (for `visual_state.icon`), it should
   be in a shared location. A thin `health.rs` module (pure function: assessment +
   chain data in, health out) might be the right home, but only if it actually needs to
   be called from multiple places.

2. **Should the sentinel state file carry chain health data?** Currently it only carries
   promise states. If the tray icon needs to distinguish `ok` from `warning` based on
   chain breaks, the sentinel needs chain health — which means the sentinel runner needs
   to compute it (filesystem I/O during assessment). Is this within the sentinel's scope?

3. **How does the "away" vs em-dash distinction work for the awareness model?** Currently,
   `assess_external_status()` returns `Unprotected` when `last_send_age` is `None` (no
   send history). The "away" concept is purely presentational (voice.rs). But the design
   wants this distinction to be meaningful for `OperationalHealth` too ("away" is not
   "blocked"). Where is the boundary between presentation and computation?
