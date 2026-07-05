//! The seal's I/O: resume-at-first-incomplete-stage over the closing
//! ceremony. Today the seal has one stage — **the earning** (UPI 071):
//! consented, fail-closed privilege bootstrap. 075 appends its stages
//! (units, first snapshot, adoption) to `resume_seal` without rework.
//!
//! The earning's install sequence (adversary F1, user-approved deviation
//! from the grill's literal command):
//!   render (pure) → unprivileged `visudo -c -f` gate → show + consent →
//!   `sudo install` to `/etc/sudoers.d/urd.staging` (the `.` in the name
//!   makes includedir ignore it — inert if anything stops here) →
//!   root-side read-back byte-compared to the shown render (binds what
//!   activates to what was consented, closing the same-uid temp-swap
//!   window) → `sudo visudo -c -f` on the root-owned staged bytes →
//!   atomic `sudo mv` into place (no partial file is ever active) →
//!   `sudo -k` + passwordless probe + coverage cross-check.
//!
//! Never `sudo tee`. Every failure path leaves either nothing or an
//! inert file, and says so. TTY-gated by both callers (the Encounter's
//! post-carve tail and `urd init`'s resume).

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};

use crate::config::Config;
use crate::output::OutputMode;
use crate::sudoers::{self, Coverage, GrantProbe, RenderContext};
use crate::voice;

/// The active grant file. The name carries no `.` — includedir reads it.
pub const SUDOERS_DEST: &str = "/etc/sudoers.d/urd";
/// The staging name. The `.` makes includedir skip it: inert by design.
const SUDOERS_STAGING: &str = "/etc/sudoers.d/urd.staging";

/// How the seal's privilege stage ended. All outcomes are honest
/// conclusions, not errors — the carve already succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOutcome {
    /// Grant installed, probe answered, coverage confirmed.
    Sealed,
    /// Grant installed and probing; coverage could not be confirmed.
    SealedUnverifiedCoverage,
    /// Nothing installed (declined, deferred, or sudo unavailable).
    Declined,
    /// Installed, but the passwordless probe did not answer.
    InstalledUnverified,
}

/// Resume the seal at its first incomplete stage (UPI 075). Stage order:
/// earning → adoption → units → first snapshot → send offer → second look →
/// summary. Each stage opens with an idempotent done-check, so `urd init`
/// re-enters cleanly from any interruption; no stage failure unwinds a
/// prior stage (ADR-109).
///
/// The earning's outcome decides continuation per variant (adversary F4):
/// only a grant that answers lets threads spin. `Declined` and
/// `InstalledUnverified` stop here with their honest sentences already
/// printed — running a backup that is guaranteed to fail at `sudo btrfs`
/// would bury the earning's conclusion under executor noise.
pub fn resume_seal(config: &Config, config_path: &Path) -> anyhow::Result<SealOutcome> {
    let outcome = earn_privilege(config, config_path)?;
    match outcome {
        SealOutcome::Sealed | SealOutcome::SealedUnverifiedCoverage => {}
        SealOutcome::Declined | SealOutcome::InstalledUnverified => return Ok(outcome),
    }

    adopt_drives(config);
    let units_enabled = install_units(config);
    let first_thread_spun = first_snapshot(config, config_path);
    let send = if first_thread_spun {
        offer_first_send(config, config_path)
    } else {
        crate::output::SealSendState::NotApplicable
    };

    // Stage 6: the privileged second look — annotation only, one sentence
    // in the summary, never a re-opened conversation (arc Q1). Only with a
    // grant that answers (reuse the stage-1 outcome; no extra probe, no
    // extra auth-log line).
    let uncovered = second_look(
        config,
        &crate::btrfs::RealBtrfs::for_reads(&config.general.btrfs_path),
    );

    // Stage 7: the summary scroll and the handoff to `urd status`.
    let summary = crate::output::SealSummary {
        threads: config
            .resolved_subvolumes()
            .iter()
            .filter(|sv| sv.enabled)
            .map(|sv| crate::output::SealThread {
                name: sv.name.clone(),
                level: sv.protection_level.map(|l| l.to_string()),
            })
            .collect(),
        units_enabled,
        next_action: voice::describe_next_action(&config.general.run_frequency),
        linger_loose: linger_loose().is_some(),
        first_thread_spun,
        send,
        uncovered_subvolumes: uncovered,
    };
    print!("{}", voice::render_seal_summary(&summary));

    Ok(outcome)
}

// ── Stage 6: the privileged second look ─────────────────────────────────

/// Count the subvolumes on the promised pools that no promise covers.
/// Annotation, not verification: any failure (probe, findmnt, parse) omits
/// the note rather than scaring — `None` renders as silence.
fn second_look(config: &Config, btrfs: &dyn crate::btrfs::BtrfsRead) -> Option<usize> {
    // Map every enabled source into its pool's subvol-path space
    // (adversary F2): `btrfs subvolume list` prints paths relative to the
    // FILESYSTEM root, while config paths are mount-space. findmnt's FSROOT
    // is the mounted subvolume's path within the filesystem.
    let mut pools: std::collections::BTreeMap<PathBuf, PoolView> =
        std::collections::BTreeMap::new();
    for sv in config.resolved_subvolumes().iter().filter(|sv| sv.enabled) {
        let (mount, fsroot) = mount_and_fsroot(&sv.source)?;
        let view = pools.entry(mount.clone()).or_insert_with(|| PoolView {
            fsroot: fsroot.clone(),
            covered: Vec::new(),
            snapshot_homes: Vec::new(),
        });
        view.covered
            .push(fs_relative(&sv.source, &mount, &fsroot)?);
        if let Some(dir) = config.local_snapshot_dir(&sv.name)
            && dir.starts_with(&mount)
        {
            view.snapshot_homes.push(fs_relative(&dir, &mount, &fsroot)?);
        }
    }

    let mut listings = Vec::new();
    for (mount, view) in pools {
        let listing = btrfs.list_subvolumes(&mount).ok()?;
        listings.push((listing, view));
    }
    Some(count_uncovered(&listings))
}

/// One promised pool as the classifier sees it: everything in the pool's
/// own subvol-path coordinates.
struct PoolView {
    #[allow(dead_code)] // fsroot is consumed before the view is stored
    fsroot: PathBuf,
    covered: Vec<PathBuf>,
    snapshot_homes: Vec<PathBuf>,
}

/// The pure classification (adversary F2 fixtures drive this): a listed
/// subvolume counts as uncovered unless it IS a covered source, lives under
/// a snapshot home, carries a `.snapshots` component (snapper convention +
/// urd's own homes), or is docker layer machinery.
fn count_uncovered(listings: &[(Vec<PathBuf>, PoolView)]) -> usize {
    listings
        .iter()
        .map(|(listing, view)| {
            listing
                .iter()
                .filter(|sub| {
                    !view.covered.iter().any(|c| *sub == c)
                        && !view.snapshot_homes.iter().any(|h| sub.starts_with(h))
                        && !sub
                            .components()
                            .any(|c| c.as_os_str() == ".snapshots")
                        && !sub.starts_with("var/lib/docker")
                })
                .count()
        })
        .sum()
}

/// A config path in its pool's subvol-path space: FSROOT (the mounted
/// subvolume's in-filesystem path) joined with the path below the mount.
fn fs_relative(path: &Path, mount: &Path, fsroot: &Path) -> Option<PathBuf> {
    let below = path.strip_prefix(mount).ok()?;
    let root = fsroot.strip_prefix("/").unwrap_or(fsroot);
    Some(root.join(below))
}

/// One findmnt call: the mountpoint holding `path` and that mount's FSROOT.
fn mount_and_fsroot(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = Command::new("findmnt")
        .env("LC_ALL", "C")
        .args(["-n", "-P", "-o", "TARGET,FSROOT", "--target"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_findmnt_target_fsroot(&String::from_utf8_lossy(&out.stdout))
}

/// Pure parse of `findmnt -P -o TARGET,FSROOT`: `TARGET="/" FSROOT="/root"`.
fn parse_findmnt_target_fsroot(stdout: &str) -> Option<(PathBuf, PathBuf)> {
    let extract = |key: &str| -> Option<PathBuf> {
        let needle = format!("{key}=\"");
        let start = stdout.find(&needle)? + needle.len();
        let rest = &stdout[start..];
        Some(PathBuf::from(&rest[..rest.find('"')?]))
    };
    Some((extract("TARGET")?, extract("FSROOT")?))
}

// ── Stage 4: the first local snapshot ───────────────────────────────────

/// Spin the first thread: pre-create the local snapshot homes (#250 — the
/// executor assumes its parents exist), then run the normal backup pipeline
/// scoped local-only. Reusing `backup::run` wholesale means the first run
/// behaves exactly like every future run: lock, per-subvolume isolation,
/// watchdog, heartbeat, and the honest summary all come with it. Returns
/// whether the promises now hold at least one local thread (the send
/// offer's gate — a failed spin leaves nothing to send).
fn first_snapshot(config: &Config, config_path: &Path) -> bool {
    for root in &config.local_snapshots.roots {
        if let Err(e) = std::fs::create_dir_all(&root.path) {
            print!("{}", voice::render_data_dir_failed(&root.path, &e.to_string()));
        }
    }

    if !first_thread_pending(config) {
        print!("{}", voice::render_first_thread_already());
        return true;
    }

    print!("{}", voice::render_first_thread_intro());
    if let Err(e) = run_seal_backup(config_path, /* local_only */ true) {
        print!("{}", voice::render_first_thread_failed(&format!("the run failed: {e}")));
        return false;
    }
    // The executor isolates per-subvolume failures without erring the run —
    // ask the filesystem of truth, not the exit path.
    if first_thread_pending(config) {
        print!(
            "{}",
            voice::render_first_thread_failed(
                "some promises still have no local snapshot (the summary above names them)",
            )
        );
        return false;
    }
    true
}

/// True while any enabled subvolume that PLANS local snapshots (adversary
/// F5 — a mapping-less subvolume must not pend forever) has none on disk.
/// Filesystem of truth via `plan::read_snapshot_dir` (absent dir = none).
fn first_thread_pending(config: &Config) -> bool {
    config
        .resolved_subvolumes()
        .iter()
        .filter(|sv| sv.enabled)
        .any(|sv| match config.local_snapshot_dir(&sv.name) {
            None => false,
            Some(dir) => crate::plan::read_snapshot_dir(&dir)
                .map(|snaps| snaps.is_empty())
                .unwrap_or(false),
        })
}

// ── Stage 5: the first-send offer ───────────────────────────────────────

/// The user's answer at the send offer. Enter sends now (the recommended
/// path — protected before the terminal closes); EOF and `t`/`q` defer to
/// tonight's timer, the least destructive reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendChoice {
    SendNow,
    Tonight,
}

/// Classify one consent line.
fn parse_send_choice(line: &str) -> Option<SendChoice> {
    match line.trim().to_lowercase().as_str() {
        "" | "s" => Some(SendChoice::SendNow),
        "t" | "q" => Some(SendChoice::Tonight),
        _ => None,
    }
}

/// Offer the first send when it means something: a promise wants sends, a
/// configured drive is reachable, and nothing has been sent yet. Declining
/// is recorded nowhere (arc Q5) — a later `urd init` re-offers, honestly.
fn offer_first_send(config: &Config, config_path: &Path) -> crate::output::SealSendState {
    use crate::output::SealSendState;
    if !wants_send(&config.resolved_subvolumes()) {
        return SealSendState::NotApplicable;
    }
    let reachable: Vec<&crate::config::DriveConfig> = config
        .drives
        .iter()
        .filter(|d| {
            use crate::drives::DriveAvailability as A;
            matches!(
                crate::drives::drive_availability(d),
                A::Available | A::TokenMissing | A::TokenMismatch { .. } | A::TokenExpectedButMissing
            )
        })
        .collect();
    if reachable.is_empty() {
        return SealSendState::NotApplicable;
    }
    if already_sent(&reachable, &config.resolved_subvolumes()) {
        return SealSendState::Sent;
    }

    print!("{}", voice::render_send_offer());
    let choice = loop {
        match crate::commands::encounter::read_input_line() {
            Err(_) | Ok(None) => break SendChoice::Tonight,
            Ok(Some(line)) => match parse_send_choice(&line) {
                Some(choice) => break choice,
                None => print!("{}", voice::render_send_offer()),
            },
        }
    };
    match choice {
        SendChoice::Tonight => {
            print!("{}", voice::render_send_deferred());
            SealSendState::Tonight
        }
        SendChoice::SendNow => match run_seal_backup(config_path, /* local_only */ false) {
            Ok(()) => SealSendState::Sent,
            Err(e) => {
                print!(
                    "{}",
                    voice::render_first_thread_failed(&format!("the send failed: {e}"))
                );
                SealSendState::Tonight
            }
        },
    }
}

/// One manual-mode run through the full backup pipeline, scoped to the
/// seal's needs: `local_only` spins the first threads, its inverse
/// (external-only) carries the first send. `backup::run` takes the config
/// by value and `Config` is not `Clone`: reload from disk, the same
/// posture as `post_carve` (a delve edit may have changed mappings —
/// sealing from a stale value ships drift).
fn run_seal_backup(config_path: &Path, local_only: bool) -> anyhow::Result<()> {
    let fresh = crate::config::Config::load(Some(config_path))?;
    crate::commands::backup::run(
        fresh,
        crate::cli::BackupArgs {
            dry_run: false,
            priority: None,
            subvolume: None,
            local_only,
            external_only: !local_only,
            confirm_retention_change: false,
            force_full: false,
            auto: false,
            force_snapshot: false,
        },
    )
}

/// Does any enabled promise want external sends at all? (Pure.)
fn wants_send(resolved: &[crate::config::ResolvedSubvolume]) -> bool {
    resolved.iter().any(|sv| sv.enabled && sv.send_enabled)
}

/// Has any reachable drive already received a snapshot for a send-enabled
/// promise? Filesystem of truth over the drives' snapshot homes.
fn already_sent(
    drives: &[&crate::config::DriveConfig],
    resolved: &[crate::config::ResolvedSubvolume],
) -> bool {
    drives.iter().any(|drive| {
        resolved
            .iter()
            .filter(|sv| sv.enabled && sv.send_enabled && sv.accepts_drive(&drive.label))
            .any(|sv| {
                let dir = crate::drives::external_snapshot_dir(drive, &sv.name);
                crate::plan::read_snapshot_dir(&dir)
                    .map(|snaps| !snaps.is_empty())
                    .unwrap_or(false)
            })
    })
}

// ── Stage 2: drive adoption ─────────────────────────────────────────────

/// Adopt every configured drive that is mounted with a verified UUID:
/// create its snapshot home (a side effect of the token write) and settle
/// its identity token, reusing the `urd drives adopt` decision verbatim.
/// Unreachable drives get one honest sentence each and the seal continues —
/// the grilled adoption-fail/local-ok state (ADR-109). SQLite trouble never
/// blocks adoption (ADR-102): the on-disk token is the identity that
/// matters; the DB record self-heals on the next `verify_drive_token`.
fn adopt_drives(config: &Config) {
    use crate::drives::{self, DriveAvailability};
    use crate::state::StateDb;

    if config.drives.is_empty() {
        return;
    }

    let state = match StateDb::open(&config.general.state_db) {
        Ok(db) => Some(db),
        Err(e) => {
            log::warn!("state DB unavailable during adoption, continuing without: {e}");
            None
        }
    };
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    for drive in &config.drives {
        let reason = match drives::drive_availability(drive) {
            DriveAvailability::Available
            | DriveAvailability::TokenMissing
            | DriveAvailability::TokenMismatch { .. }
            | DriveAvailability::TokenExpectedButMissing => {
                match adopt_one(drive, state.as_ref(), &now) {
                    Ok(action) => {
                        print!("{}", voice::render_seal_adoption(&drive.label, &action));
                        continue;
                    }
                    Err(e) => format!("adoption failed: {e}"),
                }
            }
            DriveAvailability::NotMounted => {
                format!("not mounted at {}", drive.mount_path.display())
            }
            DriveAvailability::UuidMismatch { expected, found } => format!(
                "a different filesystem sits at {} (expected UUID {expected}, found {found})",
                drive.mount_path.display()
            ),
            DriveAvailability::UuidCheckFailed(reason) => {
                format!("its identity could not be verified: {reason}")
            }
        };
        print!(
            "{}",
            voice::render_seal_adoption_skipped(&drive.label, &reason)
        );
    }
}

// ── Stage 3: systemd units ──────────────────────────────────────────────

/// What `systemctl --user is-enabled <unit>` answered, classified. Exit 1
/// with a state word on stdout is a clean "disabled"; an empty stdout means
/// no user manager answered (container, ssh session without a bus) — a
/// different sentence, never a doomed consent prompt (adversary F8).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnabledProbe {
    Enabled,
    NotEnabled,
    NoManager(String),
}

/// Classify one `is-enabled` invocation from its raw pieces (pure).
fn classify_is_enabled(stdout: &str, stderr: &str) -> EnabledProbe {
    match stdout.trim() {
        "enabled" => EnabledProbe::Enabled,
        "" => EnabledProbe::NoManager(stderr.trim().to_string()),
        _ => EnabledProbe::NotEnabled,
    }
}

/// The user's answer at the units asking. Enter installs; EOF skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitsChoice {
    Install,
    Skip,
}

/// Classify one consent line: `""`/`i` install, `q` not now.
fn parse_units_choice(line: &str) -> Option<UnitsChoice> {
    match line.trim().to_lowercase().as_str() {
        "" | "i" => Some(UnitsChoice::Install),
        "q" => Some(UnitsChoice::Skip),
        _ => None,
    }
}

/// Install and enable the unit set the cadence answer selected: oracle
/// render (exe-substituted), consent, plain writes into the user's own
/// systemd directory, `daemon-reload` + `enable --now`, then the linger
/// truth (adversary F1). Every failure is one honest sentence and the seal
/// continues — units-fail is an enumerated, `urd init`-resumable state.
/// Returns whether the units ended up installed and enabled.
fn install_units(config: &Config) -> bool {
    let exe = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(p) => p,
        Err(e) => {
            print!(
                "{}",
                voice::render_units_failed("resolving the urd binary path", &e.to_string())
            );
            return false;
        }
    };
    let units = match crate::systemd_units::expected_units(&config.general.run_frequency, &exe)
    {
        Ok(units) => units,
        Err(refusal) => {
            print!("{}", voice::render_units_failed("rendering the units", &refusal.reason));
            return false;
        }
    };
    let Some(dir) = dirs::config_dir().map(|d| d.join("systemd/user")) else {
        print!(
            "{}",
            voice::render_units_failed("locating ~/.config", "no config directory for this user")
        );
        return false;
    };
    let next_action = voice::describe_next_action(&config.general.run_frequency);

    // Enabled-state probe doubles as the user-manager reachability check.
    let enabled = enabled_units(&units);
    if let Some(EnabledProbe::NoManager(detail)) =
        enabled.iter().find(|p| matches!(p, EnabledProbe::NoManager(_)))
    {
        print!("{}", voice::render_units_no_manager(detail));
        return false;
    }

    // Done-detection: byte-true files + everything enabled → nothing to ask.
    let installed = installed_unit_contents(&units, &dir);
    if crate::systemd_units::diff_units(&units, &installed).is_empty()
        && enabled.iter().all(|p| *p == EnabledProbe::Enabled)
    {
        print!("{}", voice::render_units_already(&next_action));
        print_linger_notice();
        return true;
    }

    let names: Vec<&str> = units.iter().map(|u| u.name).collect();
    print!("{}", voice::render_units_request(&names, &dir, &next_action));
    loop {
        match crate::commands::encounter::read_input_line() {
            Err(e) => {
                print!("{}", voice::render_units_failed("reading your answer", &e.to_string()));
                return false;
            }
            // EOF is walking away — the least destructive reading.
            Ok(None) => {
                print!("{}", voice::render_units_skipped());
                return false;
            }
            Ok(Some(line)) => match parse_units_choice(&line) {
                Some(UnitsChoice::Install) => break,
                Some(UnitsChoice::Skip) => {
                    print!("{}", voice::render_units_skipped());
                    return false;
                }
                None => print!("{}", voice::render_units_request(&names, &dir, &next_action)),
            },
        }
    }

    if let Err(e) = write_units(&units, &dir) {
        print!("{}", voice::render_units_failed("writing the unit files", &e.to_string()));
        return false;
    }
    if let Err(detail) = systemctl_user(&["daemon-reload"]) {
        print!("{}", voice::render_units_failed("systemctl --user daemon-reload", &detail));
        return false;
    }
    for unit in enableable(&units) {
        if let Err(detail) = systemctl_user(&["enable", "--now", unit]) {
            print!(
                "{}",
                voice::render_units_failed(&format!("enabling {unit}"), &detail)
            );
            return false;
        }
    }
    print!("{}", voice::render_units_installed(&next_action));
    print_linger_notice();
    true
}

/// The units that get `enable --now`: the timer (which starts the service
/// by schedule, never a backup run now) and, when present, the sentinel
/// daemon. The backup service itself is timer-started, not enabled.
fn enableable(units: &[crate::systemd_units::UnitFile]) -> Vec<&'static str> {
    units
        .iter()
        .filter_map(|u| match u.name {
            "urd-backup.timer" | "urd-sentinel.service" => Some(u.name),
            _ => None,
        })
        .collect()
}

/// Probe `is-enabled` for every enableable unit.
fn enabled_units(units: &[crate::systemd_units::UnitFile]) -> Vec<EnabledProbe> {
    enableable(units)
        .iter()
        .map(|name| match Command::new("systemctl")
            .env("LC_ALL", "C")
            .args(["--user", "is-enabled", name])
            .output()
        {
            Ok(out) => classify_is_enabled(
                &String::from_utf8_lossy(&out.stdout),
                &String::from_utf8_lossy(&out.stderr),
            ),
            Err(e) => EnabledProbe::NoManager(format!("could not run systemctl: {e}")),
        })
        .collect()
}

/// Read every expected unit's installed content (`None` = absent).
fn installed_unit_contents(
    units: &[crate::systemd_units::UnitFile],
    dir: &Path,
) -> std::collections::HashMap<String, Option<String>> {
    units
        .iter()
        .map(|u| (u.name.to_string(), std::fs::read_to_string(dir.join(u.name)).ok()))
        .collect()
}

/// Plain writes into the user's own directory: a torn write is named by the
/// doctor drift advisory and healed by `urd init` (ponytail — no temp+rename
/// ceremony here).
fn write_units(
    units: &[crate::systemd_units::UnitFile],
    dir: &Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for unit in units {
        std::fs::write(dir.join(unit.name), &unit.content)?;
    }
    Ok(())
}

/// Run one `systemctl --user` verb; non-success becomes the failure detail.
fn systemctl_user(args: &[&str]) -> Result<(), String> {
    match Command::new("systemctl")
        .env("LC_ALL", "C")
        .arg("--user")
        .args(args)
        .output()
    {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("could not run systemctl: {e}")),
    }
}

/// The linger truth (adversary F1): a user timer fires only while a
/// session exists. `Some(user)` iff loginctl clearly answers `Linger=no`;
/// `Linger=yes` and every unreadable answer are `None` — the advisory must
/// not scare, and doctor carries the standing check.
fn linger_loose() -> Option<String> {
    let user = invoking_username().ok()?;
    let out = Command::new("loginctl")
        .env("LC_ALL", "C")
        .args(["show-user", &user, "--property=Linger"])
        .output()
        .ok()?;
    (out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "Linger=no")
        .then_some(user)
}

/// Print the linger sentence at the units stage when the thread is loose.
fn print_linger_notice() {
    if let Some(user) = linger_loose() {
        print!("{}", voice::render_linger_notice(&user));
    }
}

/// Settle one reachable drive's identity: the `urd drives adopt` decision
/// (shared via `decide_adoption`) plus its filesystem actions. The token
/// write's `create_dir_all` is what creates the drive's snapshot home.
fn adopt_one(
    drive: &crate::config::DriveConfig,
    state: Option<&crate::state::StateDb>,
    now: &str,
) -> anyhow::Result<crate::output::AdoptAction> {
    use crate::drives::{self, AdoptDecision};
    use crate::output::AdoptAction;

    let on_disk = drives::read_drive_token(drive)?;
    let stored = match state {
        Some(db) => db.get_drive_token(&drive.label).unwrap_or_else(|e| {
            log::warn!("token lookup failed for {}: {e}", drive.label);
            None
        }),
        None => None,
    };
    let store = |token: &str| {
        if let Some(db) = state
            && let Err(e) = db.store_drive_token(&drive.label, token, now)
        {
            log::warn!("could not record {}'s token in the state DB: {e}", drive.label);
        }
    };
    match drives::decide_adoption(on_disk.as_deref(), stored.as_deref()) {
        AdoptDecision::AlreadyCurrent => Ok(AdoptAction::AlreadyCurrent),
        AdoptDecision::AdoptExisting => {
            let token =
                on_disk.ok_or_else(|| anyhow!("AdoptExisting implies an on-disk token"))?;
            store(&token);
            Ok(AdoptAction::AdoptedExisting { token })
        }
        AdoptDecision::GenerateNew => {
            let token = drives::generate_drive_token();
            drives::write_drive_token(drive, &token)?;
            store(&token);
            Ok(AdoptAction::GeneratedNew { token })
        }
    }
}

// ── Stage 1: the earning ────────────────────────────────────────────────

fn earn_privilege(config: &Config, config_path: &Path) -> anyhow::Result<SealOutcome> {
    let dest = Path::new(SUDOERS_DEST);

    // Already answers (e.g. a broader hand-managed grant)? Nothing to ask;
    // the doctor drift advisory watches coverage.
    if probe_grant(&config.general.btrfs_path).0 == GrantProbe::Granted {
        print!("{}", voice::render_earning_already());
        return Ok(SealOutcome::Sealed);
    }

    let user = invoking_username()?;

    // The grant names the configured btrfs binary; a grant for a missing
    // binary would verify never and confuse always.
    let btrfs = Path::new(&config.general.btrfs_path);
    if !btrfs.exists() {
        bail!(
            "btrfs not found at {} — fix `btrfs_path` in {} (try `which btrfs`), \
             then run `urd init` to resume the earning",
            btrfs.display(),
            config_path.display()
        );
    }

    let rendered = sudoers::render_sudoers(
        config,
        &RenderContext {
            user: &user,
            config_path,
            today: chrono::Local::now().date_naive(),
        },
    )
    .map_err(|refusal| anyhow!("{refusal}"))?;

    // Stage unprivileged, 0440 before the gate (visudo owner/mode variance).
    let tmp = stage_rendered(&rendered)?;

    // Unprivileged visudo gate — early, before anyone is asked anything.
    match Command::new("visudo").arg("-c").arg("-f").arg(tmp.path()).output() {
        Err(e) => {
            // No visudo at all: nothing installable by us — print the
            // manual path and leave the decision with the user.
            print!("{}", voice::render_earning_declined(&rendered, dest));
            println!("(visudo could not be run here: {e})");
            return Ok(SealOutcome::Declined);
        }
        Ok(out) if !out.status.success() => {
            // A render visudo refuses is a bug in urd, never installable.
            let kept = tmp.keep().context("keeping the refused file for inspection")?.1;
            print!(
                "{}",
                voice::render_visudo_refusal(&kept, &String::from_utf8_lossy(&out.stderr))
            );
            bail!("visudo refused the rendered sudoers file — nothing was activated");
        }
        Ok(_) => {}
    }

    // The asking: exact content, plain meaning, lettered consent.
    print!("{}", voice::render_earning_request(&rendered, dest));
    let choice = loop {
        let line = crate::commands::encounter::read_input_line()?;
        match line {
            // EOF is walking away — the least destructive reading.
            None => break EarningChoice::Decline,
            Some(line) => {
                if let Some(choice) = parse_earning_choice(&line) {
                    break choice;
                }
                print!("{}", voice::render_earning_request(&rendered, dest));
            }
        }
    };
    match choice {
        EarningChoice::Install => {}
        EarningChoice::Print => {
            print!("{}", voice::render_earning_declined(&rendered, dest));
            return Ok(SealOutcome::Declined);
        }
        EarningChoice::Decline => {
            print!("{}", voice::render_earning_deferred());
            return Ok(SealOutcome::Declined);
        }
    }

    // Consented: stage inertly as root, re-validate the root-owned bytes,
    // then activate atomically. Interactive sudo may prompt — that's the
    // ceremony, and both callers are TTY-gated.
    let install = Command::new("sudo")
        .args(["install", "-m", "0440", "-o", "root", "-g", "root"])
        .arg(tmp.path())
        .arg(SUDOERS_STAGING)
        .status()
        .context("failed to run sudo install")?;
    if !install.success() {
        print!(
            "{}",
            voice::render_earning_unavailable(&format!("`sudo install` exited {install}"))
        );
        print!("{}", voice::render_earning_declined(&rendered, dest));
        return Ok(SealOutcome::Declined);
    }

    // Root-side integrity check: read the now-root-owned staged bytes back
    // and confirm they are exactly what was rendered and shown. `install`
    // copied whatever the unprivileged temp held at that instant; a same-uid
    // writer could have swapped its content between the consent prompt and
    // that copy. Comparing the staged bytes to `rendered` (not merely
    // re-checking syntax) binds what activates to what the user approved.
    let staged = Command::new("sudo")
        .arg("cat")
        .arg(SUDOERS_STAGING)
        .output()
        .context("failed to read the staged sudoers file back")?;
    if !staged.status.success() || staged.stdout != rendered.as_bytes() {
        remove_staging();
        print!(
            "{}",
            voice::render_visudo_refusal(
                Path::new(SUDOERS_STAGING),
                "the staged file did not match the rendered grant",
            )
        );
        bail!(
            "the staged sudoers file did not match what was shown — nothing was \
             activated (the staged file was removed)"
        );
    }

    // Root-side re-validation: the staged bytes are now beyond a same-uid
    // writer's reach. visudo prints its own diagnosis to the terminal.
    let revalidate = Command::new("sudo")
        .args(["visudo", "-c", "-f", SUDOERS_STAGING])
        .status()
        .context("failed to run sudo visudo -c on the staged file")?;
    if !revalidate.success() {
        remove_staging();
        print!(
            "{}",
            voice::render_visudo_refusal(
                Path::new(SUDOERS_STAGING),
                "visudo reported the error above",
            )
        );
        bail!("root-side visudo refused the staged sudoers file — nothing was activated");
    }

    // Same-directory rename: atomic, never a partial file under includedir.
    let activate = Command::new("sudo")
        .args(["mv", SUDOERS_STAGING, SUDOERS_DEST])
        .status()
        .context("failed to run sudo mv")?;
    if !activate.success() {
        bail!(
            "could not activate the grant: `sudo mv` exited {activate}. \
             The staged file at {SUDOERS_STAGING} is inert (includedir skips \
             dot-names); `urd init` retries the earning"
        );
    }
    drop(tmp);

    // Verify. `sudo -k` first: the install cached a credential ticket, and
    // without dropping it the probe would pass regardless of file content.
    let _ = Command::new("sudo").arg("-k").status();
    let (probe, detail) = probe_grant(&config.general.btrfs_path);
    match probe {
        GrantProbe::Granted => {}
        GrantProbe::Denied | GrantProbe::Unclear => {
            print!("{}", voice::render_earning_verify_failed(&detail));
            return Ok(SealOutcome::InstalledUnverified);
        }
    }

    // Coverage cross-check (adversary F3): the probe proves *a* grant;
    // this proves *the* grant, at the one moment the user is watching.
    match check_coverage(config) {
        Ok(()) => {
            print!("{}", voice::render_earning_installed());
            Ok(SealOutcome::Sealed)
        }
        Err(reason) => {
            print!("{}", voice::render_earning_coverage_unconfirmed(&reason));
            Ok(SealOutcome::SealedUnverifiedCoverage)
        }
    }
}

/// The invoking user's passwd name — what the sudoers User_List matches.
/// Never `$USER` (wrong under `su`); no passwd entry is an honest failure,
/// never a guess.
pub(crate) fn invoking_username() -> anyhow::Result<String> {
    let uid = nix::unistd::Uid::current();
    let user = nix::unistd::User::from_uid(uid)
        .with_context(|| format!("passwd lookup failed for uid {uid}"))?
        .ok_or_else(|| anyhow!("no passwd entry for uid {uid} — cannot name the grantee"))?;
    Ok(user.name)
}

/// The user's answer at the asking. Enter installs; EOF declines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarningChoice {
    Install,
    Print,
    Decline,
}

/// Classify one consent line: `""`/`i` install, `p` print, `q` not now.
fn parse_earning_choice(line: &str) -> Option<EarningChoice> {
    match line.trim().to_lowercase().as_str() {
        "" | "i" => Some(EarningChoice::Install),
        "p" => Some(EarningChoice::Print),
        "q" => Some(EarningChoice::Decline),
        _ => None,
    }
}

/// Best-effort removal of the inert staged file after a rejected install.
/// The dot-name is includedir-ignored, so a leftover is harmless — but
/// tidying it keeps the next earning's staging path clear.
fn remove_staging() {
    let _ = Command::new("sudo").args(["rm", "-f", SUDOERS_STAGING]).status();
}

/// Write the rendered content to an unprivileged temp file, fsynced,
/// mode 0440 (visudo `-c -f` owner/mode variance across sudo versions).
fn stage_rendered(rendered: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".urd-sudoers-")
        .tempfile()
        .context("failed to create a staging file for the rendered grant")?;
    tmp.write_all(rendered.as_bytes())
        .context("failed to write the rendered grant")?;
    tmp.as_file().sync_all().context("failed to sync the rendered grant")?;
    let mut perms = tmp.as_file().metadata()?.permissions();
    perms.set_mode(0o440);
    tmp.as_file().set_permissions(perms)?;
    Ok(tmp)
}

/// The first incomplete seal stage a status surface should name (UPI 075
/// widened `probe_unsealed`; single owner of the rule). Interactive only —
/// a denied probe writes an auth log line, so automated and monitoring
/// callers must never generate that noise — and only on clear evidence:
/// `Unclear` probes and unreadable directories stay silent rather than
/// guess. Seal order decides: privilege → units → first thread (one
/// sentence, one cause — adversary F4/F7). The units arm checks file
/// EXISTENCE only (content drift is doctor's job; no systemctl call here),
/// and the first-thread arm is policy-aware (F5).
pub(crate) fn seal_completeness(
    config: &Config,
    output_mode: OutputMode,
) -> Option<crate::output::SealGap> {
    if output_mode != OutputMode::Interactive {
        return None;
    }
    seal_gap_given_probe(config, probe_grant(&config.general.btrfs_path).0)
}

/// The gap decision with the probe injected — `urd init` already holds a
/// probe result and must not pay (or log) a second one.
pub(crate) fn seal_gap_given_probe(
    config: &Config,
    probe: GrantProbe,
) -> Option<crate::output::SealGap> {
    use crate::output::SealGap;

    if probe == GrantProbe::Denied {
        return Some(SealGap::Privilege);
    }
    if units_missing(config) {
        return Some(SealGap::Units);
    }
    if first_thread_pending(config) {
        return Some(SealGap::FirstThread);
    }
    None
}

/// Is any expected unit file absent? Existence only, by expected name —
/// no exe resolution and no content compare on the status path; an
/// unresolvable config dir stays silent (never a guess).
fn units_missing(config: &Config) -> bool {
    let Some(dir) = dirs::config_dir().map(|d| d.join("systemd/user")) else {
        return false;
    };
    crate::systemd_units::expected_unit_names(&config.general.run_frequency)
        .iter()
        .any(|name| !dir.join(name).exists())
}

/// The passwordless probe (`LC_ALL=C sudo -n <btrfs> filesystem show /`),
/// classified. Returns the classification plus a human detail string for
/// honest failure sentences. Shared by the seal, `urd init`'s resume, the
/// status banner, and doctor. Never prompts (`-n`); never run from a
/// daemon path (plan invariant — a denied probe writes an auth log line).
pub(crate) fn probe_grant(btrfs_path: &str) -> (GrantProbe, String) {
    match Command::new("sudo")
        .env("LC_ALL", "C")
        .arg("-n")
        .arg(btrfs_path)
        .args(["filesystem", "show", "/"])
        .output()
    {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let probe = sudoers::classify_probe(out.status.success(), &stderr);
            let detail = if out.status.success() {
                String::new()
            } else {
                format!(
                    "exit {}: {}",
                    out.status.code().unwrap_or(-1),
                    stderr.trim()
                )
            };
            (probe, detail)
        }
        Err(e) => (GrantProbe::Unclear, format!("could not run sudo: {e}")),
    }
}

/// Diff effective privileges (`LC_ALL=C sudo -n -l`) against the config's
/// expected grants. `Ok(())` = fully covered; `Err(reason)` = the honest
/// cannot-confirm sentence's substance (never a silent pass).
pub(crate) fn check_coverage(config: &Config) -> Result<(), String> {
    let expected = sudoers::expected_grant_lines(config).map_err(|r| r.to_string())?;
    let out = Command::new("sudo")
        .env("LC_ALL", "C")
        .args(["-n", "-l"])
        .output()
        .map_err(|e| format!("could not run sudo -n -l: {e}"))?;
    if !out.status.success() {
        return Err("the privilege listing needs a password (sudo -n -l)".to_string());
    }
    let listing = sudoers::parse_privilege_listing(&String::from_utf8_lossy(&out.stdout))
        .map_err(|u| u.reason)?;
    match sudoers::coverage(&expected, &listing) {
        Coverage::AllCovered => Ok(()),
        Coverage::CannotInterpret { reason } => Err(reason),
        Coverage::Gaps { missing, uncertain } => {
            let mut gaps: Vec<String> = missing;
            gaps.extend(uncertain);
            Err(format!(
                "the privilege listing does not echo these grants verbatim: {}",
                gaps.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_earning_choice_defaults_to_install_and_knows_the_letters() {
        assert_eq!(parse_earning_choice(""), Some(EarningChoice::Install));
        assert_eq!(parse_earning_choice("i"), Some(EarningChoice::Install));
        assert_eq!(parse_earning_choice("I"), Some(EarningChoice::Install));
        assert_eq!(parse_earning_choice("p"), Some(EarningChoice::Print));
        assert_eq!(parse_earning_choice("q"), Some(EarningChoice::Decline));
        assert_eq!(parse_earning_choice("x"), None);
        assert_eq!(parse_earning_choice("install"), None);
    }

    #[test]
    fn stage_rendered_writes_content_at_mode_0440() {
        let tmp = stage_rendered("# rendered grant\n").unwrap();
        let on_disk = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(on_disk, "# rendered grant\n");
        let mode = tmp.as_file().metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o440, "visudo -c gate wants a sober mode");
    }

    #[test]
    fn kept_staging_file_survives_drop() {
        // The grilled decision: a visudo refusal names the temp path — so
        // the path must outlive the handle (adversary F1/G5).
        let tmp = stage_rendered("refused content\n").unwrap();
        let kept = tmp.keep().unwrap().1;
        assert!(kept.exists());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "refused content\n");
        std::fs::remove_file(&kept).unwrap();
    }

    #[test]
    fn parse_units_choice_defaults_to_install_and_knows_the_letters() {
        assert_eq!(parse_units_choice(""), Some(UnitsChoice::Install));
        assert_eq!(parse_units_choice("i"), Some(UnitsChoice::Install));
        assert_eq!(parse_units_choice("Q"), Some(UnitsChoice::Skip));
        assert_eq!(parse_units_choice("p"), None, "the print option was cut");
        assert_eq!(parse_units_choice("x"), None);
    }

    #[test]
    fn classify_is_enabled_separates_disabled_from_no_manager() {
        assert_eq!(classify_is_enabled("enabled\n", ""), EnabledProbe::Enabled);
        assert_eq!(classify_is_enabled("disabled\n", ""), EnabledProbe::NotEnabled);
        assert_eq!(classify_is_enabled("static\n", ""), EnabledProbe::NotEnabled);
        assert_eq!(
            classify_is_enabled("", "Failed to connect to bus: No medium found"),
            EnabledProbe::NoManager("Failed to connect to bus: No medium found".to_string())
        );
    }

    /// Unit files land byte-true and the done-detection reads them back
    /// (TempDir — mocks are blind to filesystem preconditions).
    #[test]
    fn write_units_then_installed_contents_round_trips() {
        let units = crate::systemd_units::expected_units(
            &crate::types::RunFrequency::Sentinel,
            Path::new("/home/alice/.cargo/bin/urd"),
        )
        .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("systemd/user");

        write_units(&units, &dir).unwrap();
        let installed = installed_unit_contents(&units, &dir);
        assert!(crate::systemd_units::diff_units(&units, &installed).is_empty());

        // Tamper with one file: the diff names exactly it.
        std::fs::write(dir.join("urd-backup.timer"), "[Timer]\n").unwrap();
        let installed = installed_unit_contents(&units, &dir);
        let drift = crate::systemd_units::diff_units(&units, &installed);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].name, "urd-backup.timer");
    }

    #[test]
    fn enableable_targets_the_timer_and_sentinel_never_the_service() {
        let units = crate::systemd_units::expected_units(
            &crate::types::RunFrequency::Sentinel,
            Path::new("/home/alice/.cargo/bin/urd"),
        )
        .unwrap();
        assert_eq!(enableable(&units), vec!["urd-backup.timer", "urd-sentinel.service"]);
    }

    #[test]
    fn parse_send_choice_enter_sends_now_and_t_defers() {
        assert_eq!(parse_send_choice(""), Some(SendChoice::SendNow));
        assert_eq!(parse_send_choice("s"), Some(SendChoice::SendNow));
        assert_eq!(parse_send_choice("t"), Some(SendChoice::Tonight));
        assert_eq!(parse_send_choice("q"), Some(SendChoice::Tonight));
        assert_eq!(parse_send_choice("x"), None);
    }

    fn seal_config(toml: &str) -> crate::config::Config {
        crate::config::Config::from_str(toml).unwrap()
    }

    #[test]
    fn wants_send_is_false_for_recorded_only_configs() {
        let recorded = seal_config(
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
        );
        assert!(!wants_send(&recorded.resolved_subvolumes()));
    }

    /// First-thread pending is policy-aware (adversary F5): satisfied once
    /// every enabled subvolume that plans local snapshots has one on disk.
    #[test]
    fn first_thread_pending_follows_the_filesystem_of_truth() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join(".snapshots");
        let config = seal_config(&format!(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "{}"
protection = "recorded"
"#,
            root.display()
        ));
        // No snapshot dir at all → pending.
        assert!(first_thread_pending(&config));
        // A real snapshot dir entry satisfies it.
        std::fs::create_dir_all(root.join("docs/20260705-0400-docs")).unwrap();
        assert!(!first_thread_pending(&config));
    }

    /// Send-offer done-detection over a real (temp) drive layout.
    #[test]
    fn already_sent_sees_external_snapshots_on_the_drive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = seal_config(&format!(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "sheltered"

[[drives]]
label = "backup-1"
mount_path = "{}"
snapshot_root = ".snapshots"
uuid = "abcd-1234"
role = "primary"
"#,
            tmp.path().display()
        ));
        let drives: Vec<&crate::config::DriveConfig> = config.drives.iter().collect();
        let resolved = config.resolved_subvolumes();
        assert!(wants_send(&resolved), "sheltered promises send");
        assert!(!already_sent(&drives, &resolved));
        std::fs::create_dir_all(tmp.path().join(".snapshots/docs/20260705-0400-docs"))
            .unwrap();
        assert!(already_sent(&drives, &resolved));
    }

    // ── The second look's pure classification (adversary F2) ───────────

    #[test]
    fn parse_findmnt_target_fsroot_reads_both_fields() {
        assert_eq!(
            parse_findmnt_target_fsroot("TARGET=\"/home\" FSROOT=\"/home\"\n"),
            Some((PathBuf::from("/home"), PathBuf::from("/home")))
        );
        assert_eq!(
            parse_findmnt_target_fsroot("TARGET=\"/\" FSROOT=\"/root\"\n"),
            Some((PathBuf::from("/"), PathBuf::from("/root")))
        );
        assert_eq!(parse_findmnt_target_fsroot("garbage"), None);
    }

    #[test]
    fn fs_relative_maps_mount_space_into_subvol_space() {
        // Fedora layout: /home is subvol `home`; /data/docs under a
        // whole-pool mount at /data stays `docs`... relative to FSROOT.
        assert_eq!(
            fs_relative(Path::new("/home/alice"), Path::new("/home"), Path::new("/home")),
            Some(PathBuf::from("home/alice"))
        );
        assert_eq!(
            fs_relative(Path::new("/data/docs"), Path::new("/data"), Path::new("/")),
            Some(PathBuf::from("docs"))
        );
        assert_eq!(
            fs_relative(Path::new("/elsewhere"), Path::new("/data"), Path::new("/")),
            None
        );
    }

    /// Fixture shaped like the live host's `btrfs subvolume list` output
    /// (Fedora `root`/`home` layout): the classifier must not count the
    /// covered source, urd's own snapshot home, snapper dirs, or docker
    /// layers — and must count the nested and foreign subvolumes.
    #[test]
    fn count_uncovered_classifies_in_subvol_path_space() {
        let listing = vec![
            PathBuf::from("home"),                                  // covered source
            PathBuf::from("root"),                                  // foreign top-level → counts
            PathBuf::from("home/alice/nested-vm-images"),           // nested under source → counts
            PathBuf::from("home/alice/.snapshots/docs/20260705-0400-docs"), // urd home
            PathBuf::from(".snapshots/1/snapshot"),                 // snapper convention
            PathBuf::from("var/lib/docker/btrfs/subvolumes/abc123"), // docker layer
        ];
        let view = PoolView {
            fsroot: PathBuf::from("/"),
            covered: vec![PathBuf::from("home")],
            snapshot_homes: vec![PathBuf::from("home/alice/.snapshots")],
        };
        assert_eq!(count_uncovered(&[(listing, view)]), 2);
    }

    #[test]
    fn count_uncovered_empty_listing_counts_nothing() {
        let view = PoolView {
            fsroot: PathBuf::from("/"),
            covered: vec![],
            snapshot_homes: vec![],
        };
        assert_eq!(count_uncovered(&[(Vec::new(), view)]), 0);
    }

    /// The adoption action over a real (temp) filesystem: the token write
    /// creates the snapshot home, and a re-run without a DB record adopts
    /// the existing identity instead of minting a new one (mocks are blind
    /// to filesystem preconditions — CLAUDE.md testing rule).
    #[test]
    fn adopt_one_creates_snapshot_home_and_keeps_identity_on_rerun() {
        use crate::output::AdoptAction;

        let tmp = tempfile::TempDir::new().unwrap();
        let drive = crate::config::DriveConfig {
            label: "backup-1".to_string(),
            uuid: None,
            mount_path: tmp.path().to_path_buf(),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Primary,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        };

        let first = adopt_one(&drive, None, "2026-07-05T12:00:00").unwrap();
        let AdoptAction::GeneratedNew { token } = first else {
            panic!("fresh drive should mint a token, got {first:?}");
        };
        let root = tmp.path().join(".snapshots");
        assert!(root.is_dir(), "the token write creates the snapshot home");
        assert!(root.join(".urd-drive-token").is_file());

        // Re-run (interrupted-seal resume): the on-disk identity survives.
        let second = adopt_one(&drive, None, "2026-07-05T12:01:00").unwrap();
        assert_eq!(second, AdoptAction::AdoptedExisting { token });
    }

    /// With the state DB recording the token, the re-run is a no-op.
    #[test]
    fn adopt_one_is_already_current_once_the_db_agrees() {
        use crate::output::AdoptAction;

        let tmp = tempfile::TempDir::new().unwrap();
        let drive = crate::config::DriveConfig {
            label: "backup-1".to_string(),
            uuid: None,
            mount_path: tmp.path().to_path_buf(),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Primary,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        };
        let db = crate::state::StateDb::open(&tmp.path().join("urd.db")).unwrap();

        let first = adopt_one(&drive, Some(&db), "2026-07-05T12:00:00").unwrap();
        assert!(matches!(first, AdoptAction::GeneratedNew { .. }));
        let second = adopt_one(&drive, Some(&db), "2026-07-05T12:01:00").unwrap();
        assert_eq!(second, AdoptAction::AlreadyCurrent);
    }

    /// Real `visudo -c -f` over a real render: the gate accepts what urd
    /// renders and refuses garbage. `#[ignore]`: shells out to visudo.
    #[test]
    #[ignore]
    fn visudo_accepts_the_render_and_refuses_garbage() {
        let config = crate::config::Config::from_str(
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
        .unwrap();
        let rendered = sudoers::render_sudoers(
            &config,
            &RenderContext {
                user: "alice",
                config_path: Path::new("/home/alice/.config/urd/urd.toml"),
                today: chrono::NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(),
            },
        )
        .unwrap();

        let good = stage_rendered(&rendered).unwrap();
        let ok = Command::new("visudo")
            .arg("-c")
            .arg("-f")
            .arg(good.path())
            .output()
            .expect("visudo must exist for this ignored test");
        assert!(ok.status.success(), "visudo refused urd's render:\n{rendered}");

        let bad = stage_rendered("alice ALL=(root NOPASSWD /broken\n").unwrap();
        let refused = Command::new("visudo")
            .arg("-c")
            .arg("-f")
            .arg(bad.path())
            .output()
            .unwrap();
        assert!(!refused.status.success(), "visudo must refuse garbage");
    }
}
