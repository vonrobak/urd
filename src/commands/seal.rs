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
    let mut pools: std::collections::BTreeMap<String, PoolView> =
        std::collections::BTreeMap::new();
    for sv in config.resolved_subvolumes().iter().filter(|sv| sv.enabled) {
        let locus = pool_locus(&sv.source)?;
        let snapshot_dir = config.local_snapshot_dir(&sv.name);
        fold_source(&mut pools, &locus, &sv.source, snapshot_dir.as_deref())?;
    }

    let mut listings = Vec::new();
    for view in pools.into_values() {
        let listing = btrfs.list_subvolumes(&view.mount).ok()?;
        listings.push((listing, view));
    }
    Some(count_uncovered(&listings))
}

/// Fold one promised source into its pool's view. Pools are keyed by
/// filesystem UUID, not mount point (live-found 2026-07-05): Fedora's
/// default layout mounts one filesystem at both `/` (subvol `root`) and
/// `/home` (subvol `home`), and `subvolume list` returns the WHOLE
/// filesystem from either mount — keying by mount listed the pool twice
/// with split coverage, so each promised sibling counted as uncovered and
/// every genuinely uncovered subvolume counted once per mount.
fn fold_source(
    pools: &mut std::collections::BTreeMap<String, PoolView>,
    locus: &PoolLocus,
    source: &Path,
    snapshot_dir: Option<&Path>,
) -> Option<()> {
    let view = pools
        .entry(locus.uuid.clone())
        .or_insert_with(|| PoolView {
            mount: locus.mount.clone(),
            covered: Vec::new(),
            snapshot_homes: Vec::new(),
        });
    view.covered
        .push(fs_relative(source, &locus.mount, &locus.fsroot)?);
    if let Some(dir) = snapshot_dir
        && dir.starts_with(&locus.mount)
    {
        view.snapshot_homes
            .push(fs_relative(dir, &locus.mount, &locus.fsroot)?);
    }
    Some(())
}

/// Where a promised source lives: the mount holding it, that mount's
/// FSROOT, and the filesystem's UUID (the pool identity).
struct PoolLocus {
    mount: PathBuf,
    fsroot: PathBuf,
    uuid: String,
}

/// One promised pool as the classifier sees it: everything in the pool's
/// own subvol-path coordinates, plus one mount to list it through.
struct PoolView {
    mount: PathBuf,
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

/// One findmnt call: the mountpoint holding `path`, that mount's FSROOT,
/// and the filesystem UUID.
fn pool_locus(path: &Path) -> Option<PoolLocus> {
    let out = Command::new("findmnt")
        .env("LC_ALL", "C")
        .args(["-n", "-P", "-o", "TARGET,FSROOT,UUID", "--target"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_findmnt_locus(&String::from_utf8_lossy(&out.stdout))
}

/// Pure parse of `findmnt -P -o TARGET,FSROOT,UUID`:
/// `TARGET="/" FSROOT="/root" UUID="abcd-..."`. An empty UUID is a parse
/// failure — without a pool identity the second look stays silent rather
/// than guessing.
fn parse_findmnt_locus(stdout: &str) -> Option<PoolLocus> {
    let extract = |key: &str| -> Option<String> {
        let needle = format!("{key}=\"");
        let start = stdout.find(&needle)? + needle.len();
        let rest = &stdout[start..];
        Some(rest[..rest.find('"')?].to_string())
    };
    let uuid = extract("UUID")?;
    if uuid.is_empty() {
        return None;
    }
    Some(PoolLocus {
        mount: PathBuf::from(extract("TARGET")?),
        fsroot: PathBuf::from(extract("FSROOT")?),
        uuid,
    })
}

/// Whether a configured snapshot root needs the earning's privileged
/// `install -d` (#282): missing entirely, or present but not writable by
/// the current user (the pool-canonical deep-pool case — typically
/// root-owned). Probes by attempting the exact write stage 4 will need
/// (`create_dir_all` + a throwaway file), never by inspecting mode bits —
/// the kernel's own permission check is the only reliable oracle across
/// ACLs/group membership/mount options. No side effect on a definite
/// "needs privilege" verdict: a failed `create_dir_all` creates nothing.
fn root_needs_privileged_creation(root: &Path) -> bool {
    if std::fs::create_dir_all(root).is_err() {
        return true;
    }
    let probe = root.join(".urd-write-test");
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .is_err()
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
    if let Err(e) = with_logs_suppressed(|| run_seal_backup(config_path, /* local_only */ true)) {
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
        SendChoice::SendNow => match with_logs_suppressed(|| {
            run_seal_backup(config_path, /* local_only */ false)
        }) {
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

/// Restores the prior `log` max level on drop — the error/panic path must
/// un-suppress too, or a failure inside `f` would leave every later log call
/// silent for the rest of the process (UPI 081 A3).
struct LogLevelGuard(log::LevelFilter);
impl Drop for LogLevelGuard {
    fn drop(&mut self) {
        log::set_max_level(self.0);
    }
}

/// Suppresses `log!` echoes for the duration of `f` (#277): the ceremony's
/// backup calls would otherwise leak raw `ERROR urd::executor` lines above
/// the mythic-voice summary. Honors `--verbose` (only suppresses below
/// `Debug`). Safe to suppress broadly — every `error!` that names a real
/// failure is a companion to a returned `OperationOutcome::Failure`/`Err`
/// that the (unsuppressed) `print!` summary already renders; the bracket
/// loses log echoes, never the failure itself (adversary M1, design A3).
fn with_logs_suppressed<T>(f: impl FnOnce() -> T) -> T {
    let prev = log::max_level();
    let _guard = LogLevelGuard(prev);
    if prev < log::LevelFilter::Debug {
        log::set_max_level(log::LevelFilter::Off);
    }
    f()
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
    let names: Vec<&str> = units.iter().map(|u| u.name).collect();
    let installed = installed_unit_contents(&names, &dir);
    if crate::systemd_units::diff_units(&units, &installed).is_empty()
        && enabled.iter().all(|p| *p == EnabledProbe::Enabled)
    {
        print!("{}", voice::render_units_already(&next_action));
        print_linger_notice();
        return true;
    }

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

/// Read every named unit's installed content (`None` = absent). Shared by
/// `units_drifted` (the deep gate) and doctor's units-drift rows (UPI 085)
/// so both read the installed map through one loop instead of each
/// hand-rolling the same `read_to_string` scan.
pub(crate) fn installed_unit_contents(
    names: &[&str],
    dir: &Path,
) -> std::collections::HashMap<String, Option<String>> {
    names
        .iter()
        .map(|name| (name.to_string(), std::fs::read_to_string(dir.join(name)).ok()))
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

    // Already answers (e.g. a broader hand-managed grant)? Ask again only
    // on clear evidence: expected lines the listing definitively lacks (a
    // config the installed grant predates). Wildcard uncertainty stays
    // doctor's — a hand-managed broad grant is never nagged (071).
    let regrant = if probe_grant(&config.general.btrfs_path).0 == GrantProbe::Granted {
        let missing = coverage_missing(config);
        if missing.is_empty() {
            print!("{}", voice::render_earning_already());
            return Ok(SealOutcome::Sealed);
        }
        print!("{}", voice::render_earning_regrant(&missing));
        true
    } else {
        false
    };
    // A declined RE-render leaves a grant that still answers: later stages
    // can proceed, so the seal continues with coverage unconfirmed instead
    // of stopping as a fresh earning's decline does (F4 table).
    let declined = declined_outcome(regrant);

    let user = invoking_username()?;

    // The grant names the configured btrfs binary; a grant for a missing
    // binary would verify never and confuse always.
    let btrfs = Path::new(&config.general.btrfs_path);
    if !btrfs.exists() {
        // Declined here = Urd-blocked, not user-declined; no consumer
        // distinguishes — see design A1 (UPI 081 M4).
        print!(
            "{}",
            voice::render_earning_blocked(&format!(
                "btrfs not found at {} — fix `btrfs_path` in {} (try `which btrfs`)",
                btrfs.display(),
                config_path.display()
            ))
        );
        return Ok(declined);
    }

    let rendered = match sudoers::render_sudoers(
        config,
        &RenderContext {
            user: &user,
            config_path,
            today: chrono::Local::now().date_naive(),
        },
    ) {
        Ok(rendered) => rendered,
        Err(refusal) => {
            // Declined here = Urd-blocked, not user-declined; no consumer
            // distinguishes — see design A1 (UPI 081 M4).
            print!("{}", voice::render_earning_blocked(&refusal.to_string()));
            return Ok(declined);
        }
    };

    // Stage unprivileged, 0440 before the gate (visudo owner/mode variance).
    let tmp = stage_rendered(&rendered)?;

    // Unprivileged visudo gate — early, before anyone is asked anything.
    match Command::new("visudo").arg("-c").arg("-f").arg(tmp.path()).output() {
        Err(e) => {
            // No visudo at all: nothing installable by us — print the
            // manual path and leave the decision with the user.
            print!("{}", voice::render_earning_declined(&rendered, dest));
            println!("(visudo could not be run here: {e})");
            return Ok(declined);
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
            return Ok(declined);
        }
        EarningChoice::Decline => {
            print!("{}", voice::render_earning_deferred());
            return Ok(declined);
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
        return Ok(declined);
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

    // Deep-pool snapshot roots (#282, F7 residual): a home-relative root is
    // already user-writable by construction (073), but a pool-canonical
    // root (e.g. `/mnt/btrfs-pool/.snapshots`) is typically root-owned, and
    // stage 4 creates snapshot dirs unprivileged. This is the earning's one
    // interactive-root moment — use it, in the same authenticated window,
    // rather than stranding stage 4 on a permission error.
    for root in &config.local_snapshots.roots {
        if root_needs_privileged_creation(&root.path) {
            let install_root = Command::new("sudo")
                .args(["install", "-d", "-o", &user])
                .arg(&root.path)
                .status();
            if !install_root.is_ok_and(|s| s.success()) {
                log::warn!(
                    "could not prepare snapshot root {} as {user} during the earning \
                     — stage 4 will report the permission failure if it persists",
                    root.path.display()
                );
            }
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

/// What a not-installed conclusion means for the seal (pure). A fresh
/// earning's decline leaves no grant — the seal stops (F4). A re-render's
/// decline leaves a grant that still answers — the seal continues with
/// coverage unconfirmed.
fn declined_outcome(regrant: bool) -> SealOutcome {
    if regrant {
        SealOutcome::SealedUnverifiedCoverage
    } else {
        SealOutcome::Declined
    }
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

/// The full picture a status surface needs (UPI 081): the first incomplete
/// seal stage alongside whether the machine is earned (grant answers) and
/// whether the probe itself couldn't confirm (`Unclear`, e.g. sudo erroring).
/// A single probe backs all three fields — probing twice would double the
/// auth-log line (memory `feedback_never_time_limit_sends`/071).
pub(crate) struct SealPosture {
    pub gap: Option<crate::output::SealGap>,
    pub earned: bool,
    pub privilege_unclear: bool,
}

/// Interactive only — a denied probe writes an auth log line, so automated
/// and monitoring callers must never generate that noise; non-interactive
/// callers get no probe (`earned` defaults true — machine consumers keep
/// today's advice, the suppression is a human-UX concern).
pub(crate) fn seal_posture(config: &Config, output_mode: OutputMode) -> SealPosture {
    let probe =
        (output_mode == OutputMode::Interactive).then(|| probe_grant(&config.general.btrfs_path).0);
    posture_from_probe(config, probe)
}

/// The pure mapping from an already-run probe (`None` = non-interactive, no
/// probe) to a `SealPosture`. Seal order decides the gap: privilege → units
/// → first thread (one sentence, one cause — adversary F4/F7). The units arm
/// checks file EXISTENCE only (content drift is doctor's job; no systemctl
/// call here), and the first-thread arm is policy-aware (F5). `Unclear`
/// never falls through to `seal_gap_given_probe`: we don't presume
/// Units/FirstThread stages when privilege itself is unconfirmed.
pub(crate) fn posture_from_probe(config: &Config, probe: Option<GrantProbe>) -> SealPosture {
    let Some(probe) = probe else {
        return SealPosture { gap: None, earned: true, privilege_unclear: false };
    };
    SealPosture {
        gap: match probe {
            GrantProbe::Unclear => None,
            p => seal_gap_given_probe(config, p),
        },
        earned: probe == GrantProbe::Granted,
        privilege_unclear: probe == GrantProbe::Unclear,
    }
}

/// The cheap gap decision with the probe injected — status surfaces only.
/// `urd init` gates on `seal_gap_deep` instead: the make-whole verb also
/// sees coverage gaps and units content drift.
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

/// The make-whole verb's deeper gate (`urd init` only): the content-level
/// checks the cheap status probe deliberately avoids (adversary F7 scoped
/// the existence-only rule to status surfaces, not to the explicit resume
/// verb). Privilege also gaps on a grant that answers but definitively
/// lacks expected lines (a config the installed file predates); units also
/// gap on content drift (e.g. an ExecStart naming another binary). Both
/// arms stay silent on uncertainty — clear evidence only, as everywhere —
/// and both stages' done-checks make re-entry idempotent and consent-gated.
pub(crate) fn seal_gap_deep(
    config: &Config,
    probe: GrantProbe,
) -> Option<crate::output::SealGap> {
    use crate::output::SealGap;

    if probe == GrantProbe::Denied {
        return Some(SealGap::Privilege);
    }
    if probe == GrantProbe::Granted && !coverage_missing(config).is_empty() {
        return Some(SealGap::Privilege);
    }
    if units_missing(config) || units_drifted(config) {
        return Some(SealGap::Units);
    }
    if first_thread_pending(config) {
        return Some(SealGap::FirstThread);
    }
    None
}

/// Has any expected unit's installed content drifted from what THIS binary
/// would render? Deep-gate only; any resolution failure is silence, never
/// a guess — doctor owns the honest cannot-verify sentences.
fn units_drifted(config: &Config) -> bool {
    let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return false;
    };
    let Ok(units) = crate::systemd_units::expected_units(&config.general.run_frequency, &exe)
    else {
        return false;
    };
    let Some(dir) = dirs::config_dir().map(|d| d.join("systemd/user")) else {
        return false;
    };
    let names: Vec<&str> = units.iter().map(|u| u.name).collect();
    !crate::systemd_units::diff_units(&units, &installed_unit_contents(&names, &dir)).is_empty()
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
    match effective_coverage(config)? {
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

/// Effective privileges (`LC_ALL=C sudo -n -l`) diffed against the
/// config's expected grants. `Err` = the listing could not be rendered,
/// obtained, or parsed.
fn effective_coverage(config: &Config) -> Result<Coverage, String> {
    let expected = sudoers::expected_grant_lines(config).map_err(|r| r.to_string())?;
    let out = Command::new("sudo")
        .env("LC_ALL", "C")
        .args(["-n", "-l"])
        .output()
        .map_err(|e| format!("could not run sudo -n -l: {e}"))?;
    if !out.status.success() {
        return Err("the privilege listing needs a password (sudo -n -l)".to_string());
    }
    coverage_from_listing(&expected, &String::from_utf8_lossy(&out.stdout))
}

/// Parse a raw `sudo -n -l` listing and diff it against `expected`. Pure —
/// the I/O boundary stays with each caller (`effective_coverage`'s own
/// probe; doctor's injected listing for its test seam) — so the deep gate
/// and doctor's drift rows share ONE parse-and-compare instead of each
/// independently calling `parse_privilege_listing`/`coverage` (UPI 085).
pub(crate) fn coverage_from_listing(
    expected: &[String],
    raw_listing: &str,
) -> Result<Coverage, String> {
    let listing = sudoers::parse_privilege_listing(raw_listing).map_err(|u| u.reason)?;
    Ok(sudoers::coverage(expected, &listing))
}

/// The expected grant lines the effective listing DEFINITIVELY lacks.
/// Empty on full coverage — and on every uncertainty (unlistable,
/// unparseable, wildcard candidates urd does not interpret): the deep
/// gate and the earning's done-check act only on clear evidence; doctor
/// owns the honest uncertain sentences.
fn coverage_missing(config: &Config) -> Vec<String> {
    match effective_coverage(config) {
        Ok(Coverage::Gaps { missing, .. }) => missing,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UPI 081 A3 (#277): sets `Off` for the closure's duration and always
    /// restores the prior level after — on the success path, and on the
    /// panic path via the `Drop` guard (M1: the broad suppression is safe
    /// only because it always un-suppresses). One test, not three: global
    /// `log::max_level()` state would race across parallel test threads.
    #[test]
    fn with_logs_suppressed_restores_level_on_success_and_panic() {
        let before = log::max_level();

        let seen_during = with_logs_suppressed(log::max_level);
        assert_eq!(seen_during, log::LevelFilter::Off);
        assert_eq!(log::max_level(), before, "must restore after a normal return");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_logs_suppressed(|| panic!("boom"));
        }));
        assert!(result.is_err());
        assert_eq!(log::max_level(), before, "the Drop guard must restore even after a panic");

        log::set_max_level(log::LevelFilter::Debug);
        let seen_at_debug = with_logs_suppressed(log::max_level);
        assert_eq!(
            seen_at_debug,
            log::LevelFilter::Debug,
            "--verbose (Debug+) must not be suppressed"
        );
        log::set_max_level(before);
    }

    #[test]
    fn declined_outcome_stops_a_fresh_earning_and_continues_a_regrant() {
        // F4 continuation table: no grant → the seal stops; a grant that
        // still answers (declined re-render) → the seal continues with
        // coverage unconfirmed, so the units heal is never stranded.
        assert_eq!(declined_outcome(false), SealOutcome::Declined);
        assert_eq!(declined_outcome(true), SealOutcome::SealedUnverifiedCoverage);
    }

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
    fn root_needs_privileged_creation_false_for_writable_existing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".snapshots");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!root_needs_privileged_creation(&root));
    }

    #[test]
    fn root_needs_privileged_creation_false_for_missing_but_creatable_root() {
        // The common case: the root's parent is user-writable (home-relative
        // fallback, 073), so create_dir_all succeeds unprivileged and no
        // earning-time install -d is needed.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("nested").join(".snapshots");
        assert!(!root.exists());
        assert!(!root_needs_privileged_creation(&root));
        assert!(root.exists(), "the probe itself creates the root when it can");
    }

    #[test]
    fn root_needs_privileged_creation_true_for_unwritable_existing_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".snapshots");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let needs_privilege = root_needs_privileged_creation(&root);
        // Restore write perms so tempdir cleanup can remove it regardless of outcome.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(needs_privilege, "a read-only existing root must ask for privileged creation");
    }

    #[test]
    fn root_needs_privileged_creation_true_when_parent_is_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("locked-parent");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let root = parent.join(".snapshots");
        let needs_privilege = root_needs_privileged_creation(&root);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(needs_privilege, "an unwritable parent must ask for privileged creation");
        assert!(!root.exists(), "create_dir_all must not have partially succeeded");
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

        let names: Vec<&str> = units.iter().map(|u| u.name).collect();
        write_units(&units, &dir).unwrap();
        let installed = installed_unit_contents(&names, &dir);
        assert!(crate::systemd_units::diff_units(&units, &installed).is_empty());

        // Tamper with one file: the diff names exactly it.
        std::fs::write(dir.join("urd-backup.timer"), "[Timer]\n").unwrap();
        let installed = installed_unit_contents(&names, &dir);
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

    /// UPI 081 A2 (#275): btrfs-not-found routes through `render_earning_blocked`
    /// and returns `Ok(Declined)`, never `Err` — a blocked first-timer is an
    /// honest conclusion, not a crash.
    #[test]
    fn earn_privilege_declined_not_err_when_btrfs_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join(".snapshots");
        let config = seal_config(&format!(
            r#"
[general]
config_version = 2
run_frequency = "daily"
btrfs_path = "/nonexistent/btrfs-for-test"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "{}"
protection = "recorded"
"#,
            root.display()
        ));
        let outcome = earn_privilege(&config, &tmp.path().join("urd.toml")).unwrap();
        assert_eq!(outcome, SealOutcome::Declined);
    }

    /// UPI 081 A2 (#275): a sudoers render refusal (here: a scope too
    /// shallow for the floor) routes through `render_earning_blocked` and
    /// returns `Ok(Declined)`, never `Err`. `btrfs_path` points at a real,
    /// existing binary that carries no grant of its own (`/usr/bin/true`),
    /// so the probe reads Denied rather than Granted regardless of the test
    /// runner's own cached sudo ticket — the "already earns" shortcut must
    /// not mask the render refusal this test targets.
    #[test]
    fn earn_privilege_declined_not_err_when_render_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = seal_config(
            r#"
[general]
config_version = 2
run_frequency = "daily"
btrfs_path = "/usr/bin/true"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/"
protection = "recorded"
"#,
        );
        let outcome = earn_privilege(&config, &tmp.path().join("urd.toml")).unwrap();
        assert_eq!(outcome, SealOutcome::Declined);
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

    #[test]
    fn posture_from_probe_maps_each_probe_class() {
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
        // No snapshot on disk yet: first_thread_pending is true, so this
        // config is incomplete regardless of the ambient units state.

        let non_interactive = posture_from_probe(&config, None);
        assert_eq!(non_interactive.gap, None);
        assert!(non_interactive.earned);
        assert!(!non_interactive.privilege_unclear);

        let denied = posture_from_probe(&config, Some(GrantProbe::Denied));
        assert_eq!(denied.gap, Some(crate::output::SealGap::Privilege));
        assert!(!denied.earned);
        assert!(!denied.privilege_unclear);

        let granted = posture_from_probe(&config, Some(GrantProbe::Granted));
        assert!(granted.gap.is_some());
        assert!(granted.earned);
        assert!(!granted.privilege_unclear);

        // Unclear, even though the same config is incomplete: gap stays None
        // — never presume Units/FirstThread when privilege itself is
        // unconfirmed (the one behavior change vs. the old seal_completeness).
        let unclear = posture_from_probe(&config, Some(GrantProbe::Unclear));
        assert_eq!(unclear.gap, None);
        assert!(!unclear.earned);
        assert!(unclear.privilege_unclear);
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
    fn parse_findmnt_locus_reads_all_three_fields() {
        let locus =
            parse_findmnt_locus("TARGET=\"/\" FSROOT=\"/root\" UUID=\"ab12\"\n").unwrap();
        assert_eq!(locus.mount, PathBuf::from("/"));
        assert_eq!(locus.fsroot, PathBuf::from("/root"));
        assert_eq!(locus.uuid, "ab12");
        // No pool identity → no locus: the second look must stay silent
        // rather than key pools on a guess.
        assert!(parse_findmnt_locus("TARGET=\"/\" FSROOT=\"/root\" UUID=\"\"\n").is_none());
        assert!(parse_findmnt_locus("garbage").is_none());
    }

    /// The live-found double count (2026-07-05): one filesystem mounted at
    /// both `/` (subvol `root`) and `/home` (subvol `home`) — Fedora's
    /// default layout — must fold into ONE pool view holding both covered
    /// sources, not two views that each count the sibling as uncovered.
    #[test]
    fn fold_source_merges_mounts_of_the_same_filesystem() {
        let mut pools = std::collections::BTreeMap::new();
        let root_locus = PoolLocus {
            mount: PathBuf::from("/"),
            fsroot: PathBuf::from("/root"),
            uuid: "ab12".to_string(),
        };
        let home_locus = PoolLocus {
            mount: PathBuf::from("/home"),
            fsroot: PathBuf::from("/home"),
            uuid: "ab12".to_string(),
        };
        fold_source(&mut pools, &root_locus, Path::new("/"), None).unwrap();
        fold_source(&mut pools, &home_locus, Path::new("/home"), None).unwrap();
        assert_eq!(pools.len(), 1);
        let view = pools.get("ab12").unwrap();
        assert_eq!(
            view.covered,
            vec![PathBuf::from("root"), PathBuf::from("home")]
        );

        // Both sources covered + one foreign subvolume → exactly 1, not 5.
        let listing = vec![
            PathBuf::from("root"),
            PathBuf::from("home"),
            PathBuf::from("var/lib/machines"),
        ];
        let view = pools.remove("ab12").unwrap();
        assert_eq!(count_uncovered(&[(listing, view)]), 1);
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
            mount: PathBuf::from("/"),
            covered: vec![PathBuf::from("home")],
            snapshot_homes: vec![PathBuf::from("home/alice/.snapshots")],
        };
        assert_eq!(count_uncovered(&[(listing, view)]), 2);
    }

    #[test]
    fn count_uncovered_empty_listing_counts_nothing() {
        let view = PoolView {
            mount: PathBuf::from("/"),
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
