//! External process probes — the read-only system questions urd asks outside
//! `btrfs`: which mount holds a path (`findmnt`), which block devices and btrfs
//! mounts exist (`lsblk`, `findmnt -t btrfs`), whether the user's session
//! lingers (`loginctl`), what the effective sudo grant lists (`sudo -n -l`), and
//! how large a snapshot is (`du -sb`).
//!
//! I/O module. One function per probe: each spawns its command with the exact
//! argument list and `LC_ALL=C` its callers depend on, and returns parsed data
//! (or the raw listing where the parse is a caller's domain logic — discovery's
//! mount tree, sudoers' privilege grammar). Callers own the policy of what a
//! failed probe means; the variants here keep every failure distinguishable so
//! no caller has to re-run a probe to find out why it failed. `btrfs` itself
//! goes through `BtrfsOps` (`btrfs.rs`), never here.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::UrdError;

// ── findmnt --target: which filesystem holds a path ────────────────────

/// One resolved row from the concentrated `findmnt --target` probe: which
/// filesystem (if any) holds an arbitrary path. All three fields are
/// independent — a mount can resolve a `target` with an empty `uuid` (no
/// superblock UUID), and `fstype` lets a caller gate on "must be btrfs"
/// without a second probe (UPI 084; see `discover()`'s home-pool lookup).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindmntEntry {
    pub target: Option<PathBuf>,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
}

/// The concentrated `findmnt --target` probe (UPI 084): one subprocess spawn
/// and one parser for "which filesystem holds this path?" queries, shared by
/// `pools::pool_uuid_for_path`, `pools::resolve_source_pool`,
/// `drives::get_filesystem_uuid`, and `discovery`'s home-pool lookup (which
/// additionally gates on `fstype == "btrfs"` at its call site, since that
/// filter is specific to discovery's zero-state inventory and not shared by
/// the other consumers).
///
/// Uses `-J` (JSON) output — the most robust `findmnt` format to parse,
/// tolerant of field reordering and locale quirks that broke the older `-P`
/// key="value" parser. `Err` only on a findmnt I/O failure with stderr
/// content; a missing/unmounted path → `Ok(FindmntEntry::default())`.
pub fn findmnt_target(path: &Path) -> crate::error::Result<FindmntEntry> {
    let path_str = path.to_str().ok_or_else(|| UrdError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not valid UTF-8",
        ),
    })?;

    let output = Command::new("findmnt")
        .env("LC_ALL", "C")
        .args(["-J", "-o", "TARGET,FSTYPE,UUID", "--target", path_str])
        .output()
        .map_err(|e| UrdError::Io {
            path: PathBuf::from("findmnt"),
            source: e,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        if stdout.trim().is_empty() {
            // findmnt complained about a missing path; treat as "unresolved".
            return Ok(FindmntEntry::default());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UrdError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(format!("findmnt failed: {}", stderr.trim())),
        });
    }

    Ok(parse_findmnt_probe_target(&stdout))
}

/// Pure parse of `findmnt -J -o TARGET,FSTYPE,UUID --target <path>` output —
/// a single-entry `filesystems` array. Empty/absent fields map to `None`.
/// Extracted for unit testing (the subprocess wrapper above stays a thin I/O
/// shim).
#[must_use]
fn parse_findmnt_probe_target(json: &str) -> FindmntEntry {
    let Some(fs) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|root| root.get("filesystems")?.as_array()?.first().cloned())
    else {
        return FindmntEntry::default();
    };
    let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_string);
    FindmntEntry {
        target: non_empty(fs.get("target").and_then(serde_json::Value::as_str)).map(PathBuf::from),
        fstype: non_empty(fs.get("fstype").and_then(serde_json::Value::as_str)),
        uuid: non_empty(fs.get("uuid").and_then(serde_json::Value::as_str)),
    }
}

// ── findmnt --target with FSROOT: a path's pool locus ──────────────────

/// Where a promised source lives: the mount holding it, that mount's
/// FSROOT, and the filesystem's UUID (the pool identity).
pub(crate) struct PoolLocus {
    pub(crate) mount: PathBuf,
    pub(crate) fsroot: PathBuf,
    pub(crate) uuid: String,
}

/// One findmnt call: the mountpoint holding `path`, that mount's FSROOT,
/// and the filesystem UUID. `None` on any failure (spawn, exit, parse) —
/// the seal's second look is annotation, not verification.
pub(crate) fn findmnt_locus(path: &Path) -> Option<PoolLocus> {
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

// ── Discovery's inventory probes (lsblk, findmnt -t btrfs) ─────────────

/// Run one probe command. Error mapping per the pools.rs convention:
/// spawn failure → `Io` with the binary name as path; non-zero exit with
/// stdout content or stderr → `Io`. `tolerate_empty_failure` maps non-zero
/// exit with empty stdout to `Ok("")` — required for `findmnt -t btrfs`,
/// which exits non-zero on a machine with zero btrfs mounts; lsblk has no
/// such legitimate empty failure, so there it stays an error.
fn run_probe(cmd: &str, args: &[&str], tolerate_empty_failure: bool) -> crate::error::Result<String> {
    let output = Command::new(cmd)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| UrdError::Io {
            path: PathBuf::from(cmd),
            source: e,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        if tolerate_empty_failure && stdout.trim().is_empty() && output.stderr.is_empty() {
            return Ok(String::new());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UrdError::Io {
            path: PathBuf::from(cmd),
            source: std::io::Error::other(format!("{cmd} failed: {}", stderr.trim())),
        });
    }
    Ok(stdout)
}

/// `lsblk -J -o <columns>`: the raw block-device tree. The column list and
/// its parser are discovery's (`discovery::LSBLK_COLUMNS`).
pub(crate) fn lsblk_json(columns: &str) -> crate::error::Result<String> {
    run_probe("lsblk", &["-J", "-o", columns], false)
}

/// `findmnt -t btrfs -J`: the raw mount tree of every mounted btrfs
/// filesystem, `Ok("")` on a machine with none. Discovery parses it into its
/// own mount rows.
pub(crate) fn findmnt_btrfs_json() -> crate::error::Result<String> {
    run_probe("findmnt", &["-t", "btrfs", "-J"], true)
}

// ── loginctl: session lingering ────────────────────────────────────────

/// The answer to `loginctl show-user <user> --property=Linger`. A user
/// timer fires only while a session exists, so lingering decides whether
/// scheduled backups run while the user is logged out (UPI 075, adversary F1).
#[derive(Debug)]
pub(crate) enum Linger {
    /// `Linger=yes`.
    On,
    /// `Linger=no`.
    Off,
    /// loginctl succeeded but printed something else (trimmed stdout).
    Unrecognized(String),
    /// loginctl exited non-zero (trimmed stderr).
    Failed(String),
    /// loginctl could not be run at all.
    NotRun(std::io::Error),
}

/// Ask loginctl whether `user`'s session lingers.
pub(crate) fn loginctl_linger(user: &str) -> Linger {
    match Command::new("loginctl")
        .env("LC_ALL", "C")
        .args(["show-user", user, "--property=Linger"])
        .output()
    {
        Ok(out) if out.status.success() => {
            match String::from_utf8_lossy(&out.stdout).trim() {
                "Linger=no" => Linger::Off,
                "Linger=yes" => Linger::On,
                other => Linger::Unrecognized(other.to_string()),
            }
        }
        Ok(out) => Linger::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Linger::NotRun(e),
    }
}

// ── sudo -n -l: the effective privilege listing ────────────────────────

/// `LC_ALL=C sudo -n -l`: the raw effective-privilege listing, for
/// `sudoers::parse_privilege_listing`. `Err` = sudo could not be run;
/// `Ok(None)` = sudo exited non-zero (the listing needs a password);
/// `Ok(Some(listing))` on success.
pub(crate) fn sudo_privilege_listing() -> std::io::Result<Option<String>> {
    let out = Command::new("sudo").env("LC_ALL", "C").args(["-n", "-l"]).output()?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

// ── du -sb: a snapshot's apparent size ─────────────────────────────────

/// The answer to `du -sb <path>`: the apparent size in bytes, for
/// `urd calibrate`.
#[derive(Debug)]
pub(crate) enum DuSize {
    /// A positive byte count from du's first field.
    Bytes(u64),
    /// du succeeded but its output held no positive byte count (trimmed stdout).
    Unusable(String),
    /// du exited non-zero (trimmed stderr).
    Failed(String),
    /// du could not be run at all.
    NotRun(std::io::Error),
}

/// Measure `path`'s apparent size with `du -sb`.
pub(crate) fn du_apparent_bytes(path: &Path) -> DuSize {
    match Command::new("du").env("LC_ALL", "C").args(["-sb"]).arg(path).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let bytes: Option<u64> = stdout
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .filter(|&b: &u64| b > 0);
            match bytes {
                Some(bytes) => DuSize::Bytes(bytes),
                None => DuSize::Unusable(stdout.trim().to_string()),
            }
        }
        Ok(output) => DuSize::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(e) => DuSize::NotRun(e),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── UPI 084: concentrated findmnt --target probe ────────────────

    #[test]
    fn parse_findmnt_probe_target_both_present() {
        let entry = parse_findmnt_probe_target(
            r#"{"filesystems":[{"target":"/","fstype":"btrfs","uuid":"6c1a-1234"}]}"#,
        );
        assert_eq!(entry.uuid.as_deref(), Some("6c1a-1234"));
        assert_eq!(entry.target, Some(PathBuf::from("/")));
        assert_eq!(entry.fstype.as_deref(), Some("btrfs"));
    }

    #[test]
    fn parse_findmnt_probe_target_empty_uuid_is_none() {
        // Non-BTRFS mount: TARGET resolves, UUID is empty → mountpoint still
        // surfaces so storage-signal gathering can read free-ratio (S5).
        let entry = parse_findmnt_probe_target(
            r#"{"filesystems":[{"target":"/boot","fstype":"vfat","uuid":""}]}"#,
        );
        assert_eq!(entry.uuid, None);
        assert_eq!(entry.target, Some(PathBuf::from("/boot")));
        assert_eq!(entry.fstype.as_deref(), Some("vfat"));
    }

    #[test]
    fn parse_findmnt_probe_target_empty_output_is_default() {
        assert_eq!(parse_findmnt_probe_target(""), FindmntEntry::default());
        assert_eq!(parse_findmnt_probe_target("{}"), FindmntEntry::default());
        assert_eq!(
            parse_findmnt_probe_target(r#"{"filesystems":[]}"#),
            FindmntEntry::default()
        );
    }

    #[test]
    fn parse_findmnt_probe_target_tolerates_target_with_space() {
        let entry = parse_findmnt_probe_target(
            r#"{"filesystems":[{"target":"/mnt/my drive","fstype":"btrfs","uuid":"abcd"}]}"#,
        );
        assert_eq!(entry.uuid.as_deref(), Some("abcd"));
        assert_eq!(entry.target, Some(PathBuf::from("/mnt/my drive")));
    }

    // ── The seal's second look: pool locus ──────────────────────────

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
}
