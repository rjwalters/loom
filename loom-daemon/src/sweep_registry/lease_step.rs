//! The issue lease record's `step` field (Issue #11898; design #11894 §5).
//!
//! An issue lease (`<!-- loom:lease host=<h> sweep=<s> -->`, #6179) is read
//! by first line only, and every reader (the daemon's ordering and
//! reclamation, `check-claim`, co-occupancy, and `sweep-lease-{publish,
//! fence,renew}.sh`) splits that line on ` sweep=` and takes the rest as the
//! sweep id. A `step=` token there would become part of every older reader's
//! sweep id. So the step rides on the record's **second** line, as its own
//! marker:
//!
//! ```text
//! <!-- loom:lease host=<host-id> sweep=<sweep-id> -->
//! <!-- loom:lease-step step=builder -->
//! ```
//!
//! Older readers never look past the first line; [`parse_lease_step`] reads
//! the second. Only the step engine's Builder step writes one today: an issue
//! sweep's lease names no step. The other step claims have no issue-path
//! lease: Curator and Champion use the role-keyed `lease pr` family
//! (`--role curator`, `--role champion --step …`), and Judge and Doctor
//! already did, because the issue-path readers assume one claim per number.

use super::*;
use crate::gh_invocation::AccessIntent;
use crate::step_role::StepRole;

/// The second-line marker naming a lease's step.
pub const LEASE_STEP_PREFIX: &str = "<!-- loom:lease-step step=";

/// The step a lease record names on its second line, if any. Pure.
#[must_use]
pub fn parse_lease_step(body: &str) -> Option<StepRole> {
    body.lines()
        .nth(1)?
        .strip_prefix(LEASE_STEP_PREFIX)?
        .strip_suffix(" -->")?
        .parse()
        .ok()
}

/// The lease record's body: the marker, the step line when `step` is given,
/// then prose no reader parses.
#[must_use]
pub(crate) fn lease_record_body(
    host: &str,
    sweep_id: &str,
    step: Option<StepRole>,
    ts: &str,
) -> String {
    let step_line = step
        .map(|s| format!("\n{LEASE_STEP_PREFIX}{s} -->"))
        .unwrap_or_default();
    let holder = step.map_or_else(|| "sweep".to_string(), |s| format!("{s} step"));
    format!(
        "{LEASE_MARKER_PREFIX}{host} sweep={sweep_id} -->{step_line}\n\
         This issue's `loom:building` claim was acquired by {holder} `{sweep_id}` on host \
         `{host}` at {ts}. This comment is a **lease record** (Issue #6179, Epic #6165) — \
         its liveness signal is this comment's own forge-assigned `updated_at`, never a \
         timestamp embedded in this text. See `defaults/docs/lease-record.md` for the \
         format contract this establishes, and `defaults/docs/lease-renewal.md` for how \
         the owning sweep keeps it fresh for the lifetime of its claim."
    )
}

/// Expired lease records of one host kept per issue when a new claim prunes
/// (#13245).
const KEEP_EXPIRED_LEASES: usize = 3;

/// Ids of `host`'s lease comments a new claim may delete (#13245): those whose
/// own `updated_at` is past `ttl`, beyond the newest `keep`. A live lease, a
/// record of another host and a record without `updated_at` are never listed.
/// Pure.
#[must_use]
pub(crate) fn expired_lease_ids(
    comments: &[LeaseComment],
    host: &str,
    now: chrono::DateTime<Utc>,
    ttl: chrono::Duration,
    keep: usize,
) -> Vec<u64> {
    let mut dead: Vec<u64> = comments
        .iter()
        .filter(|c| c.host == host)
        .filter(|c| {
            c.updated_at
                .is_some_and(|u| now.signed_duration_since(u) >= ttl)
        })
        .map(|c| c.id)
        .collect();
    dead.sort_unstable();
    dead.truncate(dead.len().saturating_sub(keep));
    dead
}

impl SweepRegistry {
    /// Delete this host's expired lease comments on `issue` beyond the newest
    /// [`KEEP_EXPIRED_LEASES`] (#13245). Best-effort: any failure only logs.
    fn prune_expired_leases(&self, issue: u32, host: &str) {
        let Some(comments) = self.read_lease_comments(issue) else {
            return;
        };
        let ttl_secs = (crate::claim_reconciliation::resolve_lease_ttl_minutes() * 60.0) as i64;
        let ids = expired_lease_ids(
            &comments,
            host,
            Utc::now(),
            chrono::Duration::seconds(ttl_secs),
            KEEP_EXPIRED_LEASES,
        );
        for id in ids {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            let del = ["api", "-X", "DELETE", path.as_str(), "--silent"];
            if !self
                .gh_write("guard.lease_prune", del)
                .is_ok_and(|o| o.is_some_and(|o| o.status.success()))
            {
                log::debug!("lease prune of comment {id} on #{issue} failed (non-fatal)");
            }
        }
    }

    /// Write a lease record (Issue #6179) for `issue`, naming `step` when the
    /// claimant is a step (#11898). Bounded, and a `gh` failure only logs and
    /// never fails the dispatch or undoes the claim; the result says whether
    /// the comment was posted, for a caller (the Builder step, #12133) that
    /// must refuse without it. Other callers ignore it.
    pub(crate) fn write_lease_comment_for(
        &self,
        issue: u32,
        sweep_id: &str,
        step: Option<StepRole>,
    ) -> bool {
        if self.config.skip_label_flip {
            return true;
        }
        let gh = self.resolved_gh();
        let host = self.published_host_id();
        let body = lease_record_body(&host, sweep_id, step, &Utc::now().to_rfc3339());
        let mut comment = self.gh_inv("guard.lease_comment", AccessIntent::Write, &gh);
        comment = comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(self.dashboard_comment_body(issue, &body));
        // This repo's issue, in a multi-workspace daemon (#3928/#3937).
        comment = comment.args(crate::claim_reconciliation::gh_call::loom_repo_flag());
        // Bounded so a wedged `gh` never blocks the dispatch path (#3973).
        let timeout = reaper::reap_gh_timeout();
        match self.run_counted(comment) {
            Ok(Some(output)) if output.status.success() => {
                self.prune_expired_leases(issue, &host);
                true
            }
            Ok(Some(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                self.report_forge_failure(&stderr, "sweep_dispatch (lease comment)");
                log::warn!(
                    "lease comment for #{issue} exited {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                );
                false
            }
            Ok(None) => {
                log::warn!(
                    "lease comment for #{issue} exceeded {}s, killed (#3973)",
                    timeout.as_secs()
                );
                false
            }
            Err(e) => {
                log::warn!("lease comment for #{issue} failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn lc(id: u64, host: &str, age_min: i64) -> LeaseComment {
        LeaseComment {
            id,
            created_at: None,
            updated_at: Some(Utc::now() - chrono::Duration::minutes(age_min)),
            host: host.into(),
            sweep_id: format!("s{id}"),
        }
    }

    #[test]
    fn expired_leases_are_pruned_beyond_the_newest_k_and_live_or_peer_ones_survive() {
        let mut cs: Vec<LeaseComment> = (1..=6).map(|i| lc(i, "h", 120)).collect();
        cs.push(lc(7, "h", 1)); // live
        cs.push(lc(8, "other", 120)); // peer host
        let ttl = chrono::Duration::minutes(15);
        assert_eq!(expired_lease_ids(&cs, "h", Utc::now(), ttl, 3), vec![1, 2, 3]);
        assert!(expired_lease_ids(&cs, "h", Utc::now(), ttl, 9).is_empty());
    }

    #[test]
    fn a_step_lease_keeps_the_first_line_every_older_reader_parses() {
        let body = lease_record_body("host-a", "step-builder-7-1", Some(StepRole::Builder), "t");
        assert_eq!(
            SweepRegistry::parse_lease_marker_line(&body),
            Some(("host-a".to_string(), "step-builder-7-1".to_string()))
        );
        assert_eq!(parse_lease_step(&body), Some(StepRole::Builder));
    }

    #[test]
    fn a_sweep_lease_names_no_step() {
        let body = lease_record_body("host-a", "sweep-issue-7-1", None, "t");
        assert_eq!(parse_lease_step(&body), None);
        assert!(SweepRegistry::parse_lease_marker_line(&body).is_some());
        assert!(body.lines().nth(1).unwrap().starts_with("This issue's"));
    }
}
