//! Issue-side `loom:curating` claim reconciliation (Issue #14816, parent
//! #14811).
//!
//! The pre-#14815 roll writer requeued an interrupted Curator step through
//! the **Builder** restore: it added `loom:issue`, left `loom:curating`
//! behind, and posted a "the claim was released" comment that was not true
//! of the forge state. Three live instances of that shape (recorded in
//! #14811) then sat undrawn by any role — the work finder's claim gate
//! passes them as `claimed` (`work_finder/step_item.rs`,
//! `PassReason::Claimed`), so a stale Curator claim label is a permanent
//! stall, exactly like a stale `loom:building` one. The writer is fixed
//! (#14815); this module is the recovery path for the claims already on
//! open issues.
//!
//! ## Evidence and decision
//!
//! Discovery is the same ETag-cached REST listing the `loom:building` pass
//! uses, keyed on `loom:curating`. Per issue, the pass needs THREE facts,
//! each with its own failure posture:
//!
//! 1. **The Curator lease** — the role-keyed `lease pr` family
//!    (`<!-- loom:pr-lease role=curator … -->`, `sweep_registry/
//!    step_lease_owner.rs`) the dispatch path takes and renews. This is the
//!    only *trusted* liveness source: liveness is the record's forge-assigned
//!    `updated_at` within the shared lease TTL
//!    ([`super::resolve_lease_ttl_minutes`]), a yield record kills a lease,
//!    and only a **trusted author's** record counts (#9548 — an outsider's
//!    reads as absent, never as a live claim holding the issue). The read
//!    distinguishes *failed* from *found-nothing* exactly like
//!    [`super::forge::LeaseProbe`] does for the Builder lease (#7591).
//! 2. **The claim's age**, from the label's own most recent `labeled`
//!    timeline event ([`super::forge::fetch_claim_labeled_at`], the #4618
//!    lesson: never the issue's aggregate `updated_at`, which any comment
//!    bumps), gated by [`crate::claim_liveness::stale_after_minutes`] for
//!    `loom:curating` — the registry's 45-minute threshold, the SAME
//!    threshold `claim-staleness.sh` arbitrates the agent-side double-claim
//!    race with. One threshold, both surfaces; the daemon never strips a
//!    claim the agent-side arbitration window still protects.
//! 3. **The current labels**, from the discovery listing (and re-read fresh
//!    immediately before any write).
//!
//! A claim is released only when **both** say so: the lease record is
//! released, expired or yielded (a record must exist — **absence is not
//! evidence**: the attended Curator path of `curator.md` takes no lease at
//! all, and `claim-staleness.sh` owns that case), AND the label has been
//! stale past the arbitration threshold. Everything else — a live lease,
//! an unverifiable read, no record, an unknown age, a fresh claim, a frozen
//! issue — keeps the claim untouched. [#11743] applies: a `loom:frozen`
//! issue gets no write at all, not even the claim removal.
//!
//! The release itself reuses the resume side's canonical per-role planner
//! ([`plan_step_release`] for [`StepRole::Curator`], #11898) against a fresh
//! label read: remove `loom:curating` only when the item already advanced
//! (`loom:curated`/`loom:issue`) or is held/closed, remove-and-restore
//! `loom:triage` otherwise. Later approvals, holds and unrelated labels are
//! preserved by construction — each edit touches only its own label. A restore
//! confirms triage before removing the claim so an interrupted restore stays
//! discoverable. Re-read lease, claim age and issue state after any first write.
//!
//! ## The re-curation record
//!
//! `loom:issue` on a still-`loom:curating` item is the incident's signature:
//! either a later, earned approval or the roll-requeue's unearned injection,
//! and the label set alone cannot tell them apart. Where history cannot
//! establish whether the approval was earned, the pass **records that the
//! issue needs re-curation** (one marker comment,
//! [`RECURATION_MARKER_PREFIX`]) rather than guessing the pre-claim state —
//! it never removes or adds an approval label. The record must be confirmed
//! before the claim is removed. An unconfirmed record leaves the claim in the
//! listing for the next pass. A confirmed record survives a failed label edit
//! without being posted again; a completed repair leaves the listing.
//!
//! Runs from [`super::pass_loop`] inside the shared reconciliation cadence
//! and its `LOOM_STALE_CLAIM_RECONCILE` kill switch — no separate knob, per
//! #14816's scope (no new config keys or flags) — self-logging like
//! `building_heal` (no stats plumbing).

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::step_role::StepRole;
use crate::sweep_registry::restore_to_ready::step_restore::{plan_step_release, StepReleasePlan};
use crate::sweep_registry::step_lease_owner::{
    earliest_live_holder, parse_role_lease, RoleLeaseKind, RoleLeaseRow,
};

use super::gh_call;

/// The claim label this pass reconciles (also
/// `StepRole::Curator.claim_label()`).
const CLAIM: &str = "loom:curating";
const READY: &str = "loom:triage";

/// The role token the trusted lease family keys on (`lease pr claim --role
/// curator`); the first token after `role=` in every record of the family.
const ROLE: &str = "curator";

/// Fallback staleness threshold when the label registry has no
/// `stale_after_minutes` for `loom:curating` — it does (45, pinned from
/// `claim-staleness.sh` by `claim_liveness`'s registry test), so this only
/// defends a registry regression from turning into a zero-minute floor.
const DEFAULT_STALE_CURATING_MINUTES: f64 = 45.0;

/// First-line marker of the re-curation record this pass posts. Readers
/// locate it via this literal prefix only, like every marker convention in
/// this module family.
pub(super) const RECURATION_MARKER_PREFIX: &str = "<!-- loom:recuration-needed";

// ---------------------------------------------------------------------------
// Pure: lease evidence
// ---------------------------------------------------------------------------

/// What the trusted Curator-lease read established about one claim. The
/// forge-side read-failure case is handled by the caller BEFORE
/// classification (never an enum arm here), mirroring
/// [`super::forge::LeaseProbe::ReadFailed`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum CuratorLeaseEvidence {
    /// A trusted, in-TTL, un-yielded `role=curator` lease exists — the
    /// holder is live on some host. The claim is never touched.
    Live { age_minutes: f64 },
    /// The newest trusted record says `released` — the holder finished and
    /// the claim outlived its release.
    Released { age_minutes: f64 },
    /// A lease record exists but has not been renewed within the TTL — the
    /// holder is presumed gone by the one signal every host shares.
    Expired { age_minutes: f64 },
    /// The claimant stood down to an earlier holder and no live lease
    /// remains.
    Yielded,
    /// The read succeeded and found no trusted `role=curator` record at all.
    /// NOT evidence of abandonment: the attended Curator path takes no lease
    /// (`curator.md`), and `claim-staleness.sh` arbitrates that case on the
    /// next dispatch.
    Absent,
}

impl std::fmt::Display for CuratorLeaseEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live { age_minutes } => {
                write!(f, "curator_lease=live (renewed {age_minutes:.1}m ago)")
            }
            Self::Released { age_minutes } => {
                write!(f, "curator_lease=released (record updated {age_minutes:.1}m ago)")
            }
            Self::Expired { age_minutes } => write!(
                f,
                "curator_lease=expired (last renewed {age_minutes:.1}m ago, past the TTL)"
            ),
            Self::Yielded => write!(f, "curator_lease=yielded (claimant stood down)"),
            Self::Absent => write!(
                f,
                "curator_lease=absent (no trusted role=curator record — not evidence of \
                 abandonment; claim-staleness.sh owns the no-lease case)"
            ),
        }
    }
}

/// Classify one issue's trusted Curator-lease records. Pure; the liveness
/// rule (in-TTL `Lease` record not followed by a later yield of the same
/// identity) is [`earliest_live_holder`]'s, reused rather than restated.
pub(super) fn classify_curator_lease(
    rows: &[RoleLeaseRow],
    now: DateTime<Utc>,
    ttl_minutes: f64,
) -> CuratorLeaseEvidence {
    let ttl = chrono::Duration::seconds((ttl_minutes * 60.0) as i64);
    let age_of = |r: &RoleLeaseRow| (now - r.updated_at).num_seconds() as f64 / 60.0;
    let of_role = |r: &RoleLeaseRow| parse_role_lease(&r.body).is_some_and(|p| p.role == ROLE);
    if earliest_live_holder(rows, ROLE, now, ttl).is_some() {
        // The freshest in-TTL lease row names the age (cosmetic — for the
        // log line only; the live/not-live answer above is the authority,
        // and it guarantees at least one such row exists).
        let age_minutes = rows
            .iter()
            .filter(|r| of_role(r))
            .filter(|r| parse_role_lease(&r.body).is_some_and(|p| p.kind == RoleLeaseKind::Lease))
            .map(age_of)
            .filter(|age| *age < ttl_minutes)
            .fold(f64::INFINITY, f64::min);
        return CuratorLeaseEvidence::Live { age_minutes };
    }
    // No live lease: name what the newest trusted record of the role says.
    rows.iter()
        .filter(|r| of_role(r))
        .max_by_key(|r| (r.updated_at, r.id))
        .map_or(CuratorLeaseEvidence::Absent, |newest| {
            let age_minutes = age_of(newest);
            match parse_role_lease(&newest.body).map(|p| p.kind) {
                Some(RoleLeaseKind::Released) => CuratorLeaseEvidence::Released { age_minutes },
                Some(RoleLeaseKind::Yield) => CuratorLeaseEvidence::Yielded,
                _ => CuratorLeaseEvidence::Expired { age_minutes },
            }
        })
}

// ---------------------------------------------------------------------------
// Pure: the decision
// ---------------------------------------------------------------------------

/// Why a `loom:curating` claim was left alone. Every variant is a
/// deliberate keep — none of the uncertain or live cases permits stripping
/// another Curator's claim. (The unreadable-lease case is refused by the
/// forge-side caller BEFORE the decision, never an enum arm here — the same
/// place [`super::forge::LeaseProbe::ReadFailed`] is handled for the
/// building pass.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum CuratingKeepReason {
    /// `loom:frozen`: no write at all, not even the claim removal (#11743).
    FrozenItem,
    /// A trusted, live Curator lease backs the claim.
    LeaseLive { age_minutes: f64 },
    /// No trusted lease record exists (the attended path takes none).
    NoLeaseRecord,
    /// The claim's `labeled` timestamp could not be read.
    ClaimAgeUnknown,
    /// The claim is younger than the arbitration threshold
    /// (`stale_after_minutes`), stale lease or not.
    ClaimTooFresh {
        age_minutes: f64,
        stale_minutes: f64,
    },
    /// The claim label is gone from the (fresh) label set — a concurrent
    /// repair already completed; nothing to do.
    ClaimAlreadyGone,
}

/// What the pass does with one discovered `loom:curating` claim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum CuratingAction {
    Keep(CuratingKeepReason),
    /// Remove `loom:curating` only; `recuration_record` says whether the
    /// approval-provenance record must be posted.
    RemoveClaim {
        recuration_record: bool,
    },
    /// Remove `loom:curating` and restore `loom:triage`.
    Restore,
}

/// Decide one claim. Pure, total, fully unit-testable — the forge-side
/// caller supplies the inputs and re-derives the write against a fresh
/// label read before touching anything.
///
/// `labels` are the issue's current labels (from the discovery listing),
/// `claim_labeled_at` the claim label's own most recent `labeled` event
/// (`None` when unreadable), `lease` the classified trusted-lease evidence,
/// `stale_minutes` the resolved `stale_after_minutes` for `loom:curating`.
pub(super) fn decide_curating_claim(
    labels: &[String],
    claim_labeled_at: Option<DateTime<Utc>>,
    lease: CuratorLeaseEvidence,
    now: DateTime<Utc>,
    stale_minutes: f64,
) -> CuratingAction {
    use CuratingAction as A;
    use CuratingKeepReason as K;
    if crate::work_finder::is_frozen(labels) {
        return A::Keep(K::FrozenItem);
    }
    match lease {
        CuratorLeaseEvidence::Live { age_minutes } => {
            return A::Keep(K::LeaseLive { age_minutes });
        }
        CuratorLeaseEvidence::Absent => return A::Keep(K::NoLeaseRecord),
        // Released / Expired / Yielded fall through to the age gate.
        _ => {}
    }
    let Some(labeled_at) = claim_labeled_at else {
        return A::Keep(K::ClaimAgeUnknown);
    };
    let age_minutes = (now - labeled_at).num_seconds() as f64 / 60.0;
    if age_minutes < stale_minutes {
        return A::Keep(K::ClaimTooFresh {
            age_minutes,
            stale_minutes,
        });
    }
    match plan_step_release(StepRole::Curator, labels, Some(false)) {
        StepReleasePlan::Nothing => A::Keep(K::ClaimAlreadyGone),
        StepReleasePlan::RemoveClaim => A::RemoveClaim {
            recuration_record: needs_recuration(labels),
        },
        StepReleasePlan::Restore => A::Restore,
    }
}

/// Whether the approval's provenance on a released claim cannot be
/// established from the label set: an `loom:issue` without the
/// `loom:curated` a completed curation leaves behind is either a later,
/// earned approval or the #14811 requeue's unearned injection, and the
/// labels alone cannot tell them apart. (`loom:triage` beside them is an
/// unrelated earlier label and changes nothing.)
fn needs_recuration(labels: &[String]) -> bool {
    let has = |name: &str| labels.iter().any(|l| l == name);
    has("loom:issue") && !has("loom:curated")
}

/// The re-curation record's body. Posted BEFORE the label edit so a partial
/// repair (record posted, edit failed) retries the edit without duplicating
/// the record on the next pass.
fn recuration_record_body(lease: &CuratorLeaseEvidence, now: DateTime<Utc>) -> String {
    format!(
        "{RECURATION_MARKER_PREFIX} claim={CLAIM} -->\n\
         Loom's claim reconciliation found a stale `{CLAIM}` claim on this issue at \
         {now} ({lease}).\n\n\
         The issue also carries `loom:issue`, and the claim's history cannot establish \
         whether that approval was earned before the claim went stale. The reconciler does not \
         change approval labels. **A Curator must re-run curation, or a \
         maintainer must confirm the approval, before this issue is built.**\n\n\
         See #14816 (parent #14811) for the incident and the recovery design."
    )
}

// ---------------------------------------------------------------------------
// Forge glue
// ---------------------------------------------------------------------------

/// One trusted-lease comments read: the `role=curator` records of the
/// `loom:pr-lease` family, plus whether a re-curation record already exists.
pub(super) enum CuratorLeaseRead {
    Records {
        rows: Vec<RoleLeaseRow>,
        recuration_recorded: bool,
    },
    /// The comments read itself failed — carries no information about the
    /// lease (never treated as absence).
    ReadFailed,
}

/// Read one issue's trusted Curator-lease records and any existing
/// re-curation record in a single REST comments walk. Only a trusted
/// author's records count (#9548); an untrusted record reads as absent.
fn fetch_curator_lease_read(gh_bin: &Path, root: &Path, issue: u32) -> CuratorLeaseRead {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments?per_page=100");
    let jq = format!(
        ".[] | select(.body | startswith(\"<!-- loom:pr-lease role={ROLE}\") or \
         startswith(\"<!-- loom:pr-lease-yield role={ROLE}\") or \
         startswith(\"<!-- loom:pr-lease-released role={ROLE}\") or \
         startswith(\"{RECURATION_MARKER_PREFIX}\")) | {{id, updated_at, body, {}}}",
        crate::comment_trust::records::AUTHOR_JQ
    );
    let Some(stdout) =
        gh_call::ok_stdout(gh_call::read("claim.curating_comments", gh_bin, root).args([
            "api",
            &path,
            "--paginate",
            "--jq",
            &jq,
        ]))
    else {
        return CuratorLeaseRead::ReadFailed;
    };
    let Some(trusted) = crate::comment_trust::TrustPolicy::for_root(root).trusted_records(&stdout)
    else {
        return CuratorLeaseRead::ReadFailed;
    };
    let mut rows = Vec::new();
    let mut recuration_recorded = false;
    for v in trusted {
        let Some(body) = v.get("body").and_then(|b| b.as_str()) else {
            continue;
        };
        if body.starts_with(RECURATION_MARKER_PREFIX) {
            recuration_recorded = true;
            continue;
        }
        let (Some(id), Some(at)) = (
            v.get("id").and_then(|i| i.as_u64()),
            v.get("updated_at")
                .and_then(|s| s.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok()),
        ) else {
            continue;
        };
        rows.push(RoleLeaseRow {
            id,
            updated_at: at.with_timezone(&Utc),
            body: body.to_string(),
        });
    }
    CuratorLeaseRead::Records {
        rows,
        recuration_recorded,
    }
}

/// One reconciliation pass over one workspace's open `loom:curating`
/// claims. Best-effort and bounded exactly like the issue-side building
/// pass: any `gh` failure for one issue is logged at `warn` and skipped,
/// never propagated (one repo's forge hiccup must not block the daemon).
/// Self-logging; returns nothing (see the module docs for why).
pub(super) fn reconcile_curating_claims(gh_bin: &Path, root: &Path) {
    let listing = crate::forge_listing::list_issues_only_cached_as(
        "claim_reconciliation",
        gh_bin,
        Some(root),
        None,
        CLAIM,
        "open",
    );
    let issues = match listing {
        Ok(v) => v,
        Err(e) => {
            log::warn!("claim_reconciliation: curating pass {}: {e}", root.display());
            crate::rate_limit_breaker::global_observe_failure(
                &e.to_string(),
                "claim_reconciliation",
            );
            return;
        }
    };
    // Defense in depth against a claim-label backlog turning one tick into a
    // `gh`-API storm — same bound as the building pass.
    let issues: Vec<(u32, Vec<String>)> = issues
        .into_iter()
        .take(super::MAX_ISSUES_PER_WORKSPACE as usize)
        .map(|r| (r.number, r.labels))
        .collect();
    if issues.is_empty() {
        return;
    }
    let now = Utc::now();
    let stale_minutes =
        crate::claim_liveness::stale_after_minutes(CLAIM).unwrap_or(DEFAULT_STALE_CURATING_MINUTES);
    let ttl_minutes = super::resolve_lease_ttl_minutes();
    let mut checked = 0usize;
    let mut released = 0usize;
    for (number, labels) in issues {
        checked += 1;
        if !labels.iter().any(|label| label == CLAIM) || crate::work_finder::is_frozen(&labels) {
            continue;
        }
        let Some(labeled_at) = super::forge::fetch_claim_labeled_at(gh_bin, root, number, CLAIM)
        else {
            continue;
        };
        if (now - labeled_at).num_seconds() as f64 / 60.0 < stale_minutes {
            continue;
        }
        let confirm =
            || read_current_release(gh_bin, root, number, labeled_at, stale_minutes, ttl_minutes);
        let Some(release) = confirm() else {
            continue;
        };
        match release.plan {
            StepReleasePlan::Nothing => continue,
            StepReleasePlan::RemoveClaim if release.record_needed => {
                if let Err(e) = post_recuration_record(gh_bin, root, number, &release.lease, now) {
                    log::warn!(
                        "claim_reconciliation: could not confirm the re-curation record for \
                         #{number} in {}: {e}; keeping {CLAIM} for retry (#14816)",
                        root.display(),
                    );
                    continue;
                }
                // A confirmed comment does not establish that the same claim
                // is still stale. A renewal can land during the POST.
                if !confirm().is_some_and(|current| current.plan == StepReleasePlan::RemoveClaim) {
                    continue;
                }
            }
            StepReleasePlan::Restore if !release.triage_present => {
                // Keep the claim in discovery until the first write is
                // confirmed. A rejected or lost response can be retried.
                if let Err(e) = run_issue_edit(gh_bin, root, number, "--add-label", READY) {
                    log::warn!(
                        "claim_reconciliation: could not restore {READY} on #{number} in {}: {e}; \
                         keeping {CLAIM} for retry (#14816)",
                        root.display(),
                    );
                    continue;
                }
                if !confirm().is_some_and(|current| {
                    current.plan == StepReleasePlan::Restore && current.triage_present
                }) {
                    continue;
                }
            }
            _ => {}
        }
        match run_issue_edit(gh_bin, root, number, "--remove-label", CLAIM) {
            Ok(()) => {
                released += 1;
                log::warn!(
                    "claim_reconciliation: released stale {CLAIM} from #{number} in {} \
                     ({}, plan={:?}) (#14816)",
                    root.display(),
                    release.lease,
                    release.plan,
                );
            }
            Err(e) => {
                log::warn!(
                    "claim_reconciliation: failed to release {CLAIM} from #{number} in {}: {e}",
                    root.display(),
                );
            }
        }
    }
    if released > 0 {
        log::info!(
            "claim_reconciliation: curating pass checked {checked} {CLAIM} issue(s) in {}, \
             released {released} stale claim(s) (#14816)",
            root.display(),
        );
    } else {
        log::debug!(
            "claim_reconciliation: curating pass checked {checked} {CLAIM} issue(s) in {}, \
             nothing to release",
            root.display(),
        );
    }
}

struct CurrentRelease {
    plan: StepReleasePlan,
    lease: CuratorLeaseEvidence,
    record_needed: bool,
    triage_present: bool,
}

/// Recheck the same claim and read current trusted lease evidence just
/// before a write. A new claim, renewed lease, uncertain read or frozen
/// item keeps the claim discoverable for another pass.
fn read_current_release(
    gh_bin: &Path,
    root: &Path,
    issue: u32,
    expected_labeled_at: DateTime<Utc>,
    stale_minutes: f64,
    ttl_minutes: f64,
) -> Option<CurrentRelease> {
    let labels = match super::forge::issue_label_names(gh_bin, root, issue) {
        Ok(labels) => labels,
        Err(error) => {
            log::warn!(
                "claim_reconciliation: could not read #{issue} in {} before releasing \
                 {CLAIM}: {error}; retrying next pass (#14816)",
                root.display(),
            );
            return None;
        }
    };
    if !labels.iter().any(|label| label == CLAIM) || crate::work_finder::is_frozen(&labels) {
        return None;
    }
    let closed = super::forge::issue_is_confirmed_closed(gh_bin, root, issue);
    let labeled_at = super::forge::fetch_claim_labeled_at(gh_bin, root, issue, CLAIM);
    if labeled_at != Some(expected_labeled_at) {
        return None;
    }
    let CuratorLeaseRead::Records {
        rows,
        recuration_recorded,
    } = fetch_curator_lease_read(gh_bin, root, issue)
    else {
        log::warn!(
            "claim_reconciliation: could not verify the Curator lease on #{issue} in {} before \
             releasing {CLAIM}; retrying next pass (#14816)",
            root.display(),
        );
        return None;
    };
    let now = Utc::now();
    let lease = classify_curator_lease(&rows, now, ttl_minutes);
    if let CuratingAction::Keep(reason) =
        decide_curating_claim(&labels, labeled_at, lease, now, stale_minutes)
    {
        log::debug!(
            "claim_reconciliation: keeping {CLAIM} on #{issue} in {} — {reason:?} ({lease}) (#14816)",
            root.display(),
        );
        return None;
    }
    Some(CurrentRelease {
        plan: plan_step_release(StepRole::Curator, &labels, Some(closed)),
        lease,
        record_needed: needs_recuration(&labels) && !recuration_recorded,
        triage_present: labels.iter().any(|label| label == READY),
    })
}

fn run_issue_edit(
    gh_bin: &Path,
    root: &Path,
    issue: u32,
    operation: &str,
    label: &str,
) -> anyhow::Result<()> {
    let n = issue.to_string();
    let cmd = gh_call::write("claim.curating_release", gh_bin, root)
        .args(["issue", "edit", &n, operation, label]);
    let out = gh_call::output(cmd.args(gh_call::loom_repo_flag()))?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        anyhow::bail!("gh issue edit failed for #{issue} in {root}: {err}");
    }
    Ok(())
}

/// Post the re-curation record and require its Forge-assigned comment ID.
fn post_recuration_record(
    gh_bin: &Path,
    root: &Path,
    issue: u32,
    lease: &CuratorLeaseEvidence,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments");
    let body = recuration_record_body(lease, now);
    let out = gh_call::output(
        gh_call::write("claim.curating_recuration_comment", gh_bin, root)
            .args(["api", "-X", "POST", &path, "-f", &format!("body={body}")])
            .args(["--jq", ".id"]),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        anyhow::bail!("re-curation record post failed for #{issue} in {root}: {err}");
    }
    if !String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .is_ok_and(|id| id > 0)
    {
        anyhow::bail!("re-curation record post returned no comment ID for #{issue}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "curating_reclaim_tests.rs"]
mod tests;
