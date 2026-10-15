//! Tracking parents with open sub-issues are not dispatchable (Issue #11483).
//!
//! A parent decomposed into native sub-issues (`create-issue.sh --parent`)
//! keeps `loom:issue` while its slices land as `Part of #N`, so every sweep
//! dispatched on it declines it. The REST listing already carries
//! `sub_issues_summary` ([`crate::forge_listing::RestIssue::open_sub_issues`]),
//! so this gate costs no forge call. The parent is eligible again, with no
//! label change, once every child is closed (`total == completed`). A missing
//! summary (Gitea, older payloads) keeps the pre-#11483 behaviour.
//!
//! A starred parent is skipped too: the star belongs on its open slices.

use super::WorkItem;

impl WorkItem {
    /// Builder-style setter for the open native sub-issue count (#11483).
    #[must_use]
    pub fn with_open_sub_issues(mut self, open_sub_issues: Option<u32>) -> Self {
        self.open_sub_issues = open_sub_issues;
        self
    }

    /// The number of open sub-issues that make this a tracking parent the
    /// work finder must not dispatch this tick, or `None` when it may.
    #[must_use]
    pub fn open_sub_issues_blocking(&self) -> Option<u32> {
        self.open_sub_issues.filter(|&open| open > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_children_block_and_closed_or_absent_do_not() {
        let item = |open| WorkItem::new(7, vec!["loom:issue".into()]).with_open_sub_issues(open);
        assert_eq!(item(Some(2)).open_sub_issues_blocking(), Some(2));
        assert_eq!(item(Some(0)).open_sub_issues_blocking(), None);
        assert_eq!(item(None).open_sub_issues_blocking(), None);
    }
}
