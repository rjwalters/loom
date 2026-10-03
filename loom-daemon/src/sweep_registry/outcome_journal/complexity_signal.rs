//! Curator complexity-tier signal for `sweep.outcome` (Issue #8542), plus the
//! issue's own end state (Issue #9441) — both off ONE forge read.
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
//! # Why the end state rides along (Issue #9441)
//!
//! [`crate::telemetry::SweepOutcomeRecord::disposition`] has to separate three
//! shapes that are byte-identical in the local signals — "the issue was
//! already done", "the Curator closed it instead of building it", and "the
//! Curator handed it back for re-scoping". All three are the issue's own
//! state, and the single REST object this module already fetches carries
//! every field that answers them (`state`, `closed_at`, `labels`). So the
//! `--jq` filter widened from `.body` to a small projection and the SAME call
//! now yields both signals: **no extra forge round trip**, and the two can
//! never disagree about which issue they described.
//!
//! # Fail-open contract
//!
//! Every failure — `skip_label_flip`, the fleet rate-limit breaker
//! suppressing forge polling, spawn error, timeout, non-zero exit, unparseable
//! output, an issue body with no recognized marker — yields an empty
//! [`IssueSignals`], which the caller turns into an **absent** `complexity`
//! key and an unconstrained (`None`) end state. Never a fabricated
//! `"routine"`: unlike `resolve-tier-model.sh`'s own dispatch-time fold (an
//! absent/unrecognized marker there is a deliberate SAFE DEFAULT for model
//! selection), this journal field must stay honest about "unobserved" — a
//! routing-evaluation consumer needs the true absence rate, not a default
//! masquerading as data. The end state is held to the same standard: an
//! unread issue is `None`, which classifies as "no forge opinion", never as
//! "still open".

use super::*;

use crate::script_helpers::model_tiers::COMPLEXITY_TIERS;
use crate::script_helpers::sweep_experiment::extract_complexity_marker;
use crate::telemetry::IssueEndState;

/// The pre-build labels a Curator hands an issue BACK to when it rescopes
/// rather than builds it (CLAUDE.md § "Issues Are Suggestions": *"relabel back
/// to `loom:triage`/`loom:curated` if the scope no longer matches"*).
///
/// **Presence alone is NOT a rescope** — see [`PIPELINE_LABELS`]. `loom:curated`
/// in particular is a *persistent milestone marker*: Loom never strips it, so
/// promotion (`loom:issue`) and the Builder's claim (`loom:building`) both
/// leave it in place and essentially every built issue carries it for life.
const RESCOPE_LABELS: [&str; 2] = ["loom:triage", "loom:curated"];

/// The labels that prove the issue is still in — or was restored to — the
/// normal build pipeline, which is what makes a [`RESCOPE_LABELS`] entry on the
/// same issue a stale milestone rather than a hand-back (Issue #9441).
///
/// Both are needed, for two different mechanisms:
///
/// - `loom:building` is the Builder's own claim, and `loom:curated` survives it
///   untouched. Without this exclusion a normally-curated, normally-dispatched
///   issue would report `Rescoped` for its whole life — measured at 5/5 (100%)
///   of open `loom:building` issues on this repo, which would silently divert
///   that entire population into `curator_rescoped` (a *non*-failure bucket
///   exempt from the mandatory-`failure_class` invariant).
/// - `loom:issue` is what the reaper's orphaned-claim recovery restores
///   (`loom:building` → `loom:issue`) before this record is written, so
///   counting it as a rescope would misread every failed sweep as a hand-back.
///
/// What survives: an issue genuinely handed back carries a rescope label and
/// **neither** of these — the Curator's rescope removes `loom:issue` /
/// `loom:building` by definition, since the point is that it is no longer ready
/// to build.
const PIPELINE_LABELS: [&str; 2] = ["loom:issue", "loom:building"];

/// The subset of an issue's forge state this terminal-transition read needs.
/// Every field is independently optional — a partial read is still worth
/// whatever it did resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IssueSignals {
    /// The Curator's `<!-- loom:complexity=<tier> -->` marker, validated
    /// against the closed `mechanical`/`routine`/`complex` vocabulary.
    pub(crate) complexity: Option<String>,
    /// Whether the issue's `state` is `closed`. `None` when the read failed.
    pub(crate) closed: Option<bool>,
    /// When it was closed, for a closed issue that reported a timestamp.
    pub(crate) closed_at: Option<DateTime<Utc>>,
    /// The issue's current label names.
    pub(crate) labels: Vec<String>,
}

impl IssueSignals {
    /// Fold these signals into the [`IssueEndState`] the disposition
    /// classifier consumes, relative to this sweep's own `started_at`.
    ///
    /// `None` whenever the read did not resolve the issue's state at all — the
    /// classifier then falls back to its local-signal arms rather than being
    /// told something untrue.
    ///
    /// A closed issue with **no** `closed_at` (or with no `started_at` to
    /// compare against) is reported as [`IssueEndState::ClosedDuringSweep`]:
    /// the conservative side, since `ClosedBeforeDispatch` asserts the sweep
    /// was pointless and should never be claimed without evidence.
    ///
    /// [`IssueEndState::Rescoped`] is held to the same standard: it needs a
    /// [`RESCOPE_LABELS`] entry **and** the absence of every
    /// [`PIPELINE_LABELS`] entry, because the rescope labels are additive
    /// milestones that outlive the state they once described.
    pub(crate) fn end_state(&self, started_at: Option<DateTime<Utc>>) -> Option<IssueEndState> {
        let closed = self.closed?;
        if closed {
            return Some(match (self.closed_at, started_at) {
                (Some(closed_at), Some(started_at)) if closed_at < started_at => {
                    IssueEndState::ClosedBeforeDispatch
                }
                _ => IssueEndState::ClosedDuringSweep,
            });
        }
        let has_rescope_label = self
            .labels
            .iter()
            .any(|label| RESCOPE_LABELS.contains(&label.as_str()));
        let in_build_pipeline = self
            .labels
            .iter()
            .any(|label| PIPELINE_LABELS.contains(&label.as_str()));
        if has_rescope_label && !in_build_pipeline {
            return Some(IssueEndState::Rescoped);
        }
        Some(IssueEndState::StillOpen)
    }

    /// The Curator's story-point size for this issue (Issue #9432, epic #9429),
    /// folded out of [`Self::labels`] — the label list this read ALREADY
    /// carries for the #9441 end state, so points cost **no** extra forge round
    /// trip and can never describe a different issue than `complexity` does.
    ///
    /// Delegates the vocabulary and the one-`points:*`-label-per-issue guard to
    /// [`crate::story_points::resolve_story_points`], which is also what the
    /// dispatch path resolves `sweep.started`'s value through, so the two
    /// telemetry records cannot disagree about what a legal size is or about
    /// how a defective label set resolves.
    ///
    /// `None` — and therefore an **absent** attribute, never `0` — for an
    /// unsized issue, an out-of-vocabulary label, more than one points label
    /// (logged loudly), and a read that failed or was skipped (which yields
    /// empty [`IssueSignals`], hence no labels at all).
    pub(crate) fn story_points(&self, issue: u32) -> Option<u32> {
        crate::story_points::resolve_story_points(issue, &self.labels)
    }
}

/// Parse the `gh api --jq` projection this module requests into
/// [`IssueSignals`]. A malformed or unexpected payload yields the empty
/// signals rather than an error — same fail-open contract as the fetch.
fn parse_issue_signals(stdout: &str) -> IssueSignals {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return IssueSignals::default();
    };
    let complexity = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .and_then(extract_complexity_marker)
        .filter(|tier| COMPLEXITY_TIERS.contains(tier))
        .map(str::to_string);
    let closed = value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(|state| state.eq_ignore_ascii_case("closed"));
    let closed_at = value
        .get("closed_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let labels = value
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    IssueSignals {
        complexity,
        closed,
        closed_at,
        labels,
    }
}

impl SweepRegistry {
    /// Best-effort issue signals for `issue` — the Curator complexity tier
    /// (Issue #8542) and the issue's end state (Issue #9441) — read via ONE
    /// REST `gh api` call on the independent, larger pool, not the GraphQL one
    /// every agent-side `gh issue view` burns (the same reasoning
    /// [`super::label_timeline::fetch_timeline_signals`] documents for the PR
    /// timeline read alongside this one).
    ///
    /// Skipped outright (never shelling to `gh`) when `skip_label_flip` is
    /// set or the fleet rate-limit breaker is suppressing forge polling,
    /// matching every other real-forge probe on this terminal-transition
    /// path. Empty [`IssueSignals`] on any read or parse failure.
    pub(crate) fn fetch_issue_signals(&self, issue: u32) -> IssueSignals {
        if self.config.skip_label_flip {
            return IssueSignals::default();
        }
        if crate::rate_limit_breaker::global_skip_pass("outcome_journal") {
            log::debug!(
                "sweep_outcomes: skipping the issue #{issue} signal read — the \
                 rate-limit breaker is suppressing forge polling (#8542/#9441)"
            );
            return IssueSignals::default();
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
            // One projection, four signals. Kept to exactly the keys this
            // module consumes so the payload stays small and no unrelated
            // issue content (title, assignees, comment bodies) is ever read
            // into the daemon.
            .arg(
                "{body: .body, state: .state, closed_at: .closed_at, \
                 labels: [(.labels // [])[] | .name]}",
            );
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
                    "sweep_outcomes: issue #{issue} signal read failed ({}) — \
                     omitting complexity/disposition signals, record still written \
                     (#8542/#9441): {stderr}",
                    o.status
                );
                return IssueSignals::default();
            }
            Ok(None) => {
                log::warn!(
                    "sweep_outcomes: issue #{issue} signal read timed out — omitting \
                     complexity/disposition signals, record still written (#8542/#9441)"
                );
                return IssueSignals::default();
            }
            Err(e) => {
                log::warn!(
                    "sweep_outcomes: could not invoke {} for issue #{issue}'s signals: \
                     {e} — omitting complexity/disposition signals, record still written \
                     (#8542/#9441)",
                    gh.display()
                );
                return IssueSignals::default();
            }
        };
        parse_issue_signals(&String::from_utf8_lossy(&output.stdout))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

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

    #[test]
    fn parses_the_full_projection() {
        let signals = parse_issue_signals(
            r#"{"body":"text\n<!-- loom:complexity=complex -->\n","state":"closed",
                "closed_at":"2026-09-29T10:00:00Z","labels":["loom:building","tier:2"]}"#,
        );
        assert_eq!(signals.complexity.as_deref(), Some("complex"));
        assert_eq!(signals.closed, Some(true));
        assert_eq!(signals.closed_at, Some(at("2026-09-29T10:00:00Z")));
        assert_eq!(signals.labels, vec!["loom:building", "tier:2"]);
    }

    #[test]
    fn an_open_issue_has_no_closed_at_and_is_not_closed() {
        let signals =
            parse_issue_signals(r#"{"body":null,"state":"open","closed_at":null,"labels":[]}"#);
        assert_eq!(signals.complexity, None);
        assert_eq!(signals.closed, Some(false));
        assert_eq!(signals.closed_at, None);
        assert_eq!(
            signals.end_state(Some(at("2026-09-29T09:00:00Z"))),
            Some(IssueEndState::StillOpen)
        );
    }

    #[test]
    fn unparseable_output_is_empty_signals_not_an_error() {
        for raw in ["", "not json", "null", "[1,2,3]"] {
            let signals = parse_issue_signals(raw);
            assert_eq!(signals.closed, None, "{raw}");
            assert_eq!(signals.end_state(None), None, "{raw}");
        }
    }

    #[test]
    fn closed_before_dispatch_is_distinguished_from_closed_during() {
        let started = at("2026-09-29T12:00:00Z");
        let before = IssueSignals {
            closed: Some(true),
            closed_at: Some(at("2026-09-20T00:00:00Z")),
            ..IssueSignals::default()
        };
        assert_eq!(before.end_state(Some(started)), Some(IssueEndState::ClosedBeforeDispatch));
        let during = IssueSignals {
            closed: Some(true),
            closed_at: Some(at("2026-09-29T12:30:00Z")),
            ..IssueSignals::default()
        };
        assert_eq!(during.end_state(Some(started)), Some(IssueEndState::ClosedDuringSweep));
    }

    /// Without a `closed_at` (or without a known dispatch instant) the read
    /// cannot prove the sweep was pointless, so it must NOT claim it was.
    #[test]
    fn a_closed_issue_with_no_timestamp_never_claims_closed_before_dispatch() {
        let signals = IssueSignals {
            closed: Some(true),
            closed_at: None,
            ..IssueSignals::default()
        };
        assert_eq!(
            signals.end_state(Some(at("2026-09-29T12:00:00Z"))),
            Some(IssueEndState::ClosedDuringSweep)
        );
        let no_start = IssueSignals {
            closed: Some(true),
            closed_at: Some(at("2026-09-20T00:00:00Z")),
            ..IssueSignals::default()
        };
        assert_eq!(no_start.end_state(None), Some(IssueEndState::ClosedDuringSweep));
    }

    fn open_with(labels: &[&str]) -> IssueSignals {
        IssueSignals {
            closed: Some(false),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            ..IssueSignals::default()
        }
    }

    /// A rescope label on an issue that is NOT in the build pipeline is the
    /// hand-back shape: the Curator's rescope removes `loom:issue` /
    /// `loom:building` by definition.
    #[test]
    fn a_rescope_label_without_a_pipeline_label_is_a_rescope() {
        for label in RESCOPE_LABELS {
            assert_eq!(
                open_with(&[label]).end_state(None),
                Some(IssueEndState::Rescoped),
                "{label}"
            );
        }
        // The full hand-back shape a Curator actually leaves behind: the
        // persistent `loom:curated` milestone plus a fresh `loom:triage`.
        assert_eq!(
            open_with(&["loom:curated", "loom:triage", "tier:goal-supporting"]).end_state(None),
            Some(IssueEndState::Rescoped)
        );
    }

    /// #9441 (Judge finding on PR #9471): the rescope labels are **additive
    /// milestones**, not transient steps — Loom never strips `loom:curated`,
    /// so an issue that was normally curated, promoted and claimed still
    /// carries it. Reading that as a Curator hand-back would divert almost
    /// every built issue's sweep into `curator_rescoped`, a non-failure bucket
    /// exempt from the mandatory-`failure_class` invariant. Measured on this
    /// repo when the defect was found: 5/5 (100%) of open `loom:building`
    /// issues also carried `loom:curated`.
    #[test]
    fn a_pipeline_label_beats_a_persistent_rescope_milestone() {
        for pipeline in PIPELINE_LABELS {
            for rescope in RESCOPE_LABELS {
                assert_eq!(
                    open_with(&[rescope, pipeline]).end_state(None),
                    Some(IssueEndState::StillOpen),
                    "{rescope} + {pipeline}"
                );
            }
        }
        // The real-world label set of a curated, promoted, claimed issue.
        assert_eq!(
            open_with(&["loom:curated", "loom:building", "tier:goal-supporting"]).end_state(None),
            Some(IssueEndState::StillOpen)
        );
    }

    /// #9432: the story-point size comes out of the labels THIS read already
    /// carries — no second fetch — and honors the one-label-per-issue guard.
    /// The three telemetry-visible outcomes, on the real projection shape.
    #[test]
    fn story_points_are_folded_out_of_the_same_projection() {
        let sized = parse_issue_signals(
            r#"{"body":"text","state":"open","closed_at":null,
                "labels":["loom:building","loom:curated","points:5"]}"#,
        );
        assert_eq!(sized.story_points(9432), Some(5));

        // No points label at all: absent, NOT zero.
        let no_points = parse_issue_signals(
            r#"{"body":"text","state":"open","closed_at":null,
                "labels":["loom:building","loom:curated"]}"#,
        );
        assert_eq!(no_points.story_points(9432), None);

        // Stacked labels: logged loudly by `resolve_story_points` and resolved
        // to NO value — never to either of the two candidates.
        let stacked = parse_issue_signals(
            r#"{"body":"text","state":"open","closed_at":null,
                "labels":["points:3","points:8"]}"#,
        );
        assert_eq!(stacked.story_points(9432), None);

        // A failed/skipped read yields empty signals, hence no labels at all —
        // the same absent attribute as an unsized issue, never a fabricated 0.
        assert_eq!(IssueSignals::default().story_points(9432), None);
    }

    /// #9441: the reaper's own orphaned-claim recovery sets `loom:issue`
    /// on every failed sweep BEFORE this record is written — reading it as
    /// a Curator rescope would mislabel the entire failure population.
    #[test]
    fn the_restored_ready_label_is_not_a_rescope() {
        assert_eq!(open_with(&["loom:issue"]).end_state(None), Some(IssueEndState::StillOpen));
        assert_eq!(
            open_with(&["loom:issue", "loom:curated"]).end_state(None),
            Some(IssueEndState::StillOpen)
        );
    }
}
