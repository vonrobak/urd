//! Integration test (#136): every `--subvolume`-accepting CLI command must
//! reject an unknown subvolume name with a non-zero exit and a clear message.
//!
//! Guards against the regression class from #134/#135. The fix as shipped relies
//! on per-handler discipline (`cli_validation::require_known_subvolume` at the
//! top of each `run`) with no compile-time enforcement — a future command that
//! forgets the guard would silently reintroduce the empty-set bug (#134). The
//! canonical list below is the tripwire: a 9th `--subvolume`-accepting command
//! must be added here (and given the guard) or it ships without coverage.
//!
//! `#[ignore]`'d because it builds and shells out to the binary (slow); run in
//! CI's pre-release gate via `cargo test -- --ignored`.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

/// Minimal valid config with two subvolumes. Paths are absolute (under `dir`)
/// and need not exist on disk — config validation checks path *safety*, not
/// presence, and the subvolume guard fires before any filesystem access.
fn write_config(dir: &TempDir) -> PathBuf {
    let base = dir.path().display();
    let toml = format!(
        r#"
drives = []

[general]
state_db = "{base}/urd.db"
metrics_file = "{base}/backup.prom"
log_dir = "{base}"

[local_snapshots]
roots = [
  {{ path = "{base}/snap", subvolumes = ["subvol1", "subvol2"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "subvol1"
short_name = "s1"
source = "{base}/data/subvol1"

[[subvolumes]]
name = "subvol2"
short_name = "s2"
source = "{base}/data/subvol2"
"#
    );
    let path = dir.path().join("urd.toml");
    std::fs::write(&path, toml).expect("write throwaway config");
    path
}

/// Every command that accepts a subvolume selector, with the exact arg form it
/// uses (`--subvolume <name>` for most; a positional for `retention-preview`;
/// `get` also needs a path + `--at`). Keep this in sync with the handlers that
/// call `require_known_subvolume`.
const SUBVOLUME_COMMANDS: &[&[&str]] = &[
    &["plan", "--subvolume", "bogus"],
    &["backup", "--dry-run", "--subvolume", "bogus"],
    &["history", "--subvolume", "bogus"],
    &["calibrate", "--subvolume", "bogus"],
    &["verify", "--subvolume", "bogus"],
    &["events", "--subvolume", "bogus"],
    &["get", "/some/file", "--at", "2026-01-01", "--subvolume", "bogus"],
    &["retention-preview", "bogus"], // positional subvolume
];

#[test]
#[ignore = "builds + shells out to the binary; run via `cargo test -- --ignored`"]
fn every_subvolume_command_rejects_unknown_name() {
    let dir = TempDir::new().expect("tempdir");
    let config = write_config(&dir);
    let bin = env!("CARGO_BIN_EXE_urd");

    for argv in SUBVOLUME_COMMANDS {
        let output = Command::new(bin)
            .arg("--config")
            .arg(&config)
            .args(*argv)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `urd {}`: {e}", argv.join(" ")));

        assert!(
            !output.status.success(),
            "`urd {}` should exit non-zero for an unknown subvolume, but succeeded\nstdout:\n{}\nstderr:\n{}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(r#"no subvolume named "bogus""#),
            "`urd {}` stderr should name the unknown subvolume; got:\n{stderr}",
            argv.join(" "),
        );
    }
}
