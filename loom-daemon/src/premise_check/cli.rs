//! `loom-daemon premise-check` — the gate's entry point.
//!
//! Stdout is `KEY=VALUE` lines plus zero or more `EVIDENCE-CANDIDATE=` lines,
//! so a role prompt can read it with `grep`/`sed` or `eval` the scalar half.
//! The exit code is the contract callers branch on; see
//! [`super::exit`](crate::premise_check::exit).
//!
//! Two input modes, the same split `classify-ac-verification.sh` uses:
//!
//! - **Forge mode** (`--issue N`) — what Curator and the sweep orchestrator
//!   run.
//! - **Hermetic mode** (`--body-file`) — no forge calls at all, which is what
//!   the tests drive and what makes a #7855 reconstruction reproducible.

use super::{decide, evidence, exit, record, Inputs, Trigger};
use crate::premise_check::record::{Exists, Outcome, Verdict};
use std::path::{Path, PathBuf};

/// Everything the subcommand accepts, already parsed by clap in
/// `cli/premise_check.rs`.
#[derive(Debug, Default)]
pub struct Options {
    pub issue: Option<i64>,
    pub repo: Option<String>,
    pub body_file: Option<PathBuf>,
    pub title: Option<String>,
    pub labels: Option<String>,
    pub record_file: Option<PathBuf>,
    pub repo_root: Option<PathBuf>,
    pub no_scan: bool,
    pub scan_limit: usize,
    pub no_cache: bool,
}

fn err(msg: &str) {
    eprintln!("ERROR: {msg}");
}

/// Never returns.
pub fn run(opts: &Options) -> ! {
    match evaluate(opts) {
        Ok(code) => std::process::exit(code),
        Err(code) => std::process::exit(code),
    }
}

fn evaluate(opts: &Options) -> Result<i32, i32> {
    // TWO roots, deliberately (issue #8499). `repo_root` is the shared clone:
    // the `gh` read cache and the origin remote live there, one per checkout,
    // so a linked worktree's `.git` pointer is followed back to the main one.
    let repo_root = opts
        .repo_root
        .clone()
        .or_else(crate::repo_root::find_repo_root_from_cwd)
        .unwrap_or_else(|| PathBuf::from("."));
    // `content_root` is the working tree the caller is reasoning IN. A
    // citation is a claim about file content, and the primary clone sits on
    // `main` while N worktrees run ahead of it — resolving citations there
    // failed a correct record with RECORD-MALFORMED (exit 12) purely because
    // the shared checkout had not pulled the cited file yet, and would
    // equally pass a citation that resolves only in the stale tree. The
    // evidence scan reads the same content and follows the same root.
    //
    // An explicit `--repo-root` still sets both: a caller naming a tree is
    // naming the one tree it wants everything resolved against.
    let content_root = opts
        .repo_root
        .clone()
        .or_else(crate::repo_root::find_worktree_root_from_cwd)
        .unwrap_or_else(|| repo_root.clone());

    let inputs = match (&opts.body_file, opts.issue) {
        (Some(path), _) => hermetic_inputs(opts, path)?,
        (None, Some(n)) => forge_inputs(opts, n, &repo_root)?,
        (None, None) => {
            err("usage: premise-check --issue <N> [--repo <owner/repo>] | --body-file <path>");
            return Err(exit::ERROR);
        }
    };

    let decision = decide(&inputs, &content_root);

    println!(
        "SCOPE={}",
        if decision.trigger.is_some() {
            "in"
        } else {
            "out"
        }
    );
    println!(
        "TRIGGER={}",
        decision
            .trigger
            .as_ref()
            .map_or_else(|| "none".to_string(), Trigger::to_string)
    );
    println!(
        "RECORD={}",
        match (&decision.record, &decision.outcome) {
            (Some(_), _) => "present",
            (None, Outcome::Malformed(_)) => "malformed",
            (None, Outcome::OutOfScope) => "not-checked",
            (None, _) => "absent",
        }
    );
    println!("VERDICT={}", decision.outcome);
    if let Outcome::Malformed(why) = &decision.outcome {
        println!("REASON={why}");
    }
    if let Some(rec) = &decision.record {
        println!(
            "SUMMARY=exists={} deliberate={} reversal={} verdict={}",
            match rec.exists {
                Exists::Yes => "yes",
                Exists::No => "no",
                Exists::Unclear => "unclear",
            },
            yesno(rec.deliberate),
            yesno(rec.reversal),
            match rec.verdict {
                Verdict::Clear => "clear",
                Verdict::OperatorDecision => "operator-decision",
            }
        );
    }

    // The scan is advisory and costs two `git grep` passes, so it runs only
    // where it can change what a human does next: when the gate is closed.
    if !opts.no_scan && decision.exit_code != exit::PROCEED {
        emit_candidates(opts, &inputs, &content_root, decision.record.as_ref());
    }

    Ok(decision.exit_code)
}

fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// `content_root`, not the clone root: the scan reads the same file content the
/// citations resolve against, so the two must agree or a suggested
/// `EVIDENCE-CANDIDATE=` would name a path the record could not then cite
/// (issue #8499). `evaluate` holds both roots in scope, so this parameter is
/// named for the one it must be given.
fn emit_candidates(
    opts: &Options,
    inputs: &Inputs,
    content_root: &Path,
    rec: Option<&record::Record>,
) {
    let anchors = evidence::anchors(&inputs.title, &inputs.body, content_root);
    let candidates = evidence::scan(content_root, &anchors, opts.scan_limit);
    let cited = rec.map(record::cited_paths).unwrap_or_default();
    for c in &candidates {
        // Tab-separated, one line each: path:line, whether the record already
        // names that path, the anchors that surfaced it, then the text.
        println!(
            "EVIDENCE-CANDIDATE={}:{}\t{}\t{}\t{}",
            c.path,
            c.line,
            if cited.contains(&c.path) {
                "cited"
            } else {
                "undisposed"
            },
            c.anchors.join(","),
            c.snippet
        );
    }
    if candidates.is_empty() {
        println!("EVIDENCE-CANDIDATES=0");
    }
}

fn hermetic_inputs(opts: &Options, body_file: &Path) -> Result<Inputs, i32> {
    let body = std::fs::read_to_string(body_file).map_err(|e| {
        err(&format!("--body-file {}: {e}", body_file.display()));
        exit::ERROR
    })?;
    let mut comments = Vec::new();
    if let Some(rf) = &opts.record_file {
        comments.push(std::fs::read_to_string(rf).map_err(|e| {
            err(&format!("--record-file {}: {e}", rf.display()));
            exit::ERROR
        })?);
    }
    Ok(Inputs {
        title: opts.title.clone().unwrap_or_default(),
        body,
        labels: opts
            .labels
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        comments,
    })
}

fn forge_inputs(opts: &Options, issue: i64, repo_root: &Path) -> Result<Inputs, i32> {
    if issue <= 0 {
        err(&format!("--issue must be a positive number (got: {issue})"));
        return Err(exit::ERROR);
    }
    let repo = match opts.repo.clone().filter(|r| !r.is_empty()) {
        Some(r) => r,
        None => resolve_repo(repo_root).ok_or_else(|| {
            err("could not determine the repo - pass --repo <owner/repo>");
            exit::ERROR
        })?,
    };

    #[derive(serde::Deserialize)]
    struct View {
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        labels: Vec<Label>,
        #[serde(default)]
        comments: Vec<Comment>,
    }
    #[derive(serde::Deserialize)]
    struct Label {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct Comment {
        #[serde(default)]
        body: String,
    }

    let q: crate::cmd_out::Query<View> = crate::script_helpers::gh_query(
        &[
            "issue",
            "view",
            &issue.to_string(),
            "--repo",
            &repo,
            "--json",
            "title,body,labels,comments",
        ],
        repo_root,
        !opts.no_cache,
        // An issue with an empty body is still a real issue; only an
        // unreadable one is absent.
        |_: &View| false,
    );
    let crate::cmd_out::Query::Populated(v) = q else {
        err(&format!("could not read issue #{issue} in {repo}"));
        return Err(exit::ERROR);
    };

    Ok(Inputs {
        title: v.title,
        body: v.body,
        labels: v.labels.into_iter().map(|l| l.name).collect(),
        comments: v.comments.into_iter().map(|c| c.body).collect(),
    })
}

/// `owner/repo` from the checkout's origin remote, `gh` as the fallback — the
/// same order, for the same offline/rate-limit reason, as `dep_classify`.
fn resolve_repo(repo_root: &Path) -> Option<String> {
    if let Some(nwo) = crate::script_helpers::run_git(repo_root, &["remote", "get-url", "origin"])
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
        repo_root,
        false,
        |n: &NameWithOwner| n.name_with_owner.is_empty(),
    );
    match q {
        crate::cmd_out::Query::Populated(n) => Some(n.name_with_owner),
        _ => None,
    }
}

fn nwo_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.strip_suffix(".git").unwrap_or(url);
    let mut parts: Vec<&str> = trimmed.rsplit(['/', ':']).take(2).collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    parts.reverse();
    Some(parts.join("/"))
}
