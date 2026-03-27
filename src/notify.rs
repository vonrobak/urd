// Notification dispatcher — evaluates promise state changes and dispatches
// notifications through configured channels.
//
// The core logic is a pure function: `compute_notifications()` takes the
// previous and current heartbeats and returns what to notify about.
// `dispatch()` is the I/O layer that sends them through configured channels.
//
// This is component 5a of the Sentinel design (see docs/95-ideas/
// 2026-03-26-design-sentinel.md). It runs at the end of `urd backup`,
// not as a daemon.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

use crate::heartbeat::Heartbeat;

// ── Event types ────────────────────────────────────────────────────────

/// What happened that might warrant a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// A subvolume's promise state worsened (e.g., PROTECTED -> AT RISK).
    PromiseDegraded {
        subvolume: String,
        from: String,
        to: String,
    },
    /// A subvolume's promise state improved.
    PromiseRecovered {
        subvolume: String,
        from: String,
        to: String,
    },
    /// Backup run had failures.
    BackupFailures {
        failed_count: usize,
        total_count: usize,
    },
    /// All promises are now UNPROTECTED (critical).
    AllUnprotected,
    /// Heartbeat is stale — no backup completed within expected window.
    /// Evaluated by the Sentinel (5b), not by `urd backup` itself.
    #[allow(dead_code)] // Constructed by Sentinel (5b), not backup command
    BackupOverdue {
        last_heartbeat_age_hours: u64,
        stale_after_hours: u64,
    },
}

// ── Urgency ────────────────────────────────────────────────────────────

/// Urgency determines which channels fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Info,
    Warning,
    Critical,
}

impl std::fmt::Display for Urgency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => write!(f, "info"),
            Self::Warning => write!(f, "warning"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

// ── Notification ───────────────────────────────────────────────────────

/// A notification ready to be dispatched.
#[derive(Debug, Clone)]
pub struct Notification {
    #[allow(dead_code)] // Used in tests for pattern matching; will be used by Sentinel (5b)
    pub event: NotificationEvent,
    pub urgency: Urgency,
    pub title: String,
    pub body: String,
}

// ── Channels ───────────────────────────────────────────────────────────

/// How to deliver a notification.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NotificationChannel {
    /// Desktop notification via notify-send.
    Desktop,
    /// Webhook POST (Slack, Discord, Ntfy, generic).
    Webhook {
        url: String,
        #[serde(default)]
        template: Option<String>,
    },
    /// Command execution (arbitrary script).
    Command {
        path: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Write to log (always enabled, no config needed).
    Log,
}

// ── Config ─────────────────────────────────────────────────────────────

/// Notification configuration from `[notifications]` in urd.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_urgency")]
    pub min_urgency: Urgency,
    #[serde(default)]
    pub channels: Vec<NotificationChannel>,
}

fn default_true() -> bool {
    true
}

fn default_urgency() -> Urgency {
    Urgency::Warning
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_urgency: Urgency::Warning,
            channels: vec![],
        }
    }
}

// ── Pure computation ───────────────────────────────────────────────────

/// Compute notifications from a heartbeat state transition.
///
/// Pure function: no I/O. Takes before/after heartbeat state, returns
/// what to notify about. When `previous` is `None` (first run), no
/// degradation notifications fire — there's nothing to compare against.
#[must_use]
pub fn compute_notifications(
    previous: Option<&Heartbeat>,
    current: &Heartbeat,
) -> Vec<Notification> {
    let mut notifications = Vec::new();

    // ── Promise state transitions ──────────────────────────────────
    if let Some(prev) = previous {
        for current_sv in &current.subvolumes {
            if let Some(prev_sv) = prev
                .subvolumes
                .iter()
                .find(|s| s.name == current_sv.name)
                .filter(|prev_sv| prev_sv.promise_status != current_sv.promise_status)
            {
                if is_degradation(&prev_sv.promise_status, &current_sv.promise_status) {
                    notifications.push(Notification {
                        event: NotificationEvent::PromiseDegraded {
                            subvolume: current_sv.name.clone(),
                            from: prev_sv.promise_status.clone(),
                            to: current_sv.promise_status.clone(),
                        },
                        urgency: Urgency::Warning,
                        title: format!(
                            "Urd: {} is now {}",
                            current_sv.name, current_sv.promise_status
                        ),
                        body: format!(
                            "The thread of {} has frayed — it was {}, now {}. \
                             The well remembers, but the weave grows thin.",
                            current_sv.name, prev_sv.promise_status, current_sv.promise_status
                        ),
                    });
                } else {
                    notifications.push(Notification {
                        event: NotificationEvent::PromiseRecovered {
                            subvolume: current_sv.name.clone(),
                            from: prev_sv.promise_status.clone(),
                            to: current_sv.promise_status.clone(),
                        },
                        urgency: Urgency::Info,
                        title: format!(
                            "Urd: {} restored to {}",
                            current_sv.name, current_sv.promise_status
                        ),
                        body: format!(
                            "The thread of {} is rewoven — restored from {} to {}.",
                            current_sv.name, prev_sv.promise_status, current_sv.promise_status
                        ),
                    });
                }
            }
        }
    }

    // ── All unprotected ────────────────────────────────────────────
    let all_unprotected = !current.subvolumes.is_empty()
        && current
            .subvolumes
            .iter()
            .all(|sv| sv.promise_status == "UNPROTECTED");

    if all_unprotected {
        notifications.push(Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "Urd: all promises broken".to_string(),
            body: "Every thread in the well has snapped. No subvolume is protected. \
                   Attend to this — your data stands unguarded."
                .to_string(),
        });
    }

    // ── Backup failures ────────────────────────────────────────────
    let failed_count = current
        .subvolumes
        .iter()
        .filter(|sv| sv.backup_success == Some(false))
        .count();
    let total_count = current
        .subvolumes
        .iter()
        .filter(|sv| sv.backup_success.is_some())
        .count();

    if failed_count > 0 {
        let urgency = if failed_count == total_count {
            Urgency::Critical
        } else {
            Urgency::Warning
        };

        notifications.push(Notification {
            event: NotificationEvent::BackupFailures {
                failed_count,
                total_count,
            },
            urgency,
            title: format!("Urd: {failed_count}/{total_count} backups failed"),
            body: if failed_count == total_count {
                "The loom has seized — every weaving failed. Check the logs.".to_string()
            } else {
                format!(
                    "{failed_count} of {total_count} threads could not be woven. \
                     The others hold, but the pattern is incomplete."
                )
            },
        });
    }

    notifications
}

/// Status ordering for degradation detection: PROTECTED > AT RISK > UNPROTECTED.
fn status_rank(status: &str) -> u8 {
    match status {
        "PROTECTED" => 2,
        "AT RISK" => 1,
        "UNPROTECTED" => 0,
        _ => 0,
    }
}

fn is_degradation(from: &str, to: &str) -> bool {
    status_rank(from) > status_rank(to)
}

// ── Dispatch (I/O) ─────────────────────────────────────────────────────

/// Send notifications through configured channels.
///
/// Filters by `min_urgency` — only notifications at or above the threshold
/// are dispatched. Errors are logged but never propagated (notifications
/// must not prevent backups).
pub fn dispatch(notifications: &[Notification], config: &NotificationConfig) {
    if !config.enabled || config.channels.is_empty() {
        return;
    }

    let eligible: Vec<&Notification> = notifications
        .iter()
        .filter(|n| n.urgency >= config.min_urgency)
        .collect();

    if eligible.is_empty() {
        return;
    }

    for notification in &eligible {
        for channel in &config.channels {
            match channel {
                NotificationChannel::Desktop => {
                    dispatch_desktop(notification);
                }
                NotificationChannel::Webhook { url, template } => {
                    dispatch_webhook(notification, url, template.as_deref());
                }
                NotificationChannel::Command { path, args } => {
                    dispatch_command(notification, path, args);
                }
                NotificationChannel::Log => {
                    dispatch_log(notification);
                }
            }
        }
    }
}

fn urgency_to_notify_send(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Info => "normal",
        Urgency::Warning => "normal",
        Urgency::Critical => "critical",
    }
}

fn dispatch_desktop(notification: &Notification) {
    let result = Command::new("notify-send")
        .arg("--urgency")
        .arg(urgency_to_notify_send(notification.urgency))
        .arg("--app-name")
        .arg("Urd")
        .arg(&notification.title)
        .arg(&notification.body)
        .output();

    match result {
        Ok(output) if !output.status.success() => {
            log::warn!(
                "notify-send failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            log::warn!("Failed to run notify-send: {e}");
        }
        _ => {}
    }
}

fn dispatch_webhook(notification: &Notification, url: &str, template: Option<&str>) {
    let body = match template {
        Some(_tmpl) => {
            // Future: template substitution. For now, use default JSON.
            default_webhook_body(notification)
        }
        None => default_webhook_body(notification),
    };

    let result = Command::new("curl")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg("10")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&body)
        .arg(url)
        .output();

    match result {
        Ok(output) if !output.status.success() => {
            log::warn!(
                "Webhook POST to {} failed (exit {}): {}",
                url,
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            log::warn!("Failed to run curl for webhook: {e}");
        }
        _ => {}
    }
}

fn default_webhook_body(notification: &Notification) -> String {
    // Simple JSON payload compatible with most webhook receivers
    format!(
        r#"{{"title":"{}","body":"{}","urgency":"{}"}}"#,
        notification.title.replace('"', "\\\""),
        notification.body.replace('"', "\\\""),
        notification.urgency,
    )
}

fn dispatch_command(notification: &Notification, path: &PathBuf, args: &[String]) {
    let result = Command::new(path)
        .args(args)
        .env("URD_NOTIFICATION_TITLE", &notification.title)
        .env("URD_NOTIFICATION_BODY", &notification.body)
        .env("URD_NOTIFICATION_URGENCY", notification.urgency.to_string())
        .output();

    match result {
        Ok(output) if !output.status.success() => {
            log::warn!(
                "Notification command {:?} failed (exit {}): {}",
                path,
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            log::warn!("Failed to run notification command {:?}: {e}", path);
        }
        _ => {}
    }
}

fn dispatch_log(notification: &Notification) {
    match notification.urgency {
        Urgency::Critical => log::error!("[notification] {}: {}", notification.title, notification.body),
        Urgency::Warning => log::warn!("[notification] {}: {}", notification.title, notification.body),
        Urgency::Info => log::info!("[notification] {}: {}", notification.title, notification.body),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::{Heartbeat, SubvolumeHeartbeat};

    fn make_heartbeat(statuses: &[(&str, &str, Option<bool>)]) -> Heartbeat {
        Heartbeat {
            schema_version: 1,
            timestamp: "2026-03-27T04:00:00".to_string(),
            stale_after: "2026-03-27T04:30:00".to_string(),
            run_result: "success".to_string(),
            run_id: Some(1),
            notifications_dispatched: false,
            subvolumes: statuses
                .iter()
                .map(|(name, status, success)| SubvolumeHeartbeat {
                    name: name.to_string(),
                    promise_status: status.to_string(),
                    backup_success: *success,
                })
                .collect(),
        }
    }

    // ── Promise state transitions ──────────────────────────────────

    #[test]
    fn degraded_generates_notification() {
        let prev = make_heartbeat(&[("home", "PROTECTED", Some(true))]);
        let curr = make_heartbeat(&[("home", "AT RISK", Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let degraded: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseDegraded { .. }))
            .collect();
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].urgency, Urgency::Warning);
        assert!(degraded[0].title.contains("AT RISK"));
    }

    #[test]
    fn recovered_generates_notification() {
        let prev = make_heartbeat(&[("home", "AT RISK", Some(true))]);
        let curr = make_heartbeat(&[("home", "PROTECTED", Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let recovered: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseRecovered { .. }))
            .collect();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].urgency, Urgency::Info);
        assert!(recovered[0].title.contains("restored"));
    }

    #[test]
    fn no_change_no_notification() {
        let prev = make_heartbeat(&[("home", "PROTECTED", Some(true))]);
        let curr = make_heartbeat(&[("home", "PROTECTED", Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let state_changes: Vec<_> = notifications
            .iter()
            .filter(|n| {
                matches!(
                    n.event,
                    NotificationEvent::PromiseDegraded { .. }
                        | NotificationEvent::PromiseRecovered { .. }
                )
            })
            .collect();
        assert!(state_changes.is_empty());
    }

    #[test]
    fn first_heartbeat_no_degradation() {
        let curr = make_heartbeat(&[("home", "AT RISK", Some(true))]);

        let notifications = compute_notifications(None, &curr);

        let state_changes: Vec<_> = notifications
            .iter()
            .filter(|n| {
                matches!(
                    n.event,
                    NotificationEvent::PromiseDegraded { .. }
                        | NotificationEvent::PromiseRecovered { .. }
                )
            })
            .collect();
        assert!(state_changes.is_empty());
    }

    // ── All unprotected ────────────────────────────────────────────

    #[test]
    fn all_unprotected_is_critical() {
        let curr = make_heartbeat(&[
            ("home", "UNPROTECTED", Some(true)),
            ("docs", "UNPROTECTED", Some(true)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let all_unprotected: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert_eq!(all_unprotected.len(), 1);
        assert_eq!(all_unprotected[0].urgency, Urgency::Critical);
    }

    #[test]
    fn partial_unprotected_not_all_unprotected() {
        let curr = make_heartbeat(&[
            ("home", "UNPROTECTED", Some(true)),
            ("docs", "PROTECTED", Some(true)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let all_unprotected: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert!(all_unprotected.is_empty());
    }

    // ── Backup failures ────────────────────────────────────────────

    #[test]
    fn partial_failures_generate_warning() {
        let curr = make_heartbeat(&[
            ("home", "PROTECTED", Some(true)),
            ("docs", "AT RISK", Some(false)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let failures: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::BackupFailures { .. }))
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].urgency, Urgency::Warning);
        assert!(failures[0].title.contains("1/2"));
    }

    #[test]
    fn all_failures_is_critical() {
        let curr = make_heartbeat(&[
            ("home", "AT RISK", Some(false)),
            ("docs", "AT RISK", Some(false)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let failures: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::BackupFailures { .. }))
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].urgency, Urgency::Critical);
    }

    #[test]
    fn no_failures_no_notification() {
        let curr = make_heartbeat(&[
            ("home", "PROTECTED", Some(true)),
            ("docs", "PROTECTED", Some(true)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let failures: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::BackupFailures { .. }))
            .collect();
        assert!(failures.is_empty());
    }

    #[test]
    fn empty_run_no_failure_notification() {
        // backup_success = None means not attempted (empty run)
        let curr = make_heartbeat(&[("home", "PROTECTED", None)]);

        let notifications = compute_notifications(None, &curr);

        let failures: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::BackupFailures { .. }))
            .collect();
        assert!(failures.is_empty());
    }

    // ── Urgency ordering ───────────────────────────────────────────

    #[test]
    fn urgency_ordering() {
        assert!(Urgency::Info < Urgency::Warning);
        assert!(Urgency::Warning < Urgency::Critical);
        assert!(Urgency::Info < Urgency::Critical);
    }

    // ── Status ranking ─────────────────────────────────────────────

    #[test]
    fn status_rank_ordering() {
        assert!(status_rank("PROTECTED") > status_rank("AT RISK"));
        assert!(status_rank("AT RISK") > status_rank("UNPROTECTED"));
    }

    // ── Multiple events in one transition ──────────────────────────

    #[test]
    fn multiple_degradations_produce_multiple_notifications() {
        let prev = make_heartbeat(&[
            ("home", "PROTECTED", Some(true)),
            ("docs", "PROTECTED", Some(true)),
        ]);
        let curr = make_heartbeat(&[
            ("home", "AT RISK", Some(true)),
            ("docs", "UNPROTECTED", Some(false)),
        ]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let degraded: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseDegraded { .. }))
            .collect();
        assert_eq!(degraded.len(), 2);
    }

    // ── Dispatch filtering ─────────────────────────────────────────

    #[test]
    fn dispatch_respects_min_urgency() {
        let config = NotificationConfig {
            enabled: true,
            min_urgency: Urgency::Critical,
            channels: vec![NotificationChannel::Log],
        };

        let prev = make_heartbeat(&[("home", "PROTECTED", Some(true))]);
        let curr = make_heartbeat(&[("home", "AT RISK", Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);
        // Warning-level notification should not fire through Critical-minimum config
        let eligible: Vec<_> = notifications
            .iter()
            .filter(|n| n.urgency >= config.min_urgency)
            .collect();
        assert!(eligible.is_empty());
    }

    #[test]
    fn dispatch_disabled_does_nothing() {
        let config = NotificationConfig {
            enabled: false,
            min_urgency: Urgency::Info,
            channels: vec![NotificationChannel::Log],
        };
        // Should not panic or error
        dispatch(&[Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "test".to_string(),
            body: "test".to_string(),
        }], &config);
    }

    // ── Config deserialization ──────────────────────────────────────

    #[test]
    fn notification_config_defaults() {
        let config: NotificationConfig = toml::from_str("").unwrap();
        assert!(config.enabled);
        assert_eq!(config.min_urgency, Urgency::Warning);
        assert!(config.channels.is_empty());
    }

    #[test]
    fn notification_config_with_channels() {
        let config: NotificationConfig = toml::from_str(r#"
            [[channels]]
            type = "desktop"

            [[channels]]
            type = "webhook"
            url = "https://ntfy.sh/test"

            [[channels]]
            type = "command"
            path = "/usr/local/bin/notify"
            args = ["--json"]

            [[channels]]
            type = "log"
        "#).unwrap();
        assert_eq!(config.channels.len(), 4);
    }

    // ── Webhook body ───────────────────────────────────────────────

    #[test]
    fn webhook_body_is_valid_json() {
        let notification = Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "Test \"title\"".to_string(),
            body: "Test body".to_string(),
        };
        let body = default_webhook_body(&notification);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["urgency"], "critical");
    }
}
