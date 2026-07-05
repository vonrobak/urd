//! The Encounter's I/O: the thin stdin loop that drives the pure state
//! machine (`encounter.rs`, UPI 072), the delve-deeper editor loop, and
//! the carve (UPI 074) — self-check the generated config, refuse
//! anything dishonest, and publish the file atomically without ever
//! clobbering an existing one.
//!
//! Refusal order is load-bearing: the checks that touch nothing (empty
//! strategy, self-check) run before any filesystem side effect.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context};
use chrono::NaiveDate;

use crate::commands::seal::SealOutcome;
use crate::commands::CliExit;
use crate::config::Config;
use crate::config_render::generate_config;
use crate::encounter::{
    advance, parse_line, AdvanceResult, AfterCarve, Effect, EncounterState, Input,
};
use crate::strategy::ProposedStrategy;
use crate::voice;

/// The seal step both terminal carve branches run — injected so tests of
/// the conversation and editor loops never spawn sudo.
type SealFn<'a> = &'a dyn Fn(&Config, &Path) -> anyhow::Result<SealOutcome>;

/// Post-carve tail shared by every valid-config exit: name the file, then
/// run the seal from the **reloaded on-disk config** — a delve edit may
/// have changed mappings, and sealing from the strategy-derived config
/// would ship day-one sudoers drift (adversary G6). The reload is the only
/// config in scope here, so the drift cannot regress silently.
fn post_carve(path: &Path, seal: SealFn<'_>) -> anyhow::Result<CliExit> {
    print!("{}", voice::render_post_carve(path));
    let config = Config::load(Some(path))
        .with_context(|| format!("reloading the carved config at {}", path.display()))?;
    seal(&config, path)?;
    Ok(CliExit::Done)
}

/// Staging file for the atomic publish, sibling to the final config.
const TEMP_NAME: &str = ".urd.toml.tmp";

/// Carve the approved strategy into `path`: generate, self-check, refuse
/// or publish. Nothing is ever written unless every check passes; an
/// existing config is never overwritten (deleting a config is a human
/// act — the designed refusal sentence is 072's trigger-time rendering,
/// this one is the race backstop).
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

    let (toml, _expected) = checked_toml(strategy, today)?;

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
    stage(&temp, &toml)
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

/// Generate and self-check the TOML for an approved strategy: migrate's
/// pattern plus the equality half. The rendered TOML must survive the
/// full load path (parse → expand_paths → validate) AND reload to
/// exactly the config it was rendered from — parse+validate alone would
/// pass a divergent-but-valid render (e.g. a non-UTF-8 path rendered
/// lossily names a phantom source). A config that differs from what the
/// runestone showed must never reach disk.
fn checked_toml(strategy: &ProposedStrategy, today: NaiveDate) -> anyhow::Result<(String, Config)> {
    let generated = generate_config(strategy, today);
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
    Ok((generated.toml, expected))
}

/// Write and fsync the staged config, truncating any stale temp a crashed
/// carve left behind.
fn stage(temp: &Path, toml: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(toml.as_bytes())?;
    file.sync_all()
}

// ── The conversation loop (UPI 072) ─────────────────────────────────────

/// Run the Fate Conversation: discover, walk the pure machine over
/// stdin, and execute its terminal effect (carve + confirm, carve +
/// editor, or farewell). The loop itself stays thin — every decision
/// lives in `encounter.rs` (test the functions, not readline).
pub fn run_conversation(config_path: Option<&Path>) -> anyhow::Result<CliExit> {
    let target = crate::commands::resolve_config_path(config_path)?;
    let inventory = crate::discovery::discover();
    let today = chrono::Local::now().date_naive();
    let mut result = EncounterState::begin(inventory, today);
    loop {
        let AdvanceResult {
            state,
            effect,
            notice,
        } = result;
        if let Some(notice) = &notice {
            print!("{}", voice::render_invalid_notice(notice));
        }
        match effect {
            Effect::Prompt(spec) => {
                print!("{}", voice::render_prompt(&spec));
                let input = match read_input_line()? {
                    // Closing stdin is walking away.
                    None => Input::Quit,
                    Some(line) => parse_line(&spec, &line),
                };
                result = advance(state, input);
            }
            Effect::Farewell(kind) => {
                print!("{}", voice::render_farewell(&kind));
                return Ok(CliExit::Done);
            }
            Effect::Carve {
                strategy,
                today,
                then,
            } => {
                // A carve refusal (config appeared mid-conversation, or a
                // self-check failure) surfaces as the error it is —
                // nothing was written, the sentence says so.
                carve_config(&strategy, today, &target)?;
                let seal: SealFn<'_> = &|config, path| {
                    crate::commands::seal::resume_seal(config, path)
                };
                return match then {
                    AfterCarve::Confirm => post_carve(&target, seal),
                    AfterCarve::Edit => delve(&strategy, today, &target, seal),
                };
            }
        }
    }
}

/// One line from stdin; `None` on EOF. Prompts end without a newline
/// marker, so flush before blocking. Shared with the earning's consent
/// loop (`commands::seal`).
pub(crate) fn read_input_line() -> anyhow::Result<Option<String>> {
    print!("> ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .context("failed to read from stdin")?;
    Ok(if n == 0 { None } else { Some(line) })
}

// ── Delve deeper: the editor loop (UPI 072, arc grill Q7) ──────────────

/// The user's choice in the visudo-shaped failure loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorChoice {
    EditAgain,
    Revert,
    QuitKeep,
}

/// Resolve which editor to launch: `$VISUAL`, then `$EDITOR`, split on
/// whitespace (first token = program, rest = args; no shell
/// interpretation — a quoted-argument editor value is a documented
/// limitation). `None` when neither is set or both are blank.
fn resolve_editor(visual: Option<&str>, editor: Option<&str>) -> Option<Vec<String>> {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(|v| v.split_whitespace().map(str::to_string).collect())
}

/// Classify one line of the failure-loop prompt. Enter defaults to
/// edit-again; `r` exists only while a generated baseline does.
fn parse_editor_choice(line: &str, revert_available: bool) -> Option<EditorChoice> {
    match line.trim().to_lowercase().as_str() {
        "" | "e" => Some(EditorChoice::EditAgain),
        "r" if revert_available => Some(EditorChoice::Revert),
        "q" => Some(EditorChoice::QuitKeep),
        _ => None,
    }
}

/// Delve deeper: open the carved file in the user's editor, re-validate
/// on exit, and walk the (e)dit / (r)evert / (q)uit failure loop until
/// the config loads or the user keeps it broken. The editor's exit
/// status is deliberately ignored — re-validation of the file is the
/// only truth (no editor reports abort reliably).
fn delve(
    strategy: &ProposedStrategy,
    today: NaiveDate,
    path: &Path,
    seal: SealFn<'_>,
) -> anyhow::Result<CliExit> {
    let Some(command) = editor_from_env() else {
        // The file is already carved and valid — keeping it is the
        // honest fallback, never deletion. This exit skips the earning;
        // the sentence names `urd init` as the resume verb.
        print!("{}", voice::render_no_editor(path));
        return Ok(CliExit::Done);
    };
    delve_with(&command, strategy, today, path, seal)
}

/// `resolve_editor` over the live environment — the only env read in
/// this module, shared by delve and the fix-it loop.
fn editor_from_env() -> Option<Vec<String>> {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    resolve_editor(visual.as_deref(), editor.as_deref())
}

/// Launch the editor on the file and wait. The exit status is
/// deliberately dropped: re-validation of the file is the only truth.
fn open_in_editor(command: &[String], path: &Path) -> anyhow::Result<()> {
    std::process::Command::new(&command[0])
        .args(&command[1..])
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor `{}`", command[0]))?;
    Ok(())
}

fn delve_with(
    command: &[String],
    strategy: &ProposedStrategy,
    today: NaiveDate,
    path: &Path,
    seal: SealFn<'_>,
) -> anyhow::Result<CliExit> {
    loop {
        open_in_editor(command, path)?;
        let error = match Config::load(Some(path)) {
            Ok(_) => return post_carve(path, seal),
            Err(e) => e.to_string(),
        };
        match failure_loop_choice(&error, true)? {
            EditorChoice::EditAgain => {}
            EditorChoice::Revert => {
                overwrite_with_generated(strategy, today, path)?;
                return post_carve(path, seal);
            }
            EditorChoice::QuitKeep => return Err(kept_invalid(path)),
        }
    }
}

/// Prompt the failure loop until a usable letter arrives. EOF keeps the
/// file (the least destructive reading of a closed stdin).
fn failure_loop_choice(error: &str, revert_available: bool) -> anyhow::Result<EditorChoice> {
    loop {
        print!("{}", voice::render_editor_failure(error, revert_available));
        let Some(line) = read_input_line()? else {
            return Ok(EditorChoice::QuitKeep);
        };
        if let Some(choice) = parse_editor_choice(&line, revert_available) {
            return Ok(choice);
        }
    }
}

/// Replace the file with the freshly generated, self-checked TOML — the
/// one place 072 intentionally overwrites, and only on the user's
/// explicit (r)evert. Never routed through `carve_config`, whose
/// no-clobber contract stays absolute.
fn overwrite_with_generated(
    strategy: &ProposedStrategy,
    today: NaiveDate,
    path: &Path,
) -> anyhow::Result<()> {
    let (toml, _expected) = checked_toml(strategy, today)?;
    let dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent directory", path.display()))?;
    let temp = dir.join(TEMP_NAME);
    stage(&temp, &toml)
        .with_context(|| format!("failed to stage config at {}", temp.display()))?;
    fs::rename(&temp, path)
        .with_context(|| format!("failed to restore config at {}", path.display()))
}

fn kept_invalid(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "the config at {} does not load — kept as you left it.\n\
         `urd init` returns to this fix-it loop.",
        path.display()
    )
}

/// `urd init`'s fix-it re-entry for an invalid config (arc grill Q5/Q7):
/// the same failure loop without (r)evert — no generated baseline exists
/// for a hand-edited file. Returns the loaded config on success so init
/// can continue being the make-whole verb.
pub fn fix_invalid_config(path: &Path, initial_error: &str) -> anyhow::Result<Config> {
    let Some(command) = editor_from_env() else {
        print!("{}", voice::render_no_editor(path));
        bail!(
            "the config at {} does not load: {initial_error}",
            path.display()
        );
    };
    let mut error = initial_error.to_string();
    loop {
        match failure_loop_choice(&error, false)? {
            // Revert is never offered without a baseline; if it somehow
            // arrived it would mean edit-again, the safe reading.
            EditorChoice::EditAgain | EditorChoice::Revert => {}
            EditorChoice::QuitKeep => return Err(kept_invalid(path)),
        }
        open_in_editor(&command, path)?;
        match Config::load(Some(path)) {
            Ok(config) => return Ok(config),
            Err(e) => error = e.to_string(),
        }
    }
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

    // ── checked_toml / conversation-loop pieces (UPI 072) ───────────────

    #[test]
    fn checked_toml_matches_what_carve_writes() {
        let strategy = sheltered_strategy();
        let (toml, expected) = checked_toml(&strategy, today()).unwrap();

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), toml);

        let reloaded = Config::from_str(&toml).unwrap();
        assert_eq!(reloaded, expected);
    }

    #[test]
    fn resolve_editor_prefers_visual_then_editor_then_none() {
        assert_eq!(
            resolve_editor(Some("code --wait"), Some("vi")),
            Some(vec!["code".to_string(), "--wait".to_string()])
        );
        assert_eq!(
            resolve_editor(None, Some("nano")),
            Some(vec!["nano".to_string()])
        );
        assert_eq!(
            resolve_editor(Some("  "), Some("vi")),
            Some(vec!["vi".to_string()]),
            "blank VISUAL falls through to EDITOR"
        );
        assert_eq!(resolve_editor(None, None), None);
        assert_eq!(resolve_editor(Some(""), Some("   ")), None);
    }

    #[test]
    fn parse_editor_choice_defaults_to_edit_and_gates_revert() {
        assert_eq!(parse_editor_choice("", true), Some(EditorChoice::EditAgain));
        assert_eq!(parse_editor_choice("e", true), Some(EditorChoice::EditAgain));
        assert_eq!(parse_editor_choice("E", true), Some(EditorChoice::EditAgain));
        assert_eq!(parse_editor_choice("r", true), Some(EditorChoice::Revert));
        assert_eq!(
            parse_editor_choice("r", false),
            None,
            "revert must not exist without a generated baseline"
        );
        assert_eq!(parse_editor_choice("q", true), Some(EditorChoice::QuitKeep));
        assert_eq!(parse_editor_choice("x", true), None);
    }

    #[test]
    fn overwrite_with_generated_restores_the_carved_content() {
        let strategy = sheltered_strategy();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();
        let carved = std::fs::read_to_string(&path).unwrap();

        std::fs::write(&path, "ruined by an edit [[[").unwrap();
        overwrite_with_generated(&strategy, today(), &path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), carved);
        assert!(!dir.path().join(TEMP_NAME).exists(), "temp not cleaned up");
        assert!(Config::load(Some(&path)).is_ok());
    }

    #[test]
    fn delve_with_a_clean_editor_exit_confirms_the_valid_file() {
        // /bin/true touches nothing: the carved file stays valid and the
        // delve ends confirmed — the editor's exit status is not what
        // decides, the file's validity is.
        let strategy = sheltered_strategy();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();

        let no_seal: SealFn<'_> = &|_, _| Ok(SealOutcome::Sealed);
        let result = delve_with(&["/bin/true".to_string()], &strategy, today(), &path, no_seal);
        assert_eq!(result.unwrap(), CliExit::Done);
        assert!(Config::load(Some(&path)).is_ok());
    }

    #[test]
    fn delve_seals_from_the_reloaded_config_not_the_strategy() {
        // Adversary G6: an editor that adds a drive must reach the seal —
        // sealing from the strategy-derived config would render a sudoers
        // file missing the new mapping on day one.
        let strategy = sheltered_strategy();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();

        let script = dir.path().join("add-drive.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >> \"$1\" <<'EOF'\n\n[[drives]]\nlabel = \"delve-added\"\n\
             mount_path = \"/run/media/alice/delve-added\"\nsnapshot_root = \".snapshots\"\n\
             role = \"offsite\"\nEOF\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let sealed_labels = std::cell::RefCell::new(Vec::<String>::new());
        let capture: SealFn<'_> = &|config, _| {
            sealed_labels
                .borrow_mut()
                .extend(config.drives.iter().map(|d| d.label.clone()));
            Ok(SealOutcome::Sealed)
        };
        let result = delve_with(&[script.display().to_string()], &strategy, today(), &path, capture);
        assert_eq!(result.unwrap(), CliExit::Done);
        assert!(
            sealed_labels.borrow().iter().any(|l| l == "delve-added"),
            "the seal must see the drive the edit added: {:?}",
            sealed_labels.borrow()
        );
    }

    #[test]
    fn delve_with_a_ruining_editor_keeps_the_file_on_eof() {
        // An "editor" that writes garbage puts the loop into the failure
        // prompt; test stdin is at EOF, which reads as quit-keeping-the-
        // file: the error is named, the user's bytes survive.
        let strategy = sheltered_strategy();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("urd.toml");
        carve_config(&strategy, today(), &path).unwrap();

        let script = dir.path().join("ruin.sh");
        std::fs::write(&script, "#!/bin/sh\necho 'ruined [[[' > \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let no_seal: SealFn<'_> = &|_, _| Ok(SealOutcome::Sealed);
        let result =
            delve_with(&[script.display().to_string()], &strategy, today(), &path, no_seal);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("kept as you left it"), "{err}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("ruined"),
            "the user's keystrokes must survive"
        );
    }
}
