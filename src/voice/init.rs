//! `urd init` renderer. Reports infrastructure, sources, snapshot roots,
//! drive status, pin files, incomplete snapshots, snapshot counts, and
//! preflight warnings in interactive mode; serializes as JSON in daemon
//! mode.

use std::fmt::Write;
use std::path::Path;

use colored::Colorize;

use crate::output::{InitOutput, InitStatus, OutputMode};
use crate::types::{ByteSize, DriveRole};

/// First-run guidance when `urd init` finds no config. The bare-`urd`
/// greeting points new users here, so a missing config is the expected
/// starting state — greet and guide, never error.
#[must_use]
pub fn render_init_first_time(config_path: &Path, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => format!(
            "Urd is not configured yet — nothing to verify.\n\
             \n\
             To begin, create a config at {}.\n\
             Start from the annotated example (config/urd.toml.example in the\n\
             Urd repository, walked through in the README's Configuration\n\
             section), then run `urd init` again to check your setup.\n",
            config_path.display()
        ),
        OutputMode::Daemon => r#"{"status":"not_configured"}"#.to_string(),
    }
}

/// Render init output according to the given mode.
#[must_use]
pub fn render_init(data: &InitOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_init_interactive(data),
        OutputMode::Daemon => render_init_daemon(data),
    }
}

fn render_init_daemon(data: &InitOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_init_interactive(data: &InitOutput) -> String {
    let mut out = String::new();

    writeln!(out, "{}", "Urd initialization".bold()).ok();
    writeln!(out).ok();

    // ── Infrastructure checks ──────────────────────────────────────
    for check in &data.infrastructure {
        let status = format_init_status(check.status);
        if let Some(ref detail) = check.detail {
            writeln!(out, "{status} {}: {detail}", check.name).ok();
        } else {
            writeln!(out, "{status} {}", check.name).ok();
        }
    }

    // ── Subvolume sources ──────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Checking subvolume sources:".bold()).ok();
    for check in &data.subvolume_sources {
        let status = format_init_status(check.status);
        if let Some(ref detail) = check.detail {
            writeln!(out, "  {status} {}: {detail}", check.name).ok();
        } else {
            writeln!(out, "  {status} {}", check.name).ok();
        }
    }

    // ── Snapshot roots ─────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Checking snapshot roots:".bold()).ok();
    for check in &data.snapshot_roots {
        let status = format_init_status(check.status);
        writeln!(out, "  {status} {}", check.name).ok();
    }

    // ── Drives ─────────────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Drive status:".bold()).ok();
    for drive in &data.drives {
        let status = if drive.mounted {
            "CONNECTED".green().to_string()
        } else if drive.role == DriveRole::Offsite {
            "AWAY".yellow().to_string()
        } else {
            "DISCONNECTED".yellow().to_string()
        };
        let free_info = drive
            .free_bytes
            .map(|b| format!(" ({} free)", ByteSize(b)))
            .unwrap_or_default();
        writeln!(
            out,
            "  {status} {} [{}] at {}{free_info}",
            drive.label.bold(),
            drive.role,
            drive.mount_path,
        )
        .ok();
    }

    // ── Pin files ──────────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Pin file status:".bold()).ok();
    for pin in &data.pin_files {
        match pin.status {
            InitStatus::Ok => {
                writeln!(
                    out,
                    "  {} {}/{}: {}",
                    "OK".green(),
                    pin.subvolume,
                    pin.drive,
                    pin.snapshot_name.as_deref().unwrap_or("—"),
                )
                .ok();
            }
            InitStatus::Warn => {
                writeln!(
                    out,
                    "  {} {}/{}: no pin file",
                    "—".dimmed(),
                    pin.subvolume,
                    pin.drive,
                )
                .ok();
            }
            InitStatus::Error => {
                writeln!(
                    out,
                    "  {} {}/{}: {}",
                    "ERROR".red(),
                    pin.subvolume,
                    pin.drive,
                    pin.error.as_deref().unwrap_or("unknown error"),
                )
                .ok();
            }
        }
    }

    // ── Incomplete snapshots ───────────────────────────────────────
    if !data.incomplete_snapshots.is_empty() {
        writeln!(out).ok();
        writeln!(
            out,
            "{}",
            "Potentially incomplete snapshots on external drives:".bold()
        )
        .ok();
        for inc in &data.incomplete_snapshots {
            writeln!(
                out,
                "  {} {} on {} (not pinned, may be from interrupted transfer)",
                "WARNING".yellow(),
                inc.snapshot,
                inc.drive,
            )
            .ok();
        }
    }

    // ── Snapshot counts ────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Snapshot counts:".bold()).ok();
    for sc in &data.snapshot_counts {
        let ext_display = if sc.external_counts.is_empty() {
            "no drives mounted".to_string()
        } else {
            sc.external_counts
                .iter()
                .map(|(label, count)| format!("{label}:{count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(
            out,
            "  {} — local: {}, external: [{ext_display}]",
            sc.subvolume.bold(),
            sc.local_count,
        )
        .ok();
    }

    // ── Preflight warnings ─────────────────────────────────────────
    if !data.preflight_warnings.is_empty() {
        writeln!(out).ok();
        writeln!(out, "{}", "Config consistency checks:".bold()).ok();
        for warning in &data.preflight_warnings {
            writeln!(out, "  {} {warning}", "WARN".yellow()).ok();
        }
    }

    writeln!(out).ok();
    writeln!(out, "{}", "Initialization complete.".green().bold()).ok();

    out
}

fn format_init_status(status: InitStatus) -> String {
    match status {
        InitStatus::Ok => "OK".green().to_string(),
        InitStatus::Warn => "WARN".yellow().to_string(),
        InitStatus::Error => "ERROR".red().to_string(),
    }
}
