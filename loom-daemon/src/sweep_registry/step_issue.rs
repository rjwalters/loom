//! The issue a step's telemetry belongs to (Issue #11898).
//!
//! A sweep's telemetry identity is its issue: `prepare_child` stamps it on the
//! execution's root span, and the agent relay and the usage join key on it. An
//! issue step's item is that issue. A PR step's item (Judge, Doctor, Champion
//! merge) is a **PR** number, and keying on it would attribute a Doctor step
//! on PR #M to an issue #M that does not exist. So a PR step is re-keyed to
//! the issue its PR closes: one REST read of the PR body, its single closing
//! reference. No single closing reference, a failed read, or a fixture
//! without forge access: `None`, an unattributed issue rather than a wrong
//! one.

use super::*;
use crate::observability::lifecycle::StepKeys;

impl SweepRegistry {
    /// The issue of a run of `kind`: an issue sweep's issue, an issue
    /// step's item, a PR step's linked issue (best-effort), `None` for a PR set.
    #[cfg(test)]
    pub(crate) fn step_issue(&self, kind: &SweepKind) -> Option<u32> {
        self.step_keys(kind).issue
    }

    /// The join keys of a run of `kind` (#13169): its issue, and for a step
    /// its role, plus a PR step's PR number (known from the item, so set even
    /// when the forge read fails) and the PR's head SHA. One REST read of the
    /// PR serves both the issue and the head SHA.
    pub(crate) fn step_keys(&self, kind: &SweepKind) -> StepKeys {
        match kind {
            SweepKind::Issue(issue) => StepKeys::issue(Some(*issue)),
            SweepKind::Step { item, role } if !role.acts_on_pr() => StepKeys {
                role: Some(role.role_name()),
                ..StepKeys::issue(Some(*item))
            },
            SweepKind::Step { item, role } => {
                let (issue, head_sha) = self.pr_facts(*item);
                StepKeys {
                    issue,
                    pr_number: Some(*item),
                    role: Some(role.role_name()),
                    head_sha,
                }
            }
            SweepKind::PrSet(_) => StepKeys::default(),
        }
    }

    /// The PR a finished Builder step opened for `issue`: the newest PR on
    /// its `feature/issue-N` branch, one REST read (#13169). The outcome line
    /// carries no PR number, so the forge is the only record. `None` when
    /// tracing is off (no read is made), the step opened no PR, or the read
    /// fails.
    pub(crate) fn builder_pr(&self, issue: u32) -> Option<u32> {
        if self.config.skip_label_flip
            || !crate::observability::tracing::enabled(&self.config.workspace_root)
        {
            return None;
        }
        let path = format!(
            "repos/{{owner}}/{{repo}}/pulls?head={{owner}}:feature/issue-{issue}&state=all&per_page=1"
        );
        let out = self
            .gh_read("spawn.builder_pr", ["api", path.as_str(), "--jq", ".[0].number // empty"])
            .ok()
            .flatten()
            .filter(|o| o.status.success())?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    /// The single issue `pr`'s body closes and the PR's head SHA, read once
    /// over REST. Each is `None` when the read fails, or label flips (and so
    /// forge reads) are off; the issue is also `None` when the body closes
    /// none or several.
    fn pr_facts(&self, pr: u32) -> (Option<u32>, Option<String>) {
        if self.config.skip_label_flip {
            return (None, None);
        }
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
        let Some(out) = self
            .gh_read(
                "spawn.step_issue",
                [
                    "api",
                    path.as_str(),
                    "--jq",
                    "(.head.sha // \"\"), (.body // \"\")",
                ],
            )
            .ok()
            .flatten()
            .filter(|o| o.status.success())
        else {
            return (None, None);
        };
        pr_facts_of(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Split the PR read's output (head SHA on the first line, the body after) into
/// the closed issue and the head SHA. Pure.
#[must_use]
pub(crate) fn pr_facts_of(out: &str) -> (Option<u32>, Option<String>) {
    let (sha, body) = out.split_once('\n').unwrap_or((out, ""));
    let sha = sha.trim();
    let sha = (sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| sha.to_ascii_lowercase());
    (issue_of_body(body), sha)
}

/// The single issue a PR body closes. Pure.
#[must_use]
pub(crate) fn issue_of_body(body: &str) -> Option<u32> {
    match crate::merge_pr::refs::closing_refs(body).as_slice() {
        [one] => u32::try_from(*one).ok(),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::step_role::StepRole;
    use crate::sweep_registry::test_support::fixture_registry;

    #[test]
    fn a_pr_body_names_its_issue_only_when_it_closes_one_issue() {
        assert_eq!(issue_of_body("Closes #42\n\nmore"), Some(42));
        assert_eq!(issue_of_body("Part of #42"), None);
        assert_eq!(issue_of_body("Closes #1, closes #2"), None);
        assert_eq!(issue_of_body(""), None);
    }

    #[test]
    fn the_pr_read_yields_the_issue_and_head_sha_independently() {
        let sha = "a".repeat(40);
        assert_eq!(pr_facts_of(&format!("{sha}\nCloses #42")), (Some(42), Some(sha.clone())));
        assert_eq!(pr_facts_of(&format!("{sha}\nno ref")), (None, Some(sha.clone())));
        assert_eq!(pr_facts_of(&format!("{sha}\nCloses #1, closes #2")), (None, Some(sha)));
        assert_eq!(pr_facts_of("\nCloses #5"), (Some(5), None));
        assert_eq!(pr_facts_of(""), (None, None));
    }

    /// A PR step names its PR and role even when the forge read yields
    /// nothing; an issue step names its role and no PR.
    #[test]
    fn step_keys_name_the_pr_and_role_without_the_forge() {
        let dir = tempfile::tempdir().unwrap();
        let (reg, _) = fixture_registry(dir.path());
        let keys = reg.step_keys(&SweepKind::Step {
            item: 77,
            role: StepRole::Judge,
        });
        assert_eq!(keys.pr_number, Some(77));
        assert_eq!(keys.role, Some("judge"));
        assert_eq!(keys.issue, None);
        let keys = reg.step_keys(&SweepKind::Step {
            item: 78,
            role: StepRole::Curator,
        });
        assert_eq!((keys.issue, keys.pr_number, keys.role), (Some(78), None, Some("curator")));
        assert_eq!(reg.step_keys(&SweepKind::Issue(79)), StepKeys::issue(Some(79)));
    }

    /// A PR step never takes its PR number as its issue.
    #[test]
    fn a_pr_step_is_never_keyed_by_its_pr_number() {
        let dir = tempfile::tempdir().unwrap();
        let (reg, _) = fixture_registry(dir.path());
        let judge = SweepKind::Step {
            item: 77,
            role: StepRole::Judge,
        };
        assert_eq!(reg.step_issue(&judge), None);
        let curator = SweepKind::Step {
            item: 78,
            role: StepRole::Curator,
        };
        assert_eq!(reg.step_issue(&curator), Some(78));
        assert_eq!(reg.step_issue(&SweepKind::Issue(79)), Some(79));
        assert_eq!(reg.step_issue(&SweepKind::PrSet(vec![1])), None);
    }
}
