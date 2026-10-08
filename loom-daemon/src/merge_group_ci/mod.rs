//! `loom-daemon merge-group-ci` — combined-tree CI qualification for GitHub's
//! merge queue (#10257, Phase C of #9978).
//!
//! Queue mode only protects integration if every relied-on suite validates
//! the **combined merge-group tree**. A workflow that merely lists
//! `merge_group` as a trigger can still skip the suites that matter on it: a
//! job gated on `github.event_name == 'pull_request'`, a job behind a
//! PR-only path-filter job, a step that reads `github.event.pull_request.*`,
//! a checkout of the PR head instead of the merge-group commit, or a
//! concurrency stanza that cancels or supersedes the merge-group run. Every
//! one of those reports a green (or absent-but-skipped) check while the
//! combined tree went unvalidated.
//!
//! Two read-only verbs:
//!
//! | Verb | Exit 0 | Exit 1 | Exit 2 |
//! |---|---|---|---|
//! | `audit` | every relied-on suite and required context covers `merge_group` | a finding | could not run |
//! | `eligibility` | the repository may pilot queue mode | a named prerequisite failed | could not run |
//!
//! `audit` is purely local (workflow files + `.loom/config.json`).
//! `eligibility` adds `GET`-only forge reads (owner type, permissions, the
//! branch's effective rules) or takes them from `--facts`. Neither verb ever
//! writes to the forge, enqueues, or touches a ruleset/branch protection.
//!
//! Operator reference: `defaults/docs/merge-queue-ci.md`.

pub mod audit;
pub mod context;
pub mod eligibility;
pub mod expr;
pub mod main_cancel;
pub mod workflow;
pub mod yaml;

use std::fmt::Write as _;
use std::path::Path;

pub use audit::{AuditReport, Code, Finding};
pub use eligibility::{Eligibility, Prereq, RepoFacts};
pub use workflow::Workflow;

/// Prefix on every human-readable line.
pub const PREFIX: &str = "merge-group-ci:";

/// Workflows that parsed, and `(file, why)` for those that did not.
pub type Loaded = (Vec<Workflow>, Vec<(String, String)>);

/// Load `<root>/.github/workflows`, optionally narrowed to `only` (file names
/// or repo-relative paths).
///
/// # Errors
///
/// When the workflows directory cannot be listed, or a requested file is
/// absent.
pub fn load(root: &Path, only: &[String]) -> Result<Loaded, String> {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for r in workflow::load_dir(root)? {
        let file = match &r {
            Ok(w) => w.file.clone(),
            Err((f, _)) => f.clone(),
        };
        if !only.is_empty()
            && !only
                .iter()
                .any(|o| file == *o || file.ends_with(&format!("/{o}")))
        {
            continue;
        }
        match r {
            Ok(w) => ok.push(w),
            Err(e) => bad.push(e),
        }
    }
    for o in only {
        let seen = ok.iter().map(|w| &w.file).chain(bad.iter().map(|(f, _)| f));
        if !seen
            .into_iter()
            .any(|f| f == o || f.ends_with(&format!("/{o}")))
        {
            return Err(format!("workflow `{o}` not found under .github/workflows"));
        }
    }
    Ok((ok, bad))
}

/// Required contexts declared in `<root>/.loom/config.json`
/// (`branchProtection.requiredStatusChecks`), or none.
#[must_use]
pub fn configured_required(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".loom/config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.pointer("/branchProtection/requiredStatusChecks")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        })
        .unwrap_or_default()
}

fn state_cell(s: context::RunState) -> &'static str {
    match s {
        context::RunState::Runs => "runs",
        context::RunState::Skipped => "skipped",
        context::RunState::Unknown => "unknown",
    }
}

/// Human rendering of an audit.
#[must_use]
pub fn render_audit(r: &AuditReport, verbose: bool) -> String {
    let mut out = String::new();
    for w in &r.workflows {
        if w.relied_on || verbose {
            let _ = writeln!(
                out,
                "{PREFIX} workflow {} ({}): {}; merge_group trigger: {}",
                w.file,
                w.name,
                w.reason,
                if w.merge_group_trigger { "yes" } else { "no" }
            );
        }
    }
    for j in r.jobs.iter().filter(|j| j.relied_on || verbose) {
        let s = |e| state_cell(j.states[&e]);
        let _ = writeln!(
            out,
            "{PREFIX}   {} {:<28} pr={:<7} push={:<7} merge_group={:<7} {}",
            if j.covered {
                "COVERED  "
            } else if j.relied_on {
                "UNCOVERED"
            } else {
                "-        "
            },
            j.id,
            s(context::Event::PullRequest),
            s(context::Event::Push),
            s(context::Event::MergeGroup),
            j.pr_only_marker
                .as_deref()
                .map(|m| format!("(pr-only: {m})"))
                .unwrap_or_default(),
        );
    }
    for c in &r.required {
        let _ = writeln!(
            out,
            "{PREFIX} required context `{}`: {} ({})",
            c.context,
            if c.covered { "covered" } else { "NOT covered" },
            c.job.as_deref().unwrap_or("no matching job")
        );
    }
    for f in &r.findings {
        let _ = writeln!(out, "{PREFIX} FINDING {f}");
    }
    let _ = writeln!(
        out,
        "{PREFIX} {}",
        if r.qualified() {
            "QUALIFIED — every relied-on suite runs on the merge-group commit (nothing was changed)"
                .to_string()
        } else {
            format!(
                "NOT QUALIFIED — {} finding(s); a skipped or unproven suite is not coverage (nothing was changed)",
                r.findings.len()
            )
        }
    );
    out
}

/// Human rendering of an eligibility verdict.
#[must_use]
pub fn render_eligibility(e: &Eligibility) -> String {
    let mut out = String::new();
    let repo = e.facts.repository.as_deref().unwrap_or("(facts file)");
    let _ = writeln!(
        out,
        "{PREFIX} repository {repo}: owner type {}, branch {}, push permission {}",
        e.facts.owner_type.as_deref().unwrap_or("unknown"),
        e.facts.branch.as_deref().unwrap_or("unknown"),
        e.facts
            .can_push
            .map_or("unknown", |b| if b { "yes" } else { "no" })
    );
    if let Some(mq) = &e.merge_queue {
        let _ = writeln!(out, "{PREFIX} merge_queue rule parameters: {mq}");
    }
    if !e.required_checks.is_empty() {
        let _ = writeln!(out, "{PREFIX} required checks: {}", e.required_checks.join(", "));
    }
    for f in &e.failures {
        let _ = writeln!(out, "{PREFIX} FAILED {}: {}", f.prereq.as_str(), f.detail);
    }
    let _ = writeln!(
        out,
        "{PREFIX} {}",
        if e.eligible {
            "ELIGIBLE — prerequisites met; enabling queue mode still needs operator authorization (nothing was changed)"
        } else {
            "NOT ELIGIBLE — see FAILED prerequisites above (nothing was changed)"
        }
    );
    out
}

#[cfg(test)]
mod tests;
