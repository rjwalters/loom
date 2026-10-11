//! The Z.ai free-seat line under the dynamic-cap breakdown on
//! `loom-daemon status` (#11284) — a sibling module because
//! `status_render.rs` is a frozen over-threshold ledger entry.
//!
//! The cap's formula line above this names `max(configured max, zai free
//! seats)` as its third term; this line is the observation behind it, printed
//! from the daemon's report (never re-probed client-side) so what the
//! operator reads and what dispatch admitted against are the same numbers.
//! Occupancy is deliberately restated here — `reserved` counts the in-flight
//! lease holds on the counted accounts, which is the seat-side counterpart of
//! the sweep counts printed above the cap block.
//!
//! Rendering rules:
//!
//! - `None` (a pre-#11284 daemon) prints nothing — no invented zeros.
//! - A pool with no selectable capped accounts says so explicitly, so "the
//!   floor is inactive" is distinguishable from "the field is missing".
//! - Only counts are ever printed; account names and credentials never enter
//!   the report this renders (`ZaiSeats` is count-only by construction).

use loom_daemon::types::{DaemonStatusReport, ZaiSeats};

/// The seat line, or `None` when the daemon did not send the observation.
#[must_use]
pub fn render(report: &DaemonStatusReport) -> Option<String> {
    report.zai_seats.as_ref().map(|s| render_seats(s, report))
}

fn render_seats(s: &ZaiSeats, report: &DaemonStatusReport) -> String {
    let floor_note = if s.free > report.configured_max {
        format!(
            " — raising the cap term above the configured {} (#11284)",
            report.configured_max
        )
    } else {
        String::new()
    };
    if s.accounts == 0 {
        return format!(
            "  zai seats: 0 free (no selectable capped zai accounts; the seat floor is \
             inactive){floor_note}"
        );
    }
    format!(
        "  zai seats: {} free of {} across {} selectable account(s), {} held by running \
         launches{floor_note}",
        s.free, s.capacity, s.accounts, s.reserved
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn report_with(seats: Option<ZaiSeats>, configured: usize) -> DaemonStatusReport {
        DaemonStatusReport {
            zai_seats: seats,
            configured_max: configured,
            ..crate::cli::status::sample_report::sample_report()
        }
    }

    #[test]
    fn a_missing_observation_from_an_old_daemon_prints_nothing() {
        assert!(render(&report_with(None, 4)).is_none());
    }

    #[test]
    fn a_live_pool_prints_free_capacity_and_occupancy() {
        let seats = ZaiSeats {
            free: 4,
            accounts: 2,
            capacity: 6,
            reserved: 2,
        };
        let line = render(&report_with(Some(seats), 2)).unwrap();
        assert!(line.contains("4 free of 6"), "{line}");
        assert!(line.contains("2 selectable account(s)"), "{line}");
        assert!(line.contains("2 held by running launches"), "{line}");
        assert!(line.contains("raising the cap term above the configured 2"), "{line}");
    }

    #[test]
    fn a_floor_below_the_configured_cap_does_not_claim_to_raise_it() {
        let seats = ZaiSeats {
            free: 1,
            accounts: 1,
            capacity: 3,
            reserved: 2,
        };
        let line = render(&report_with(Some(seats), 5)).unwrap();
        assert!(line.contains("1 free of 3"), "{line}");
        assert!(!line.contains("raising the cap term"), "{line}");
    }

    #[test]
    fn an_empty_pool_is_distinguished_from_a_missing_one() {
        let line = render(&report_with(Some(ZaiSeats::default()), 4)).unwrap();
        assert!(line.contains("0 free"), "{line}");
        assert!(line.contains("no selectable capped zai accounts"), "{line}");
    }
}
