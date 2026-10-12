//! The PR's currently-applied state labels plus its draft status.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

use super::gh_call;

/// The PR's currently-applied state labels plus its draft status (a
/// best-effort subset of the REST `pulls/{n}` body — `labels[].name` and
/// `draft` — used only to decide whether [`super::forge::reclaim_pr`]'s
/// safety net needs to fire, and whether it is safe to backfill
/// `loom:review-requested` at all). A `gh` failure here degrades to an empty
/// label list and `is_draft: false` — see `reclaim_pr`'s call site for why
/// that is the safe direction: it makes the safety net fire (adds
/// `loom:review-requested`) rather than silently leaving a PR with no state
/// label at all. Likewise, an absent/unparseable `draft` field defaults to
/// `false` (non-draft), preserving that same fail-safe direction rather than
/// silently skipping the backfill.
#[derive(Debug, Default)]
pub(super) struct PrLabelInfo {
    pub(super) labels: Vec<String>,
    pub(super) is_draft: bool,
}

/// One REST `pulls/{n}` read (#10507; was GraphQL `gh pr view --json
/// labels,isDraft`). It runs right after `reclaim_pr`'s own
/// `claim.pr_reclaim` write, so it is pinned to the WRITER identity
/// ([`gh_call::read_own_write`]): a reader App can lag that write (W4-C). No
/// ETag — the write just moved it, so a conditional read would always `200`.
/// `LOOM_REPO` reaches `gh api` as `GH_REPO` through the facade (#8263).
pub(super) fn pr_label_names(gh_bin: &Path, root: &Path, pr_number: u32) -> Result<PrLabelInfo> {
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}");
    let out = gh_call::output(
        gh_call::read_own_write("claim.pr_labels", gh_bin, root).args(["api", &path]),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        return Err(anyhow!("gh api {path} failed in {root}: {err}"));
    }
    parse_pull_labels(&out.stdout)
}

/// `labels[].name` + `draft` from a REST `pulls/{n}` body. A missing `draft`
/// is non-draft (the fail-safe direction above).
fn parse_pull_labels(body: &[u8]) -> Result<PrLabelInfo> {
    #[derive(Debug, Deserialize)]
    struct GhLabel {
        name: String,
    }
    #[derive(Debug, Default, Deserialize)]
    struct RestPull {
        labels: Vec<GhLabel>,
        #[serde(default)]
        draft: bool,
    }
    let parsed: RestPull =
        serde_json::from_slice(body).context("parse REST pulls/{n} labels,draft JSON")?;
    Ok(PrLabelInfo {
        labels: parsed.labels.into_iter().map(|l| l.name).collect(),
        is_draft: parsed.draft,
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
    /// for `pr list`, and reports `extra_labels` plus `draft` for the REST
    /// `pulls/{n}` read (the retired `gh pr view` is refused with exit 97 and
    /// a logged `FORBIDDEN`, #10507) — used to reproduce #8250 (a stale
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
case "$*" in
  'pr view'*) echo FORBIDDEN >> "{log}"; exit 97 ;;
  "api repos/{{owner}}/{{repo}}/pulls/{pr_number}")
    echo '{{"labels":[{labels_json}],"draft":{is_draft}}}'; exit 0 ;;
esac
{pulls}exit 0
"#,
            log = gh_log.display(),
            pulls = super::super::open_pr_listing::test_support::pulls_arm(&[
                super::super::open_pr_listing::test_support::row(pr_number, &["loom:reviewing"])
                    .head(head_ref_name)
                    .updated(updated_at)
            ]),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        fake_gh
    }

    /// #8250: a stale `loom:reviewing`/`loom:treating` claim reclaimed off a
    /// **draft** PR (`draft: true`, no state label) must NOT be backfilled
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
        assert!(!gh_calls.contains("FORBIDDEN"), "a GraphQL read was attempted: {gh_calls}");
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

    /// #8250 edge case: if `draft` is absent from the `pulls/{n}` response
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
case "$*" in
  'pr view'*) echo FORBIDDEN >> "{log}"; exit 97 ;;
  "api repos/{{owner}}/{{repo}}/pulls/503") echo '{{"labels":[]}}'; exit 0 ;;
esac
{pulls}exit 0
"#,
            log = gh_log.display(),
            pulls = super::super::open_pr_listing::test_support::pulls_arm(&[
                super::super::open_pr_listing::test_support::row(503, &["loom:reviewing"])
                    .head("some-random-branch")
                    .updated(&old)
            ]),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

        assert!(checked >= 1);
        assert!(reclaimed >= 1);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(!gh_calls.contains("FORBIDDEN"), "a GraphQL read was attempted: {gh_calls}");
        assert!(
            gh_calls.contains("pr edit 503 --add-label loom:review-requested"),
            "a missing draft field must default to non-draft, not panic/skip the backfill: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }

    #[test]
    fn the_rest_pull_body_parses_labels_and_draft() {
        let info = super::parse_pull_labels(
            br#"{"number":7,"draft":true,"labels":[{"name":"loom:pr","id":1}]}"#,
        )
        .unwrap();
        assert_eq!((info.labels, info.is_draft), (vec!["loom:pr".to_string()], true));
        let info = super::parse_pull_labels(br#"{"labels":[]}"#).unwrap();
        assert!(!info.is_draft, "a missing draft field is non-draft");
        assert!(super::parse_pull_labels(b"not json").is_err());
    }
}
