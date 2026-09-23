//! Thin `gh` CLI wrappers shared by `clean.rs` / `aggressive.rs` /
//! `orphan_recovery.rs`.
//!
//! Mirrors the small slice of `loom_tools.common.github` (`gh_list`,
//! `gh_run`) these modules actually use. The `Command`-issuing wrappers are not
//! unit-tested directly — like `claim_reconciliation::forge` and
//! `work_finder::forge`, they are thin `Command` wrappers; the decision logic
//! that consumes their output lives in pure, fully-tested functions elsewhere
//! in `worktree_ops`. The exceptions are [`parse_open_linked_pr`] and
//! [`parse_open_linked_pr_timeline`], which ARE pure decision functions (the
//! `state == "OPEN"` closes-graph filter and the REST cross-reference union),
//! unit-tested at the bottom of this file along with both transports' argv.

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::proc_exec::{run_bounded, Completion};

/// The `gh` binary to invoke. Honors `LOOM_GH_BIN` (tests / overrides), the
/// same seam `forge_cmd::gh_bin`, `forge_cached_list`, and `role_collision`
/// already use — so a fixture can steer these helpers without mutating the
/// process-wide `PATH`, which races with every other concurrently-running
/// test's `Command` spawn.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

fn gh_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new(gh_bin());
    cmd.current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner). This is the
    // single choke point every helper in this module builds its `Command` through,
    // so wiring it here covers `clean.rs` / `aggressive.rs` / `orphan_recovery.rs`
    // without touching each call site individually.
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    cmd
}

/// Wall-clock deadline for every read-only `gh` probe this module and
/// `clean.rs` issue (issue #8708).
///
/// A `clean --workspace <ws> --deep --safe` on loom-worker-1 sat for 25
/// hours with its `gh api repos/{owner}/{repo}/pulls?...` child wedged in
/// `futex_do_wait`, pinning `loom-fleet-clean.service` in `activating` for
/// a day and starving the scheduled 4-hourly fleet clean. Every `gh` call
/// bounded here is a **read probe** — a hang must resolve as "no answer"
/// rather than outlive the pass: past this deadline the child's whole
/// process group is terminated (see [`crate::proc_exec`]) and the caller
/// maps the result to its existing fail-closed answer — `"UNKNOWN"`,
/// `None`, or `clean::PrStatus::Unknown` — never to "safe to delete".
/// Mutating `gh` calls (`edit_labels`, `comment`) stay unbounded on
/// purpose: a write whose transport died mid-flight may already have
/// landed, and "assume it failed" is not a safe default there.
pub(crate) const GH_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Run one read-only `gh` probe to completion under `timeout` (#8708).
///
/// The seam every bounded probe in this module and `clean.rs` executes
/// through. Returns `None` on deadline expiry (logged — the operational
/// signal this bound exists for), on spawn failure, and on
/// output-collection failure: exactly the "no answer" each caller's
/// previous `cmd.output().ok()` / `let Ok(..) else` handling already
/// mapped, now also covering the slow-hang side those handlers could not
/// see. A probe that **exits** — zero or nonzero — is a completed answer
/// and keeps its [`Output`], so each caller's existing
/// `!out.status.success()` fail-closed path keeps deciding those.
#[must_use]
pub(crate) fn bounded_output(mut cmd: Command, timeout: Duration) -> Option<Output> {
    // `Command::output()` nulls stdin when the caller left it unset; the
    // bounded runner leaves stdin to the caller by design, so preserve that
    // contract here rather than letting the child inherit the daemon's.
    cmd.stdin(std::process::Stdio::null());
    match run_bounded(cmd, timeout) {
        Ok(Completion::Exited(out)) => Some(out),
        Ok(Completion::TimedOut { .. }) => {
            eprintln!(
                "gh: probe exceeded {timeout:?} deadline — treating result as UNKNOWN (issue #8708)"
            );
            None
        }
        Err(_) => None,
    }
}

/// `gh issue view <N> --json state --jq .state`. Returns `"UNKNOWN"` on any
/// failure (matches `clean.py`'s `except Exception: issue_state = "UNKNOWN"`).
#[must_use]
pub fn issue_state(repo_root: &Path, issue: u32) -> String {
    let mut cmd = gh_command(repo_root);
    cmd.args([
        "issue",
        "view",
        &issue.to_string(),
        "--json",
        "state",
        "--jq",
        ".state",
    ]);
    let out = bounded_output(cmd, GH_PROBE_TIMEOUT);
    match out {
        Some(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "UNKNOWN".to_string()
            } else {
                s
            }
        }
        _ => "UNKNOWN".to_string(),
    }
}

/// `gh api repos/{owner}/{repo}/issues/<N> --jq .state`, normalized to
/// `"OPEN"` / `"CLOSED"` / `"UNKNOWN"`.
///
/// Deliberately the REST endpoint rather than [`issue_state`]'s `gh issue
/// view` (which goes through GraphQL): GraphQL quota exhaustion under
/// concurrent agents is a live failure mode in this repo, and the callers of
/// this probe are bulk hygiene passes that can issue one call per stale file
/// (#4450). REST returns lowercase states, so they are upper-cased here to
/// match [`issue_state`]'s contract.
#[must_use]
pub fn issue_state_rest(repo_root: &Path, issue: u32) -> String {
    let mut cmd = gh_command(repo_root);
    cmd.args([
        "api",
        &format!("repos/{{owner}}/{{repo}}/issues/{issue}"),
        "--jq",
        ".state",
    ]);
    let out = bounded_output(cmd, GH_PROBE_TIMEOUT);
    match out {
        Some(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_uppercase();
            match s.as_str() {
                "OPEN" | "CLOSED" => s,
                _ => "UNKNOWN".to_string(),
            }
        }
        _ => "UNKNOWN".to_string(),
    }
}

/// `gh api repos/{owner}/{repo}/issues/<N> --jq .closed_at`: the issue's own
/// close timestamp (issue #6653), REST rather than GraphQL for the same
/// quota-isolation reason as [`issue_state_rest`].
///
/// Used to gate the grace period for a closed issue whose worktree never had
/// a PR opened at all (`clean::PrStatus::NoPr`) — there is no PR
/// `closedAt`/`mergedAt` to read in that case, so the issue's own close time
/// is the only timestamp available. `None` on any failure, an empty/`null`
/// response, or an issue that is not (yet) closed — a probe failure must
/// never be read as "grace period already elapsed".
#[must_use]
pub fn issue_closed_at_rest(repo_root: &Path, issue: u32) -> Option<String> {
    let mut cmd = gh_command(repo_root);
    cmd.args([
        "api",
        &format!("repos/{{owner}}/{{repo}}/issues/{issue}"),
        "--jq",
        ".closed_at",
    ]);
    let out = bounded_output(cmd, GH_PROBE_TIMEOUT)?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "null" {
        None
    } else {
        Some(s)
    }
}

#[derive(Debug, Deserialize)]
struct PrRow {
    #[allow(dead_code)]
    number: u32,
}

/// Whether `branch` has an open PR. Returns `(has_open_pr, lookup_succeeded)`
/// — mirrors `clean.py::_check_open_pr`'s fail-closed contract: a failed
/// lookup must not be silently treated as "no open PR".
#[must_use]
pub fn has_open_pr(repo_root: &Path, branch: &str) -> (bool, bool) {
    let out = gh_command(repo_root)
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", "number", "--limit", "1",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let rows: Result<Vec<PrRow>, _> = serde_json::from_slice(&o.stdout);
            match rows {
                Ok(v) => (!v.is_empty(), true),
                Err(_) => (false, false),
            }
        }
        _ => (false, false),
    }
}

/// Three-state result of the open-linked-PR probe (Issue #4452), replacing the
/// old `Option<u32>` that conflated a *verified* "no open linked PR" with a
/// *probe failure* (missing/failed/timed-out `gh`, unresolvable repo, non-zero
/// exit, unparseable output). Distinguishing the two matters because the probe's
/// consumers have **opposite** failure stakes:
///
/// - The #4123 open-PR **dispatch guard** must fail *open* — a forge outage must
///   never wedge dispatch — so it treats both [`OpenPrProbe::NoneOpen`] and
///   [`OpenPrProbe::ProbeFailed`] as "proceed" (only a verified `Open` blocks).
///   That contract is unchanged, but the *registry's* probe narrows how often
///   `ProbeFailed` is reached at all: #5911 (REST fallback), #6058 (bounded
///   whole-probe retry), and #6788 (re-verify the last known linked PR over one
///   targeted `pulls/<n>` call) each recover a verdict the pre-fix probe would
///   have conceded. None of them removes the fail-open arm — see
///   `SweepRegistry::probe_open_linked_pr`.
/// - The #4366 **no-progress predicate** must also fail open, but in the
///   *opposite* direction: a probe failure must NOT let a benign self-skip count
///   as a failed attempt, so it counts ONLY a verified [`OpenPrProbe::NoneOpen`]
///   toward `no_progress`, treating [`OpenPrProbe::ProbeFailed`] as "unverified,
///   don't punish".
/// - Orphan recovery (#5511) must fail toward "assume alive": only a verified
///   [`OpenPrProbe::NoneOpen`] lets a `loom:building` reset proceed;
///   [`OpenPrProbe::ProbeFailed`] blocks the reset exactly like a verified
///   `Open` does.
///
/// The old `Option<u32>` collapsed `ProbeFailed` into `None`, so a PARTIAL forge
/// outage (PR probe fails while the issue probe answers OPEN) could still accrue
/// wrongful quarantine pressure via the no-progress predicate. The enum makes it
/// impossible to silently re-conflate the two at a call site.
///
/// Lives here (rather than in `sweep_registry::guards`, where it started) so the
/// `worktree_ops` family can share ONE closes-graph implementation with the
/// registry instead of maintaining a second copy of the query — see #5511.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrProbe {
    /// Verified: at least one *open* linked PR exists (carries its number).
    Open(u32),
    /// Verified: the forge answered and there is no open linked PR.
    NoneOpen,
    /// The probe could not produce a verdict — `gh` missing/failed/timed out,
    /// repo unresolvable, non-zero exit, or unparseable output.
    ProbeFailed,
}

/// The closes-graph GraphQL document behind [`open_linked_pr_args`]. Shared by
/// `sweep_registry::guards::SweepRegistry::probe_open_linked_pr` and
/// [`probe_open_linked_pr`] so the two transports (timeout-bounded registry
/// `Command` vs. plain `worktree_ops` `Command`) cannot drift apart.
pub const OPEN_LINKED_PR_QUERY: &str = "query($owner:String!,$repo:String!,$num:Int!){\
     repository(owner:$owner,name:$repo){\
     issue(number:$num){\
     closedByPullRequestsReferences(first:20,includeClosedPrs:false){\
     nodes{ number state } } } } }";

/// The REST `--jq` filter behind [`open_linked_pr_timeline_args`], as a format
/// template over `{owner}/{repo}`.
///
/// Walks `issues/{n}/timeline` for `cross-referenced` events whose source is an
/// OPEN pull request in this same repo. GitHub emits a `cross-referenced` event
/// for **any** PR body reference to the issue, so this is a strict superset of
/// the closes-graph for the yes/no question the #4123 guard actually asks: it
/// sees `Part of #N` and `Refs #N` phase PRs, which
/// `closedByPullRequestsReferences` structurally cannot (#7757/#7859, and the
/// `worktree_ops` half of that in #8116).
const OPEN_LINKED_PR_TIMELINE_JQ: &str = "[.[] | select(.event == \"cross-referenced\" \
     and .source.issue.pull_request != null \
     and .source.issue.state == \"open\" \
     and .source.issue.repository.full_name == \"{full_name}\") \
     | .source.issue.number] | unique | .[0] // empty";

/// `gh` arguments for the REST timeline (non-closing-reference) probe on
/// `issue` — the union transport shared by
/// `sweep_registry::guards::SweepRegistry::probe_open_linked_pr_rest` and
/// [`probe_open_linked_pr`], so the two cannot drift apart the way they had
/// before #8116 (the registry gained the timeline union in #7859; this module's
/// copy still asked only the closes-graph, so orphan recovery reset claims out
/// from under live `Part of #N` phase PRs).
#[must_use]
pub fn open_linked_pr_timeline_args(owner: &str, repo: &str, issue: u32) -> Vec<String> {
    vec![
        "api".to_string(),
        format!("repos/{owner}/{repo}/issues/{issue}/timeline"),
        "--paginate".to_string(),
        "--jq".to_string(),
        OPEN_LINKED_PR_TIMELINE_JQ.replace("{full_name}", &format!("{owner}/{repo}")),
    ]
}

/// Classify the raw stdout of the [`open_linked_pr_timeline_args`] query.
///
/// Empty output is a verified [`OpenPrProbe::NoneOpen`] (the filter emitted
/// nothing, i.e. no open cross-referencing PR); a leading line that parses as a
/// PR number is [`OpenPrProbe::Open`]; anything else is
/// [`OpenPrProbe::ProbeFailed`] — an answer we cannot read is never a verified
/// absence, same contract as [`parse_open_linked_pr`].
#[must_use]
pub fn parse_open_linked_pr_timeline(stdout: &str) -> OpenPrProbe {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return OpenPrProbe::NoneOpen;
    }
    match trimmed.lines().next().unwrap_or("").trim().parse::<u32>() {
        Ok(pr) => OpenPrProbe::Open(pr),
        Err(_) => OpenPrProbe::ProbeFailed,
    }
}

/// `gh` arguments for the closes-graph open-linked-PR query on `issue`.
///
/// Deliberately emits the RAW GraphQL payload rather than pushing a `--jq`
/// filter onto the wire: the `state == "OPEN"` filter is load-bearing (see
/// [`parse_open_linked_pr`]) and doing it in Rust makes it unit-testable
/// without a live `gh`/`jq` (#5511).
#[must_use]
pub fn open_linked_pr_args(owner: &str, repo: &str, issue: u32) -> Vec<String> {
    vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={OPEN_LINKED_PR_QUERY}"),
        "-F".to_string(),
        format!("owner={owner}"),
        "-F".to_string(),
        format!("repo={repo}"),
        "-F".to_string(),
        format!("num={issue}"),
    ]
}

/// Classify the raw stdout of the [`open_linked_pr_args`] query.
///
/// Filtering is on the node `state == "OPEN"`, NOT on the GraphQL
/// `includeClosedPrs:false` flag alone: live testing showed a *merged* PR still
/// returns from the closes-graph even with `includeClosedPrs:false` (it comes
/// back with `state: MERGED`), so relying on the flag would false-positive
/// forever on every issue whose PR ever merged. The `state == "OPEN"` filter is
/// the load-bearing one; the flag is kept only to trim the payload. This uses
/// the forge's closes-link graph, not `Closes #N` body-parsing. GitHub-only.
///
/// Anything that is not a well-formed, complete answer — unparseable JSON, a
/// top-level GraphQL `errors` array, a missing/`null` node list, or an OPEN
/// node whose `number` will not parse — is an [`OpenPrProbe::ProbeFailed`],
/// never a verified [`OpenPrProbe::NoneOpen`].
#[must_use]
pub fn parse_open_linked_pr(stdout: &str) -> OpenPrProbe {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return OpenPrProbe::ProbeFailed;
    };
    // A GraphQL response can carry `errors` alongside a partial `data` — a
    // partial answer is not a verdict.
    if v.get("errors").is_some_and(|e| !e.is_null()) {
        return OpenPrProbe::ProbeFailed;
    }
    let Some(serde_json::Value::Array(nodes)) =
        v.pointer("/data/repository/issue/closedByPullRequestsReferences/nodes")
    else {
        return OpenPrProbe::ProbeFailed;
    };
    for node in nodes {
        if node.get("state").and_then(serde_json::Value::as_str) != Some("OPEN") {
            continue;
        }
        return match node
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        {
            Some(pr) => OpenPrProbe::Open(pr),
            // An OPEN node we cannot name is a malformed payload, not an
            // absence — fail toward "unverified".
            None => OpenPrProbe::ProbeFailed,
        };
    }
    OpenPrProbe::NoneOpen
}

/// Resolve `(owner, repo)` for `repo_root` via `gh repo view`.
///
/// Unlike `sweep_registry::guards::SweepRegistry::resolve_owner_repo` this does
/// NOT honor the process-global `LOOM_REPO` override: the `worktree_ops`
/// callers are always scoped to a concrete `repo_root`, and a `LOOM_REPO`
/// pointing at a *different* repo would silently answer the open-PR probe from
/// the wrong closes-graph — which, for orphan recovery, is a false
/// `NoneOpen` that greenlights resetting a live claim (#5511). `None` on any
/// failure, which callers must treat as a probe failure.
#[must_use]
pub fn resolve_owner_repo(repo_root: &Path) -> Option<(String, String)> {
    let out = gh_command(repo_root)
        .args([
            "repo",
            "view",
            "--json",
            "owner,name",
            "--jq",
            r#".owner.login + "/" + .name"#,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.trim().split_once('/').and_then(|(o, r)| {
        if o.is_empty() || r.is_empty() {
            None
        } else {
            Some((o.to_string(), r.to_string()))
        }
    })
}

/// Best-effort probe for an **open** pull request linked to `issue`.
///
/// The `worktree_ops` counterpart of
/// `sweep_registry::guards::SweepRegistry::probe_open_linked_pr`, resolved
/// against `repo_root` instead of a registry workspace, and — since #8116 —
/// sharing that probe's two-transport union rather than only its first leg:
///
/// 1. **GraphQL closes-graph** ([`open_linked_pr_args`] /
///    [`parse_open_linked_pr`]). Decisive when it finds an OPEN PR.
/// 2. **REST timeline** ([`open_linked_pr_timeline_args`] /
///    [`parse_open_linked_pr_timeline`]), consulted on *both* `NoneOpen` and
///    `ProbeFailed`. The closes-graph only knows about **closing keywords**, so
///    a phase PR saying `Part of #N` / `Refs #N` is invisible to leg 1 and the
///    probe wrongly answered "no open linked PR" — which, for
///    [`super::orphan_recovery`], is a verified `NoneOpen` that greenlights
///    resetting a live claim. That is precisely the #8116 report: phase-scoped
///    PRs use `Part of #N`, so the guard did not protect the issue even after
///    its PR opened. REST also bills a separate quota, so leg 2 doubles as
///    #5911's rate-limit fallback.
///
/// Fail direction is unchanged: only a verified answer from either leg is a
/// verdict, and a union that cannot answer is [`OpenPrProbe::ProbeFailed`].
/// Added for #5511, where orphan recovery reset a `loom:building` issue that
/// had a live `Closes #N` PR open because nothing on that path ever asked the
/// forge about linked PRs.
#[must_use]
pub fn probe_open_linked_pr(repo_root: &Path, issue: u32) -> OpenPrProbe {
    // Repo resolution failure is a PROBE FAILURE, not a verified absence.
    let Some((owner, repo)) = resolve_owner_repo(repo_root) else {
        return OpenPrProbe::ProbeFailed;
    };
    let graphql = run_probe(repo_root, open_linked_pr_args(&owner, &repo, issue), &|s| {
        parse_open_linked_pr(s)
    });
    if matches!(graphql, OpenPrProbe::Open(_)) {
        return graphql;
    }
    let timeline = run_probe(repo_root, open_linked_pr_timeline_args(&owner, &repo, issue), &|s| {
        parse_open_linked_pr_timeline(s)
    });
    // A verified NoneOpen from leg 1 survives a leg-2 probe failure: leg 2 is a
    // superset *when it answers*, and an unanswered superset is no evidence.
    if matches!(timeline, OpenPrProbe::ProbeFailed) {
        return graphql;
    }
    timeline
}

/// Run one `gh` transport for [`probe_open_linked_pr`] and classify it.
///
/// A spawn error or non-zero exit (rate limit, auth failure, transient forge
/// error) is a PROBE FAILURE, never a verified "no open PR".
fn run_probe(
    repo_root: &Path,
    args: Vec<String>,
    classify: &dyn Fn(&str) -> OpenPrProbe,
) -> OpenPrProbe {
    match gh_command(repo_root).args(args).output() {
        Ok(o) if o.status.success() => classify(&String::from_utf8_lossy(&o.stdout)),
        _ => OpenPrProbe::ProbeFailed,
    }
}

/// `gh issue edit <N> --remove-label <remove> --add-label <add>`.
pub fn edit_labels(repo_root: &Path, issue: u32, remove: &str, add: &str) -> Result<()> {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "edit",
            &issue.to_string(),
            "--remove-label",
            remove,
            "--add-label",
            add,
        ])
        .output()
        .context("failed to invoke gh issue edit")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue edit {issue} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `gh issue comment <N> --body <body>`.
pub fn comment(repo_root: &Path, issue: u32, body: &str) -> Result<()> {
    let out = gh_command(repo_root)
        .args(["issue", "comment", &issue.to_string(), "--body", body])
        .output()
        .context("failed to invoke gh issue comment")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue comment {issue} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct BuildingIssueRow {
    pub number: u32,
    #[serde(default)]
    pub title: String,
}

/// `gh issue list --label loom:building --state open --json number,title`.
pub fn list_building_issues(repo_root: &Path) -> Result<Vec<BuildingIssueRow>> {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "list",
            "--label",
            "loom:building",
            "--state",
            "open",
            "--json",
            "number,title",
        ])
        .output()
        .context("failed to invoke gh issue list")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue list --label loom:building failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")
}

/// Seconds since the most recent `labeled` timeline event for `loom:building`
/// on `issue`, or `None` if it cannot be determined (API failure, no such
/// event, unparseable timestamp). Mirrors `orphan_recovery.py::_get_building_label_age`.
#[must_use]
pub fn building_label_age_seconds(repo_root: &Path, issue: u32) -> Option<i64> {
    let out = gh_command(repo_root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}/events"),
            "--jq",
            r#"[.[] | select(.event == "labeled" and .label.name == "loom:building")] | last | .created_at"#,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ts = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_matches('"')
        .to_string();
    if ts.is_empty() || ts == "null" {
        return None;
    }
    let dt = chrono::DateTime::parse_from_rfc3339(&ts).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .num_seconds(),
    )
}

/// Whether a `## Orphan Recovery` comment was posted on `issue` within the
/// last `dedup_seconds` (dedup guard, mirrors
/// `orphan_recovery.py::_has_recent_orphan_comment`).
#[must_use]
pub fn has_recent_orphan_comment(repo_root: &Path, issue: u32, dedup_seconds: i64) -> bool {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "comments",
            "--jq",
            r###".comments | map(select(.body | startswith("## Orphan Recovery"))) | sort_by(.createdAt) | last | .createdAt // empty"###,
        ])
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    let ts = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ts.is_empty() {
        return false;
    }
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) else {
        return false;
    };
    let age = chrono::Utc::now()
        .signed_duration_since(dt.with_timezone(&chrono::Utc))
        .num_seconds();
    age < dedup_seconds
}

/// Best-effort fetch of the freshest `updated_at` among `issue`'s lease-
/// record comments (`<!-- loom:lease host=... sweep=... -->`, Issue #6179,
/// consulted here per Epic #6165 Phase 2 / Issue #6286) — the fleet-scoped
/// liveness evidence `orphan_recovery::check_untracked_building` consults as
/// the final gate before flagging a `loom:building` claim orphaned.
///
/// Delegates to [`crate::claim_reconciliation::forge::fetch_freshest_lease_updated_at`]
/// (Issue #7596) rather than re-issuing the `gh api ... --jq ...` call a
/// second time: that is the already-hardened sibling implementation Issue
/// #7591 / PR #7597 fixed for the periodic-reconciliation path, and this
/// `recover-orphans` CLI path had the identical conflation bug (`None`
/// meaning either "no lease comment" or "the read itself failed"
/// indistinguishably) until this fix. Returns
/// [`crate::claim_reconciliation::forge::LeaseProbe`] so the caller
/// ([`crate::worktree_ops::orphan_recovery::lease_blocks_reset`]) can refuse
/// the reset on a read failure exactly like it already does on a found,
/// fresh lease — never treating an unverifiable read as evidence the lease
/// is absent.
#[must_use]
pub(crate) fn freshest_lease_updated_at(
    repo_root: &Path,
    issue: u32,
) -> crate::claim_reconciliation::forge::LeaseProbe {
    // Route through the same `Command`-building seam (`GH_CONFIG_DIR`
    // credential preflight, `LOOM_GH_BIN` override) every other helper in
    // this module uses, rather than hard-coding `"gh"`.
    let gh_bin = std::path::PathBuf::from(gh_bin());
    crate::claim_reconciliation::forge::fetch_freshest_lease_updated_at(&gh_bin, repo_root, issue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes_graph(nodes: &str) -> String {
        format!(
            r#"{{"data":{{"repository":{{"issue":{{"closedByPullRequestsReferences":{{"nodes":[{nodes}]}}}}}}}}}}"#
        )
    }

    #[test]
    fn open_node_is_a_verified_open_pr() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"OPEN"}"#)),
            OpenPrProbe::Open(5507)
        );
    }

    #[test]
    fn empty_node_list_is_a_verified_absence() {
        assert_eq!(parse_open_linked_pr(&closes_graph("")), OpenPrProbe::NoneOpen);
    }

    /// The load-bearing filter: `includeClosedPrs:false` does NOT keep merged
    /// PRs out of the closes-graph, so a `MERGED` node must never read as an
    /// open PR (it would pin an already-finished issue forever).
    #[test]
    fn merged_node_does_not_count_as_open() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"MERGED"}"#)),
            OpenPrProbe::NoneOpen
        );
    }

    #[test]
    fn closed_node_does_not_count_as_open() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"CLOSED"}"#)),
            OpenPrProbe::NoneOpen
        );
    }

    #[test]
    fn open_node_wins_over_merged_siblings() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(
                r#"{"number":1,"state":"MERGED"},{"number":2,"state":"OPEN"}"#
            )),
            OpenPrProbe::Open(2)
        );
    }

    #[test]
    fn unparseable_payload_is_a_probe_failure() {
        assert_eq!(parse_open_linked_pr(""), OpenPrProbe::ProbeFailed);
        assert_eq!(parse_open_linked_pr("not json"), OpenPrProbe::ProbeFailed);
        // Truncated: right shape, missing the node list.
        assert_eq!(
            parse_open_linked_pr(r#"{"data":{"repository":{"issue":null}}}"#),
            OpenPrProbe::ProbeFailed
        );
    }

    #[test]
    fn graphql_errors_are_a_probe_failure() {
        let body = r#"{"errors":[{"message":"rate limited"}],"data":{"repository":null}}"#;
        assert_eq!(parse_open_linked_pr(body), OpenPrProbe::ProbeFailed);
    }

    #[test]
    fn open_node_with_unusable_number_is_a_probe_failure() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":null,"state":"OPEN"}"#)),
            OpenPrProbe::ProbeFailed
        );
    }

    #[test]
    fn query_args_carry_the_closes_graph_query_and_variables() {
        let args = open_linked_pr_args("rjwalters", "loom", 5501);
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "graphql");
        assert!(args
            .iter()
            .any(|a| a.contains("closedByPullRequestsReferences")));
        assert!(args.iter().any(|a| a == "owner=rjwalters"));
        assert!(args.iter().any(|a| a == "repo=loom"));
        assert!(args.iter().any(|a| a == "num=5501"));
    }

    // -- REST timeline union (#8116) ----------------------------------------

    #[test]
    fn timeline_args_target_the_paginated_timeline_and_scope_the_filter_to_this_repo() {
        let args = open_linked_pr_timeline_args("rjwalters", "loom", 8116);
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "repos/rjwalters/loom/issues/8116/timeline");
        assert!(args.iter().any(|a| a == "--paginate"));
        let filter = args.last().unwrap();
        assert!(filter.contains("cross-referenced"));
        assert!(filter.contains(r#".source.issue.state == "open""#));
        assert!(
            filter.contains(r#"full_name == "rjwalters/loom""#),
            "the filter must be scoped to this repo, got: {filter}"
        );
        assert!(
            !filter.contains("{full_name}"),
            "the template placeholder must be substituted, got: {filter}"
        );
    }

    /// AC (#8116): an issue whose only open PR references it with a NON-closing
    /// keyword (`Part of #N`) must read as `Open`. The closes-graph cannot see
    /// such a PR at all; the timeline's `cross-referenced` event can, which is
    /// the entire reason this transport exists.
    #[test]
    fn a_non_closing_part_of_reference_reads_as_an_open_linked_pr() {
        assert_eq!(parse_open_linked_pr_timeline("8140\n"), OpenPrProbe::Open(8140));
    }

    #[test]
    fn empty_timeline_output_is_a_verified_none_open() {
        assert_eq!(parse_open_linked_pr_timeline(""), OpenPrProbe::NoneOpen);
        assert_eq!(parse_open_linked_pr_timeline("  \n"), OpenPrProbe::NoneOpen);
    }

    #[test]
    fn unparseable_timeline_output_is_a_probe_failure_not_an_absence() {
        assert_eq!(
            parse_open_linked_pr_timeline("gh: rate limit exceeded"),
            OpenPrProbe::ProbeFailed
        );
    }

    // --- probe bounding (#8708) ------------------------------------------
    //
    // The 25-hour `clean --deep --safe` hang this issue reports was an
    // unbounded `gh api` child wedged in `futex_do_wait`. Every bounded
    // probe funnels through `bounded_output`; these tests pin the seam's
    // contract with fixture executables addressed by ABSOLUTE path — no
    // PATH mutation (the #5961 rule) and no `LOOM_GH_BIN` env racing other
    // tests — and a sub-second deadline so CI never pays the 60s budget.

    /// Write an executable `sh` fixture under `dir` and return its path.
    fn write_probe_fixture(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn a_hung_gh_probe_is_killed_at_its_deadline_and_reports_no_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let hung = write_probe_fixture(tmp.path(), "hung-probe", "#!/bin/sh\nsleep 30\n");
        let started = std::time::Instant::now();
        let out = bounded_output(Command::new(&hung), Duration::from_millis(750));
        assert!(
            out.is_none(),
            "a probe past its deadline must report no answer, never hang: {out:?}"
        );
        // Bounded wall-clock: killed ~750ms in; 10s is an order-of-magnitude
        // margin for CI scheduling, still nothing like the 30s the fixture
        // would need to exit on its own.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hung probe must be killed promptly, not waited out: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_fast_gh_probe_completes_with_its_output_captured() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = write_probe_fixture(tmp.path(), "ok-probe", "#!/bin/sh\nprintf 'OPEN'\n");
        // Retry the spawn on a no-answer: a freshly-written script can hit
        // ETXTBSY (or a transient fork failure) under the parallel test
        // harness — a harness race, not a property of the code under test
        // (same mitigation `clean::tests::spawn_service_in` applies).
        let out = (0..10)
            .find_map(|_| bounded_output(Command::new(&ok), GH_PROBE_TIMEOUT))
            .expect("a probe that exits inside the deadline must return its Output");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "OPEN");
    }

    #[test]
    fn a_failing_gh_probe_is_a_completed_answer_not_a_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let fail = write_probe_fixture(tmp.path(), "fail-probe", "#!/bin/sh\nexit 1\n");
        // Same spawn-retry rationale as the fast-probe test above.
        let out = (0..10)
            .find_map(|_| bounded_output(Command::new(&fail), GH_PROBE_TIMEOUT))
            .expect("a probe that ran and exited nonzero completed, it did not time out");
        assert!(
            !out.status.success(),
            "the nonzero exit must survive the bounding so each caller's existing \
             `!out.status.success()` -> fail-closed path keeps deciding it"
        );
    }

    #[test]
    fn a_missing_gh_binary_is_no_answer_exactly_as_before_the_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bounded_output(Command::new(tmp.path().join("no-such-gh")), GH_PROBE_TIMEOUT);
        assert!(out.is_none(), "a spawn failure must stay a no-answer: {out:?}");
    }
}
