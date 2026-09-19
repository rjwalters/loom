//! Section 4: is the heartbeat fresh?
//!
//! The daemon writes its heartbeat file on a declared cadence (#4011). A live
//! daemon whose heartbeat has gone stale is likely wedged — still a process,
//! but no longer doing its periodic work. The threshold is a comfortable
//! multiple of that cadence so a single missed write never false-positives.
//!
//! Three of the five outcomes here are **liveness-only OK** rather than a
//! verdict about the heartbeat, and that is deliberate. An absent file, an
//! unreadable mtime, and a heartbeat predating the process all mean *this tick
//! has no evidence about the current process* — which is not the same as
//! evidence of health, and emphatically not evidence of a wedge. Reporting any
//! of them as a divergence would page an operator for a missing optional
//! signal.

use crate::daemon_install_state::HeartbeatFreshness;

use super::report::{Level, Reporter};

/// What the heartbeat section decided, and the exit code it implies.
pub struct Verdict {
    pub level: Level,
    pub message: String,
    /// `true` when this is an OK-shaped outcome that must route through
    /// [`Reporter::heartbeat_ok`] so a divergence earlier in the tick is not
    /// papered over by a clean-looking final line (#5790).
    pub ok_shaped: bool,
}

/// Decide, given the classification and the numbers behind it.
#[must_use]
pub fn decide(
    freshness: Option<HeartbeatFreshness>,
    liveness_detail: &str,
    heartbeat_file: &str,
    age_secs: Option<u64>,
    stale_threshold_secs: Option<u64>,
    process_age_secs: Option<u64>,
) -> Verdict {
    let ok = |message: String| Verdict {
        level: Level::Ok,
        message,
        ok_shaped: true,
    };

    match freshness {
        // The file's mtime predates the live process, so it is necessarily left
        // over from a previous boot (or a previous enablement of the opt-in
        // heartbeat loop) and carries NO evidence about the process running
        // now. Never rendered as stale/wedged (#4368).
        Some(HeartbeatFreshness::PriorBoot) => {
            let age = age_secs.unwrap_or(0);
            let proc_age = process_age_secs.unwrap_or(0);
            ok(format!(
                "daemon alive ({liveness_detail}); heartbeat {heartbeat_file} is from a PREVIOUS \
                 boot ({age}s old; this process is only {proc_age}s old) — not evidence about \
                 the current process. Liveness-only OK; re-check after the process is well past \
                 startup if you still suspect a wedge."
            ))
        }
        Some(HeartbeatFreshness::Stale) => {
            let age = age_secs.unwrap_or(0);
            let threshold = stale_threshold_secs.unwrap_or(0);
            Verdict {
                level: Level::Divergence,
                message: format!(
                    "Daemon process is alive ({liveness_detail}) but its heartbeat \
                     {heartbeat_file} is STALE ({age}s old > {threshold}s threshold) — the \
                     daemon may be wedged. Inspect with 'loom-daemon status'; consider \
                     ./.loom/scripts/cli/loom-daemon-stop.sh && ...start.sh."
                ),
                ok_shaped: false,
            }
        }
        Some(HeartbeatFreshness::Fresh) => {
            let age = age_secs.unwrap_or(0);
            let threshold = stale_threshold_secs.unwrap_or(0);
            ok(format!(
                "daemon healthy ({liveness_detail}); heartbeat fresh ({age}s ≤ {threshold}s)."
            ))
        }
        // Unknown splits two ways on whether the file exists at all, because
        // the operator-facing advice differs: an absent file usually means the
        // opt-in loop is off, an unreadable mtime means something stranger.
        Some(HeartbeatFreshness::Unknown) | None => {
            if age_secs.is_some() {
                ok(format!(
                    "daemon alive ({liveness_detail}); heartbeat mtime unreadable — \
                     liveness-only OK."
                ))
            } else {
                ok(format!(
                    "daemon alive ({liveness_detail}); no heartbeat file at {heartbeat_file} \
                     (heartbeat disabled or not yet written) — liveness-only OK."
                ))
            }
        }
    }
}

/// Emit the verdict and return the tick's exit code.
///
/// An OK-shaped outcome routes through [`Reporter::heartbeat_ok`], which
/// substitutes `DEGRADED` when the probe diverged earlier in this tick — before
/// #5790 these called `report OK` unconditionally, so a tick that had already
/// logged a sub-threshold `DIVERGENCE` still ended with a clean
/// `[OK] daemon healthy` line for a log-scraper to find.
#[must_use]
pub fn emit(verdict: &Verdict, reporter: &Reporter) -> i32 {
    if verdict.ok_shaped {
        reporter.heartbeat_ok(&verdict.message);
        // exit_ok(): a divergence anywhere in this tick owns the exit code,
        // even when the last line read as healthy.
        i32::from(reporter.diverged())
    } else {
        reporter.report(verdict.level, &verdict.message);
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(f: Option<HeartbeatFreshness>, age: Option<u64>, proc_age: Option<u64>) -> Verdict {
        decide(f, "pid 42 alive", "/tmp/hb", age, Some(300), proc_age)
    }

    #[test]
    fn a_stale_heartbeat_is_the_only_divergence_here() {
        assert_eq!(v(Some(HeartbeatFreshness::Stale), Some(400), None).level, Level::Divergence);
        for f in [
            HeartbeatFreshness::Fresh,
            HeartbeatFreshness::Unknown,
            HeartbeatFreshness::PriorBoot,
        ] {
            assert!(v(Some(f), Some(10), Some(5)).ok_shaped, "{f:?} must not page");
        }
    }

    #[test]
    fn a_prior_boot_heartbeat_is_never_described_as_stale() {
        // #4368: it is older than the threshold, but it cannot say anything
        // about a process younger than itself.
        let got = v(Some(HeartbeatFreshness::PriorBoot), Some(9000), Some(30));
        assert!(got.ok_shaped);
        assert!(got.message.contains("PREVIOUS boot"), "{}", got.message);
        assert!(!got.message.to_lowercase().contains("stale"), "{}", got.message);
        assert!(got.message.contains("9000s old"), "{}", got.message);
        assert!(got.message.contains("only 30s old"), "{}", got.message);
    }

    #[test]
    fn an_absent_file_and_an_unreadable_mtime_give_different_advice() {
        let absent = v(Some(HeartbeatFreshness::Unknown), None, None);
        let unreadable = v(Some(HeartbeatFreshness::Unknown), Some(5), None);
        assert!(absent.message.contains("no heartbeat file at /tmp/hb"), "{}", absent.message);
        assert!(unreadable.message.contains("mtime unreadable"), "{}", unreadable.message);
        assert!(absent.ok_shaped && unreadable.ok_shaped);
    }

    #[test]
    fn a_fresh_heartbeat_states_both_numbers_it_compared() {
        let got = v(Some(HeartbeatFreshness::Fresh), Some(12), None);
        assert!(got.message.contains("12s"), "{}", got.message);
        assert!(got.message.contains("300s"), "{}", got.message);
    }
}
