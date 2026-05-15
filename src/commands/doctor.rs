use crate::awareness::{self, PromiseStatus};
use crate::cli::{DoctorArgs, VerifyArgs};
use crate::config::Config;
use crate::drives;
use crate::output::{
    DoctorCheck, DoctorCheckStatus, DoctorDataSafety, DoctorOutput, DoctorRecommendationRow,
    DoctorRecommendationView, DoctorSentinelStatus, DoctorVerdict, InitStatus, OutputMode,
    SchemaStatus,
};
use crate::plan::RealFileSystemState;
use crate::policy::{self, ShapeRole};
use crate::preflight;
use crate::sentinel_runner;
use crate::state::StateDb;
use crate::types::{LocalRetentionPolicy, ProtectionLevel};
use crate::voice;

use crate::commands::{init, verify};

pub fn run(config: Config, args: DoctorArgs, output_mode: OutputMode) -> anyhow::Result<()> {
    let mut warn_count: usize = 0;
    let mut error_count: usize = 0;

    // ── 1. Config checks (preflight — pure, instant) ──────────────
    let preflight_results = preflight::preflight_checks(&config);
    let config_checks: Vec<DoctorCheck> = if preflight_results.is_empty() {
        let subvol_count = config
            .subvolumes
            .iter()
            .filter(|s| s.enabled.unwrap_or(true))
            .count();
        let drive_count = config.drives.len();
        vec![DoctorCheck {
            name: format!("{subvol_count} subvolumes, {drive_count} drives"),
            status: DoctorCheckStatus::Ok,
            detail: None,
            suggestion: None,
        }]
    } else {
        preflight_results
            .iter()
            .map(|c| {
                warn_count += 1;
                let suggestion = match c.name {
                    "weakening-override" => {
                        Some("Reduce the interval to match, or change protection to custom".to_string())
                    }
                    _ => None,
                };
                DoctorCheck {
                    name: c.message.clone(),
                    status: DoctorCheckStatus::Warn,
                    detail: None,
                    suggestion,
                }
            })
            .collect()
    };

    // ── 2. Infrastructure checks (I/O: DB, dirs, sudo btrfs) ─────
    let init_checks = init::collect_infrastructure_checks(&config);
    let mut infra_checks: Vec<DoctorCheck> = init_checks
        .into_iter()
        .map(|c| {
            let status = match c.status {
                InitStatus::Ok => DoctorCheckStatus::Ok,
                InitStatus::Warn => {
                    warn_count += 1;
                    DoctorCheckStatus::Warn
                }
                InitStatus::Error => {
                    error_count += 1;
                    DoctorCheckStatus::Error
                }
            };
            DoctorCheck {
                name: c.name,
                status,
                detail: c.detail,
                suggestion: None,
            }
        })
        .collect();

    // UUID fingerprinting checks for mounted drives
    for (label, _uuid, snippet) in drives::check_missing_uuids(&config.drives) {
        warn_count += 1;
        infra_checks.push(DoctorCheck {
            name: format!("{label}: no UUID configured"),
            status: DoctorCheckStatus::Warn,
            detail: None,
            suggestion: Some(format!("Add {snippet} to [[drives]] for {label}")),
        });
    }

    // Space trend warnings: approaching min_free_bytes threshold
    for root in &config.local_snapshots.roots {
        let Some(min_free_bs) = root.min_free_bytes else {
            continue;
        };
        let min_free = min_free_bs.bytes();
        if let Ok(free) = drives::filesystem_free_bytes(&root.path)
            && free < min_free * 2
        {
            warn_count += 1;
            let free_display = crate::types::ByteSize(free);
            let threshold_display = crate::types::ByteSize(min_free);
            infra_checks.push(DoctorCheck {
                name: format!(
                    "{}: {} free, threshold {}",
                    root.path.display(),
                    free_display,
                    threshold_display
                ),
                status: DoctorCheckStatus::Warn,
                detail: if free < min_free {
                    Some("Space pressure active. Emergency retention may trigger on next backup.".to_string())
                } else {
                    Some("Approaching space pressure threshold.".to_string())
                },
                suggestion: Some("Run `urd emergency` to recover space now.".to_string()),
            });
        }
    }

    // ── 3. Data safety (awareness model) ──────────────────────────
    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    let now = chrono::Local::now().naive_local();
    let assessments = awareness::assess(&config, now, &fs_state);

    let resolved = config.resolved_subvolumes();
    let data_safety: Vec<DoctorDataSafety> = assessments
        .iter()
        .map(|a| {
            let sv_config = resolved.iter().find(|sv| sv.name == a.name);
            let send_enabled = sv_config.is_none_or(|sv| sv.send_enabled);
            let external_only = sv_config.is_some_and(|sv| sv.local_retention.is_transient());
            let advice = awareness::compute_advice(a, send_enabled, external_only);

            // Extract structured advice into doctor display fields.
            let unpack = |adv: &awareness::ActionableAdvice| {
                (
                    Some(adv.issue.clone()),
                    adv.command.as_ref().map(|c| format!("Run `{c}`.")),
                    adv.reason.clone(),
                )
            };

            let (issue, suggestion, reason) = match a.status {
                PromiseStatus::Protected if a.health == awareness::OperationalHealth::Healthy => {
                    (None, None, None)
                }
                PromiseStatus::Protected => advice.as_ref().map(unpack).unwrap_or_default(),
                PromiseStatus::Unprotected => {
                    error_count += 1;
                    advice.as_ref().map(unpack).unwrap_or_else(|| {
                        (
                            Some("exposed — data may not be recoverable".to_string()),
                            Some("Run `urd backup` or connect a drive.".to_string()),
                            None,
                        )
                    })
                }
                PromiseStatus::AtRisk => {
                    warn_count += 1;
                    advice.as_ref().map(unpack).unwrap_or_else(|| {
                        (
                            Some("waning".to_string()),
                            Some("Run `urd backup` to refresh.".to_string()),
                            None,
                        )
                    })
                }
            };
            DoctorDataSafety {
                name: a.name.clone(),
                status: a.status.to_string(),
                health: a.health.to_string(),
                issue,
                suggestion,
                reason,
            }
        })
        .collect();

    // ── 4. Sentinel status ────────────────────────────────────────
    let state_path = sentinel_runner::sentinel_state_path(&config);
    let sentinel = match sentinel_runner::read_sentinel_state_file(&state_path) {
        Some(state) if sentinel_runner::is_pid_alive(state.pid) => {
            // Compute uptime from started timestamp
            let uptime = chrono::NaiveDateTime::parse_from_str(&state.started, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .or_else(|| {
                    chrono::NaiveDateTime::parse_from_str(&state.started, "%Y-%m-%d %H:%M:%S").ok()
                })
                .map(|started| {
                    let dur = now - started;
                    let hours = dur.num_hours();
                    let minutes = dur.num_minutes() % 60;
                    if hours > 0 {
                        format!("{hours}h {minutes}m")
                    } else {
                        format!("{minutes}m")
                    }
                });
            DoctorSentinelStatus {
                running: true,
                pid: Some(state.pid),
                uptime,
            }
        }
        _ => {
            warn_count += 1;
            DoctorSentinelStatus {
                running: false,
                pid: None,
                uptime: None,
            }
        }
    };

    // ── 5. Verify (optional — --thorough only) ────────────────────
    let verify_output = if args.thorough {
        let verify_args = VerifyArgs {
            subvolume: None,
            drive: None,
            detail: false,
        };
        Some(verify::collect_verify_output(&config, &verify_args))
    } else {
        None
    };

    if let Some(ref v) = verify_output {
        warn_count += v.warn_count as usize;
        error_count += v.fail_count as usize;
    }

    // ── 5.5 Churn (UPI 030 — only with --thorough) ────────────────
    let churn_view = if args.thorough {
        Some(build_doctor_churn_view(&config, state_db.as_ref()))
    } else {
        None
    };

    // ── 5.6 Recommendations (UPI 041 — only with --thorough) ──────
    let recommendation_view = if args.thorough {
        Some(build_doctor_recommendation_view(&config, state_db.as_ref()))
    } else {
        None
    };

    // ── 6. Verdict ────────────────────────────────────────────────
    let degraded_count = data_safety
        .iter()
        .filter(|d| d.status == "PROTECTED" && d.health != "healthy")
        .count();

    // NOTE: When --thorough is used, verify's drive-mounted warnings inflate warn_count.
    // This can mask the Degraded verdict when absent drives are the only issue.
    // Accepted trade-off: plain `urd doctor` (where status directs users) works correctly.
    // The --thorough path may show Warnings instead of Degraded in this scenario.
    let verdict = if error_count > 0 {
        DoctorVerdict::issues(error_count)
    } else if warn_count > 0 {
        DoctorVerdict::warnings(warn_count)
    } else if degraded_count > 0 {
        DoctorVerdict::degraded(degraded_count)
    } else {
        DoctorVerdict::healthy()
    };

    // UPI 042 Branch G: surface a soft notice when loaded config is older
    // than the current schema. Build SchemaStatus only when there's
    // something to say (current < latest).
    const LATEST_SCHEMA_VERSION: u32 = 2;
    let schema_status = match config.general.config_version {
        Some(v) if v >= LATEST_SCHEMA_VERSION => None,
        current => Some(SchemaStatus {
            current,
            latest: LATEST_SCHEMA_VERSION,
        }),
    };

    let output = DoctorOutput {
        config_checks,
        infra_checks,
        data_safety,
        sentinel,
        schema_status,
        verify: verify_output,
        churn: churn_view,
        recommendations: recommendation_view,
        verdict,
    };

    print!("{}", voice::render_doctor(&output, output_mode));

    Ok(())
}

/// Build the `--thorough` Churn-section view from `drift_samples` history.
///
/// Best-effort per-subvolume: a query failure renders as `NotMeasured`.
fn build_doctor_churn_view(
    config: &Config,
    state_db: Option<&StateDb>,
) -> crate::output::DoctorChurnView {
    let now = chrono::Local::now().naive_local();
    build_doctor_churn_view_inner(config, state_db, now)
}

/// Build the `--thorough` Recommendations-section view (UPI 041).
///
/// Iterates resolved subvolumes, computes a recommendation per role from
/// the same rolling-churn aggregate the Churn section uses, drops aligned
/// rows, and sorts by recovery magnitude descending.
fn build_doctor_recommendation_view(
    config: &Config,
    state_db: Option<&StateDb>,
) -> DoctorRecommendationView {
    let now = chrono::Local::now().naive_local();
    build_doctor_recommendation_view_inner(config, state_db, now)
}

fn build_doctor_recommendation_view_inner(
    config: &Config,
    state_db: Option<&StateDb>,
    now: chrono::NaiveDateTime,
) -> DoctorRecommendationView {
    let window = crate::drift::default_window();
    let since = now - window;
    let header = format!(
        "based on {}-day churn observation; apply by editing ~/.config/urd/urd.toml",
        window.num_days()
    );
    let resolved = config.resolved_subvolumes();
    let mut rows: Vec<DoctorRecommendationRow> = Vec::new();

    for sv in resolved.iter().filter(|sv| sv.enabled) {
        let churn = compute_churn_for(state_db, &sv.name, since, window, now);

        let local = match &sv.local_retention {
            LocalRetentionPolicy::Transient => None,
            LocalRetentionPolicy::Graduated(g) => {
                policy::recommend_shape(g, &churn, ShapeRole::Local)
            }
        };

        let external = if sv.send_enabled {
            policy::recommend_shape(&sv.external_retention, &churn, ShapeRole::External)
        } else {
            None
        };

        let local = local.filter(|r| r.suggested != r.current);
        let external = external.filter(|r| r.suggested != r.current);

        if local.is_none() && external.is_none() {
            continue;
        }

        let note = local
            .as_ref()
            .and_then(|r| r.note)
            .or_else(|| external.as_ref().and_then(|r| r.note));

        let was_named_level = sv
            .protection_level
            .filter(|p| *p != ProtectionLevel::Custom);

        rows.push(DoctorRecommendationRow {
            name: sv.name.clone(),
            local,
            external,
            note,
            was_named_level,
        });
    }

    rows.sort_by_key(|r| std::cmp::Reverse(recovery_bytes(r)));

    DoctorRecommendationView { header, rows }
}

fn recovery_bytes(row: &DoctorRecommendationRow) -> u64 {
    let local = row
        .local
        .as_ref()
        .map(|r| {
            r.current_cost
                .data_bytes
                .saturating_sub(r.suggested_cost.data_bytes)
        })
        .unwrap_or(0);
    let external = row
        .external
        .as_ref()
        .map(|r| {
            r.current_cost
                .data_bytes
                .saturating_sub(r.suggested_cost.data_bytes)
        })
        .unwrap_or(0);
    local.saturating_add(external)
}

fn compute_churn_for(
    state_db: Option<&StateDb>,
    name: &str,
    since: chrono::NaiveDateTime,
    window: chrono::Duration,
    now: chrono::NaiveDateTime,
) -> crate::drift::ChurnEstimate {
    state_db
        .and_then(|db| db.drift_samples_for_subvolume(name, since).ok())
        .map(|rows| {
            let samples: Vec<_> = rows.into_iter().map(StateDb::drift_row_to_sample).collect();
            crate::drift::compute_rolling_churn(&samples, window, now)
        })
        .unwrap_or(crate::drift::ChurnEstimate {
            mean_bytes_per_second: None,
            mean_incremental_bytes: None,
            incremental_count: 0,
            full_count: 0,
            median_full_bytes: None,
            latest_full_bytes: None,
            latest_full_interval_secs: None,
        })
}

/// Pure-ish core (still does DB I/O via `state_db`) — extracted so unit tests
/// can pass an in-memory `StateDb` and a fixed `now`.
fn build_doctor_churn_view_inner(
    config: &Config,
    state_db: Option<&StateDb>,
    now: chrono::NaiveDateTime,
) -> crate::output::DoctorChurnView {
    use crate::output::{DoctorChurnRow, DoctorChurnView};

    let since = now - crate::drift::default_window();
    let rows: Vec<DoctorChurnRow> = config
        .subvolumes
        .iter()
        .map(|sv| {
            let state = state_db
                .and_then(|db| db.drift_samples_for_subvolume(&sv.name, since).ok())
                .map(|rows| {
                    let samples: Vec<crate::drift::DriftSample> =
                        rows.into_iter().map(StateDb::drift_row_to_sample).collect();
                    let estimate = crate::drift::compute_rolling_churn(
                        &samples,
                        crate::drift::default_window(),
                        now,
                    );
                    crate::output::render_churn(&estimate)
                })
                .unwrap_or(crate::output::ChurnRender::NotMeasured);
            DoctorChurnRow {
                name: sv.name.clone(),
                state,
            }
        })
        .collect();

    DoctorChurnView {
        window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ".to_string(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-test/urd.db"
metrics_file = "/tmp/urd-doctor-test/backup.prom"
log_dir = "/tmp/urd-doctor-test"
heartbeat_file = "/tmp/urd-doctor-test/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta", "gamma"] }
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

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"

[[subvolumes]]
name = "gamma"
short_name = "gamma"
source = "/data/gamma"
"#;
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn doctor_thorough_populates_churn_view_for_all_subvolumes() {
        let config = cfg();
        let db = StateDb::open_memory().unwrap();
        let now = chrono::NaiveDateTime::parse_from_str(
            "2026-05-01T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();

        // Seed alpha + beta with a successful incremental sample each.
        // Leave gamma without any samples so it renders as NotMeasured.
        for name in &["alpha", "beta"] {
            db.record_drift_sample_best_effort(&crate::state::DriftSampleRow {
                run_id: None,
                subvolume: (*name).to_string(),
                sampled_at: now - chrono::Duration::days(1),
                seconds_since_prev_send: Some(86_400),
                bytes_transferred: 1_000_000,
                source_free_bytes: None,
                send_kind: crate::types::SendKind::Incremental,
            });
        }

        let view = build_doctor_churn_view_inner(&config, Some(&db), now);
        assert_eq!(view.rows.len(), 3);
        assert_eq!(view.rows[0].name, "alpha");
        assert_eq!(view.rows[1].name, "beta");
        assert_eq!(view.rows[2].name, "gamma");
        assert!(matches!(
            view.rows[2].state,
            crate::output::ChurnRender::NotMeasured
        ));
        // alpha and beta each have one incremental → FirstMeasurement.
        assert!(matches!(
            view.rows[0].state,
            crate::output::ChurnRender::FirstMeasurement { .. }
        ));
        assert!(matches!(
            view.rows[1].state,
            crate::output::ChurnRender::FirstMeasurement { .. }
        ));
    }

    // ── UPI 041 Recommendations builder ────────────────────────────

    fn empty_cfg() -> Config {
        let toml_str = r#"
drives = []
subvolumes = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"

[local_snapshots]
roots = []

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// Three subvolumes — containers (hot), photos (warmer), docs (cold).
    /// Configured retention covers the symmetry case for recovery sorting.
    fn three_subvols_cfg() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["containers", "photos", "docs"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "containers"
short_name = "containers"
source = "/data/containers"

[[subvolumes]]
name = "photos"
short_name = "photos"
source = "/data/photos"

[[subvolumes]]
name = "docs"
short_name = "docs"
source = "/data/docs"
"#;
        toml::from_str(toml_str).unwrap()
    }

    fn seed_incremental(db: &StateDb, name: &str, sampled_at: chrono::NaiveDateTime, bytes: u64) {
        db.record_drift_sample_best_effort(&crate::state::DriftSampleRow {
            run_id: None,
            subvolume: name.to_string(),
            sampled_at,
            seconds_since_prev_send: Some(86_400),
            bytes_transferred: bytes,
            source_free_bytes: None,
            send_kind: crate::types::SendKind::Incremental,
        });
    }

    fn now_fixed() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-05-01T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    #[test]
    fn recommendation_view_empty_when_no_subvolumes() {
        let config = empty_cfg();
        let db = StateDb::open_memory().unwrap();
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now_fixed());
        assert!(view.rows.is_empty());
    }

    #[test]
    fn recommendation_view_suppresses_aligned_shapes() {
        // Build a config whose configured shape already matches the
        // engine's output: a single cold subvolume with retention =
        // {24, 60, 52, 24} for both local and external. With churn ≈ 81 B/s
        // the engine clamps to the same max-shape, so suggested == current
        // and the row is dropped.
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["cold"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "cold"
short_name = "cold"
source = "/data/cold"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        // ~81 B/s — clamps every slot to clamp_max for both roles.
        // bytes_transferred = 81 * 86_400 ≈ 7_000_000 over one-day interval.
        seed_incremental(&db, "cold", now - chrono::Duration::hours(12), 7_000_000);
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);
        assert!(
            view.rows.is_empty(),
            "aligned shape must be suppressed: {:?}",
            view.rows
        );
    }

    #[test]
    fn recommendation_view_emits_row_when_one_role_differs() {
        // Hot churn against the max-shape config: at minimum the local
        // recommendation tightens. Some external slots may also clamp,
        // but the test only needs *some* row to surface.
        let config = three_subvols_cfg();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        // Hot rate ~ 31_250 B/s = ~2.7 GB/day. bytes for 1-day interval.
        seed_incremental(&db, "containers", now - chrono::Duration::hours(12), 2_700_000_000);
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);
        let containers_row = view
            .rows
            .iter()
            .find(|r| r.name == "containers")
            .expect("containers row present");
        assert!(
            containers_row.local.is_some() || containers_row.external.is_some(),
            "containers must have at least one role recommendation"
        );
    }

    #[test]
    fn recommendation_view_skips_local_for_transient() {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["transient"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "transient"
short_name = "transient"
source = "/data/transient"
local_retention = "transient"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        seed_incremental(&db, "transient", now - chrono::Duration::hours(12), 2_700_000_000);
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);
        let row = view
            .rows
            .iter()
            .find(|r| r.name == "transient")
            .expect("transient row should surface from external recommendation");
        assert!(row.local.is_none(), "transient must have no local rec");
        assert!(
            row.external.is_some(),
            "transient row should be carried by external recommendation"
        );
    }

    #[test]
    fn recommendation_view_omits_external_for_send_disabled() {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["local-only"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "local-only"
short_name = "local-only"
source = "/data/local-only"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        seed_incremental(&db, "local-only", now - chrono::Duration::hours(12), 2_700_000_000);
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);
        let row = view
            .rows
            .iter()
            .find(|r| r.name == "local-only")
            .expect("local-only row should surface from local recommendation");
        assert!(
            row.external.is_none(),
            "send_enabled=false must have no external rec"
        );
        assert!(
            row.local.is_some(),
            "local-only row should be carried by local recommendation"
        );
    }

    #[test]
    fn recommendation_view_sorts_by_recovery_magnitude_descending() {
        // Three subvolumes with rates that produce ordered recovery
        // magnitudes: containers (hot) → photos (warm) → docs (cold).
        // docs is already at max-shape (the engine's output for cold
        // churn) so its recovery is zero.
        let config = three_subvols_cfg();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        seed_incremental(&db, "containers", now - chrono::Duration::hours(12), 2_700_000_000); // ~31_250 B/s
        seed_incremental(&db, "photos", now - chrono::Duration::hours(12), 50_000_000); // ~580 B/s
        seed_incremental(&db, "docs", now - chrono::Duration::hours(12), 7_000_000); // ~81 B/s (cold)
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);
        // At least the first two rows must be ordered by descending
        // recovery; the third may or may not exist depending on whether
        // docs gets suppressed by the aligned-shape filter.
        let names: Vec<&str> = view.rows.iter().map(|r| r.name.as_str()).collect();
        let containers_idx = names.iter().position(|n| *n == "containers");
        let photos_idx = names.iter().position(|n| *n == "photos");
        assert!(containers_idx.is_some(), "containers must surface");
        if let (Some(c), Some(p)) = (containers_idx, photos_idx) {
            assert!(
                c < p,
                "containers (higher recovery) must precede photos: {names:?}"
            );
        }
        // docs is already aligned with the engine's cold-clamp output,
        // so it should be suppressed.
        assert!(
            !names.contains(&"docs"),
            "docs at max-shape should be suppressed: {names:?}"
        );
    }

    #[test]
    fn recommendation_view_populates_was_named_level_for_named_level_subvol() {
        // Two subvolumes: one declares protection_level = "sheltered",
        // the other "custom". Both have a differing recommendation.
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-rec-test/urd.db"
metrics_file = "/tmp/urd-doctor-rec-test/backup.prom"
log_dir = "/tmp/urd-doctor-rec-test"
heartbeat_file = "/tmp/urd-doctor-rec-test/heartbeat.json"
run_frequency = "daily"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sheltered-sv", "custom-sv"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "sheltered-sv"
short_name = "sheltered-sv"
source = "/data/sheltered-sv"
protection_level = "sheltered"

[[subvolumes]]
name = "custom-sv"
short_name = "custom-sv"
source = "/data/custom-sv"
protection_level = "custom"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        // Hot enough to force a differing recommendation in both rows.
        seed_incremental(&db, "sheltered-sv", now - chrono::Duration::hours(12), 2_700_000_000);
        seed_incremental(&db, "custom-sv", now - chrono::Duration::hours(12), 2_700_000_000);
        let view = build_doctor_recommendation_view_inner(&config, Some(&db), now);

        let sheltered_row = view
            .rows
            .iter()
            .find(|r| r.name == "sheltered-sv")
            .expect("sheltered-sv row");
        assert_eq!(
            sheltered_row.was_named_level,
            Some(crate::types::ProtectionLevel::Sheltered)
        );

        let custom_row = view
            .rows
            .iter()
            .find(|r| r.name == "custom-sv")
            .expect("custom-sv row");
        assert!(custom_row.was_named_level.is_none(),
            "custom protection_level must not populate was_named_level"
        );
    }
}
