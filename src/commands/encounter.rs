//! The Encounter's carve wiring (UPI 074) — the I/O half of config
//! generation. The conversation that produces the approved strategy is
//! UPI 072's; this module owns the last step: self-check the generated
//! config, refuse anything dishonest, and publish the file atomically
//! without ever clobbering an existing one.
//!
//! Refusal order is load-bearing: the checks that touch nothing (empty
//! strategy, self-check) run before any filesystem side effect.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context};
use chrono::NaiveDate;

use crate::config::Config;
use crate::config_render::generate_config;
use crate::strategy::ProposedStrategy;

/// Staging file for the atomic publish, sibling to the final config.
const TEMP_NAME: &str = ".urd.toml.tmp";

/// Carve the approved strategy into `path`: generate, self-check, refuse
/// or publish. Nothing is ever written unless every check passes; an
/// existing config is never overwritten (deleting a config is a human
/// act — the designed refusal sentence is 072's trigger-time rendering,
/// this one is the race backstop).
#[allow(dead_code)] // Consumed by UPI 072 (conversation).
pub fn carve_config(
    strategy: &ProposedStrategy,
    today: NaiveDate,
    path: &Path,
) -> anyhow::Result<()> {
    // An empty strategy would carve a config that protects nothing while
    // silencing the "no config → offer the encounter" trigger. 072 renders
    // the empty case honestly and never calls carve; this is the backstop.
    if strategy.subvolumes.is_empty() {
        bail!("the encounter proposed nothing to protect — nothing to carve, nothing written");
    }

    let generated = generate_config(strategy, today);

    // Self-check, migrate's pattern plus the equality half: the rendered
    // TOML must survive the full load path (parse → expand_paths →
    // validate) AND reload to exactly the config it was rendered from.
    // Parse+validate alone would pass a divergent-but-valid render (e.g. a
    // non-UTF-8 path rendered lossily names a phantom source) — a config
    // that differs from what the runestone showed must never reach disk.
    let reparsed = match Config::from_str(&generated.toml) {
        Ok(config) => config,
        Err(e) => bail!(
            "the encounter would produce an invalid config — nothing written.\n\
             load check: {e}"
        ),
    };
    let mut expected = generated.config;
    expected.expand_paths();
    if reparsed != expected {
        bail!(
            "the carved config would not reload to the approved strategy — nothing written.\n\
             (render/parse divergence: a bug in urd, not in your answers)"
        );
    }

    if path.exists() {
        return Err(exists_refusal(path));
    }

    let dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent directory", path.display()))?;
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config directory {}", dir.display()))?;

    // Stage next to the target, then publish via hard_link — which fails
    // with AlreadyExists instead of clobbering, so a config that appeared
    // mid-carve survives and the carve loses loudly.
    let temp = dir.join(TEMP_NAME);
    stage(&temp, &generated.toml)
        .with_context(|| format!("failed to stage config at {}", temp.display()))?;

    if let Err(e) = fs::hard_link(&temp, path) {
        let _ = fs::remove_file(&temp);
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(exists_refusal(path));
        }
        return Err(e)
            .with_context(|| format!("failed to publish config at {}", path.display()));
    }
    let _ = fs::remove_file(&temp);
    Ok(())
}

/// Write and fsync the staged config, truncating any stale temp a crashed
/// carve left behind.
fn stage(temp: &Path, toml: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(toml.as_bytes())?;
    file.sync_all()
}

fn exists_refusal(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "a config already exists at {} — nothing written.\n\
         Edit it yourself, or move it aside and run `urd init` again.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::test_support::{
        drive, external_btrfs_drive, inventory, pool, subvol, today, EXTERNAL_POOL, SYSTEM_POOL,
    };
    use crate::discovery::{DriveClass, LuksState};
    use crate::strategy::{
        derive_strategy, FateAnswers, GranularityAnswer, Importance, ImportanceAnswer,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A real derivable strategy: Fedora pair + one usable external drive,
    /// `/home` classified irreplaceable.
    fn sheltered_strategy() -> crate::strategy::ProposedStrategy {
        let mut inv = inventory(
            vec![pool(SYSTEM_POOL, &["/", "/home"])],
            vec![
                subvol("/", "/root", SYSTEM_POOL),
                subvol("/home", "/home", SYSTEM_POOL),
            ],
            vec![drive(
                "nvme0n1",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/backup"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));

        let answers = FateAnswers {
            importance: vec![ImportanceAnswer {
                mountpoint: PathBuf::from("/home"),
                importance: Importance::Irreplaceable,
            }],
            residence: None,
            granularity: GranularityAnswer::YesterdayIsFine,
            drive_residency: Vec::new(),
        };
        derive_strategy(&inv, &answers, today())
    }

    /// All-whole-pool inventory: zero candidates, empty strategy.
    fn empty_strategy() -> crate::strategy::ProposedStrategy {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/"])],
            vec![subvol("/", "/", SYSTEM_POOL)],
            vec![drive(
                "nvme0n1",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let answers = FateAnswers {
            importance: Vec::new(),
            residence: None,
            granularity: GranularityAnswer::YesterdayIsFine,
            drive_residency: Vec::new(),
        };
        derive_strategy(&inv, &answers, today())
    }

    #[test]
    fn carve_writes_a_config_that_reloads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&sheltered_strategy(), today(), &path).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(Config::from_str(&on_disk).is_ok());
        assert!(!dir.path().join(TEMP_NAME).exists(), "temp not cleaned up");
    }

    #[test]
    fn carve_creates_missing_parent_directories() {
        // #250 finding 16: nothing created ~/.config/urd on a fresh system.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/config/urd/urd.toml");
        carve_config(&sheltered_strategy(), today(), &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn carve_refuses_existing_config_with_nothing_written() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        std::fs::write(&path, "# hand-written\n").unwrap();

        let err = carve_config(&sheltered_strategy(), today(), &path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert!(err.to_string().contains(&path.display().to_string()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# hand-written\n");
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn carve_refuses_empty_strategy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        let err = carve_config(&empty_strategy(), today(), &path).unwrap_err();
        assert!(err.to_string().contains("nothing written"));
        assert!(!path.exists());
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn carve_self_check_refuses_hostile_path_with_nothing_written() {
        // A quote in a mountpoint breaks the unescaped render — the
        // self-check must refuse the parse failure, never write garbage.
        let mut strategy = sheltered_strategy();
        strategy.subvolumes[0].source = PathBuf::from("/data/\"quoted\"");

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        let err = carve_config(&strategy, today(), &path).unwrap_err();
        assert!(err.to_string().contains("invalid config"));
        assert!(err.to_string().contains("nothing written"));
        assert!(!path.exists());
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn carve_refuses_divergent_render_nothing_written() {
        // 074 adversary F2: a non-UTF-8 source renders lossily, PARSES
        // validly, and names a phantom path — only the equality half of
        // the self-check catches it.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut strategy = sheltered_strategy();
        strategy.subvolumes[0].source =
            PathBuf::from(OsStr::from_bytes(b"/data/\xff-photos"));

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        let err = carve_config(&strategy, today(), &path).unwrap_err();
        assert!(err.to_string().contains("would not reload"));
        assert!(err.to_string().contains("nothing written"));
        assert!(!path.exists());
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn carve_overwrites_a_stale_temp_file() {
        // A crash between stage and publish leaves a temp behind; the next
        // carve truncates it and still publishes cleanly.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        std::fs::write(dir.path().join(TEMP_NAME), "stale junk").unwrap();

        carve_config(&sheltered_strategy(), today(), &path).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(Config::from_str(&on_disk).is_ok());
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn carve_result_reloads_via_load_path_equivalence() {
        // What lands on disk must reload to exactly the config the
        // self-check approved — guards divergence between the checked
        // string and the written bytes.
        let strategy = sheltered_strategy();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let reloaded = Config::from_str(&on_disk).unwrap();
        let mut expected = crate::config_render::generate_config(&strategy, today()).config;
        expected.expand_paths();
        assert_eq!(reloaded, expected);
    }
}
