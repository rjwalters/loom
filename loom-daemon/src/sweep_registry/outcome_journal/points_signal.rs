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
//! `fetch_complexity_signal` does) would add a needless forge round trip to
//! every terminal transition even while
//! `autonomous.sweepOutcomeWriteback.enabled` stays at its default `false`.
//!
//! # Fail-open contract
//!
//! Identical to [`super::complexity_signal::fetch_complexity_signal`]: any
//! failure — `skip_label_flip`, the fleet rate-limit breaker, a spawn error, a
//! timeout, a non-zero exit, or an issue body with no recognized marker —
//! yields `None`, never a fabricated value.

use super::*;
use regex::Regex;

/// Closed vocabulary the Curator's `<!-- loom:points=<N> -->` marker MUST be
/// one of (Issue #9056) — mirrors `COMPLEXITY_TIERS`'s closed-enum discipline:
/// an out-of-vocabulary value is a curation defect, not a style choice.
pub(crate) const POINTS_VALUES: &[&str] = &["1", "2", "3", "5", "8", "13"];

/// `<!-- loom:points=<N> -->`, anchored to the canonical HTML-comment form
/// exactly like `require-complexity-marker.sh`'s own
/// `<!--[[:space:]]*loom:complexity=...-->` pattern — so prose that merely
/// *discusses* the marker syntax cannot be mistaken for a real marker (the
/// same #4840 concern the complexity marker's own parser guards against).
fn points_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<!--\s*loom:points=([0-9]+)\s*-->").expect("static points-marker pattern")
    })
}

/// Pure extraction of the LAST `loom:points` marker in `body`, mirroring
/// `require-complexity-marker.sh`'s `grep -oE ... | tail -1` (the marker
/// nearest the end of the body wins when more than one is present).
/// Validated against [`POINTS_VALUES`] — an out-of-vocabulary digit string
/// (e.g. `21`) is treated as absent, same as an unrecognized complexity tier.
#[must_use]
pub(crate) fn extract_points_marker(body: &str) -> Option<&str> {
    let value = points_marker_re()
        .captures_iter(body)
        .last()?
        .get(1)?
        .as_str();
    POINTS_VALUES.contains(&value).then_some(value)
}

impl SweepRegistry {
    /// Best-effort Curator points estimate for `issue` (Issue #9056), read
    /// off the issue body via one REST `gh api` call — same transport,
    /// timeout, breaker-gating and `skip_label_flip` short-circuit as
    /// [`SweepRegistry::fetch_complexity_signal`]. Called only from
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_valid_marker() {
        let body = "Some issue body.\n\n<!-- loom:points=5 -->\n";
        assert_eq!(extract_points_marker(body), Some("5"));
    }

    #[test]
    fn rejects_out_of_vocabulary_values() {
        let body = "Body.\n\n<!-- loom:points=21 -->\n";
        assert_eq!(extract_points_marker(body), None);
    }

    #[test]
    fn absent_marker_is_none() {
        assert_eq!(extract_points_marker("no marker here"), None);
    }

    #[test]
    fn takes_the_last_marker_when_several_are_present() {
        let body = "<!-- loom:points=1 -->\n\nDrifted.\n\n<!-- loom:points=8 -->\n";
        assert_eq!(extract_points_marker(body), Some("8"));
    }

    #[test]
    fn prose_mentioning_the_marker_syntax_does_not_block_the_real_one() {
        // A `<N>` placeholder has no digits to capture, so it simply never
        // matches the regex — the real marker later in the body is still
        // found, mirroring the #4840 fix for the complexity marker.
        let body = "Emit `<!-- loom:points=<N> -->` in the body.\n\n<!-- loom:points=3 -->\n";
        assert_eq!(extract_points_marker(body), Some("3"));
    }

    #[test]
    fn closed_vocabulary_matches_the_documented_set() {
        for v in ["1", "2", "3", "5", "8", "13"] {
            assert!(POINTS_VALUES.contains(&v));
        }
        assert!(!POINTS_VALUES.contains(&"21"));
        assert!(!POINTS_VALUES.contains(&"0"));
    }
}
