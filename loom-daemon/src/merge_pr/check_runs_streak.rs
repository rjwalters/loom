//! The persistent-vs-transient check-runs HTTP 404 classification inside
//! `_wait_for_checks_then_sync_merge`'s poll loop (#6389, an #8191 slice).
//!
//! # The incident
//!
//! A repo with NO GitHub Actions workflows configured (and `allow_auto_merge`
//! disabled) makes `GET /repos/{nwo}/commits/{sha}/check-runs` return a
//! PERSISTENT HTTP 404 for every SHA — the Checks API itself is unavailable
//! for that repo, not merely for this commit. Before #6389, the `--auto` wait
//! loop could not distinguish that from a transient fetch blip (a network
//! error, a 5xx): both looked like "still pending", so a PR on such a repo
//! polled all the way to `LOOM_AUTO_MERGE_TIMEOUT` (600s default) even though
//! nothing was ever going to answer.
//!
//! # The rule
//!
//! Each poll iteration attempts the fetch, and — only on failure — retries
//! once (absorbing a single blip). Whether THIS iteration counts as a
//! "confirmed 404" requires BOTH attempts to have returned the dedicated
//! not-found return code; a 404 on the first attempt followed by a
//! DIFFERENT failure (a 5xx, say) on the retry is not confirmed, because the
//! two attempts disagree about what is actually wrong. A confirmed 404
//! increments a running streak; anything else — success, or an unconfirmed
//! failure — resets it to zero. Once the streak reaches the configured
//! threshold (`LOOM_CHECK_RUNS_404_STREAK`, default 2), the loop gives up
//! waiting and proceeds directly to the synchronous merge, on the theory that
//! a repo whose Checks API answers 404 that consistently, that many polls in
//! a row (each spaced a full `LOOM_AUTO_MERGE_POLL_INTERVAL` apart), simply
//! has no checks configured for this commit.
//!
//! # Fail direction
//!
//! This subcommand is invoked ONLY on the failure path (the shell calls it
//! after establishing `fetch_rc != 0`; success resets the streak inline and
//! never reaches here) — see `cli::merge_pr_check_runs_streak`. A missing or
//! older binary is read by the caller as `Pending` with the streak reset to
//! zero: the pre-#6389 behaviour, so a guard fault can only ever cost time —
//! bounded by the ordinary `LOOM_AUTO_MERGE_TIMEOUT` ceiling — never
//! misclassify a transient blip as the persistent condition that skips
//! waiting altogether.

/// One iteration's outcome, once the fetch has already failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The streak has reached the threshold: give up waiting and proceed to
    /// the synchronous merge — this repo's check-runs API is treated as
    /// persistently unavailable for this commit.
    Proceed,
    /// Below the threshold: keep polling (subject to the caller's own
    /// deadline/timeout handling, which this decision does not own).
    Pending,
}

impl Verdict {
    /// The token this verdict renders as on the decision line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proceed => "PROCEED",
            Self::Pending => "PENDING",
        }
    }
}

/// Everything one failed-fetch iteration's classification needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    /// The first fetch attempt's return code this iteration.
    pub attempt1_rc: i32,
    /// The retry's return code — meaningful only when `attempt1_rc != 0`
    /// (mirroring the shell's own retry-once-on-failure absorption); a caller
    /// invoking this on a successful first attempt should not reach here at
    /// all (see the module docs' "fail direction" note).
    pub attempt2_rc: i32,
    /// The running streak BEFORE this iteration.
    pub streak_in: u64,
    /// `LOOM_CHECK_RUNS_404_STREAK` — consecutive confirmed 404s required
    /// before giving up on the wait.
    pub threshold: u64,
    /// `FORGE_CHECK_RUNS_RC_NOT_FOUND` — the dedicated confirmed-404 return
    /// code, passed in rather than hardcoded so the shell's single source of
    /// truth (`lib/forge-helpers.sh`) stays the only place that constant is
    /// declared.
    pub not_found_rc: i32,
}

/// One iteration's decision: the new streak, and what the caller does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    /// The streak AFTER this iteration — the caller's next `--streak` input.
    pub streak_out: u64,
}

/// Classify one failed-fetch iteration. Pure: both fetch attempts already
/// happened.
///
/// A confirmed 404 (both attempts equal `not_found_rc`) increments the
/// streak; anything else (a single 404, a transient failure repeated any
/// number of times, or a 404 paired with a DIFFERENT failure on the retry)
/// resets it to zero — exactly the shell's own
/// `[[ "$attempt1_rc" -eq "$FORGE_CHECK_RUNS_RC_NOT_FOUND" && "$attempt2_rc"
/// -eq "$FORGE_CHECK_RUNS_RC_NOT_FOUND" ]]` gate.
#[must_use]
pub fn decide(inp: &Inputs) -> Decision {
    let confirmed = inp.attempt1_rc == inp.not_found_rc && inp.attempt2_rc == inp.not_found_rc;
    let streak_out = if confirmed { inp.streak_in + 1 } else { 0 };
    let verdict = if streak_out >= inp.threshold {
        Verdict::Proceed
    } else {
        Verdict::Pending
    };
    Decision {
        verdict,
        streak_out,
    }
}

/// The one stdout line the caller parses: `LOOM-CHECK-RUNS-STREAK <verdict>
/// <streak>`. Three space-separated tokens, so `read -r sentinel verdict
/// streak` lands cleanly with no trailing free-text field to worry about
/// escaping — unlike the guards that carry operator-facing narration, this
/// decision has none: the shell already owns every message this iteration
/// might print (the "unavailable, proceeding" info line, the "still-pending"
/// warning, the timeout warning), keyed off the verdict alone.
#[must_use]
pub fn render(d: &Decision) -> String {
    format!("LOOM-CHECK-RUNS-STREAK {} {}", d.verdict.as_str(), d.streak_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NF: i32 = 44;
    const THRESHOLD: u64 = 2;

    fn inp(a1: i32, a2: i32, streak_in: u64) -> Inputs {
        Inputs {
            attempt1_rc: a1,
            attempt2_rc: a2,
            streak_in,
            threshold: THRESHOLD,
            not_found_rc: NF,
        }
    }

    #[test]
    fn persistent_404_crosses_the_threshold_on_the_second_confirmed_iteration() {
        let d1 = decide(&inp(NF, NF, 0));
        assert_eq!(
            d1,
            Decision {
                verdict: Verdict::Pending,
                streak_out: 1
            }
        );
        let d2 = decide(&inp(NF, NF, d1.streak_out));
        assert_eq!(
            d2,
            Decision {
                verdict: Verdict::Proceed,
                streak_out: 2
            }
        );
    }

    #[test]
    fn transient_5xx_never_crosses_the_threshold_no_matter_how_often_repeated() {
        let mut streak = 0;
        for _ in 0..10 {
            let d = decide(&inp(1, 1, streak));
            assert_eq!(d.verdict, Verdict::Pending);
            assert_eq!(d.streak_out, 0);
            streak = d.streak_out;
        }
    }

    #[test]
    fn mixed_404_then_different_failure_is_not_confirmed_and_resets() {
        // attempt1 is a confirmed 404, but the retry is a DIFFERENT failure
        // (a 5xx) — only a 404 on BOTH attempts counts.
        let d = decide(&inp(NF, 1, 1));
        assert_eq!(
            d,
            Decision {
                verdict: Verdict::Pending,
                streak_out: 0
            }
        );
    }

    #[test]
    fn a_recovering_success_between_iterations_resets_the_streak() {
        // (Success itself never reaches `decide` per the fail-direction note —
        // the shell resets the streak inline — but the NEXT confirmed-404
        // iteration must restart at 1, not resume from wherever it left off.)
        let after_recovery_streak = 0;
        let d = decide(&inp(NF, NF, after_recovery_streak));
        assert_eq!(
            d,
            Decision {
                verdict: Verdict::Pending,
                streak_out: 1
            }
        );
    }

    #[test]
    fn threshold_of_one_proceeds_on_the_first_confirmed_iteration() {
        let d = decide(&Inputs {
            threshold: 1,
            ..inp(NF, NF, 0)
        });
        assert_eq!(
            d,
            Decision {
                verdict: Verdict::Proceed,
                streak_out: 1
            }
        );
    }

    #[test]
    fn render_matches_the_documented_three_token_line() {
        let d = Decision {
            verdict: Verdict::Proceed,
            streak_out: 3,
        };
        assert_eq!(render(&d), "LOOM-CHECK-RUNS-STREAK PROCEED 3");
        let d2 = Decision {
            verdict: Verdict::Pending,
            streak_out: 0,
        };
        assert_eq!(render(&d2), "LOOM-CHECK-RUNS-STREAK PENDING 0");
    }
}
