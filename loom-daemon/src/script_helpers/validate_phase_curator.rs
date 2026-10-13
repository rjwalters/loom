//! Curator recovery for the `validate-phase` contract (#11787).
//!
//! Split out of [`super::validate_phase`] (a file-size-ratchet-frozen module):
//! the `loom:curated` label is applied *only* by `curation-complete`, never bare.

use std::path::Path;
use std::process::Command;

use super::run_gh;
use super::validate_phase::{report_milestone, ValidationResult, ValidationStatus};

/// Recovery branch of [`validate_curator`], split out so the policy is testable:
/// the label is applied *only* by `complete`; when it fails the issue is left
/// as-is (in `loom:curating`) and the failure is reported, not papered over.
pub(super) fn recover_curator_with(
    issue: i64,
    task_id: Option<&str>,
    repo_root: &Path,
    complete: &dyn Fn() -> Result<(), String>,
) -> ValidationResult {
    match complete() {
        Ok(()) => {
            report_milestone(
                task_id,
                repo_root,
                "recovery: curation-complete applied loom:curated",
            );
            ValidationResult::new(
                "curator",
                issue,
                ValidationStatus::Recovered,
                "Ran curation-complete (journaled estimate, applied loom:curated)",
            )
            .with_action("curation_complete")
        }
        Err(why) => ValidationResult::new(
            "curator",
            issue,
            ValidationStatus::Failed,
            format!(
                "Issue left in loom:curating: curation-complete did not complete ({why}); \
                 write the points marker and run `loom-daemon curation-complete`"
            ),
        ),
    }
}

/// Run `loom-daemon curation-complete` for `issue` with the session's
/// provenance (`LOOM_CURATION_MODEL` / `LOOM_CURATION_RUNTIME`, else `unknown`).
pub(super) fn run_curation_complete(repo_root: &Path, issue: i64) -> Result<(), String> {
    let nwo = run_gh(
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
        repo_root,
        false,
    );
    let repo = nwo.stdout_trimmed();
    if !nwo.succeeded() || repo.is_empty() {
        return Err("could not resolve repository".to_string());
    }
    let env_or = |k: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe)
        .args([
            "curation-complete",
            "--repo",
            &repo,
            "--issue",
            &issue.to_string(),
        ])
        .args(["--model", &env_or("LOOM_CURATION_MODEL")])
        .args(["--runtime", &env_or("LOOM_CURATION_RUNTIME")])
        .args(["--completion-id", &uuid::Uuid::new_v4().to_string()])
        .arg("--workspace-root")
        .arg(repo_root)
        .current_dir(repo_root)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .next()
            .unwrap_or("non-zero exit")
            .chars()
            .take(200)
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn curator_recovery_failure_leaves_issue_curating() {
        let r = recover_curator_with(7, None, Path::new("."), &|| Err("no marker".into()));
        assert_eq!(r.status, ValidationStatus::Failed);
        assert!(r.message.contains("loom:curating"), "{}", r.message);
        assert_eq!(r.recovery_action, "none");
    }

    #[test]
    fn curator_recovery_success_goes_through_curation_complete() {
        let r = recover_curator_with(7, None, Path::new("."), &|| Ok(()));
        assert_eq!(r.status, ValidationStatus::Recovered);
        assert_eq!(r.recovery_action, "curation_complete");
    }

    #[test]
    fn validate_curator_has_no_bare_curated_label_write() {
        let src = include_str!("validate_phase.rs");
        let body = src.split("pub fn validate_curator").nth(1).unwrap();
        let body = body.split("/// Judge contract").next().unwrap();
        assert!(!body.contains("\"--add-label\""), "bare label write in curator recovery");
    }

    /// #11787 doc-lint: no shipped prompt/skill/CLAUDE.md may tell an agent to
    /// set `loom:curated` with a bare `--add-label`; that skips the estimate gate.
    #[test]
    fn no_shipped_text_instructs_bare_curated_label() {
        fn walk(dir: &Path, hits: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, hits);
                } else if p.extension().is_some_and(|x| x == "md") {
                    let Ok(txt) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    // Join wrapped lines so `--add-label\n"loom:curated"` is seen.
                    let flat = txt.split_whitespace().collect::<Vec<_>>().join(" ");
                    for pat in [
                        "--add-label loom:curated",
                        "--add-label \"loom:curated\"",
                        "--add-label 'loom:curated'",
                        "--add-label=loom:curated",
                    ] {
                        if flat.contains(pat) {
                            hits.push(format!("{}: {pat}", p.display()));
                        }
                    }
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut hits = Vec::new();
        walk(&root.join("defaults"), &mut hits);
        for f in ["CLAUDE.md"] {
            let txt = std::fs::read_to_string(root.join(f)).unwrap_or_default();
            let flat = txt.split_whitespace().collect::<Vec<_>>().join(" ");
            if flat.contains("--add-label \"loom:curated\"")
                || flat.contains("--add-label loom:curated")
            {
                hits.push(f.to_string());
            }
        }
        assert!(
            hits.is_empty(),
            "bare loom:curated label instructions (use curation-complete): {hits:?}"
        );
    }
}
