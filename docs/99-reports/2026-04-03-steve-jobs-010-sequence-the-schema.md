---
upi: "010"
date: 2026-04-03
mode: vision-filter
---

# Steve Jobs Review: Sequencing the Config Schema v1

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** How UPI 010 (Config Schema v1) fits into the roadmap alongside the test session and Phase D
**Mode:** Vision Filter (sequencing)

## The Verdict

The test session and UPI 010 are not sequential — they're complementary, and the right move is to interleave them so the encounter arrives on solid ground without adding months to the timeline.

## What's Insanely Great

**The question being asked is the right question.** "How does this fit?" is better than "let's just build it." The project has a pattern now: design, stress-test the design, then build. That pattern produced v0.9.0 — the most honest version of Urd that has ever existed. Maintaining the pattern for the config schema, which is arguably more foundational than anything in Phases A–C, is not conservatism. It's taste.

**UPI 010 absorbs P6a and P6b.** The roadmap previously had three separate items: ADR-111 resolution, P6a (enum rename), and P6b (config Serialize). UPI 010 rolls them into one coherent scope with a clear sequencing plan. That's cleaner than three deferred chores floating around the roadmap. One item, one dependency chain, one gate. Good.

**The design's own sequencing is risk-ordered correctly.** ADR revision → enum rename → Serialize → v1 parser → migrate. Each step depends on the last. The riskiest piece (parser) comes after the mechanical pieces clear the path. This isn't just good engineering — it's good product thinking. If the enum rename surfaces vocabulary problems, you catch them before they're baked into a new parser. If Serialize reveals structural issues in Config, you catch them before building migrate on top.

## What's Not Good Enough

### The current roadmap treats the test session and UPI 010 as sequential

The roadmap says:

```
test session → ADR-111 resolution → P6b → Phase D
```

That's a straight line with no parallelism. The test session takes "several days" of living with v0.9.0. During those days, the builder — you — is not building. You're observing, using the tool, noting friction. That's valuable and necessary. But the test session is not CPU-bound. It's calendar-bound.

The ADR-111 document revision (session 1 of UPI 010) is entirely a writing task. It doesn't touch code. It doesn't change behavior. It can happen while you're living with v0.9.0. The enum rename (also session 1) is mechanical and doesn't affect the user experience being tested. P6b (session 2) adds Serialize — again, no behavioral change, no risk to the test session's validity.

Treating these as sequential wastes calendar time. The test session validates the *runtime experience*. UPI 010 sessions 1-2 are *infrastructure preparation* that doesn't change what the user sees.

### The encounter is still 6-8 sessions away

Count the sessions in the current roadmap:
- Test session: ~1 session (plus calendar days)
- Fix phase from test findings: ~1 session (estimated)
- UPI 010: 3-4 sessions
- Phase D (6-O + 6-H): 6-8 sessions

Total: **11-14 sessions to v1.0 readiness**. That's a lot. The question is whether any of these can overlap without compromising quality.

The answer is yes, but only for the pieces that don't interact. The test session validates runtime. UPI 010 sessions 1-2 are spec and infrastructure. These don't conflict. UPI 010 sessions 3-4 (parser + migrate) change runtime behavior — those should come after the test session findings are addressed.

## The Vision

Here's the sequence I would build:

### Week 1: Test session + UPI 010 foundations (parallel)

**Track A: Living with v0.9.0.** Fix the systemd timer `--auto` flag. Let the nightly runs accumulate. Plug in a drive, unplug it. Run commands. Note friction. This is calendar time, not session time.

**Track B: UPI 010 sessions 1-2.** While living with v0.9.0:
- Session 1: Write the revised ADR-111 document + do the P6a enum rename. The rename is a codebase-wide search-and-replace that changes internal names. It doesn't change any user-facing behavior — `urd status` still shows the old presentation-layer terms until voice.rs is updated later. The test session is testing the *experience*, not the enum variant names.
- Session 2: P6b (add Serialize to Config). Again, purely additive — no behavioral change. The test session continues unaffected.

**Why this is safe:** Sessions 1-2 change internal names and add a trait derive. They don't change what any command outputs, how backups run, or what the Sentinel does. The test session is testing the v0.9.0 *experience*. That experience is identical before and after P6a/P6b.

### Week 2: Test findings + UPI 010 runtime changes

**Session 3: Address test session findings.** Whatever the test session surfaced — fix it. This might be zero sessions (everything is fine) or 1-2 sessions (issues found). Either way, the findings are now grounded in real usage.

**Session 4: UPI 010 session 3 — v1 parser + `urd migrate`.** Now we change runtime behavior. The parser gains dual-path config loading. The migrate command exists. The validation messages ship. This comes after the test session because:
1. The test session might reveal config-related issues that should inform v1 design
2. The v1 parser is a structural change to config loading — every command is affected
3. Once v1 ships, you migrate your own config and live with it before building the encounter on top

**Session 5: Migrate your own config, live with v1.** Run `urd migrate` on your production config. Watch a few nightly runs on the v1 schema. This is a mini-validation: does the new schema work in practice? Any issues surface here, not during the encounter.

### Week 3+: Phase D

Now Phase D begins with:
- A validated v0.9.0 runtime (test session)
- A complete v1 config schema (UPI 010)
- A migrated production config running on v1 (personal validation)
- The encounter targeting the right schema from day one

```
Week 1:
  Track A: test session (calendar)  ─────────────────────
  Track B: UPI 010 s1 (ADR + P6a)  ── s2 (P6b + parser prep)
                                                          │
Week 2:                                                   │
  Test findings fix ── UPI 010 s3 (v1 parser + migrate) ──┘
  Migrate own config, validate v1                          │
                                                           │
Week 3+:                                                   │
  Phase D: 6-O (progressive disclosure) ─→ 6-H (encounter)
```

**Estimated total: ~8-10 sessions to v1.0 readiness.** The parallelism saves 1-2 sessions compared to the purely sequential roadmap. More importantly, it saves calendar time — the test session days aren't idle.

## The Details

- **The enum rename (P6a) should use the new names in code but keep the old names in serde for the legacy parser.** This is subtle but important. The Rust enum becomes `Recorded`, `Sheltered`, `Fortified`. The legacy parser still accepts `"guarded"`, `"protected"`, `"resilient"` from TOML. The v1 parser accepts only the new names. This means P6a can ship without breaking the running production config.

- **Don't update the example config until session 3.** The example config (`config/urd.toml.example`) should match the production schema. Update it to v1 alongside the v1 parser, not during the enum rename.

- **The test session should explicitly include a "read my config" test.** Open `urd.toml`. Read it. Can you narrate your protection story? This grounds the config-readability question in real experience before UPI 010 changes the schema.

- **`urd migrate --dry-run` is a natural demo during the test session findings review.** If you've built sessions 1-2 by then, you can show what the migration *would* produce and evaluate it against the test session's readability findings.

## The Ask

1. **Run UPI 010 sessions 1-2 in parallel with the test session.** The ADR revision, enum rename, and Serialize refactor don't change user-facing behavior. They're safe to do while validating the runtime experience. This saves 1-2 weeks of calendar time.

2. **Migrate your own production config as a validation step before Phase D.** Don't just build `urd migrate` — use it. Live with the v1 schema for a few nightly runs. The encounter shouldn't be the first time v1 runs in production.

3. **Update the roadmap to show the parallel tracks.** The current roadmap is a straight line. The parallel structure is more honest about what depends on what — and it shows the path to Phase D is shorter than it looks.

4. **Keep UPI 010 sessions 3-4 after the test session findings are addressed.** The v1 parser and migrate command change runtime behavior. They belong on the other side of the test validation gate.

5. **Add "read my config" as an explicit test session goal.** The config readability question is central to UPI 010's motivation. Ground it in real experience before building the solution.
