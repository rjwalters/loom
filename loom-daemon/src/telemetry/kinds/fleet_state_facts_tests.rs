//! The closed vocabularies of `repos[].halt.reason` and
//! `rows[].eligible_reason` (Issue #11564).

use chrono::TimeZone;

use super::*;

/// The halt reasons are exactly the `HaltCause` tokens plus `unknown`, and
/// the serde form is the wire name.
#[test]
fn halt_reason_vocabulary_is_the_halt_causes_plus_unknown() {
    let tokens: Vec<&str> = FleetHaltReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "main_red",
            "gate_pending",
            "token_pool",
            "preflight_advisory",
            "drain",
            "breaker",
            "write_scope",
            "ci_billing",
            "install_incompatible",
            "daemon_too_old",
            "maintain_only",
            "disk_reservation",
            "unknown",
        ]
    );
    for reason in FleetHaltReason::ALL {
        assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
    }
    // Every cause maps onto its own token; no cause lands on `unknown`.
    for cause in HaltCause::ALL {
        let reason = FleetHaltReason::from_cause(Some(cause));
        assert_eq!(reason.as_str(), cause.as_str());
        assert_ne!(reason, FleetHaltReason::Unknown);
    }
    assert_eq!(FleetHaltReason::from_cause(None), FleetHaltReason::Unknown);
    // A newer emitter's token decodes as `unknown`, never a guess.
    let read: FleetHaltReason = serde_json::from_value(serde_json::json!("solar_flare")).unwrap();
    assert_eq!(read, FleetHaltReason::Unknown);
}

/// The eligible reasons are the seven clocks, each reached from exactly one
/// disposition; every other disposition carries none.
#[test]
fn eligible_reason_vocabulary_is_closed_and_each_has_one_disposition() {
    let tokens: Vec<&str> = FleetEligibleReason::ALL
        .iter()
        .map(|r| r.as_str())
        .collect();
    assert_eq!(
        tokens,
        [
            "recheck_interval",
            "dispatch_backoff",
            "open_pr_backoff",
            "noop_cooldown",
            "declined",
            "prless_retry",
            "build_backoff",
        ]
    );
    for reason in FleetEligibleReason::ALL {
        assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
    }
    let every = QueueDisposition::ALL
        .iter()
        .copied()
        .chain([QueueDisposition::LabelledBlocked, QueueDisposition::Unknown]);
    let mut reached = Vec::new();
    for disposition in every {
        if let Some(reason) = FleetEligibleReason::from_disposition(disposition) {
            reached.push(reason);
            // The token is the disposition's own, but for the build back-off.
            let expected = match disposition {
                QueueDisposition::DeferredBuildBackoff => "build_backoff",
                other => other.as_str(),
            };
            assert_eq!(reason.as_str(), expected);
        }
    }
    reached.sort();
    assert_eq!(reached, FleetEligibleReason::ALL.to_vec());
    // Only the build back-off has no clock.
    let clockless: Vec<_> = FleetEligibleReason::ALL
        .into_iter()
        .filter(|r| !r.has_clock())
        .collect();
    assert_eq!(clockless, [FleetEligibleReason::BuildBackoff]);
}

/// The new fields are additive: absent when unset, so an older reader and an
/// older record are unchanged.
#[test]
fn the_new_fields_are_absent_when_unset_and_round_trip_when_set() {
    let at = Utc.with_ymd_and_hms(2026, 10, 9, 3, 5, 0).unwrap();
    let row = FleetStateRow::new(7, FleetStage::ReadyWait, at);
    let wire = serde_json::to_value(&row).unwrap();
    assert!(wire.get("eligible_at").is_none() && wire.get("eligible_reason").is_none());

    let row = FleetStateRow {
        eligible_at: Some(at),
        eligible_reason: Some(FleetEligibleReason::NoopCooldown),
        ..row
    };
    let wire = serde_json::to_value(&row).unwrap();
    assert_eq!(wire["eligible_at"], "2026-10-09T03:05:00Z");
    assert_eq!(wire["eligible_reason"], "noop_cooldown");
    assert_eq!(serde_json::from_value::<FleetStateRow>(wire).unwrap(), row);

    let halt = FleetRepoHalt {
        reason: FleetHaltReason::InstallIncompatible,
        since: at,
        since_lower_bound: false,
    };
    let wire = serde_json::to_value(&halt).unwrap();
    assert!(wire.get("since_lower_bound").is_none(), "exact since omits the flag");
    assert_eq!(serde_json::from_value::<FleetRepoHalt>(wire).unwrap(), halt);
}
