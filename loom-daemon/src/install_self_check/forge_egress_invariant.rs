//! [`Invariant::ForgeEgressAligned`] (#9984): the forge egress routing
//! verdict as a self-check invariant, with an issue lifecycle the other
//! invariants do not need — **file** one issue naming the finding codes,
//! **refresh** its body when the code set changes, **close** it once routing
//! is aligned again. Like every other filing, it acts only in repair/act mode.

use std::path::Path;

use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

use super::{Invariant, InvariantOutcome, InvariantStatus, ViolationReporter};

/// Prefix of the hidden codes marker that makes a refresh idempotent.
const CODES_MARKER_PREFIX: &str = "<!-- loom:forge-egress-codes:";

/// Check: `Skipped` when no policy is configured, `Ok` when routing is
/// aligned, else a `Violation` naming the codes and the first remedy.
pub(super) fn check(repo_root: &Path) -> InvariantStatus {
    let report = crate::forge_egress::assert_for(repo_root);
    if !report.is_configured() {
        return InvariantStatus::Skipped("no forge egress policy configured".to_string());
    }
    match report.exit_code() {
        0 => InvariantStatus::Ok,
        code => {
            let fix = crate::forge_egress::gate::first_remedy(&report.routing)
                .map(|f| crate::forge_egress::report::redact(&f.remedy))
                .unwrap_or_default();
            InvariantStatus::Violation(format!(
                "routing verdict exit {code}; finding codes: {}; fix: {fix}; full report: `{}`",
                report.routing_codes().join(", "),
                crate::forge_egress::gate::REPAIR_COMMAND
            ))
        }
    }
}

/// The codes portion of a [`check`] violation detail.
fn codes_of(detail: &str) -> &str {
    detail
        .split_once("finding codes: ")
        .and_then(|(_, rest)| rest.split(';').next())
        .unwrap_or("")
}

fn codes_marker(codes: &str) -> String {
    format!("{CODES_MARKER_PREFIX}{} -->", codes.replace(' ', ""))
}

/// The issue body for `detail`.
#[must_use]
pub(super) fn issue_body(repo_root: &Path, detail: &str) -> String {
    let inv = Invariant::ForgeEgressAligned;
    format!(
        "The daemon install/host self-check (#5035) found that this host's `gh` would not reach \
         the mandated API origin (forge egress policy, #9984).\n\n\
         - **Invariant**: `{id}`\n\
         - **Repo**: `{repo}`\n\
         - **Finding codes**: {codes}\n\
         - **Evidence**: {detail}\n\n\
         Run `loom-daemon forge egress doctor` on the host for the full report. The self-check \
         refreshes this body when the codes change and closes this issue once routing is \
         aligned.\n\n{marker}\n{codes_marker}\n",
        id = inv.id(),
        repo = repo_root.display(),
        codes = codes_of(detail),
        marker = inv.issue_marker(),
        codes_marker = codes_marker(codes_of(detail)),
    )
}

/// File / refresh / close for one pass. Report-only mode only logs.
pub(super) fn reconcile<R: ViolationReporter>(
    repo_root: &Path,
    outcome: &InvariantOutcome,
    repair_mode: bool,
    reporter: &R,
) {
    let id = outcome.invariant.id();
    let marker = outcome.invariant.issue_marker();
    let root = repo_root.display();
    let detail = match &outcome.status {
        InvariantStatus::Skipped(reason) => {
            log::debug!("install_self_check: {id} skipped ({root}): {reason}");
            return;
        }
        InvariantStatus::Violation(d) => {
            log::warn!("install_self_check: {id} VIOLATED ({root}): {d}");
            Some(d.as_str())
        }
        InvariantStatus::Ok => None,
    };
    if !repair_mode {
        if detail.is_some() {
            log::info!("install_self_check: report-only — not filing {id} for {root}");
        }
        return;
    }
    let open = match reporter.find_open_issue(&marker) {
        Ok(open) => open,
        Err(e) => {
            log::warn!("install_self_check: {id} lookup failed for {root} ({e}); not acting");
            return;
        }
    };
    let result = match (detail, open) {
        (None, None) => Ok(()),
        (None, Some((n, _))) => reporter
            .close_issue(n, "Forge egress routing is aligned again (`forge egress assert` exit 0) — closing (#9984).")
            .map(|()| log::info!("install_self_check: closed #{n} — {id} aligned in {root}")),
        (Some(d), None) => reporter
            .file_issue(&format!("install self-check: {}", outcome.invariant.title()), &issue_body(repo_root, d))
            .map(|n| log::warn!("install_self_check: filed #{n} for {id} in {root}")),
        (Some(d), Some((n, body))) if !body.contains(&codes_marker(codes_of(d))) => reporter
            .update_issue_body(n, &issue_body(repo_root, d))
            .map(|()| log::info!("install_self_check: refreshed #{n} for {id} in {root}")),
        (Some(_), Some(_)) => Ok(()),
    };
    if let Err(e) = result {
        log::warn!("install_self_check: {id} issue lifecycle failed for {root}: {e}");
    }
}

/// Bound on each lifecycle `gh` call.
const GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// One `gh` call through the spawn choke point (#9985): the facade applies the
/// owner-correct `GH_CONFIG_DIR` for `repo_root` (#5431) and the resolved
/// executable.
fn gh(
    repo_root: &Path,
    operation: &'static str,
    intent: AccessIntent,
    args: &[&str],
) -> Result<String, String> {
    let completion =
        GhInvocation::new(Operation::new(operation), intent, GhTarget::None, GH_TIMEOUT)
            .args(args)
            .current_dir(repo_root)
            .execute()
            .map_err(|e| format!("could not run gh: {e}"))?;
    let out = match completion {
        GhCompletion::Captured(Completion::Exited(out)) => out,
        GhCompletion::Captured(_) => return Err(format!("gh {operation} timed out")),
        GhCompletion::Passthrough(_) => return Err("unexpected passthrough".to_string()),
    };
    if !out.status.success() {
        return Err(format!(
            "gh {operation} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `GhIssueFiler::find_open_issue`.
pub(super) fn gh_find_open_issue(
    repo_root: &Path,
    marker: &str,
) -> Result<Option<(u64, String)>, String> {
    let stdout = gh(
        repo_root,
        "issue.list",
        AccessIntent::Read,
        &[
            "issue",
            "list",
            "--state",
            "open",
            "--search",
            marker,
            "--json",
            "number,body",
            "--limit",
            "5",
        ],
    )?;
    let rows: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("could not parse gh json: {e}"))?;
    Ok(rows.as_array().into_iter().flatten().find_map(|r| {
        let body = r["body"].as_str()?;
        body.contains(marker)
            .then(|| (r["number"].as_u64().unwrap_or(0), body.to_string()))
    }))
}

/// `gh issue <args…>` for the edit/close half of the lifecycle.
pub(super) fn gh_issue_op(
    repo_root: &Path,
    operation: &'static str,
    args: &[&str],
) -> Result<(), String> {
    let mut full = vec!["issue"];
    full.extend_from_slice(args);
    gh(repo_root, operation, AccessIntent::Write, &full).map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        open: RefCell<Option<(u64, String)>>,
        log: RefCell<Vec<String>>,
    }

    impl ViolationReporter for Fake {
        fn has_open_issue(&self, _marker: &str) -> Result<bool, String> {
            Ok(self.open.borrow().is_some())
        }
        fn file_issue(&self, _title: &str, body: &str) -> Result<u64, String> {
            self.log.borrow_mut().push("file".into());
            *self.open.borrow_mut() = Some((9, body.to_string()));
            Ok(9)
        }
        fn find_open_issue(&self, _marker: &str) -> Result<Option<(u64, String)>, String> {
            Ok(self.open.borrow().clone())
        }
        fn update_issue_body(&self, n: u64, body: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("update {n}"));
            *self.open.borrow_mut() = Some((n, body.to_string()));
            Ok(())
        }
        fn close_issue(&self, n: u64, _comment: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("close {n}"));
            *self.open.borrow_mut() = None;
            Ok(())
        }
    }

    fn outcome(status: InvariantStatus) -> InvariantOutcome {
        InvariantOutcome {
            invariant: Invariant::ForgeEgressAligned,
            status,
        }
    }

    fn violation(codes: &str) -> InvariantStatus {
        InvariantStatus::Violation(format!(
            "routing verdict exit 1; finding codes: {codes}; fix: x; full report: `y`"
        ))
    }

    #[test]
    fn files_refreshes_on_code_change_and_closes_when_aligned() {
        let root = Path::new("/repo");
        let fake = Fake::default();
        reconcile(root, &outcome(violation("toolchain.below-api-host-floor")), true, &fake);
        let body = fake.open.borrow().clone().unwrap().1;
        assert!(body.contains("toolchain.below-api-host-floor"), "{body}");
        assert!(body.contains(&Invariant::ForgeEgressAligned.issue_marker()));
        // Same codes again: no duplicate, no edit.
        reconcile(root, &outcome(violation("toolchain.below-api-host-floor")), true, &fake);
        // Codes changed: refresh in place.
        reconcile(
            root,
            &outcome(violation("toolchain.below-api-host-floor, apiconfig.api-host-missing")),
            true,
            &fake,
        );
        assert!(fake
            .open
            .borrow()
            .clone()
            .unwrap()
            .1
            .contains("apiconfig.api-host-missing"));
        // Aligned: close.
        reconcile(root, &outcome(InvariantStatus::Ok), true, &fake);
        assert_eq!(*fake.log.borrow(), vec!["file", "update 9", "close 9"]);
        assert!(fake.open.borrow().is_none());
    }

    #[test]
    fn report_only_and_unconfigured_never_touch_the_forge() {
        let fake = Fake::default();
        reconcile(Path::new("/r"), &outcome(violation("a")), false, &fake);
        reconcile(Path::new("/r"), &outcome(InvariantStatus::Skipped("none".into())), true, &fake);
        assert!(fake.log.borrow().is_empty());
    }

    #[test]
    fn an_unconfigured_repo_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        if Path::new(crate::forge_egress::policy::MACHINE_POLICY_PATH).exists()
            || std::env::var_os(crate::forge_egress::policy::POLICY_ENV).is_some()
        {
            return;
        }
        assert!(matches!(check(tmp.path()), InvariantStatus::Skipped(_)));
    }
}
