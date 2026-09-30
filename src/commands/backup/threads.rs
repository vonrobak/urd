//! Worker-thread teardown for the backup run's progress and watchdog threads
//! (issue #381): join without swallowing a panic, and drain the watchdog's
//! firing slot even when a panic poisoned it.

use std::sync::Mutex;

use crate::run_tail::WatchdogFiring;

/// Join a worker thread without swallowing a panic (issue #381). `join().ok()`
/// discarded the payload, so a dead progress or watchdog thread left no trace
/// anywhere — no log line, no summary line, no notification. This logs an
/// `error!` naming the thread and hands the panic message back so the caller
/// can carry it further; the watchdog does (its death means the run continued
/// without ADR-113's in-flight guard), the cosmetic progress display does not.
///
/// Returns `None` when the thread returned normally. The message is the panic
/// payload's own text — `panic!("literal")` yields a `&str`, a formatted
/// `panic!` yields a `String`; anything else (`panic_any`) has no printable
/// form, so the message says that rather than inventing one.
#[must_use]
pub(super) fn join_logged(handle: std::thread::JoinHandle<()>, thread_name: &str) -> Option<String> {
    match handle.join() {
        Ok(()) => None,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            log::error!("The {thread_name} thread panicked: {message}");
            Some(message)
        }
    }
}

/// Drain the watchdog's thread→main firing slot, recovering the stash even
/// when the mutex is **poisoned** (issue #381). A watchdog that dies while
/// holding this guard poisons it, and the former `.map(…).unwrap_or_default()`
/// then discarded exactly the record the panic made most urgent: the pool the
/// thread had just tripped, whose abort-reclaim, event, and notification all
/// hang off it. The recovery is the same one `progress::progress_display_loop` uses for
/// its context mutex — a `Vec<WatchdogFiring>` is a plain owned value that a
/// `push` either completed or did not, so a panic cannot leave it half-written
/// in a way that makes reading it unsound. Only the thread that held the lock
/// died; the data behind it is intact.
pub(super) fn take_firings(slot: &Mutex<Vec<WatchdogFiring>>) -> Vec<WatchdogFiring> {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::svname;
    use std::path::PathBuf;
    use std::sync::Arc;

    // ── Worker-thread teardown (issue #381) ────────────────────────────

    #[test]
    fn join_logged_returns_none_when_the_thread_returns_normally() {
        let h = std::thread::spawn(|| {});
        assert_eq!(join_logged(h, "progress display"), None);
    }

    #[test]
    fn join_logged_reports_a_literal_panic_payload() {
        // `panic!("literal")` hands back a `&str` payload.
        let h = std::thread::spawn(|| panic!("poll loop exploded"));
        assert_eq!(
            join_logged(h, "storage watchdog"),
            Some("poll loop exploded".to_string()),
        );
    }

    #[test]
    fn join_logged_reports_a_formatted_panic_payload() {
        // A formatted `panic!` hands back a `String` payload instead.
        let pool = "/data";
        let h = std::thread::spawn(move || panic!("watchdog lost {pool}"));
        assert_eq!(
            join_logged(h, "storage watchdog"),
            Some("watchdog lost /data".to_string()),
        );
    }

    #[test]
    fn take_firings_recovers_a_stash_from_a_poisoned_slot() {
        // The exact ordering this fix exists for: the watchdog stashes a
        // firing and then dies while still holding the guard. The old
        // `.unwrap_or_default()` dropped the stash on the floor — the abort
        // reclaim, the WatchdogAbort event, and the notification with it.
        let slot: Arc<Mutex<Vec<WatchdogFiring>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = slot.clone();
        let h = std::thread::spawn(move || {
            let mut stash = writer.lock().unwrap();
            stash.push(poisoning_firing());
            panic!("watchdog died holding the firing slot");
        });
        assert!(h.join().is_err(), "the writer must have panicked");
        assert!(slot.is_poisoned(), "a panic under the guard poisons the mutex");

        let taken = take_firings(&slot);

        assert_eq!(taken.len(), 1, "the stashed firing survives the poisoning");
        assert_eq!(taken[0].pool_label, "/data");
        assert!(
            take_firings(&slot).is_empty(),
            "the slot is drained, not copied"
        );
    }

    #[test]
    fn take_firings_drains_a_healthy_slot() {
        let slot: Mutex<Vec<WatchdogFiring>> = Mutex::new(vec![poisoning_firing()]);
        assert_eq!(take_firings(&slot).len(), 1);
        assert!(take_firings(&slot).is_empty(), "one drain only");
    }

    /// A minimal cross-filesystem firing with nothing stashed — enough to
    /// prove identity through a poisoned slot.
    fn poisoning_firing() -> WatchdogFiring {
        WatchdogFiring {
            pool_label: "/data".to_string(),
            subvol_names: vec![svname("home")],
            mountpoint: PathBuf::from("/data"),
            floor_bytes: 4_000_000_000,
            send_aborted: false,
            reclaim: None,
        }
    }

    #[test]
    fn join_logged_names_an_unprintable_panic_payload() {
        // `panic_any` with a non-string payload has no printable form — say
        // so rather than invent a message.
        let h = std::thread::spawn(|| std::panic::panic_any(7u8));
        assert_eq!(
            join_logged(h, "storage watchdog"),
            Some("non-string panic payload".to_string()),
        );
    }
}
