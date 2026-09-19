//! The PR's currently-applied state labels plus its draft status.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};

/// The PR's currently-applied state labels plus its draft status (a
/// best-effort subset of `--json labels,isDraft`, used only to decide
/// whether [`super::forge::reclaim_pr`]'s safety net needs to fire, and
/// whether it is safe to backfill `loom:review-requested` at all). A `gh`
/// failure here degrades to an empty label list and `is_draft: false` —
/// see `reclaim_pr`'s call site for why that is the safe direction: it
/// makes the safety net fire (adds `loom:review-requested`) rather than
/// silently leaving a PR with no state label at all. Likewise, an
/// absent/unparseable `isDraft` field defaults to `false` (non-draft),
/// preserving that same fail-safe direction rather than silently
/// skipping the backfill.
#[derive(Debug, Default)]
pub(super) struct PrLabelInfo {
    pub(super) labels: Vec<String>,
    pub(super) is_draft: bool,
}

pub(super) fn pr_label_names(gh_bin: &Path, root: &Path, pr_number: u32) -> Result<PrLabelInfo> {
    #[derive(Debug, Deserialize)]
    struct GhLabel {
        name: String,
    }
    #[derive(Debug, Default, Deserialize)]
    struct GhPrLabels {
        labels: Vec<GhLabel>,
        #[serde(default, rename = "isDraft")]
        is_draft: bool,
    }
    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("view")
        .arg(pr_number.to_string())
        .arg("--json")
        .arg("labels,isDraft");
    cmd.current_dir(root);
    // #5401: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr view {pr_number} --json labels,isDraft failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parsed: GhPrLabels =
        serde_json::from_slice(&out.stdout).context("parse gh pr view labels,isDraft JSON")?;
    Ok(PrLabelInfo {
        labels: parsed.labels.into_iter().map(|l| l.name).collect(),
        is_draft: parsed.is_draft,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::forge;
    use super::super::{STALE_REVIEWING_MINUTES_ENV, STALE_TREATING_MINUTES_ENV};
    use crate::sweep_journal;
    use chrono::{Duration, Utc};
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Write a fake `gh` script (tests only) that logs every invocation to
    /// `gh_log`, reports exactly one PR carrying the requested claim label
    /// for `pr list`, and reports `extra_labels` plus `is_draft` for
    /// `pr view --json labels,isDraft` — used to reproduce #8250 (a stale
    /// claim reclaimed on a draft PR must not be backfilled
    /// `loom:review-requested`).
    fn write_fake_gh_pr_with_draft(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        pr_number: u32,
        updated_at: &str,
        head_ref_name: &str,
        extra_labels: &[&str],
        is_draft: bool,
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh-pr.sh");
        let labels_json = extra_labels
            .iter()
            .map(|l| format!(r#"{{"name":"{l}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":{pr_number},"updatedAt":"{updated_at}","headRefName":"{head_ref_name}"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[{labels_json}],"isDraft":{is_draft}}}'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        fake_gh
    }

    /// #8250: a stale `loom:reviewing`/`loom:treating` claim reclaimed off a
    /// **draft** PR (`isDraft: true`, no state label) must NOT be backfilled
    /// `loom:review-requested` -- Judge is not ready to look at a draft, so
    /// the backfill would just waste a review pass. The claim label itself
    /// must still be removed (the reclaim happens; only the backfill is
    /// skipped).
    #[test]
    #[serial]
    fn reconcile_pr_claims_reclaims_stale_claim_but_skips_backfill_on_draft_pr() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
        std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

        let gh_log = dir.path().join("gh-invocations.log");
        let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
        let fake_gh = write_fake_gh_pr_with_draft(
            dir.path(),
            &gh_log,
            502,
            &old,
            "some-random-branch",
            &[],
            true,
        );

        let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

        assert!(checked >= 1);
        assert!(reclaimed >= 1, "the stale claim label must still be reclaimed");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("pr edit 502 --remove-label loom:reviewing"),
            "expected loom:reviewing to be removed from #502; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--add-label loom:review-requested"),
            "a draft PR must never be backfilled loom:review-requested; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }

    /// #8250 edge case: if `isDraft` is absent from the `pr view` response
    /// (e.g. an older `gh` or a partial API response), the fail-safe default
    /// is non-draft -- matching the existing accepted failure class for a
    /// wholesale `gh pr view` failure (see `pr_label_names`'s doc comment)
    /// -- so the backfill still fires rather than silently getting skipped.
    #[test]
    #[serial]
    fn reconcile_pr_claims_backfills_when_is_draft_field_is_absent() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
        std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

        let gh_log = dir.path().join("gh-invocations.log");
        let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
        let fake_gh = dir.path().join("fake-gh-pr-no-draft-field.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":503,"updatedAt":"{old}","headRefName":"some-random-branch"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[]}}'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

        assert!(checked >= 1);
        assert!(reclaimed >= 1);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("pr edit 503 --add-label loom:review-requested"),
            "a missing isDraft field must default to non-draft, not panic/skip the backfill: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }
}
