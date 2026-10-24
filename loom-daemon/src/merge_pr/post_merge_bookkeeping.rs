//! `merge-pr.sh`'s post-merge issue bookkeeping, as one orchestrator (#8191,
//! slice 2 of `docs/migration/merge-pr-orchestrator-port.md`).
//!
//! Three passes ran after a confirmed merge, each a shell loop around Rust
//! predicates that were already ported (`partial-reset`, `partial-comment`,
//! `closed-building`, `merge-pr-refs`): `_reset_partial_increment_labels`
//! (#3667/#4569), `_close_branch_derived_issue` (#12615) and
//! `_strip_closed_issue_building_labels` (#6199/#11517). Every loop was also a
//! place a fault could read as an answer — a `$(...)` capture, a first-token
//! compare, a hand-written "the verb did not run" warning. Here the predicates
//! are called in-process, in the order the shell called them, and the shell
//! keeps only a wrapper that replays the output.
//!
//! # One step per invocation
//!
//! `--step partial-reset | branch-close | strip` selects ONE pass. The shell
//! still calls them in the retired order (reset, branch close, strip), because
//! the order is load-bearing: a #4569 reopen must precede the strip pass reading
//! the issue, and the branch close must precede the strip so the strip sees the
//! issue it just closed. Splitting by step also keeps the `LOOM-NEED` protocol
//! replay-safe (see below), since a pass's reads never follow its own writes.
//!
//! # What stays in the shell, and why
//!
//! * **Close targets** (`forge_pr_close_targets`) serve Gitea as well as GitHub
//!   and are shared with other callers, so they are asked for with one
//!   `LOOM-NEED close-targets` line (exit [`NEED_EXIT`], nothing done), the
//!   same protocol as `post-merge-cleanup`. The fact is PR-level, never
//!   affected by this pass's writes, and is demanded before the first write.
//! * **The forge writes** — reopen, label swap, label removal and comment — go
//!   through `forge_gh_{reopen_issue,swap_label,remove_label,comment}_rl_safe`
//!   in `lib/forge-helpers.sh`. Those carry the write-scope vet (#9548), the
//!   permission-window ladder (#6074) and the GraphQL-exhaustion REST fallback
//!   (#4856). They are CALLED, not re-implemented: [`SystemHost::helper`]
//!   sources the library into a child `bash` and runs the one function. A
//!   second copy of that ladder in Rust would be a second place for it to
//!   drift; moving it is a separate slice with its own differential.
//!
//! # Output
//!
//! `LEVEL<TAB>message` per line (`INFO`, `WARNING`, `SUCCESS`), replayed
//! through the script's logging, ended by [`DONE`]. Exit 0 without the
//! terminator is a stale or muted binary, never "ran and had nothing to say".
//!
//! # Fail direction
//!
//! Every pass is best-effort and never fails the merge, exactly as before. A
//! read that fails yields the same answer it always did (`{}` for an issue, no
//! commit messages), and a pass that cannot establish its facts changes
//! nothing.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use regex::bytes::Regex;
use serde_json::Value;

use super::closed_building::{self, Plan};
use super::partial_comment::{
    now_timestamp, partial_merged_comment, partial_merged_held_comment, premature_close_comment,
};
use super::partial_reset::{self, hold_from_json, IssueView, PreMerge, Step};
use super::refs::partial_increment_refs;
use crate::claim_reconciliation::gh_call;
use crate::gh_invocation::{AccessIntent, GhInvocation};

/// Last stdout line of a finished run.
pub const DONE: &str = "LOOM-POST-MERGE-BOOKKEEPING DONE";

/// Exit status meaning "I need a fact; see the `LOOM-NEED` line".
pub const NEED_EXIT: i32 = 3;

/// Which retired shell pass to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Pass {
    /// `_reset_partial_increment_labels`.
    PartialReset,
    /// `_close_branch_derived_issue`.
    BranchClose,
    /// `_strip_closed_issue_building_labels`.
    Strip,
}

/// One replayable output record: `(LEVEL, message)`.
pub type Record = (String, String);

/// What the pass knows without asking the forge.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub repo: String,
    pub pr_number: String,
    pub pr_branch: String,
    /// The merged PR's body (`.body // ""`).
    pub pr_body: String,
    /// `$PARTIAL_CONFLICT_ISSUES`, space separated.
    pub conflicted: String,
    /// `$PARTIAL_OPEN_BEFORE_MERGE`, space separated.
    pub open_before_merge: String,
}

/// Facts the shell supplies once asked.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// `forge_pr_close_targets`' output; `Some("")` is an answer.
    pub close_targets: Option<String>,
}

/// The only fact a pass can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    CloseTargets,
}

impl Need {
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Need::CloseTargets => "close-targets",
        }
    }
}

/// How a pass ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Need(Need),
    Done(Vec<Record>),
}

/// Everything the pass does to the outside world.
pub trait Host {
    /// `gh api repos/<nwo>/issues/<n> || echo '{}'`, uncached, as the shell
    /// captured it (trailing newlines stripped).
    fn read_issue(&self, repo: &str, issue: &str) -> String;
    /// Every commit message of the PR, concatenated (`_pr_commit_messages`);
    /// empty on any failure.
    fn commit_messages(&self, repo: &str, pr: &str) -> String;
    /// `pr` / `open` / `closed` / empty for the branch-derived close's state read.
    fn issue_kind(&self, repo: &str, issue: &str) -> String;
    /// Run one `lib/forge-helpers.sh` function; true on exit 0.
    fn helper(&self, func: &str, args: &[&str]) -> bool;
    /// `gh issue close <n> --repo <nwo> --reason completed --comment <body>`.
    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool;
    /// The sign-off timestamp the audit comments carry.
    fn timestamp(&self) -> String;
}

fn rec(level: &str, text: impl Into<String>) -> Record {
    (level.to_string(), text.into())
}

fn member(set: &str, n: &str) -> bool {
    set.split_whitespace().any(|x| x == n)
}

/// Run one pass.
pub fn run(pass: Pass, inputs: &Inputs, facts: &Facts, host: &dyn Host) -> Outcome {
    match pass {
        Pass::PartialReset => Outcome::Done(partial_reset_pass(inputs, host)),
        Pass::BranchClose => branch_close_pass(inputs, facts, host),
        Pass::Strip => strip_pass(inputs, facts, host),
    }
}

// ---------------------------------------------------------------------------
// _reset_partial_increment_labels / _reset_one_partial_issue
// ---------------------------------------------------------------------------

fn partial_reset_pass(inputs: &Inputs, host: &dyn Host) -> Vec<Record> {
    let mut out = Vec::new();
    if inputs.pr_body.is_empty() {
        return out;
    }
    for n in partial_increment_refs(&inputs.pr_body) {
        reset_one(&n.to_string(), inputs, host, &mut out);
    }
    out
}

fn post_comment(
    host: &dyn Host,
    repo: &str,
    issue: &str,
    body: &str,
    post_warn: &str,
    out: &mut Vec<Record>,
) {
    if !host.helper("forge_gh_comment_rl_safe", &[repo, issue, body]) {
        out.push(rec("WARNING", post_warn));
    }
}

fn reset_one(issue: &str, inputs: &Inputs, host: &dyn Host, out: &mut Vec<Record>) {
    let (repo, pr) = (inputs.repo.as_str(), inputs.pr_number.as_str());
    let issue_json = host.read_issue(repo, issue);
    let view = IssueView::from_json(&issue_json);
    let pre = PreMerge {
        conflicted: member(&inputs.conflicted, issue),
        open_before_merge: member(&inputs.open_before_merge, issue),
    };
    let at = host.timestamp();
    let mut reopened = false;
    for step in partial_reset::plan(issue, pr, repo, &view, pre) {
        match step {
            Step::Info(m) => out.extend(m.split('\n').map(|l| rec("INFO", l))),
            Step::Warning(m) => out.extend(m.split('\n').map(|l| rec("WARNING", l))),
            Step::Reopen => {
                if host.helper("forge_gh_reopen_issue_rl_safe", &[repo, issue]) {
                    out.push(rec(
                        "SUCCESS",
                        format!("Issue #{issue} reopened (premature auto-close reverted)"),
                    ));
                    reopened = true;
                    post_comment(
                        host,
                        repo,
                        issue,
                        &premature_close_comment(issue, pr, &at),
                        &format!(
                            "Could not post premature-close comment on issue #{issue} (reopen still applied)"
                        ),
                        out,
                    );
                } else {
                    out.push(rec(
                        "WARNING",
                        format!(
                            "Could not reopen issue #{issue} after its premature auto-close — reopen manually: gh issue reopen {issue} --repo {repo}"
                        ),
                    ));
                    return;
                }
            }
            Step::Swap(held) => {
                let add = held.clone().unwrap_or_else(|| "loom:issue".to_string());
                if host.helper("forge_gh_swap_label_rl_safe", &[repo, issue, "loom:building", &add])
                {
                    out.push(rec(
                        "SUCCESS",
                        format!(
                            "Issue #{issue}: loom:building -> {add} (partial increment; issue remains open)"
                        ),
                    ));
                    let body = if held.is_some() {
                        let reason = hold_from_json(&issue_json).unwrap_or_default();
                        partial_merged_held_comment(pr, reopened, &reason, &at)
                    } else {
                        partial_merged_comment(pr, reopened, &at)
                    };
                    post_comment(
                        host,
                        repo,
                        issue,
                        &body,
                        &format!(
                            "Could not post partial-increment comment on issue #{issue} (label swap still applied)"
                        ),
                        out,
                    );
                } else {
                    out.push(rec(
                        "WARNING",
                        format!(
                            "Could not reset labels on issue #{issue} (partial increment) — may need manual 'gh issue edit'"
                        ),
                    ));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// _close_branch_derived_issue (#12615)
// ---------------------------------------------------------------------------

/// `feature/issue-<N>` exactly (the shell's `^feature/issue-([0-9]+)$`).
fn branch_issue(branch: &str) -> Option<&str> {
    let n = branch.strip_prefix("feature/issue-")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(n)
}

/// The retired `grep -Eqi "\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b[[:space:]]+#N\b"`
/// over the body and the commit messages. grep is line-oriented, so the
/// whitespace between keyword and `#N` never spans a newline; ASCII-only
/// boundaries and case folding are what `grep -E` gave under `LC_ALL=C`; a
/// bytes regex is what lets `(?-u)` match non-UTF-8-safe classes at all.
fn has_closing_keyword(text: &str, issue: &str) -> bool {
    let pat = format!(
        r"(?i-u)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b[^\S\n]+#{}\b",
        regex::escape(issue)
    );
    Regex::new(&pat).is_ok_and(|re| re.is_match(text.as_bytes()))
}

fn branch_close_pass(inputs: &Inputs, facts: &Facts, host: &dyn Host) -> Outcome {
    let Some(issue) = branch_issue(&inputs.pr_branch) else {
        return Outcome::Done(Vec::new());
    };
    let done = |recs: Vec<Record>| Outcome::Done(recs);
    // A partial-increment declaration for this issue: never close it.
    if partial_increment_refs(&inputs.pr_body)
        .iter()
        .any(|n| n.to_string() == issue)
    {
        return done(Vec::new());
    }
    let Some(targets) = facts.close_targets.as_deref() else {
        return Outcome::Need(Need::CloseTargets);
    };
    if targets.lines().any(|l| l == issue) {
        return done(Vec::new());
    }
    let msgs = host.commit_messages(&inputs.repo, &inputs.pr_number);
    if has_closing_keyword(&format!("{}\n{}\n", inputs.pr_body, msgs), issue) {
        return done(Vec::new());
    }
    if host.issue_kind(&inputs.repo, issue) != "open" {
        return done(Vec::new());
    }
    let (pr, branch, repo) = (&inputs.pr_number, &inputs.pr_branch, &inputs.repo);
    let comment = format!(
        "Closing: PR #{pr} merged from `{branch}` without a closing keyword or a `Part of #{issue}` partial-increment reference, so this issue's work is delivered (#12615). If more work remains, reopen it and have the follow-up PR declare `Part of #{issue}`."
    );
    let rec_ = if host.close_issue(repo, issue, &comment) {
        rec(
            "SUCCESS",
            format!(
                "Issue #{issue}: closed (merged from {branch} with no closing keyword, #12615)"
            ),
        )
    } else {
        rec(
            "WARNING",
            format!(
                "Could not close issue #{issue} after merging {branch} -- may need manual: gh issue close {issue} --repo {repo}"
            ),
        )
    };
    done(vec![rec_])
}

// ---------------------------------------------------------------------------
// _strip_closed_issue_building_labels (#6199, #11517)
// ---------------------------------------------------------------------------

fn strip_pass(inputs: &Inputs, facts: &Facts, host: &dyn Host) -> Outcome {
    let Some(targets) = facts.close_targets.as_deref() else {
        return Outcome::Need(Need::CloseTargets);
    };
    let mut out = Vec::new();
    for issue in targets.lines().filter(|l| !l.is_empty()) {
        let view = IssueView::from_json(&host.read_issue(&inputs.repo, issue));
        let labels: &[&str] = match closed_building::plan(&view) {
            Plan::Strip => &["loom:building"],
            Plan::StripReady => &["loom:issue"],
            Plan::StripBoth => &["loom:building", "loom:issue"],
            Plan::Skip(_) => continue,
        };
        for label in labels {
            if host.helper("forge_gh_remove_label_rl_safe", &[&inputs.repo, issue, label]) {
                out.push(rec(
                    "SUCCESS",
                    format!(
                        "Issue #{issue}: removed stale {label} label (closed by this merge, #6199/#11517)"
                    ),
                ));
            } else {
                out.push(rec(
                    "WARNING",
                    format!(
                        "Could not remove {label} from closed issue #{issue} — may need manual: gh issue edit {issue} --repo {} --remove-label {label}",
                        inputs.repo
                    ),
                ));
            }
        }
    }
    Outcome::Done(out)
}

// ---------------------------------------------------------------------------
// The real host
// ---------------------------------------------------------------------------

/// The live forge and the shell helper library.
pub struct SystemHost {
    /// Path to `lib/forge-helpers.sh`.
    pub forge_helpers: String,
}

/// Run `gh <args…>` through the facade (#10089), booked under `op`.
///
/// The working directory is not a checkout of `repo`, so a read names it
/// (`reader_repo`) or the facade cannot route it to a reader and sheds it.
pub(crate) fn gh_inv(
    op: &'static str,
    intent: AccessIntent,
    repo: &str,
    args: &[&str],
) -> GhInvocation {
    let gh = crate::gh_invocation::gh_bin();
    let inv = gh_call::inv(op, intent, Path::new(&gh), Path::new(".")).args(args);
    if intent == AccessIntent::Read {
        inv.reader_repo(repo)
    } else {
        inv
    }
}

pub(crate) fn gh_stdout(op: &'static str, repo: &str, args: &[&str]) -> Option<String> {
    gh_call::ok_stdout(gh_inv(op, AccessIntent::Read, repo, args))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

impl Host for SystemHost {
    fn read_issue(&self, repo: &str, issue: &str) -> String {
        let path = format!("repos/{repo}/issues/{issue}");
        let o = gh_call::output(gh_inv(
            "post_merge.issue_read",
            AccessIntent::Read,
            repo,
            &["api", &path],
        ));
        // `$(gh api … 2>/dev/null || echo '{}')`: on failure the (possibly
        // error-body) stdout is followed by `{}`.
        let mut s = match &o {
            Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
            Err(_) => String::new(),
        };
        if !matches!(&o, Ok(o) if o.status.success()) {
            s.push_str("{}\n");
        }
        s.trim_end_matches('\n').to_string()
    }

    fn commit_messages(&self, repo: &str, pr: &str) -> String {
        let path = format!("repos/{repo}/pulls/{pr}/commits");
        let Some(raw) = gh_stdout("post_merge.commits", repo, &["api", &path, "--paginate"]) else {
            return String::new();
        };
        // `--paginate` concatenates one JSON array per page.
        let mut msgs = Vec::new();
        for doc in serde_json::Deserializer::from_str(&raw).into_iter::<Value>() {
            let Ok(Value::Array(items)) = doc else { break };
            for c in items {
                if let Some(m) = c.pointer("/commit/message").and_then(Value::as_str) {
                    msgs.push(m.to_string());
                }
            }
        }
        msgs.join("\n")
    }

    fn issue_kind(&self, repo: &str, issue: &str) -> String {
        let path = format!("repos/{repo}/issues/{issue}");
        let Some(raw) = gh_stdout("post_merge.issue_kind", repo, &["api", &path]) else {
            return String::new();
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.get("pull_request").is_some() => "pr".to_string(),
            Ok(v) => v
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Err(_) => String::new(),
        }
    }

    fn helper(&self, func: &str, args: &[&str]) -> bool {
        Command::new("bash")
            .arg("-c")
            .arg(r#"source "$1" || exit 127; shift; "$@""#)
            .arg("_")
            .arg(&self.forge_helpers)
            .arg(func)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool {
        gh_call::output(gh_inv(
            "post_merge.issue_close",
            AccessIntent::Write,
            repo,
            &[
                "issue",
                "close",
                issue,
                "--repo",
                repo,
                "--reason",
                "completed",
                "--comment",
                comment,
            ],
        ))
        .is_ok_and(|o| o.status.success())
    }

    fn timestamp(&self) -> String {
        now_timestamp()
    }
}

/// Print a finished run in the wrapper's line protocol.
pub fn print_records(records: &[Record]) {
    print_records_with(records, DONE);
}

/// [`print_records`] with an explicit terminator (shared by sibling verbs).
pub fn print_records_with(records: &[Record], done: &str) {
    let mut out = std::io::stdout().lock();
    for (level, text) in records {
        for line in text.split('\n') {
            let _ = writeln!(out, "{level}\t{line}");
        }
    }
    let _ = writeln!(out, "{done}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests;
