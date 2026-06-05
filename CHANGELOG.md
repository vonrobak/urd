# Changelog

All notable changes to Urd are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Armed-tier coherence hardened from convention into structure** (hardens the ADR-113
  single-gather invariant). The hysteresis-resolved storage tier is now carried on
  `ResolvedStorageSignal` and derived once by its constructor; awareness reads that stamped
  tier instead of independently re-resolving it, so the staleness judgement can no longer
  desync from the tier the planner timed against — closing a latent false-AT-RISK /
  false-PROTECTED path. Behavior-preserving: the planner/executor map and the post-exec
  writeback are unchanged.

## [0.24.1] - 2026-06-04

### Fixed
- **Post-upgrade acknowledgment preamble now gated on output mode** (#168). The one-time
  v0.13.0 "trust repair" notice could be prepended ahead of the JSON document on a single
  Daemon (non-TTY) run, making that one output non-parseable as JSON. The preamble is now
  suppressed in Daemon mode and, crucially, a daemon run no longer consumes the one-shot
  marker — so the user still sees the reassurance on their next interactive invocation.
  Closes the latent pattern where any future acknowledgment reusing `preamble_for` would
  inherit the same JSON-corruption window.

## [0.24.0] - 2026-06-03

### Added
- **Rotation voice: forecast, "hibernating," and the offsite drive-row ladder** (UPI 056 PR2,
  cites ADR-116). `urd status` now *speaks the rhythm* instead of reporting bald absence. An
  offsite drive away on schedule reads **hibernating** with a *due home in ~Nd* forecast (shown
  only while the homecoming is still ahead); past the cadence midpoint but still protected it
  reads **due home — cycle it on your next trip** — both calm and uncoloured. Only a genuinely
  overdue/stale copy crosses to **absent**, its offsite thread **fraying** (amber) → **worn
  thin** (red). Gravity comes solely from the per-copy promise status — the rotation words only
  enrich the wording within each band, so a `source_unchanged` away offsite never reddens
  regardless of its data-age. The `OffsiteDriveStale` advisory is now **cadence-relative** ("…
  overdue — 11 days past its usual ~45d cycle"). After a clean offsite send, `urd backup` adds a
  *safe to take {drive} back offsite* cue. The `--json` status surface gains an additive,
  offsite-only `rotation` block (`cadence_secs`, `last_home`, `forecast_secs`, `source`) for
  Spindle — no `schema_version` bump (additive `--json` evolution, ADR-114 precedent), no
  metric/heartbeat change.
- **Role-aware offsite freshness model + rotation view** (UPI 055, ADR-116). Urd's strongest
  tier (multi-drive + offsite) no longer reads chronically degraded while an offsite drive is
  away on its normal rotation rhythm. An offsite drive's absence is now judged against its
  **rotation cadence** — declared via a new optional `rotation_interval` on the drive block
  (e.g. `"3mo"`; PRIMARY), the observed median homecoming gap (fallback, ≥3 homecomings), or a
  30-day default — instead of the send interval. Away **on schedule** → PROTECTED and silent;
  only a genuinely **overdue** copy degrades health and re-arms the `OffsiteDriveStale`
  advisory. Introduces the two-clocks distinction: the per-copy promise keys on **data-age**
  (time since last send), while the health "away" nag keys on **presence-age** (time since the
  drive was here). The relaxation fires only when a real redundancy peer is currently mounted —
  a subvolume whose only external drive is an away offsite keeps the honest send-interval
  judgment, so a missing sole copy is never falsely reported PROTECTED. `Interval` gains `mo`
  (30d) and `y` (365d) units. No metric/heartbeat field changes, but existing promise/advisory
  *values* shift (the intended flattened sawtooth) — verify against the homelab's ADR-021 if any
  alert is calibrated to the old offsite-away-degrades behavior.

### Changed
- **Fortified offsite-freshness overlay is now rotation-window-aware, capped at AT RISK** (UPI
  056, cites ADR-116). The last surface still measuring offsite freshness on a fixed 30/90-day
  clock now reduces over the per-copy, window-aware promise that `assess()` already computes
  (UPI 055) — the freshest offsite copy wins. This changes the *timing* of a Fortified
  subvolume's `promise_status` (same `PROTECTED`/`AT RISK`/`UNPROTECTED` field — no schema
  change): a long declared rotation window relaxes AT RISK → PROTECTED earlier, a short window
  degrades earlier. A stale offsite copy now caps the promise at **AT RISK, never UNPROTECTED** —
  the present local/primary copy keeps the data recoverable, so the old
  `>90d → UNPROTECTED-from-offsite` degrade is removed (genuine "no current copy" still reaches
  UNPROTECTED independently). A Fortified subvolume with *no* offsite drive at all now reads AT
  RISK (site-loss-exposed) rather than UNPROTECTED, with the `NoOffsiteProtection` advisory still
  firing. No `schema_version` bump (additive `--json` evolution, ADR-114 precedent). Verify
  against the homelab's ADR-021 if any alert keys on the old offsite-degrade timing.

## [0.23.0] - 2026-06-02

### Added
- **Presence-aware graduated pin shedding** (UPI 058, new ADR-116 "Offsite rotation is
  expected absence"). Under storage pressure Urd now sheds an **away** offsite drive's pin
  first — the old, large-CoW pin for a drive that isn't even here — and preserves the
  **connected** drive's cheap incremental chain; a full send is only the fallback. Fixes two
  paths that handled the multi-drive (connected primary + away offsite) case backwards. The
  Critical `clear-all` lifecycle is now **presence-conditional**: with an away-*only* pin Urd
  retains-one for the connected chain and sheds the away pin in-run, escalating statelessly next
  run if pressure persists (byte-identical to v0.22.0 when no away pin exists). The emergency
  reclaim (the watchdog abort-reclaim / idle eject backstop) is now **two-tier** — shed away
  pins, re-measure against the host-survival floor, blanket-clear only if the floor still demands
  it (the connected chains survive when the away shed alone relieves the pressure). Shedding an
  offsite pin loses no data — a pin proves a completed offsite copy, so only the incremental
  *chain* breaks (next send full), the cost the user explicitly tolerates. The presence predicate
  is snapshot-level (a snapshot a connected drive still needs is never shed) and computed from a
  single shared scope helper, so the planner's and executor's decisions cannot diverge. All
  ADR-106/107 data-loss gates are preserved (the presence-blind pre-delete re-check,
  never-the-only-copy, fail-closed pin handling). Amends v0.22.0's unconditional Critical
  clear-all. No metric/heartbeat/on-disk/config-schema change (Cross-Repo Impact: None).

## [0.22.0] - 2026-06-02

### Added
- **Idle emergency eject** (UPI 034, ADR-113 Layer 3). Closes the last do-no-harm gap —
  the idle window between runs. Layers 1 (031-b) and 2 (033) are both run-coupled, so a
  source pool filling while Urd sat idle (pin CoW delta, ambient host writes) could slide
  toward a full disk with no Urd code able to reclaim. The always-on sentinel now polls each
  source pool on a dedicated ~60 s timer and, when a pool crosses the host-survival floor
  (`min_free + cleanup_budget` — the *same* `source_floor_bytes` the watchdog uses, extracted
  so the two layers cannot drift) with **no backup running**, sheds the pool's send-enabled,
  offsite-confirmed local snapshots by reusing 033's `emergency_reclaim_pool` (never-the-only-copy
  gate, fail-closed pin-drop ordering, per-subvol isolation — zero new shedding logic). It is the
  sentinel's **first filesystem-mutating action**; blast radius is bounded by sudoers (btrfs-only)
  and the never-the-only-copy rule. It defers to a running backup via a try-lock on the backup lock
  path (mutually exclusive with the watchdog) and re-confirms free space under the lock before
  acting. A confirmed pin is trusted as proof of the offsite copy — idle eject does **not**
  re-verify against the (usually absent) drive, the deliberate trade that keeps the drive-absent
  case covered (ADR-113 catastrophic-floor doctrine). Adds the distinct `EmergencyEject` ADR-114
  event (`EventKind::EmergencyEject`, filterable via `urd events --kind emergency_eject`) and a
  `Critical`-urgency "severed … thread(s)" notification on an actual reclaim — the notification
  says the offsite copy is still safe, not that nothing is lost. No new ADR, no config field, no
  metric/heartbeat/on-disk/config-schema change (Cross-Repo Impact: None).
- **Mid-op watchdog + reserve file** (UPI 033, ADR-113 Layer 2). Closes the
  in-flight blind spot: between "send started" and "send finished" nothing watched
  the host, yet a long send holds its source snapshot the whole transfer while live
  `/` churns CoW into it. An in-process sibling thread now polls each armed source
  pool's free level **and** drop-rate during sends (pure decision core in the new
  `guard.rs`; reserve I/O in the new `reserve.rs`). On a floor (`min_free +
  cleanup_budget`) or cliff (free falling > 100 MB/s) trigger it first deletes a
  pre-allocated 1 GiB `.urd-emergency-reserve` — the fast bridge, freed on the
  watchdog thread so it fires even if the copy thread is wedged — then, if still
  tripping, cancels the in-flight send via a flag in the copy loop. Because
  cancelling a send frees no source space on its own, once the send exits the
  executor's new `emergency_reclaim_pool` clears the **triggering pool's** local
  snapshots (the just-aborted snapshot *and* its pin parent), reusing the 031-b
  fail-closed clear-all ordering: host survival over chain continuity, the next send
  becomes full (an ADR-106-scoped exception authorized by ADR-113's catastrophic-floor
  doctrine; the live subvolume is untouched and falls back to its prior offsite copy).
  Only subvolumes with a confirmed offsite copy are cleared — a subvolume that has never
  been sent keeps its local snapshots (never delete the only stored copy) — and a run that
  began below the watchdog floor (a pre-flight condition) is not self-aborted; the watchdog
  watches for in-flight free-fall instead.
  Adds the optional per-`snapshot_root` `cleanup_budget` config field (additive across
  legacy/v1/v2, no `urd migrate` step; defaults to 1.5 % of pool capacity) and the
  `WatchdogAbort` ADR-114 event with a `Critical`-urgency notification. The watchdog
  arms only on Tight/Critical source pools with a send-enabled subvolume and is **not**
  TTY-gated (autonomous runs need it most); a reserve is pre-positioned at the first
  Tight (or roomy-with-room) run so it exists before a pool jumps to Critical. The
  reserve is `fallocate`d (real extents, exempt from transparent compression) — never
  zero-byte-written, which would free nothing on a `compress` mount. Event-only surface:
  no metric, heartbeat, on-disk, or config-schema-version change (Cross-Repo Impact: None).
- **Tier-graded ephemeral footprint-cap** (UPI 031-b, ADR-113 increment 2). Makes the
  storage tightness tier from 031-a *act* on Urd's own footprint instead of merely
  reporting it. The armed tier is now resolved once pre-plan and threaded into the
  planner, executor, and awareness, so a tight source pool automatically sheds Urd's
  local footprint: **Tight** → retain-one parent (incremental sends) plus a modest 1.5×
  send-interval stretch; **Critical** → clear-all (drop the pin, full sends, ≈0 steady
  footprint) plus a weekly send-interval floor so the forced full sends stay rare.
  Awareness judges staleness against the *effective* (adapted) interval and caps the
  promise at AT RISK while Critical — surfaced told-not-silent in `urd status` as
  deliberate care ("tight drive — backing up every 7d to spare it. Reads AT RISK by
  design, not a failure."), not a failure. The clear-all deletion path routes through the
  existing executor gate (all-sends-succeeded + no-pin-failure + fail-closed re-read,
  ADR-106/107), removing the pin before the re-read so the just-sent snapshot can be
  cleared; a pin-removal failure fails open (skips the clear, retries next run). Both
  `urd plan` and `backup --dry-run` now gather signals and show the storage-adapted plan.
  Carries two in-place ADR amendments — ADR-113 (four→three defensive layers, predictive
  guards retired) and ADR-110 (the AT-RISK cap overturns arc decision R4) — and deletes
  the now-confirmed-dead `HeadroomSeverity::Critical` machinery (AB5). No metric,
  heartbeat, on-disk, or config-schema change (Cross-Repo Impact: None).
- **Storage-pressure state in `urd status`** (UPI 031-a, reworks the unreleased UPI 031).
  Splits 031's single `is_storage_critical` predicate — which conflated host-root-ness
  with current pressure and inverted the severity/response ladder — into two orthogonal
  axes: a **tightness tier** (`Roomy / Tight / Critical`, free-ratio only on the source
  pool) and a **host-root** escalation flag. The tier is now surfaced **told-not-silent**
  in `urd status` and bare `urd` (per-pool: "your `/data` runs tight — N subvolumes
  affected"; host-root pressure adds "…pressure here risks the machine itself"), backed
  by a persisted, best-effort, hysteresis-stabilized per-pool armed tier
  (`pool_armed_tier` SQLite table) so the state survives runs and does not flap. Backup
  runs dispatch a best-effort `notify.rs` notification on escalation (status-only when no
  channel is configured); de-escalation is silent. The inverted `doctor --thorough` row
  advisory and its `storage_critical` field are removed — the posture now appears in the
  `doctor` data-safety section instead, and the recommendation row returns to pure
  retention-shape advice. The detection + state foundation that UPIs 032 (predictive
  guards) and 033 (mid-op watchdog) build behavior on; ships with them as ADR-113
  increment 2. No metric or heartbeat schema change (Cross-Repo Impact: None); no on-disk
  or config-schema change.

## [0.21.2] - 2026-05-29

### Changed
- **Internal refactor: thread `PromiseStatus` through the output boundary** (UPI 053).
  Typed all eight structured-output promise-status fields (`status`, `local_status`,
  `worst_safety`, heartbeat `promise_status`, the sentinel state file, `doctor`/`backup
  --json`) as the `PromiseStatus` enum instead of `String`, and deleted the
  `notify::status_rank` / `voice::status_severity` re-derivation in favor of the enum's
  `Ord`. Unified `PromiseStatus`'s serde form on SCREAMING (matching `Display`) via
  per-variant `rename` + permanent legacy `snake_case` `alias`. No external contract
  change: every write-form is byte-identical, except the internal `events`-table payload
  (`at_risk` → `AT RISK`), which is read-compatible via the alias (see ADR-114 amendment
  2026-05-29). Closes the "Status string fragility" known issue.

## [0.21.1] - 2026-05-29

### Changed
- **Internal refactor: `FileSystemState` read-side split, PR 1** (UPI 052).
  Split the 14-method `plan::FileSystemState` trait into two narrow query traits
  along the ADR-102 axis — `FilesystemQuery` (filesystem-of-truth + drive
  availability) and `HistoryQuery` (SQLite history) — in a new `observation.rs`
  module. A `FileSystemState` bridge supertrait + blanket impl keeps every
  existing caller and mock compiling unchanged while the seam is narrowed
  incrementally in later PRs. No behavior, on-disk, or config-schema change.
- **Internal refactor: `Observation` cutover + ADR-101 generation-read fix, PR 2**
  (UPI 052). Introduced `Observation<'a>` — a `{ fs, history, btrfs }` bundle of
  read-only seams — and threaded it through `plan::plan` and `awareness::assess`,
  replacing the wide `&dyn FileSystemState` parameter. Closed the ADR-101 loophole
  where `subvolume_generation` ran a `sudo btrfs` subprocess from a free function
  outside `BtrfsOps`: it now lives on a new read-only `BtrfsRead` trait
  (`BtrfsOps: BtrfsRead`), so pure planners read generations through a non-mutating
  seam that cannot upcast to the mutating ops. Fail-open generation semantics
  preserved verbatim. No behavior, on-disk, or config-schema change.
- **Internal refactor: command-layer tail narrowing + bridge removal, PR 3 (final)**
  (UPI 052). Narrowed the 13 remaining `&dyn FileSystemState` command-layer
  signatures (in `init`, `verify`, `plan_cmd`, `backup`) to the single query half
  each uses, then deleted the now-callerless `FileSystemState` bridge supertrait,
  its blanket impl, and the `plan` re-export. Completes the UPI 052 arc:
  `FileSystemState` no longer exists in the source tree. No behavior, on-disk, or
  config-schema change; zero test edits.

## [0.21.0] - 2026-05-25

### Added
- **Prometheus metric `backup_external_expected{subvolume}`** — emits `1` for each
  subvolume that has an external destination configured (sends enabled and at least one
  drive in scope); the line is absent otherwise. Lets monitoring distinguish a genuinely
  missing offsite copy from an intentionally local-only subvolume (e.g. `send_enabled =
  false`), via `backup_snapshot_count{location="external"} == 0 and on(subvolume)
  backup_external_expected == 1`.
- **Prometheus metric `backup_pool_total_bytes{uuid,role,label}`** — total BTRFS pool
  capacity (statvfs), alongside the existing `backup_pool_free_bytes`. Enables a
  destination free-% alert for offsite drives that `node_exporter` does not scrape. Free
  and capacity are read from a single statvfs call so they never skew within a run.

## [0.20.5] - 2026-05-20

### Changed
- **Internal refactor: voice.rs decomposition phase 2** (UPI 050 follow-up).
  Extracted the remaining 13 per-command renderers from `voice/mod.rs` into
  sibling sub-modules: `voice/backup.rs` (render_backup_summary +
  render_pre_action + transitions/skipped-block/assessment-table helpers),
  `voice/plan.rs` (render_plan + render_empty_plan + skip-group helpers),
  `voice/verify.rs` (render_verify + render_failures), `voice/history.rs`
  (render_history + render_subvolume_history + render_events),
  `voice/init.rs`, `voice/calibrate.rs`, `voice/sentinel.rs`,
  `voice/emergency.rs` (assessment + result), `voice/retention.rs`,
  `voice/drives.rs` (list + adopt), `voice/get.rs`, `voice/chooser.rs`, and
  extended `voice/status.rs` with `render_default_status` + `render_first_time`.
  `voice/mod.rs` shrank from 8008 lines (post-phase-1) to ~6000 lines,
  containing only the cross-renderer helpers (`humanize_duration`,
  `exposure_label`, `color_*`, `pluralize`, `classify_verify_checks`,
  `SuggestionContext`/`append_suggestion`, `format_history_table`,
  `truncate_str`, `skip_tag`, `aggregate_drive_info`,
  `unmounted_drive_label`, `format_drive_age_label`, `status_severity`,
  test fixtures) and the parent's test suite. Public surface unchanged
  (`pub use status::{...}`, `pub use backup::{...}`, etc.) — zero changes
  at 23 caller sites in `src/commands/`. Voice Contract suite (44 tests)
  green pre- and post-split; full suite 1435 passing. Closes UPI 050.
- **Internal refactor: fold `state_views.rs`** back into its callers per the
  2026-05-19 citizenship decision. `ChurnView::for_subvolume` and
  `::for_subvolume_default_window` had only one citizen (`ChurnView`) by the
  end of the UPI 049 + UPI 050 phase 1 probation window; no second view
  materialized from real call-site composition pain. The composition
  (optional `StateDb` → `drift_samples_for_subvolume` → `drift_row_to_sample`
  map → `compute_rolling_churn`) is now inlined into the two substantive
  caller sites: a single private `compute_churn_for` helper in
  `commands/doctor.rs` (used by both `compute_churn_for` callers and by
  `build_doctor_churn_view_inner`) and one inlined block in
  `commands/backup.rs::build_churn_views`. `drift::ChurnEstimate` now derives
  `Default` (safe-empty estimate is the ADR-102 fallback). `src/state_views.rs`
  deleted, `mod state_views;` removed from `src/main.rs`, CLAUDE.md
  module-table row removed (the row-conversion helper
  `StateDb::drift_row_to_sample` already lived in `state.rs`). 4 tests removed
  with the module (`compute_rolling_churn` and `drift_samples_for_subvolume`
  both have their own coverage; the inlined fallback is exercised by the
  existing `build_doctor_churn_view_inner` test).
- **Internal refactor: voice.rs decomposition phase 1** (UPI 050). Converted
  `src/voice.rs` to a `src/voice/` directory with the parent `voice/mod.rs`
  and two extracted sub-modules: `voice/doctor.rs` (render_doctor + private
  recommendation/churn/check-section helpers, 555 lines) and
  `voice/status.rs` (render_status + private summary/table/advisory/drive
  helpers, 436 lines). Cross-renderer helpers (`humanize_duration`,
  `exposure_label`, `format_status_table`, `color_*`, `pluralize`,
  `classify_verify_checks`, `aggregate_drive_info`, `unmounted_drive_label`,
  `append_suggestion`) stay in `voice/mod.rs` as `pub(super)` for sub-module
  access. Public surface unchanged (`pub use doctor::render_doctor;
  pub use status::render_status;`). Voice Contract suite (44 tests) green
  pre- and post-split — no rendered text change.
- **Internal refactor: split advice surface out of `awareness.rs` into a new
  `src/advice.rs` module** (UPI 049). `awareness.rs` now owns observation
  (assess promise state) only; the new `advice.rs` owns translation
  (`compute_advice`, `compute_redundancy_advisories`, `overlay_offsite_freshness`,
  `ActionableAdvice`, `RedundancyAdvisory`, `RedundancyAdvisoryKind`). Shared
  test fixtures (`dt`, `snap`, `test_config`, `offsite_test_config`) live in a
  new `awareness::test_support` module imported by both modules' tests. No
  behavior change, no on-disk contract change. Pure-function invariant
  (ADR-108) and module-table option-C symmetry preserved.

## [0.20.4] - 2026-05-19

### Fixed
- **Per-delete `btrfs subvolume sync` removed for policy-driven retention deletes**
  (#138, #139). The executor previously called `sudo btrfs subvolume sync` after every
  successful delete to refresh free-space before the post-delete
  `space_recovered` check. On a busy pool the sync blocks for 7–140 s while the
  BTRFS cleaner thread drains queued cleanup, which made catch-up runs take
  hours where they should take minutes (measured: 30 deletes in 39 minutes on
  a 12 TB pool; median per-delete 75–120 s entirely inside the sync). After
  v0.20.3 the `space_recovered` check applies only to `SpacePressure` deletes,
  so the sync is now also scoped to `SpacePressure`. `Policy` deletes return
  immediately; the BTRFS cleaner runs asynchronously regardless. Trade-off
  (bounded): a `Policy` delete followed by `SpacePressure` deletes on the same
  location won't have published recovery — the first trailing `SpacePressure`
  delete will execute (then sync + check + publish, re-engaging the
  short-circuit for any further pressure deletes). New
  `policy_deletes_do_not_sync` test pins this contract.

## [0.20.3] - 2026-05-19

### Fixed
- **Retention thinning no longer silently skipped after the first delete per
  location** (#137). The executor's space-recovery short-circuit (introduced
  with UPI 016) was applied to every delete, including policy-driven graduated
  retention. Because the `space_recovered` map is keyed by location (drive
  label or local snapshot-root path) and shared across subvolumes, on a
  filesystem with comfortable free space the very first delete at a location
  tipped recovery to "satisfied" and every subsequent delete — across all
  subvolumes sharing that location — was skipped with
  `space recovered, deletion skipped`. Symptom: snapshot counts grew unbounded
  even though `urd plan` reported correct retention targets (e.g. 61 local
  snapshots for a `daily=7, weekly=4` policy that targets ~11).
  Fix: introduced `DeleteKind { Policy, SpacePressure }` derived from
  `PruneRule` at the retention boundary; the short-circuit now only applies
  to `SpacePressure` deletes (hourly thinning under pressure, space-governed
  extras, emergency reclaim). `Policy` deletes always execute, subject to
  the unchanged pin re-check (ADR-106 layer 3). Pin protection invariants
  preserved; no on-disk format changes; `urd plan` output unchanged.

## [0.20.2] - 2026-05-19

### Fixed
- **`--subvolume NAME` now errors on unknown names** (#134). `urd plan
  --subvolume FOO` and `urd backup --subvolume FOO` previously responded
  with a falsely-reassuring `All sealed.` / `Nothing to do.` when `FOO`
  didn't match any configured subvolume — the silent empty-set result of
  the planner's filter. Validation now runs at the CLI boundary across
  all 8 `--subvolume`-accepting commands (`plan`, `backup`, `history`,
  `calibrate`, `verify`, `events`, `get`, `retention-preview`) and exits
  non-zero with the configured-names listing and a Levenshtein-based
  "Did you mean: …?" suggestion. Two pre-existing ad-hoc validators
  (`get`, `retention-preview`) are unified on the shared helper for
  consistent phrasing.

### Changed
- **`urd retention-preview --all --subvolume BOGUS` now errors** instead
  of silently ignoring the unknown name and running `--all`. A valid
  `--subvolume` name combined with `--all` is unchanged (`--all` still
  wins). Script callers templating in a subvolume name with `--all` will
  now see a hard failure on typos rather than a silent success.

## [0.20.1] - 2026-05-17

### Changed
- **Renamed `policy` module to `recommendation`** (internal). The module
  introduced for UPI 041 / ADR-115 is the advisory retention-shape
  recommendation engine, not a "policy" derivation — `types.rs::derive_policy()`
  is the latter. The new name matches the glossary's "recommendation layer"
  vocabulary and removes the conflation. No behaviour change, no public
  CLI / config / on-disk surface affected.
- **Glossary: added a Recommendation cluster** in
  `docs/00-foundation/glossary.md` defining `shape`, `inter-slot delta`,
  `outer-edge span`, `drift signal`, `symmetric data-cost model`, `headroom`,
  and `recommended shape`, plus a `derive_policy()` vs `recommend_shape()`
  comparison. Closes the doc-debt the glossary itself flagged after UPI 041
  shipped.
- **New `state_views` module** — composed read views over `StateDb`
  (`ChurnView::for_subvolume` and `::for_subvolume_default_window`). Three
  inline `drift_samples_for_subvolume` → `drift_row_to_sample` →
  `compute_rolling_churn` dances in `commands/doctor.rs` (×2) and
  `commands/backup.rs` (×1) now call the view directly. Internal refactor;
  no behaviour change. Best-effort per ADR-102 — empty estimate on `None`
  db or query failure.

## [0.20.0] - 2026-05-17

### Added
- **Headroom-aware recommendations** (UPI 044, ADR-115 amendment 2026-05-16).
  `urd doctor --thorough` now adjusts retention recommendations when the
  source pool is shrinking or destination metadata is pressured: Caution
  surfaces an adjustment note ("source pool at N% — applying sooner is
  recommended"); Pressure also tightens the recommended shape (×0.7
  re-clamped); Critical defers to UPI 031's future storage_critical surface.
  Thresholds (25%/15% free, 90/30 days time-to-empty, 0.85/0.92 metadata)
  and the 0.7 tightening multiplier are committed in `src/policy.rs` and
  N=1-calibrated; the post-UPI-044 30-day evidence checkpoint owns
  revision. Recommendation severity is classified per (subvolume, role)
  pair; the storage_critical stub in `src/storage_critical.rs` returns
  `false` and is replaced by UPI 031 when it ships.

### Changed
- **`urd doctor --json` schema bumped to v2** (UPI 044, R3). Adds a
  top-level `schema_version: u32` field and restructures recommendation
  rows: `recommendations.rows[].local` and `.external` changed type from
  `ShapeRecommendation` to `HeadroomAwareRecommendation` (the original
  shape is now nested under `.recommendation`, alongside new `severity`,
  `reason`, `adjusted`, and `adjusted_cost` fields). Consumers reading
  `row.local.role` should migrate to `row.local.recommendation.role`.
  ADR-115 amendment formalizes the schema commitment alongside heartbeat
  and Prometheus contracts (ADR-105).

## [0.19.0] - 2026-05-16

### Fixed
- UPI 045 — Voice evolution pt 1. `urd doctor` and `urd plan` now lead with
  the verdict ("All clear." / "N warnings." / "All sealed." / "N operations
  planned." / "No subvolumes configured.") instead of "Checking Urd health…"
  / "Urd backup plan for {ts}" (Rule 5 contract). The new four-arm plan
  verdict closes Finding 1: a zero-subvolume config no longer renders as
  "All sealed.", and `urd doctor` on a zero-subvolume config surfaces a
  Warning rather than the misleading "All clear." (R-10).
- Issue [#103] — `awareness.rs` no longer labels a recently-unplugged drive
  as "away for N days" when N is the stale-send age. A shared
  `cascade_age_source` primitive consults physical `Unmount` truth first
  (`absent_duration_secs`) and only falls back to the per-caller ops-log
  age, with the source word ("away" vs "last backup") matching the source.

### Changed
- Two `#[ignore]`'d Rule 5 contract stubs in `voice_contract.rs` are now
  active and passing. Three deferred stubs (rule3 / rule4-drive / rule7)
  now point at UPI 045-a as the next owner.

## [0.18.0] - 2026-05-16

### Added
- UPI 043 — pool-level observability. Four new Prometheus gauges
  (`backup_pool_free_bytes`, `backup_pool_metadata_utilization_ratio`,
  `backup_subvolume_local_snapshot_count`,
  `backup_subvolume_estimated_local_pinned_delta_bytes`), heartbeat
  schema v4 with `pools`, `drives`, and per-subvolume `pool_uuid` /
  `local_snapshot_count` / `estimated_local_pinned_delta_bytes` fields.
  Source-pool detection via `findmnt --target`; metadata utilization
  from BTRFS sysfs. Heartbeat schema contract softened to SHOULD/MAY
  semantics (additive forward-compat by serde default). ADR-105 amended;
  companion homelab ADR-021 update in `vonrobak/fedora-homelab-containers`.

## [0.17.0] - 2026-05-15

### Added
- **UPI 042 — Config schema v2 (`monthly = "unlimited"` + yearly tier).** v2
  closes the `monthly = 0` footgun: written explicitly as `monthly =
  "unlimited"` (string) for unbounded retention, or omitted for "no monthly
  retention." A new optional `yearly: u32` retention tier (one snapshot per
  calendar year for `yearly` years) lives alongside the four existing
  granularities. `urd migrate` auto-targets the latest schema in a single
  hop: legacy → v2 or v1 → v2 (replacing the legacy → v1 path). v1 and
  legacy reads continue to interpret `monthly = 0` as unlimited via the
  lenient `MonthlyCount` deserializer; v2 rejects literal `monthly = 0` at
  parse time with an actionable error message. `urd doctor` shows a
  one-line `Schema: v1 (current: v2; run urd migrate to upgrade)` notice
  for non-v2 configs. `urd preflight` (advisory) flags the
  `monthly = "unlimited"` + `yearly > 0` redundancy. See
  ADR-104/105/110/111 amendments (2026-05-15).

### Changed
- `derive_policy()` switches `recorded_external_retention.monthly` from
  unlimited to `Count(0)` (no monthly window). Behavior-neutral because
  `send_enabled = false` for recorded subvolumes means
  `plan_external_retention` is never invoked. Local `recorded` retention
  keeps `monthly = Unlimited` (one snapshot per month indefinitely),
  preserving pre-UPI 042 behavior (ADR-110 amendment).

## [0.16.2] - 2026-05-14

### Fixed
- Bare `urd` no longer reports "1 degraded" and recommends connecting an
  offsite drive when a subvolume's source has had no changes since the last
  send (#120). `compute_health`'s "drive away >7 days" check now honors
  `source_unchanged`, mirroring the planner's skip-when-source-unchanged
  behavior: if the absent drive's pin generation matches the live source,
  there is nothing pending to send and the drive's absence is not an
  operational concern. The promise-status path already had this guard; the
  operational-health path was missing it, which is what produced the
  contradictory "All connected drives are sealed. 1 degraded." output.

## [0.16.1] - 2026-05-14

### Fixed
- Live progress line in `urd backup` no longer latches to the first send's
  subvolume name and `[i/N]` index after subsequent sends start (#118).
  The display thread keyed new-send detection off the `bytes_counter == 0`
  transition inside `RealBtrfs::send_receive`; that window is sub-millisecond
  and the 250 ms poll missed it, so the cached context never refreshed
  while byte/rate values kept updating from later sends. The display now
  uses `send_index` as a generation marker, refreshing cached fields and
  the elapsed-time anchor whenever the executor moves to a new send.

## [0.16.0] - 2026-05-14

### Added
- `urd doctor --thorough` Recommendations section: per-subvolume retention
  shape advice derived from observed churn (UPI 041, ADR-115). New pure
  module `src/policy.rs` projects steady-state data cost and recommends
  a four-slot shape per role. Advisory only; nothing is applied
  automatically. Rows are sorted by recovery magnitude descending and
  suppressed when current matches the suggestion.
- `urd-sentinel.service` systemd user unit shipped alongside the existing
  backup service/timer pair, so the in-binary
  `systemctl --user start urd-sentinel` instruction now works for new
  installs (UPI 044). Type=simple, Restart=on-failure, low-priority
  (Nice=19, IOSchedulingClass=idle).

## [0.15.1] - 2026-05-02

### Fixed
- Non-transient planner now augments `local_snaps` with the just-planned
  snapshot before computing sends, so a freshly-created snapshot ships in
  the same run when latest local already equals latest external (the
  "caught up" state). Mirrors the transient-lifecycle fix from 0f52555.
  Previously, after emergency retention or a generation-equality skip
  caught local and external up, the next run would create a new local
  snapshot and silently defer its send as "already on <drive>", stranding
  the snapshot until the following night's run.

## [0.15.0] - 2026-05-01

### Added
- Drift telemetry (UPI 030, ADR-113 Layer 0): a new `src/drift.rs` pure
  module aggregates per-send wire bytes into a rolling time-windowed
  churn rate, persisted in a new `drift_samples` SQLite table populated
  by a one-shot idempotent backfill from `operations` history on first
  open. Each backup run writes one drift sample per `(run_id, subvolume)`
  derived from the first successful send in plan order. Heartbeat schema
  bumps 2 → 3 with two additive `Option` fields
  (`churn_bytes_per_second`, `last_full_send_bytes`); two new Prometheus
  gauges (`backup_subvolume_churn_bytes_per_second`,
  `backup_subvolume_last_full_send_bytes`) expose the same signal in
  base units. `urd doctor --thorough` gains a Churn section with a
  five-state ladder (cold-start, first measurement, incremental,
  full-send-only first, full-send-only) and a bursty-churn disclaimer.
- Voice contract tests (UPI 035): a new `src/voice_contract.rs` test
  module encodes the seven-rule presentation-layer contract (no
  falsehoods, no contradictions, acknowledged transitions, first-line
  answer, gravity calibration) so future voice changes can't silently
  regress UPI 026's fixes. Surfaces two pre-existing findings as
  `#[ignore]`d tests: `urd doctor` and `urd plan` emit a process-header
  first line rather than the verdict/operation count. Lifts shared test
  fixtures (`test_status_output`, `test_backup_summary`,
  `test_doctor_output`, `test_verify_output`, `test_plan_output`) into
  `voice::test_fixtures` for reuse across test modules.

### Changed
- `colored::control::set_override` calls in `voice.rs` and
  `voice_events.rs` tests now route through a shared `color_guard()`
  Mutex-backed helper, eliminating the parallel-test colour-override
  race that the new contract tests would otherwise hit.

## [0.14.0] - 2026-05-01

### Added
- Structured event log (ADR-114): typed `Event` records of decisions and
  state transitions persisted to a new `events` SQLite table. Pure modules
  (planner, retention, awareness, sentinel state machine) emit events as
  part of their output; impure callers persist them best-effort. Visible
  via the new `urd events` subcommand with `--since`, `--kind`,
  `--subvolume`, `--drive`, `--limit`, and `--json` filters. Drive
  connection records and four Prometheus counter families
  (circuit-breaker trips, full sends by reason, deferrals by scope,
  prunes by rule) now derive from the same table.

### Changed
- Quality gate command updated to `cargo clippy --all-targets` (covers
  test code, which bare clippy skips). CLAUDE.md adds a
  `cargo check --all-targets` line specifically for post-mass-edit
  verification — the bare form misses lints and type errors that live
  in `mod tests` / `tests/`.

## [0.13.0] - 2026-04-21

### Added
- `BackupSummary.notes` — separate output channel for by-design informational
  outcomes (not warnings). Currently carries the space-guard message; future
  similar advisory outputs will land here.
- One-time post-upgrade acknowledgment shown to returning users whose
  previously-reported `blocked` states become `healthy`. Appears above
  `urd status` / `urd backup` / `urd` output once, then never again. Fresh
  installs see nothing.

### Fixed
- Incremental space estimates no longer use the full subvolume's calibrated
  size — fixes false `blocked` health reports on healthy incremental chains
  where the actual delta fits comfortably.
- Drive operation-type queries now use the correct schema strings; previously
  dead size-estimate fallback tiers now activate.
- Drive `away` duration now sources from drive connection events rather than
  last send age — a freshly unplugged drive no longer reports `away 3d`
  based on the last successful backup timestamp. When no connection event
  is available, the status line shows `last backup Nd ago` instead.
- Informational cleanup outcomes no longer render as warnings. "Space
  recovered — N skipped deletion(s)" is replaced with a dimmed note
  "space guard held — N snapshot(s) retained."

### Changed
- Homelab monitoring consumers: the `warnings` bucket semantically narrowed
  (the cleanup `space recovered` string moves to `notes`). No currently
  shipped homelab alert is affected; see the homelab ADR-021 update for
  future JSON-consumer precedent.
- Unmounted drive labels use the word `away` uniformly; the severity
  escalation (yellow, `protection aging`) is carried by color and suffix,
  not by a separate word.

## [0.12.2] - 2026-04-17

### Fixed
- Daily timer drift no longer silently drops snapshots and sends. Interval
  checks now apply a grace tolerance (5% of interval, capped at 15 minutes),
  so a daily run firing a few minutes earlier than yesterday's snapshot still
  creates today's snapshot instead of waiting another day.
- Status no longer reports UNPROTECTED for subvolumes whose source has not changed since the last successful send. Awareness now compares the source's BTRFS generation against the pin snapshot's generation and overrides age-based freshness when they match. Applies to both external send status and local snapshot status. The external override additionally requires the pin snapshot to still exist on the drive when the drive is mounted, so that drives whose data was destroyed externally do not mask as PROTECTED. Fails open — if generation queries error, falls back to the previous age-only assessment.

## [0.12.1] - 2026-04-06

### Fixed
- Transient subvolumes no longer create orphaned snapshots when no drives can receive sends
- Transient subvolumes no longer create snapshots when send interval hasn't elapsed

## [0.12.0] - 2026-04-05

### Added
- Findings-first verify: `urd verify` now shows problems first and collapses OK checks into a summary; `--detail` restores verbose output
- Doctor trust gap fix: `urd doctor` no longer says "All clear" when degraded subvolumes exist — shows "N subvolumes degraded. Data is safe — drives are absent."
- Doctor `--thorough` threads section separates findings from expected conditions (absent drives collapsed into summary line)
- Actionable suggestions on verify chain-break findings (`suggestion` field on verify checks)
- Relative timestamps in `urd status` — "10h ago" instead of raw ISO 8601
- Status summary line now names all absent drives (up to 3, then "and N more")
- Guided subvolume chooser for `urd retention-preview` — sorted list with usage hint instead of comma-separated error dump

### Changed
- Doctor verdict text uses proper pluralization and removes misleading "Run suggested commands to resolve"
- "ext-only" thread label renamed to "drive-only" for clarity
- "protection degrading" vocabulary replaced with "protection aging" for absent drives
- Zero-duration history runs show "<1s" instead of "0s"
- Drives table TOKEN column uses ASCII text (ok/MISMATCH/MISSING) instead of Unicode symbols for portable alignment

## [0.11.1] - 2026-04-05

### Fixed
- Transient snapshots accumulating for absent drives — retention now only protects pins from mounted drives, preventing space exhaustion on constrained filesystems
- Sentinel "all N chains broke" phrasing — detection now reports actual broken count and fires on 2+ broken chains (delta-based), not only when all chains break
- "send disabled" skip text for local-only subvolumes replaced with "local only"

### Added
- Transient snapshot creation skipped when no drives are available for send (defense-in-depth)

## [0.11.0] - 2026-04-05

### Added
- Compressed send pass-through: auto-detects `--compressed-data` support (btrfs-progs 5.18+) and enables protocol v2 sends — less CPU, preserves compression on destination
- Post-delete sync: `btrfs subvolume sync` after each retention delete ensures freed space is visible to the space check before the next snapshot
- Context-aware suggestions: `urd doctor`, `urd status`, and bare `urd` now show specific commands based on chain health, drive state, and subvolume config instead of static "run `urd backup`" advice
- Sentinel config reload: daemon detects config file changes via mtime polling and hot-reloads without restart
- Token-aware chain-break gate: verified drives proceed with full sends in auto mode, breaking the deadlock where broken chains permanently blocked transient subvolumes
- `send_completed` field in heartbeat (schema v2): distinguishes "backup ran successfully" from "data actually reached an external drive"
- `SendType::Deferred` (Prometheus metric value 3): distinguishes intentional no-send from blocked-by-gate deferral
- Deferred synthesis in backup summary: subvolumes with no local snapshots to send now surface actionable guidance instead of silent skips
- `SkipCategory::NoSnapshotsAvailable` for structured classification of send-blocked skips
- External-only runtime: subvolumes with `local_snapshots = false` no longer show false "degraded" health or "broken chain" warnings — status table shows em-dash for LOCAL and "ext-only" for THREAD, plan output uses `[EXT]` skip tag
- Skip unchanged subvolumes: compares BTRFS generation counters to avoid creating identical snapshots for quiet subvolumes — shown as `[SAME]` in plan output with elapsed time, overrideable via `--force-snapshot`
- `urd emergency` command: guided emergency space recovery — assesses snapshot roots, previews aggressive thinning (keep latest + pinned only), executes with confirmation
- Automatic emergency pre-flight: backup command detects critically low space (<50% of `min_free_bytes`) and runs emergency retention under the advisory lock before planning
- Doctor space trend warning: `urd doctor` warns when snapshot roots approach free-space thresholds, suggests `urd emergency`
- Shared pin re-check helper (`chain::is_pinned_at_delete_time`): single implementation of ADR-106 defense-in-depth layer 3, used by executor and emergency paths

### Fixed
- False "all chains broke simultaneously" anomaly when a drive disconnects (total=0 was treated as all-broken)
- Duplicate default config path logic in `urd migrate` consolidated to single implementation

## [0.10.0] - 2026-04-03

### Added
- `local_snapshots = false` in v1 config — replaces `local_retention = "transient"` with a clear boolean opt-out of local snapshot history
- `urd migrate` command — transforms legacy config to v1 schema with backup, dry-run, and semantic equivalence (no behavioral changes)
- V1 example config at `config/urd.toml.v1.example`
- Serialize support on all config types — enables `urd migrate` and config round-tripping
- V1 config schema parser with `config_version = 1` dispatch — self-describing subvolumes, no `[defaults]`/`[local_snapshots]` sections
- V1 validation: named protection levels reject operational overrides, enforce drive requirements
- `snapshot_root` and `min_free_bytes` fields on `ResolvedSubvolume` — eliminates per-call Config lookups in planner and awareness

### Fixed
- `urd migrate` partial retention overrides on named levels now bake all four fields (hourly/daily/weekly/monthly) — previously, unspecified fields silently inherited from v1 synthesized defaults instead of the derived level's values

## [0.9.1] - 2026-04-03

### Changed
- Protection level vocabulary: guarded→recorded, protected→sheltered, resilient→fortified — names now describe what the data *becomes*, not a generic safety adjective
- ADR-111 revised with complete v1 schema specification, field tables, migration spec, and validation error messages
- ADR-110 updated with new level names and implementation gate progress

## [0.9.0] - 2026-04-03

### Added
- `urd drives` subcommand — list configured drives with status, token state, free space, and role
- `urd drives adopt <label>` — accept a drive into Urd's identity system (reset token relationship)
- Drive reconnection notifications via Sentinel — desktop alert when an absent drive returns
- Identity-aware reconnection: drives with token issues get "needs adoption" notification instead of false "all clear"

### Changed
- TokenExpectedButMissing error messages now direct users to `urd drives adopt` instead of `urd doctor`

## [0.8.2] - 2026-04-03

### Fixed
- Safety gate (chain-break full send blocked) now reports `DEFERRED` instead of `FAILED` — the tool made a correct decision, not an error
- Deferred-only backup runs report "success" instead of "failure" in summary, heartbeat, and metrics
- `urd doctor --thorough` stale-pin message changed from accusatory "sends may be failing" to neutral "last successful send was N day(s) ago"
- `urd doctor` no longer suggests adding a UUID that's already configured on another drive (cloned drive scenario)

## [0.8.1] - 2026-04-03

### Fixed
- `urd status` no longer shows false degradation for subvolumes scoped to specific drives (assess ignored per-subvolume `drives` field)
- Cloned or swapped drives with missing identity tokens are now blocked from receiving sends (TokenExpectedButMissing safety gate)

### Changed
- Local-only subvolumes (`send_enabled = false`) display as `[LOCAL]` instead of `[OFF] Disabled` in plan output
- Local-only subvolumes suppressed from backup summary skip section (they're complete, not skipped)
- `urd plan` and `urd backup --dry-run` show `[WARNING]` for drives with token identity issues

## [0.8.0] - 2026-04-02

### Added
- `/steve` skill: Steve Jobs product vision and UX quality gatekeeper — reviews brainstorms, designs, and finished features from the user's perspective
- `urd backup` now acts immediately — fresh snapshots and sends without waiting for intervals. Automated runs use `--auto` to respect interval gating.
- Pre-action briefing shown before manual backups: "Backing up everything to WD-18TB. 7 snapshots, 7 sends, ~53.0GB"
- Mode-aware empty-plan messages explain why nothing was backed up and suggest fixes

### Changed
- `urd plan` shows the manual (no-interval) view by default; `urd plan --auto` shows the timer view
- Lock trigger string changed from "timer" to "auto"/"manual" for clearer diagnostics

## [0.7.1] - 2026-04-01

### Fixed
- Btrfs receive stdout ("At snapshot ...") no longer leaks into terminal during sends
- Backup progress display: completion lines now print synchronously from executor, fixing race where wrong subvolume names and missing completions appeared for fast sends
- `[preflight]` internal prefix removed from user-facing backup warnings

### Changed
- Default command: "All sealed." → "All connected drives are sealed." with health degradation surfacing
- Status table: PROTECTION column hidden unless exposure conflicts with promise; disconnected drive columns collapsed; RECOVERY column hidden (showed policy, not actual depth)
- Backup skipped section: only absent drives and actionable skips shown; [WAIT] and [OFF] suppressed
- Doctor warnings include concrete numbers (e.g., "snapshot_interval (1w) exceeds guarded requirement (1d)") with fix suggestions
- UUID missing warning moved from runtime log to `urd doctor` check
- Log output (WARN level) suppressed on interactive TTY; structured presentation layer handles all user-facing warnings

## [0.7.0] - 2026-04-01

### Added
- Staleness escalation: disconnected drives show graduated urgency text based on awareness promise status (PROTECTED → minimal, AT RISK → "consider connecting", UNPROTECTED → "protection degrading")
- Next-action suggestions: context-specific dimmed hints after `urd status`, `urd plan`, `urd backup`, `urd verify`, and bare `urd` (silence when healthy)
- Structured redundancy advisory system: detects no-offsite-protection, offsite-drive-stale (>30 days), single-point-of-failure, and transient-no-local-recovery gaps
- REDUNDANCY section in `urd status` with per-advisory observation and suggestion
- `advisory_summary` field in sentinel state file (schema v3) for Spindle tray icon integration
- `urd retention-preview` command: shows recovery windows, disk estimates, and transient/graduated comparison for retention policies
- RECOVERY column in `urd status` table showing compact retention summary per subvolume (e.g., "31d / 7mo / ∞")
- `urd doctor` command: unified health check composing config, infrastructure, awareness, sentinel, and optional thread verification (`--thorough`)
- Mythic voice on backup transitions: brief event-aware lines when threads are mended, first sends established, promises recovered, or all subvolumes reach sealed

### Changed
- Offsite cycling advisory migrated from stringly-typed 7-day threshold to structured `OffsiteDriveStale` with 30-day threshold

## [0.6.0] - 2026-04-01

### Added
- Bare `urd` (no subcommand) shows a one-sentence status: "All sealed. Last backup 7h ago." or "3 of 9 sealed. htpc-root exposed." First-time users see setup guidance instead of help text
- `urd completions <shell>` generates tab-completion scripts for bash, zsh, fish, elvish, and powershell
- `StateDb::last_run_info()` shared helper for building presentation-ready last-run summaries
- Transient immediate cleanup: executor deletes old pin parent immediately after successful send to all drives, reducing local snapshot count from two to one between runs
- `Config::drive_labels()` helper for collecting configured drive labels
- Promise redundancy encoding: resilient protection level now requires at least one offsite-role drive and degrades promise status when the offsite copy goes stale (30/90-day thresholds)
- Preflight check `resilient-without-offsite` warns when resilient subvolume lacks an offsite drive
- Offsite drive role shown as "(offsite)" annotation in `urd status` table column headers
- `DriveRole` plumbed through `DriveAssessment`, `StatusDriveAssessment`, `DriveInfo`, and `InitDriveStatus`

### Changed
- Vocabulary overhaul: safety labels are now sealed/waning/exposed, chain→thread, mounted→connected/disconnected/away, SAFETY→EXPOSURE, CHAIN→THREAD, PROMISE→PROTECTION column headers
- CLI command descriptions rewritten to intent-first language (e.g., "Check whether your data is safe")
- Summary line now differentiates exposure levels: "htpc-root exposed. docs waning." instead of generic "needs attention"
- Skip tags differentiated by category: [WAIT], [AWAY], [SPACE], [OFF], [SKIP] replace overloaded [SKIP]
- Drive status is now role-aware: offsite drives show "away" when disconnected, primary drives show "disconnected"
- Notification mythology cleaned up: loom/weave→spindle/thread, rewoven→mended, unguarded→exposed
- `UrdError::Chain` error message changed from "Chain error" to "Thread error" (log grep patterns may need updating)

### Fixed
- 7-day "consider cycling" advisory now scoped to offsite-role drives only (previously fired for all unmounted drives)

## [0.5.0] - 2026-03-30

### Added
- Transient local retention mode (`local_retention = "transient"`): delete local snapshots after external send, keep only pinned chain parents for incremental sends
- Preflight checks for transient misconfiguration (transient without send, transient with named protection level)

### Fixed
- Awareness model now understands transient retention: local status defers to external send freshness instead of falsely reporting UNPROTECTED

## [0.4.3] - 2026-03-30

### Added
- Sentinel tracks health transitions and fires HealthDegraded/HealthRecovered notifications
- `visual_state` block in sentinel-state.json: icon state and safety/health counts for tray icon consumers
- Per-subvolume health and health_reasons in sentinel state file promise states
- Sentinel state file schema version 2 (backward-compatible with v1)

### Changed
- Generic `NamedSnapshot` trait replaces duplicated change-detection logic for promise and health axes
- All-blocked health escalates to Critical icon (was Warning)

## [0.4.2] - 2026-03-30

### Added
- Sentinel detects simultaneous chain breaks on a drive and notifies (hardware swap signal)
- `FullSendReason` annotation on full sends: `first send`, `chain broken`, or `no pin`
- Full-send gate in autonomous mode: chain-break sends are blocked unless `--force-full` is passed
- Drive token verification wired into backup path (filters sends to token-mismatched drives)

## [0.4.1] - 2026-03-30

### Added
- Rich progress display during backup: subvolume name, send counter, completion trail, and ETA for full sends
- Estimated send sizes in `urd plan` output with three-tier fallback (same-drive > cross-drive > calibrated)
- Qualified summary totals: `"6 sends (~623 GB total)"` or `"estimated for 4 of 6"` when partial
- Cross-drive fallback for send size estimation (covers drive swap scenarios)
- Structural headings in `urd plan` output (operations and skipped sections)
- Collapsed skip reasons: grouped by category instead of 20+ individual lines
- `SkipCategory` enum with structured classification in JSON daemon output

### Fixed
- Planner space check now uses cross-drive fallback (previously only same-drive history)

## [0.4.0] - 2026-03-29

### Added
- Drive session tokens for hardware swap detection (`.urd-drive-token` identity files)
- Chain health computation in awareness model (incremental chain intact/broken per drive)
- Two-axis status display: data safety (OK/aging/gap) + operational health (healthy/degraded/blocked)
- Temporal context in status table: snapshot counts show age (e.g., "47 (30m)", "12 (2h)")
- Unmounted drives shown as "away" in status table when they have send history
- Notification deduplication: backup defers to sentinel when daemon is running
- Drive connection recording in SQLite (mount/unmount events with typed enums)

### Changed
- README rewritten for public repository
- Status command derives chain health from awareness assessment instead of recomputing
- `SentinelStateFile::read()` moved from output.rs to sentinel_runner.rs (ADR-108)
- Sentinel initial assessment log differentiates missing heartbeat (first-run)

## [0.3.0] - 2026-03-27

### Added
- Sentinel daemon: pure state machine with event-driven transitions and circuit breaker
- Sentinel I/O runner and `urd sentinel` CLI command
- Protection promise model (ADR-110): typed promise levels with derivation function
- Notification dispatcher with promise-state-driven alerts
- Awareness model: pure function computing promise states per subvolume
- Heartbeat file: JSON health signal written after every backup run
- Presentation layer: structured output with interactive/daemon rendering and mythic voice
- `urd get` command for file restore from snapshots
- UUID drive fingerprinting: verify drive identity before sending snapshots
- Post-backup structured summary and local space guard
- Pre-flight config consistency checks
- Structured error types with actionable btrfs error translation
- Lock extraction module with shared advisory locking

### Changed
- Voice migration initiated: presentation logic moving to voice.rs
- Config system design review and ADR suite update (ADR-110, ADR-111)

### Fixed
- Pre-cutover hardening: mkdir before btrfs receive, legacy pin file accuracy
- Space estimation queries drive mount path instead of per-subvolume directory
- Phase 4 adversary review findings addressed

## [0.2.0] - 2026-03-24

### Added
- Phase 4: cutover polish and review-driven fixes
- Pre-send space estimation with real-world testing
- Failed-send byte tracking, progress display, and `urd calibrate` command
- Documentation system, CONTRIBUTING.md, and project status tracking
- Operating guide covering build, install, update, and daily use
- Vision document, brainstorm synthesis, and architecture-grounded roadmap
- Founding ADRs formalized (ADR-100 through ADR-109)

### Fixed
- Systemd backup timer shifted to 04:00

## [0.1.0] - 2026-03-22

### Added
- Initial project scaffold with config, types, and example configuration
- Phase 1: config parsing, retention logic, planner, and `urd plan` CLI
- Phase 2: executor, SQLite state database, Prometheus metrics, `urd backup` command
- Phase 3: `urd status`, `urd history`, `urd verify` commands, systemd units
- Phase 3.5: hardening for production cutover
- BtrfsOps trait abstracting all btrfs subprocess calls
- Interval-based scheduling for snapshots and sends
- Graduated retention policy (hourly/daily/weekly/monthly thinning)
- Defense-in-depth pin file protection for unsent snapshots
- Per-subvolume error isolation in executor

[Unreleased]: https://github.com/vonrobak/urd/compare/v0.24.1...HEAD
[0.24.1]: https://github.com/vonrobak/urd/compare/v0.24.0...v0.24.1
[0.24.0]: https://github.com/vonrobak/urd/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/vonrobak/urd/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/vonrobak/urd/compare/v0.21.2...v0.22.0
[0.21.2]: https://github.com/vonrobak/urd/compare/v0.21.1...v0.21.2
[0.21.1]: https://github.com/vonrobak/urd/compare/v0.21.0...v0.21.1
[0.21.0]: https://github.com/vonrobak/urd/compare/v0.20.5...v0.21.0
[0.20.5]: https://github.com/vonrobak/urd/compare/v0.20.4...v0.20.5
[0.20.4]: https://github.com/vonrobak/urd/compare/v0.20.3...v0.20.4
[0.20.3]: https://github.com/vonrobak/urd/compare/v0.20.2...v0.20.3
[0.20.2]: https://github.com/vonrobak/urd/compare/v0.20.1...v0.20.2
[0.20.1]: https://github.com/vonrobak/urd/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/vonrobak/urd/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/vonrobak/urd/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/vonrobak/urd/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/vonrobak/urd/compare/v0.16.2...v0.17.0
[0.16.2]: https://github.com/vonrobak/urd/compare/v0.16.1...v0.16.2
[0.16.1]: https://github.com/vonrobak/urd/compare/v0.16.0...v0.16.1
[0.16.0]: https://github.com/vonrobak/urd/compare/v0.15.1...v0.16.0
[0.15.1]: https://github.com/vonrobak/urd/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/vonrobak/urd/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/vonrobak/urd/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/vonrobak/urd/compare/v0.12.2...v0.13.0
[0.12.2]: https://github.com/vonrobak/urd/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/vonrobak/urd/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/vonrobak/urd/compare/v0.11.1...v0.12.0
[0.11.1]: https://github.com/vonrobak/urd/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/vonrobak/urd/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/vonrobak/urd/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/vonrobak/urd/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/vonrobak/urd/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/vonrobak/urd/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/vonrobak/urd/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/vonrobak/urd/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/vonrobak/urd/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/vonrobak/urd/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/vonrobak/urd/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/vonrobak/urd/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/vonrobak/urd/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/vonrobak/urd/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/vonrobak/urd/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/vonrobak/urd/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/vonrobak/urd/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/vonrobak/urd/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/vonrobak/urd/releases/tag/v0.1.0
