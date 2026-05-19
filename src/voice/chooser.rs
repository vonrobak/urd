//! Guided chooser message for commands that require a subvolume argument.
//!
//! Small, self-contained guidance surface — no rendering helpers, no
//! dependency on the cross-renderer formatters in `voice/mod.rs`. Lives
//! in its own sub-module per UPI 050 phase 2 so the per-command voice
//! sub-module pattern stays complete.

use std::fmt::Write;

/// Format a subvolume chooser message for commands that require a subvolume argument.
/// Names are sorted alphabetically for easy scanning.
#[must_use]
pub fn format_subvolume_chooser(command: &str, names: &[&str]) -> String {
    let mut out = format!(
        "Usage: {command} <subvolume> or {command} --all\n\nAvailable subvolumes:\n"
    );
    let mut sorted = names.to_vec();
    sorted.sort();
    for name in &sorted {
        writeln!(out, "  {name}").ok();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subvolume_chooser_contains_usage_and_names() {
        let output = format_subvolume_chooser("urd retention-preview", &["docs", "pics", "home"]);
        assert!(output.contains("Usage: urd retention-preview <subvolume>"));
        assert!(output.contains("--all"));
        assert!(output.contains("Available subvolumes:"));
        // Should be sorted alphabetically
        let docs_pos = output.find("docs").unwrap();
        let home_pos = output.find("home").unwrap();
        let pics_pos = output.find("pics").unwrap();
        assert!(docs_pos < home_pos && home_pos < pics_pos, "names should be sorted");
    }

    #[test]
    fn subvolume_chooser_single_name() {
        let output = format_subvolume_chooser("urd history", &["only-one"]);
        assert!(output.contains("only-one"));
        assert!(output.contains("Usage: urd history"));
    }
}
