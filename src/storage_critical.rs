//! Storage-critical predicate (ADR-113 Layer 1 hook, UPI 044 → UPI 031).
//!
//! UPI 044 ships this as a stub returning `false`. UPI 031 replaces the
//! body with its chosen truth source. Doctor injects this as a closure
//! into `build_doctor_recommendation_view_inner` for testability.
//!
//! Signature-change note: if UPI 031 needs additional inputs (state_db,
//! sentinel state, event log, per-destination granularity, etc.), the
//! closure call site in `src/commands/doctor.rs` must be updated
//! simultaneously. Grep for `is_critical(` and `is_storage_critical` in
//! `src/commands/doctor.rs` to find all call sites; the closure injection
//! pattern means both the resolver wiring AND the test closures need
//! updating in lockstep.

#[must_use]
pub fn is_storage_critical(subvolume: &str) -> bool {
    let _ = subvolume;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_false_for_any_subvolume_name() {
        // Minimal regression guard so UPI 031's replacement is intentional.
        assert!(!is_storage_critical(""));
        assert!(!is_storage_critical("containers"));
        assert!(!is_storage_critical("htpc-root"));
    }
}
