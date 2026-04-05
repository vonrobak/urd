---
status: raw
date: 2026-04-05
source: Steve Jobs product review of v0.11.1 test session
---

# Brainstorm: Presentation Layer Polish

Divergent ideas for solving the UX issues identified in the
[Steve Jobs v0.11.1 test session review](../99-reports/2026-04-05-steve-jobs-000-v0111-test-session.md).

The review praised the bare invocation, vocabulary, and TTY/pipe split. It flagged
information hierarchy problems in verify/doctor, trust gaps between commands, opaque
table numbers, verbose error messages, and several small paper cuts.

These ideas explore solutions — some surgical, some ambitious.

---

## Theme A: Information Hierarchy ("Lead with what matters")

Steve's core critique: verify and doctor bury findings under noise. The user ran these
commands to learn if something is wrong, not to read 34 lines of "OK."

### A1. Verify: findings-first default

Invert the verify output. Default mode shows only findings (warn + fail), with a
one-line summary of what passed. Current verbose output becomes `--detail`.

Default:
```
htpc-root/WD-18TB: Chain broken — pinned snapshot missing locally.
  Next send will be full.

6 subvolumes verified clean. 2 drives not mounted (WD-18TB1, 2TB-backup).
```

`--detail` gives current behavior (every check for every subvolume).

Touches: `voice.rs` (render_verify_interactive), `cli.rs` (VerifyArgs gets `--detail`
flag). Pure presentation change — `output.rs` data model unchanged.

### A2. Verify: progressive reveal with streaming output

Instead of buffering everything and printing at once, verify streams output as it checks.
Clean subvolumes get a single-line "✓ subvol1-docs" that stays on screen briefly, then
findings get full detail. Creates a sense of progress and naturally foregrounds problems.

More complex: requires changing from `String` buffering to streaming `Write`. May not
compose well with daemon/JSON mode. Ambitious but would feel alive.

### A3. Doctor --thorough: separate findings from expected conditions

When doctor runs thread verification, group output into two tiers:
1. **Findings** (fail + unexpected warn) — shown first, with full detail
2. **Expected conditions** (absent drives, known states) — collapsed into a summary line

```
  Threads
    ✗ htpc-root/WD-18TB: Chain broken — next send will be full
      → Run `urd backup` when ready (will do a full send)

    14 checks OK. 2 drives absent (WD-18TB1, 2TB-backup) — skipped as expected.
```

The key insight: an absent drive is not a *finding* — it's a *known state* that the user
already sees in `urd status` and `urd drives`. Repeating it 12 times is noise.

Touches: `voice.rs` (render_doctor_interactive, the verify-inside-doctor section).
Could filter on `check.status == "warn"` where the check name is `drive-mounted` to
identify "expected" vs "unexpected" warnings.

### A4. Doctor --thorough: fold identical warnings

Instead of listing each subvolume/drive combination separately, group identical messages:

```
  Threads
    ✗ htpc-root/WD-18TB: Chain broken — next send will be full
    ⚠ 6 subvolumes × WD-18TB1: Drive not mounted
    ⚠ 6 subvolumes × 2TB-backup: Drive not mounted
```

Compresses 14 warning lines into 2. Information preserved, noise eliminated.

### A5. All commands: "nothing to report" compression

Generalize the principle: when a section has all-OK results, compress to one summary line.
This could be a voice.rs utility function used by verify, doctor, and future commands.

```rust
fn compress_homogeneous_results(items: &[CheckResult]) -> CompressedOutput {
    // If all same status, return summary line
    // If mixed, return the exceptions + summary of the rest
}
```

---

## Theme B: Trust Coherence ("Commands should agree")

Steve's finding: status says "2 need attention, run doctor" but doctor says "All clear."
The breadcrumb leads to a dead end.

### B1. Doctor acknowledges what status surfaced

When doctor's data safety section shows degraded subvolumes, the verdict should reflect
this. "All clear" should mean *everything* is fine, not just infrastructure.

Option: change verdict logic to include health degradation as a warning-level concern.
"All clear" becomes "Infrastructure healthy. 2 subvolumes degraded — absent drives."

Touches: `commands/doctor.rs` (verdict computation), `voice.rs` (verdict rendering).

### B2. Status advice points to doctor --thorough, not doctor

When status generates the "run doctor for details" advice, it should point to
`urd doctor --thorough` when the issues are thread/drive-related (not infrastructure).
This ensures the user lands on the view that actually shows their problem.

Simpler: always recommend `urd doctor --thorough` in advice text. The base doctor is
fast enough that the difference doesn't matter for UX.

### B3. Doctor: tiered verdicts

Replace the binary healthy/issues verdict with a three-tier assessment:

```
Infrastructure: all clear
Protection: 2 subvolumes degraded (WD-18TB1 away 8d)
Threads: 1 broken chain (htpc-root)
```

Each tier gets its own one-line verdict. The user sees exactly what's fine and what isn't.
The combined "All clear" only appears when all three tiers are clear.

### B4. Doctor inherits the status summary line

Start doctor output with the same safety summary that status uses: "All sealed.
2 degraded." This frames the diagnostic run in the context of what the user already
knows, and makes it obvious if doctor sees the same state status reported.

---

## Theme C: Self-Documenting Output ("Don't make me guess")

### C1. Status table: count+age column headers

Change `LOCAL` → `LOCAL #` or `LOCAL (n/age)`. Change drive columns similarly:
`WD-18TB` → `WD-18TB #`. The `#` suffix is compact and conventional for "count."

Alternatively, add a footnote line below the table:
```
# = snapshot count, (age) = newest snapshot age
```

### C2. Status table: use words for small counts

Instead of `31 (10h)`, use `31 snapshots (10h)` for the first row only, then just
numbers. The first row teaches the format, the rest are compact.

Too clever? Maybe. But the first-row-as-legend pattern works in dense tables.

### C3. Status summary line: match bare invocation quality

Change "All sealed. 2 degraded — WD-18TB1 away for 8 days." to include context like
the bare invocation does. And mention *all* absent drives in the summary, not just one.

```
All sealed. 2 degraded — WD-18TB1 away 8d, 2TB-backup away 2d.
```

### C4. Relative timestamps in status

The bare invocation says "10h ago" (warm, immediate). The status command says
"2026-04-05T04:01:11" (cold, precise). Steve's suggestion: lead with relative, append
precise.

```
Last backup: 10h ago (04:01, success) [#29]
```

The `humanize_duration()` function already exists in voice.rs. The status output already
has `last_run_age_secs` computed. This is a rendering-only change.

### C5. Thread column: rename ext-only to drive-only

"ext-only" is internal vocabulary. "drive-only" maps to the user's mental model (their
data lives on the drive, not locally). Or use `—` in the thread column with a note in
the drive summary that htpc-root has no local snapshots.

### C6. History: humanize zero-duration runs

When a backup completes in 0 seconds (nothing to do), show `<1s` or `(no-op)` instead
of `0s`. The current `0s` reads as broken or erroneous.

```
20   2026-04-01T20:56:05  full  success  <1s
```

---

## Theme D: Error Messages as Guidance ("Guide, don't dump")

### D1. Retention-preview: columnar subvolume list

Replace the comma-separated dump with a structured error:

```
Usage: urd retention-preview <subvolume> [--all]

Available subvolumes:
  subvol1-docs        subvol3-opptak      subvol5-music
  subvol2-pics        subvol4-multimedia  subvol6-tmp
  subvol7-containers  htpc-home           htpc-root
```

Touches: `commands/retention_preview.rs` (the `anyhow::bail!` call). Could move to
voice.rs as a reusable `format_subvolume_chooser()` for any command that needs a
subvolume argument.

### D2. Subvolume chooser pattern

Multiple commands need a subvolume argument (retention-preview, history <subvolume>,
potentially verify). A shared `format_subvolume_list()` in voice.rs would ensure
consistent, scannable presentation across all of them.

### D3. Graceful --verbose handling

The global `--verbose` flag exists in `Cli` but clap requires it *before* the subcommand.
`urd status --verbose` fails because clap interprets `--verbose` as an argument to the
`Status` subcommand, which has no args.

Options:
- **a)** Add `#[arg(long, short)]` verbose flag to every subcommand struct. Slightly
  redundant but clap-idiomatic.
- **b)** Use clap's `#[command(args_conflicts_with_subcommands = false)]` or
  `#[command(propagate_version = true)]` — need to check if global arg propagation
  works.
- **c)** Catch the clap error in main.rs and suggest `urd -v status` — cheapest fix
  but teaches the user a workaround, not the right way.

The real question: what does `--verbose` *do* for each command? Status already shows
the full view. Plan could show more reasoning. Verify could show all checks (Theme A
makes this `--detail`). Define the semantics before adding the flag.

### D4. Friendly typo recovery

Clap's built-in "tip: a similar subcommand exists" is functional but impersonal. Since
Urd has a voice, consider intercepting unrecognized subcommands:

```
Did you mean `urd drives`?
```

Touches: `main.rs` error handling. Clap may expose the suggestion programmatically.

---

## Theme E: Paper Cuts ("The details that signal care")

### E1. Fix `issue(s)` pluralization

In `render_doctor_interactive`, replace `format!("{} issue(s).", count)` with proper
pluralization:

```rust
let word = if count == 1 { "issue" } else { "issues" };
format!("{count} {word}.")
```

Same for `warning(s)`. Five-minute fix. Apply everywhere this pattern appears.

### E2. Doctor: fulfill the "suggested commands" promise

The verdict says "Run suggested commands to resolve" but the chain-break finding doesn't
include a command suggestion. Either:
- Add a suggestion: `→ Run 'urd backup' to trigger a full send`
- Change the verdict text: `1 issue found.` without the promise

### E3. Drives table: fix Unicode width alignment

The `✓` (U+2713) and `—` (U+2014) have different display widths in many terminal fonts.
Use fixed-width representations or pad explicitly to account for this. The `strip_ansi_len`
function exists but doesn't account for Unicode display width. Consider the `unicode-width`
crate, or use ASCII alternatives (`OK` / `-`).

### E4. Protection aging vocabulary

Status says "protection degrading" for absent drives. Steve suggests "protection aging" or
"stale" — less urgent, more accurate. The user can't act on this (the drive is physically
elsewhere), so the vocabulary should convey drift, not emergency.

Options:
- `WD-18TB1 absent 8d — copies aging`
- `WD-18TB1 absent 8d — stale`
- `WD-18TB1 absent 8d — last sync aging`

### E5. History: relative timestamps option

Currently history shows ISO timestamps only. A `--relative` flag (or default behavior)
could show relative times alongside:

```
RUN  STARTED          AGE     MODE  RESULT   DURATION
29   04:01 today      10h     full  success  9m 31s
28   04:00 yesterday  1d      full  success  31s
```

Ambitious but natural extension of the "humanize timestamps" principle.

---

## Theme F: Ambitious ("What if we could?")

### F1. Verify as a quiet guardian

What if `urd verify` was designed like a linter? Default output: one line per finding,
nothing for clean. A return code tells scripts whether everything passed. The user gets
signal, not ceremony.

```
$ urd verify
htpc-root/WD-18TB: chain broken — next send will be full
$ echo $?
1
```

Clean run:
```
$ urd verify
All threads intact.
$ echo $?
0
```

The detailed per-check view becomes `urd verify --detail` for debugging. This is the
`cargo clippy` model — you want to know what's wrong, not that 200 things are right.

### F2. Command output that teaches

What if every command could optionally explain itself? Not `--verbose` (more data) but
`--explain` (why this data):

```
$ urd status --explain
All sealed. 2 degraded — WD-18TB1 away for 8 days.

  "sealed" means every subvolume has at least one copy on an external drive,
  matching its configured protection level.

  "degraded" means a drive that's expected to have copies hasn't been seen
  recently. Your data is safe, but redundancy is reduced.
```

This is progressive disclosure at the command level. Useful for onboarding without
cluttering the default experience. Could be powered by a `voice::explain()` module.

### F3. The diagnostic journey

What if doctor wasn't a static report but a guided diagnostic? Instead of showing
everything and hoping the user finds the problem, doctor asks questions:

```
$ urd doctor
Your data is safe. One thread is broken.

  htpc-root last sent to WD-18TB 1 day ago.
  The local copy was deleted, so the next send will be a full transfer.

  This is expected after manual cleanup. No action needed —
  the next nightly will handle it.

Want to see infrastructure checks? [y/N]
```

Interactive when TTY, static report when piped. The key insight: most of the time,
the user just wants to know "should I worry?" Doctor should answer that first, then
offer depth.

### F4. Unified health score

What if Urd had a single number — a health score from 0-100? Not as the primary
interface, but as a quick-glance metric for monitoring dashboards and bare invocation:

```
Urd health: 94/100. All sealed, 2 drives aging.
```

Dangerous territory — reductive metrics can mislead. But if the scoring is transparent
(deductions for: broken chain -3, absent drive -2/drive, degraded subvol -1/subvol),
it could serve the "is my data safe?" question at a glance.

Probably a bad idea for the CLI. Maybe good for Prometheus metrics. Park this.

### F5. The voice module as a personality layer

Currently `voice.rs` is a rendering engine — it formats structured data into text. But
the mythic voice aspiration in CLAUDE.md suggests something more: a personality layer
that makes the same data feel different depending on context.

What if the same data — "htpc-root chain broken" — could be presented differently
based on context?

- Interactive (invoked by user): "htpc-root's thread to WD-18TB is broken. The next
  send will retrace the full path."
- Notification (sentinel alert): "A thread has broken. htpc-root will need a full send."
- Bare invocation (glance): "1 thread needs mending."

Same data, different register. The structured output types already support this — the
rendering layer just needs to be context-aware. This is the mythic voice done right:
not dramatic prose, but contextual communication that matches the user's current
attention level.

---

## Handoff to Architecture

The five most promising ideas for deeper analysis:

1. **A1 + A3: Findings-first verify and doctor** — The single highest-impact UX change.
   Both commands need the same inversion: lead with problems, summarize what's fine.
   Pure voice.rs changes with no data model impact. Should be a single design.

2. **B1 + B2: Trust coherence between status and doctor** — The "All clear" / "2 need
   attention" gap is a trust-eroding bug. Doctor's verdict needs to account for degraded
   health, and status's advice should point to `--thorough`.

3. **C4: Relative timestamps in status** — Quick win. The function exists, the data
   exists, it's just a rendering change. Brings status in line with the bare invocation's
   warmth.

4. **D3: Graceful --verbose handling** — The global `-v` flag exists but doesn't
   propagate to subcommands. Solving this requires deciding what verbose *means* per
   command, which is a design question worth getting right.

5. **D1 + D2: Subvolume chooser pattern** — The retention-preview error is the worst
   error message in the tool. A shared columnar subvolume list would fix it and
   establish a pattern for future commands.
