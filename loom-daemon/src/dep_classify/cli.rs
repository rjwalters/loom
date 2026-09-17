//! The CLI boundary for dependency classification (epic #7810, PR 3).
//!
//! Everything below this line is I/O scheduling and rendering. The decisions
//! themselves live in [`super::defer`], [`super::unescalate`], [`super::fact`]
//! and [`super::cycle`], which know nothing about a forge, a terminal or an
//! exit code.
//!
//! # What is contract here
//!
//! Role prompts invoke these entry points **by path** and parse their stdout
//! line-wise — `champion-issue-promo.md`, `champion-reference.md` and
//! `curator.md` all do. So three things are frozen:
//!
//! - **argv**: the same long flags the shell accepted, in any order;
//! - **stdout**: the same marker lines, in the same order;
//! - **exit codes**: 0/1/2 everywhere, plus 3 (re-evaluate) and 4 (promote a
//!   subset) on `--check-defer`.
//!
//! `defaults/scripts/tests/test-classify-dependency-block.sh` drives all three
//! through that CLI with a stubbed `gh`, and was kept rather than translated:
//! assertions written against the shell implementation still passing against
//! this one is the equivalence evidence.
//!
//! # Bounded cost
//!
//! The shell's header promises "one cached read of the issue, one cached read
//! per DISTINCT referenced blocker, and — only when an open blocker is found —
//! one bounded cycle walk". That promise is kept here by
//! [`super::defer::blockers_to_classify`] and its un-escalate twin, which
//! answer *whether a read is warranted at all* without restating the gate
//! order. A proposal with no dependency findings still costs exactly one read.

use super::{apply, consts, cycle, defer, fact, forge, state, subset, unescalate};
use crate::script_helpers::run_git;
use std::path::{Path, PathBuf};

/// The directory forge calls are made from.
///
/// The enclosing repository root when there is one, else `cwd` unchanged.
///
/// This is not cosmetic. `script_helpers::gh_cmd` looks for the read cache at
/// `<dir>/.loom/scripts/gh-cached`, and the shell resolved that relative to the
/// SCRIPT's own directory — always right, wherever it was invoked from. Passing
/// the process cwd instead silently loses the cache for any caller that runs
/// from a subdirectory: still correct, but one uncached `gh` call per blocker
/// per pass, against a module whose header promises "one cached read of the
/// issue, one cached read per DISTINCT referenced blocker".
#[must_use]
fn forge_dir(cwd: &Path) -> PathBuf {
    crate::repo_root::find_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// Which question is being asked. Defaults to [`Mode::Defer`], as the shell did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Defer,
    Unescalate,
    FactUnescalate,
}

/// `classify-dependency-block.sh`'s argv.
#[derive(Debug, Clone, Default)]
pub struct ClassifyOpts {
    pub issue: i64,
    pub repo: Option<String>,
    pub mode: Mode,
    pub apply: bool,
    pub resolutions_file: Option<String>,
    pub commit: Option<String>,
    pub findings_file: Option<String>,
    pub skip_cycle_check: bool,
    pub no_cache: bool,
}

/// `detect-dependency-cycle.sh`'s argv.
#[derive(Debug, Clone, Default)]
pub struct CycleOpts {
    pub issue: i64,
    pub repo: Option<String>,
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    pub max_steps: Option<usize>,
    pub report: bool,
    pub no_cache: bool,
}

/// `detect-startable-subset.sh`'s argv.
#[derive(Debug, Clone, Default)]
pub struct SubsetOpts {
    pub issue: i64,
    pub repo: Option<String>,
    pub body_file: Option<String>,
    pub no_cache: bool,
}

fn err(msg: &str) {
    eprintln!("ERROR: {msg}");
}

fn warn(msg: &str) {
    eprintln!("WARNING: {msg}");
}

/// Resolve `owner/repo`, git remote first.
///
/// Order matters and is the shell's: `git remote get-url origin` is answered
/// locally, so it keeps working offline and under GraphQL exhaustion — which
/// is exactly when a Champion pass most needs to keep running. `gh repo view`
/// is the fallback for a checkout with no origin.
#[must_use]
fn resolve_repo(cwd: &Path) -> Option<String> {
    if let Some(nwo) = run_git(cwd, &["remote", "get-url", "origin"])
        .ok_stdout_trimmed()
        .as_deref()
        .and_then(nwo_from_remote_url)
    {
        return Some(nwo);
    }

    #[derive(serde::Deserialize)]
    struct NameWithOwner {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let q: crate::cmd_out::Query<NameWithOwner> = crate::script_helpers::gh_query(
        &["repo", "view", "--json", "nameWithOwner"],
        cwd,
        false,
        |n: &NameWithOwner| n.name_with_owner.is_empty(),
    );
    match q {
        crate::cmd_out::Query::Populated(n) => Some(n.name_with_owner),
        _ => None,
    }
}

/// `owner/repo` from a git remote URL, in either the SSH
/// (`git@host:owner/repo.git`) or HTTPS (`https://host/owner/repo`) form.
///
/// `None` when the URL cannot yield both halves — the shell's
/// `[[ "$url" == */* ]]` guard, which falls through to `gh` rather than
/// guessing.
#[must_use]
fn nwo_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.strip_suffix(".git").unwrap_or(url);
    let mut parts: Vec<&str> = trimmed.rsplit(['/', ':']).take(2).collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    parts.reverse();
    Some(parts.join("/"))
}

/// Resolve the repo from `--repo` or the checkout, or exit 2 with the shell's
/// message.
fn repo_or_exit(explicit: Option<&String>, cwd: &Path) -> Result<String, i32> {
    if let Some(r) = explicit.filter(|r| !r.is_empty()) {
        return Ok(r.clone());
    }
    match resolve_repo(cwd) {
        Some(r) if !r.is_empty() => Ok(r),
        _ => {
            err("could not determine the repo - pass --repo <owner/repo>");
            Err(2)
        }
    }
}

/// `--issue` must be a positive integer, as the shell's `^[0-9]+$` required.
fn check_issue(issue: i64) -> Result<(), i32> {
    if issue <= 0 {
        err(&format!("--issue must be a number (got: {issue})"));
        return Err(2);
    }
    Ok(())
}

fn read_file(path: &str, flag: &str) -> Result<String, i32> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(_) => {
            err(&format!("{flag} not found: {path}"));
            Err(2)
        }
    }
}

// ===========================================================================
// classify-dependency-block
// ===========================================================================

/// Run `classify-dependency-block`, returning its exit code.
#[must_use]
pub fn run_classify(cwd: &Path, opts: &ClassifyOpts) -> i32 {
    match classify(cwd, opts) {
        Ok(code) | Err(code) => code,
    }
}

fn classify(cwd: &Path, opts: &ClassifyOpts) -> Result<i32, i32> {
    let cwd = &forge_dir(cwd);
    check_issue(opts.issue)?;

    // `--apply` on `--check-defer` would be a silent no-op: that mode only
    // reports. Rejecting it is what keeps a mis-typed invocation from looking
    // like a successful un-escalation that changed nothing.
    if opts.apply && opts.mode == Mode::Defer {
        err("--apply is only meaningful with --check-unescalate or --check-fact-unescalate");
        return Err(2);
    }
    if opts.resolutions_file.is_some() && opts.mode != Mode::FactUnescalate {
        err("--resolutions-file is only meaningful with --check-fact-unescalate");
        return Err(2);
    }
    if opts.commit.is_some() && opts.mode != Mode::FactUnescalate {
        err("--commit is only meaningful with --check-fact-unescalate");
        return Err(2);
    }

    let findings_override = match opts.findings_file.as_deref() {
        Some(p) => Some(read_file(p, "--findings-file")?),
        None => None,
    };

    let repo = repo_or_exit(opts.repo.as_ref(), cwd)?;
    let use_cache = !opts.no_cache;
    let self_node = format!("{repo}#{}", opts.issue);

    let fields = match opts.mode {
        Mode::Defer => "body,comments",
        Mode::Unescalate | Mode::FactUnescalate => "body,labels,comments",
    };
    let Some(view) = forge::read_issue(opts.issue, &repo, cwd, use_cache, fields) else {
        err(&format!("could not read {self_node}"));
        return Err(2);
    };

    match opts.mode {
        Mode::Defer => defer_mode(cwd, opts, &repo, &self_node, &view, findings_override),
        Mode::Unescalate => unescalate_mode(cwd, opts, &repo, &self_node, &view, findings_override),
        Mode::FactUnescalate => fact_mode(cwd, opts, &repo, &self_node, &view),
    }
}

fn defer_mode(
    cwd: &Path,
    opts: &ClassifyOpts,
    repo: &str,
    self_node: &str,
    view: &forge::IssueView,
    findings_override: Option<String>,
) -> Result<i32, i32> {
    let mut inputs = defer::Inputs {
        body: view.body.clone(),
        // The **last** rejection comment, not the first: a proposal accumulates
        // verdicts and the current decision is about the most recent one.
        source_body: findings_override
            .unwrap_or_else(|| view.last_comment_containing(consts::REJECT_NEEDLE)),
        ..Default::default()
    };

    let blockers = defer::blockers_to_classify(&inputs, repo, self_node);
    if !blockers.is_empty() {
        inputs.refs = state::classify_refs(&blockers.join("\n"), cwd, !opts.no_cache);
    }

    // Only when an open blocker survives classification: a walk costs one read
    // per node, and nothing about a cleared blocker set needs one.
    inputs.has_cycle = !inputs.refs.open.is_empty()
        && !opts.skip_cycle_check
        && cycle_present(cwd, repo, opts.issue, !opts.no_cache);

    let decision = defer::decide(&inputs, repo, self_node);
    let (out, code) = defer::render(&decision, &inputs.refs.unknown);
    print!("{out}");
    Ok(code)
}

/// Whether the dependency graph rooted at this issue contains a cycle.
///
/// The shell shelled out to `detect-dependency-cycle.sh` and read exit code 1.
/// In-process now, but the answer is the same one: budget truncation and
/// unreadable nodes yield "no cycle found", never a cycle.
fn cycle_present(cwd: &Path, repo: &str, issue: i64, use_cache: bool) -> bool {
    let root = format!("{repo}#{issue}");
    let mut walk = cycle::Walk::new(
        |node: &str| forge::fetch_node(node, cwd, use_cache),
        cycle::Budgets::default(),
    );
    matches!(walk.run(&root), cycle::Outcome::Cycle(_))
}

fn unescalate_mode(
    cwd: &Path,
    opts: &ClassifyOpts,
    repo: &str,
    self_node: &str,
    view: &forge::IssueView,
    findings_override: Option<String>,
) -> Result<i32, i32> {
    let markers = unescalate::Markers {
        operator_only_label: consts::OPERATOR_ONLY_LABEL,
        cycle_prefix: consts::CYCLE_MARKER_PREFIX,
        unescalate_prefix: consts::UNESCALATE_MARKER_PREFIX,
    };
    let mut inputs = unescalate::Inputs {
        body: view.body.clone(),
        labels: view.label_names(),
        comments: view.comments_joined(),
        escalation: findings_override
            .unwrap_or_else(|| view.last_comment_containing(consts::ESCALATE_MARKER)),
        ..Default::default()
    };

    let blockers = unescalate::blockers_to_classify(&inputs, repo, self_node, &markers);
    if !blockers.is_empty() {
        inputs.refs = state::classify_refs(&blockers.join("\n"), cwd, !opts.no_cache);
    }

    let decision = unescalate::decide(&inputs, repo, self_node, &markers);
    let (out, code) = unescalate::render(&decision, &inputs.refs.unknown);
    print!("{out}");

    if !opts.apply {
        return Ok(code);
    }

    let body = match &decision {
        unescalate::Decision::Cleared {
            cleared,
            blocker_fingerprint,
        } => apply::cleared_body(
            consts::OPERATOR_ONLY_LABEL,
            consts::OPERATOR_BLOCKED_LABEL,
            &cleared.join(" "),
            &format!("{}{blocker_fingerprint} -->", consts::UNESCALATE_MARKER_PREFIX),
        ),
        unescalate::Decision::Subset {
            still_open,
            blocker_fingerprint,
            subset,
        } => apply::subset_body(
            consts::OPERATOR_ONLY_LABEL,
            consts::OPERATOR_BLOCKED_LABEL,
            &still_open.join(" "),
            subset,
            &format!("{}{blocker_fingerprint} -->", consts::UNESCALATE_MARKER_PREFIX),
        ),
        // Nothing to apply: the verdict was a refusal, and its exit code stands.
        unescalate::Decision::NoUnescalate { .. } => return Ok(code),
    };

    let mut writer = forge::GhWriter {
        issue: opts.issue,
        repo,
        repo_root: cwd,
    };
    let labels = apply::Labels {
        operator_only: consts::OPERATOR_ONLY_LABEL,
        operator_blocked: consts::OPERATOR_BLOCKED_LABEL,
    };
    // The warning wording is the shell's, verbatim. It is what an operator
    // greps a sweep log for, and it names which half landed — which is the
    // whole point of the write ordering above.
    match apply::apply_unescalation(&mut writer, &labels, &body) {
        Ok(()) => {
            println!("UNESCALATED: {self_node}");
            Ok(0)
        }
        // The comment is the audit trail; the label removal is the state change.
        // Losing the former is not a failed un-escalation, and reporting it as
        // one would misstate the issue's real state.
        Err(apply::ApplyError::CommentPost(_)) => {
            warn(&format!(
                "removed {} from {self_node} but could not post the un-escalation comment (audit trail missing)",
                consts::OPERATOR_ONLY_LABEL
            ));
            println!("UNESCALATED: {self_node}");
            Ok(0)
        }
        Err(_) => {
            warn(&format!(
                "could not remove {} from {self_node} (no comment posted; a later pass will retry)",
                consts::OPERATOR_ONLY_LABEL
            ));
            println!("REASON: apply-failed");
            Ok(1)
        }
    }
}

fn fact_mode(
    cwd: &Path,
    opts: &ClassifyOpts,
    repo: &str,
    self_node: &str,
    view: &forge::IssueView,
) -> Result<i32, i32> {
    // Read but do NOT fail on a missing file: "no resolutions file" is a
    // verdict (`missing-resolutions-file`, exit 1), not a usage error. The
    // shell tested `-n && -f` together for the same reason.
    let resolutions = opts
        .resolutions_file
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok());

    let inputs = fact::Inputs {
        labels: view.label_names(),
        comments: view.comments_joined(),
        escalation: view.last_comment_containing(consts::ESCALATE_MARKER),
        resolutions: resolutions.clone(),
        commit_sha: opts.commit.clone(),
    };
    let markers = fact::Markers {
        operator_only_label: consts::OPERATOR_ONLY_LABEL,
        cycle_prefix: consts::CYCLE_MARKER_PREFIX,
        fact_unescalate_prefix: consts::FACT_UNESCALATE_MARKER_PREFIX,
    };

    let decision = fact::decide(&inputs, &markers);
    let (out, code) = fact::render(&decision);
    print!("{out}");

    let fact::Decision::FactUnescalate {
        verified_commit,
        fingerprint,
        ..
    } = &decision
    else {
        return Ok(code);
    };
    if !opts.apply {
        return Ok(code);
    }

    let summary = apply::resolutions_summary(resolutions.as_deref().unwrap_or_default());
    let revision_marker = format!("{}{fingerprint} -->", consts::FACT_REVISION_MARKER_PREFIX);
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let revised =
        apply::fact_revised_body(&view.body, &today, verified_commit, &summary, &revision_marker);
    let comment = apply::fact_comment_body(
        consts::OPERATOR_ONLY_LABEL,
        consts::OPERATOR_DECISION_LABEL,
        verified_commit,
        &summary,
        &format!("{}{fingerprint} -->", consts::FACT_UNESCALATE_MARKER_PREFIX),
    );

    let mut writer = forge::GhWriter {
        issue: opts.issue,
        repo,
        repo_root: cwd,
    };
    let labels = apply::FactLabels {
        operator_only: consts::OPERATOR_ONLY_LABEL,
        operator_decision: consts::OPERATOR_DECISION_LABEL,
    };
    match apply::apply_fact_unescalation(
        &mut writer,
        &labels,
        &view.body,
        &revised,
        &revision_marker,
        &comment,
    ) {
        Ok(()) => {
            println!("UNESCALATED: {self_node}");
            Ok(0)
        }
        Err(apply::ApplyError::CommentPost(_)) => {
            warn(&format!(
                "de-escalated {self_node} but could not post the confirming comment (audit trail missing)"
            ));
            println!("UNESCALATED: {self_node}");
            Ok(0)
        }
        // Two distinct retryable failures, and the message says which: the body
        // edit leaves nothing changed at all, the label removal leaves a revised
        // body behind (which the revision marker makes safe to re-encounter).
        Err(apply::ApplyError::BodyEdit(_)) => {
            warn(&format!(
                "could not append the ## Revision section to {self_node} (no label change, no comment; a later pass will retry)"
            ));
            println!("REASON: apply-failed");
            Ok(1)
        }
        Err(apply::ApplyError::LabelRemoval(_)) => {
            warn(&format!(
                "could not remove {} from {self_node} (no comment posted; a later pass will retry)",
                consts::OPERATOR_ONLY_LABEL
            ));
            println!("REASON: apply-failed");
            Ok(1)
        }
    }
}

// ===========================================================================
// detect-dependency-cycle
// ===========================================================================

/// Run `detect-dependency-cycle`, returning its exit code.
///
/// **1 means "cycle found"**, not "error" — the shell used it as data, and
/// `classify --check-defer` read exactly that. 2 is the error code.
#[must_use]
pub fn run_cycle(cwd: &Path, opts: &CycleOpts) -> i32 {
    match detect_cycle(cwd, opts) {
        Ok(code) | Err(code) => code,
    }
}

fn detect_cycle(cwd: &Path, opts: &CycleOpts) -> Result<i32, i32> {
    let cwd = &forge_dir(cwd);
    check_issue(opts.issue)?;
    let repo = repo_or_exit(opts.repo.as_ref(), cwd)?;
    let use_cache = !opts.no_cache;
    let root = format!("{repo}#{}", opts.issue);

    let d = cycle::Budgets::default();
    let budgets = cycle::Budgets {
        max_depth: opts.max_depth.unwrap_or(d.max_depth),
        max_nodes: opts.max_nodes.unwrap_or(d.max_nodes),
        max_steps: opts.max_steps.unwrap_or(d.max_steps),
    };

    // The root is fetched separately so an unreadable root is a hard error
    // (exit 2) rather than a quiet "no cycle" — there is no graph to walk.
    if forge::fetch_node(&root, cwd, use_cache).is_none() {
        err(&format!("could not read {root}"));
        return Err(2);
    }

    let mut walk = cycle::Walk::new(|node: &str| forge::fetch_node(node, cwd, use_cache), budgets);
    let outcome = walk.run(&root);
    let unreadable = walk.unreadable().to_vec();

    match outcome {
        cycle::Outcome::Cycle(path) => {
            let pretty = path.join(" -> ");
            // Fingerprint the node SET, sorted and deduplicated, so one cycle
            // has one identity whichever member it was discovered from — and a
            // genuine membership change yields a new one.
            let mut nodes: Vec<String> = path.clone();
            nodes.sort();
            nodes.dedup();
            let nodes = nodes.join(" ");
            let fp = super::fingerprint::fingerprint(&nodes);

            println!("CYCLE_DETECTED");
            println!("CYCLE_PATH: {pretty}");
            println!("CYCLE_NODES: {nodes}");
            println!("CYCLE_FINGERPRINT: {fp}");
            if !unreadable.is_empty() {
                println!("UNREADABLE: {}", unreadable.join(" "));
            }
            if opts.report {
                report_cycle(cwd, &root, &pretty, &nodes, &fp);
            }
            Ok(1)
        }
        cycle::Outcome::NoCycle => {
            println!("NO_CYCLE");
            println!("SCANNED: {}", walk.fetch_count());
            let truncations = walk.truncations();
            if !truncations.is_empty() {
                let joined: Vec<&str> = truncations.iter().map(|t| t.as_str()).collect();
                println!("SEARCH_TRUNCATED: {}", joined.join(" "));
            }
            if !unreadable.is_empty() {
                println!("UNREADABLE: {}", unreadable.join(" "));
            }
            Ok(0)
        }
    }
}

/// Post the cycle report and park the issue for an operator.
///
/// Idempotent on the fingerprint marker: a cycle re-found on a later pass must
/// not re-comment. Unlike an un-escalation, the comment goes **first** here —
/// the label is what an operator acts on, and a label with no explanation is
/// the worse half to be left holding.
fn report_cycle(cwd: &Path, root: &str, pretty: &str, nodes: &str, fingerprint: &str) {
    let Some((repo, num)) = root.rsplit_once('#') else {
        return;
    };
    let marker = format!("{}{fingerprint} -->", consts::CYCLE_MARKER_PREFIX);

    // Uncached: a stale hit here would re-post a report that already exists.
    if let Some(view) = forge::read_issue_in(num, repo, cwd, false, "comments") {
        if view.comments_joined().contains(&marker) {
            println!("ALREADY_REPORTED: {root}");
            return;
        }
    }

    let body = cycle_report_body(pretty, nodes, &marker);
    let mut writer = forge::GhWriter {
        issue: num.parse().unwrap_or(0),
        repo,
        repo_root: cwd,
    };
    use apply::Writer as _;
    if !writer.post_comment(&body).succeeded() {
        warn(&format!("could not comment on {root}"));
        return;
    }
    if !writer
        .add_labels("loom:operator-only,loom:operator-decision")
        .succeeded()
    {
        warn(&format!("could not add loom:operator-only to {root}"));
    }
    println!("REPORTED: {root}");
}

/// The cycle report comment. Reproduced from the shell verbatim: it is posted
/// on live issues, and an operator reads it to decide which declared edge is
/// the wrong one.
#[must_use]
pub fn cycle_report_body(pretty: &str, nodes: &str, marker: &str) -> String {
    format!(
        "**Dependency cycle detected** - this issue cannot be unblocked by waiting.\n\nThe declared `Blocked by` / `Depends on` / `Requires` references form a closed loop:\n\n```\n{pretty}\n```\n\nEvery issue on that path is OPEN, and the progress of each node is declared to depend\non the next, so the loop can never resolve itself: each side re-derives \"still\nblocked\" on every pass, indefinitely. This is the deadlock shape that previously had\nto be found by hand.\n\nBreaking a cycle is a human decision - which declared dependency edge is wrong, or\nwhich side ships a partial increment first - so Champion is routing this to\n`loom:operator-only` (with the `loom:operator-decision` sub-kind: this is a\njudgement call, not a self-clearing wait) instead of re-deriving the same\nconclusion forever.\n\n**Cycle members** (machine-readable, #5671): Blocked by {nodes}\n\n---\n*Automated by Champion role (detect-dependency-cycle.sh)*\n{marker}\n"
    )
}

// ===========================================================================
// detect-startable-subset
// ===========================================================================

/// Run `detect-startable-subset`, returning its exit code.
///
/// **1 means "no subset declared"**, not "error" — data, like the cycle code.
#[must_use]
pub fn run_subset(cwd: &Path, opts: &SubsetOpts) -> i32 {
    match detect_subset(cwd, opts) {
        Ok(code) | Err(code) => code,
    }
}

fn detect_subset(cwd: &Path, opts: &SubsetOpts) -> Result<i32, i32> {
    let cwd = &forge_dir(cwd);
    check_issue(opts.issue)?;

    let body = match opts.body_file.as_deref() {
        Some(p) => read_file(p, "--body-file")?,
        None => {
            let repo = repo_or_exit(opts.repo.as_ref(), cwd)?;
            let Some(view) = forge::read_issue(opts.issue, &repo, cwd, !opts.no_cache, "body")
            else {
                err(&format!("could not read {repo}#{}", opts.issue));
                return Err(2);
            };
            view.body
        }
    };

    if subset::has_startable_subset(&body) {
        println!("STARTABLE_SUBSET");
        let extracted = subset::extract_startable_subset(&body);
        print!("{extracted}");
        if !extracted.ends_with('\n') {
            println!();
        }
        return Ok(0);
    }
    println!("NO_STARTABLE_SUBSET");
    Ok(1)
}

#[cfg(test)]
mod tests;
