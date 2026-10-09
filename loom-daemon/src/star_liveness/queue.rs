//! Where a deferred starred issue stands in the host's queue, and what holds
//! it there (#10214). Pure: the work finder's last tick rows and cap terms in,
//! one [`CapacityWait`] out.
//!
//! Before #10214 every capacity-style deferral was reported as one
//! undifferentiated `no-capacity` with next actor `work-finder`, so "stuck"
//! and "waiting at position 96 behind a disk-limited cap of 2" looked the
//! same, and the no-progress escalation sent the operator to the wrong
//! component. The wait names the gate, the binding cap term and the queue
//! position, so the row reads `queued #88 of 106 (cap 2, disk-limited)` and
//! the watchdog can count an advancing position as progress.

use crate::types::{CapView, CapacityWait, QueueDisposition, ReadyQueueRow};

/// The work-finder gate behind a capacity-style disposition, or `None` for a
/// disposition that is not a capacity-style deferral.
#[must_use]
pub fn gate(d: QueueDisposition) -> Option<&'static str> {
    Some(match d {
        QueueDisposition::DeferredCapacity => "capacity",
        QueueDisposition::DeferredRampCap => "ramp",
        QueueDisposition::DeferredSaturation => "saturation",
        QueueDisposition::DeferredBuildBackoff => "build-backoff",
        QueueDisposition::DeferredOutOfSlice => "repo-slice",
        QueueDisposition::DeferredRepoCap => "repo-cap",
        QueueDisposition::DeferredFileOverlap => "file-overlap",
        QueueDisposition::HostConstraint => "host-affinity",
        QueueDisposition::HostClassRefused => "host-class",
        _ => return None,
    })
}

/// Whether `r` is a starred issue waiting on a capacity-style gate.
#[must_use]
pub fn waiting_star(r: &ReadyQueueRow) -> bool {
    r.operator_priority && r.disposition.state() == "ready"
}

/// `row`'s 1-based position among the waiting starred issues in `queue` (the
/// host-wide tick rows, in dispatch order), and how many there are. A row
/// that is not itself marked starred (an inheriting blocker) is placed by its
/// rank among them.
#[must_use]
pub fn position(queue: &[ReadyQueueRow], row: &ReadyQueueRow) -> (u32, u32) {
    let mut stars: Vec<&ReadyQueueRow> = queue.iter().filter(|r| waiting_star(r)).collect();
    stars.sort_by_key(|r| r.rank);
    let same = |r: &&ReadyQueueRow| r.repo == row.repo && r.issue == row.issue;
    let (pos, total) = match stars.iter().position(same) {
        Some(i) => (i + 1, stars.len()),
        None => {
            let ahead = stars.iter().filter(|r| r.rank < row.rank).count();
            (ahead + 1, stars.len() + 1)
        }
    };
    let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    (n(pos), n(total))
}

/// The wait for `row`, when its disposition is a capacity-style deferral.
/// `queue` is the host-wide tick (an empty slice falls back to `fallback`,
/// the repo's own rows); `cap` is the tick's cap terms, when recorded.
#[must_use]
pub fn wait(
    row: &ReadyQueueRow,
    queue: &[ReadyQueueRow],
    fallback: &[ReadyQueueRow],
    cap: Option<CapView>,
) -> Option<CapacityWait> {
    let gate = gate(row.disposition)?;
    let queued = !matches!(gate, "host-affinity" | "host-class");
    let rows = if queue.is_empty() { fallback } else { queue };
    let (position, total) = if queued {
        let (p, t) = position(rows, row);
        (Some(p), Some(t))
    } else {
        (None, None)
    };
    Some(CapacityWait {
        gate: gate.to_string(),
        limiter: cap.filter(|_| gate == "capacity").map(|c| c.limiter()),
        position,
        total,
        cap: cap.map(|c| c.effective()),
        configured_cap: cap.map(|c| c.configured),
    })
}

/// What the operator can do about a queue that is not moving, by gate and
/// binding term. One sentence, for the no-progress escalation.
#[must_use]
pub fn advice(w: &CapacityWait) -> String {
    let below = match (w.cap, w.configured_cap) {
        (Some(c), Some(k)) if c < k => {
            format!(" (holding the cap at {c}, below the configured {k})")
        }
        _ => String::new(),
    };
    let rerank = "or unstar / re-rank older stars so this one goes sooner";
    match (w.gate.as_str(), w.limiter) {
        ("capacity", Some(crate::types::CapLimiter::Disk)) => format!(
            "Disk headroom on the worktree volume is the limit{below}: free disk or add \
             capacity, {rerank}."
        ),
        ("capacity", Some(crate::types::CapLimiter::Ram)) => {
            format!("Available RAM is the limit{below}: free memory or add capacity, {rerank}.")
        }
        ("capacity", _) => format!(
            "Every slot is taken by older work: raise `maxConcurrent`, add a host, {rerank}."
        ),
        ("saturation", _) => format!(
            "The host's saturation brake is holding new admissions: reduce load or add a \
             host, {rerank}."
        ),
        ("repo-cap", _) => {
            format!("Its repo is at `maxConcurrentPerRepo`: raise that cap, {rerank}.")
        }
        ("host-affinity" | "host-class", _) => {
            "This host may not run it: make sure a host that can is up and managing the repo, \
             or remove the host constraint."
                .to_string()
        }
        _ => format!("Check the work finder's queue (`loom-daemon queue`), {rerank}."),
    }
}
