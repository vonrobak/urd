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

use serde::{Deserialize, Serialize};

use crate::awareness::{PromiseChange, PromiseRollup, PromiseSnapshot, promise_changes};
use crate::heartbeat::{Heartbeat, SubvolumeHeartbeat};
use crate::storage_critical::{TightnessTier, Transition};

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
    /// Pin file write(s) failed after successful send — next send will be
    /// full instead of incremental (potentially hours instead of seconds).
    PinWriteFailures {
        total_failures: u32,
    },
    /// Heartbeat is stale — no backup completed within expected window.
    /// Evaluated by the Sentinel (5b), not by `urd backup` itself.
    #[allow(dead_code)] // Constructed by Sentinel (5b), not backup command
    BackupOverdue {
        last_heartbeat_age_hours: u64,
        stale_after_hours: u64,
    },
    /// A subvolume's operational health worsened (e.g., Healthy -> Degraded).
    /// Separate from PromiseDegraded — health is operational readiness, not data safety.
    #[allow(dead_code)] // Constructed by Sentinel (VFM-B), not backup command
    HealthDegraded {
        subvolume: String,
        from: String,
        to: String,
    },
    /// A subvolume's operational health improved.
    #[allow(dead_code)] // Constructed by Sentinel (VFM-B), not backup command
    HealthRecovered {
        subvolume: String,
        from: String,
        to: String,
    },
    /// Multiple incremental chains on a drive broke simultaneously.
    /// Strong signal for drive swap or mass pin file loss.
    DriveAnomalyDetected {
        drive_label: String,
        total_chains: usize,
        broken_count: usize,
    },
    /// A drive transitioned from absent to connected.
    DriveReconnected {
        drive_label: String,
        absent_duration: Option<String>,
    },
    /// A drive is mounted but needs identity verification before sends proceed.
    DriveNeedsAdoption {
        drive_label: String,
    },
    /// Emergency retention ran before a backup to recover critical space.
    /// Dispatched by the backup command's emergency pre-flight path.
    /// `freed_bytes` is `None` when the post-delete free-space probe failed.
    EmergencyRetentionRan {
        root: String,
        freed_bytes: Option<u64>,
        deleted_count: usize,
    },
    /// A source pool's tightness tier escalated (UPI 031-a). Dispatched
    /// best-effort by `urd backup` only — the told-not-silent "just noticed"
    /// surface (D6). `from`/`to` are tier labels (`roomy`/`tight`/`critical`).
    StoragePressureRising {
        pool_label: String,
        from: String,
        to: String,
        host_root: bool,
    },
    /// The mid-op watchdog aborted an in-flight send to protect the host
    /// (UPI 033, ADR-113 Layer 2). Dispatched best-effort by `urd backup` after
    /// the abort-reclaim. `snapshots_reclaimed` distinguishes the
    /// reclaimed-prose path from the could-not-reclaim degrade (S4).
    WatchdogAborted {
        pool_label: String,
        snapshots_reclaimed: u32,
    },
    /// The always-on sentinel shed Urd-owned local snapshots while idle to keep a
    /// source pool above the host-survival floor (UPI 034, ADR-113 Layer 3).
    /// Dispatched best-effort by the sentinel only when at least one snapshot was
    /// actually reclaimed.
    EmergencyEjected {
        pool_label: String,
        snapshots_reclaimed: u32,
        free_bytes_before: u64,
        floor_bytes: u64,
    },
    /// Urd released an away/offsite drive's incremental chain under Critical
    /// pressure (UPI 064-b). The data is safe offsite; only the chain breaks, so
    /// the next return is a full re-send. `Urgency::Warning` (not Critical —
    /// Critical stays reserved for host-survival actions).
    OffsiteChainReleased {
        subvolume: String,
        drive: String,
    },
}

// ── Urgency ────────────────────────────────────────────────────────────

/// Urgency determines which channels fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    fn snapshots(subvolumes: &[SubvolumeHeartbeat]) -> Vec<PromiseSnapshot> {
        subvolumes
            .iter()
            .map(|sv| PromiseSnapshot {
                name: sv.name.clone(),
                status: sv.promise_status,
            })
            .collect()
    }

    // ── Promise state transitions + all-unprotected ────────────────
    // First-run semantics: `previous: None` yields no transitions, but
    // all-unprotected is still evaluated on `current` — a system born
    // broken must say so.
    let current_snaps = snapshots(&current.subvolumes);
    let changes = match previous {
        Some(prev) => promise_changes(&snapshots(&prev.subvolumes), &current_snaps),
        None => Vec::new(),
    };
    let all_unprotected =
        PromiseRollup::from_pairs(current_snaps.iter().map(|s| (s.name.clone(), s.status)))
            .all_unprotected();

    let mut notifications = build_promise_change_notifications(&changes, all_unprotected);

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
                "The spindle has stopped — every thread snapped. Check the logs.".to_string()
            } else {
                format!(
                    "{failed_count} of {total_count} threads could not be spun. \
                     The others hold, but the pattern is incomplete."
                )
            },
        });
    }

    // ── Pin file write failures ─────────────────────────────────────
    let total_pin_failures: u32 = current.subvolumes.iter().map(|sv| sv.pin_failures).sum();
    if total_pin_failures > 0 {
        notifications.push(Notification {
            event: NotificationEvent::PinWriteFailures {
                total_failures: total_pin_failures,
            },
            urgency: Urgency::Warning,
            title: format!("Urd: {total_pin_failures} pin file write(s) failed"),
            body: format!(
                "{total_pin_failures} send(s) succeeded but their chain markers could not be \
                 written. The next send will be full instead of incremental. \
                 Run `urd verify` to diagnose."
            ),
        });
    }

    notifications
}

/// Render promise-change notifications — THE single home of the
/// degraded/recovered/all-unprotected prose (UPI 088-a, arc R1).
///
/// Both notification paths feed it: `compute_notifications` (heartbeat
/// diff, backup path) and `sentinel_runner::build_notifications`
/// (assessment diff, daemon path). First-run suppression is deliberately
/// the caller's: pass the changes you want spoken and the
/// all-unprotected verdict you computed — the two callers suppress
/// differently and must keep doing so.
#[must_use]
pub fn build_promise_change_notifications(
    changes: &[PromiseChange],
    all_unprotected: bool,
) -> Vec<Notification> {
    let mut notifications = Vec::new();

    for change in changes {
        if change.to.worsened_from(change.from) {
            notifications.push(Notification {
                event: NotificationEvent::PromiseDegraded {
                    subvolume: change.name.clone(),
                    from: change.from.to_string(),
                    to: change.to.to_string(),
                },
                urgency: Urgency::Warning,
                title: format!("Urd: {} is now {}", change.name, change.to),
                body: format!(
                    "The thread of {} has frayed — it was {}, now {}. \
                     The well remembers, but the thread grows thin.",
                    change.name, change.from, change.to
                ),
            });
        } else {
            notifications.push(Notification {
                event: NotificationEvent::PromiseRecovered {
                    subvolume: change.name.clone(),
                    from: change.from.to_string(),
                    to: change.to.to_string(),
                },
                urgency: Urgency::Info,
                title: format!("Urd: {} restored to {}", change.name, change.to),
                body: format!(
                    "The thread of {} is mended — restored from {} to {}.",
                    change.name, change.from, change.to
                ),
            });
        }
    }

    if all_unprotected {
        notifications.push(Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "Urd: all promises broken".to_string(),
            body: "Every thread in the well has snapped. No subvolume is protected. \
                   Attend to this — your data stands exposed."
                .to_string(),
        });
    }

    notifications
}

// ── Drive reconnection notifications ──────────────────────────────────

/// Build a notification for a drive that transitioned from absent to connected.
#[must_use]
pub fn build_drive_reconnected_notification(
    label: &str,
    absent_duration: Option<&str>,
) -> Notification {
    let body = match absent_duration {
        Some(duration) => format!(
            "Absent {duration}. Run `urd backup` to restore full protection."
        ),
        None => "Run `urd backup` to restore full protection.".to_string(),
    };

    Notification {
        event: NotificationEvent::DriveReconnected {
            drive_label: label.to_string(),
            absent_duration: absent_duration.map(|s| s.to_string()),
        },
        urgency: Urgency::Info,
        title: format!("{label} is back"),
        body,
    }
}

// ── Storage-pressure notifications (UPI 031-a) ────────────────────────

/// Build a best-effort notification for a source pool whose tightness tier
/// escalated (D6). Urgency scales with severity: `Critical` tier or a
/// host-root pool fires at `Critical`; a plain `Tight` escalation fires at
/// `Warning`. The host-root case carries the relocated 031 stakes prose.
#[must_use]
pub fn build_storage_pressure_notification(
    pool_label: &str,
    transition: Transition,
    host_root: bool,
) -> Notification {
    let to_label = tier_word(transition.to);
    let urgency = if transition.to == TightnessTier::Critical || host_root {
        Urgency::Critical
    } else {
        Urgency::Warning
    };

    let mut body = format!(
        "{pool_label} is now {to_label} on free space \
         ({} → {to_label}). Consider freeing space or tightening retention.",
        tier_word(transition.from),
    );
    if host_root {
        body.push_str(
            " This is your host root, so pressure here risks the machine itself.",
        );
    }

    Notification {
        event: NotificationEvent::StoragePressureRising {
            pool_label: pool_label.to_string(),
            from: transition.from.as_db_str().to_string(),
            to: transition.to.as_db_str().to_string(),
            host_root,
        },
        urgency,
        title: format!("Storage running {to_label}: {pool_label}"),
        body,
    }
}

/// Build a notification for a mid-op watchdog firing (UPI 033, pool-scoped by UPI
/// 065-b). `send_aborted` selects the register: `true` when the in-flight send
/// read the *same* filesystem and was stopped (UPI 033 behaviour); `false` when
/// it read a *different, independent* filesystem and was left running while this
/// pool was relieved concurrently (UPI 065-b — no backup was interrupted). Prose
/// is further aligned to what was **actually** reclaimed: when snapshots were
/// freed, reassure that the offsite copy is safe and the next send will be full;
/// when nothing could be reclaimed (wedged receive / no reserve — S4), ask the
/// user to check free space. `Critical` urgency for both: a host-survival action
/// is a serious, user-visible event.
#[must_use]
pub fn build_watchdog_abort_notification(
    pool_label: &str,
    snapshots_reclaimed: u32,
    send_aborted: bool,
) -> Notification {
    let (title, body) = if send_aborted {
        let body = if snapshots_reclaimed > 0 {
            format!(
                "Stopped this backup — {pool_label} got tight, so I freed Urd's own \
                 snapshots to protect it. The previous offsite copy is still safe; \
                 the next backup will be a full one."
            )
        } else {
            format!(
                "Stopped this backup — {pool_label} got tight. I couldn't fully reclaim \
                 space this run; please check free space on the source drive."
            )
        };
        (format!("Backup stopped to protect {pool_label}"), body)
    } else {
        // Cross-filesystem: the running backup read a different, independent pool,
        // so it kept going — only this pool was relieved.
        let body = if snapshots_reclaimed > 0 {
            format!(
                "{pool_label} got tight, so I freed Urd's own snapshots to protect it. \
                 The backup that was running reads a different drive and kept going \
                 untouched; the previous offsite copy here is still safe and the next \
                 backup of this pool will be a full one."
            )
        } else {
            format!(
                "{pool_label} got tight. I couldn't fully reclaim space this run; the \
                 running backup reads a different drive and was left untouched. Please \
                 check free space on this source drive."
            )
        };
        (format!("Freed space to protect {pool_label}"), body)
    };

    Notification {
        event: NotificationEvent::WatchdogAborted {
            pool_label: pool_label.to_string(),
            snapshots_reclaimed,
        },
        urgency: Urgency::Critical,
        title,
        body,
    }
}

/// Build the notification for an idle emergency eject (UPI 034, ADR-113 Layer 3).
/// The sentinel sheds Urd-owned local snapshots while no backup runs to keep a
/// source pool above the host-survival floor. Dispatched only when at least one
/// snapshot was reclaimed (`snapshots_reclaimed > 0`). Body uses the sever
/// register and mirrors `build_watchdog_abort_notification`'s careful "still
/// safe" claim — it does not say "nothing is lost" (the local restore points are
/// gone until the next, full, send).
#[must_use]
pub fn build_emergency_eject_notification(
    pool_label: &str,
    snapshots_reclaimed: u32,
    free_bytes_before: u64,
    floor_bytes: u64,
) -> Notification {
    Notification {
        event: NotificationEvent::EmergencyEjected {
            pool_label: pool_label.to_string(),
            snapshots_reclaimed,
            free_bytes_before,
            floor_bytes,
        },
        urgency: Urgency::Critical,
        title: format!("{pool_label} nearly full — Urd freed space"),
        body: format!(
            "{pool_label} is nearly full — Urd severed {snapshots_reclaimed} local thread(s) to \
             protect it. The offsite copy of each is still safe; the next backup of these will be \
             a full send."
        ),
    }
}

/// Build the told-not-silent notification for an offsite chain released under
/// Critical pressure (UPI 064-b). `Urgency::Warning`: the data is **safe
/// offsite** (a pin proves a completed copy) — only the incremental chain
/// breaks, so the next return is a full re-send. Critical urgency stays reserved
/// for host-survival actions (`WatchdogAbort`/`EmergencyEject`). The `parent` is
/// taken for prose completeness only (not stored on the event).
#[must_use]
pub fn build_offsite_chain_released_notification(
    subvolume: &str,
    drive: &str,
    parent: &str,
) -> Notification {
    Notification {
        event: NotificationEvent::OffsiteChainReleased {
            subvolume: subvolume.to_string(),
            drive: drive.to_string(),
        },
        urgency: Urgency::Warning,
        title: format!("Offsite chain released: {subvolume}"),
        body: format!(
            "Storage pressure forced Urd to release {subvolume}'s offsite chain to {drive} \
             (was {parent}). The data remains safe offsite; only the incremental link is gone, so \
             the next backup to {drive} on its return will be a full re-send."
        ),
    }
}

/// User-facing word for a tightness tier in notification prose.
fn tier_word(tier: TightnessTier) -> &'static str {
    match tier {
        TightnessTier::Roomy => "roomy",
        TightnessTier::Tight => "tight",
        TightnessTier::Critical => "critically tight",
    }
}

/// Build a notification for a drive that needs identity verification.
#[must_use]
pub fn build_drive_needs_adoption_notification(label: &str) -> Notification {
    Notification {
        event: NotificationEvent::DriveNeedsAdoption {
            drive_label: label.to_string(),
        },
        urgency: Urgency::Warning,
        title: format!("{label} needs identity verification"),
        body: format!(
            "Drive is mounted but its identity token is missing or mismatched. \
             Run `urd drives adopt {label}` to accept this drive."
        ),
    }
}

/// Build a notification for emergency retention that ran before a backup.
/// `freed_bytes` is `None` when the post-delete free-space probe failed —
/// the body then reports only the deletion count, never a made-up size.
#[must_use]
pub fn build_emergency_retention_notification(
    root: &str,
    freed_bytes: Option<u64>,
    deleted_count: usize,
) -> Notification {
    let body = match freed_bytes {
        Some(bytes) => format!(
            "Freed {} from {root} by deleting {deleted_count} snapshots before backup.",
            crate::types::ByteSize(bytes)
        ),
        None => format!(
            "Deleted {deleted_count} snapshots from {root} to recover critical space before backup."
        ),
    };
    Notification {
        event: NotificationEvent::EmergencyRetentionRan {
            root: root.to_string(),
            freed_bytes,
            deleted_count,
        },
        urgency: Urgency::Warning,
        title: "Emergency retention ran".to_string(),
        body,
    }
}

// ── Dispatch (I/O) ─────────────────────────────────────────────────────

/// Send notifications through configured channels.
///
/// Filters by `min_urgency` — only notifications at or above the threshold
/// are dispatched. Errors are logged but never propagated (notifications
/// must not prevent backups).
///
/// Returns `true` if at least one notification was successfully delivered
/// through at least one channel, `false` if all channels failed or there
/// were no eligible notifications.
pub fn dispatch(notifications: &[Notification], config: &NotificationConfig) -> bool {
    if !config.enabled || config.channels.is_empty() {
        return false;
    }

    let eligible: Vec<&Notification> = notifications
        .iter()
        .filter(|n| n.urgency >= config.min_urgency)
        .collect();

    if eligible.is_empty() {
        return false;
    }

    let mut any_succeeded = false;

    for notification in &eligible {
        for channel in &config.channels {
            let ok = match channel {
                NotificationChannel::Desktop => dispatch_desktop(notification),
                NotificationChannel::Webhook { url, template } => {
                    dispatch_webhook(notification, url, template.as_deref())
                }
                NotificationChannel::Command { path, args } => {
                    dispatch_command(notification, path, args)
                }
                NotificationChannel::Log => {
                    dispatch_log(notification);
                    true // Log channel always succeeds
                }
            };
            if ok {
                any_succeeded = true;
            }
        }
    }

    any_succeeded
}

fn urgency_to_notify_send(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Info => "normal",
        Urgency::Warning => "normal",
        Urgency::Critical => "critical",
    }
}

fn dispatch_desktop(notification: &Notification) -> bool {
    let result = Command::new("notify-send")
        .arg("--urgency")
        .arg(urgency_to_notify_send(notification.urgency))
        .arg("--app-name")
        .arg("Urd")
        .arg(&notification.title)
        .arg(&notification.body)
        .output();

    match result {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            log::warn!(
                "notify-send failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
            false
        }
        Err(e) => {
            log::warn!("Failed to run notify-send: {e}");
            false
        }
    }
}

fn dispatch_webhook(notification: &Notification, url: &str, template: Option<&str>) -> bool {
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
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            log::warn!(
                "Webhook POST to {} failed (exit {}): {}",
                url,
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
            false
        }
        Err(e) => {
            log::warn!("Failed to run curl for webhook: {e}");
            false
        }
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

fn dispatch_command(notification: &Notification, path: &PathBuf, args: &[String]) -> bool {
    let result = Command::new(path)
        .args(args)
        .env("URD_NOTIFICATION_TITLE", &notification.title)
        .env("URD_NOTIFICATION_BODY", &notification.body)
        .env("URD_NOTIFICATION_URGENCY", notification.urgency.to_string())
        .output();

    match result {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            log::warn!(
                "Notification command {:?} failed (exit {}): {}",
                path,
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
            false
        }
        Err(e) => {
            log::warn!("Failed to run notification command {:?}: {e}", path);
            false
        }
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
    use crate::awareness::PromiseStatus;
    use crate::heartbeat::{Heartbeat, SubvolumeHeartbeat};

    fn make_heartbeat(statuses: &[(&str, PromiseStatus, Option<bool>)]) -> Heartbeat {
        make_heartbeat_with_pins(statuses, 0)
    }

    fn make_heartbeat_with_pins(
        statuses: &[(&str, PromiseStatus, Option<bool>)],
        pin_failures: u32,
    ) -> Heartbeat {
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
                    promise_status: *status,
                    backup_success: *success,
                    pin_failures,
                    send_completed: true,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    pool_uuid: None,
                    local_snapshot_count: None,
                    estimated_local_pinned_delta_bytes: None,
                })
                .collect(),
            pools: vec![],
            drives: vec![],
        }
    }

    // ── Watchdog abort notification (UPI 033) ──────────────────────

    #[test]
    fn watchdog_abort_reclaimed_prose() {
        let n = build_watchdog_abort_notification("/data", 3, true);
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(n.title.contains("/data"));
        assert!(n.title.contains("stopped"), "same-fs title says the backup was stopped");
        assert!(n.body.contains("Stopped this backup"));
        assert!(n.body.contains("offsite copy is still safe"));
        assert!(matches!(
            n.event,
            NotificationEvent::WatchdogAborted { snapshots_reclaimed: 3, .. }
        ));
    }

    #[test]
    fn watchdog_abort_zero_reclaim_prose() {
        let n = build_watchdog_abort_notification("/", 0, true);
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(n.body.contains("couldn't fully reclaim"));
        assert!(matches!(
            n.event,
            NotificationEvent::WatchdogAborted { snapshots_reclaimed: 0, .. }
        ));
    }

    #[test]
    fn watchdog_cross_fs_prose_says_backup_kept_running() {
        // UPI 065-b: a cross-filesystem firing did NOT stop a backup — the running
        // send read a different, independent pool. Prose must not say "stopped"
        // and must reassure the other backup kept going.
        let n = build_watchdog_abort_notification("/home", 5, false);
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(!n.title.contains("stopped"), "cross-fs did not stop a backup");
        assert!(!n.body.contains("Stopped this backup"));
        assert!(n.body.contains("kept going"));
        assert!(n.body.contains("still safe"));
    }

    #[test]
    fn watchdog_cross_fs_zero_reclaim_asks_to_check_space() {
        let n = build_watchdog_abort_notification("/home", 0, false);
        assert!(n.body.contains("couldn't fully reclaim"));
        assert!(n.body.contains("left untouched"));
    }

    // ── Emergency eject notification (UPI 034) ─────────────────────

    #[test]
    fn emergency_eject_notification_sever_prose() {
        let n = build_emergency_eject_notification("/data", 2, 3_800_000_000, 4_000_000_000);
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(n.title.contains("/data"));
        assert!(n.body.contains("severed 2 local thread(s)"));
        // Honesty (S1): mirrors the watchdog's "still safe", not "nothing is lost".
        assert!(n.body.contains("still safe"));
        assert!(!n.body.contains("nothing is lost"));
        assert!(matches!(
            n.event,
            NotificationEvent::EmergencyEjected {
                snapshots_reclaimed: 2,
                free_bytes_before: 3_800_000_000,
                floor_bytes: 4_000_000_000,
                ..
            }
        ));
    }

    // ── Offsite chain released notification (UPI 064-b) ────────────

    #[test]
    fn offsite_chain_released_notification_is_warning_and_reassures() {
        let n = build_offsite_chain_released_notification(
            "subvol3-opptak",
            "WD-18TB1",
            "20260514-1000-opptak",
        );
        // Warning, NOT Critical — the data is safe offsite, only the chain breaks.
        assert_eq!(n.urgency, Urgency::Warning);
        assert!(n.title.contains("subvol3-opptak"));
        assert!(n.body.contains("WD-18TB1"));
        assert!(n.body.contains("safe offsite"));
        assert!(n.body.contains("full re-send"));
        assert!(matches!(
            n.event,
            NotificationEvent::OffsiteChainReleased { .. }
        ));
    }

    // ── Promise state transitions ──────────────────────────────────

    #[test]
    fn degraded_generates_notification() {
        let prev = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);

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
        let prev = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);

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
        let prev = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);

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
        let curr = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);

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
            ("home", PromiseStatus::Unprotected, Some(true)),
            ("docs", PromiseStatus::Unprotected, Some(true)),
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
            ("home", PromiseStatus::Unprotected, Some(true)),
            ("docs", PromiseStatus::Protected, Some(true)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let all_unprotected: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert!(all_unprotected.is_empty());
    }

    // ── Golden prose (UPI 088-a, arc R8) ───────────────────────────
    // Byte-exact fixtures for every promise-transition sentence. Before
    // 088-a nothing pinned these bodies, and sentinel_runner's twin
    // builder emits the identical prose independently — these goldens
    // are the acceptance criterion for collapsing both onto one shared
    // core. Change a string here only as a deliberate voice decision.

    #[test]
    fn golden_degraded_prose_exact() {
        let prev = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let degraded: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseDegraded { .. }))
            .collect();
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].urgency, Urgency::Warning);
        assert_eq!(degraded[0].title, "Urd: home is now AT RISK");
        assert_eq!(
            degraded[0].body,
            "The thread of home has frayed — it was PROTECTED, now AT RISK. \
             The well remembers, but the thread grows thin."
        );
    }

    #[test]
    fn golden_recovered_prose_exact() {
        let prev = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let recovered: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseRecovered { .. }))
            .collect();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].urgency, Urgency::Info);
        assert_eq!(recovered[0].title, "Urd: home restored to PROTECTED");
        assert_eq!(
            recovered[0].body,
            "The thread of home is mended — restored from AT RISK to PROTECTED."
        );
    }

    #[test]
    fn golden_all_unprotected_prose_exact() {
        let curr = make_heartbeat(&[
            ("home", PromiseStatus::Unprotected, Some(true)),
            ("docs", PromiseStatus::Unprotected, Some(true)),
        ]);

        let notifications = compute_notifications(None, &curr);

        let all_unprotected: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert_eq!(all_unprotected.len(), 1);
        assert_eq!(all_unprotected[0].urgency, Urgency::Critical);
        assert_eq!(all_unprotected[0].title, "Urd: all promises broken");
        assert_eq!(
            all_unprotected[0].body,
            "Every thread in the well has snapped. No subvolume is protected. \
             Attend to this — your data stands exposed."
        );
    }

    #[test]
    fn golden_first_run_all_unprotected_still_fires() {
        // First run (`previous: None`): transitions are suppressed, but
        // all-unprotected is still evaluated on `current` — a system that
        // is born broken must say so.
        let curr = make_heartbeat(&[("home", PromiseStatus::Unprotected, Some(true))]);

        let notifications = compute_notifications(None, &curr);

        assert!(notifications.iter().all(|n| !matches!(
            n.event,
            NotificationEvent::PromiseDegraded { .. }
                | NotificationEvent::PromiseRecovered { .. }
        )));
        let all_unprotected: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert_eq!(all_unprotected.len(), 1);
        assert_eq!(all_unprotected[0].title, "Urd: all promises broken");
    }

    #[test]
    fn golden_mixed_transitions_prose_exact() {
        // One degradation and one recovery in the same diff: both bodies
        // exact, each carrying its own from/to pair.
        let prev = make_heartbeat(&[
            ("home", PromiseStatus::Protected, Some(true)),
            ("docs", PromiseStatus::AtRisk, Some(true)),
        ]);
        let curr = make_heartbeat(&[
            ("home", PromiseStatus::AtRisk, Some(true)),
            ("docs", PromiseStatus::Protected, Some(true)),
        ]);

        let notifications = compute_notifications(Some(&prev), &curr);

        let degraded: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseDegraded { .. }))
            .collect();
        let recovered: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseRecovered { .. }))
            .collect();
        assert_eq!(degraded.len(), 1);
        assert_eq!(
            degraded[0].body,
            "The thread of home has frayed — it was PROTECTED, now AT RISK. \
             The well remembers, but the thread grows thin."
        );
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].body,
            "The thread of docs is mended — restored from AT RISK to PROTECTED."
        );
    }

    // ── Backup failures ────────────────────────────────────────────

    #[test]
    fn partial_failures_generate_warning() {
        let curr = make_heartbeat(&[
            ("home", PromiseStatus::Protected, Some(true)),
            ("docs", PromiseStatus::AtRisk, Some(false)),
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
            ("home", PromiseStatus::AtRisk, Some(false)),
            ("docs", PromiseStatus::AtRisk, Some(false)),
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
            ("home", PromiseStatus::Protected, Some(true)),
            ("docs", PromiseStatus::Protected, Some(true)),
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
        let curr = make_heartbeat(&[("home", PromiseStatus::Protected, None)]);

        let notifications = compute_notifications(None, &curr);

        let failures: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::BackupFailures { .. }))
            .collect();
        assert!(failures.is_empty());
    }

    // ── Pin failure notifications ──────────────────────────────────

    #[test]
    fn pin_failures_generate_warning() {
        let curr = make_heartbeat_with_pins(
            &[("home", PromiseStatus::Protected, Some(true))],
            2,
        );

        let notifications = compute_notifications(None, &curr);

        let pin_events: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PinWriteFailures { .. }))
            .collect();
        assert_eq!(pin_events.len(), 1);
        assert_eq!(pin_events[0].urgency, Urgency::Warning);
        assert!(pin_events[0].title.contains("2 pin file write(s) failed"));
    }

    #[test]
    fn no_pin_failures_no_notification() {
        let curr = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);

        let notifications = compute_notifications(None, &curr);

        let pin_events: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PinWriteFailures { .. }))
            .collect();
        assert!(pin_events.is_empty());
    }

    // ── Urgency ordering ───────────────────────────────────────────

    #[test]
    fn urgency_ordering() {
        assert!(Urgency::Info < Urgency::Warning);
        assert!(Urgency::Warning < Urgency::Critical);
        assert!(Urgency::Info < Urgency::Critical);
    }

    // ── Degradation direction ──────────────────────────────────────

    // ── Multiple events in one transition ──────────────────────────

    #[test]
    fn multiple_degradations_produce_multiple_notifications() {
        let prev = make_heartbeat(&[
            ("home", PromiseStatus::Protected, Some(true)),
            ("docs", PromiseStatus::Protected, Some(true)),
        ]);
        let curr = make_heartbeat(&[
            ("home", PromiseStatus::AtRisk, Some(true)),
            ("docs", PromiseStatus::Unprotected, Some(false)),
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

        let prev = make_heartbeat(&[("home", PromiseStatus::Protected, Some(true))]);
        let curr = make_heartbeat(&[("home", PromiseStatus::AtRisk, Some(true))]);

        let notifications = compute_notifications(Some(&prev), &curr);
        // Warning-level notification should not fire through Critical-minimum config
        let eligible: Vec<_> = notifications
            .iter()
            .filter(|n| n.urgency >= config.min_urgency)
            .collect();
        assert!(eligible.is_empty());
    }

    #[test]
    fn dispatch_disabled_returns_false() {
        let config = NotificationConfig {
            enabled: false,
            min_urgency: Urgency::Info,
            channels: vec![NotificationChannel::Log],
        };
        let result = dispatch(&[Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "test".to_string(),
            body: "test".to_string(),
        }], &config);
        assert!(!result, "disabled config should return false");
    }

    #[test]
    fn dispatch_log_channel_returns_true() {
        let config = NotificationConfig {
            enabled: true,
            min_urgency: Urgency::Info,
            channels: vec![NotificationChannel::Log],
        };
        let result = dispatch(&[Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "test".to_string(),
            body: "test".to_string(),
        }], &config);
        assert!(result, "log channel should always succeed");
    }

    #[test]
    fn dispatch_no_eligible_returns_false() {
        let config = NotificationConfig {
            enabled: true,
            min_urgency: Urgency::Critical,
            channels: vec![NotificationChannel::Log],
        };
        // Warning urgency < Critical minimum — no eligible notifications
        let result = dispatch(&[Notification {
            event: NotificationEvent::PromiseDegraded {
                subvolume: "test".to_string(),
                from: "PROTECTED".to_string(),
                to: "AT RISK".to_string(),
            },
            urgency: Urgency::Warning,
            title: "test".to_string(),
            body: "test".to_string(),
        }], &config);
        assert!(!result, "no eligible notifications should return false");
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

    // ── Drive reconnection notifications ──────────────────────────────

    #[test]
    fn drive_reconnected_with_duration() {
        let n = build_drive_reconnected_notification("WD-18TB", Some("10 days"));
        assert_eq!(n.title, "WD-18TB is back");
        assert!(n.body.contains("Absent 10 days"));
        assert!(n.body.contains("urd backup"));
        assert_eq!(n.urgency, Urgency::Info);
        match &n.event {
            NotificationEvent::DriveReconnected { drive_label, absent_duration } => {
                assert_eq!(drive_label, "WD-18TB");
                assert_eq!(absent_duration.as_deref(), Some("10 days"));
            }
            other => panic!("expected DriveReconnected, got {other:?}"),
        }
    }

    #[test]
    fn drive_reconnected_without_duration() {
        let n = build_drive_reconnected_notification("2TB-backup", None);
        assert_eq!(n.title, "2TB-backup is back");
        assert!(!n.body.contains("Absent"));
        assert!(n.body.contains("urd backup"));
    }

    #[test]
    fn drive_reconnected_urgency_is_info() {
        let n = build_drive_reconnected_notification("D1", Some("3 hours"));
        assert_eq!(n.urgency, Urgency::Info);
    }

    #[test]
    fn drive_needs_adoption_notification() {
        let n = build_drive_needs_adoption_notification("WD-18TB");
        assert!(n.title.contains("WD-18TB"));
        assert!(n.title.contains("identity verification"));
        assert!(n.body.contains("urd drives adopt WD-18TB"));
        assert_eq!(n.urgency, Urgency::Warning);
    }

    #[test]
    fn drive_needs_adoption_urgency_is_warning() {
        let n = build_drive_needs_adoption_notification("D1");
        assert_eq!(n.urgency, Urgency::Warning);
    }

    #[test]
    fn emergency_retention_notification_format() {
        let n = build_emergency_retention_notification("/snap/home", Some(8_200_000_000), 39);
        assert_eq!(n.urgency, Urgency::Warning);
        assert_eq!(n.title, "Emergency retention ran");
        assert!(n.body.contains("Freed"), "body: {}", n.body);
        assert!(n.body.contains("39 snapshots"), "body: {}", n.body);
        assert!(n.body.contains("/snap/home"), "body: {}", n.body);
        assert!(
            matches!(
                n.event,
                NotificationEvent::EmergencyRetentionRan {
                    deleted_count: 39,
                    ..
                }
            ),
            "event: {:?}",
            n.event
        );
    }

    #[test]
    fn emergency_retention_notification_unknown_freed_reports_count_only() {
        let n = build_emergency_retention_notification("/snap/home", None, 3);
        assert!(
            !n.body.contains("Freed"),
            "must not claim a freed size when the probe failed: {}",
            n.body
        );
        assert!(n.body.contains("3 snapshots"), "body: {}", n.body);
    }

    // ── UPI 031-a: storage-pressure notifications ───────────────────

    fn trans(from: TightnessTier, to: TightnessTier) -> Transition {
        Transition { from, to }
    }

    #[test]
    fn storage_pressure_tight_is_warning() {
        let n = build_storage_pressure_notification(
            "/data",
            trans(TightnessTier::Roomy, TightnessTier::Tight),
            false,
        );
        assert_eq!(n.urgency, Urgency::Warning);
        assert!(n.title.contains("/data"), "title: {}", n.title);
        assert!(n.body.contains("tight"), "body: {}", n.body);
        assert!(
            !n.body.contains("host root"),
            "non-host-root body must not mention host root: {}",
            n.body
        );
        assert!(matches!(
            n.event,
            NotificationEvent::StoragePressureRising {
                from,
                to,
                host_root: false,
                ..
            } if from == "roomy" && to == "tight"
        ));
    }

    #[test]
    fn storage_pressure_critical_is_critical_urgency() {
        let n = build_storage_pressure_notification(
            "/data",
            trans(TightnessTier::Tight, TightnessTier::Critical),
            false,
        );
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(n.body.contains("critically tight"), "body: {}", n.body);
    }

    #[test]
    fn storage_pressure_host_root_escalates_urgency_and_prose() {
        // A mere Tight escalation, but on the host root → Critical + stakes prose.
        let n = build_storage_pressure_notification(
            "/",
            trans(TightnessTier::Roomy, TightnessTier::Tight),
            true,
        );
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(
            n.body.contains("host root") && n.body.contains("machine itself"),
            "host-root body must carry the stakes prose: {}",
            n.body
        );
        assert!(matches!(
            n.event,
            NotificationEvent::StoragePressureRising { host_root: true, .. }
        ));
    }
}
