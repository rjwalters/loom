//! The bare `gh pr create` recovery fallback of `validate-builder` (#10518),
//! split out of `validate_phase.rs` to keep that file under its size ratchet.

use crate::cmd_out::CmdOutcome;

/// The bare `gh pr create` fallback (`create-pr.sh` absent or unspawnable).
///
/// The script posts #10012 §2's `inherited_from=#N` audit itself; this path
/// must too, or the copied star reads as the operator's own and removing the
/// issue's star could never remove it (#10518). Only after a successful
/// creation, and best-effort: a failed audit warns and never un-opens the PR.
/// `gh` and the audit are injected so a test can exercise the fallback.
pub(super) fn create_via_gh(
    args: &[&str],
    starred: bool,
    gh: impl FnOnce(&[&str]) -> CmdOutcome,
    audit: impl FnOnce(&str) -> anyhow::Result<usize>,
) -> CmdOutcome {
    let created = gh(&[&["pr", "create"][..], args].concat());
    if starred && created.succeeded() {
        let out = created.stdout_trimmed();
        let url = out.lines().last().unwrap_or_default();
        if let Err(e) = audit(url) {
            eprintln!(
                "validate-builder: note: could not post the inherited-star audit on {url}: \
                 {e:#} (best-effort, #10518)"
            );
        }
    }
    created
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_outcome(code: i32, stdout: &str) -> CmdOutcome {
        use std::os::unix::process::ExitStatusExt;
        CmdOutcome::Ran(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    /// #10518: the missing-script fallback applies the star AND posts the audit.
    #[test]
    fn the_gh_fallback_labels_the_pr_and_audits_the_copied_star() {
        // `create_via_gh` is the path taken when create-pr.sh is absent.
        let star = ["loom:operator-priority".to_string()];
        let (argv, audited) =
            (std::cell::RefCell::new(String::new()), std::cell::RefCell::new(Vec::new()));
        let r = {
            create_via_gh(
                &["--label", "loom:review-requested", "--label", &star[0]],
                true,
                |a| {
                    *argv.borrow_mut() = a.join(" ");
                    fake_outcome(0, "https://github.com/o/r/pull/9\n")
                },
                |url| {
                    audited.borrow_mut().push(url.to_string());
                    Ok(1)
                },
            )
        };
        assert!(r.succeeded());
        assert!(argv.borrow().starts_with("pr create"), "argv: {}", argv.borrow());
        assert!(argv.borrow().contains("--label loom:operator-priority"));
        assert_eq!(*audited.borrow(), vec!["https://github.com/o/r/pull/9".to_string()]);
    }

    #[test]
    fn the_gh_fallback_posts_no_audit_when_creation_fails_or_nothing_is_starred() {
        let audited = std::cell::Cell::new(0);
        let count = |_: &str| {
            audited.set(audited.get() + 1);
            Ok(1)
        };
        let failed = create_via_gh(&[], true, |_| fake_outcome(1, ""), count);
        assert!(!failed.succeeded());
        let unstarred = create_via_gh(&[], false, |_| fake_outcome(0, "https://x/pull/1"), count);
        assert!(unstarred.succeeded());
        assert_eq!(audited.get(), 0, "no audit on failed creation or without a star");
    }

    #[test]
    fn a_failed_gh_fallback_audit_never_unopens_the_pr() {
        let r = create_via_gh(
            &[],
            true,
            |_| fake_outcome(0, "https://github.com/o/r/pull/3"),
            |_| Err(anyhow::anyhow!("denied")),
        );
        assert!(r.succeeded());
        assert_eq!(r.stdout_trimmed(), "https://github.com/o/r/pull/3");
    }
}
