//! The ETag-cached REST listing of open `loom:building` issues that claim
//! reconciliation walks (#4428).
//!
//! A sibling module rather than inline in `claim_reconciliation.rs`, which is
//! frozen by the file-size ratchet (`.loom/docs/file-size-policy.md`). Calls
//! are recorded against the `claim_reconciliation` caller in the #9251
//! forge-call accounting.

use std::path::Path;

use anyhow::Result;

use super::BuildingIssue;

pub(super) fn list_building_issues(gh_bin: &Path, root: &Path) -> Result<Vec<BuildingIssue>> {
    // An unchanged claim set costs zero rate limit (304). `LOOM_REPO`
    // precedence is handled inside; the issues-only listing keeps the
    // pre-#4428 semantics (#9929: a `loom:building` PR row must never be
    // reconciled as if it were a claimed issue).
    let rows = crate::forge_listing::list_issues_only_cached_as(
        "claim_reconciliation",
        gh_bin,
        Some(root),
        None,
        "loom:building",
        "open",
    )?;
    Ok(rows
        .into_iter()
        .map(|r| BuildingIssue {
            number: r.number,
            updated_at: r
                .updated_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
        })
        .collect())
}
