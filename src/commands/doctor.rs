use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::advice;
use crate::awareness::{self, PromiseStatus};
use crate::cli::{DoctorArgs, VerifyArgs};
use crate::config::Config;
use crate::drives;
use crate::output::{
    DoctorCheck, DoctorCheckStatus, DoctorDataSafety, DoctorOutput, DoctorRecommendationRow,
    DoctorRecommendationView, DoctorSentinelStatus, DoctorVerdict, InitStatus, OutputMode,
    SchemaStatus,
};
use crate::plan::{Observation, RealFileSystemState};
use crate::recommendation::{
    self, AdjustmentReason, HeadroomContext, HeadroomSeverity, ShapeRole,
};
use crate::pools::{self, PoolSpace};
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
        // UPI 045 R-10: a zero-subvolume config is not "All clear" — it has
        // nothing to protect. Surface as a config warning so the verdict
        // becomes Warnings rather than the misleading Healthy.
        if subvol_count == 0 {
            warn_count += 1;
            vec![DoctorCheck {
                name: "No subvolumes configured.".to_string(),
                status: DoctorCheckStatus::Warn,
                detail: Some(
                    "Add a [[subvolumes]] entry to config.toml — Urd has nothing to back up."
                        .to_string(),
                ),
                suggestion: Some("Edit ~/.config/urd/urd.toml.".to_string()),
            }]
        } else {
            vec![DoctorCheck {
                name: format!("{subvol_count} subvolumes, {drive_count} drives"),
                status: DoctorCheckStatus::Ok,
                detail: None,
                suggestion: None,
            }]
        }
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
    let sudo_probe = crate::commands::seal::probe_grant(&config.general.btrfs_path);
    let init_checks = init::collect_infrastructure_checks(&config, &sudo_probe);
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

    // ── Sudoers drift (UPI 071) — three-arm gate (adversary F4) ──
    // Granted → diff effective privileges against the config's expected
    // grants; Denied → silent here (the sudo-btrfs check above already
    // speaks — one cause, one finding); Unclear → an honest cannot-verify
    // Warn, never a silent skip. Not --thorough-gated: drift causes
    // failures that look like bugs.
    let drift_checks = match sudo_probe.0 {
        crate::sudoers::GrantProbe::Denied => Vec::new(),
        crate::sudoers::GrantProbe::Unclear => vec![cannot_verify_grant_check(format!(
            "could not determine the sudo grant state: {}",
            sudo_probe.1
        ))],
        crate::sudoers::GrantProbe::Granted => {
            let listing = std::process::Command::new("sudo")
                .env("LC_ALL", "C")
                .args(["-n", "-l"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
            build_sudoers_drift_checks(&config, listing.as_deref())
        }
    };
    for check in &drift_checks {
        match check.status {
            DoctorCheckStatus::Warn => warn_count += 1,
            DoctorCheckStatus::Error => error_count += 1,
            DoctorCheckStatus::Ok => {}
        }
    }
    infra_checks.extend(drift_checks);

    // ── Units drift + linger (UPI 075) ───────────────────────────
    // The oracle (`systemd_units::expected_units`) diffed against the
    // installed files — content included, since ExecStart carries the
    // resolved binary path (adversary F6: the detail names both paths so a
    // dev-build doctor self-diagnoses). Not --thorough-gated. The linger
    // row (adversary F1) speaks only when the units are in place: a user
    // timer fires only while a session exists.
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok();
    let installed = dirs::config_dir().map(|d| {
        let dir = d.join("systemd/user");
        crate::systemd_units::expected_unit_names(&config.general.run_frequency)
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    std::fs::read_to_string(dir.join(name)).ok(),
                )
            })
            .collect::<std::collections::HashMap<_, _>>()
    });
    let mut units_checks =
        build_units_drift_checks(&config, exe.as_deref(), installed.as_ref());
    if units_checks
        .iter()
        .all(|c| c.status == DoctorCheckStatus::Ok)
    {
        units_checks.extend(linger_check());
    }
    for check in &units_checks {
        match check.status {
            DoctorCheckStatus::Warn => warn_count += 1,
            DoctorCheckStatus::Error => error_count += 1,
            DoctorCheckStatus::Ok => {}
        }
    }
    infra_checks.extend(units_checks);

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
    let assess_btrfs = crate::btrfs::RealBtrfs::for_reads(&config.general.btrfs_path);
    let observation = Observation {
        fs: &fs_state,
        history: &fs_state,
        btrfs: &assess_btrfs,
    };
    let now = chrono::Local::now().naive_local();
    // Read-only gather: thread storage posture into the data-safety section.
    let signals = crate::commands::storage_signals::gather(&config, state_db.as_ref());
    let assessments =
        advice::assess_view(&config, now, &observation, &signals.by_subvol);

    let resolved = config.resolved_subvolumes();
    let data_safety: Vec<DoctorDataSafety> = assessments
        .iter()
        .map(|a| {
            let sv_config = resolved.iter().find(|sv| sv.name == a.name);
            let send_enabled = sv_config.is_none_or(|sv| sv.send_enabled);
            let external_only = sv_config.is_some_and(|sv| sv.local_retention.is_transient());
            let earned = sudo_probe.0 == crate::sudoers::GrantProbe::Granted;
            let advice = advice::compute_advice(a, earned, send_enabled, external_only);

            // Extract structured advice into doctor display fields.
            let (issue, suggestion, reason) = match a.status {
                PromiseStatus::Protected if a.health == awareness::OperationalHealth::Healthy => {
                    (None, None, None)
                }
                PromiseStatus::Protected => advice.as_ref().map(unpack_advice).unwrap_or_default(),
                PromiseStatus::Unprotected => {
                    error_count += 1;
                    advice.as_ref().map(unpack_advice).unwrap_or_else(|| {
                        (
                            Some("exposed — data may not be recoverable".to_string()),
                            Some("Run `urd backup` or connect a drive.".to_string()),
                            None,
                        )
                    })
                }
                PromiseStatus::AtRisk => {
                    warn_count += 1;
                    advice.as_ref().map(unpack_advice).unwrap_or_else(|| {
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
                status: a.status,
                health: a.health.to_string(),
                issue,
                suggestion,
                reason,
                storage_posture: a.storage_posture,
            }
        })
        .collect();

    // ── 4. Sentinel status (UPI 081 B4: Timer-cadence machines omit this
    // section entirely — a stopped daemon that config never installs is
    // not a warning) ────────────────────────────────────────────────
    let sentinel = if config.general.run_frequency == crate::types::RunFrequency::Sentinel {
        let state_path = sentinel_runner::sentinel_state_path(&config);
        Some(match sentinel_runner::read_sentinel_state_file(&state_path) {
            Some(state) if sentinel_runner::is_pid_alive(state.pid) => {
                // Compute uptime from started timestamp
                let uptime =
                    chrono::NaiveDateTime::parse_from_str(&state.started, "%Y-%m-%dT%H:%M:%S")
                        .ok()
                        .or_else(|| {
                            chrono::NaiveDateTime::parse_from_str(&state.started, "%Y-%m-%d %H:%M:%S")
                                .ok()
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
        })
    } else {
        None
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

    // ── 5.7 Retention: orphan pin files (#125 — only with --thorough) ──
    let retention_checks = if args.thorough {
        build_retention_checks(&config)
    } else {
        Vec::new()
    };
    warn_count += retention_checks.len();

    // ── 6. Verdict ────────────────────────────────────────────────
    let degraded_count = data_safety
        .iter()
        .filter(|d| d.status == PromiseStatus::Protected && d.health != "healthy")
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
        schema_version: crate::output::DOCTOR_OUTPUT_SCHEMA_VERSION,
        config_checks,
        infra_checks,
        data_safety,
        sentinel,
        schema_status,
        verify: verify_output,
        churn: churn_view,
        recommendations: recommendation_view,
        retention_checks,
        verdict,
    };

    print!("{}", voice::render_doctor(&output, output_mode));

    Ok(())
}

/// The honest-skip row for a drift check that cannot run: never a silent
/// pass (arc grill; adversary F4).
fn cannot_verify_grant_check(detail: String) -> DoctorCheck {
    DoctorCheck {
        name: "sudoers drift".to_string(),
        status: DoctorCheckStatus::Warn,
        detail: Some(detail),
        suggestion: Some(
            "Run `sudo -l` yourself, or re-run `urd doctor` after `sudo -v`.".to_string(),
        ),
    }
}

/// Diff the config's expected grants (`sudoers::expected_grant_lines`, the
/// single oracle) against effective privileges from `sudo -n -l`. Pure
/// over the injected listing text; the subprocess stays in `run()`.
/// `listing = None` means the listing needed a password or failed to run.
fn build_sudoers_drift_checks(config: &Config, listing: Option<&str>) -> Vec<DoctorCheck> {
    let expected = match crate::sudoers::expected_grant_lines(config) {
        Ok(expected) => expected,
        // A config the oracle refuses to render (hostile characters,
        // shallow scopes) is its own advisory — nothing to diff against.
        Err(refusal) => {
            return vec![DoctorCheck {
                name: "sudoers drift".to_string(),
                status: DoctorCheckStatus::Warn,
                detail: Some(refusal.to_string()),
                suggestion: Some("Fix the named config value, then re-run `urd init`.".to_string()),
            }];
        }
    };
    let Some(listing) = listing else {
        return vec![cannot_verify_grant_check(
            "the privilege listing needs a password (sudo -n -l) — cannot verify \
             the grant against the config without interaction"
                .to_string(),
        )];
    };
    let listing = match crate::sudoers::parse_privilege_listing(listing) {
        Ok(listing) => listing,
        Err(uncertain) => return vec![cannot_verify_grant_check(uncertain.reason)],
    };
    match crate::sudoers::coverage(&expected, &listing) {
        crate::sudoers::Coverage::AllCovered => vec![DoctorCheck {
            name: "sudoers grant covers the config".to_string(),
            status: DoctorCheckStatus::Ok,
            detail: None,
            suggestion: None,
        }],
        crate::sudoers::Coverage::CannotInterpret { reason } => {
            vec![cannot_verify_grant_check(reason)]
        }
        crate::sudoers::Coverage::Gaps { missing, uncertain } => {
            // UPI 081 B3: every expected line is missing (self-implies
            // uncertain is empty too) → this machine was never sealed, not
            // a config that drifted since. One earning-vocabulary line, not
            // N "predates" rows that bury the real cause.
            if missing.len() == expected.len() {
                return vec![DoctorCheck {
                    name: "sudoers drift".to_string(),
                    status: DoctorCheckStatus::Warn,
                    detail: Some(
                        "This machine isn't sealed yet — run `urd init` to earn root's leave \
                         for btrfs."
                            .to_string(),
                    ),
                    suggestion: Some("Run `urd init` to earn the grant.".to_string()),
                }];
            }
            let mut checks: Vec<DoctorCheck> = missing
                .into_iter()
                .map(|spec| DoctorCheck {
                    name: "sudoers drift".to_string(),
                    status: DoctorCheckStatus::Warn,
                    detail: Some(format!(
                        "no grant covers `{spec}` — a config mapping the installed \
                         sudoers file predates"
                    )),
                    suggestion: Some(
                        "Run `urd init` to re-render and reinstall the grant.".to_string(),
                    ),
                })
                .collect();
            if !uncertain.is_empty() {
                checks.push(DoctorCheck {
                    name: "sudoers drift".to_string(),
                    status: DoctorCheckStatus::Warn,
                    detail: Some(format!(
                        "{} config mapping(s) have no exact covering grant, but broader \
                         wildcard grants exist that urd does not interpret: {}",
                        uncertain.len(),
                        uncertain.join("; ")
                    )),
                    // No `urd init` here: the resume verb acts only on
                    // definitively missing lines — a hand-managed wildcard
                    // grant is honest uncertainty, never a nag (071).
                    suggestion: Some(
                        "Compare `sudo -l` against the config yourself.".to_string(),
                    ),
                });
            }
            checks
        }
    }
}

/// Diff the units oracle (`systemd_units::expected_units`) against the
/// installed unit files. Pure over the injected exe path and contents map;
/// the filesystem reads stay in `run()`. `exe = None` = the binary path
/// could not be resolved; `installed = None` = no config dir — both honest
/// skips, never a silent pass.
fn build_units_drift_checks(
    config: &Config,
    exe: Option<&std::path::Path>,
    installed: Option<&std::collections::HashMap<String, Option<String>>>,
) -> Vec<DoctorCheck> {
    let cannot_verify = |detail: String| {
        vec![DoctorCheck {
            name: "systemd units drift".to_string(),
            status: DoctorCheckStatus::Warn,
            detail: Some(detail),
            suggestion: Some("Run `urd init` to re-render and reinstall the units.".to_string()),
        }]
    };
    let Some(exe) = exe else {
        return cannot_verify(
            "could not resolve this urd binary's path — cannot verify the installed units"
                .to_string(),
        );
    };
    let expected = match crate::systemd_units::expected_units(&config.general.run_frequency, exe)
    {
        Ok(expected) => expected,
        Err(refusal) => return cannot_verify(refusal.to_string()),
    };
    let Some(installed) = installed else {
        return cannot_verify("no config directory for this user".to_string());
    };

    let drift = crate::systemd_units::diff_units(&expected, installed);
    if drift.is_empty() {
        return vec![DoctorCheck {
            name: "systemd units match this binary".to_string(),
            status: DoctorCheckStatus::Ok,
            detail: None,
            suggestion: None,
        }];
    }
    drift
        .into_iter()
        .map(|d| match d.kind {
            crate::systemd_units::UnitDriftKind::Missing => DoctorCheck {
                name: "systemd units drift".to_string(),
                status: DoctorCheckStatus::Warn,
                detail: Some(format!("{} is not installed", d.name)),
                suggestion: Some("Run `urd init` to complete the seal.".to_string()),
            },
            crate::systemd_units::UnitDriftKind::Differs => {
                // F6: name both paths — the installed ExecStart vs. this
                // binary — so a dev-build doctor run reads as what it is.
                let installed_exec = installed
                    .get(d.name)
                    .and_then(|c| c.as_ref())
                    .and_then(|c| c.lines().find(|l| l.starts_with("ExecStart=")))
                    .unwrap_or("no ExecStart line")
                    .to_string();
                DoctorCheck {
                    name: "systemd units drift".to_string(),
                    status: DoctorCheckStatus::Warn,
                    detail: Some(format!(
                        "{} differs from what this binary ({}) would render \
                         (installed: {installed_exec})",
                        d.name,
                        exe.display()
                    )),
                    suggestion: Some(
                        "Run `urd init` to re-render and reinstall the units.".to_string(),
                    ),
                }
            }
        })
        .collect()
}

/// The linger advisory (UPI 075, adversary F1): user timers fire only
/// while a session exists. `Linger=no` → one Warn with the exact command;
/// an unanswerable loginctl → an honest skip; `Linger=yes` → silence (a
/// real pass needs no row).
fn linger_check() -> Vec<DoctorCheck> {
    let row = |status, detail: String, suggestion: Option<String>| {
        vec![DoctorCheck {
            name: "session lingering".to_string(),
            status,
            detail: Some(detail),
            suggestion,
        }]
    };
    let Ok(user) = crate::commands::seal::invoking_username() else {
        return row(
            DoctorCheckStatus::Warn,
            "could not name the invoking user — cannot check lingering".to_string(),
            None,
        );
    };
    match std::process::Command::new("loginctl")
        .env("LC_ALL", "C")
        .args(["show-user", &user, "--property=Linger"])
        .output()
    {
        Ok(out) if out.status.success() => {
            match String::from_utf8_lossy(&out.stdout).trim() {
                "Linger=no" => row(
                    DoctorCheckStatus::Warn,
                    "lingering is off: backups run only while you are logged in \
                     (missed nights catch up at next login)"
                        .to_string(),
                    Some(format!("Run `loginctl enable-linger {user}` to free them.")),
                ),
                "Linger=yes" => Vec::new(),
                other => row(
                    DoctorCheckStatus::Warn,
                    format!("could not read the lingering state: {other:?}"),
                    None,
                ),
            }
        }
        Ok(out) => row(
            DoctorCheckStatus::Warn,
            format!(
                "loginctl could not answer: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            None,
        ),
        Err(e) => row(
            DoctorCheckStatus::Warn,
            format!("could not run loginctl: {e}"),
            None,
        ),
    }
}

/// Build the `--thorough` Retention-section advisories (#125): one `DoctorCheck`
/// per pin file whose drive label is not in `[[drives]]`. Such an orphan pin
/// silently anchors local retention (the planner protects everything newer than
/// the oldest pin) for a drive that no longer exists in config, overriding the
/// configured shape with no other surface to catch it.
///
/// Pure decision (`chain::orphan_pins`) over filesystem-scanned input
/// (`chain::discover_pin_files`); advisory only — nothing is deleted.
fn build_retention_checks(config: &Config) -> Vec<DoctorCheck> {
    let configured = config.drive_labels();
    let mut checks = Vec::new();

    for sv in config.resolved_subvolumes() {
        let Some(local_dir) = config.local_snapshot_dir(&sv.name) else {
            continue;
        };
        let discovered = crate::chain::discover_pin_files(&local_dir);
        for orphan in crate::chain::orphan_pins(&discovered, &configured) {
            checks.push(DoctorCheck {
                name: format!("orphan pin: {} · {}", sv.name, orphan.label),
                status: DoctorCheckStatus::Warn,
                detail: Some(format!(
                    "{} names {}, but no configured drive has label \"{}\". \
                     Retention will not delete that snapshot or any newer one on the chain.",
                    orphan.path.display(),
                    orphan.snapshot.as_str(),
                    orphan.label,
                )),
                suggestion: Some(format!(
                    "Delete the pin file after confirming {} is permanently retired, \
                     or re-add it to [[drives]].",
                    orphan.label,
                )),
            });
        }
    }
    checks
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

/// Build the `--thorough` Recommendations-section view (UPI 041, UPI 044).
///
/// Iterates resolved subvolumes, computes a recommendation per role from
/// the same rolling-churn aggregate the Churn section uses, classifies
/// headroom severity from observed pool signals, drops aligned-and-healthy
/// rows, escalates Pressure/Critical rows via the synth path (UPI 044 R1),
/// and sorts by recovery magnitude descending.
fn build_doctor_recommendation_view(
    config: &Config,
    state_db: Option<&StateDb>,
) -> DoctorRecommendationView {
    let now = chrono::Local::now().naive_local();
    let window = crate::drift::default_window();
    let pools_grouped = pools::detect_source_pools(config);
    let pools_by_uuid: HashMap<String, Vec<String>> = pools_grouped
        .iter()
        .map(|p| (p.uuid.clone(), p.subvolume_names.clone()))
        .collect();
    let since = now - window;

    build_doctor_recommendation_view_inner(
        config,
        state_db,
        now,
        &pools_grouped,
        |mp: &Path| pools::pool_space(mp).ok(),
        |uuid: &str| pools::metadata_utilization_ratio(uuid),
        |uuid: &str| {
            let names = pools_by_uuid.get(uuid)?;
            // Fail-open per ADR-102: absent db / query error → empty samples →
            // `compute_pool_free_bytes_trend(&[], …)` is `None` (same as the
            // prior `?`-on-error behavior).
            let samples = RealFileSystemState { state: state_db }.drift_samples_multi(names, since);
            crate::drift::compute_pool_free_bytes_trend(&samples, window, now, MIN_SAMPLE_DAYS)
        },
    )
}

/// Minimum distinct sample days required before
/// `compute_pool_free_bytes_trend` returns a slope (UPI 044). Three days
/// of evidence is enough to detect shrinkage without overreacting to a
/// single bad sample.
const MIN_SAMPLE_DAYS: u32 = 3;

#[allow(clippy::too_many_arguments)]
fn build_doctor_recommendation_view_inner(
    config: &Config,
    state_db: Option<&StateDb>,
    now: chrono::NaiveDateTime,
    pools_grouped: &[pools::SourcePool],
    pool_space_resolver: impl Fn(&Path) -> Option<PoolSpace>,
    metadata_resolver: impl Fn(&str) -> Option<f64>,
    pool_trend_resolver: impl Fn(&str) -> Option<i64>,
) -> DoctorRecommendationView {
    let window = crate::drift::default_window();
    let header = format!(
        "based on {}-day churn observation; apply by editing ~/.config/urd/urd.toml",
        window.num_days()
    );

    // ── R8: pre-compute resolver caches at view-build top ────────────
    // pool UUID → trend bytes/day (one query per pool).
    let pool_trend_by_uuid: HashMap<String, Option<i64>> = pools_grouped
        .iter()
        .map(|p| (p.uuid.clone(), pool_trend_resolver(&p.uuid)))
        .collect();

    // pool mountpoint → PoolSpace (one statvfs per pool).
    let mut pool_space_by_mountpoint: HashMap<PathBuf, PoolSpace> = HashMap::new();
    for pool in pools_grouped {
        for mp in &pool.mountpoints {
            if let Some(space) = pool_space_resolver(mp) {
                pool_space_by_mountpoint.insert(mp.clone(), space);
            }
        }
    }

    // subvolume name → (pool mountpoint, pool uuid) so per-row lookup is
    // a HashMap hit rather than a pool-by-pool scan.
    let mut subvol_pool: HashMap<String, (PathBuf, String)> = HashMap::new();
    for pool in pools_grouped {
        let Some(mp) = pool.mountpoints.first().cloned() else {
            continue;
        };
        for name in &pool.subvolume_names {
            subvol_pool.insert(name.clone(), (mp.clone(), pool.uuid.clone()));
        }
    }

    // Destination metadata: (drive label, metadata ratio) for each
    // available drive with a resolvable UUID. The External row's max-of
    // aggregation reads from here.
    let destination_metadata: Vec<(String, f64)> = config
        .drives
        .iter()
        .filter(|d| drives::drive_availability(d) == drives::DriveAvailability::Available)
        .filter_map(|d| {
            let uuid = d.uuid.as_deref()?;
            let ratio = metadata_resolver(uuid)?;
            Some((d.label.clone(), ratio))
        })
        .collect();

    let (max_metadata_label, max_metadata_ratio): (Option<String>, Option<f64>) =
        match destination_metadata
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        {
            Some((label, ratio)) => (Some(label.clone()), Some(*ratio)),
            None => (None, None),
        };

    let resolved = config.resolved_subvolumes();

    let mut rows: Vec<DoctorRecommendationRow> = Vec::new();

    for sv in resolved.iter().filter(|sv| sv.enabled) {
        let churn = compute_churn_for(state_db, &sv.name, window, now);

        // Source-pool signals for this subvolume.
        let source_signals = subvol_pool.get(&sv.name).map(|(mp, uuid)| {
            let space = pool_space_by_mountpoint.get(mp).copied();
            let trend = pool_trend_by_uuid.get(uuid).copied().flatten();
            (space, trend)
        });
        let (source_space, source_trend) = source_signals.unwrap_or((None, None));

        let ctx_local = HeadroomContext {
            source_pool_free_bytes: source_space.map(|s| s.free_bytes),
            source_pool_capacity_bytes: source_space.map(|s| s.capacity_bytes),
            source_pool_trend_bytes_per_day: source_trend,
            destination_metadata_ratio: None,
        };
        let ctx_external = HeadroomContext {
            source_pool_free_bytes: source_space.map(|s| s.free_bytes),
            source_pool_capacity_bytes: source_space.map(|s| s.capacity_bytes),
            source_pool_trend_bytes_per_day: source_trend,
            destination_metadata_ratio: max_metadata_ratio,
        };

        // Local recommendation. `local_current_shape.is_some()` is the
        // "local role used" signal — when None, the subvolume is
        // Transient and never gets a Local row.
        let (local_current_shape, local_rec) = match &sv.local_retention {
            LocalRetentionPolicy::Transient => (None, None),
            LocalRetentionPolicy::Graduated(g) => (
                Some(*g),
                recommendation::recommend_shape_with_headroom(
                    g,
                    &churn,
                    ShapeRole::Local,
                    ctx_local,
                    None,
                ),
            ),
        };

        // External recommendation. `external_current_shape.is_some()` is
        // the "external role used" signal — when None, send is disabled.
        let (external_current_shape, external_rec) = if sv.send_enabled {
            (
                Some(sv.external_retention),
                recommendation::recommend_shape_with_headroom(
                    &sv.external_retention,
                    &churn,
                    ShapeRole::External,
                    ctx_external,
                    max_metadata_label.as_deref(),
                ),
            )
        } else {
            (None, None)
        };

        // UPI 041 silence: drop rec when suggested == current. The
        // headroom-aware decision below may still resurrect the row via
        // the synth path for Pressure/Critical.
        let local_rec = local_rec.filter(|h| h.recommendation.suggested != h.recommendation.current);
        let external_rec = external_rec.filter(|h| h.recommendation.suggested != h.recommendation.current);

        // R1 synth path for cold-churn-but-pressured subvolumes.
        let severity_local = recommendation::classify_headroom_severity(ctx_local);
        let severity_external = recommendation::classify_headroom_severity(ctx_external);

        // Synth-path fallback reason (M3): the Pressure/Critical branch is only
        // reached when the source pool is genuinely low, so `SourcePoolLow` is
        // the honest reason when `pick_reason` returns None (it renders prose;
        // the retired `StorageCritical` reason rendered blank). Unmeasurable
        // free-ratio falls back to `0.0` — only ever read on the genuinely-low
        // branch, where most-tight is the safe direction.
        let source_free_ratio = source_space.and_then(PoolSpace::free_ratio).unwrap_or(0.0);

        let local = match (local_rec, severity_local, local_current_shape) {
            (Some(r), _, _) => Some(r),
            (None, HeadroomSeverity::Pressure, Some(cur)) => {
                let reason = recommendation::pick_reason(ctx_local, severity_local, None)
                    .unwrap_or(AdjustmentReason::SourcePoolLow {
                        free_ratio: source_free_ratio,
                    });
                Some(recommendation::headroom_aware_pointer_only(
                    &cur,
                    ShapeRole::Local,
                    severity_local,
                    reason,
                ))
            }
            _ => None,
        };
        let external = match (external_rec, severity_external, external_current_shape) {
            (Some(r), _, _) => Some(r),
            (None, HeadroomSeverity::Pressure, Some(cur)) => {
                let reason = recommendation::pick_reason(
                    ctx_external,
                    severity_external,
                    max_metadata_label.as_deref(),
                )
                .unwrap_or(AdjustmentReason::SourcePoolLow {
                    free_ratio: source_free_ratio,
                });
                Some(recommendation::headroom_aware_pointer_only(
                    &cur,
                    ShapeRole::External,
                    severity_external,
                    reason,
                ))
            }
            _ => None,
        };

        if local.is_none() && external.is_none() {
            continue;
        }

        let note = local
            .as_ref()
            .and_then(|r| r.recommendation.note)
            .or_else(|| external.as_ref().and_then(|r| r.recommendation.note));

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


/// Per-role recovery bytes. Prefers `adjusted_cost` over `suggested_cost`
/// so the sort order matches what the user sees rendered (R2).
fn role_recovery_bytes(h: &recommendation::HeadroomAwareRecommendation) -> u64 {
    let saved_to = h.adjusted_cost.unwrap_or(h.recommendation.suggested_cost);
    h.recommendation
        .current_cost
        .data_bytes
        .saturating_sub(saved_to.data_bytes)
}

fn recovery_bytes(row: &DoctorRecommendationRow) -> u64 {
    let local = row.local.as_ref().map_or(0, role_recovery_bytes);
    let external = row.external.as_ref().map_or(0, role_recovery_bytes);
    local.saturating_add(external)
}

fn compute_churn_for(
    state_db: Option<&StateDb>,
    name: &str,
    window: chrono::Duration,
    now: chrono::NaiveDateTime,
) -> crate::drift::ChurnEstimate {
    // ADR-102 best-effort: a missing db or a failed query yields empty samples,
    // and `compute_rolling_churn(&[])` is `ChurnEstimate::default()` — never an
    // error that could propagate into a backup decision.
    let samples = RealFileSystemState { state: state_db }.drift_samples(name, now - window);
    crate::drift::compute_rolling_churn(&samples, window, now)
}

/// Pure-ish core (still does DB I/O via `state_db`) — extracted so unit tests
/// can pass an in-memory `StateDb` and a fixed `now`.
fn build_doctor_churn_view_inner(
    config: &Config,
    state_db: Option<&StateDb>,
    now: chrono::NaiveDateTime,
) -> crate::output::DoctorChurnView {
    use crate::output::{DoctorChurnRow, DoctorChurnView};

    let window = crate::drift::default_window();
    let rows: Vec<DoctorChurnRow> = config
        .subvolumes
        .iter()
        .map(|sv| {
            let estimate = compute_churn_for(state_db, &sv.name, window, now);
            DoctorChurnRow {
                name: sv.name.clone(),
                state: crate::output::render_churn(&estimate),
            }
        })
        .collect();

    DoctorChurnView {
        window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ".to_string(),
        rows,
    }
}

/// Map an `ActionableAdvice` onto doctor's `(issue, suggestion, reason)`
/// display fields. UPI 029 (via 079-c): remediation always renders behind
/// the → arrow — `compute_advice` puts machine-runnable fixes in `command`
/// and human-actionable guidance in `reason` ("Connect WD-18TB1 and run
/// `urd backup`", branch 5); with no command the reason IS the suggestion,
/// so promote it rather than leaving the drive-absent row as the one row
/// without a "what do I do" handle.
fn unpack_advice(
    adv: &advice::ActionableAdvice,
) -> (Option<String>, Option<String>, Option<String>) {
    match adv.command.as_ref() {
        Some(c) => (
            Some(adv.issue.clone()),
            Some(format!("Run `{c}`.")),
            adv.reason.clone(),
        ),
        None => (Some(adv.issue.clone()), adv.reason.clone(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sudoers drift (UPI 071) ────────────────────────────────────────

    fn drift_config() -> Config {
        Config::from_str(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "recorded"
"#,
        )
        .unwrap()
    }

    fn listing_of(specs: &[String]) -> String {
        let rules: String = specs
            .iter()
            .map(|s| format!("    (root) NOPASSWD: {s}\n"))
            .collect();
        format!("User alice may run the following commands on example-host:\n{rules}")
    }

    // ── Units drift (UPI 075) ──────────────────────────────────────────

    fn installed_map(
        units: &[crate::systemd_units::UnitFile],
    ) -> std::collections::HashMap<String, Option<String>> {
        units
            .iter()
            .map(|u| (u.name.to_string(), Some(u.content.clone())))
            .collect()
    }

    #[test]
    fn units_all_matching_render_one_ok_row() {
        let config = drift_config();
        let exe = std::path::Path::new("/home/alice/.cargo/bin/urd");
        let expected =
            crate::systemd_units::expected_units(&config.general.run_frequency, exe).unwrap();
        let checks = build_units_drift_checks(&config, Some(exe), Some(&installed_map(&expected)));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Ok);
    }

    #[test]
    fn units_missing_one_warns_with_the_seal_verb() {
        let config = drift_config();
        let exe = std::path::Path::new("/home/alice/.cargo/bin/urd");
        let expected =
            crate::systemd_units::expected_units(&config.general.run_frequency, exe).unwrap();
        let mut installed = installed_map(&expected);
        installed.insert("urd-backup.timer".to_string(), None);
        let checks = build_units_drift_checks(&config, Some(exe), Some(&installed));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        assert!(checks[0].detail.as_deref().unwrap().contains("urd-backup.timer"));
        assert!(checks[0].suggestion.as_deref().unwrap().contains("urd init"));
    }

    /// Adversary F6: a Differs row names both binaries — the installed
    /// ExecStart and the doctor's own path — so a dev-build run
    /// self-diagnoses instead of alarming.
    #[test]
    fn units_differing_detail_names_both_exe_paths() {
        let config = drift_config();
        let sealed_exe = std::path::Path::new("/home/alice/.cargo/bin/urd");
        let doctor_exe = std::path::Path::new("/home/alice/dev/urd/target/debug/urd");
        let sealed =
            crate::systemd_units::expected_units(&config.general.run_frequency, sealed_exe)
                .unwrap();
        let checks =
            build_units_drift_checks(&config, Some(doctor_exe), Some(&installed_map(&sealed)));
        // The timer has no ExecStart and matches; the service differs.
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        let detail = checks[0].detail.as_deref().unwrap();
        assert!(detail.contains("target/debug/urd"), "{detail}");
        assert!(detail.contains(".cargo/bin/urd"), "{detail}");
    }

    #[test]
    fn units_unresolvable_exe_is_an_honest_skip_never_silent() {
        let config = drift_config();
        let checks = build_units_drift_checks(&config, None, None);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        assert!(checks[0].detail.as_deref().unwrap().contains("binary"));
    }

    #[test]
    fn drift_all_covered_renders_one_ok_row() {
        let config = drift_config();
        let expected = crate::sudoers::expected_grant_lines(&config).unwrap();
        let checks = build_sudoers_drift_checks(&config, Some(&listing_of(&expected)));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Ok);
        assert!(checks[0].name.contains("covers the config"));
    }

    #[test]
    fn drift_missing_mapping_warns_with_the_reinstall_verb() {
        let config = drift_config();
        let expected: Vec<String> = crate::sudoers::expected_grant_lines(&config)
            .unwrap()
            .into_iter()
            .filter(|s| !s.contains(" delete "))
            .collect();
        let checks = build_sudoers_drift_checks(&config, Some(&listing_of(&expected)));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        let detail = checks[0].detail.as_deref().unwrap();
        assert!(detail.contains("subvolume delete /data/.snapshots/*"), "{detail}");
        let suggestion = checks[0].suggestion.as_deref().unwrap();
        assert!(suggestion.contains("urd init"), "{suggestion}");
    }

    /// UPI 081 B3 (#280): every expected line missing → this machine was
    /// never sealed, not a config that drifted since. One "not sealed"
    /// line, not N per-spec "predates" rows that bury the real cause.
    #[test]
    fn drift_all_missing_collapses_to_one_not_sealed_line() {
        let config = drift_config();
        // A grant unrelated to any expected line: every expected spec is
        // missing, none uncertain (a real grant that covers nothing urd
        // asked for — distinct from an unparseable/empty listing).
        let unrelated = vec!["/usr/sbin/some-other-tool run".to_string()];
        let checks = build_sudoers_drift_checks(&config, Some(&listing_of(&unrelated)));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        let detail = checks[0].detail.as_deref().unwrap();
        assert!(detail.contains("isn't sealed yet"), "{detail}");
        let suggestion = checks[0].suggestion.as_deref().unwrap();
        assert!(suggestion.contains("urd init"), "{suggestion}");
    }

    #[test]
    fn drift_wildcard_grants_are_honest_uncertainty_not_missing() {
        let config = drift_config();
        let mut specs: Vec<String> = crate::sudoers::expected_grant_lines(&config)
            .unwrap()
            .into_iter()
            .filter(|s| !s.contains("snapshot -r"))
            .collect();
        specs.push("/usr/sbin/btrfs subvolume snapshot -r /data/* /data/.snapshots/*".to_string());
        let checks = build_sudoers_drift_checks(&config, Some(&listing_of(&specs)));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        let detail = checks[0].detail.as_deref().unwrap();
        assert!(detail.contains("does not interpret"), "{detail}");
        // Uncertainty never points at the resume verb: `urd init`'s deep
        // gate acts only on definitively missing lines, so naming it here
        // would be a dead suggestion (and a nag for hand-managed grants).
        let suggestion = checks[0].suggestion.as_deref().unwrap();
        assert!(!suggestion.contains("urd init"), "{suggestion}");
    }

    #[test]
    fn drift_unlistable_is_an_honest_skip_never_silent() {
        let checks = build_sudoers_drift_checks(&drift_config(), None);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        assert!(checks[0].detail.as_deref().unwrap().contains("password"));
    }

    #[test]
    fn drift_unparseable_listing_is_an_honest_skip() {
        let checks = build_sudoers_drift_checks(&drift_config(), Some("garbage output"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
    }

    // ── unpack_advice (UPI 029 Change 4, via 079-c) ───────────────────

    #[test]
    fn unpack_advice_keeps_command_as_suggestion_with_reason_context() {
        let adv = advice::ActionableAdvice {
            subvolume: "music".to_string(),
            issue: "waning — last external send 43h ago".to_string(),
            command: Some("urd backup --subvolume music".to_string()),
            reason: Some("Drive is mounted; a send closes the gap.".to_string()),
        };
        let (issue, suggestion, reason) = unpack_advice(&adv);
        assert_eq!(issue.as_deref(), Some("waning — last external send 43h ago"));
        assert_eq!(
            suggestion.as_deref(),
            Some("Run `urd backup --subvolume music`.")
        );
        assert_eq!(reason.as_deref(), Some("Drive is mounted; a send closes the gap."));
    }

    #[test]
    fn unpack_advice_promotes_reason_when_no_command() {
        // Branch 5 (drive absent): the guidance lives in `reason` — it must
        // reach the → suggestion slot, not render as dimmed context.
        let adv = advice::ActionableAdvice {
            subvolume: "music".to_string(),
            issue: "waning — last external send 3d ago".to_string(),
            command: None,
            reason: Some("Connect WD-18TB1 and run `urd backup`".to_string()),
        };
        let (_, suggestion, reason) = unpack_advice(&adv);
        assert_eq!(
            suggestion.as_deref(),
            Some("Connect WD-18TB1 and run `urd backup`"),
            "reason promotes to the arrowed suggestion slot"
        );
        assert_eq!(reason, None, "promoted guidance must not render twice");
    }

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

    // ── #125 Retention: orphan-pin advisories ──────────────────────

    fn drive(label: &str) -> crate::config::DriveConfig {
        crate::config::DriveConfig {
            label: label.to_string(),
            uuid: None,
            mount_path: std::path::PathBuf::from(format!("/mnt/{label}")),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Offsite,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        }
    }

    #[test]
    fn retention_checks_flags_orphan_pin() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = cfg();
        config.local_snapshots.roots[0].path = dir.path().to_path_buf();
        config.drives.push(drive("WD-18TB"));

        let alpha = dir.path().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        // One configured pin (fine) + one orphan from a removed drive.
        std::fs::write(
            alpha.join(".last-external-parent-WD-18TB"),
            "20260516-0401-alpha\n",
        )
        .unwrap();
        std::fs::write(
            alpha.join(".last-external-parent-2TB-backup"),
            "20260402-1925-alpha\n",
        )
        .unwrap();

        let checks = build_retention_checks(&config);
        assert_eq!(checks.len(), 1, "only the orphan pin should warn");
        assert_eq!(checks[0].status, DoctorCheckStatus::Warn);
        assert!(checks[0].name.contains("alpha"));
        assert!(checks[0].name.contains("2TB-backup"));
        let detail = checks[0].detail.as_ref().unwrap();
        assert!(detail.contains("20260402-1925-alpha"));
        assert!(detail.contains("2TB-backup"));
        assert!(checks[0].suggestion.is_some());
    }

    #[test]
    fn retention_checks_empty_when_all_pins_configured() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = cfg();
        config.local_snapshots.roots[0].path = dir.path().to_path_buf();
        config.drives.push(drive("WD-18TB"));

        let alpha = dir.path().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::write(
            alpha.join(".last-external-parent-WD-18TB"),
            "20260516-0401-alpha\n",
        )
        .unwrap();

        assert!(
            build_retention_checks(&config).is_empty(),
            "no orphan pins → no false gravity"
        );
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

    /// Helper that calls `build_doctor_recommendation_view_inner` with
    /// no-op resolvers — appropriate for tests that don't exercise the
    /// UPI 044 headroom path. Tests that DO exercise it pass their own
    /// closures via the full signature.
    fn build_view_for_tests(
        config: &Config,
        state_db: Option<&StateDb>,
        now: chrono::NaiveDateTime,
    ) -> DoctorRecommendationView {
        build_doctor_recommendation_view_inner(
            config,
            state_db,
            now,
            &[],
            |_| None,
            |_| None,
            |_| None,
        )
    }

    #[test]
    fn recommendation_view_empty_when_no_subvolumes() {
        let config = empty_cfg();
        let db = StateDb::open_memory().unwrap();
        let view = build_view_for_tests(&config, Some(&db), now_fixed());
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
        let view = build_view_for_tests(&config, Some(&db), now);
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
        let view = build_view_for_tests(&config, Some(&db), now);
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
        let view = build_view_for_tests(&config, Some(&db), now);
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
        let view = build_view_for_tests(&config, Some(&db), now);
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
        let view = build_view_for_tests(&config, Some(&db), now);
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
        let view = build_view_for_tests(&config, Some(&db), now);

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

    // ── UPI 044: headroom-aware recommendations ──────────────────────

    /// Configure two subvolumes "hot" + "cold" with a single pool, plus
    /// helpers for headroom signals.
    fn upi044_cfg_with_pool() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-doctor-upi044/urd.db"
metrics_file = "/tmp/urd-doctor-upi044/backup.prom"
log_dir = "/tmp/urd-doctor-upi044"
heartbeat_file = "/tmp/urd-doctor-upi044/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["hot", "cold"] }
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
name = "hot"
short_name = "hot"
source = "/data/hot"

[[subvolumes]]
name = "cold"
short_name = "cold"
source = "/data/cold"
"#;
        toml::from_str(toml_str).unwrap()
    }

    fn pool_for_subvols(names: &[&str]) -> pools::SourcePool {
        pools::SourcePool {
            uuid: "pool-uuid-test".to_string(),
            mountpoints: vec![std::path::PathBuf::from("/data")],
            subvolume_names: names.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// Tight PoolSpace resolver (~10% free → Pressure), ignoring mountpoint.
    fn tight_pool_resolver(_mp: &Path) -> Option<PoolSpace> {
        Some(PoolSpace {
            free_bytes: 100_000_000_000,
            capacity_bytes: 1_000_000_000_000,
        })
    }

    #[test]
    fn silent_healthy_row_omitted() {
        // Cold churn → cold engine clamps shape to max == matches default
        // shape; no Caution/Pressure signal → no row.
        let config = upi044_cfg_with_pool();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        // 81 B/s cold rate.
        seed_incremental(&db, "cold", now - chrono::Duration::hours(12), 7_000_000);
        let pools = vec![pool_for_subvols(&["cold"])];
        let view = build_doctor_recommendation_view_inner(
            &config,
            Some(&db),
            now,
            &pools,
            |_mp| Some(PoolSpace { free_bytes: 500_000_000_000, capacity_bytes: 1_000_000_000_000 }),
            |_uuid| None,
            |_uuid| None,
        );
        assert!(
            !view.rows.iter().any(|r| r.name == "cold"),
            "cold subvol with Healthy headroom must be silent: {view:?}"
        );
    }

    #[test]
    fn silent_caution_row_omitted() {
        // Cold subvol, Caution-tier free ratio (20%), shape already optimal.
        // D16: Caution alone does NOT escalate a silent row.
        let config = upi044_cfg_with_pool();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        seed_incremental(&db, "cold", now - chrono::Duration::hours(12), 7_000_000);
        let pools = vec![pool_for_subvols(&["cold"])];
        let view = build_doctor_recommendation_view_inner(
            &config,
            Some(&db),
            now,
            &pools,
            |_mp| Some(PoolSpace { free_bytes: 200_000_000_000, capacity_bytes: 1_000_000_000_000 }),
            |_uuid| None,
            |_uuid| None,
        );
        assert!(
            !view.rows.iter().any(|r| r.name == "cold"),
            "Caution must not escalate a silent row: {view:?}"
        );
    }

    #[test]
    fn cold_subvolume_pressure_synthesizes_row() {
        // Cold subvol with NO churn signal AND Pressure-tier headroom →
        // synth row appears with reason but no shape.
        let config = upi044_cfg_with_pool();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        // Don't seed any drift samples for "cold" — churn is None.
        let pools = vec![pool_for_subvols(&["cold"])];
        let view = build_doctor_recommendation_view_inner(
            &config,
            Some(&db),
            now,
            &pools,
            |_mp| Some(PoolSpace { free_bytes: 100_000_000_000, capacity_bytes: 1_000_000_000_000 }),
            |_uuid| None,
            |_uuid| None,
        );
        let cold = view
            .rows
            .iter()
            .find(|r| r.name == "cold")
            .expect("cold row synthesized at Pressure");
        let local = cold.local.as_ref().expect("local synth");
        assert_eq!(local.severity, HeadroomSeverity::Pressure);
        assert!(matches!(
            local.reason,
            Some(AdjustmentReason::SourcePoolLow { .. })
        ));
        // Synth: suggested == current, both costs zero.
        assert_eq!(local.recommendation.suggested, local.recommendation.current);
        assert_eq!(local.recommendation.current_cost.data_bytes, 0);
    }

    #[test]
    fn cold_subvolume_caution_does_not_synthesize_row() {
        // Cold subvol with no churn AND Caution-tier headroom → no synth.
        let config = upi044_cfg_with_pool();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        let pools = vec![pool_for_subvols(&["cold"])];
        let view = build_doctor_recommendation_view_inner(
            &config,
            Some(&db),
            now,
            &pools,
            // 20% free → Caution.
            |_mp| Some(PoolSpace { free_bytes: 200_000_000_000, capacity_bytes: 1_000_000_000_000 }),
            |_uuid| None,
            |_uuid| None,
        );
        assert!(
            !view.rows.iter().any(|r| r.name == "cold"),
            "Caution must not synthesize for cold subvol"
        );
    }

    #[test]
    fn recovery_bytes_uses_adjusted_cost_when_present() {
        // R2: recovery_bytes prefers adjusted_cost over suggested_cost
        // so sort order matches what voice renders.
        use crate::recommendation::{
            CostProjection, HeadroomAwareRecommendation, ShapeRecommendation, ShapeRole,
        };
        use crate::types::{MonthlyCount, ResolvedGraduatedRetention};
        let shape = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 60,
            weekly: 52,
            monthly: MonthlyCount::Count(24),
            yearly: 0,
        };
        let h = HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role: ShapeRole::External,
                current: shape,
                suggested: shape,
                current_cost: CostProjection {
                    data_bytes: 200_000_000_000,
                    snapshot_count: 136,
                },
                suggested_cost: CostProjection {
                    data_bytes: 50_000_000_000,
                    snapshot_count: 136,
                },
                note: None,
            },
            severity: HeadroomSeverity::Pressure,
            reason: None,
            adjusted: Some(shape),
            adjusted_cost: Some(CostProjection {
                data_bytes: 25_000_000_000,
                snapshot_count: 136,
            }),
        };
        let row = DoctorRecommendationRow {
            name: "x".to_string(),
            local: None,
            external: Some(h),
            note: None,
            was_named_level: None,
        };
        // current=200, adjusted=25 → recovery = 175 GB (not 150 GB from suggested).
        assert_eq!(recovery_bytes(&row), 175_000_000_000);
    }

    #[test]
    fn recovery_bytes_uses_inner_shape_recommendation_when_no_adjusted() {
        // When adjusted_cost is None, fall back to suggested_cost.
        use crate::recommendation::{
            CostProjection, HeadroomAwareRecommendation, ShapeRecommendation, ShapeRole,
        };
        use crate::types::{MonthlyCount, ResolvedGraduatedRetention};
        let shape = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 60,
            weekly: 52,
            monthly: MonthlyCount::Count(24),
            yearly: 0,
        };
        let h = HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role: ShapeRole::Local,
                current: shape,
                suggested: shape,
                current_cost: CostProjection {
                    data_bytes: 200_000_000_000,
                    snapshot_count: 136,
                },
                suggested_cost: CostProjection {
                    data_bytes: 50_000_000_000,
                    snapshot_count: 136,
                },
                note: None,
            },
            severity: HeadroomSeverity::Healthy,
            reason: None,
            adjusted: None,
            adjusted_cost: None,
        };
        let row = DoctorRecommendationRow {
            name: "x".to_string(),
            local: Some(h),
            external: None,
            note: None,
            was_named_level: None,
        };
        assert_eq!(recovery_bytes(&row), 150_000_000_000);
    }

    #[test]
    fn external_role_synthesizes_pressure_from_source_pool() {
        // The External role synthesizes a Pressure row from the source-pool
        // free ratio when send is enabled and churn is cold. (Adapted from
        // the former `headroom_ctx_external_carries_max_drive_label`, which
        // leaned on the now-deleted Critical-injection closure.) A tight pool
        // (10% free → Pressure) synthesizes an External row because send is
        // enabled; no churn, no metadata signal.
        let config = upi044_cfg_with_pool();
        let db = StateDb::open_memory().unwrap();
        let now = now_fixed();
        let pools = vec![pool_for_subvols(&["hot"])];
        let view = build_doctor_recommendation_view_inner(
            &config,
            Some(&db),
            now,
            &pools,
            tight_pool_resolver,
            |_| None,
            |_| None,
        );
        let hot = view.rows.iter().find(|r| r.name == "hot").expect("hot row");
        let external = hot.external.as_ref().expect("external Pressure synth");
        assert_eq!(external.severity, HeadroomSeverity::Pressure);
        assert!(matches!(
            external.reason,
            Some(AdjustmentReason::SourcePoolLow { .. })
        ));
    }
}
