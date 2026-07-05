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
use std::path::Path;
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

/// Resume the seal at its first incomplete stage. Stage 1: the earning.
/// (075 extends this with further stages; keep the early-return shape.)
pub fn resume_seal(config: &Config, config_path: &Path) -> anyhow::Result<SealOutcome> {
    earn_privilege(config, config_path)
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
fn invoking_username() -> anyhow::Result<String> {
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

/// Whether a status surface should name the "configured but unsealed" state.
/// Single owner of the rule (adversary F5): interactive only — a denied
/// probe writes an auth log line, so automated and monitoring callers must
/// never generate that noise — and only on a clear denial (`Unclear` stays
/// silent rather than guess).
pub(crate) fn probe_unsealed(config: &Config, output_mode: OutputMode) -> bool {
    output_mode == OutputMode::Interactive
        && probe_grant(&config.general.btrfs_path).0 == GrantProbe::Denied
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
