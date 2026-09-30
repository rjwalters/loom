//! Curator points-estimate signal for the sweep-outcome write-back comment
//! (Issue #9056).
//!
//! # Why a separate fetch from `complexity_signal`
//!
//! `<!-- loom:points=<N> -->` is a second, independent Curator marker (Issue
//! #9056, alongside the existing `<!-- loom:complexity=<tier> -->` — see
//! [`super::complexity_signal`] for that one). It is read the SAME way — one
//! best-effort REST `gh api` call against the sweep's own issue body, at the
//! SAME terminal transition — but only when the write-back comment
//! ([`super::writeback`]) is actually about to be built: unlike `complexity`,
//! which is a permanent field on every `sweep.outcome` telemetry record, this
//! value is opt-in-consumed only, so fetching it unconditionally (the way
//! `fetch_issue_signals` does) would add a needless forge round trip to
//! every terminal transition even while
//! `autonomous.sweepOutcomeWriteback.enabled` stays at its default `false`.
//!
//! The extraction regex and closed vocabulary themselves live in
//! [`crate::points_marker`], shared with the `require-complexity-marker.sh`
//! validation helper (`cli::points_marker_check`, the daemon's binary crate)
//! so the two never drift apart.
//!
//! # Fail-open contract
//!
//! Identical to [`super::complexity_signal`]'s own fetch: any
//! failure — `skip_label_flip`, the fleet rate-limit breaker, a spawn error, a
//! timeout, a non-zero exit, or an issue body with no recognized marker —
//! yields `None`, never a fabricated value.

use super::*;
use crate::points_marker::extract_points_marker;

impl SweepRegistry {
    /// Best-effort Curator points estimate for `issue` (Issue #9056), read
    /// off the issue body via one REST `gh api` call — same transport,
    /// timeout, breaker-gating and `skip_label_flip` short-circuit as
    /// [`SweepRegistry::fetch_issue_signals`]. Called only from
    /// [`super::writeback`]'s own gate (opt-in flag + `Success` result +
    /// idempotency check already passed), never unconditionally, so this
    /// costs nothing while the write-back stays disabled (the default).
    pub(crate) fn fetch_points_signal(&self, issue: u32) -> Option<String> {
        if self.config.skip_label_flip {
            return None;
        }
        if crate::rate_limit_breaker::global_is_suppressed() {
            log::debug!(
                "sweep_outcomes: skipping the issue #{issue} points-marker read — the rate-limit \
                 breaker is suppressing forge polling (#9056)"
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
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        crate::gh_repo_env::apply_loom_repo_override(&mut cmd);
        let output = match output_with_timeout(cmd, reap_gh_timeout()) {
            Ok(Some(o)) if o.status.success() => o,
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                crate::rate_limit_breaker::global_observe_failure(
                    &stderr,
                    "sweep_outcome_points_signal",
                );
                log::warn!(
                    "sweep_outcomes: issue #{issue} points-marker read failed ({}) — omitting \
                     points from this pass's write-back (#9056): {stderr}",
                    o.status
                );
                return None;
            }
            Ok(None) => {
                log::warn!(
                    "sweep_outcomes: issue #{issue} points-marker read timed out — omitting \
                     points (#9056)"
                );
                return None;
            }
            Err(e) => {
                log::warn!(
                    "sweep_outcomes: could not invoke {} for issue #{issue}'s points marker: {e} \
                     — omitting points (#9056)",
                    gh.display()
                );
                return None;
            }
        };
        let body = String::from_utf8_lossy(&output.stdout);
        extract_points_marker(&body).map(str::to_string)
    }
}
