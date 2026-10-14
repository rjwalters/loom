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

impl SweepRegistry {
    /// Write a lease record (Issue #6179) for `issue`, naming `step` when the
    /// claimant is a step (#11898). Best-effort and bounded, as before: a `gh`
    /// failure only logs, and never fails the dispatch or undoes the claim.
    pub(crate) fn write_lease_comment_for(
        &self,
        issue: u32,
        sweep_id: &str,
        step: Option<StepRole>,
    ) {
        if self.config.skip_label_flip {
            return;
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
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                self.report_forge_failure(&stderr, "sweep_dispatch (lease comment)");
                log::warn!(
                    "lease comment for #{issue} exited {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                );
            }
            Ok(None) => log::warn!(
                "lease comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!("lease comment for #{issue} failed: {e}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
