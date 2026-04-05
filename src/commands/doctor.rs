use crate::awareness::{self, PromiseStatus};
use crate::cli::{DoctorArgs, VerifyArgs};
use crate::config::Config;
use crate::drives;
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
