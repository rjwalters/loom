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
//! - `loom-daemon forge disable-auto-merge <pr> [--audit-comment [--hold L]]`
//!   (CLI; see [`handle_disable_auto_merge`]).
//! - [`crate::claim_reconciliation`]'s `invalidate_verdict()`, via
//!   `claim_reconciliation::auto_merge_disarm` (in-process, no CLI hop).
//! - `defaults/scripts/verdict-staleness-guard.sh --clear`, which **shells out
//!   to the CLI verb above** with `--audit-comment` rather than mirroring the
//!   mutation inline. The first draft of #8900 did mirror it in `gh api
//!   graphql`, and that duplication was rejected in review (PR #8990): it is
//!   exactly the new portable shell `.loom/docs/shell-language-policy.md`
//!   forbids and the shell-budget ratchet refuses. The consequence to keep in
//!   mind when changing this module: the guard's disarm is only as available as
//!   `loom-daemon` is on that host. The guard reports `AUTO_MERGE_DISARMED=0`
//!   and names the failure in its `REASON=` when the binary cannot be resolved,
//!   so the gap is loud rather than silent, and the periodic
//!   `claim_reconciliation` backstop still covers the same PRs on a host that
//!   runs a daemon at all.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::cmd_out::{run_command, CmdOutcome};

/// The mutation, verbatim, and the **only** copy of it in the repo — the shell
/// guard shells out to this module's CLI verb rather than mirroring it (see
/// "Callers" above), so there is nothing left to keep in sync by eye.
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
///
/// **Both fields are optional** (`#[serde(default)]`). The module doc above
/// promises that absent fields — Gitea behind a shim, an older `gh` — fail safe
/// to "not armed", and a *required* `id` broke that promise: a response with no
/// `id` key at all failed to deserialize and surfaced as [`Disarm::Failed`]
/// rather than [`Disarm::NotArmed`] (PR #8990 review). Missing or empty `id` is
/// now `Ok(None)`, matching `verdict-staleness-guard.sh`'s own shell reading
/// (empty node id ⇒ nothing to disarm). Only a `gh` invocation that actually
/// FAILED, or JSON that will not parse at all, is `Err`.
fn read_arm_state(gh_bin: &Path, cwd: Option<&Path>, pr: u32) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct AutoMergeRequest {}
    #[derive(serde::Deserialize)]
    struct PrArmState {
        #[serde(default)]
        id: Option<String>,
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
            // Armed but no node id to address the mutation to: there is nothing
            // this module can act on, and "not armed" is the fail-safe reading
            // (see the doc above) — never `Failed`, which callers report as
            // "assume still armed".
            Ok(parsed.id.filter(|id| !id.is_empty()))
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

/// The audit comment recording what a disarm attempt did, or `None` when there
/// is nothing to record.
///
/// `None` for [`Disarm::NotArmed`] is load-bearing: the overwhelmingly common
/// case must leave no trace at all, or every ordinary stale-verdict clear (and
/// every *held* PR an agent merely looks at) collects a comment saying nothing
/// happened.
///
/// `hold` is the explicit-hold label (`loom:operator` / `loom:blocked` /
/// `loom:operator-only`) when the caller found the PR parked, and it only
/// changes the *wording*, never whether the disarm ran. Disarming can only
/// **prevent** a merge, so it is the one write that enforces a hold rather than
/// undoing it — an armed queue would merge the parked PR the moment required
/// checks passed. The wording says so, because a write on a held PR that looks
/// unexplained is exactly what an operator reads as the engine ignoring them.
#[must_use]
pub fn audit_comment_body(pr: u32, outcome: &Disarm, hold: Option<&str>) -> Option<String> {
    let held = hold.map(|label| {
        format!(
            "\n\nThis PR carries `{label}`, so **no labels were changed and no verdict was \
             cleared** — the hold is respected. The disarm is the exception on purpose: it can \
             only *prevent* a merge, and an armed queue would have merged this PR regardless of \
             the hold."
        )
    });
    let footer = "\n\n---\n*Automated by `loom-daemon forge disable-auto-merge` (#8900)*";
    match outcome {
        Disarm::NotArmed => None,
        Disarm::Disarmed => Some(format!(
            "<!-- loom:auto-merge-disarmed pr={pr} -->\n\
             **GitHub auto-merge disarmed** (`disablePullRequestAutoMerge`)\n\n\
             A queued server-side merge was armed on this PR and has been stood down, because the \
             review verdict covering this tree was just invalidated. An armed auto-merge is gated \
             ONLY by the branch ruleset's REQUIRED checks: it never re-reads `loom:pr`, never \
             notices the verdict being cleared, and never runs `merge-pr.sh`'s own merge-time \
             gates — so it would have merged this unreviewed head anyway (#8694, #8847 and #8843 \
             all merged that way on 2026-09-25).{held}{footer}",
            held = held.unwrap_or_default(),
        )),
        Disarm::Failed(reason) => Some(format!(
            "<!-- loom:auto-merge-disarm-failed pr={pr} -->\n\
             ⚠️ **A server-side auto-merge may still be armed on this PR and could not be \
             disabled**\n\n\
             `{reason}`\n\n\
             If one is armed it may merge this unreviewed head as soon as the ruleset's required \
             checks pass. Disarm it by hand — `loom-daemon forge disable-auto-merge {pr}` or \
             `gh pr merge --disable-auto {pr}` — or apply `loom:operator`.{held}{footer}",
            held = held.unwrap_or_default(),
        )),
    }
}

/// Post [`audit_comment_body`]'s comment, when there is one. Best effort: a
/// failed comment is warned about on stderr and never changes the verb's exit
/// code, because the disarm itself has already happened and reporting it is
/// strictly less important than having done it.
fn post_audit_comment(gh_bin: &Path, pr: u32, outcome: &Disarm, hold: Option<&str>) {
    let Some(body) = audit_comment_body(pr, outcome, hold) else {
        return;
    };
    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("comment")
        .arg(pr.to_string())
        .arg("--body")
        .arg(body);
    apply_repo_override(&mut cmd);
    cmd.stdin(Stdio::null());
    let out = run_command(cmd, crate::forge_cmd::FORGE_CMD_TIMEOUT);
    if !out.succeeded() {
        eprintln!("Warning: could not post the auto-merge audit comment on PR #{pr}: {out:?}");
    }
}

/// Handle `loom-daemon forge disable-auto-merge <pr> [--audit-comment [--hold
/// <label>]]`. Never returns (exits).
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
///
/// With `audit_comment`, whatever the disarm actually did is also recorded as a
/// PR comment ([`audit_comment_body`]) — nothing is posted when nothing was
/// armed. This is what `verdict-staleness-guard.sh --clear` uses: the guard
/// delegates the whole disarm (mutation *and* audit trail) to this verb rather
/// than mirroring the mutation and two comment bodies in portable shell, which
/// is both the language policy (`.loom/docs/shell-language-policy.md`) and what
/// the shell-budget ratchet requires.
pub fn handle_disable_auto_merge(pr: u32, audit_comment: bool, hold: Option<&str>) -> Result<()> {
    if crate::forge_cmd::detect_forge(None) == crate::forge_cmd::ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge disable-auto-merge: gitea has no server-side auto-merge arm to \
             disable (nothing was ever armed); skipping"
        );
        std::process::exit(crate::forge_cmd::EX_FORGE_DECLINED);
    }
    // An empty `--hold ""` means "not held": the shell caller passes its
    // HOLD_LABEL unconditionally so the invocation needs no conditional
    // argument assembly (see verdict-staleness-guard.sh).
    let hold = hold.filter(|label| !label.trim().is_empty());
    let gh = crate::forge_cmd::gh_bin();
    let outcome = disarm_auto_merge(Path::new(&gh), None, pr);
    if audit_comment {
        post_audit_comment(Path::new(&gh), pr, &outcome, hold);
    }
    match outcome {
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

    /// An armed PR whose response carries **no `id` key at all** is also "not
    /// armed" (PR #8990 review). The module doc promises absent fields fail safe
    /// to `NotArmed`; a required `id` field made that case `Failed`, which
    /// callers report to operators as "assume still armed, disarm it by hand".
    /// There is nothing to disarm without a node id, and this matches the shell
    /// guard's own reading (empty node id ⇒ nothing armed).
    #[test]
    fn armed_without_a_node_id_is_not_armed_not_failed() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(
            dir.path(),
            &log,
            r#"{"autoMergeRequest":{"enabledAt":"2026-09-22T22:09:18Z"}}"#,
            0,
        );

        assert_eq!(disarm_auto_merge(&gh, Some(dir.path()), 46), Disarm::NotArmed);
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !calls.contains("graphql"),
            "no node id means no addressable mutation: {calls:?}"
        );

        // An explicitly EMPTY id is the same answer, for the same reason.
        let log2 = dir.path().join("gh2.log");
        let gh2 = fake_gh(
            dir.path(),
            &log2,
            r#"{"id":"","autoMergeRequest":{"enabledAt":"2026-09-22T22:09:18Z"}}"#,
            0,
        );
        assert_eq!(disarm_auto_merge(&gh2, Some(dir.path()), 47), Disarm::NotArmed);
    }

    /// `audit_comment_body` must stay silent on the common path: an ordinary
    /// stale-verdict clear (nothing armed) must leave no comment behind, or
    /// every clear — and every *held* PR an agent merely looks at — collects a
    /// comment saying nothing happened.
    #[test]
    fn audit_comment_is_silent_when_nothing_was_armed() {
        assert_eq!(audit_comment_body(42, &Disarm::NotArmed, None), None);
        assert_eq!(audit_comment_body(42, &Disarm::NotArmed, Some("loom:operator")), None);
    }

    /// The disarm comment records what happened, names the hazard, and — on a
    /// held PR — explains why a parked PR was written to at all.
    #[test]
    fn audit_comment_records_the_disarm_and_explains_a_hold() {
        let plain = audit_comment_body(8694, &Disarm::Disarmed, None).unwrap();
        assert!(plain.contains("auto-merge disarmed"), "{plain}");
        assert!(plain.contains("REQUIRED checks"), "{plain}");
        assert!(plain.contains("#8694"), "{plain}");
        assert!(!plain.contains("loom:operator"), "no hold was passed: {plain}");

        let held = audit_comment_body(8694, &Disarm::Disarmed, Some("loom:operator")).unwrap();
        assert!(held.contains("loom:operator"), "{held}");
        assert!(
            held.contains("no labels were changed"),
            "the held wording must say the hold was respected: {held}"
        );
        assert!(
            held.contains("only *prevent* a merge"),
            "the held wording must explain why the disarm is the exception: {held}"
        );
    }

    /// A FAILED disarm must never produce the "disarmed" wording — the comment
    /// has to say the queue may still fire, with the hand-disarm command.
    #[test]
    fn audit_comment_for_a_failure_warns_instead_of_claiming_a_disarm() {
        let body = audit_comment_body(8694, &Disarm::Failed("gh: boom".to_string()), None).unwrap();
        assert!(!body.contains("auto-merge disarmed"), "{body}");
        assert!(body.contains("could not be"), "{body}");
        assert!(body.contains("gh: boom"), "the reason must be quoted: {body}");
        assert!(
            body.contains("loom-daemon forge disable-auto-merge 8694"),
            "the hand-disarm command must be named: {body}"
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
