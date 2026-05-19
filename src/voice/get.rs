//! `urd get` — renders the per-file restore metadata banner (stderr).
//!
//! Pure renderers, no shared helpers needed: daemon mode serializes the
//! output structure as JSON, interactive mode produces a one-line
//! "Retrieving from snapshot …" banner.

use crate::output::{GetOutput, OutputMode};
use crate::types::ByteSize;

/// Render get metadata according to the given mode (for stderr, not content).
#[must_use]
pub fn render_get(data: &GetOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_get_interactive(data),
        OutputMode::Daemon => render_get_daemon(data),
    }
}

fn render_get_daemon(data: &GetOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_get_interactive(data: &GetOutput) -> String {
    let size = ByteSize(data.file_size);
    format!(
        "Retrieving from snapshot {} ({}) — {}\n",
        data.snapshot, data.snapshot_date, size,
    )
}
