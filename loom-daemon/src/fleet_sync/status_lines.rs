//! The floor and run-state lines of `loom-daemon status`'s `Fleet store:`
//! block, split out of `fleet_sync.rs` (at its file-size threshold) when
//! #11740 added the host-identity line there.

use super::{floor, Enforcement, FleetSyncStatus, FloorPass};

/// The floor lines of the `Fleet store:` block (#10711). Empty when no floor
/// is set and none was malformed, so `status` is unchanged for such a host.
pub(super) fn floor_lines(f: &FloorPass) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(floor) = &f.floor {
        let from = match (&f.source, f.carried) {
            (_, true) => " (LAST GOOD floor, carried over)".to_string(),
            (Some(src), false) => format!(" (from {src})"),
            (None, false) => String::new(),
        };
        lines.push(format!("  {}: {floor}{from}", floor::KEY));
    }
    if let Some(e) = &f.error {
        lines.push(format!("  {}: ERROR — {e}", floor::KEY));
    }
    lines
}

/// The run-state lines of the `Fleet store:` block (#9598) — the *desired*
/// state the store named, and the *actual* enforcement this host applied.
///
/// Empty when the pass resolved no state at all (a store with no
/// `fleet/state.yml`, a first boot behind an unreachable forge), except that a
/// read *error* is always reported: "the state could not be read" is the one
/// thing an operator must not have to infer from silence.
pub(super) fn state_lines(s: &FleetSyncStatus) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(desired) = s.state.desired {
        let from = match s.state.source.as_deref() {
            Some("host") => "host entry",
            Some(_) => "fleet default",
            None => "unknown entry",
        };
        let how = if s.state.from_last_recorded {
            ", from this host's LAST RECORDED state — no snapshot was readable"
        } else if s.state.cached {
            ", from a CACHED snapshot"
        } else {
            ""
        };
        let actual = match s.enforced {
            Enforcement::Proceed => "dispatching normally",
            Enforcement::Hold => "new dispatch HELD, in-flight work finishes",
            Enforcement::Stop => "refusing to start / draining to exit",
        };
        lines.push(format!("  run state: desired {} ({from}{how}) -> {actual}", desired.as_str()));
        for (k, v) in [
            ("by", &s.state.by),
            ("since", &s.state.since),
            ("reason", &s.state.reason),
        ] {
            if let Some(v) = v {
                lines.push(format!("    {k}: {v}"));
            }
        }
    }
    if let Some(e) = &s.state.error {
        lines.push(format!("  run state: NOT ENFORCED — {e}"));
    }
    lines
}
