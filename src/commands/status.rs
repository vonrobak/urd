use chrono::NaiveDateTime;

use crate::advice;
use crate::awareness::{ChainBreakReason, ChainStatus, SubvolAssessment};
use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::output::{
    AdaptationSummary, ChainHealth, ChainHealthEntry, DriveInfo, LastRunInfo, OutputMode,
    PoolPostureSummary, StatusAssessment, StatusOutput,
};
use crate::plan::{Observation, RealFileSystemState};
use crate::commands::storage_signals;
use crate::retention;
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    let assess_btrfs = crate::btrfs::RealBtrfs::for_reads(&config.general.btrfs_path);
    let observation = Observation {
        fs: &fs_state,
        history: &fs_state,
        btrfs: &assess_btrfs,
    };
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    // ── The seal (UPI 071/075) ──────────────────────────────────────
    // An incomplete seal stage is a named state, not a degraded render.
    let seal_gap = crate::commands::seal::seal_completeness(&config, output_mode);

    // ── Awareness model ─────────────────────────────────────────────
    let now = chrono::Local::now().naive_local();
    // Gather storage signals (read-only) and thread the per-subvol map into
    // assess(); status reflects the stabilized tier but never advances it (S1).
    let signals = storage_signals::gather(&config, state_db.as_ref());
    let assessments =
        advice::assess_view(&config, now, &observation, &signals.by_subvol);
    let storage_postures = storage_signals::aggregate(&assessments, &signals, now);
    // §2: collapse per-subvolume adaptations to one line per group (needs
    // `signals.pools`, which the renderer can't see — so aggregate here where
    // `signals` is live, like `storage_postures`).
    let storage_adaptations =
        storage_signals::aggregate_adaptations(&assessments, &signals, &config);

    // ── Drive info ──────────────────────────────────────────────────
    let drive_infos: Vec<DriveInfo> = config
        .drives
        .iter()
        .map(|d| {
            let mounted = drives::is_drive_mounted(d);
            DriveInfo {
                label: d.label.clone(),
                mounted,
                free_bytes: if mounted {
                    drives::filesystem_free_bytes(&d.mount_path).ok()
                } else {
                    None
                },
                role: d.role,
            }
        })
        .collect();

    // ── Last run ────────────────────────────────────────────────────
    let last_run = state_db.as_ref().and_then(|db| db.last_run_info());

    // ── Pin count ───────────────────────────────────────────────────
    let total_pins: usize = config
        .subvolumes
        .iter()
        .map(|sv| {
            config
                .local_snapshot_dir(&sv.name)
                .map(|dir| chain::find_pinned_snapshots(&dir, &drive_labels).len())
                .unwrap_or(0)
        })
        .sum();

    // ── Assemble and render ─────────────────────────────────────────
    let status_output = assemble_status_output(
        &assessments,
        storage_postures,
        storage_adaptations,
        drive_infos,
        last_run,
        total_pins,
        &config,
        now,
        seal_gap,
    );

    let rendered = voice::render_status(&status_output, output_mode);
    print!("{rendered}");

    Ok(())
}

/// Assemble the renderable `StatusOutput` from the assessment view and the
/// I/O-fetched facts. Pure function: `run()` gathers, this stitches —
/// chain-health worst-selection, promise-level threading, advice filtering,
/// redundancy advisories, and last-run age all live here.
#[must_use]
#[allow(clippy::too_many_arguments)]
fn assemble_status_output(
    assessments: &[SubvolAssessment],
    storage_postures: Vec<PoolPostureSummary>,
    storage_adaptations: Vec<AdaptationSummary>,
    drive_infos: Vec<DriveInfo>,
    last_run: Option<LastRunInfo>,
    total_pins: usize,
    config: &Config,
    now: NaiveDateTime,
    seal_gap: Option<crate::output::SealGap>,
) -> StatusOutput {
    // ── Chain health per subvolume (derived from awareness assessment) ──
    let chain_health_entries: Vec<ChainHealthEntry> = assessments
        .iter()
        .filter(|a| !a.chain_health.is_empty())
        .filter_map(|a| {
            let worst = a
                .chain_health
                .iter()
                .map(|ch| match &ch.status {
                    ChainStatus::Intact { pin_parent } => {
                        ChainHealth::Incremental(pin_parent.clone())
                    }
                    ChainStatus::Broken { reason, .. } => match reason {
                        ChainBreakReason::NoDriveData => ChainHealth::NoDriveData,
                        other => ChainHealth::Full(other.to_string()),
                    },
                })
                .min();
            worst.map(|health| ChainHealthEntry {
                subvolume: a.name.clone(),
                health,
            })
        })
        .collect();

    // ── Last run age ────────────────────────────────────────────────
    let last_run_age_secs = last_run.as_ref().and_then(|run| run.age_secs(now));

    // ── Redundancy advisories ──────────────────────────────────────
    let redundancy_advisories =
        advice::compute_redundancy_advisories(config, assessments);

    // Thread protection_level from resolved config into status assessments
    let resolved = config.resolved_subvolumes();
    let assessments_with_promises: Vec<StatusAssessment> = assessments
        .iter()
        .map(|a| {
            let mut sa = StatusAssessment::from_assessment(a);
            if let Some(sv) = resolved.iter().find(|sv| sv.name == a.name) {
                sa.promise_level = sv.protection_level.map(|pl| pl.to_string());
                sa.retention_summary = Some(retention::retention_summary(
                    &sv.local_retention,
                    &sv.snapshot_interval,
                ));
                sa.external_only = sv.local_retention.is_transient() && sv.send_enabled;
            }
            sa
        })
        .collect();

    let advice: Vec<advice::ActionableAdvice> = assessments
        .iter()
        .filter_map(|a| {
            let sv = resolved.iter().find(|sv| sv.name == a.name)?;
            advice::compute_advice(a, sv.send_enabled, sv.local_retention.is_transient())
        })
        .collect();

    StatusOutput {
        assessments: assessments_with_promises,
        chain_health: chain_health_entries,
        drives: drive_infos,
        last_run,
        last_run_age_secs,
        total_pins,
        redundancy_advisories,
        advice,
        storage_postures,
        storage_adaptations,
        seal_gap,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::test_support::dt;
    use crate::awareness::{
        DriveAssessment, DriveChainHealth, LocalAssessment, OperationalHealth,
        PromiseStatus,
    };
    use crate::types::{DriveRole, Interval};

    /// sv1: sheltered (named level, default retention). sv2: transient local
    /// retention with sends enabled (external-only mode). One primary drive.
    fn test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "ext-drive"
mount_path = "/mnt/ext"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
protection_level = "sheltered"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
local_retention = "transient"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    fn assessment(name: &str) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    fn mounted_drive(label: &str) -> DriveAssessment {
        DriveAssessment {
            drive_label: label.to_string(),
            status: PromiseStatus::Protected,
            mounted: true,
            snapshot_count: Some(3),
            last_send_age: None,
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Primary,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
        }
    }

    fn assemble(assessments: &[SubvolAssessment], config: &Config) -> StatusOutput {
        assemble_status_output(
            assessments,
            vec![],
            vec![],
            vec![],
            None,
            0,
            config,
            dt(2026, 6, 10, 12, 0),
            None,
        )
    }

    #[test]
    fn assemble_threads_the_seal_gap() {
        // UPI 071/075: the banner's substance travels through the pure
        // assembler untouched — probe at the edge, truth in the middle.
        let config = test_config();
        for gap in [
            crate::output::SealGap::Privilege,
            crate::output::SealGap::Units,
            crate::output::SealGap::FirstThread,
        ] {
            let out = assemble_status_output(
                &[],
                vec![],
                vec![],
                vec![],
                None,
                0,
                &config,
                dt(2026, 6, 10, 12, 0),
                Some(gap),
            );
            assert_eq!(out.seal_gap, Some(gap));
        }
        assert_eq!(assemble(&[], &config).seal_gap, None);
    }

    // ── Chain-health worst-selection ────────────────────────────────

    #[test]
    fn chain_health_intact_only_pins_incremental() {
        let mut a = assessment("sv1");
        a.chain_health = vec![DriveChainHealth {
            drive_label: "ext-drive".to_string(),
            status: ChainStatus::Intact {
                pin_parent: "20260610-0400-sv1".to_string(),
            },
        }];
        let out = assemble(&[a], &test_config());
        assert_eq!(out.chain_health.len(), 1);
        assert_eq!(out.chain_health[0].subvolume, "sv1");
        assert_eq!(
            out.chain_health[0].health,
            ChainHealth::Incremental("20260610-0400-sv1".to_string())
        );
    }

    #[test]
    fn chain_health_no_drive_data_maps_to_no_drive_data() {
        let mut a = assessment("sv1");
        a.chain_health = vec![DriveChainHealth {
            drive_label: "ext-drive".to_string(),
            status: ChainStatus::Broken {
                reason: ChainBreakReason::NoDriveData,
                pin_parent: None,
            },
        }];
        let out = assemble(&[a], &test_config());
        assert_eq!(out.chain_health[0].health, ChainHealth::NoDriveData);
    }

    #[test]
    fn chain_health_mixed_intact_and_broken_selects_worst() {
        // Characterization: pins today's `Ord` on output::ChainHealth
        // (severity NoDriveData < Full < Incremental; `min()` = worst).
        let mut a = assessment("sv1");
        a.chain_health = vec![
            DriveChainHealth {
                drive_label: "drive-a".to_string(),
                status: ChainStatus::Intact {
                    pin_parent: "20260610-0400-sv1".to_string(),
                },
            },
            DriveChainHealth {
                drive_label: "drive-b".to_string(),
                status: ChainStatus::Broken {
                    reason: ChainBreakReason::NoPinFile,
                    pin_parent: None,
                },
            },
        ];
        let out = assemble(&[a], &test_config());
        assert_eq!(
            out.chain_health[0].health,
            ChainHealth::Full("no pin".to_string())
        );

        // NoDriveData outranks (is worse than) a reasoned break.
        let mut b = assessment("sv2");
        b.chain_health = vec![
            DriveChainHealth {
                drive_label: "drive-a".to_string(),
                status: ChainStatus::Broken {
                    reason: ChainBreakReason::NoPinFile,
                    pin_parent: None,
                },
            },
            DriveChainHealth {
                drive_label: "drive-b".to_string(),
                status: ChainStatus::Broken {
                    reason: ChainBreakReason::NoDriveData,
                    pin_parent: None,
                },
            },
        ];
        let out = assemble(&[b], &test_config());
        assert_eq!(out.chain_health[0].health, ChainHealth::NoDriveData);
    }

    #[test]
    fn chain_health_empty_is_filtered_out() {
        let a = assessment("sv1"); // chain_health: vec![]
        let out = assemble(&[a], &test_config());
        assert!(out.chain_health.is_empty());
    }

    // ── Promise stitching ───────────────────────────────────────────

    #[test]
    fn stitching_threads_promise_level_and_retention_summary() {
        let out = assemble(&[assessment("sv1")], &test_config());
        let sa = &out.assessments[0];
        assert_eq!(sa.promise_level.as_deref(), Some("sheltered"));
        assert!(sa.retention_summary.is_some());
        assert!(!sa.external_only);
    }

    #[test]
    fn stitching_external_only_requires_transient_and_send_enabled() {
        let out = assemble(
            &[assessment("sv1"), assessment("sv2")],
            &test_config(),
        );
        let sv1 = &out.assessments[0];
        let sv2 = &out.assessments[1];
        assert!(!sv1.external_only, "non-transient retention is never external-only");
        assert!(sv2.external_only, "transient + send_enabled is external-only");
    }

    #[test]
    fn stitching_unknown_assessment_name_leaves_fields_default() {
        let out = assemble(&[assessment("ghost")], &test_config());
        let sa = &out.assessments[0];
        assert!(sa.promise_level.is_none());
        assert!(sa.retention_summary.is_none());
        assert!(!sa.external_only);
    }

    // ── Advice filtering ────────────────────────────────────────────

    #[test]
    fn advice_keeps_only_actionable_rows() {
        let healthy = assessment("sv1"); // Protected + Healthy → no advice
        let mut exposed = assessment("sv2");
        exposed.status = PromiseStatus::Unprotected;
        let out = assemble(&[healthy, exposed], &test_config());
        assert_eq!(out.advice.len(), 1);
        assert_eq!(out.advice[0].subvolume, "sv2");
    }

    // ── Redundancy advisories ───────────────────────────────────────

    #[test]
    fn redundancy_advisories_computed_and_threaded() {
        // sv1 is sheltered with sends enabled and exactly one external drive →
        // SinglePointOfFailure fires and lands on the top-level output.
        let mut a = assessment("sv1");
        a.external = vec![mounted_drive("ext-drive")];
        let out = assemble(&[a], &test_config());
        assert!(
            out.redundancy_advisories
                .iter()
                .any(|adv| adv.subvolume == "sv1"),
            "expected a redundancy advisory for sv1, got {:?}",
            out.redundancy_advisories
        );
    }

    // ── Empty input / last-run age ──────────────────────────────────

    #[test]
    fn empty_assessments_yield_empty_sections() {
        let out = assemble(&[], &test_config());
        assert!(out.assessments.is_empty());
        assert!(out.chain_health.is_empty());
        assert!(out.advice.is_empty());
        assert!(out.redundancy_advisories.is_empty());
        assert!(out.last_run_age_secs.is_none());
    }

    #[test]
    fn last_run_age_computed_from_now() {
        let last_run = LastRunInfo {
            id: 1,
            started_at: "2026-06-10T10:00:00".to_string(),
            result: "success".to_string(),
            duration: None,
        };
        let out = assemble_status_output(
            &[],
            vec![],
            vec![],
            vec![],
            Some(last_run),
            0,
            &test_config(),
            dt(2026, 6, 10, 12, 0),
            None,
        );
        assert_eq!(out.last_run_age_secs, Some(7200));
    }
}

