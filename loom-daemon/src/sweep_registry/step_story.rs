//! The story a step's telemetry and commits belong to (Issue #11898).
//!
//! A sweep's telemetry identity is its issue: `prepare_child` traces it under
//! that story, the provenance hooks stamp commits with it, the agent relay and
//! the usage join key on it. An issue step's item is that issue. A PR step's
//! item (Judge, Doctor, Champion merge) is a **PR** number, and keying on it
//! would trace a Doctor step on PR #M under a story #M that does not exist,
//! and stamp its commits with it. So a PR step is re-keyed to the issue its
//! PR closes: one REST read of the PR body,
//! its single closing reference. No single closing reference, a failed read,
//! or a fixture without forge access: `None`, an untraced story rather than
//! a wrong one.

use super::*;

impl SweepRegistry {
    /// The story issue of a run of `kind`: an issue sweep's issue, an issue
    /// step's item, a PR step's linked issue (best-effort), `None` for a PR set.
    pub(crate) fn step_story_issue(&self, kind: &SweepKind) -> Option<u32> {
        match kind {
            SweepKind::Issue(issue) => Some(*issue),
            SweepKind::Step { item, role } if !role.acts_on_pr() => Some(*item),
            SweepKind::Step { item, .. } => self.pr_story_issue(*item),
            SweepKind::PrSet(_) => None,
        }
    }

    /// The single issue `pr`'s body closes, read once over REST. `None` when
    /// the read fails, the body closes none or several, or label flips (and so
    /// forge reads) are off.
    fn pr_story_issue(&self, pr: u32) -> Option<u32> {
        if self.config.skip_label_flip {
            return None;
        }
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
        let out = self
            .gh_read("spawn.step_story", ["api", path.as_str(), "--jq", ".body // \"\""])
            .ok()
            .flatten()
            .filter(|o| o.status.success())?;
        story_of_body(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The single issue a PR body closes. Pure.
#[must_use]
pub(crate) fn story_of_body(body: &str) -> Option<u32> {
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
    fn a_pr_body_names_its_story_only_when_it_closes_one_issue() {
        assert_eq!(story_of_body("Closes #42\n\nmore"), Some(42));
        assert_eq!(story_of_body("Part of #42"), None);
        assert_eq!(story_of_body("Closes #1, closes #2"), None);
        assert_eq!(story_of_body(""), None);
    }

    /// A PR step never takes its PR number as its story.
    #[test]
    fn a_pr_step_is_never_keyed_by_its_pr_number() {
        let dir = tempfile::tempdir().unwrap();
        let (reg, _) = fixture_registry(dir.path());
        let judge = SweepKind::Step {
            item: 77,
            role: StepRole::Judge,
        };
        assert_eq!(reg.step_story_issue(&judge), None);
        let curator = SweepKind::Step {
            item: 78,
            role: StepRole::Curator,
        };
        assert_eq!(reg.step_story_issue(&curator), Some(78));
        assert_eq!(reg.step_story_issue(&SweepKind::Issue(79)), Some(79));
        assert_eq!(reg.step_story_issue(&SweepKind::PrSet(vec![1])), None);
    }
}
