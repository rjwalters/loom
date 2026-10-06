//! Claimant-liveness evidence for PR-side claims (`loom:reviewing` /
//! `loom:treating`): the claim-activity marker, Judge-progress comments and
//! claimant head force-pushes (Issue #10235), plus the best-effort forge
//! fetches that feed them. Split out of the parent module so it can grow
//! without growing the file-size-ratcheted `claim_reconciliation.rs`; the
//! public items are re-exported there unchanged.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::gh_call;
use super::ClaimedPr;

/// Marker (Issue #4636/#4618) tagging a Judge/Doctor "standing down, not
/// stomping" comment — evidence of *no* progress (a later pass declining to
/// reclaim), not genuine activity. A comment containing this substring is
/// excluded from [`ClaimedPr::most_recent_claim_activity_at`] (Issue #4638),
/// mirroring `defaults/scripts/claim-staleness.sh`'s own stand-down exclusion
/// (a substring match, not an exact marker+claim-timestamp match, so any
/// stand-down comment for any claim generation is excluded).
pub const STANDDOWN_MARKER_PREFIX: &str = "<!-- loom:standdown claim=";

/// Marker (Issue #6514, adopted daemon-side by #6523) a **claimant** appends to
/// its own progress comments to prove it is still alive:
///
/// ```text
/// <!-- loom:claim-activity claim=<CLAIMED_AT> -->
/// ```
///
/// This is the single shared definition on the Rust side, and it must stay
/// byte-identical to `ACTIVITY_PREFIX` in `defaults/scripts/claim-staleness.sh`
/// — the agent-side evaluator judge.md / doctor.md / curator.md drive, whose
/// `marker` subcommand prints exactly the string [`claim_activity_marker`]
/// builds. Both sides match on the claim's **own** `labeled`-event timestamp,
/// so a marker left behind by an earlier claim generation can never refresh a
/// later one.
///
/// Distinct from [`STANDDOWN_MARKER_PREFIX`] (a *later* pass declining to
/// reclaim — evidence of no progress) and from [`VERDICT_MARKER_PREFIX`]
/// (which tree a verdict describes).
pub const CLAIM_ACTIVITY_MARKER_PREFIX: &str = "<!-- loom:claim-activity claim=";

/// Render the full claim-activity marker for a claim labeled at `claimed_at` —
/// the exact string `claim-staleness.sh marker` prints, and the substring
/// [`most_recent_claim_activity_at`] requires a comment to contain before it
/// counts as claimant liveness.
///
/// The timestamp is rendered RFC-3339 with second precision and a `Z` suffix,
/// which is how the forge emits a timeline event's `created_at` and therefore
/// how `claim-staleness.sh` (which interpolates that field verbatim, having
/// validated it against `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`) renders it.
#[must_use]
pub fn claim_activity_marker(claimed_at: DateTime<Utc>) -> String {
    format!(
        "{CLAIM_ACTIVITY_MARKER_PREFIX}{} -->",
        claimed_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// One comment on a claimed PR, trimmed to the two fields the claim-activity
/// scan needs. Constructed by [`forge::fetch_most_recent_claim_activity_at`]
/// from `gh pr view --json comments`, and directly by the unit tests — the
/// predicate itself ([`most_recent_claim_activity_at`]) is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrComment {
    pub created_at: DateTime<Utc>,
    pub body: String,
}

/// The timestamp of the most recent **claimant activity** comment posted after
/// `claimed_at` — the pure predicate behind
/// [`ClaimedPr::most_recent_claim_activity_at`] (Issue #6523).
///
/// A comment counts iff all three hold, mirroring
/// `defaults/scripts/claim-staleness.sh` exactly:
///
/// 1. it was posted strictly after `claimed_at` (the claim's own `labeled`
///    event — anything at or before it belongs to a previous claim
///    generation);
/// 2. it carries [`claim_activity_marker`]`(claimed_at)` — the claim-activity
///    marker for **this** claim, not merely some claim;
/// 3. it is not a stand-down comment ([`STANDDOWN_MARKER_PREFIX`]).
///
/// Condition 3 is redundant against a well-formed stand-down comment (which
/// carries no activity marker) and is kept deliberately: it is the #4618
/// regression guard, and a belt-and-braces exclusion costs nothing if a future
/// stand-down body ever quotes an activity marker verbatim.
///
/// `None` when nothing qualifies — callers then anchor on `claim_labeled_at` /
/// `updated_at` alone, i.e. an unrelated comment does not postpone reclamation
/// at all.
#[must_use]
pub fn most_recent_claim_activity_at(
    comments: &[PrComment],
    claimed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let marker = claim_activity_marker(claimed_at);
    comments
        .iter()
        .filter(|c| c.created_at > claimed_at)
        .filter(|c| !c.body.contains(STANDDOWN_MARKER_PREFIX))
        .filter(|c| c.body.contains(&marker))
        .map(|c| c.created_at)
        .max()
}

/// Prefixes of the structured comments only a Judge posts while reviewing
/// (Issue #10235). A trusted-author comment carrying one, posted after the
/// claim, is Judge progress even when it carries no
/// [`claim_activity_marker`]: a Judge that merged `main`, re-ran suites or
/// recorded a fast-path evaluation is demonstrably alive. Deliberately NOT
/// including [`STANDDOWN_MARKER_PREFIX`] (a *later* pass declining to act).
pub const JUDGE_ACTIVITY_MARKER_PREFIXES: &[&str] = &[
    "<!-- loom:verdict-sha",
    "<!-- loom:review-reconciliation",
    "<!-- loom:ac-verified",
    "<!-- loom:fast-track-evaluation",
    "<!-- loom:docs-fast-path-evaluation",
    "<!-- loom:fallback-evaluated",
];

/// The newest trusted Judge-progress comment posted after `claimed_at`
/// (Issue #10235) — see [`JUDGE_ACTIVITY_MARKER_PREFIXES`]. Pure; the caller
/// supplies only trusted-author comments and only asks for it on a
/// `loom:reviewing` claim. Stand-down comments are excluded (#4618).
#[must_use]
pub fn most_recent_judge_activity_at(
    comments: &[PrComment],
    claimed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    comments
        .iter()
        .filter(|c| c.created_at > claimed_at)
        .filter(|c| !c.body.contains(STANDDOWN_MARKER_PREFIX))
        .filter(|c| {
            JUDGE_ACTIVITY_MARKER_PREFIXES
                .iter()
                .any(|p| c.body.contains(p))
        })
        .map(|c| c.created_at)
        .max()
}

/// One PR timeline event, trimmed to what the head-push scan needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    /// The timeline `event` name (`labeled`, `head_ref_force_pushed`, ...).
    pub event: String,
    pub created_at: DateTime<Utc>,
    /// The acting login, when the forge reported one.
    pub actor: Option<String>,
}

/// The newest force-push of the PR head made by the **claimant** after
/// `claimed_at` (Issue #10235): a `head_ref_force_pushed` event whose actor is
/// the actor of the claim's own `labeled` event (the one at `claimed_at`).
///
/// Only force-pushes count: a Judge that rebases/merges `main` before waiting
/// on CI force-pushes, whereas a Builder's ordinary follow-up commits do not
/// emit that event. Requiring the pusher to equal the claim-labeler stops a
/// differently-identified Builder/Doctor/stranger push from extending the
/// claim. (Fleets that run every role under ONE identity cannot distinguish
/// roles by actor; there the force-push-only restriction is the guard.)
/// `None` when the claimant cannot be identified — fail-open to the old rule.
#[must_use]
pub fn most_recent_head_push_at(
    events: &[TimelineEvent],
    claimed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let claimant = events
        .iter()
        .find(|e| e.event == "labeled" && e.created_at == claimed_at)?
        .actor
        .as_deref()?;
    events
        .iter()
        .filter(|e| e.event == "head_ref_force_pushed" && e.created_at > claimed_at)
        .filter(|e| e.actor.as_deref() == Some(claimant))
        .map(|e| e.created_at)
        .max()
}

/// Which liveness evidence anchored a PR claim's age gate (Issue #10235).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessSignal {
    /// The claim label's own `labeled` event — no later evidence.
    LabelAge,
    /// A comment carrying this claim's [`claim_activity_marker`].
    ClaimActivityMarker,
    /// A trusted Judge-progress comment ([`JUDGE_ACTIVITY_MARKER_PREFIXES`]).
    JudgeComment,
    /// A force-push of the head by the claimant ([`most_recent_head_push_at`]).
    HeadPush,
    /// Only the PR's aggregate `updatedAt` (fail-open fallback).
    UpdatedAt,
    /// No age evidence at all.
    None,
}

/// The freshest liveness anchor for `pr` and the signal that supplied it. Ties
/// prefer the earlier-listed (more specific/authoritative) signal.
#[must_use]
pub fn pr_liveness(pr: &ClaimedPr) -> (Option<DateTime<Utc>>, LivenessSignal) {
    let mut best = match (pr.claim_labeled_at, pr.updated_at) {
        (Some(a), _) => (Some(a), LivenessSignal::LabelAge),
        (None, Some(u)) => (Some(u), LivenessSignal::UpdatedAt),
        (None, None) => (None, LivenessSignal::None),
    };
    for (at, sig) in [
        (pr.most_recent_claim_activity_at, LivenessSignal::ClaimActivityMarker),
        (pr.most_recent_judge_activity_at, LivenessSignal::JudgeComment),
        (pr.most_recent_head_push_at, LivenessSignal::HeadPush),
    ] {
        if let Some(t) = at {
            // A signal only beats a `UpdatedAt` fallback / `None` or a strictly older anchor.
            if best.0.is_none_or(|b| t > b) {
                best = (Some(t), sig);
            }
        }
    }
    best
}

/// Best-effort fetch of the most recent **claimant activity** comment
/// posted on `pr_number` after `since` (Issue #4638, narrowed by #6523) —
/// [`ClaimedPr::most_recent_claim_activity_at`], the evidence
/// [`decide_pr`]'s age gate uses alongside `claim_labeled_at` to avoid
/// reclaiming a genuinely live, non-pid-joinable claimant.
///
/// The `gh` call only *narrows* (comments posted after `since`, to bound
/// the payload); the decision itself is the pure
/// [`most_recent_claim_activity_at`], so the exact rule that ships is the
/// one the unit tests exercise. Returns `None` on any
/// failure/timeout/unparseable-output, or when no comment since `since`
/// carried this claim's [`claim_activity_marker`] — callers then
/// fall back to `claim_labeled_at`/`updated_at` alone, preserving the
/// #4618 regression guard (a claim with no claimant heartbeat since must
/// still age out and be reclaimed).
///
/// #9548: the REST listing (not `gh pr view --json comments`, which cannot
/// name an App author), and only trusted authors' comments count: an
/// outsider's activity marker cannot keep a dead claim alive.
fn fetch_trusted_comments_since(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    since: DateTime<Utc>,
) -> Option<Vec<PrComment>> {
    // Render `since` in exactly the shape the forge emits for `created_at`
    // (`...Z`, second precision) so the jq `>` comparison — which is a raw
    // *string* comparison — orders correctly. `to_rfc3339()` would render
    // the same instant with a `+00:00` offset suffix, which sorts *before*
    // a `Z`-suffixed timestamp of the identical second and so would
    // misclassify a comment posted in the same second as the claim label.
    // (This is also the exact rendering `claim_activity_marker` embeds.)
    let since_iso = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // `since=` lets the forge skip older comments (it filters on
    // `updated_at`, a superset of the `created_at` filter below).
    let path = format!(
        "repos/{{owner}}/{{repo}}/issues/{pr_number}/comments?per_page=100&since={since_iso}"
    );
    let jq = format!(
        r#".[] | select(.created_at > "{since_iso}") | {{created_at, body, {}}}"#,
        crate::comment_trust::records::AUTHOR_JQ
    );
    let out =
        gh_call::ok_stdout(gh_call::read("claim.pr_activity_comments", gh_bin, root).args([
            "api",
            &path,
            "--paginate",
            "--jq",
            &jq,
        ]))?;
    let rows = crate::comment_trust::TrustPolicy::for_root(root).trusted_records(&out)?;
    let comments: Vec<PrComment> = rows
        .iter()
        .filter_map(|r| {
            Some(PrComment {
                created_at: crate::comment_trust::records::max_timestamp(
                    std::slice::from_ref(r),
                    "created_at",
                )?,
                body: r.get("body")?.as_str().unwrap_or_default().to_string(),
            })
        })
        .collect();
    Some(comments)
}

pub(crate) fn fetch_most_recent_claim_activity_at(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    since: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    most_recent_claim_activity_at(
        &fetch_trusted_comments_since(gh_bin, root, pr_number, since)?,
        since,
    )
}

/// Best-effort newest trusted Judge-progress comment since `since`
/// (Issue #10235); same trusted-author listing as the claim-activity scan.
pub(crate) fn fetch_most_recent_judge_activity_at(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    since: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    most_recent_judge_activity_at(
        &fetch_trusted_comments_since(gh_bin, root, pr_number, since)?,
        since,
    )
}

/// Best-effort newest claimant force-push of the PR head since `since`
/// (Issue #10235), from the issue timeline. `None` on any failure.
pub(crate) fn fetch_most_recent_head_push_at(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    label: &str,
    since: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/timeline?per_page=100");
    let jq = format!(
        r#".[] | select((.event == "labeled" and .label.name == "{label}") or .event == "head_ref_force_pushed") | {{event, created_at, actor: .actor.login}}"#
    );
    let out = gh_call::ok_stdout(gh_call::read("claim.pr_head_push", gh_bin, root).args([
        "api",
        &path,
        "--paginate",
        "--jq",
        &jq,
    ]))?;
    let events: Vec<TimelineEvent> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            Some(TimelineEvent {
                event: v.get("event")?.as_str()?.to_string(),
                created_at: chrono::DateTime::parse_from_rfc3339(v.get("created_at")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc),
                actor: v
                    .get("actor")
                    .and_then(|a| a.as_str())
                    .map(ToString::to_string),
            })
        })
        .collect();
    most_recent_head_push_at(&events, since)
}

/// The newest liveness evidence the daemon honors *beyond* the claim-activity
/// marker, for the in-session evaluator (`claim-staleness.sh`, via
/// `loom-daemon forge claim-liveness`): a trusted Judge-progress comment (only
/// for a `loom:reviewing` claim) or a claimant head force-push, whichever is
/// newer, after `claimed_at`. Runs the exact fetch+predicate pairs
/// `decide_pr`'s anchor uses, so the shell and the daemon cannot disagree on
/// what keeps a claim alive (Issue #10235). `None` when neither exists or a
/// read failed — the caller then keeps the marker-only age.
#[must_use]
pub fn extra_liveness_at(
    root: &Path,
    pr_number: u32,
    label: &str,
    claimed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let gh_bin = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
    let judge = (label == "loom:reviewing")
        .then(|| fetch_most_recent_judge_activity_at(&gh_bin, root, pr_number, claimed_at))
        .flatten();
    judge.max(fetch_most_recent_head_push_at(&gh_bin, root, pr_number, label, claimed_at))
}
