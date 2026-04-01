use crate::awareness::{self, PromiseStatus};
use crate::cli::{DoctorArgs, VerifyArgs};
use crate::config::Config;
use crate::output::{
    DoctorCheck, DoctorCheckStatus, DoctorDataSafety, DoctorOutput, DoctorSentinelStatus,
    DoctorVerdict, InitStatus, OutputMode,
};
use crate::plan::RealFileSystemState;
use crate::preflight;
use crate::sentinel_runner;
use crate::state::StateDb;
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
                DoctorCheck {
                    name: c.message.clone(),
                    status: DoctorCheckStatus::Warn,
                    detail: None,
                    suggestion: None,
                }
            })
            .collect()
    };

    // ── 2. Infrastructure checks (I/O: DB, dirs, sudo btrfs) ─────
    let init_checks = init::collect_infrastructure_checks(&config);
    let infra_checks: Vec<DoctorCheck> = init_checks
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

    let data_safety: Vec<DoctorDataSafety> = assessments
        .iter()
        .map(|a| {
            let (issue, suggestion) = match a.status {
                PromiseStatus::Unprotected => {
                    error_count += 1;
                    (
                        Some("exposed \u{2014} data may not be recoverable".to_string()),
                        Some("Run `urd backup` or connect a drive.".to_string()),
                    )
                }
                PromiseStatus::AtRisk => {
                    warn_count += 1;
                    let age = a
                        .local
                        .newest_age
                        .map(|d| {
                            let secs = d.num_seconds();
                            if secs >= 86400 {
                                format!("last backup {} days ago", secs / 86400)
                            } else {
                                format!("last backup {} hours ago", secs / 3600)
                            }
                        })
                        .unwrap_or_default();
                    (
                        Some(format!("waning{}", if age.is_empty() { String::new() } else { format!(" \u{2014} {age}") })),
                        Some("Run `urd backup` to refresh.".to_string()),
                    )
                }
                PromiseStatus::Protected => (None, None),
            };
            DoctorDataSafety {
                name: a.name.clone(),
                status: a.status.to_string(),
                health: a.health.to_string(),
                issue,
                suggestion,
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
        };
        Some(verify::collect_verify_output(&config, &verify_args))
    } else {
        None
    };

    if let Some(ref v) = verify_output {
        warn_count += v.warn_count as usize;
        error_count += v.fail_count as usize;
    }

    // ── 6. Verdict ────────────────────────────────────────────────
    let verdict = if error_count > 0 {
        DoctorVerdict::issues(error_count)
    } else if warn_count > 0 {
        DoctorVerdict::warnings(warn_count)
    } else {
        DoctorVerdict::healthy()
    };

    let output = DoctorOutput {
        config_checks,
        infra_checks,
        data_safety,
        sentinel,
        verify: verify_output,
        verdict,
    };

    print!("{}", voice::render_doctor(&output, output_mode));

    Ok(())
}
