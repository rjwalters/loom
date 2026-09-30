//! `loom-daemon forge pr-congestion` — the #9063 **Phase 1** congestion
//! signal: a read-only report of how congested the approved-PR merge queue
//! is, and how many queued PRs *could* be bundled into shared-CI merge
//! trains.
//!
//! # Why this exists
//!
//! When many small, independent PRs sit approved (`loom:pr`) at once, each
//! merge advances `main`, which re-dates every other open PR's required
//! checks and serializes the whole queue behind one-at-a-time merges. The
//! 2026-09 session history (merge trains A–D in `WORK_LOG.md`) shows the
//! workaround — hand-bundling several component PRs into one combined PR —
//! being applied *by feel*, with no measurement of when bundling is worth
//! doing. Issue #9063 asks for that measurement first.
//!
//! Per the operator ruling of 2026-09-30, this is **Phase 1 only**:
//! report the queue depth, the story points awaiting merge, and a
//! path-disjoint bundle *estimate* — and nothing else. Bundle execution,
//! single-CI-run candidate branches, and atomic batch merges are Phases 2–3
//! and remain operator-gated; this command never merges, creates, or closes
//! anything, and its output must not be read as license to improvise a
//! merge train by hand.
//!
//! # The estimate (and its honest limits)
//!
//! Two queued PRs are *bundle-compatible* here iff their changed-file sets
//! are disjoint — a purely syntactic rule. It knows nothing about semantic
//! conflicts, feature-flag coupling, or risk class; a green bundle estimate
//! is a *necessary* condition for a merge train, not a sufficient one. That
//! is exactly what Phase 1 is for: surface whether the signal is useful
//! before anyone builds the harder machinery on top of an unvalidated
//! compatibility rule. A PR whose file list was truncated by the fetch cap
//! is conservatively treated as compatible with nothing.
//!
//! # Forge cost
//!
//! At most two billable GraphQL calls per run: one aliased `search` pair
//! (approved queue + review-queue depth, changed files inline), and — only
//! when at least one queued PR links an issue — one aliased issue-labels
//! lookup for the story-point totals. Everything is computed client-side.
//!
//! # Exit-code contract
//!
//! | Exit | Meaning | stdout |
//! |---|---|---|
//! | `0` | Report produced (congested or not — congestion is information, not failure) | human report, or JSON with `--json` |
//! | [`EX_FORGE_DECLINED`] (3) | Gitea — the report is GitHub-only | empty |
//! | [`EX_REPORT_FAILED`] (5) | No verdict — `gh` missing/failed/rate-limited, repo unresolvable, unparseable answer | empty; **fail closed** |
//!
//! `5` mirrors `crate::forge_check_open_pr`'s probe-failed code so the whole
//! `forge` read surface means the same thing: any non-zero exit other than
//! `3` is "the question was not answered", never a measured all-clear.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cmd_out::{run_command, CmdOutcome};
use crate::credential_preflight::apply_gh_config_for_root;
use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED, FORGE_CMD_TIMEOUT};
use crate::worktree_ops::gh::resolve_owner_repo;

/// Exit code for "the report could not be produced" — fail CLOSED.
///
/// `5` so it matches [`crate::forge_check_open_pr::EX_PROBE_FAILED`]'s
/// meaning on the same `forge` surface: the question was not answered, and a
/// caller must not read that as "queue is empty / nothing to do". It also
/// stays clear of `3` ([`EX_FORGE_DECLINED`]) and `4`
/// ([`crate::forge_cmd::EX_FORGE_HEAD_MISMATCH`]).
pub const EX_REPORT_FAILED: i32 = 5;

/// Default congestion threshold for the approved-queue depth: strictly more
/// than this many open `loom:pr` rows trips the congestion verdict (the
/// issue's ">5 open PRs" example).
pub const DEFAULT_MAX_OPEN: usize = 5;

/// Default congestion threshold for story points awaiting merge: strictly
/// more than this many points trips the verdict. 13 (the top Fibonacci bucket
/// in `points:*`) is the smallest total that could not possibly be split into
/// smaller independently-mergeable units.
pub const DEFAULT_MAX_POINTS: u32 = 13;

/// Congestion thresholds, CLI-overridable so the report can be replayed at
/// different sensitivities without re-fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Thresholds {
    /// Trip when the approved-queue depth is strictly greater.
    pub max_open: usize,
    /// Trip when known story points awaiting merge are strictly greater.
    pub max_points: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            max_open: DEFAULT_MAX_OPEN,
            max_points: DEFAULT_MAX_POINTS,
        }
    }
}

/// Labels that park a PR out of any bundling consideration — the same
/// "a human / a hold is involved" set the merge path already respects. A
/// held PR is *counted* (it is queue depth) but is never a bundle candidate.
pub const HOLD_LABELS: &[&str] = &["loom:operator", "loom:operator-only", "loom:blocked"];

/// One queued (approved) PR, as far as Phase 1 needs to know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueuedPr {
    /// PR number.
    pub number: u32,
    /// PR title (verbatim, for the human render).
    pub title: String,
    /// ISO-8601 creation timestamp; the bundle walk is oldest-first and the
    /// ordering is applied client-side so it never depends on search order.
    pub created_at: String,
    /// Label names on the PR.
    pub labels: Vec<String>,
    /// Changed file paths (as fetched; possibly truncated — see
    /// [`Self::files_truncated`]).
    pub files: Vec<String>,
    /// `true` when the PR's real file count exceeded the fetch cap, in which
    /// case [`Self::files`] is incomplete and the PR is treated as compatible
    /// with nothing.
    pub files_truncated: bool,
    /// The issue this PR closes, when one could be identified: the
    /// `feature/issue-N` head branch first (the repo's convention), then a
    /// close-keyword scan of the body.
    pub linked_issue: Option<u32>,
    /// Story points of the linked issue (`points:*` label), when both the
    /// link and the label were resolvable.
    pub points: Option<u32>,
}

impl QueuedPr {
    /// Whether any hold label parks this PR out of bundling.
    pub fn is_held(&self) -> bool {
        self.labels
            .iter()
            .any(|l| HOLD_LABELS.contains(&l.as_str()))
    }
}

/// The Phase 1 congestion report: counts, the bundle estimate, and the
/// threshold verdict. Everything here is derived client-side from the two
/// GraphQL fetches; see [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CongestionReport {
    /// `owner/repo` the report describes.
    pub owner_repo: String,
    /// Open PRs labeled `loom:pr` (the approved queue the merge captain
    /// drains). This is the depth that drives the open threshold.
    pub approved_open: usize,
    /// Open PRs labeled `loom:review-requested` — context only: they are not
    /// mergeable yet, but they are the congestion that is about to arrive.
    pub review_requested_open: usize,
    /// Sum of known story points across the approved queue. A *lower bound*:
    /// PRs whose linked issue could not be resolved contribute zero here and
    /// are counted in [`Self::points_unmapped`] instead.
    pub points_total: u32,
    /// Approved PRs with no resolvable issue/points mapping.
    pub points_unmapped: usize,
    /// Approved PRs excluded from bundling by a hold label.
    pub held: Vec<u32>,
    /// Greedy path-disjoint bundles over the non-held candidates, in
    /// oldest-first order. A singleton bundle means "no queued sibling is
    /// file-disjoint with this PR".
    pub bundles: Vec<Vec<u32>>,
    /// Candidates that share a bundle with at least one sibling — the PRs a
    /// combined-CI candidate *could* cover, given nothing but path disjointness.
    pub bundleable: usize,
    /// Whether any threshold tripped. Report-only: never an error condition.
    pub congested: bool,
    /// Which threshold tripped, phrased for the human render (empty when not
    /// congested).
    pub tripped: Vec<String>,
    /// The thresholds this report was evaluated against.
    pub thresholds: Thresholds,
}

/// Extract the closed issue number from a PR's head branch and body.
///
/// The head branch wins (`feature/issue-42` — the repo's own worktree
/// convention, and the one field no body template can omit or mangle); the
/// body is only scanned when the branch does not match, looking for the
/// GitHub close keywords (`Closes #N` et al., case-insensitive, `:` optional)
/// and taking the *first* match — a PR body listing several issues closes the
/// first one it names.
pub fn link_issue_number(head_ref: &str, body: &str) -> Option<u32> {
    const BRANCH_PREFIX: &str = "feature/issue-";
    if let Some(rest) = head_ref.strip_prefix(BRANCH_PREFIX) {
        if let Ok(n) = rest
            .split(['-', '/', '.'])
            .next()
            .unwrap_or("")
            .parse::<u32>()
        {
            return Some(n);
        }
    }

    let lower = body.to_ascii_lowercase();
    for keyword in [
        "closes", "closed", "close", "fixes", "fixed", "fix", "resolves", "resolved", "resolve",
    ] {
        let mut from = 0usize;
        while let Some(pos) = lower[from..].find(keyword) {
            let abs = from + pos;
            let after = keyword.len();
            let rest = &lower[abs + after..];
            // Optional ':' plus whitespace, then '#<digits>'.
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
            if let Some(rest) = rest.strip_prefix('#') {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
            from = abs + after;
            if from >= lower.len() {
                break;
            }
        }
    }
    None
}

/// The `points:*` story-point label value, when exactly the expected label
/// shape is present. Multiple `points:*` labels are malformed by the label
/// state machine ("exactly one"), so `None` — the row counts as unmapped —
/// beats guessing which one is real.
pub fn points_from_labels(labels: &[String]) -> Option<u32> {
    let mut found: Option<u32> = None;
    for label in labels {
        if let Some(rest) = label.strip_prefix("points:") {
            match rest.parse::<u32>() {
                Ok(n) if found.is_none() => found = Some(n),
                _ => return None,
            }
        }
    }
    found
}

/// Two file sets are bundle-compatible: no path in common. An empty set is
/// compatible with anything — a PR that changed no files cannot collide on a
/// path. (Truncation is handled by the caller, not here: an *incomplete* set
/// proves nothing about overlap.)
pub fn path_disjoint(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    a.is_disjoint(b)
}

/// Greedy oldest-first bundling: walk the candidates in the given order
/// (the caller passes them oldest-first) and place each into the first
/// existing bundle whose accumulated file set is disjoint from the
/// candidate's; open a new bundle when none fits. Deterministic for a given
/// input order, which is what makes the report reproducible run-to-run.
pub fn greedy_bundles(candidates: &[&QueuedPr]) -> Vec<Vec<u32>> {
    let mut bundles: Vec<(BTreeSet<String>, Vec<u32>)> = Vec::new();
    for pr in candidates {
        let files: BTreeSet<String> = pr.files.iter().cloned().collect();
        match bundles
            .iter_mut()
            .find(|(seen, _)| !pr.files_truncated && path_disjoint(&files, seen))
        {
            Some((seen, numbers)) => {
                seen.extend(files);
                numbers.push(pr.number);
            }
            None => bundles.push((files, vec![pr.number])),
        }
    }
    bundles.into_iter().map(|(_, numbers)| numbers).collect()
}

/// Derive the whole report from fetched PR rows. This is the pure core the
/// fetch layer feeds and the unit tests exercise — no `gh`, no process
/// boundary, no clock.
pub fn evaluate(
    owner_repo: &str,
    mut queued: Vec<QueuedPr>,
    review_requested_open: usize,
    thresholds: Thresholds,
) -> CongestionReport {
    // Deterministic oldest-first walk regardless of GraphQL search order.
    queued.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.number.cmp(&b.number))
    });

    let (held_rows, candidates): (Vec<QueuedPr>, Vec<QueuedPr>) =
        queued.into_iter().partition(QueuedPr::is_held);

    let mut points_total: u32 = 0;
    let mut points_unmapped: usize = 0;
    for pr in &candidates {
        match pr.points {
            Some(p) => points_total += p,
            None => points_unmapped += 1,
        }
    }

    let refs: Vec<&QueuedPr> = candidates.iter().collect();
    let bundles = greedy_bundles(&refs);
    let bundleable = bundles
        .iter()
        .filter(|b| b.len() > 1)
        .map(|b| b.len())
        .sum::<usize>();

    let mut tripped = Vec::new();
    if candidates.len() > thresholds.max_open {
        tripped.push(format!("open {} > {}", candidates.len(), thresholds.max_open));
    }
    if points_total > thresholds.max_points {
        tripped.push(format!("points {points_total} > {}", thresholds.max_points));
    }

    CongestionReport {
        owner_repo: owner_repo.to_string(),
        approved_open: candidates.len() + held_rows.len(),
        review_requested_open,
        points_total,
        points_unmapped,
        held: held_rows.iter().map(|p| p.number).collect(),
        bundles,
        bundleable,
        congested: !tripped.is_empty(),
        tripped,
        thresholds,
    }
}

// ---------------------------------------------------------------------------
// GraphQL fetch layer (thin — everything above is the testable core)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TopData {
    #[serde(default)]
    approved: Option<PrSearch>,
    #[serde(default)]
    review: Option<CountOnly>,
    #[serde(default)]
    repository: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct PrSearch {
    #[serde(default)]
    nodes: Vec<Option<PrNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountOnly {
    issue_count: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: i64,
    title: String,
    created_at: String,
    #[serde(default)]
    body: String,
    head_ref_name: String,
    #[serde(default)]
    files: Option<FilesConn>,
    #[serde(default)]
    labels: Option<LabelsConn>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesConn {
    #[serde(default)]
    total_count: i64,
    #[serde(default)]
    nodes: Vec<FileNode>,
}

#[derive(Deserialize)]
struct FileNode {
    path: String,
}

#[derive(Deserialize)]
struct LabelsConn {
    #[serde(default)]
    nodes: Vec<LabelNode>,
}

#[derive(Deserialize)]
struct LabelNode {
    name: String,
}

#[derive(Deserialize)]
struct IssueLabels {
    #[serde(default)]
    labels: Option<LabelsConn>,
}

const APPROVED_LABEL: &str = "loom:pr";
const REVIEW_LABEL: &str = "loom:review-requested";
/// Search page cap for the approved queue. Past the cap the queue is
/// *undercounted* (`approved_open` reads at most this many) and only the
/// first rows get the bundle treatment — a queue this deep is already far
/// past every Phase 1 threshold, so precision there buys nothing.
const MAX_PRS: i64 = 30;
const FILES_FIRST: i64 = 100;
/// Upper bound on the aliased issue-labels lookup (query 2). A queue deeper
/// than this simply reports its unmapped PRs honestly.
const MAX_LINKED: usize = 20;

/// Run `gh api graphql` and return the response's top-level `data` object,
/// failing closed on subprocess failure, GraphQL-level `errors`, or a
/// missing `data`.
fn gh_graphql_data(root: &Path, query: &str) -> Result<serde_json::Value> {
    let mut cmd = Command::new("gh");
    cmd.arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={query}"))
        .current_dir(root)
        .stdin(Stdio::null());
    apply_gh_config_for_root(&mut cmd, root);

    let outcome = run_command(cmd, FORGE_CMD_TIMEOUT);
    let stdout = match outcome {
        CmdOutcome::Ran(o) if o.status.success() => o.stdout,
        CmdOutcome::Ran(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if err.is_empty() {
                bail!("gh api graphql failed (no stderr detail)");
            }
            bail!("gh api graphql failed: {err}");
        }
        CmdOutcome::Unavailable(u) => bail!("gh could not be run: {u}"),
    };

    let parsed: serde_json::Value =
        serde_json::from_slice(&stdout).context("gh api graphql returned unparseable JSON")?;
    // GraphQL-level errors (partial data + errors array): any entry means the
    // answer is incomplete, and Phase 1 has no business reporting a half
    // answer — fail closed.
    if let Some(errs) = parsed.get("errors").and_then(|e| e.as_array()) {
        if let Some(first) = errs.first() {
            let message = first
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("(no message)");
            bail!("gh api graphql reported errors: {message}");
        }
    }
    parsed
        .get("data")
        .cloned()
        .context("gh api graphql returned no data object")
}

/// Build and run query 1 (queue depths + approved rows) and return the raw
/// JSON fragments for [`fetch_report`] to decode.
fn fetch_queues(root: &Path, owner: &str, repo: &str) -> Result<(PrSearch, usize)> {
    let q = format!(
        "query {{ \
approved: search(query: \"repo:{owner}/{repo} is:pr is:open label:{APPROVED_LABEL}\", \
type: ISSUE, first: {MAX_PRS}) {{ issueCount nodes {{ ... on PullRequest {{ \
number title createdAt body headRefName \
files(first: {FILES_FIRST}) {{ totalCount nodes {{ path }} }} \
labels(first: 20) {{ nodes {{ name }} }} }} }} }} \
review: search(query: \"repo:{owner}/{repo} is:pr is:open label:{REVIEW_LABEL}\", \
type: ISSUE, first: 1) {{ issueCount }} }}"
    );
    let data = gh_graphql_data(root, &q)?;
    let top: TopData = serde_json::from_value(data)
        .context("unexpected GraphQL data shape for the queue search")?;
    let approved = top
        .approved
        .context("GraphQL data missing `approved` search")?;
    let review = top.review.context("GraphQL data missing `review` search")?;
    Ok((approved, review.issue_count.max(0) as usize))
}

/// Build and run query 2 (story points for the linked issues), returning an
/// issue-number → points map. Issues that were deleted or unresolvable
/// simply do not appear in the map; the caller counts those PRs as unmapped.
fn fetch_points(
    root: &Path,
    owner: &str,
    repo: &str,
    issues: &[u32],
) -> Result<std::collections::BTreeMap<u32, u32>> {
    let mut out = std::collections::BTreeMap::new();
    if issues.is_empty() {
        return Ok(out);
    }
    // Aliases are positional (`i0`, `i1`, …) and tracked beside the numbers,
    // because the response's `repository` object is keyed by alias, not by
    // issue number.
    let mut aliases: Vec<(String, u32)> = Vec::new();
    let mut fields = String::new();
    for (i, n) in issues.iter().take(MAX_LINKED).enumerate() {
        let alias = format!("i{i}");
        fields.push_str(&format!(
            " {alias}: issue(number: {n}) {{ labels(first: 10) {{ nodes {{ name }} }} }}"
        ));
        aliases.push((alias, *n));
    }
    let q = format!("query {{ repository(owner: \"{owner}\", name: \"{repo}\") {{{fields}}}}}");
    let data = gh_graphql_data(root, &q)?;
    let top: TopData = serde_json::from_value(data)
        .context("unexpected GraphQL data shape for the points lookup")?;
    let repository = top
        .repository
        .context("GraphQL data missing `repository`")?;
    for (alias, number) in aliases {
        let Some(value) = repository.get(&alias) else {
            continue; // deleted issue: GitHub omits the alias entry entirely
        };
        if value.is_null() {
            continue; // unresolvable issue: aliased field came back null
        }
        let node: IssueLabels = serde_json::from_value(value.clone())
            .context("unexpected issue shape in the points lookup")?;
        if let Some(labels) = node.labels {
            let names: Vec<String> = labels.nodes.into_iter().map(|n| n.name).collect();
            if let Some(points) = points_from_labels(&names) {
                out.insert(number, points);
            }
        }
    }
    Ok(out)
}

/// Fetch everything the report needs and run [`evaluate`]. Two billable
/// GraphQL calls at most (the second only when at least one queued PR links
/// an issue).
fn fetch_report(
    root: &Path,
    owner: &str,
    repo: &str,
    thresholds: Thresholds,
) -> Result<CongestionReport> {
    let (search, review_open) = fetch_queues(root, owner, repo)?;

    let mut queued: Vec<QueuedPr> = Vec::new();
    for node in search.nodes.into_iter().flatten() {
        let files_truncated = match &node.files {
            Some(conn) => conn.total_count > conn.nodes.len() as i64,
            None => true, // files connection missing: treat as unknowable
        };
        let files = node
            .files
            .map(|f| f.nodes.into_iter().map(|n| n.path).collect())
            .unwrap_or_default();
        let labels: Vec<String> = node
            .labels
            .map(|l| l.nodes.into_iter().map(|n| n.name).collect())
            .unwrap_or_default();
        queued.push(QueuedPr {
            number: node.number.clamp(0, u32::MAX as i64) as u32,
            title: node.title,
            created_at: node.created_at,
            labels,
            files,
            files_truncated,
            linked_issue: link_issue_number(&node.head_ref_name, &node.body),
            points: None,
        });
    }

    let linked: Vec<u32> = queued
        .iter()
        .filter_map(|p| p.linked_issue)
        .take(MAX_LINKED)
        .collect();
    let points = fetch_points(root, owner, repo, &linked)?;
    for pr in &mut queued {
        if let Some(issue) = pr.linked_issue {
            pr.points = points.get(&issue).copied();
        }
    }

    Ok(evaluate(&format!("{owner}/{repo}"), queued, review_open, thresholds))
}

/// The human render. Congestion is stated as information, never as an error,
/// and the report-only boundary is restated on every render so a copy-pasted
/// output cannot be mistaken for a go-ahead to hand-bundle a merge train.
pub fn render_human(r: &CongestionReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "PR congestion report — {} (#9063 Phase 1, report-only)\n",
        r.owner_repo
    ));
    out.push_str(&format!("  approved queue ({}): {}\n", APPROVED_LABEL, r.approved_open));
    out.push_str(&format!(
        "  awaiting Judge ({}): {}  [context — not mergeable yet]\n",
        REVIEW_LABEL, r.review_requested_open
    ));
    out.push_str(&format!(
        "  story points awaiting merge: {} ({} unmapped)  [context — lower bound]\n",
        r.points_total, r.points_unmapped
    ));
    out.push_str(&format!(
        "  thresholds: open > {} OR points > {}  →  congested: {}\n",
        r.thresholds.max_open,
        r.thresholds.max_points,
        if r.congested { "YES" } else { "no" }
    ));
    if !r.tripped.is_empty() {
        out.push_str(&format!("  tripped: {}\n", r.tripped.join("; ")));
    }
    out.push_str(&format!(
        "  bundle estimate (path-disjoint, oldest-first greedy): {} PRs could share a CI run\n",
        r.bundleable
    ));
    for (i, bundle) in r.bundles.iter().enumerate() {
        let joined: Vec<String> = bundle.iter().map(|n| format!("#{n}")).collect();
        let note = if bundle.len() > 1 { "  (disjoint)" } else { "" };
        out.push_str(&format!("    bundle {}: {}{}\n", i + 1, joined.join(", "), note));
    }
    if !r.held.is_empty() {
        let held: Vec<String> = r.held.iter().map(|n| format!("#{n}")).collect();
        out.push_str(&format!("    held (excluded from bundling): {}\n", held.join(", ")));
    }
    out.push_str(
        "  Phase note: report-only — bundle/merge-train execution is NOT \
implemented; #9063 Phases 2–3 are operator-gated (ruling 2026-09-30).\n",
    );
    out
}

/// Handle `loom-daemon forge pr-congestion [--json] [--max-open N]
/// [--max-points N]`. Never returns (exits the process); returns `Err` only
/// when the current directory cannot be resolved.
///
/// Resolution is cwd-scoped, exactly like
/// [`crate::forge_check_open_pr::handle`]: run it from anywhere inside the
/// repository and it answers about that repository.
pub fn handle(json: bool, max_open: usize, max_points: u32) -> Result<()> {
    let root: PathBuf = std::env::current_dir().context(
        "loom-daemon forge pr-congestion: could not resolve the current directory; \
         run it from inside the repository whose merge queue you are asking about",
    )?;

    // GitHub-only by construction: both fetches are GitHub GraphQL. Declining
    // on Gitea mirrors `forge check-open-pr`; silently reporting an empty
    // queue there would be a false all-clear.
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge pr-congestion: the congestion report is GitHub-only; \
             inspect the loom:pr queue by hand."
        );
        std::process::exit(EX_FORGE_DECLINED);
    }

    let Some((owner, repo)) = resolve_owner_repo(&root) else {
        eprintln!(
            "loom-daemon forge pr-congestion: could not resolve owner/repo from the \
             git remotes — is this directory a GitHub checkout?"
        );
        std::process::exit(EX_REPORT_FAILED);
    };

    let thresholds = Thresholds {
        max_open,
        max_points,
    };
    let report = match fetch_report(&root, &owner, &repo, thresholds) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("loom-daemon forge pr-congestion: {e:#}");
            eprintln!("No report was produced — this is NOT a measurement of an empty queue.");
            std::process::exit(EX_REPORT_FAILED);
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("CongestionReport is a plain data struct")
        );
    } else {
        print!("{}", render_human(&report));
    }
    std::process::exit(0);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pr(number: u32, created_at: &str, files: &[&str]) -> QueuedPr {
        QueuedPr {
            number,
            title: format!("pr #{number}"),
            created_at: created_at.to_string(),
            labels: vec![APPROVED_LABEL.to_string()],
            files: files.iter().map(|s| s.to_string()).collect(),
            files_truncated: false,
            linked_issue: Some(number),
            points: Some(1),
        }
    }

    // -- link_issue_number ---------------------------------------------------

    #[test]
    fn branch_link_wins_and_tolerates_suffixes() {
        assert_eq!(link_issue_number("feature/issue-9063", ""), Some(9063));
        assert_eq!(link_issue_number("feature/issue-42-extra", ""), Some(42));
        // A branch that only looks close is not a link.
        assert_eq!(link_issue_number("feature/issue-", "Closes #7"), Some(7));
    }

    #[test]
    fn body_link_scans_close_keywords_and_takes_the_first() {
        assert_eq!(link_issue_number("", "Closes #42"), Some(42));
        assert_eq!(link_issue_number("", "fixes: #7"), Some(7));
        assert_eq!(link_issue_number("", "RESOLVED #9"), Some(9));
        assert_eq!(link_issue_number("", "Closes #1, also fixes #2"), Some(1));
        assert_eq!(link_issue_number("", "no links here, see #1234 for color"), None);
    }

    // -- points_from_labels --------------------------------------------------

    #[test]
    fn points_label_parses_and_multiple_points_labels_are_rejected() {
        assert_eq!(points_from_labels(&["points:8".into()]), Some(8));
        assert_eq!(points_from_labels(&["loom:pr".into()]), None);
        assert_eq!(points_from_labels(&["points:3".into(), "points:5".into()]), None);
    }

    // -- greedy_bundles ------------------------------------------------------

    #[test]
    fn overlapping_prs_never_share_a_bundle() {
        let a = pr(1, "t1", &["src/a.rs"]);
        let b = pr(2, "t2", &["src/a.rs", "src/b.rs"]);
        let bundles = greedy_bundles(&[&a, &b]);
        assert_eq!(bundles, vec![vec![1], vec![2]]);
    }

    #[test]
    fn disjoint_prs_share_a_bundle_and_files_accumulate() {
        let a = pr(1, "t1", &["src/a.rs"]);
        let b = pr(2, "t2", &["src/b.rs"]);
        let c = pr(3, "t3", &["src/a.rs", "docs/x.md"]); // collides with a only
        let bundles = greedy_bundles(&[&a, &b, &c]);
        assert_eq!(bundles, vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn truncated_file_list_never_joins_an_existing_bundle() {
        let a = pr(1, "t1", &["src/a.rs"]);
        let mut b = pr(2, "t2", &["src/a.rs"]);
        b.files_truncated = true; // b's real file set is unknown
        let bundles = greedy_bundles(&[&a, &b]);
        assert_eq!(bundles, vec![vec![1], vec![2]]);
    }

    #[test]
    fn order_is_the_callers_order_not_the_pr_numbers() {
        let a = pr(9, "t1", &["src/a.rs"]);
        let b = pr(2, "t2", &["src/b.rs"]);
        let bundles = greedy_bundles(&[&a, &b]);
        assert_eq!(bundles, vec![vec![9, 2]]);
    }

    // -- evaluate ------------------------------------------------------------

    #[test]
    fn held_prs_are_counted_but_excluded_from_bundling() {
        let mut held = pr(1, "t1", &["src/a.rs"]);
        held.labels.push("loom:operator".into());
        let free = pr(2, "t2", &["src/b.rs"]);
        let r = evaluate("o/r", vec![held, free], 0, Thresholds::default());
        assert_eq!(r.approved_open, 2, "held rows still count as depth");
        assert_eq!(r.held, vec![1]);
        assert_eq!(r.bundles, vec![vec![2]], "held row is not a candidate");
    }

    #[test]
    fn depth_threshold_trips_strictly_above_max_open() {
        let mk = |i: u32| pr(i, &format!("t{i:04}"), &[&format!("f{i}.rs")]);
        let five: Vec<QueuedPr> = (1..=5).map(mk).collect();
        let r = evaluate("o/r", five, 0, Thresholds::default());
        assert!(!r.congested, "5 open with max_open 5 is at-threshold, not over");
        let six: Vec<QueuedPr> = (1..=6).map(mk).collect();
        let r = evaluate("o/r", six, 0, Thresholds::default());
        assert!(r.congested);
        assert!(r.tripped[0].contains("open 6 > 5"), "{:?}", r.tripped);
    }

    #[test]
    fn points_threshold_stays_at_threshold_and_unmapped_is_a_lower_bound() {
        let mut a = pr(1, "t1", &["a.rs"]);
        a.points = Some(13);
        let mut b = pr(2, "t2", &["b.rs"]);
        b.points = None; // linked issue unresolvable
        let r = evaluate("o/r", vec![a, b], 0, Thresholds::default());
        assert_eq!(r.points_total, 13, "known sum only");
        assert_eq!(r.points_unmapped, 1);
        assert!(!r.congested, "13 with max_points 13 is at-threshold, not over");
    }

    #[test]
    fn oldest_first_ordering_is_applied_regardless_of_input_order() {
        let a = pr(1, "2026-01-02T00:00:00Z", &["a.rs"]);
        let b = pr(2, "2026-01-01T00:00:00Z", &["b.rs"]);
        let r = evaluate("o/r", vec![a, b], 0, Thresholds::default());
        assert_eq!(r.bundles, vec![vec![2, 1]], "b is older and must lead");
    }

    #[test]
    fn bundleable_counts_only_prs_sharing_a_bundle() {
        let a = pr(1, "t1", &["a.rs"]);
        let b = pr(2, "t2", &["b.rs"]);
        let c = pr(3, "t3", &["a.rs"]); // conflicts with a
        let r = evaluate("o/r", vec![a, b, c], 0, Thresholds::default());
        assert_eq!(r.bundles, vec![vec![1, 2], vec![3]]);
        assert_eq!(r.bundleable, 2);
    }

    #[test]
    fn render_always_carries_the_report_only_boundary() {
        let r = evaluate("o/r", vec![], 0, Thresholds::default());
        let text = render_human(&r);
        assert!(text.contains("report-only"), "{text}");
        assert!(text.contains("NOT implemented"), "{text}");
        assert!(text.contains("congested: no"), "{text}");
    }

    /// Exit-code hygiene: the report-failed code must stay distinct from the
    /// Gitea decline so "could not answer" never masquerades as an honest
    /// refusal on other grounds.
    #[test]
    fn report_failed_code_is_distinct_from_the_decline() {
        assert_ne!(EX_REPORT_FAILED, EX_FORGE_DECLINED);
    }
}
