//! The forge side of the shared restore veto (#11480) for the release paths
//! that run outside a `SweepRegistry`: the startup quarantine reconciliation
//! and claim reconciliation's reclaim. The verdict itself is
//! [`crate::work_finder::restore_veto`]; this module only reads the labels it
//! needs, over REST, and fails open like those paths' other probes.

use std::path::Path;

use crate::claim_reconciliation::gh_call;

/// Telemetry name of the label read (an `issue.view-state` op).
pub(crate) const LABELS_OP: &str = "release_veto.issue_labels";

/// The label on `issue` that forbids re-adding `loom:issue`, ignoring the
/// caller's own `removing` labels. `None` when nothing vetoes **or** the read
/// failed: an unverifiable item is released as it was before #11480, so a
/// forge hiccup never strands a claim.
pub(crate) fn veto(gh_bin: &Path, root: &Path, issue: u32, removing: &[&str]) -> Option<String> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}");
    let jq = "[(.labels // [])[] | .name]";
    let stdout = gh_call::ok_stdout(
        gh_call::read(LABELS_OP, gh_bin, root).args(["api", &path, "--jq", jq]),
    )?;
    let labels: Vec<String> = serde_json::from_slice(&stdout).ok()?;
    crate::work_finder::restore_veto(&labels, removing).map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_gh(dir: &Path, labels_json: &str) -> std::path::PathBuf {
        let gh = dir.join("gh");
        std::fs::write(&gh, format!("#!/bin/sh\nprintf '%s' '{labels_json}'\n")).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    #[test]
    fn a_veto_label_is_reported_and_the_callers_own_label_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), r#"["loom:blocked","loom:needs-capability"]"#);
        assert_eq!(
            veto(&gh, dir.path(), 1950, &["loom:blocked"]).as_deref(),
            Some("loom:needs-capability")
        );
        let gh = fake_gh(dir.path(), r#"["loom:blocked","loom:important"]"#);
        assert_eq!(veto(&gh, dir.path(), 1950, &["loom:blocked"]), None);
    }

    #[test]
    fn an_unreadable_answer_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "not json");
        assert_eq!(veto(&gh, dir.path(), 7, &[]), None);
    }
}
