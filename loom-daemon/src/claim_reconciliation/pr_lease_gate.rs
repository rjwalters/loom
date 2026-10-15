//! The lease gate on PR-side claim reconciliation (#11911).
//!
//! The step engine claims Judge and Doctor work with a role lease
//! (`loom-daemon lease pr`, #9401, #11898):
//!
//! ```text
//! <!-- loom:pr-lease role=<judge|doctor> host=<h> sweep=<s> [step=<step>] -->
//! ```
//!
//! renewed for as long as the step runs. `loom:reviewing` / `loom:treating`
//! are projections of that lease, and the label's own age says nothing about
//! the step holding it: a step can claim a PR whose label a dead run left
//! behind, so the label reads older than the step. The age rule
//! ([`super::decide_pr`]) alone would then strip a running step's claim.
//!
//! So a `Reclaim` decision is checked against the lease first: a trusted
//! lease of the claim's role whose forge-assigned `updated_at` is inside the
//! lease TTL ([`super::resolve_lease_ttl_minutes`]) keeps the claim, and so
//! does an unreadable comment list (fail safe, like the issue side's
//! #7591 rule). Released (`loom:pr-lease-released`) and yield records never
//! match the lease prefix. A loser's lease that was not yet released counts
//! as live: the gate can only err toward keeping a claim.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::forge::LeaseProbe;
use super::{gh_call, lease_is_fresh, PrClaimKind};

/// The lease role a PR claim label projects.
#[must_use]
pub(crate) fn lease_role(kind: PrClaimKind) -> &'static str {
    match kind {
        PrClaimKind::Reviewing => "judge",
        PrClaimKind::Treating => "doctor",
    }
}

/// The marker prefix of `role`'s live leases (the trailing space keeps
/// `role=judge` from matching any longer role name).
#[must_use]
pub(crate) fn lease_prefix(role: &str) -> String {
    format!("<!-- loom:pr-lease role={role} ")
}

/// Whether `probe` (as of `now`, against `ttl_minutes`) keeps the claim: a
/// fresh lease, or an unverifiable read. Pure.
#[must_use]
pub(crate) fn keeps_claim(probe: &LeaseProbe, now: DateTime<Utc>, ttl_minutes: f64) -> bool {
    match probe {
        LeaseProbe::Found(at) => lease_is_fresh(*at, now, ttl_minutes),
        LeaseProbe::ReadFailed => true,
        LeaseProbe::NotFound => false,
    }
}

/// The freshest trusted `role` lease on PR `pr_number`.
pub(crate) fn fetch_freshest_pr_lease(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    role: &str,
) -> LeaseProbe {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/comments?per_page=100");
    let jq = format!(
        r#".[] | select(.body | startswith("{}")) | {{updated_at, {}}}"#,
        lease_prefix(role),
        crate::comment_trust::records::AUTHOR_JQ
    );
    let Some(stdout) =
        gh_call::ok_stdout(gh_call::read("claim.pr_lease_comments", gh_bin, root).args([
            "api",
            &path,
            "--paginate",
            "--jq",
            &jq,
        ]))
    else {
        return LeaseProbe::ReadFailed;
    };
    match crate::comment_trust::TrustPolicy::for_root(root).trusted_records(&stdout) {
        None => LeaseProbe::ReadFailed,
        Some(leases) => crate::comment_trust::records::max_timestamp(&leases, "updated_at")
            .map_or(LeaseProbe::NotFound, LeaseProbe::Found),
    }
}

/// Whether a live step lease (or an unreadable lease list) keeps `kind`'s
/// claim on `pr_number`, logging when it does.
pub(crate) fn lease_keeps_pr_claim(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    kind: PrClaimKind,
) -> bool {
    let role = lease_role(kind);
    let probe = fetch_freshest_pr_lease(gh_bin, root, pr_number, role);
    let keep = keeps_claim(&probe, Utc::now(), super::resolve_lease_ttl_minutes());
    if keep {
        log::info!(
            "claim_reconciliation: kept {} on PR #{pr_number} in {}: its {role} lease is live or \
             unreadable ({probe:?}) (#11911)",
            kind.label(),
            root.display()
        );
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_lease_or_an_unreadable_list_keeps_the_claim() {
        let now = Utc::now();
        let ttl = 15.0;
        let fresh = LeaseProbe::Found(now - chrono::Duration::minutes(3));
        let expired = LeaseProbe::Found(now - chrono::Duration::minutes(20));
        assert!(keeps_claim(&fresh, now, ttl));
        assert!(!keeps_claim(&expired, now, ttl));
        assert!(!keeps_claim(&LeaseProbe::NotFound, now, ttl));
        assert!(keeps_claim(&LeaseProbe::ReadFailed, now, ttl), "fail safe");
    }

    #[test]
    fn each_claim_label_reads_its_own_roles_lease() {
        assert_eq!(lease_role(PrClaimKind::Reviewing), "judge");
        assert_eq!(lease_role(PrClaimKind::Treating), "doctor");
        let judge = lease_prefix("judge");
        assert!("<!-- loom:pr-lease role=judge host=h sweep=s -->".starts_with(&judge));
        assert!(!"<!-- loom:pr-lease-released role=judge host=h sweep=s -->".starts_with(&judge));
        assert!(!"<!-- loom:pr-lease-yield role=judge host=h sweep=s -->".starts_with(&judge));
        assert!(!"<!-- loom:pr-lease role=doctor host=h sweep=s -->".starts_with(&judge));
    }
}
