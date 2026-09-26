//! `disablePullRequestAutoMerge` — disarm GitHub's server-side auto-merge
//! queue on one PR (issue #8900).
//!
//! # Why this exists
//!
//! An armed GitHub auto-merge is gated **only** by the branch ruleset's
//! REQUIRED checks. It never re-reads `loom:pr`, never notices a
//! `loom:verdict-stale` revocation, never waits for a non-required suite, and
//! never runs `merge-pr.sh`'s own merge-time gates (including the #8248
//! required-check-freshness guard, which lives inside that script). So once it
//! is armed, invalidating the verdict does nothing at all: the queued merge
//! fires the moment required checks go green on the **new, unreviewed** head.
//!
//! Observed live on 2026-09-25: #8694 merged as `528f2971` three minutes after
//! a Doctor rebase force-push, still labeled `loom:review-requested`, with no
//! approval at the merged head (auto-squash armed 2026-09-22); #8847 and #8843
//! merged ~2 minutes after `gh pr update-branch` moved their heads and the
//! stale-verdict notice had just cleared `loom:pr`.
//!
//! No Loom merge path **arms** one since #8410/#8427 (`merge-pr.sh --auto`
//! waits for check-runs and merges in-process; the shell `forge_auto_merge`
//! helper is retired; `loom-daemon forge auto-merge` survives only as an
//! operator escape hatch and a pre-#8410 CLI compatibility surface). But an arm
//! that predates a host's `resync-installed.sh`, or a deliberate operator arm,
//! can still be standing on a live PR — and nothing disarmed it. This module is
//! the disarm.
//!
//! # Not operator-gated, deliberately
//!
//! Unlike `forge auto-merge`, this verb is **monotonically safety-increasing**:
//! it can only turn a queued merge *off*, never on. There is no state in which
//! calling it makes an unreviewed merge more likely, so it carries no
//! operator-only restriction and is safe to call from any verdict-invalidation
//! path.
//!
//! # Callers
//!
//! - `loom-daemon forge disable-auto-merge <pr>` (CLI; see
//!   [`handle_disable_auto_merge`]).
//! - [`crate::claim_reconciliation`]'s `invalidate_verdict()`, via
//!   `claim_reconciliation::auto_merge_disarm`.
//! - `defaults/scripts/verdict-staleness-guard.sh --clear`, which mirrors the
//!   same mutation inline with `gh api graphql` (it hard-requires only `gh` +
//!   `jq` and runs on hosts that may not have `loom-daemon` on PATH — the same
//!   deliberate shell/Rust parallel implementation the stale-verdict guard
//!   itself already has).

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::cmd_out::{run_command, CmdOutcome};

/// The mutation, verbatim. Kept as one `const` so the `gh api graphql` mirror
/// in `defaults/scripts/verdict-staleness-guard.sh` can be diffed against it by
/// eye.
///
/// `disablePullRequestAutoMerge` takes no `expectedHeadOid` precondition (and
/// needs none): disarming is idempotent and can never merge anything, so there
/// is no head-moved race to guard against — the opposite of
/// `enablePullRequestAutoMerge`, where #5589 had to add one.
pub const DISABLE_AUTO_MERGE_MUTATION: &str = "mutation($pullRequestId: ID!) {\
  disablePullRequestAutoMerge(input: { pullRequestId: $pullRequestId }) {\
    pullRequest { number autoMergeRequest { enabledAt } }\
  }\
}";

/// What one disarm attempt actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disarm {
    /// Nothing was armed on this PR — **no mutation was sent**. The common
    /// case, and the reason the arm state is read first: it keeps an ordinary
    /// stale-verdict clear at zero extra write cost and stops a caller from
    /// claiming a disarm that never happened (#8900 acceptance criterion).
    NotArmed,
    /// An armed auto-merge was disabled.
    Disarmed,
    /// The arm state could not be read, or the mutation failed. Carries a
    /// human-readable reason. Callers treat this as "assume still armed" and
    /// say so — never as `NotArmed`.
    Failed(String),
}

impl Disarm {
    /// Did a disarm actually happen? Used by callers to decide whether to
    /// claim one in their audit comment.
    #[must_use]
    pub fn disarmed(&self) -> bool {
        matches!(self, Disarm::Disarmed)
    }
}

/// Append `--repo $LOOM_REPO` when the daemon is operating on a repo other
/// than the one `cwd` belongs to, mirroring
/// [`crate::claim_reconciliation`]'s own `gh` invocations.
fn apply_repo_override(cmd: &mut Command) {
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
}

/// Read the PR's arm state and GraphQL node id in ONE `gh` call.
///
/// `Ok(Some(node_id))` = armed (and here is the id the mutation needs);
/// `Ok(None)` = definitively not armed; `Err(reason)` = could not tell.
///
/// `autoMergeRequest` and `id` come from the same snapshot on purpose: reading
/// them separately would let the arm state and the node id disagree.
fn read_arm_state(gh_bin: &Path, cwd: Option<&Path>, pr: u32) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct AutoMergeRequest {}
    #[derive(serde::Deserialize)]
    struct PrArmState {
        id: String,
        #[serde(default, rename = "autoMergeRequest")]
        auto_merge_request: Option<AutoMergeRequest>,
    }

    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("view")
        .arg(pr.to_string())
        .arg("--json")
        .arg("id,autoMergeRequest");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        // #5401: a cross-owner managed repo needs its own owner's
        // installation-token GH_CONFIG_DIR (no-op for single-owner fleets).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    apply_repo_override(&mut cmd);
    cmd.stdin(Stdio::null());

    let out = run_command(cmd, crate::forge_cmd::FORGE_CMD_TIMEOUT);
    match out {
        ref o if o.succeeded() => {
            let stdout = o.stdout_lossy();
            let parsed: PrArmState = serde_json::from_str(&stdout).map_err(|e| {
                format!("could not decode `gh pr view {pr} --json id,autoMergeRequest`: {e}")
            })?;
            if parsed.auto_merge_request.is_none() {
                return Ok(None);
            }
            if parsed.id.is_empty() {
                return Err(format!("PR #{pr} reports an armed auto-merge but no GraphQL node id"));
            }
            Ok(Some(parsed.id))
        }
        CmdOutcome::Ran(ref o) => Err(format!(
            "`gh pr view {pr} --json id,autoMergeRequest` failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        CmdOutcome::Unavailable(ref u) => Err(format!("`gh pr view {pr}` could not be run: {u}")),
    }
}

/// Disarm GitHub's server-side auto-merge on `pr`, if anything is armed.
///
/// Best effort and idempotent: a PR with nothing armed costs one read and
/// returns [`Disarm::NotArmed`] without sending a mutation. Never panics and
/// never returns `NotArmed` for a failure — an unreadable arm state is
/// [`Disarm::Failed`], so a caller cannot mistake "could not tell" for
/// "confirmed nothing to do".
///
/// `cwd` is the repo root to run `gh` in (`None` = inherit the process cwd,
/// which is what the CLI path wants).
#[must_use]
pub fn disarm_auto_merge(gh_bin: &Path, cwd: Option<&Path>, pr: u32) -> Disarm {
    let node_id = match read_arm_state(gh_bin, cwd, pr) {
        Ok(None) => return Disarm::NotArmed,
        Ok(Some(id)) => id,
        Err(reason) => return Disarm::Failed(reason),
    };

    let mut cmd = Command::new(gh_bin);
    cmd.arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={DISABLE_AUTO_MERGE_MUTATION}"))
        .arg("-F")
        .arg(format!("pullRequestId={node_id}"));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    // NOTE: no `--repo` here. `gh api graphql` has no such flag — the PR is
    // addressed by its global node id, which is repo-independent.
    cmd.stdin(Stdio::null());

    let out = run_command(cmd, crate::forge_cmd::FORGE_CMD_TIMEOUT);
    match out {
        ref o if o.succeeded() => Disarm::Disarmed,
        CmdOutcome::Ran(ref o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let detail = if err.trim().is_empty() {
                stdout.trim()
            } else {
                err.trim()
            };
            Disarm::Failed(format!("disablePullRequestAutoMerge failed for PR #{pr}: {detail}"))
        }
        CmdOutcome::Unavailable(ref u) => Disarm::Failed(format!(
            "disablePullRequestAutoMerge for PR #{pr} could not be run: {u}"
        )),
    }
}

/// Handle `loom-daemon forge disable-auto-merge <pr>`. Never returns (exits).
///
/// Exit codes:
/// - `0` — nothing needed doing (`DISARMED=0`) **or** an armed auto-merge was
///   disabled (`DISARMED=1`). Both are success; the stdout line says which, so
///   a caller that wants to report the disarm can read it without a second
///   API call.
/// - `1` — the arm state could not be read, or the mutation failed. Callers
///   must treat this as "possibly still armed", never as an all-clear.
/// - [`crate::forge_cmd::EX_FORGE_DECLINED`] (`3`) — Gitea. There is no
///   server-side auto-merge arm to disable there (`forge auto-merge` has no
///   Gitea implementation either, #8427), so there is nothing this verb can
///   do; declining keeps that distinguishable from a genuine failure.
pub fn handle_disable_auto_merge(pr: u32) -> Result<()> {
    if crate::forge_cmd::detect_forge(None) == crate::forge_cmd::ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge disable-auto-merge: gitea has no server-side auto-merge arm to \
             disable (nothing was ever armed); skipping"
        );
        std::process::exit(crate::forge_cmd::EX_FORGE_DECLINED);
    }
    let gh = crate::forge_cmd::gh_bin();
    match disarm_auto_merge(Path::new(&gh), None, pr) {
        Disarm::NotArmed => {
            println!("DISARMED=0");
            println!("No auto-merge was armed on PR #{pr}; nothing to disable.");
            std::process::exit(0);
        }
        Disarm::Disarmed => {
            println!("DISARMED=1");
            println!("Disabled GitHub auto-merge on PR #{pr}.");
            std::process::exit(0);
        }
        Disarm::Failed(reason) => {
            eprintln!("Failed to disable auto-merge for PR #{pr}: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Write a fake `gh` that logs every invocation to `log` and answers
    /// `pr view --json id,autoMergeRequest` with `arm_json`, then reports
    /// `mutation_rc` for `api graphql`.
    fn fake_gh(dir: &Path, log: &Path, arm_json: &str, mutation_rc: i32) -> std::path::PathBuf {
        let bin = dir.join("fake-gh.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s' '{arm_json}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  if [ "{mutation_rc}" != "0" ]; then
    echo 'gh: GraphQL: Something went wrong (disablePullRequestAutoMerge)' 1>&2
    exit {mutation_rc}
  fi
  echo '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":1,"autoMergeRequest":null}}}}}}}}'
  exit 0
fi
exit 0
"#,
            log = log.display(),
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    /// The #8694 shape: auto-merge IS armed, so the mutation must fire with
    /// the node id read from the same snapshot.
    #[test]
    fn armed_pr_is_disarmed_via_the_mutation() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(
            dir.path(),
            &log,
            r#"{"id":"PR_kwDOQAPbH88AAAABEp4dZw","autoMergeRequest":{"enabledAt":"2026-09-22T22:09:18Z"}}"#,
            0,
        );

        assert_eq!(disarm_auto_merge(&gh, Some(dir.path()), 8694), Disarm::Disarmed);

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("pr view 8694 --json id,autoMergeRequest"),
            "arm state must be read in one call: {calls:?}"
        );
        assert!(
            calls.contains("disablePullRequestAutoMerge"),
            "the disable mutation must be sent: {calls:?}"
        );
        assert!(
            calls.contains("pullRequestId=PR_kwDOQAPbH88AAAABEp4dZw"),
            "the mutation must use the node id from the same snapshot: {calls:?}"
        );
    }

    /// The common case: nothing armed => NO mutation at all (no wasted API
    /// call, and no caller can claim a disarm that never happened).
    #[test]
    fn unarmed_pr_sends_no_mutation() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"id":"PR_kwDOabc","autoMergeRequest":null}"#, 0);

        assert_eq!(disarm_auto_merge(&gh, Some(dir.path()), 42), Disarm::NotArmed);

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !calls.contains("graphql"),
            "an unarmed PR must not trigger a mutation: {calls:?}"
        );
    }

    /// An entirely ABSENT `autoMergeRequest` key (a forge shim, an older `gh`)
    /// is also "not armed" — same direction as a literal `null`.
    #[test]
    fn absent_auto_merge_field_is_not_armed() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"id":"PR_kwDOabc"}"#, 0);

        assert_eq!(disarm_auto_merge(&gh, Some(dir.path()), 43), Disarm::NotArmed);
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!calls.contains("graphql"), "{calls:?}");
    }

    /// A failing mutation is `Failed`, never `NotArmed` — a caller must not be
    /// able to read "could not disarm" as "nothing to disarm".
    #[test]
    fn failing_mutation_is_reported_as_failed() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(
            dir.path(),
            &log,
            r#"{"id":"PR_kwDOabc","autoMergeRequest":{"enabledAt":"2026-09-22T22:09:18Z"}}"#,
            1,
        );

        match disarm_auto_merge(&gh, Some(dir.path()), 44) {
            Disarm::Failed(reason) => {
                assert!(
                    reason.contains("disablePullRequestAutoMerge"),
                    "reason should name the mutation: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// An unreadable arm state is `Failed` too — same reason.
    #[test]
    fn unreadable_arm_state_is_failed_not_not_armed() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("broken-gh.sh");
        std::fs::write(&bin, "#!/usr/bin/env bash\necho 'gh: boom' 1>&2\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        match disarm_auto_merge(&bin, Some(dir.path()), 45) {
            Disarm::Failed(reason) => assert!(reason.contains("gh pr view"), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The mutation text must actually be the disable one — a copy-paste of
    /// `enablePullRequestAutoMerge` here would be a silent, catastrophic
    /// regression (it would ARM the merge this module exists to prevent).
    #[test]
    fn mutation_is_the_disable_one_and_takes_no_merge_method() {
        assert!(DISABLE_AUTO_MERGE_MUTATION.contains("disablePullRequestAutoMerge"));
        assert!(!DISABLE_AUTO_MERGE_MUTATION.contains("enablePullRequestAutoMerge"));
        assert!(
            !DISABLE_AUTO_MERGE_MUTATION.contains("mergeMethod"),
            "disabling takes no merge method"
        );
    }
}
