//! Curator complexity-tier signal for `sweep.outcome` (Issue #8542).
//!
//! # Why a forge read at terminal transition, not a dispatch-time plumb
//!
//! `resolve-tier-model.sh` already reads an issue's `<!-- loom:complexity=
//! <tier> -->` marker at dispatch time to pick a model, but that resolution
//! happens in a shell process outside the daemon (and via several different
//! entry points — `dispatch_sweep`, the epic supervisor, the work finder, the
//! role runner), so there is no single dispatch-time seam that could hand the
//! resolved tier to [`crate::sweep_registry::SweepRegistry`] without plumbing
//! a new parameter through every one of them.
//!
//! The marker itself is a durable, essentially-static property of the
//! ISSUE (the Curator sets it once; it does not change over the sweep's
//! lifetime the way PR labels do), so re-reading it from the forge at the
//! SAME terminal transition that already reads the PR label timeline for
//! [`crate::telemetry::SweepOutcomeRecord::doctor_cycles`] /
//! [`judge_verdicts`](crate::telemetry::SweepOutcomeRecord::judge_verdicts)
//! (see [`super::label_timeline`]) is one more best-effort REST call with the
//! exact same cost/fail-open shape, not a second architecture.
//!
//! # Fail-open contract
//!
//! Every failure — `skip_label_flip`, the fleet rate-limit breaker
//! suppressing forge polling, spawn error, timeout, non-zero exit, an issue
//! body with no recognized marker — yields `None`, which the caller turns
//! into an **absent** `complexity` key. Never a fabricated `"routine"`: unlike
//! `resolve-tier-model.sh`'s own dispatch-time fold (an absent/unrecognized
//! marker there is a deliberate SAFE DEFAULT for model selection), this
//! journal field must stay honest about "unobserved" — a routing-evaluation
//! consumer needs the true absence rate, not a default masquerading as data.

use super::*;

use crate::script_helpers::model_tiers::COMPLEXITY_TIERS;
use crate::script_helpers::sweep_experiment::extract_complexity_marker;

impl SweepRegistry {
    /// Best-effort Curator complexity tier for `issue` (Issue #8542), read off
    /// the issue body via one REST `gh api` call — the independent, larger
    /// pool, not the GraphQL one every agent-side `gh issue view` burns (the
    /// same reasoning [`super::label_timeline::fetch_timeline_signals`]
    /// documents for the PR timeline read alongside this one).
    ///
    /// Skipped outright (never shelling to `gh`) when `skip_label_flip` is
    /// set or the fleet rate-limit breaker is suppressing forge polling,
    /// matching every other real-forge probe on this terminal-transition
    /// path. `None` on any read failure or an issue body with no recognized
    /// `mechanical`/`routine`/`complex` marker.
    pub(crate) fn fetch_complexity_signal(&self, issue: u32) -> Option<String> {
        if self.config.skip_label_flip {
            return None;
        }
        if crate::rate_limit_breaker::global_is_suppressed() {
            log::debug!(
                "sweep_outcomes: skipping the issue #{issue} complexity-marker read — the \
                 rate-limit breaker is suppressing forge polling (#8542)"
            );
            return None;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}"))
            .arg("--jq")
            .arg(".body");
        // Resolve the issue against THIS registry's repo, not the daemon's cwd
        // repo, and honor a cross-owner managed repo's own installation-token
        // config dir — same rationale as the PR-timeline read.
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        // A machine-global `LOOM_REPO` override reaches `gh api` as `GH_REPO`,
        // never as an unsupported `--repo` flag (#8263).
        crate::gh_repo_env::apply_loom_repo_override(&mut cmd);
        let output = match output_with_timeout(cmd, reap_gh_timeout()) {
            Ok(Some(o)) if o.status.success() => o,
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                crate::rate_limit_breaker::global_observe_failure(
                    &stderr,
                    "sweep_outcome_complexity_signal",
                );
                log::warn!(
                    "sweep_outcomes: issue #{issue} complexity-marker read failed ({}) — \
                     omitting complexity, record still written (#8542): {stderr}",
                    o.status
                );
                return None;
            }
            Ok(None) => {
                log::warn!(
                    "sweep_outcomes: issue #{issue} complexity-marker read timed out — omitting \
                     complexity, record still written (#8542)"
                );
                return None;
            }
            Err(e) => {
                log::warn!(
                    "sweep_outcomes: could not invoke {} for issue #{issue}'s complexity marker: \
                     {e} — omitting complexity, record still written (#8542)",
                    gh.display()
                );
                return None;
            }
        };
        let body = String::from_utf8_lossy(&output.stdout);
        let tier = extract_complexity_marker(&body)?;
        COMPLEXITY_TIERS.contains(&tier).then(|| tier.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    /// The marker vocabulary this module accepts is exactly
    /// `resolve-tier-model.sh`'s closed enum — nothing else, not even a value
    /// that `resolve-tier-model.sh` itself would fold to `routine`.
    #[test]
    fn valid_tiers_are_exactly_the_closed_vocabulary() {
        for tier in ["mechanical", "routine", "complex"] {
            assert!(COMPLEXITY_TIERS.contains(&tier));
        }
        assert!(!COMPLEXITY_TIERS.contains(&"trivial"));
        assert!(!COMPLEXITY_TIERS.contains(&""));
    }
}
