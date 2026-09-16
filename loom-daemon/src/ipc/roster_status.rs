//! The roster section of the daemon `status` report (Issue #7690 Phase A +
//! #7691 Phase B of #6704), split out of `build_daemon_status` (#7852).

use crate::role_shard::RosterMode;
use crate::types::RosterStatus;

/// Roster section (#7690, Phase A of #6704) — read ONLY from the
/// cache the heartbeat task populates (`role_shard::roster::roster_snapshot`),
/// never a live forge call: `status` must never itself add a forge
/// round-trip, and a disabled/never-yet-published roster leaves the
/// cache `None`, which renders nothing (byte-identical to pre-#7690
/// status when the roster is off).
pub(super) fn roster_status(roster: &RosterMode) -> Option<RosterStatus> {
    crate::role_shard::roster::roster_snapshot().map(|snap| {
        let view = crate::role_shard::roster::build_roster_status(
            &snap.issue,
            &snap.comments,
            &snap.host,
            chrono::Utc::now(),
            snap.ttl_secs,
        );
        crate::types::RosterStatus {
            issue: view.issue,
            live_count: view.live_count,
            seen_count: view.seen_count,
            generation: view.generation,
            settled_secs: view.settled_secs,
            // The admission fence's verdict for this host (#7691),
            // taken from the same `decide` call the header's posture
            // came from — so `status` never renders a fence state the
            // tick path would not have taken.
            fence: Some(match roster {
                crate::role_shard::RosterMode::Ring { generation } => {
                    format!("admitted — acting under the ring settled at generation {generation}")
                }
                crate::role_shard::RosterMode::Yield(reason) => {
                    format!("YIELDING role ticks — {}", reason.describe())
                }
                crate::role_shard::RosterMode::Off(off) => format!(
                    "not consulted for ownership ({}) — the static #6374 ring is in effect",
                    off.label()
                ),
            }),
            members: view
                .members
                .into_iter()
                .map(|m| crate::types::RosterMemberStatus {
                    host: m.host,
                    fresh: m.fresh,
                    last_beat_secs_ago: m.last_beat_secs_ago,
                    serves_count: m.serves_count,
                    is_this_host: m.is_this_host,
                })
                .collect(),
        }
    })
}
