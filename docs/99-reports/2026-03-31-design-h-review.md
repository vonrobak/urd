# Design Review: Guided Setup Wizard (`urd setup`)

**Design document:** `docs/95-ideas/2026-03-31-design-h-guided-setup-wizard.md`
**Reviewer:** arch-adversary
**Date:** 2026-03-31

---

## Scores

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 7/10 | Derivation logic is sound, but config serialization gap and existing-snapshot conflicts are unaddressed |
| Security | 8/10 | Minimal new attack surface; privilege escalation path needs explicit scoping |
| Architectural Excellence | 9/10 | Clean module decomposition, pure derivation function, voice separation is well-reasoned |
| Systems Design | 7/10 | --evaluate creates a divergent code path; Config lacks Serialize; discovery depends on unbuilt primitives |

**Overall:** Strong design with the right instincts. The voice philosophy is unusually well-considered for a feature that could easily devolve into gimmickry. The major gaps are in the plumbing, not the architecture.

---

## Catastrophic Failure Checklist

| # | Risk | Assessment |
|---|------|------------|
| 1 | Silent data loss | **LOW.** The wizard only generates config; it never touches snapshots or retention. No deletion path. |
| 2 | Path traversal | **LOW.** Subvolume paths come from `btrfs subvolume list` output, not user input. But see Finding 4 on discovery parsing. |
| 3 | Pinned snapshot deletion | **NOT APPLICABLE.** Wizard does not interact with snapshots. |
| 4 | Space exhaustion | **LOW.** Config generation has no space impact. However, a wizard-generated config with aggressive retention could cause space exhaustion on first run -- the design does not include a space projection warning. |
| 5 | Config change orphaning snapshots | **MEDIUM.** See Finding 1. This is the primary catastrophic concern. |
| 6 | TOCTOU privilege boundaries | **LOW.** Discovery reads are one-shot, not used for security decisions. |

---

## Findings

### Finding 1: Config replacement can orphan existing snapshots (Severity: HIGH)

**The problem.** If a user has an existing `urd.toml` with active snapshots on disk and runs `urd setup` to generate a new config, the wizard may produce a config that:

- Renames subvolumes (the wizard derives names from mount points like `@home`, but existing config uses names like `htpc-home`)
- Omits subvolumes that exist in the current config
- Changes snapshot roots
- Changes drive assignments

Any of these changes means existing snapshots become invisible to Urd. They are not deleted (no catastrophic data loss), but they consume space invisibly and their incremental chains break. The user's next `urd backup` would do full sends for everything -- potentially hours of work and massive space consumption.

**The design acknowledges this partially** -- Phase 1 says "if found, offer to evaluate rather than replace" -- but does not define what happens when the user chooses to replace anyway. The GAPS section in Phase 5 should include a migration analysis: "You have 47 existing snapshots under names that will change. Run `urd plan` after writing to see what the first backup will do."

**Recommendation:** Before writing a new config over an existing one, the wizard must:
1. Load the existing config
2. Diff subvolume names, snapshot roots, and drive assignments
3. Warn explicitly about orphaned snapshots and broken chains
4. Suggest running `urd plan` before `urd backup`

This is not optional polish -- it is a safety requirement given the project's history with snapshot congestion causing catastrophic storage failure.

---

### Finding 2: Config struct has no Serialize (Severity: HIGH, implementation blocker)

**The problem.** The `Config` struct in `config.rs` derives only `Deserialize`, not `Serialize`. The wizard's core output is writing a `Config` to TOML. There are three paths:

1. Add `Serialize` to `Config` and all nested types -- a significant refactor touching `types.rs`, `config.rs`, and `notify.rs`. Every `Interval`, `ByteSize`, `GraduatedRetention`, `LocalRetentionConfig`, `ProtectionLevel`, `DriveRole`, `RunFrequency`, and `NotificationConfig` needs `Serialize`.
2. Build a parallel "wizard output" struct that serializes to TOML independently of `Config` -- creates a divergence risk where the wizard's output format drifts from the config parser's expectations.
3. Generate TOML via string templating -- fragile, hard to test, and violates the pure-function design.

**Recommendation:** Option 1 is correct but the design should acknowledge it as prerequisite work. Adding `Serialize` to the config types is a meaningful refactor that also enables future features (config migration, `urd config show`, config round-tripping). Budget a session for it. The round-trip test mentioned in the test strategy depends on this.

---

### Finding 3: Subvolume discovery does not exist yet (Severity: MEDIUM)

**The problem.** Phase 1 calls for `btrfs subvolume list /` and `findmnt` to discover subvolumes. The `BtrfsOps` trait currently has no `list_subvolumes` method -- only `create_readonly_snapshot`, `send_receive`, `delete_subvolume`, `subvolume_exists`, and `filesystem_free_bytes`. The `drives.rs` module uses `findmnt` only for UUID verification.

This means:
- A new `BtrfsOps` method is needed (trait extension, mock update)
- Parsing `btrfs subvolume list` output is nontrivial (nested subvolumes, generation numbers, paths relative to top-level subvolume)
- Mapping subvolume IDs to mount points requires correlating `btrfs subvolume list` with `findmnt -l` or `/proc/mounts`

**Recommendation:** The design should call out the discovery layer as prerequisite infrastructure, not just "extend existing `detect_mounted_drives`." It is a new capability in `BtrfsOps` that needs its own design attention, especially around edge cases (systems with 50+ subvolumes including snapper snapshots, timeshift snapshots, Docker subvolumes, etc.).

The discovery layer must also filter aggressively. A typical BTRFS system has many subvolumes the user does not care about (Docker's per-layer subvolumes, snapper's `.snapshots` tree, Urd's own snapshot subvolumes). Presenting all of these in Phase 1 would overwhelm the user and undermine the "she derives, she does not ask" principle. The design should define a filtering strategy.

---

### Finding 4: `--evaluate` mode creates a divergent code path (Severity: MEDIUM)

**The problem.** The `--evaluate` mode reads an existing config and maps it to the disaster framework. The setup flow asks questions and derives a config. These are fundamentally different operations that share only the output vocabulary (protection levels, gap analysis, disaster framing).

The risk is that evaluate mode becomes a second implementation of awareness/coverage logic that diverges from the canonical awareness model in `awareness.rs`. The design acknowledges this: "If G is implemented as a standalone pure function, the evaluate mode calls it." But if G does not exist, evaluate "implements its own simpler version" -- and that simpler version will inevitably diverge.

**Recommendation:** Either:
- Make evaluate mode depend on the existing awareness model (it already computes promise states per subvolume), adding only the disaster framing as a presentation concern in `setup_voice.rs`. This is architecturally clean and avoids duplication.
- Or defer evaluate mode to a later slice, after the coverage map (idea G) exists as a shared pure function.

Do not implement a "simpler version" of coverage analysis inside the wizard. That path leads to two sources of truth about what survives what.

---

### Finding 5: Privilege requirements are underspecified (Severity: MEDIUM)

**The problem.** `btrfs subvolume list /` requires root privileges (or at minimum read access to the BTRFS filesystem tree). The existing `BtrfsOps` methods all go through `sudo`. The wizard will be an interactive command run by a regular user in a terminal.

Questions the design does not answer:
- Does `urd setup` require sudo? That feels wrong for a config generator.
- Can subvolume discovery use unprivileged fallbacks? (`/proc/self/mountinfo` + `findmnt` can get some information without root, but `btrfs subvolume list` typically needs root.)
- If sudo is required, what is the UX? A sudo prompt in the middle of a conversational wizard is jarring.

**Recommendation:** Design the discovery phase to attempt unprivileged detection first (`findmnt -t btrfs` for mount points, which does not require root). Fall back to `sudo btrfs subvolume list` only if the user agrees. If neither works, fall back to manual entry as the design already proposes. Document the privilege escalation path explicitly, including what sudoers entries are needed.

---

### Finding 6: Recovery depth to retention mapping needs bounds checking (Severity: LOW)

**The problem.** The mapping table from recovery depth to retention values is reasonable, but the design does not address:
- What happens on systems where run_frequency is not daily? The retention counts assume daily snapshots. A sentinel-mode system taking snapshots every 6 hours would accumulate 4x the snapshots for the same "daily" retention count.
- The "Year" depth with daily=30, weekly=52, monthly=12 on an external drive could consume significant space. No space projection is offered.

**Recommendation:** The derivation function should account for `run_frequency` when computing retention counts (it does this today for named protection levels via `derive_policy`). The wizard should show estimated space consumption for the proposed retention, especially for external drives. Even a rough estimate ("approximately 30 snapshots on WD-18TB") helps the user understand the impact.

---

## Conversation Flow Assessment

### Is the five-phase structure right?

**Yes, mostly.** The linear flow is correct -- branching would add complexity without value. Each phase has a clear purpose and output. However:

**Phase 2 and Phase 4 could merge.** Asking "how precious is this?" and then later "how far back do you need?" about the same subvolumes creates two passes over the same list. For a user with 7 subvolumes, that is 14 separate classification decisions. Consider asking both questions together per subvolume group: "Your photos are irreplaceable. How far back would you need to reach -- a month, a season, a year?" This reduces cognitive load and makes the conversation feel more natural.

**Phase 3 is well-placed.** Disaster scope is a global question that properly sits between local classification and retention derivation. Moving it would break the logic flow.

**Missing phase: validation.** Between Phase 4 (retention) and Phase 5 (summary), there should be a validation step where the derivation function runs `preflight`-style checks: "This config requires 2 drives with role=primary, but you only have 1." The design shows gap analysis in Phase 5's output, but the validation logic itself is not specified.

---

## Voice Assessment

### Where it works

- **Phase 1 output.** "I see what you have." Perfect. Terse, confident, zero embellishment. The factual table speaks for itself.
- **Phase 3 fallback.** "You have no drive kept away from this place." This is the mythological voice earning its keep -- the phrasing makes the physical reality vivid without being theatrical. "I will configure for drive failure" is direct and respectful.
- **Phase 5 GAPS section.** "If WD-18TB1 stays beside WD-18TB, site loss protection is nominal, not real." Outstanding. This is genuine insight delivered with authority. The distinction between nominal and real protection is exactly what a user needs to hear.
- **Post-approval next steps.** "Written." One word. Then practical commands. The restraint is the voice.

### Where it falls flat

- **Phase 2 intro.** "Some things, once lost, cannot be remade. Others can be rebuilt, given time." This is a fortune cookie. The user is about to classify 7 subvolumes -- they need clarity, not poetry. Consider: "Tell me what each of these is worth." Or simply present the classification options with no preamble. The four-word options (irreplaceable/important/replaceable/expendable) are strong enough to stand alone.
- **Phase 3 opener.** "What ruin would you guard against?" This works in isolation but reads as slightly performative in context. The options below it are plainly stated ("Drive failure -- a disk dies, but the machine survives"), which creates a tonal mismatch. Either elevate the options to match the question, or lower the question to match the options. Suggestion: "What is the worst that could happen here?" followed by the same plain options.
- **Phase 4 intro.** "How far back would you need to reach?" Good. Not cheesy, functionally clear.
- **Phase 5 header.** "This is what I would carve." This is on the line. In context -- after a serious conversation about disaster survival -- it works as a quiet callback to the runestone metaphor. In isolation, it risks eye-rolling. The litmus test: would a senior engineer at a Linux meetup find this acceptable? Probably yes, if the preceding conversation earned it. But it needs to be tested with real users.

### The cheesy risk, specifically

The design's self-awareness about the cheesy risk is its strongest defense. The litmus test ("Would I be embarrassed if a senior engineer saw this on my screen?") is correct. The draft text mostly passes this test, with Phase 2's intro being the main exception. The evaluate mode's "I read your current weave" is borderline -- "weave" is Urd-specific vocabulary that may not land for a first-time user encountering evaluate mode without having run the setup wizard.

---

## setup_voice.rs vs. voice.rs

**The separation is correct and well-reasoned.** The design doc's justification -- different voice register, conversational vs. terse -- is architecturally sound. Both are presentation layer modules. Both take structured data in and produce text out. They share no vocabulary.

The risk of merging them is real: `voice.rs` is currently 700+ lines of pragmatic, table-driven rendering. Adding conversational wizard text would create a file with two distinct personalities and no clear organization principle. Keeping them separate honors CLAUDE.md's "voice belongs in presentation layer" rule while acknowledging that the presentation layer can have multiple modules.

**One caveat:** If `setup_voice.rs` grows to need the same colored output utilities as `voice.rs`, extract shared formatting primitives rather than duplicating them.

---

## Open Questions (from the design)

**1. Is the conversation flow right?** See the detailed assessment above. Consider merging Phases 2 and 4 to reduce passes over the subvolume list.

**2. Is the voice earned?** Mostly yes. Phase 1, Phase 3 fallback, and Phase 5 GAPS are strong. Phase 2 intro needs work. See voice assessment above.

**3. Grouping strategy.** Group by device. The design's suggestion is correct. Users think in terms of "my NVMe" and "my storage pool," not individual subvolume names. Offer per-subvolume override only when the user says importance varies within a group.

**4. Should the wizard detect existing snapshots?** Yes, but only to inform -- not to configure. The wizard should report: "I see 23 existing snapshots under `htpc-home` on WD-18TB." This helps the user understand the current state and informs Finding 1's migration analysis. The wizard should not attempt to adopt or reconfigure existing snapshot state.

**5. Recovery depth granularity.** Four options is correct. "Forever" should not be an option -- it implies unbounded retention, which conflicts with space management. "Year" is the practical maximum for graduated retention. If a user truly needs longer, they can edit the TOML after generation.

---

## Summary of Recommendations

| Priority | Action |
|----------|--------|
| **Must** | Add orphan-snapshot detection when replacing an existing config (Finding 1) |
| **Must** | Add `Serialize` to Config and all nested types as prerequisite work (Finding 2) |
| **Must** | Design the subvolume discovery layer with filtering for non-user subvolumes (Finding 3) |
| **Should** | Make --evaluate reuse awareness.rs rather than reimplementing coverage logic (Finding 4) |
| **Should** | Design the privilege escalation path for discovery explicitly (Finding 5) |
| **Should** | Consider merging Phases 2 and 4 to reduce classification fatigue |
| **Should** | Revise Phase 2 intro text -- it is the weakest voice moment in the draft |
| **Could** | Add space projection warnings for the proposed retention configuration |
| **Could** | Account for run_frequency in retention depth derivation (Finding 6) |
