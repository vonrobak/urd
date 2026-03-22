use colored::Colorize;

use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::plan::{FileSystemState, RealFileSystemState};
use crate::state::StateDb;

pub fn run(config: Config) -> anyhow::Result<()> {
    let fs_state = RealFileSystemState;
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();
    let mounted_drives: Vec<_> = config
        .drives
        .iter()
        .filter(|d| drives::is_drive_mounted(d))
        .collect();

    // ── Per-subvolume table ────────────────────────────────────────────

    // Build header: SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]  CHAIN
    let mut headers: Vec<String> = vec!["SUBVOLUME".to_string(), "LOCAL".to_string()];
    for drive in &mounted_drives {
        headers.push(drive.label.clone());
    }
    headers.push("CHAIN".to_string());

    let mut rows: Vec<Vec<String>> = Vec::new();

    for sv in &config.subvolumes {
        let Some(root) = config.snapshot_root_for(&sv.name) else {
            continue;
        };
        let local_dir = root.join(&sv.name);

        // Local snapshot count
        let local_count = fs_state
            .local_snapshots(&root, &sv.name)
            .map(|s| s.len())
            .unwrap_or(0);

        let mut row = vec![sv.name.clone(), local_count.to_string()];

        // Per-drive: external snapshot count + chain health (worst case)
        let mut worst_health: Option<ChainHealth> = None;
        let mut any_ext = false;
        for drive in &mounted_drives {
            let ext_count = fs_state
                .external_snapshots(drive, &sv.name)
                .map(|s| s.len())
                .unwrap_or(0);
            row.push(if ext_count > 0 {
                any_ext = true;
                ext_count.to_string()
            } else {
                "\u{2014}".to_string() // em dash
            });

            // Chain health: track worst case across all drives
            let ext_dir = drives::external_snapshot_dir(drive, &sv.name);
            let health = chain_health(&local_dir, &drive.label, ext_count, &ext_dir);
            worst_health = Some(match worst_health {
                Some(current) => current.min(health),
                None => health,
            });
        }

        let chain_display = if mounted_drives.is_empty() || (!any_ext && worst_health.is_none()) {
            "\u{2014}".to_string()
        } else {
            worst_health.map_or("\u{2014}".to_string(), |h| h.to_string())
        };

        row.push(chain_display);
        rows.push(row);
    }

    // Print table with simple column alignment
    print_table(&headers, &rows);

    // ── Drive summary ──────────────────────────────────────────────────

    println!();
    if mounted_drives.is_empty() {
        println!("{}", "Drives: none mounted".dimmed());
    } else {
        for drive in &mounted_drives {
            let free = drives::filesystem_free_bytes(&drive.mount_path).unwrap_or(0);
            println!(
                "Drives: {} {} ({} free)",
                drive.label.bold(),
                "mounted".green(),
                crate::types::ByteSize(free),
            );
        }
    }

    // Unmounted drives
    for drive in &config.drives {
        if !drives::is_drive_mounted(drive) {
            println!(
                "Drives: {} {}",
                drive.label.bold(),
                "not mounted".dimmed(),
            );
        }
    }

    // ── Last run ───────────────────────────────────────────────────────

    if config.general.state_db.exists() {
        match StateDb::open(&config.general.state_db) {
            Ok(db) => match db.last_run() {
                Ok(Some(run)) => {
                    let result_colored = match run.result.as_str() {
                        "success" => run.result.green().to_string(),
                        "partial" => run.result.yellow().to_string(),
                        "failure" => run.result.red().to_string(),
                        _ => run.result.clone(),
                    };
                    let duration_str = run
                        .finished_at
                        .as_ref()
                        .and_then(|f| crate::types::format_run_duration(&run.started_at, f))
                        .unwrap_or_default();
                    println!(
                        "Last backup: {} ({}{}) [#{}]",
                        run.started_at,
                        result_colored,
                        if duration_str.is_empty() {
                            String::new()
                        } else {
                            format!(", {duration_str}")
                        },
                        run.id,
                    );
                }
                Ok(None) => println!("{}", "Last backup: no runs recorded".dimmed()),
                Err(e) => log::warn!("Failed to query last run: {e}"),
            },
            Err(_) => println!("{}", "Last backup: state database not available".dimmed()),
        }
    } else {
        println!("{}", "Last backup: no runs recorded".dimmed());
    }

    // ── Pin file summary ───────────────────────────────────────────────

    let total_pins: usize = config
        .subvolumes
        .iter()
        .map(|sv| {
            config
                .local_snapshot_dir(&sv.name)
                .map(|dir| chain::find_pinned_snapshots(&dir, &drive_labels).len())
                .unwrap_or(0)
        })
        .sum();

    if total_pins > 0 {
        println!(
            "Pinned snapshots: {} across {} subvolumes",
            total_pins,
            config.subvolumes.len()
        );
    }

    Ok(())
}

// ── ChainHealth ─────────────────────────────────────────────────────

/// Chain health status for a subvolume/drive pair, ordered worst-to-best.
/// `min()` across drives yields the worst health.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChainHealth {
    NoDriveData,
    Full(String),
    Incremental(String),
}

impl ChainHealth {
    fn severity(&self) -> u8 {
        match self {
            Self::NoDriveData => 0,
            Self::Full(_) => 1,
            Self::Incremental(_) => 2,
        }
    }
}

impl Ord for ChainHealth {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.severity().cmp(&other.severity())
    }
}

impl PartialOrd for ChainHealth {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for ChainHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDriveData => write!(f, "none"),
            Self::Full(reason) => write!(f, "full ({reason})"),
            Self::Incremental(pin) => write!(f, "incremental ({pin})"),
        }
    }
}

fn chain_health(
    local_dir: &std::path::Path,
    drive_label: &str,
    ext_count: usize,
    ext_dir: &std::path::Path,
) -> ChainHealth {
    if ext_count == 0 {
        return ChainHealth::NoDriveData;
    }

    match chain::read_pin_file(local_dir, drive_label) {
        Ok(Some(pin)) => {
            let local_exists = local_dir.join(pin.as_str()).exists();
            if !local_exists {
                return ChainHealth::Full("pin missing locally".to_string());
            }
            let ext_exists = ext_dir.join(pin.as_str()).exists();
            if !ext_exists {
                return ChainHealth::Full("pin missing on drive".to_string());
            }
            ChainHealth::Incremental(pin.to_string())
        }
        Ok(None) => ChainHealth::Full("no pin".to_string()),
        Err(_) => ChainHealth::Full("pin error".to_string()),
    }
}

fn print_table(headers: &[String], rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("{}", "No subvolumes configured.".dimmed());
        return;
    }

    // Calculate column widths
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    // Print header
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    println!("{}", header_line.join("  ").bold());

    // Print rows
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(cell.len());
                format!("{:<width$}", cell, width = w)
            })
            .collect();
        println!("{}", line.join("  "));
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_health_ordering() {
        let none = ChainHealth::NoDriveData;
        let full = ChainHealth::Full("no pin".to_string());
        let inc = ChainHealth::Incremental("20260322-1430-opptak".to_string());

        assert!(none < full);
        assert!(full < inc);
        assert!(none < inc);
    }

    #[test]
    fn chain_health_min_finds_worst() {
        let inc = ChainHealth::Incremental("snap".to_string());
        let full = ChainHealth::Full("no pin".to_string());
        let none = ChainHealth::NoDriveData;

        assert_eq!(inc.clone().min(full.clone()), full);
        assert_eq!(full.min(none.clone()), none);
        assert_eq!(inc.min(none.clone()), none);
    }

    #[test]
    fn chain_health_display() {
        assert_eq!(ChainHealth::NoDriveData.to_string(), "none");
        assert_eq!(
            ChainHealth::Full("no pin".to_string()).to_string(),
            "full (no pin)"
        );
        assert_eq!(
            ChainHealth::Incremental("20260322-snap".to_string()).to_string(),
            "incremental (20260322-snap)"
        );
    }

    #[test]
    fn chain_health_min_two_fulls_keeps_first() {
        let full_a = ChainHealth::Full("pin missing locally".to_string());
        let full_b = ChainHealth::Full("pin missing on drive".to_string());
        // Both are Full — min returns self (first), which is fine
        let result = full_a.clone().min(full_b);
        assert!(matches!(result, ChainHealth::Full(_)));
    }
}

