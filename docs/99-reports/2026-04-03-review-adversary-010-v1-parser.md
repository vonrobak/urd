# Architectural Adversary Review: UPI 010 Session 3 — V1 Parser + ResolvedSubvolume Migration

**Date:** 2026-04-03
**Reviewer:** arch-adversary
**Scope:** `src/config.rs` (V1 parser, synthesized LocalSnapshotsConfig, ResolvedSubvolume enrichment), `src/plan.rs` (caller migration), `src/awareness.rs` (caller migration), test config construction in `src/preflight.rs`, `src/heartbeat.rs`, `src/commands/backup.rs`
**Context:** Urd is the sole backup system for a Linux workstation. Catastrophic failure mode is silent data loss. 790+ tests, clippy clean, v0.9.1.

---

## Scoring

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Synthesized LocalSnapshotsConfig is sound. Two validation gaps identified. |
| Safety | 8/10 | Fail-open for snapshot_root=None in plan.rs (returns error, good). min_free_bytes conflict is undetected. |
| Backward compatibility | 9/10 | Legacy configs parse identically. Round-trip tests pass. config_version=None preserved. |
| Completeness | 7/10 | Missing test coverage for several edge cases. Sheltered validation has a gap. |
| Clarity | 9/10 | Code is well-structured. V1Config::into_config() is readable. Validation messages are actionable. |
| Architectural alignment | 9/10 | Correctly follows plan/execute separation. Pure modules stay pure. Synthesized config is a clean bridge. |

---

## Findings

### Finding 1: Sheltered validation does not check `drives = []` (empty vec)

**Severity: HIGH**

The sheltered check at line 706 only tests `self.drives.is_empty()` — whether there are zero globally configured drives. It does not check whether the subvolume itself has `drives = Some(vec![])` (an empty drives list), which would effectively mean the subvolume has no drives assigned.

A v1 config with `protection = "sheltered"` and `drives = []` passes validation, but the subvolume will never send to any drive. The derived policy sets `send_enabled = true`, but the effective drive set in plan.rs and awareness.rs filters to zero drives. This is a promise violation: the user declared "sheltered" but gets no external protection.

```toml
[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
protection = "sheltered"
drives = []         # <-- passes validation, defeats the promise
```

**Recommendation:** In `validate_v1()`, after the sheltered check, add: if `sv.drives` is `Some(ref d)` and `d.is_empty()`, reject with an error. Also check that `drives = []` on fortified subvolumes cannot bypass the offsite check — currently the fortified check handles `Some(ref sv_drives)` by checking if any are offsite, and `any()` on an empty iterator returns `false`, so it would be caught. But add a test to confirm.

**Also applies to:** The sheltered check should mirror the fortified check's pattern — check `sv.drives` when present, fall back to global drives when absent. Currently sheltered only checks global drives even when the subvolume restricts to a subset.

---

### Finding 2: Conflicting `min_free_bytes` across subvolumes in the same root silently uses first-wins

**Severity: MEDIUM**

In `V1Config::into_config()` (line 590-593), when two subvolumes share a `snapshot_root` but declare different `min_free_bytes`, the code uses "first non-None wins" with no warning. This depends on HashMap iteration order (which is non-deterministic in Rust).

```toml
[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
min_free_bytes = "50GB"
```

Which value wins is undefined. The user has no way to know, and the wrong value could allow space exhaustion (if 10GB wins instead of 50GB) or premature space-pressure retention (if 50GB wins when 10GB was intended).

**Recommendation:** Either (a) reject configs where subvolumes in the same root have different `min_free_bytes` values in `validate_v1()`, or (b) take the maximum value (fail-closed: more conservative threshold). Option (a) is cleaner and forces the user to be explicit.

---

### Finding 3: `VersionProbe` pre-parse can succeed on structurally invalid TOML

**Severity: LOW**

`extract_config_version()` uses `toml::from_str::<VersionProbe>()` which is lenient — it ignores unknown fields. A TOML file with `config_version = 1` in `[general]` but with structurally broken content elsewhere (missing required fields, wrong types) will extract version 1, then fail in `parse_v1()`. This is the intended behavior.

However, there is a subtler issue: a TOML file that is syntactically invalid (e.g., unclosed quotes, bad escapes) will fail `extract_config_version()` with an unhelpful error message from the toml parser, prefixed with "failed to read config_version:". The user sees a TOML syntax error attributed to "config_version reading" which is confusing.

**Recommendation:** Consider catching TOML syntax errors at the top of `Config::load()` before version dispatch, so the error message says "invalid TOML syntax" rather than "failed to read config_version: ...". This is a UX polish item, not a correctness issue.

---

### Finding 4: HashMap ordering makes synthesized root order non-deterministic

**Severity: LOW**

`V1Config::into_config()` collects `root_map.into_iter()` into a `Vec<SnapshotRoot>`. HashMap iteration order is non-deterministic. While the current validation and lookup code (`snapshot_root_for()`, `root_min_free_bytes()`) iterates all roots and doesn't depend on ordering, this could cause:

- Non-deterministic output in serialized configs (if a v1-loaded config is ever re-serialized)
- Non-deterministic `resolved_subvolumes()` ordering within the same priority level (since sort is by priority only, ties preserve insertion order)

Neither is a correctness issue today, but it introduces test fragility. The existing test `v1_synthesizes_local_snapshots_config` checks `roots.len() == 1` which avoids the ordering issue, and `v1_multiple_snapshot_roots` checks by path lookup which is order-independent.

**Recommendation:** Use `BTreeMap` instead of `HashMap` for `root_map`, or sort the resulting `Vec<SnapshotRoot>` by path. This costs nothing and makes the output deterministic.

---

### Finding 5: No test for v1 config that fails `Config::validate()` (post-conversion)

**Severity: MEDIUM**

`validate_v1()` enforces v1-specific rules. `Config::validate()` enforces structural rules (unique names, path safety, drive references, root assignment). After `V1Config::into_config()` converts to `Config`, the result goes through `Config::validate()` in `Config::load()`.

There is no test that exercises the post-conversion `Config::validate()` path for v1 configs. For example:
- A v1 config with `drives = ["NONEXISTENT"]` would pass `validate_v1()` but fail `Config::validate()` at the drive label check.
- A v1 config with `source = "relative/path"` would fail path validation.
- A v1 config with `name = "foo/bar"` would fail name safety.

These should work correctly because `Config::validate()` is well-tested for legacy configs, but the v1 conversion path has no coverage for this. Since this is the sole backup system, the test gap matters.

**Recommendation:** Add at least one test that constructs a v1 config string that passes `validate_v1()` but fails `Config::validate()` (e.g., `drives = ["NONEXISTENT"]`). This confirms the full validation chain works end-to-end through the v1 path.

Note: The existing tests call `parse_v1()` directly, which calls `validate_v1()` but NOT `Config::validate()` or `expand_paths()`. The full chain is only exercised through `Config::load()`, which reads from disk. Consider adding a `Config::load_from_str()` for testing, or at minimum test that `parse_v1()` output passes `config.validate()` after `expand_paths()`.

---

### Finding 6: No equivalence test between v1 and legacy configs

**Severity: MEDIUM**

The plan explicitly calls for: "Add test: plan with v1 config produces correct operations" and "test that a v1 config produces the same `resolved_subvolumes()` as an equivalent legacy config."

Neither exists. `v1_resolves_subvolumes_correctly` tests that a v1 config resolves, but does not compare against an equivalent legacy config. Without this, there is no proof that the synthesized `LocalSnapshotsConfig` + `DefaultsConfig` produce identical behavior to a hand-written legacy config.

**Recommendation:** Write a test that defines both a legacy config string and an equivalent v1 config string, parses both, calls `resolved_subvolumes()` on each, and asserts structural equality (ignoring field ordering). This is the single most valuable missing test.

---

### Finding 7: `plan_local_retention` uses `subvol.min_free_bytes.unwrap_or(0)` — safe but semantically imprecise

**Severity: LOW**

At `plan.rs` line 411, `subvol.min_free_bytes.unwrap_or(0)` converts `None` to `0`. The CLAUDE.md coding conventions state: "Fallback values must be safe, not just convenient. `unwrap_or(0)` is wrong when 0 is in-range but semantically meaningless."

Here, `min_free = 0` means "no threshold," which disables space-pressure retention (`min_free > 0 && free_bytes < min_free` is always false when `min_free == 0`). So `0` is semantically correct as "disabled." The same pattern appears at line 151 for `plan_local_snapshot`.

This is technically compliant because `0` genuinely means "no minimum" — it's the intended semantic, not a meaningless default. But it would be clearer to use an explicit check: `if let Some(min_free) = subvol.min_free_bytes { ... }`.

**Recommendation:** No change required. The current code is correct. Consider refactoring to `Option`-based checks in a future simplification pass if desired, but this is not a defect.

---

### Finding 8: Sheltered validation checks global drives but not the subvolume's effective drive set

**Severity: HIGH** (extension of Finding 1)

The sheltered check at line 706 checks `self.drives.is_empty()`. But a sheltered subvolume with `drives = ["D1"]` where "D1" is a primary drive (not offsite) technically satisfies the sheltered promise — sheltered only requires one external backup, not an offsite one. This is correct.

However, the check does NOT verify that the subvolume's assigned drives actually exist in `self.drives`. A v1 config with:

```toml
protection = "sheltered"
drives = ["NONEXISTENT"]
```

...passes `validate_v1()` (because `self.drives` is non-empty). It would later fail `Config::validate()` at the drive reference check, so this is caught — but the error message will be about "unknown drive" rather than "sheltered requires drives," which is less helpful.

More critically: if `drives` is `None` (not specified), sheltered falls through to all global drives. If `drives` is `Some(vec!["D1"])` where D1 exists, it works. The gap is only `drives = Some(vec![])` (empty vec, Finding 1) and the error message quality for invalid drive references.

**Recommendation:** Already covered by Finding 1's fix. The empty-vec case is the real gap. Invalid drive labels are caught downstream.

---

### Finding 9: V1 `DefaultsConfig` hardcodes retention values instead of referencing `derive_policy`

**Severity: LOW**

In `V1Config::into_config()` (lines 606-623), the synthesized `DefaultsConfig` hardcodes retention values that happen to match `full_retention` from `derive_policy()`. If `derive_policy()` values change in the future, the v1 defaults will diverge silently.

The comment says "Values match full_retention from derive_policy() in types.rs" — this is documentation of intent but not enforcement.

**Recommendation:** Consider calling `derive_policy(ProtectionLevel::Sheltered, RunFrequency::Timer { interval: Interval::days(1) })` to get the values programmatically, rather than hardcoding them. Alternatively, add a test that asserts the hardcoded values match `derive_policy` output. This prevents silent divergence.

---

### Finding 10: `plan.rs` error handling for missing `snapshot_root` returns `Err` — verify all callers handle this

**Severity: LOW**

At `plan.rs` line 130-134, a missing `snapshot_root` returns `Err(UrdError::Config(...))`. This aborts the entire plan, not just the single subvolume. This violates architectural invariant #4: "Individual subvolume failures never abort the run."

In `awareness.rs` (line 198-219), the same condition correctly handles it per-subvolume by pushing an Unprotected assessment and continuing. The plan.rs approach is inconsistent.

In practice, `snapshot_root` should never be `None` after `resolved_subvolumes()` enrichment (since `Config::validate()` ensures every subvolume is in a root). But if it were `None` — perhaps from a manually constructed test config with an empty roots vec — the planner would abort all subvolumes instead of skipping one.

**Recommendation:** Change plan.rs to skip the subvolume with an error in `skipped` rather than returning `Err`. This aligns with awareness.rs behavior and invariant #4. The current code is safe (it can't happen after validation) but violates the stated architectural contract.

---

## Summary

The implementation is solid. The synthesized `LocalSnapshotsConfig` approach is correct and well-tested for the happy path. The version dispatch is clean and the legacy path is untouched. The caller migration in plan.rs and awareness.rs is complete and correct.

The two HIGH findings (both relating to `drives = []` on sheltered subvolumes) represent the only path to silent promise violation. The MEDIUM findings are test coverage gaps that should be addressed before merge, given that this is the sole backup system and the catastrophic failure mode is silent data loss.

**Verdict: Address Findings 1, 2, 5, and 6 before merge. The rest can be deferred.**
