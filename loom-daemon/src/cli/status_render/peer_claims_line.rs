//! The peer-coordination verdict line under `loom-daemon status`'s
//! `Peer claims:` block (Issue #9294).
//!
//! # Why this line exists
//!
//! `#6157` added a DEGRADED verdict for a one-way peer-claim channel, and
//! `#5921` added the four transport counters. Neither was rendered as a
//! *conclusion*: the status block printed the counters and nothing else, so the
//! only way to notice a broken channel was for an operator to compare
//! `advertised` against `received` by eye and know what the gap meant.
//!
//! Measured consequence (`loom-worker-1`/`loom-worker-2`, 2026-09-29): the
//! fleet's claims room had been rejecting **every** send with a homeserver 500
//! since 2026-09-23, and `loom-daemon status` reported
//! `advertised=3765 received=0` with no verdict, no marker, and no reason — six
//! days of a completely inert cross-host coordination channel presenting as
//! normal output. See `loom_daemon::peer_claims::send_health` for the full trace.
//!
//! Kept in its own sibling module (the `status_render/*_line.rs` convention)
//! because `status_render.rs` is over the size ratchet
//! (`scripts/file-size-baseline.txt`).

use loom_daemon::types::PeerCoordinationHealth;

/// Render the peer-coordination verdict line that belongs beneath the
/// counters — a `DEGRADED` marker plus the verdict's own `reason` sentence.
///
/// Returns `None` while healthy: this block is already several lines and a
/// healthy fleet should not pay for a diagnostic that only matters when it
/// fires. That asymmetry is deliberate — the failure mode being fixed is a
/// degraded channel that looked identical to a healthy one, not a healthy
/// channel that was hard to confirm.
///
/// Split from [`print_coordination_verdict`] so the text itself is assertable:
/// this issue's acceptance criterion is that a one-way channel *surfaces* in
/// `loom-daemon status`, which is a claim about the rendered string, not about
/// a function having been called.
#[must_use]
pub fn render_coordination_verdict(health: &PeerCoordinationHealth) -> Option<String> {
    if !health.degraded {
        return None;
    }
    let held_for = health
        .degraded_for_secs
        .map_or_else(|| "just now".to_string(), |s| format!("{s}s"));
    Some(format!(
        "               DEGRADED ({held_for}, {}/{} receives toward recovery){}",
        health.consecutive_receives_toward_recovery,
        health.recovery_threshold,
        // A daemon predating #9294 (or one whose reaper has not ticked yet)
        // carries no reason; print the marker without inventing one.
        health
            .reason
            .as_deref()
            .map_or_else(String::new, |r| format!(" — {r}"))
    ))
}

/// Print [`render_coordination_verdict`]'s line, if there is one.
pub fn print_coordination_verdict(health: &PeerCoordinationHealth) {
    if let Some(line) = render_coordination_verdict(health) {
        println!("{line}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn healthy() -> PeerCoordinationHealth {
        PeerCoordinationHealth {
            degraded: false,
            degraded_for_secs: None,
            consecutive_receives_toward_recovery: 0,
            recovery_threshold: 3,
            reason: Some("receiving normally".to_string()),
        }
    }

    /// A healthy verdict prints nothing — the counters line already said it.
    #[test]
    fn healthy_prints_nothing() {
        assert_eq!(render_coordination_verdict(&healthy()), None);
        // Also infallible through the printing wrapper.
        print_coordination_verdict(&healthy());
    }

    /// A degraded verdict with no reason (pre-#9294 daemon over IPC) must still
    /// render the marker rather than panicking or printing a dangling dash.
    #[test]
    fn degraded_without_a_reason_still_renders() {
        let line = render_coordination_verdict(&PeerCoordinationHealth {
            degraded: true,
            degraded_for_secs: Some(4200),
            reason: None,
            ..healthy()
        })
        .expect("a degraded verdict always renders");
        assert!(line.contains("DEGRADED"), "{line}");
        assert!(!line.trim_end().ends_with('—'), "no dangling dash: {line}");
    }

    /// **This issue's acceptance criterion, item 4.** A host that has been
    /// advertising into a dead channel must surface as DEGRADED *in the status
    /// output itself*, with the reason — not as two counters an operator has
    /// to compare by eye. Before #9294 this block rendered
    /// `advertised=3765 received=0` and stopped, while `loom-daemon health`
    /// knew perfectly well the verdict was DEGRADED.
    #[test]
    fn a_publish_blocked_channel_surfaces_in_the_status_line() {
        let line = render_coordination_verdict(&PeerCoordinationHealth {
            degraded: true,
            degraded_for_secs: Some(518_400),
            reason: Some(loom_daemon::peer_claims::publish_blocked_reason(
                "[500 / M_UNKNOWN] no forward extremities",
                3765,
            )),
            ..healthy()
        })
        .expect("a degraded verdict always renders");
        assert!(line.contains("DEGRADED"), "{line}");
        assert!(line.contains("518400s"), "how long it has been bad: {line}");
        assert!(
            line.contains("no forward extremities"),
            "the operator needs the transport's own error: {line}"
        );
        assert!(line.contains("publishing nothing"), "and the conclusion drawn from it: {line}");
    }

    /// The receive-quiet verdict surfaces through the same line — item 4 is
    /// about any sustained one-way channel, not only the publish-blocked one.
    #[test]
    fn a_receive_quiet_channel_surfaces_too() {
        let line = render_coordination_verdict(&PeerCoordinationHealth {
            degraded: true,
            degraded_for_secs: Some(3600),
            consecutive_receives_toward_recovery: 1,
            recovery_threshold: 3,
            reason: Some(
                "no peer claim received in 1800s (>= 1200s grace) despite this host \
                 advertising — the receive path looks one-way or dead"
                    .to_string(),
            ),
        })
        .expect("a degraded verdict always renders");
        assert!(line.contains("1/3 receives toward recovery"), "{line}");
        assert!(line.contains("one-way or dead"), "{line}");
    }
}
