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
//! `state == "OPEN"` closes-graph filter, and the REST cross-reference union
//! plus its #6216/#8940 bare-mention phrase filter), unit-tested at the bottom
//! of this file along with both transports' argv.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::gh_invocation::{gh_bin, AccessIntent, GhInvocation, GhTarget, Operation, ReadClass};

/// Deadline for the unbounded-by-history `gh` calls this module made through a
/// bare `Command::output()` (#10089: the facade always bounds its child).
/// Generous on purpose: paginated timeline walks and writes must not be cut
/// short by a slow forge, only by a wedged one.
const GH_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Build one counted facade invocation rooted at `repo_root`.
///
/// This is the single choke point every helper in this module builds through
/// (#5401/#5431 cross-owner `GH_CONFIG_DIR` comes from the cwd lookup inside
/// the facade), and the call is booked in `forge_call_stats` under `op`
/// (#10089).
fn invocation(
    op: &'static str,
    intent: AccessIntent,
    repo_root: &Path,
    timeout: Duration,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> GhInvocation {
    GhInvocation::new(Operation::new(op), intent, GhTarget::None, timeout)
        .args(args)
        .current_dir(repo_root)
}

/// Run a counted read with the long deadline.
fn run_read(
    op: &'static str,
    repo_root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> CmdOutcome {
    invocation(op, AccessIntent::Read, repo_root, GH_CALL_TIMEOUT, args).run()
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

/// Run one read-only `gh` probe to completion under its invocation's deadline
/// (#8708), through the counted facade (#10089).
///
/// Returns `None` on deadline expiry (logged — the operational signal this
/// bound exists for), on spawn failure, and on output-collection failure:
/// exactly the "no answer" each caller's previous `cmd.output().ok()` handling
/// mapped. A probe that **exits** — zero or nonzero — is a completed answer
/// and keeps its [`Output`], so each caller's existing
/// `!out.status.success()` fail-closed path keeps deciding those.
#[must_use]
pub(crate) fn bounded_via(inv: GhInvocation) -> Option<Output> {
    match inv.run() {
        CmdOutcome::Ran(out) => Some(out),
        CmdOutcome::Unavailable(Unavailable::TimedOut { after, .. }) => {
            eprintln!(
                "gh: probe exceeded {after:?} deadline — treating result as UNKNOWN (issue #8708)"
            );
            None
        }
        CmdOutcome::Unavailable(_) => None,
    }
}

/// [`bounded_counted`] for a deferrable hygiene probe (W4-C,
/// [`ReadClass::Hygiene`]): when every reader for the repo's owner is
/// withdrawn the probe is shed instead of spending the writer's bucket, and
/// the shed is "no answer" (`None`) exactly like a timeout — so each caller
/// keeps its fail-closed `UNKNOWN` / `PrStatus::Unknown` path.
pub(crate) fn bounded_hygiene(
    op: &'static str,
    repo_root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Option<Output> {
    bounded_via(
        invocation(op, AccessIntent::Read, repo_root, GH_PROBE_TIMEOUT, args)
            .read_class(ReadClass::Hygiene),
    )
}

/// A bounded counted read probe: the `None`-on-no-answer contract of
/// [`bounded_via`] for a `gh <args>` call run from `repo_root`, booked under
/// `op`.
pub(crate) fn bounded_counted(
    op: &'static str,
    repo_root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Option<Output> {
    bounded_via(invocation(op, AccessIntent::Read, repo_root, GH_PROBE_TIMEOUT, args))
}

/// `gh issue view <N> --json state --jq .state`. Returns `"UNKNOWN"` on any
/// failure (matches `clean.py`'s `except Exception: issue_state = "UNKNOWN"`).
#[must_use]
pub fn issue_state(repo_root: &Path, issue: u32) -> String {
    let out = bounded_hygiene(
        "worktree.issue_state",
        repo_root,
        [
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "state",
            "--jq",
            ".state",
        ],
    );
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

/// `GET repos/{owner}/{repo}/issues/<N>` as JSON, through the shared ETag
/// store (#10512): the `ETag` and body persist on disk under `issue-`, so a
/// re-probe of an unchanged issue — the reaper's every-15-minute case, and
/// the `closed_at` read right after `state` — is a `304`, free on the core
/// bucket. Booked under `op`. An explicit `LOOM_REPO` names the repo, else
/// the checkout's `origin`, else gh's placeholder (the store's
/// `resolve_target` rule). `None` on any failure or a `404`: each caller's
/// fail-closed answer.
///
/// Routed as a `Gate` read by the store, so under reader-budget exhaustion it
/// is no longer shed (W4-C `Hygiene`); it falls through to the writer, where
/// it is mostly a `304`.
fn cached_issue_json(op: &'static str, repo_root: &Path, issue: u32) -> Option<serde_json::Value> {
    use crate::forge_etag_store as store;
    let loom_repo = std::env::var("LOOM_REPO")
        .ok()
        .filter(|r| !r.trim().is_empty());
    let target = store::resolve_target(Some(repo_root), loom_repo.as_deref());
    let url = match &target.repo {
        Some(r) => format!("repos/{r}/issues/{issue}"),
        None => format!("repos/{{owner}}/{{repo}}/issues/{issue}"),
    };
    let read = store::cached_read(
        store::ConditionalRead::new(op, crate::forge_call_stats::ops::ISSUE_VIEW_STATE),
        &std::path::PathBuf::from(gh_bin()),
        Some(repo_root),
        loom_repo.as_deref(),
        &url,
        "issue-",
    )
    .ok()?;
    serde_json::from_str(&read.body?).ok()
}

/// The issue's REST `.state`, normalized to `"OPEN"` / `"CLOSED"` /
/// `"UNKNOWN"` (any failure, `404`, or other value).
///
/// Deliberately the REST endpoint rather than [`issue_state`]'s `gh issue
/// view` (which goes through GraphQL): GraphQL quota exhaustion under
/// concurrent agents is a live failure mode in this repo, and the callers of
/// this probe are bulk hygiene passes that can issue one call per stale file
/// (#4450). REST returns lowercase states, so they are upper-cased here to
/// match [`issue_state`]'s contract. Conditional since #10512
/// ([`cached_issue_json`]).
#[must_use]
pub fn issue_state_rest(repo_root: &Path, issue: u32) -> String {
    let state = cached_issue_json("worktree.issue_state_rest", repo_root, issue)
        .and_then(|v| v.get("state")?.as_str().map(str::to_uppercase));
    match state.as_deref() {
        Some(s @ ("OPEN" | "CLOSED")) => s.to_string(),
        _ => "UNKNOWN".to_string(),
    }
}

/// The issue's REST `.closed_at`: its own close timestamp (issue #6653), REST
/// rather than GraphQL for the same quota-isolation reason as
/// [`issue_state_rest`], and the same conditional read ([`cached_issue_json`]).
///
/// Used to gate the grace period for a closed issue whose worktree never had
/// a PR opened at all (`clean::PrStatus::NoPr`) — there is no PR
/// `closedAt`/`mergedAt` to read in that case, so the issue's own close time
/// is the only timestamp available. `None` on any failure, an empty/`null`
/// value, or an issue that is not (yet) closed — a probe failure must
/// never be read as "grace period already elapsed".
#[must_use]
pub fn issue_closed_at_rest(repo_root: &Path, issue: u32) -> Option<String> {
    let json = cached_issue_json("worktree.issue_closed_at", repo_root, issue)?;
    let s = json.get("closed_at")?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
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
    let out = invocation(
        "worktree.has_open_pr",
        AccessIntent::Read,
        repo_root,
        GH_CALL_TIMEOUT,
        [
            "pr", "list", "--head", branch, "--state", "open", "--json", "number", "--limit", "1",
        ],
    )
    .read_class(ReadClass::Hygiene)
    .run();
    match out {
        CmdOutcome::Ran(o) if o.status.success() => {
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
     nodes{ number state isCrossRepository authorAssociation author{ login __typename } } } } } }";

/// The REST `--jq` filter behind [`open_linked_pr_timeline_args`], as a format
/// template over `{owner}/{repo}`.
///
/// Walks `issues/{n}/timeline` for `cross-referenced` events whose source is an
/// OPEN pull request in this same repo. GitHub emits a `cross-referenced` event
/// for **any** reference to the issue — body *or* comment, linking phrase or
/// not — so this leg sees `Part of #N` phase PRs that
/// `closedByPullRequestsReferences` structurally cannot (#7757/#7859, and the
/// `worktree_ops` half of that in #8116), and it also sees PRs that merely
/// *mention* `#N` in passing.
///
/// A bare mention is **not** a linkage, so the filter emits one compact JSON
/// object per surviving candidate (`{number, body}`, one per line, deduped in
/// Rust) rather than a single PR number, and
/// [`parse_open_linked_pr_timeline`] applies the #6216 phrase filter to each
/// body. Keeping that regex in Rust (rather than pushing a jq `test()` onto the
/// wire) is the same call [`open_linked_pr_args`] makes about the closes-graph
/// `state == "OPEN"` filter: the load-bearing predicate stays unit-testable
/// without a live `gh`/`jq` (#5511).
///
/// `source.issue.body` is the referring PR's own body, present inline on every
/// `cross-referenced` event, so the filter costs no extra round trip — unlike
/// `/loom:sweep`'s shell recipe, which re-reads each candidate over
/// `gh pr view <n> --json body`.
const OPEN_LINKED_PR_TIMELINE_JQ: &str = ".[] | select(.event == \"cross-referenced\" \
     and .source.issue.pull_request != null \
     and .source.issue.state == \"open\" \
     and .source.issue.repository.full_name == \"{full_name}\") \
     | {number: .source.issue.number, body: (.source.issue.body // \"\"), \
     user: {login: .source.issue.user.login, type: .source.issue.user.type}, \
     author_association: .source.issue.author_association}";

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

/// The #6216 phrase filter: does `body` reference `issue` with a phrase that
/// makes the referring PR a *linked* PR, rather than merely mentioning it?
///
/// Two families count, and nothing else:
///
/// 1. **Closing keywords** — GitHub's own set (`close`/`closes`/`closed`,
///    `fix`/`fixes`/`fixed`, `resolve`/`resolves`/`resolved`). These are
///    normally caught by the closes-graph leg, but leg 2 must accept them too:
///    when GraphQL cannot answer at all (#5911 quota exhaustion) the timeline is
///    the *only* leg left, and discarding a live `Closes #N` PR there would fall
///    the #4123 guard open — or, worse, hand
///    [`super::orphan_recovery`] a verified `NoneOpen` that resets a live claim
///    (#5511).
/// 2. **Partial-increment phrases** — `Part of #N` / `Contributes to #N`, the
///    convention phase PRs use (#7757/#7859) and the exact pair
///    `/loom:sweep`'s existing-PR probe confirms
///    (`sweep-wave-lifecycle.md` → "Existing-PR probe", #6216).
///
/// Matching is case-insensitive and tolerant of markdown emphasis/colon between
/// the phrase and `#N` (`**Part of:** #123`), mirroring `parse_dependencies`'
/// convention (#4508). A trailing non-digit boundary is required so `Part of
/// #1234` is not read as a reference to `#123`, and the phrase must start at a
/// non-alphanumeric boundary so `Prefixes #123` is not read as `fixes #123`.
/// That leading boundary is deliberately NOT `\b` — a markdown `_`/`*` sigil
/// immediately before the phrase (`_contributes to_ #123`) is a word character
/// to `\b` but an emphasis marker to a human.
///
/// Returns `None` only if the regex itself fails to compile, which callers must
/// treat as a probe failure rather than an absence.
pub(crate) fn linkage_phrase_regex(issue: u32) -> Option<regex::Regex> {
    regex::Regex::new(&format!(
        r"(?i)(?:^|[^0-9A-Za-z]){LINKAGE_PHRASE}[*_:\s]*#{issue}(?:[^0-9]|$)"
    ))
    .ok()
}

/// The two phrase families of [`linkage_phrase_regex`], one alternation per
/// family. Shared with [`linkage_refs`] so the guard and the ETA cache's
/// reconstruction of it (#10197) cannot drift apart.
const LINKAGE_PHRASE: &str =
    r"(?:(clos(?:e|es|ed)|fix(?:es|ed)?|resolv(?:e|es|ed))|(part\s+of|contributes\s+to))";

/// Which [`linkage_phrase_regex`] family linked a PR to an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkageKind {
    /// A GitHub closing keyword (`Closes #N`).
    Closes,
    /// A partial-increment phrase (`Part of #N` / `Contributes to #N`).
    PartOf,
}

impl LinkageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LinkageKind::Closes => "closes",
            LinkageKind::PartOf => "part_of",
        }
    }
}

/// Every issue `body` links with a [`linkage_phrase_regex`] phrase, ascending,
/// one entry per issue: exactly the set `n` for which
/// `linkage_phrase_regex(n)` matches `body`. An issue named by both families
/// is reported as [`LinkageKind::Closes`].
#[must_use]
pub fn linkage_refs(body: &str) -> Vec<(u32, LinkageKind)> {
    static RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
        // No trailing boundary group: `[0-9]+` is greedy, so the match already
        // ends at a non-digit or the end, and not consuming it leaves the next
        // phrase's leading boundary available.
        regex::Regex::new(&format!(r"(?i)(?:^|[^0-9A-Za-z]){LINKAGE_PHRASE}[*_:\s]*#([0-9]+)")).ok()
    });
    let Some(re) = RE.as_ref() else {
        return Vec::new();
    };
    let mut found: std::collections::BTreeMap<u32, LinkageKind> = std::collections::BTreeMap::new();
    for caps in re.captures_iter(body) {
        let digits = caps.get(3).map_or("", |m| m.as_str());
        // `#0123` never matches the per-issue regex for 123.
        if digits.starts_with('0') {
            continue;
        }
        let Ok(issue) = digits.parse::<u32>() else {
            continue;
        };
        let kind = if caps.get(1).is_some() {
            LinkageKind::Closes
        } else {
            LinkageKind::PartOf
        };
        found
            .entry(issue)
            .and_modify(|k| *k = (*k).min(kind))
            .or_insert(kind);
    }
    found.into_iter().collect()
}

/// Classify the raw stdout of the [`open_linked_pr_timeline_args`] query as a
/// verdict about `issue`.
///
/// Each non-blank line is one candidate `{number, body}` object (see
/// [`OPEN_LINKED_PR_TIMELINE_JQ`]); a candidate counts as an open linked PR only
/// when its body passes [`linkage_phrase_regex`]. **A bare mention is
/// discarded** — that is the whole point of #8940: PR #8314 mentioned `#8322`
/// once in a stand-down comment, which was enough for the pre-fix query to
/// refuse dispatch of #8322 for 6.5 days even though #8314 neither closed it nor
/// claimed a slice of it.
///
/// Empty output (or output whose every candidate is a bare mention) is a
/// verified [`OpenPrProbe::NoneOpen`]; the lowest surviving candidate number is
/// [`OpenPrProbe::Open`] (deterministic, matching the `unique | .[0]` ordering
/// the pre-#8940 jq produced); a line we cannot read is
/// [`OpenPrProbe::ProbeFailed`] — an answer we cannot parse is never a verified
/// absence, same contract as [`parse_open_linked_pr`].
#[must_use]
pub fn parse_open_linked_pr_timeline(stdout: &str, issue: u32) -> OpenPrProbe {
    let Some(phrase) = linkage_phrase_regex(issue) else {
        return OpenPrProbe::ProbeFailed;
    };
    let mut linked: Option<u32> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<serde_json::Value>(line) else {
            return OpenPrProbe::ProbeFailed;
        };
        let Some(number) = candidate
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        else {
            // A candidate we cannot name is a malformed payload, not an absence.
            return OpenPrProbe::ProbeFailed;
        };
        let body = candidate
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !phrase.is_match(body) {
            continue;
        }
        linked = Some(linked.map_or(number, |lowest: u32| lowest.min(number)));
    }
    match linked {
        Some(pr) => OpenPrProbe::Open(pr),
        None => OpenPrProbe::NoneOpen,
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

/// [`parse_open_linked_pr`] after #9548 (H14): an open PR from a FORK whose
/// author is not trusted is not a linked PR. Anyone can open a fork PR saying
/// `Closes #N`; counting it would let an outsider park any issue indefinitely.
/// A same-repo branch needs write access, so it always counts, and a node
/// without author fields errs toward counting (the non-destructive answer).
#[must_use]
pub fn parse_open_linked_pr_trusted(stdout: &str, root: &Path) -> OpenPrProbe {
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    parse_open_linked_pr(&policy.drop_untrusted_fork_prs(stdout))
}

/// [`parse_open_linked_pr_timeline`] after #9548 (H14): a candidate whose
/// author is known and untrusted is dropped. The timeline cannot say which
/// repo a PR's head lives in, so the author alone decides here; the fleet's
/// own PRs are App-authored (`x[bot]`) and pass.
#[must_use]
pub fn parse_open_linked_pr_timeline_trusted(stdout: &str, issue: u32, root: &Path) -> OpenPrProbe {
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    parse_open_linked_pr_timeline(&policy.drop_untrusted_timeline_prs(stdout), issue)
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
///
/// Answered from the repo-facts record (`gh repo view` semantics: `GH_REPO`
/// ignored, post-redirect) when facts are on; the `gh repo view` call below
/// when they are off or the root is pinned to legacy (W3a).
#[must_use]
pub fn resolve_owner_repo(repo_root: &Path) -> Option<(String, String)> {
    match crate::forge_repo_facts::canonical(repo_root, crate::forge_repo_facts::GhRepoEnv::Ignore)
    {
        crate::forge_repo_facts::Lookup::Fact(f) => Some((f.owner, f.name)),
        crate::forge_repo_facts::Lookup::Unavailable => None,
        crate::forge_repo_facts::Lookup::Legacy => resolve_owner_repo_legacy(repo_root),
    }
}

/// The pre-facts `gh repo view` resolve behind [`resolve_owner_repo`].
fn resolve_owner_repo_legacy(repo_root: &Path) -> Option<(String, String)> {
    let out = run_read(
        "worktree.resolve_repo",
        repo_root,
        [
            "repo",
            "view",
            "--json",
            "owner,name",
            "--jq",
            r#".owner.login + "/" + .name"#,
        ],
    );
    let CmdOutcome::Ran(out) = out else {
        return None;
    };
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
/// sharing that probe's transports rather than only its first leg:
///
/// 0. **Cached open-PR listing** ([`super::linked_pr_listing`], #10514). When
///    the ETag'd REST listing reads, its verdict is final and legs 1-2 never
///    run: the normal path spends no GraphQL and no timeline walk. Legs 1-2
///    below are its fallback for a listing that could not be read.
/// 1. **GraphQL closes-graph** ([`open_linked_pr_args`] /
///    [`parse_open_linked_pr`]). Decisive when it finds an OPEN PR.
/// 2. **REST timeline** ([`open_linked_pr_timeline_args`] /
///    [`parse_open_linked_pr_timeline`]), consulted on *both* `NoneOpen` and
///    `ProbeFailed`. The closes-graph only knows about **closing keywords**, so
///    a phase PR saying `Part of #N` is invisible to leg 1 and the
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
///
/// Leg 2 is a superset of leg 1, but **not** an unfiltered one: since #8940 it
/// discards candidates whose body merely mentions `#N` without a linking phrase
/// (see [`parse_open_linked_pr_timeline`]), matching `/loom:sweep`'s own
/// existing-PR probe. Before that filter, one passing `#N` in an unrelated PR's
/// comment thread refused dispatch of `#N` for as long as that PR stayed open.
#[must_use]
pub fn probe_open_linked_pr(repo_root: &Path, issue: u32) -> OpenPrProbe {
    use crate::forge_repo_facts::{self as facts, GhRepoEnv, Lookup};
    // Repo resolution failure is a PROBE FAILURE, not a verified absence.
    let (owner, repo, fact) = match facts::canonical(repo_root, GhRepoEnv::Ignore) {
        Lookup::Fact(f) => (f.owner.clone(), f.name.clone(), Some(f)),
        Lookup::Unavailable => return OpenPrProbe::ProbeFailed,
        Lookup::Legacy => match resolve_owner_repo_legacy(repo_root) {
            Some((o, r)) => (o, r, None),
            None => return OpenPrProbe::ProbeFailed,
        },
    };
    // Leg 0 (#10514): the cached open-PR listing, pinned to the repo just
    // resolved (never `LOOM_REPO`, #5511). A read listing is decisive.
    let nwo = format!("{owner}/{repo}");
    let gh = gh_bin();
    let listed = super::linked_pr_listing::probe(
        "worktree.linked_pr_listing",
        Path::new(&gh),
        repo_root,
        Some(&nwo),
        (&nwo, issue),
        None,
    );
    let gone = std::cell::Cell::new(false);
    let verdict = match listed {
        Some(verdict) => verdict,
        None => legacy_union(repo_root, &owner, &repo, issue, &gone),
    };
    let Some(fact) = fact else {
        return verdict;
    };
    // W3a: a repo the forge could not resolve under the remembered name is
    // suspect, and nothing computed from it is a verdict.
    if gone.get() {
        facts::invalidate(repo_root, "linked-PR probe could not resolve the repository");
        return OpenPrProbe::ProbeFailed;
    }
    // A NoneOpen greenlights claim resets (orphan recovery, check-claim): it
    // is a verdict only when the owner it was asked under is confirmed now.
    if verdict == OpenPrProbe::NoneOpen
        && !fact.fresh
        && !facts::confirmed_in_pass(&fact)
        && !facts::confirm_owner(repo_root, GhRepoEnv::Ignore, &fact.owner)
    {
        return OpenPrProbe::ProbeFailed;
    }
    verdict
}

/// The pre-#10514 GraphQL-then-timeline union of [`probe_open_linked_pr`], now
/// only its fallback when the open-PR listing could not be read.
fn legacy_union(
    repo_root: &Path,
    owner: &str,
    repo: &str,
    issue: u32,
    gone: &std::cell::Cell<bool>,
) -> OpenPrProbe {
    let graphql = run_probe(
        "worktree.linked_pr_graphql",
        repo_root,
        open_linked_pr_args(owner, repo, issue),
        &|s| parse_open_linked_pr_trusted(s, repo_root),
        gone,
    );
    if matches!(graphql, OpenPrProbe::Open(_)) {
        return graphql;
    }
    let timeline = run_probe(
        "worktree.linked_pr_timeline",
        repo_root,
        open_linked_pr_timeline_args(owner, repo, issue),
        &|s| parse_open_linked_pr_timeline_trusted(s, issue, repo_root),
        gone,
    );
    // A verified NoneOpen from leg 1 survives a leg-2 probe failure: leg 2
    // is a superset *when it answers*, and an unanswered superset is no
    // evidence.
    if matches!(timeline, OpenPrProbe::ProbeFailed) {
        graphql
    } else {
        timeline
    }
}

/// Run one `gh` transport for [`probe_open_linked_pr`] and classify it.
///
/// A spawn error or non-zero exit (rate limit, auth failure, transient forge
/// error) is a PROBE FAILURE, never a verified "no open PR". An answer saying
/// the repository itself does not resolve sets `gone`.
fn run_probe(
    op: &'static str,
    repo_root: &Path,
    args: Vec<String>,
    classify: &dyn Fn(&str) -> OpenPrProbe,
    gone: &std::cell::Cell<bool>,
) -> OpenPrProbe {
    match run_read(op, repo_root, args) {
        CmdOutcome::Ran(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains(REPO_UNRESOLVED) {
                gone.set(true);
            }
            classify(&stdout)
        }
        CmdOutcome::Ran(o) => {
            let said = |b: &[u8]| String::from_utf8_lossy(b).contains(REPO_UNRESOLVED);
            if said(&o.stdout) || said(&o.stderr) {
                gone.set(true);
            }
            OpenPrProbe::ProbeFailed
        }
        CmdOutcome::Unavailable(_) => OpenPrProbe::ProbeFailed,
    }
}

/// GitHub's GraphQL error for a repository that does not resolve.
const REPO_UNRESOLVED: &str = "Could not resolve to a Repository";

/// #9548: both writers below resolve the repo from `repo_root`'s remotes;
/// refuse unless this installation may write there.
fn require_write_scope(repo_root: &Path) -> Result<()> {
    match crate::write_scope::root_writable(repo_root) {
        crate::write_scope::Verdict::Allow(_) => Ok(()),
        crate::write_scope::Verdict::Deny(why) => Err(anyhow!("write refused (#9548): {why}")),
    }
}

/// The completed [`Output`], or an error naming why `gh` gave no answer.
fn ran_or_err(outcome: CmdOutcome) -> Result<Output> {
    match outcome {
        CmdOutcome::Ran(out) => Ok(out),
        CmdOutcome::Unavailable(u) => Err(anyhow!("{u}")),
    }
}

/// `gh issue edit <N> --remove-label <remove> --add-label <add>`.
pub fn edit_labels(repo_root: &Path, issue: u32, remove: &str, add: &str) -> Result<()> {
    require_write_scope(repo_root)?;
    let out = ran_or_err(
        invocation(
            "worktree.edit_labels",
            AccessIntent::Write,
            repo_root,
            GH_CALL_TIMEOUT,
            [
                "issue",
                "edit",
                &issue.to_string(),
                "--remove-label",
                remove,
                "--add-label",
                add,
            ],
        )
        .run(),
    )
    .context("failed to invoke gh issue edit")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue edit {issue} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `gh issue comment <N> --body <body>` through the #9772 chokepoint
/// (`forge_comment::post_comment`, which appends the dashboard footer the
/// daemon's comments carry). The `owner/repo` slug resolves from
/// `repo_root` so the footer's dashboard link can be built; a resolution
/// failure surfaces as an error rather than posting an unlinked comment.
pub fn comment(repo_root: &Path, issue: u32, body: &str) -> Result<()> {
    require_write_scope(repo_root)?;
    let (owner, name) = resolve_owner_repo(repo_root).ok_or_else(|| {
        anyhow!("could not resolve owner/repo from {repo_root:?} to comment on {issue}")
    })?;
    crate::forge_comment::post_comment(
        std::ffi::OsStr::new(&gh_bin()),
        Some(repo_root),
        &format!("{owner}/{name}"),
        issue,
        /* is_pr */ false,
        body,
    )
    .map(|_| ())
    .map_err(|e| anyhow!("gh issue comment {issue} failed: {e}"))
}

#[derive(Debug, Deserialize)]
pub struct BuildingIssueRow {
    pub number: u32,
    #[serde(default)]
    pub title: String,
}

/// `gh issue list --label loom:building --state open --json number,title`.
pub fn list_building_issues(repo_root: &Path) -> Result<Vec<BuildingIssueRow>> {
    let out = ran_or_err(run_read(
        "worktree.list_building",
        repo_root,
        [
            "issue",
            "list",
            "--label",
            "loom:building",
            "--state",
            "open",
            "--json",
            "number,title",
        ],
    ))
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
    let CmdOutcome::Ran(out) = run_read(
        "worktree.building_label_age",
        repo_root,
        [
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}/events"),
            "--jq",
            r#"[.[] | select(.event == "labeled" and .label.name == "loom:building")] | last | .created_at"#,
        ],
    ) else {
        return None;
    };
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
    let CmdOutcome::Ran(out) = run_read(
        "worktree.orphan_comment",
        repo_root,
        [
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "comments",
            "--jq",
            r###".comments | map(select(.body | startswith("## Orphan Recovery"))) | sort_by(.createdAt) | last | .createdAt // empty"###,
        ],
    ) else {
        return false;
    };
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
        // #8940: the filter must ship each candidate's BODY, not just its
        // number — the phrase filter has nothing to match without it.
        assert!(
            filter.contains("body: (.source.issue.body"),
            "the filter must carry each candidate's body for the #8940 phrase \
             filter, got: {filter}"
        );
    }

    /// One `{number, body}` candidate line, exactly as `gh --jq` emits it.
    fn timeline_candidate(number: u32, body: &str) -> String {
        format!("{}\n", serde_json::json!({ "number": number, "body": body }))
    }

    /// AC (#8116): an issue whose only open PR references it with a NON-closing
    /// keyword (`Part of #N`) must read as `Open`. The closes-graph cannot see
    /// such a PR at all; the timeline's `cross-referenced` event can, which is
    /// the entire reason this transport exists.
    #[test]
    fn a_non_closing_part_of_reference_reads_as_an_open_linked_pr() {
        assert_eq!(
            parse_open_linked_pr_timeline(
                &timeline_candidate(8140, "## Summary\n\nPart of #8116 — phase 2 of the epic."),
                8116
            ),
            OpenPrProbe::Open(8140)
        );
    }

    /// AC (#8940), the core regression: a PR that merely MENTIONS `#N` — no
    /// closing keyword, no partial-increment phrase — is not a linked PR and
    /// must not refuse dispatch. This is the live shape that starved #8322 for
    /// 6.5 days: PR #8314 said "filed #8322 to track it, standing down" and
    /// nothing else.
    #[test]
    fn a_bare_mention_is_not_a_linked_pr() {
        assert_eq!(
            parse_open_linked_pr_timeline(
                &timeline_candidate(
                    8314,
                    "## Summary\n\nFiled #8322 to track the remainder; standing down \
                     without pushing.\n\nCloses #8256",
                ),
                8322
            ),
            OpenPrProbe::NoneOpen,
            "a bare `#N` mention must not count as a linked PR (#8940)"
        );
    }

    /// AC (#8940): the accepted phrase families, and the mention shapes that
    /// must still be discarded alongside them.
    #[test]
    fn only_closing_keywords_and_partial_increment_phrases_count_as_linkage() {
        for (body, expected) in [
            // Closing keywords still count — the timeline is the ONLY leg left
            // when GraphQL cannot answer (#5911), so discarding them here would
            // fall the #4123 guard open.
            ("Closes #8940", OpenPrProbe::Open(700)),
            ("fixes #8940", OpenPrProbe::Open(700)),
            ("Resolved #8940", OpenPrProbe::Open(700)),
            // Partial-increment phrases, with markdown emphasis/colon tolerance.
            ("Part of #8940", OpenPrProbe::Open(700)),
            ("**Part of:** #8940", OpenPrProbe::Open(700)),
            ("Contributes to #8940", OpenPrProbe::Open(700)),
            ("_contributes to_ #8940", OpenPrProbe::Open(700)),
            // Bare mentions and near-misses are not linkage.
            ("See #8940 for context", OpenPrProbe::NoneOpen),
            ("#8940", OpenPrProbe::NoneOpen),
            ("Supersedes #8940", OpenPrProbe::NoneOpen),
            // A different issue's linkage is not this issue's.
            ("Part of #8941", OpenPrProbe::NoneOpen),
            // Trailing-digit boundary: #89401 is not #8940.
            ("Part of #89401", OpenPrProbe::NoneOpen),
            // `fixes` inside a longer word is not a closing keyword.
            ("Prefixes #8940 with a slug", OpenPrProbe::NoneOpen),
        ] {
            assert_eq!(
                parse_open_linked_pr_timeline(&timeline_candidate(700, body), 8940),
                expected,
                "body: {body:?}"
            );
        }
    }

    /// #10197: the extractor names exactly the issues the per-issue guard regex
    /// matches, so the ETA cache reconstructs the same lockout the guard saw.
    #[test]
    #[allow(clippy::regex_creation_in_loops)] // the per-issue regex is the oracle
    fn linkage_refs_agrees_with_the_per_issue_regex() {
        let corpus = [
            "Closes #8940",
            "Closes: #8940",
            "**Part of:** #8940",
            "_contributes to_ #8940, fixes #12",
            "Part of #89401",
            "Prefixes #8940 with a slug",
            "See #8940; Supersedes #7",
            "Closes #5\nfixes #7, part of #8. Part of #5",
            "fixes #0123 and Resolved #44",
            "closes #1,fixes #2 part of#3",
        ];
        // Any issue the guard matches is spelled `#<digits>` in the body.
        let number = regex::Regex::new(r"#([0-9]+)").unwrap();
        for body in corpus {
            let found: Vec<u32> = linkage_refs(body).into_iter().map(|(n, _)| n).collect();
            let candidates: std::collections::BTreeSet<u32> = number
                .captures_iter(body)
                .filter_map(|c| c[1].parse().ok())
                .collect();
            let expected: Vec<u32> = candidates
                .into_iter()
                .filter(|&n| linkage_phrase_regex(n).unwrap().is_match(body))
                .collect();
            assert_eq!(found, expected, "body: {body:?}");
        }
        assert_eq!(
            linkage_refs("Closes #5\nfixes #7, part of #8. Part of #5"),
            vec![
                (5, LinkageKind::Closes),
                (7, LinkageKind::Closes),
                (8, LinkageKind::PartOf)
            ]
        );
        assert_eq!(linkage_refs("**Part of:** #10197"), vec![(10197, LinkageKind::PartOf)]);
    }

    /// Several candidates: only phrase-confirmed ones count, and the verdict is
    /// the lowest of them (deterministic, matching the pre-#8940 `unique |
    /// .[0]` ordering).
    #[test]
    fn multiple_candidates_report_the_lowest_confirmed_pr() {
        let stdout = format!(
            "{}{}{}",
            timeline_candidate(9100, "mentions #8940 in passing"),
            timeline_candidate(9050, "Part of #8940"),
            timeline_candidate(9070, "Contributes to #8940"),
        );
        assert_eq!(parse_open_linked_pr_timeline(&stdout, 8940), OpenPrProbe::Open(9050));
    }

    #[test]
    fn empty_timeline_output_is_a_verified_none_open() {
        assert_eq!(parse_open_linked_pr_timeline("", 8940), OpenPrProbe::NoneOpen);
        assert_eq!(parse_open_linked_pr_timeline("  \n", 8940), OpenPrProbe::NoneOpen);
    }

    #[test]
    fn unparseable_timeline_output_is_a_probe_failure_not_an_absence() {
        assert_eq!(
            parse_open_linked_pr_timeline("gh: rate limit exceeded", 8940),
            OpenPrProbe::ProbeFailed
        );
        // Well-formed JSON that is not a candidate object is equally unreadable
        // — including the pre-#8940 bare-number shape.
        assert_eq!(parse_open_linked_pr_timeline("8140\n", 8940), OpenPrProbe::ProbeFailed);
        assert_eq!(
            parse_open_linked_pr_timeline(r#"{"number":null,"body":"Part of #8940"}"#, 8940),
            OpenPrProbe::ProbeFailed
        );
    }

    /// A candidate with no `body` at all (null or absent) is a bare mention as
    /// far as this filter can tell — discarded, not a probe failure: the
    /// candidate IS readable, it just carries no linking phrase.
    #[test]
    fn a_bodyless_candidate_is_discarded_rather_than_failing_the_probe() {
        assert_eq!(
            parse_open_linked_pr_timeline(r#"{"number":8314,"body":null}"#, 8322),
            OpenPrProbe::NoneOpen
        );
        assert_eq!(
            parse_open_linked_pr_timeline(r#"{"number":8314}"#, 8322),
            OpenPrProbe::NoneOpen
        );
    }

    // --- probe bounding (#8708) ------------------------------------------
    //
    // The 25-hour `clean --deep --safe` hang this issue reports was an
    // unbounded `gh api` child wedged in `futex_do_wait`. Every bounded
    // probe funnels through `bounded_via`; these tests pin the seam's
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

    /// A facade invocation pinned to a fixture executable (no PATH or
    /// `LOOM_GH_BIN` mutation).
    fn probe_inv(program: &Path, timeout: Duration) -> GhInvocation {
        GhInvocation::new(
            Operation::new("worktree.test_probe"),
            AccessIntent::Read,
            GhTarget::None,
            timeout,
        )
        .program(program)
    }

    #[test]
    fn a_hung_gh_probe_is_killed_at_its_deadline_and_reports_no_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let hung = write_probe_fixture(tmp.path(), "hung-probe", "#!/bin/sh\nsleep 30\n");
        let started = std::time::Instant::now();
        let out = bounded_via(probe_inv(&hung, Duration::from_millis(750)));
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
            .find_map(|_| bounded_via(probe_inv(&ok, GH_PROBE_TIMEOUT)))
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
            .find_map(|_| bounded_via(probe_inv(&fail, GH_PROBE_TIMEOUT)))
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
        let out = bounded_via(probe_inv(&tmp.path().join("no-such-gh"), GH_PROBE_TIMEOUT));
        assert!(out.is_none(), "a spawn failure must stay a no-answer: {out:?}");
    }
}

#[cfg(test)]
#[path = "gh_issue_json_tests.rs"]
mod issue_json_tests;
