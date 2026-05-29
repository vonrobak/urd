//! Storage-critical predicate (ADR-113 Layer 1, UPI 044 stub → UPI 031 rule).
//!
//! UPI 044 shipped this as a stub returning `false`. UPI 031 retires the stub
//! and replaces it with a **pure structural rule**: a subvolume is
//! storage-critical when its source lives on the host's root-filesystem pool
//! (UUID match), an *enabled* subvolume entrusts `/` itself to Urd, *and* that
//! pool is genuinely tight right now (free-ratio ≥ Pressure).
//!
//! This is distinct from momentary headroom pressure: headroom answers "is this
//! pool tight now?"; `storage_critical` adds the structural dimension — pressure
//! here threatens the *host*, not just retention. Only the intersection
//! (structurally fragile **and** tight) is critical.
//!
//! Purity (ADR-108): this module takes resolved inputs and performs no I/O. The
//! doctor wiring (`src/commands/doctor.rs`) resolves the one I/O-bound input
//! (`root_pool_uuid`, one `findmnt /`) at the boundary and feeds it in; the
//! per-subvolume pool UUID, the `root_subvol_configured` gate, and the
//! free-ratio severity are all computed there from already-available data.
//!
//! The behavioral half (ephemeral lifecycle, conservative intervals) is
//! deliberately *not* shipped here — per ADR-113's increment-2 sequencing it
//! belongs with UPIs 032/033 where the safety scaffolding lands.

use crate::recommendation::HeadroomSeverity;

/// Resolved inputs to the storage-critical structural rule (UPI 031). All
/// fields are pre-resolved by the doctor wiring; the rule itself is pure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StorageCriticalInput<'a> {
    /// The BTRFS pool UUID hosting this subvolume's source, if resolvable.
    pub subvol_pool_uuid: Option<&'a str>,
    /// The BTRFS pool UUID hosting `/`, if resolvable.
    pub root_pool_uuid: Option<&'a str>,
    /// True when an **enabled** configured subvolume has `source == "/"`,
    /// i.e. the user has actively entrusted the root filesystem to Urd.
    pub root_subvol_configured: bool,
    /// Tightness of the source pool by free-ratio only (Branch E): reuses
    /// `recommendation::classify_free_ratio`, no trend/metadata, no new
    /// constant. `Pressure` (or the dormant `Critical`) means genuinely tight.
    pub source_free_ratio_severity: HeadroomSeverity,
}

/// True iff the subvolume is storage-critical under the structural rule.
///
/// ```text
/// storage_critical =
///       subvol_pool_uuid == root_pool_uuid     // on the pool hosting "/"
///   AND root_subvol_configured                  // an ENABLED subvol has source == "/"
///   AND source_free_ratio_severity >= Pressure  // genuinely tight NOW (<15% free)
/// ```
///
/// Fails toward *not*-critical for every unmeasurable input (unresolved pool
/// UUID, unmeasurable free-ratio → `Healthy`) — this is an advisory surface.
#[must_use]
pub fn is_storage_critical(input: StorageCriticalInput<'_>) -> bool {
    let on_root_pool = matches!(
        (input.subvol_pool_uuid, input.root_pool_uuid),
        (Some(a), Some(b)) if a == b
    );
    on_root_pool
        && input.root_subvol_configured
        && input.source_free_ratio_severity >= HeadroomSeverity::Pressure
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an input with the structural gate satisfied; callers override
    /// the field under test.
    fn on_root(severity: HeadroomSeverity) -> StorageCriticalInput<'static> {
        StorageCriticalInput {
            subvol_pool_uuid: Some("root-pool"),
            root_pool_uuid: Some("root-pool"),
            root_subvol_configured: true,
            source_free_ratio_severity: severity,
        }
    }

    #[test]
    fn root_pool_match_configured_and_pressure_is_critical() {
        assert!(is_storage_critical(on_root(HeadroomSeverity::Pressure)));
    }

    #[test]
    fn htpc_root_archetype_is_critical() {
        // Source `/` on a tight root NVMe (~6% free → Pressure), `/` entrusted.
        let input = StorageCriticalInput {
            subvol_pool_uuid: Some("nvme-root"),
            root_pool_uuid: Some("nvme-root"),
            root_subvol_configured: true,
            source_free_ratio_severity: HeadroomSeverity::Pressure,
        };
        assert!(is_storage_critical(input));
    }

    #[test]
    fn caution_is_not_critical() {
        // Branch E: only Pressure+ qualifies; Caution is below the gate.
        assert!(!is_storage_critical(on_root(HeadroomSeverity::Caution)));
    }

    #[test]
    fn healthy_is_not_critical() {
        assert!(!is_storage_critical(on_root(HeadroomSeverity::Healthy)));
    }

    #[test]
    fn critical_severity_is_critical() {
        // Defensive: `>=` accepts the dormant Critical tier.
        assert!(is_storage_critical(on_root(HeadroomSeverity::Critical)));
    }

    #[test]
    fn not_configured_is_not_critical() {
        // Gate: no enabled `source == "/"` subvolume entrusts `/`.
        let mut input = on_root(HeadroomSeverity::Pressure);
        input.root_subvol_configured = false;
        assert!(!is_storage_critical(input));
    }

    #[test]
    fn uuid_mismatch_is_not_critical() {
        // Tight + configured, but this subvol is on a different pool.
        let input = StorageCriticalInput {
            subvol_pool_uuid: Some("other-pool"),
            root_pool_uuid: Some("root-pool"),
            root_subvol_configured: true,
            source_free_ratio_severity: HeadroomSeverity::Pressure,
        };
        assert!(!is_storage_critical(input));
    }

    #[test]
    fn subvol_pool_uuid_none_is_not_critical() {
        let mut input = on_root(HeadroomSeverity::Pressure);
        input.subvol_pool_uuid = None;
        assert!(!is_storage_critical(input));
    }

    #[test]
    fn root_pool_uuid_none_is_not_critical() {
        // Root not btrfs / unresolved findmnt → no structural claim.
        let mut input = on_root(HeadroomSeverity::Pressure);
        input.root_pool_uuid = None;
        assert!(!is_storage_critical(input));
    }

    #[test]
    fn both_uuids_none_is_not_critical() {
        let input = StorageCriticalInput {
            subvol_pool_uuid: None,
            root_pool_uuid: None,
            root_subvol_configured: true,
            source_free_ratio_severity: HeadroomSeverity::Pressure,
        };
        assert!(!is_storage_critical(input));
    }
}
