//! The backup run's drive-token gating (UPI 059-b) and retention-change
//! baseline recording (ADR-110): the token probes at the I/O boundary, the
//! pure classification and plan mutation, and the run-end retention-shape
//! record the next run's gate compares against.

use crate::commands::plan_cmd::BaselineRead;
use crate::config::Config;
use crate::drives;
use crate::plan::{BackupPlan, PlannedOperation};
use crate::state::StateDb;
use crate::types::DriveLabel;

/// Probe every mounted drive's identity token — the I/O half of token gating
/// (UPI 059-b): whether its token file is readable, and its availability
/// verified against the state DB. Operator warnings for a suspicious drive are
/// logged here, at the I/O boundary; the classification is the pure
/// [`resolve_token_gating`].
pub(super) fn probe_drive_tokens(
    config: &Config,
    db: &StateDb,
) -> Vec<(DriveLabel, drives::DriveAvailability, bool)> {
    config
        .drives
        .iter()
        .filter(|d| drives::is_drive_mounted(d))
        .map(|drive| {
            // Pre-check: can we read the token file?
            let has_readable_token = matches!(drives::read_drive_token(drive), Ok(Some(_)));
            let avail = drives::verify_drive_token(drive, db);
            // Operator warnings stay at the I/O boundary.
            match &avail {
                drives::DriveAvailability::TokenMismatch { expected, found } => {
                    log::warn!(
                        "Drive {} has a token mismatch (expected {}, found {}) — \
                         skipping sends to this drive",
                        drive.label, expected, found,
                    );
                }
                drives::DriveAvailability::TokenExpectedButMissing => {
                    log::warn!(
                        "Drive {} is mounted but missing its identity token. Urd has \
                         previously sent to a drive with this label — this may be a \
                         different physical drive. Sends to {} are blocked. \
                         Run `urd drives adopt {}` to accept this drive.",
                        drive.label, drive.label, drive.label,
                    );
                }
                _ => {}
            }
            (drive.label.clone(), avail, has_readable_token)
        })
        .collect()
}

/// Result of classifying drive token probes: which drive labels are blocked
/// from receiving sends, and which have a confirmed identity.
///
/// `blocked` — token mismatch or expected-but-missing: a clone or a swap is
/// suspected, so sends are held back (but retention deletes still proceed).
/// `verified` — token file readable and matching: the executor's chain-break
/// gate may proceed for these drives.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TokenGating {
    blocked: std::collections::BTreeSet<DriveLabel>,
    verified: std::collections::BTreeSet<DriveLabel>,
}

/// Classify drive token probes into blocked and verified labels (pure).
///
/// Each probe is `(drive_label, availability, has_readable_token)` — the
/// I/O results gathered by [`probe_drive_tokens`]. The classification mirrors
/// the verify semantics: a drive is "verified" only when its token file is readable AND
/// the stored token matches, which excludes fail-open paths (unreadable token
/// file) from being treated as verified. Operator warnings stay in
/// [`probe_drive_tokens`] at the I/O boundary — this function does no logging.
#[must_use]
pub(super) fn resolve_token_gating(
    probes: &[(DriveLabel, drives::DriveAvailability, bool)],
) -> TokenGating {
    let mut gating = TokenGating::default();
    for (label, avail, has_readable_token) in probes {
        match avail {
            drives::DriveAvailability::TokenMismatch { .. }
            | drives::DriveAvailability::TokenExpectedButMissing => {
                gating.blocked.insert(label.clone());
            }
            drives::DriveAvailability::Available if *has_readable_token => {
                // Token file exists and matches — drive identity confirmed.
                gating.verified.insert(label.clone());
            }
            _ => {
                // TokenMissing (first use), fail-open, or no token file:
                // neither blocked nor verified.
            }
        }
    }
    gating
}

/// Apply token gating to a backup plan (pure plan mutation).
///
/// Drops only the SENDS (`SendFull` / `SendIncremental`) targeting blocked
/// drives — retention `Delete*` ops are untouched, because a clone's snapshots
/// are redundant copies and blocking deletes would cause space exhaustion
/// without safety benefit. Stamps `token_verified = true` on `SendFull`
/// operations for verified drives so the executor's chain-break gate may
/// proceed on known-good drives.
pub(super) fn apply_token_gating(plan: &mut BackupPlan, gating: &TokenGating) {
    if !gating.blocked.is_empty() {
        plan.operations.retain(|op| {
            !matches!(
                op,
                PlannedOperation::SendFull { drive_label, .. }
                | PlannedOperation::SendIncremental { drive_label, .. }
                if gating.blocked.contains(drive_label)
            )
        });
    }

    for op in &mut plan.operations {
        if let PlannedOperation::SendFull {
            drive_label,
            token_verified,
            ..
        } = op
            && gating.verified.contains(drive_label.as_str())
        {
            *token_verified = true;
        }
    }
}

/// Record, at run end, the retention shape of every subvolume whose
/// deletions this run did not withhold (ADR-110 transition safety): the
/// baseline the next run's gate compares against. Best-effort (ADR-102) —
/// with no state DB nothing is recorded, and the next run gates nothing.
/// A run whose baseline read failed ([`BaselineRead::Unreadable`]) records
/// nothing either: its gate held nothing, so recording would silently accept
/// any tightening it let through; the old rows stay to hold it next run.
pub(super) fn record_retention_shapes(
    db: Option<&StateDb>,
    gate: &crate::retention::RetentionGate,
    baseline: BaselineRead,
    recorded_at: chrono::NaiveDateTime,
) {
    let Some(db) = db else {
        return;
    };
    if baseline == BaselineRead::Unreadable {
        return;
    }
    for (subvolume, shape) in &gate.record {
        db.upsert_retention_shape_best_effort(subvolume, shape, recorded_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::dlabel;
    use crate::testkit::svname;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use crate::plan::{DeleteKind, PlanFilters};
    use crate::types::FullSendReason;

    // ── Retention-change gate recording (ADR-110) ──────────────────────

    /// `alpha` carries a named level (derived retention); `beta` is custom.
    fn gate_config() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-gate/urd.db"
metrics_file = "/tmp/urd-gate/m.prom"
log_dir = "/tmp/urd-gate"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"
protection_level = "recorded"

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// A shape strictly looser than anything the config derives: what the
    /// subvolume "used to keep" before its level changed.
    fn roomy_shape() -> crate::retention::RecordedRetention {
        let g = crate::types::ResolvedGraduatedRetention {
            hourly: 1000,
            daily: 1000,
            weekly: 1000,
            monthly: crate::types::MonthlyCount::Unlimited,
            yearly: 1000,
        };
        crate::retention::RecordedRetention {
            local: Some(crate::types::LocalRetentionPolicy::Graduated(g)),
            external: Some(g),
        }
    }

    fn gate_t() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap()
    }

    /// `decide_retention_gate` over the DB's rows for a run with `filters`.
    fn gate_for(
        resolved: &[crate::config::ResolvedSubvolume],
        db: &StateDb,
        confirmed: bool,
        filters: &PlanFilters,
    ) -> crate::retention::RetentionGate {
        crate::retention::decide_retention_gate(
            resolved,
            &db.all_retention_shapes().unwrap(),
            confirmed,
            crate::retention::RecordScope { filters },
        )
    }

    #[test]
    fn retention_gate_first_run_records_then_tightening_holds_until_confirmed() {
        use crate::retention::{RecordedRetention, RetentionShape};
        let config = gate_config();
        let resolved = config.resolved_subvolumes();
        // `recorded` sends nothing, so only alpha's local half ever runs; its
        // external half stays absent ("no record").
        assert!(!resolved[0].send_enabled);
        let alpha_now = RecordedRetention {
            local: Some(RetentionShape::of(&resolved[0]).local),
            external: None,
        };
        let db = StateDb::open_memory().unwrap();
        let full = PlanFilters::default();

        // First run on an upgraded install: no record → nothing held, all recorded.
        let gate = gate_for(&resolved, &db, false, &full);
        assert!(gate.held.is_empty());
        record_retention_shapes(Some(&db), &gate, BaselineRead::Read, gate_t());
        assert_eq!(db.all_retention_shapes().unwrap()["alpha"], alpha_now);

        // The level changed since: alpha used to keep far more.
        db.upsert_retention_shape_best_effort(&svname("alpha"), &roomy_shape(), gate_t());
        let gate = gate_for(&resolved, &db, false, &full);
        assert_eq!(gate.held.len(), 1);
        assert_eq!(gate.held[0].subvolume, "alpha");
        record_retention_shapes(Some(&db), &gate, BaselineRead::Read, gate_t());
        assert_eq!(
            db.all_retention_shapes().unwrap()["alpha"],
            roomy_shape(),
            "a held subvolume keeps its old record, so the next run holds again"
        );

        // One confirmed run applies and records the new local half; the
        // external half (no sends ran) keeps the old row's value. Only the
        // local half had tightened, so the next unconfirmed run holds nothing.
        let gate = gate_for(&resolved, &db, true, &full);
        assert!(gate.held.is_empty());
        record_retention_shapes(Some(&db), &gate, BaselineRead::Read, gate_t());
        assert_eq!(
            db.all_retention_shapes().unwrap()["alpha"],
            RecordedRetention {
                external: roomy_shape().external,
                ..alpha_now
            }
        );
        assert!(gate_for(&resolved, &db, false, &full).held.is_empty());
    }

    #[test]
    fn confirmed_subvolume_scoped_run_records_that_subvolume_only() {
        // `urd backup --subvolume beta --confirm-retention-change` while alpha
        // (named level) has tightened: beta records, alpha keeps its old row
        // and is still held by the next full run.
        let config = gate_config();
        let resolved = config.resolved_subvolumes();
        let db = StateDb::open_memory().unwrap();
        db.upsert_retention_shape_best_effort(&svname("alpha"), &roomy_shape(), gate_t());
        let scoped = PlanFilters {
            subvolume: Some(svname("beta")),
            ..PlanFilters::default()
        };
        let gate = gate_for(&resolved, &db, true, &scoped);
        let recorded: Vec<&str> = gate.record.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(recorded, vec!["beta"]);
        record_retention_shapes(Some(&db), &gate, BaselineRead::Read, gate_t());
        assert_eq!(db.all_retention_shapes().unwrap()["alpha"], roomy_shape());

        let next = gate_for(&resolved, &db, false, &PlanFilters::default());
        assert_eq!(next.held.len(), 1);
        assert_eq!(next.held[0].subvolume, "alpha");
    }

    #[test]
    fn retention_gate_never_holds_a_custom_subvolume() {
        let config = gate_config();
        let resolved = config.resolved_subvolumes();
        let db = StateDb::open_memory().unwrap();
        db.upsert_retention_shape_best_effort(&svname("beta"), &roomy_shape(), gate_t());
        let gate = gate_for(&resolved, &db, false, &PlanFilters::default());
        assert!(gate.held.is_empty(), "beta tightened but is custom: {:?}", gate.held);
    }

    #[test]
    fn record_retention_shapes_without_a_db_is_a_no_op() {
        let gate = crate::retention::RetentionGate {
            held: vec![],
            record: vec![(svname("alpha"), roomy_shape())],
        };
        record_retention_shapes(None, &gate, BaselineRead::Read, chrono::NaiveDateTime::default());
    }

    #[test]
    fn unreadable_retention_baseline_gates_nothing() {
        // No DB: the baseline is unknown, so nothing is held (ADR-102) — the
        // helper warns once; the empty map is what the gate sees.
        let baseline = crate::commands::plan_cmd::retention_baseline_or_warn(None);
        assert!(baseline.shapes.is_empty());
        assert_eq!(baseline.read, BaselineRead::Unreadable);
        assert!(crate::commands::plan_cmd::recorded_retention_shapes(None).is_none());
    }

    /// A row whose `shape` is a BLOB fails the typed read, so the whole
    /// baseline read errors while writes to the table still succeed.
    fn poison_retention_baseline(db: &StateDb) {
        db.conn
            .execute(
                "INSERT INTO retention_shapes (subvolume, shape, recorded_at)
                 VALUES ('ghost', X'00', '2026-09-01T04:00:00')",
                [],
            )
            .unwrap();
    }

    fn retention_shape_rows(db: &StateDb, subvolume: &str) -> i64 {
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM retention_shapes WHERE subvolume = ?1",
                [subvolume],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn unreadable_retention_baseline_records_no_shapes() {
        // The run-path sequence: read the baseline, decide, record at run end.
        let config = gate_config();
        let resolved = config.resolved_subvolumes();
        let db = StateDb::open_memory().unwrap();
        poison_retention_baseline(&db);

        let baseline = crate::commands::plan_cmd::retention_baseline_or_warn(Some(&db));
        assert_eq!(baseline.read, BaselineRead::Unreadable);
        let gate = crate::retention::decide_retention_gate(
            &resolved,
            &baseline.shapes,
            false,
            crate::retention::RecordScope { filters: &PlanFilters::default() },
        );
        assert!(gate.held.is_empty(), "an unknown baseline holds nothing");
        assert!(!gate.record.is_empty(), "the gate would have recorded");
        record_retention_shapes(Some(&db), &gate, baseline.read, gate_t());
        assert_eq!(retention_shape_rows(&db, "alpha"), 0);
        assert_eq!(retention_shape_rows(&db, "beta"), 0);
    }

    #[test]
    fn readable_retention_baseline_records_shapes() {
        let config = gate_config();
        let resolved = config.resolved_subvolumes();
        let db = StateDb::open_memory().unwrap();

        let baseline = crate::commands::plan_cmd::retention_baseline_or_warn(Some(&db));
        assert_eq!(baseline.read, BaselineRead::Read);
        let gate = crate::retention::decide_retention_gate(
            &resolved,
            &baseline.shapes,
            false,
            crate::retention::RecordScope { filters: &PlanFilters::default() },
        );
        record_retention_shapes(Some(&db), &gate, baseline.read, gate_t());
        assert_eq!(retention_shape_rows(&db, "alpha"), 1);
        assert_eq!(retention_shape_rows(&db, "beta"), 1);
    }

    // ── Token gating (UPI 059-b) ───────────────────────────────────────

    fn send_full(drive: &str, token_verified: bool) -> PlannedOperation {
        PlannedOperation::SendFull {
            snapshot: PathBuf::from(format!("/snaps/sv/{drive}-snap")),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/sv")),
            drive_label: drive.into(),
            subvolume_name: svname("sv"),
            pin_on_success: None,
            reason: FullSendReason::FirstSend,
            token_verified,
        }
    }

    fn send_incremental(drive: &str) -> PlannedOperation {
        PlannedOperation::SendIncremental {
            parent: PathBuf::from("/snaps/sv/parent"),
            snapshot: PathBuf::from("/snaps/sv/snap"),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/sv")),
            drive_label: drive.into(),
            subvolume_name: svname("sv"),
            pin_on_success: None,
        }
    }

    fn delete_snapshot(subvol: &str) -> PlannedOperation {
        PlannedOperation::DeleteSnapshot {
            path: PathBuf::from(format!("/snaps/{subvol}/old")),
            reason: "retention".to_string(),
            subvolume_name: subvol.into(),
            kind: DeleteKind::Policy,
        }
    }

    fn token_plan(ops: Vec<PlannedOperation>) -> BackupPlan {
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: ops,
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn resolve_token_gating_mismatch_blocks() {
        // A readable-but-mismatched token blocks (readable=true must not verify it).
        let probes = vec![(
            dlabel("WD-18TB"),
            drives::DriveAvailability::TokenMismatch {
                expected: "aaa".to_string(),
                found: "bbb".to_string(),
            },
            true,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.contains("WD-18TB"));
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_expected_but_missing_blocks() {
        let probes = vec![(
            dlabel("WD-18TB"),
            drives::DriveAvailability::TokenExpectedButMissing,
            false,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.contains("WD-18TB"));
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_available_and_readable_verifies() {
        let probes = vec![(
            dlabel("WD-18TB"),
            drives::DriveAvailability::Available,
            true,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.verified.contains("WD-18TB"));
        assert!(g.blocked.is_empty());
    }

    #[test]
    fn resolve_token_gating_available_but_unreadable_is_neither() {
        // Fail-open: drive is available but its token file can't be read.
        // Must NOT be treated as verified (excludes fail-open from verified).
        let probes = vec![(
            dlabel("WD-18TB"),
            drives::DriveAvailability::Available,
            false,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.is_empty());
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_fallopen_variants_are_neither() {
        // TokenMissing (genuine first use), unmounted, and UUID-level
        // unavailability all fall through to neither — even when the token
        // file happens to be readable (TokenMissing with readable=true).
        let probes = vec![
            (dlabel("a"), drives::DriveAvailability::TokenMissing, true),
            (dlabel("b"), drives::DriveAvailability::NotMounted, false),
            (
                dlabel("c"),
                drives::DriveAvailability::UuidCheckFailed("findmnt not found".to_string()),
                true,
            ),
            (
                dlabel("d"),
                drives::DriveAvailability::UuidMismatch {
                    expected: "x".to_string(),
                    found: "y".to_string(),
                },
                true,
            ),
        ];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.is_empty());
        assert!(g.verified.is_empty());
    }

    #[test]
    fn apply_token_gating_blocks_sends_keeps_deletes() {
        // The load-bearing rule: blocked drives lose their sends, but their
        // retention deletes proceed (a clone's snapshots are redundant copies).
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),
            send_incremental("WD-18TB"),
            delete_snapshot("sv"),
        ]);
        let gating = TokenGating {
            blocked: [dlabel("WD-18TB")].into_iter().collect(),
            verified: Default::default(),
        };
        apply_token_gating(&mut plan, &gating);
        // Both sends dropped; the delete retained.
        assert_eq!(plan.operations.len(), 1);
        assert!(matches!(
            plan.operations[0],
            PlannedOperation::DeleteSnapshot { .. }
        ));
    }

    #[test]
    fn apply_token_gating_verifies_full_sends_only() {
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),    // verified drive → flag flipped
            send_full("2TB-backup", false), // not verified → stays false
            send_incremental("WD-18TB"),    // incrementals carry no flag → no-op
        ]);
        let gating = TokenGating {
            blocked: Default::default(),
            verified: [dlabel("WD-18TB")].into_iter().collect(),
        };
        apply_token_gating(&mut plan, &gating);
        // Nothing dropped (no blocked labels).
        assert_eq!(plan.operations.len(), 3);
        match &plan.operations[0] {
            PlannedOperation::SendFull {
                drive_label,
                token_verified,
                ..
            } => {
                assert_eq!(drive_label, "WD-18TB");
                assert!(*token_verified, "verified drive's SendFull should be stamped");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
        match &plan.operations[1] {
            PlannedOperation::SendFull { token_verified, .. } => {
                assert!(!*token_verified, "unverified drive's SendFull stays false");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
        // SendIncremental has no token_verified field — unaffected by construction.
        assert!(matches!(
            plan.operations[2],
            PlannedOperation::SendIncremental { .. }
        ));
    }

    #[test]
    fn apply_token_gating_empty_is_noop() {
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),
            send_incremental("2TB-backup"),
            delete_snapshot("sv"),
        ]);
        let before = plan.operations.clone();
        apply_token_gating(&mut plan, &TokenGating::default());
        // Empty gating touches nothing — no drops, no stamps.
        assert_eq!(plan.operations, before);
    }
}
