//! `worktree.sh`'s base-ref preparation: the `origin/$DEFAULT_BRANCH` fetch and
//! the `--base <branch>` stacked-PR resolution (#8195 slice 13, epic #7810).
//!
//! Two shell blocks moved together because they are one decision — *which ref
//! does the new feature branch start from* — and share a single ordering
//! constraint (default branch fetched first, then the explicit base):
//!
//! 1. `fetch_latest_main`: a fetch-only refresh of `origin/$DEFAULT_BRANCH`,
//!    never fatal (offline builds continue on local state).
//! 2. The `--base` block (#3729): validate the name (#9106), fetch it, prefer
//!    `origin/<base>`, fall back to a local `<base>`, refuse when neither
//!    resolves — silently branching off the default branch would un-stack the
//!    child.
//!
//! # Record protocol
//!
//! Like `worktree-check --porcelain`, the verb prints `TOKEN<TAB>text` records
//! that `worktree.sh` replays through its own `print_*` helpers, so message text
//! and order stay owned here while the shell keeps only what it must (its own
//! variables and `exit`). `BASE_REF` / `BASE_DISPLAY` are data records. `JSON`
//! carries the complete `--json` failure document, built with `serde_json`: the
//! retired shell spliced `$BASE_BRANCH` into a JSON string by hand, which
//! produced invalid JSON for any name containing a quote or backslash.
//!
//! Exit 0 = resolved, 1 = refused (the shell exits 1 too), 2 is reserved for
//! "could not run at all" and is the stub's, never this module's.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::refname::check_refname;

/// One replayed line; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `print_info`.
    Info,
    /// `print_success`.
    Success,
    /// `print_warning`.
    Warning,
    /// `print_error` (stderr).
    Error,
    /// Plain `echo`.
    Plain,
    /// A complete JSON document for the caller's `>&3`.
    Json,
    /// Data: the ref new branches are created from.
    BaseRef,
    /// Data: the human name of that ref.
    BaseDisplay,
}

impl Level {
    /// The protocol token exactly as the wrapper's `case` reads it.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Success => "SUCCESS",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Plain => "PLAIN",
            Level::Json => "JSON",
            Level::BaseRef => "BASE_REF",
            Level::BaseDisplay => "BASE_DISPLAY",
        }
    }
}

/// The outcome: records in replay order plus the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Records, in the order the shell must replay them.
    pub records: Vec<(Level, String)>,
    /// 0 resolved, 1 refused.
    pub code: i32,
}

/// Fetch `origin/<default_branch>` and resolve the base ref, in `repo`.
///
/// `quiet` is `--json` mode: message records are suppressed, exactly as the
/// retired shell wrapped each in `[[ "$JSON_OUTPUT" == "true" ]]`. Refusal
/// documents are still emitted (they are the JSON contract).
#[must_use]
pub fn resolve(repo: &Path, default_branch: &str, base: Option<&str>, quiet: bool) -> Outcome {
    let mut rec: Vec<(Level, String)> = Vec::new();
    let say = |rec: &mut Vec<(Level, String)>, lvl: Level, text: String| {
        if !quiet {
            rec.push((lvl, text));
        }
    };

    // 1. fetch_latest_main. `--` ends option parsing (#9106); the caller has
    // already validated `default_branch` (loom_default_branch), and this
    // refuses again rather than trusting that.
    if let Err(e) = check_refname(default_branch) {
        rec.push((Level::Error, refusal("default branch", &e)));
        return Outcome {
            records: rec,
            code: 1,
        };
    }
    say(
        &mut rec,
        Level::Info,
        format!("Fetching latest changes from origin/{default_branch}..."),
    );
    if git_ok(repo, &["fetch", "origin", "--", default_branch]) {
        say(&mut rec, Level::Success, format!("Fetched latest origin/{default_branch}"));
    } else {
        say(
            &mut rec,
            Level::Warning,
            format!("Could not fetch origin/{default_branch} (continuing with local state)"),
        );
    }

    let mut base_ref = format!("origin/{default_branch}");
    let mut base_display = default_branch.to_string();

    // 2. --base (#3729).
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        if let Err(e) = check_refname(base) {
            rec.push((Level::Error, refusal("--base branch", &e)));
            rec.push((
                Level::Json,
                serde_json::json!({
                    "success": false, "error": "unsafe-base-branch-name", "baseBranch": base
                })
                .to_string(),
            ));
            return Outcome {
                records: rec,
                code: 1,
            };
        }
        // Best effort: an unreachable remote falls through to the local ref.
        let _ = git_ok(repo, &["fetch", "origin", "--", base]);
        if git_ok(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{base}"),
            ],
        ) {
            base_ref = format!("origin/{base}");
            base_display.clone_from(&base_ref);
        } else if git_ok(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{base}"),
            ],
        ) {
            base_ref = base.to_string();
            base_display.clone_from(&base_ref);
        } else {
            if quiet {
                rec.push((
                    Level::Json,
                    serde_json::json!({
                        "success": false, "error": "base-branch-not-found", "baseBranch": base
                    })
                    .to_string(),
                ));
            } else {
                rec.push((
                    Level::Error,
                    format!(
                        "Requested --base '{base}' not found as origin/{base} or a local branch."
                    ),
                ));
                rec.push((
                    Level::Plain,
                    "  Ensure the parent sweep has created/pushed feature/issue-<parent> before \
                     stacking a child on it."
                        .to_string(),
                ));
            }
            return Outcome {
                records: rec,
                code: 1,
            };
        }
        say(
            &mut rec,
            Level::Info,
            format!("Stacked worktree base: {base_display} (from --base {base})"),
        );
    }

    rec.push((Level::BaseRef, base_ref));
    rec.push((Level::BaseDisplay, base_display));
    Outcome {
        records: rec,
        code: 0,
    }
}

/// Print an [`Outcome`] in protocol form and return its exit code.
#[must_use]
pub fn emit(outcome: &Outcome) -> i32 {
    for (level, text) in &outcome.records {
        println!("{}\t{}", level.token(), text.replace('\n', " "));
    }
    outcome.code
}

/// The wording `check_branch_name` printed, which retained suites (and operators'
/// muscle memory) match on: naming WHICH operand was refused.
fn refusal(what: &str, e: &crate::refname::RefnameError) -> String {
    format!("check_branch_name: REFUSING this {what} — {e}")
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests;
