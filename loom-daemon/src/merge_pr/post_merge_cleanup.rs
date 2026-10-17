//! `merge-pr.sh`'s whole post-merge worktree/branch cleanup block, as one
//! orchestrator (#8191, slice 1 of `docs/migration/merge-pr-orchestrator-port.md`).
//!
//! Before this module the cleanup was ~580 lines of shell sequencing nine Rust
//! predicates (`cleanup-paths`, `worktree-preserve`, `discovered-worktree`,
//! `remove-gate`, `dirty-guard`, `worktree-teardown`, `issue-close-gate`,
//! `delete-branch`, `cargo-target-dir`), each reached through a `$(...)`
//! capture, a `|| rc=$?`, a first-line token compare and a hand-written "the
//! verb did not run" warning. Every one of those seams was a place a fault
//! could read as an answer. Here the predicates are called in-process, in the
//! order the shell called them, and only the sequencing lives in this file.
//!
//! # The #5031 data-loss guard, and what it is NOT (carried over from the shell)
//!
//! Post-merge cleanup keys the worktree only by branch name
//! (`feature/issue-<N>`), and `worktree.sh`'s naming makes that name collide
//! whenever two hosts or sessions claim the same issue. A different, still-live
//! builder with unsaved edits in that worktree must never be `--force`d away
//! (observed 2026-08-03 on #5001), so `Run::remove_loom_worktree` reads
//! `git status --porcelain` and refuses on user work; Loom's own runtime markers
//! are filtered by [`super::dirty_guard`]. Distinct root causes, not to be
//! re-conflated in triage: #4463 (closed) was same-HOST duplicate dispatch — lock
//! *ownership* — and would not have caught #5031 (the colliding worktree came
//! from a host that never touched this daemon's lock); #4146 (operator-held)
//! *measures* the cross-host collision rate and changes no behaviour, while this
//! guard is the behavioural mitigation for the data-loss instance it quantifies.
//!
//! # What stays in the shell, and the `LOOM-NEED` protocol
//!
//! Three facts come from forge-aware shell libraries that serve Gitea as well
//! as GitHub (`forge_pr_close_targets`, `forge_get_issue_state`) and from
//! `branch_has_landed` (`lib/branch-landed.sh`). They are LAZY in the retired
//! code: the overwhelming common case (`Closes #N`) never pays for the issue
//! lookup, and a PR with no worktree on disk pays for none of them. The verb
//! keeps that by asking: when it needs a fact it was not given it prints one
//! `LOOM-NEED <fact>` line, exits [`NEED_EXIT`] and has done NOTHING — output
//! is buffered and every fact is demanded before the first mutation (see
//! `Run::need`) — so the shell fetches the fact and calls again with it.
//!
//! # Output
//!
//! `LEVEL<TAB>message` per line (`INFO`, `WARNING`, `SUCCESS`, `PLAIN`),
//! replayed through the script's own logging, plus at most one `CD<TAB>dir`
//! asking the shell to leave a worktree that no longer exists.
//!
//! # Fail direction
//!
//! Unchanged from the shell: every guard fails toward removing nothing. A fact
//! that was fetched and came back empty is an answer (the issue is not a close
//! target, its state did not read CLOSED, the branch is not landed) and
//! resolves to PRESERVE exactly as it did; a fact that was never supplied is
//! not an answer and is asked for.

use std::cell::RefCell;

use super::{
    cleanup_paths, dirty_guard, discovered_worktree, issue_close_gate, remove_gate,
    worktree_preserve, worktrees,
};

/// Last stdout line of a finished run. Without it the shell treats the verb as
/// not having run: an exit 0 with no output is what a stale or muted binary
/// produces, and must not read as "cleanup ran and had nothing to say".
pub const DONE: &str = "LOOM-POST-MERGE-CLEANUP DONE";

/// Exit status meaning "I need a fact; see the `LOOM-NEED` line".
pub const NEED_EXIT: i32 = 3;

/// One replayable output record: `(LEVEL, message)`.
pub type Record = (String, String);

/// A forge-derived fact the shell must supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// `forge_pr_close_targets <pr>` — one issue number per line.
    CloseTargets,
    /// `forge_get_issue_state` for this issue number (`OPEN`/`CLOSED`/empty).
    IssueState(String),
    /// `branch_has_landed` — verdict, evidence and whether it returned 0.
    Landed,
}

impl Need {
    /// What follows `LOOM-NEED `: the fact's name, then its argument if any.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Need::CloseTargets => "close-targets".to_string(),
            Need::IssueState(issue) => format!("issue-state {issue}"),
            Need::Landed => "landed".to_string(),
        }
    }
}

/// `branch_has_landed`'s answer, as the shell fetched it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Landed {
    /// Whether `branch_has_landed` returned 0.
    pub landed: bool,
    /// `$BRANCH_LANDED_VERDICT`.
    pub verdict: String,
    /// `$BRANCH_LANDED_EVIDENCE`.
    pub evidence: String,
}

/// Facts the shell has already fetched. `None` means "never asked", which is
/// not the same as an empty answer.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub close_targets: Option<String>,
    pub issue_state: Option<String>,
    pub landed: Option<Landed>,
}

/// What the merge settled, from the script's variables.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub repo_root: String,
    pub pr_number: String,
    pub pr_branch: String,
    pub pr_head_sha: String,
    /// `$DEFAULT_BRANCH_NAME`; `None` when it did not resolve.
    pub default_branch: Option<String>,
    /// `$CLEANUP_WORKTREE` (`--no-cleanup-worktree` sets it false).
    pub cleanup_worktree: bool,
    /// `$CLEANUP_PRIMARY_CHECKOUT` (`--no-cleanup-primary` sets it false).
    pub cleanup_primary: bool,
    /// `LOOM_PRESERVE_WORKTREE=1`.
    pub preserve_env: bool,
    /// `--worktree-path`, already validated by the shell; empty when absent.
    pub worktree_path_override: String,
}

/// The world the orchestrator touches. A trait so the sequencing is tested
/// without a repository; [`SystemHost`] is the production wiring.
pub trait Host {
    fn is_dir(&self, path: &str) -> bool;
    /// `git -C <repo_root> worktree list --porcelain`, empty on failure (the
    /// retired code piped `2>/dev/null` into a parser that read empty as "no
    /// worktrees").
    fn worktree_list(&self, repo_root: &str) -> String;
    /// `git -C <path> status --porcelain`, empty on failure — `git status`
    /// failing has always meant "no dirt, proceed" here (#5177).
    fn git_status(&self, path: &str) -> String;
    fn sentinel_present(&self, path: &str) -> bool;
    /// `cd <path> && pwd -P`.
    fn canonical(&self, path: &str) -> Option<String>;
    fn cwd(&self) -> Option<String>;
    fn chdir(&self, path: &str);
    /// `(default_path, issue_num, judge_pr_path)`; `None` when the resolved
    /// worktree root cannot be framed (a tab/newline in it).
    fn cleanup_targets(
        &self,
        repo_root: &str,
        branch: &str,
        pr: &str,
    ) -> Option<(String, String, String)>;
    fn resolve_cargo_target(&self, worktree: &str) -> String;
    /// `worktree_teardown::teardown`'s rendered text.
    fn teardown(&self, repo_root: &str, path: &str) -> String;
    fn reclaim_cargo_target(
        &self,
        repo_root: &str,
        worktree: &str,
        resolved: &str,
    ) -> Option<Record>;
    fn record_removal(&self, repo_root: &str, worktree: &str, branch: Option<&str>);
    fn delete_branch(
        &self,
        inputs: &Inputs,
        branch: &str,
        expected_head_sha: &str,
        out: &mut Vec<Record>,
    );
}

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Finished; replay `records`, then `cd` if asked.
    Done {
        records: Vec<Record>,
        cd: Option<String>,
    },
    /// A fact is missing; nothing was done.
    Need(Need),
}

/// Run the cleanup block. See the module docs.
pub fn run(inputs: &Inputs, facts: &Facts, host: &dyn Host) -> Outcome {
    let mut r = Run {
        inputs,
        facts,
        host,
        out: RefCell::new(Vec::new()),
        cd: RefCell::new(None),
        mutated: RefCell::new(false),
    };
    match r.run() {
        Ok(()) => Outcome::Done {
            records: r.out.into_inner(),
            cd: r.cd.into_inner(),
        },
        Err(need) => Outcome::Need(need),
    }
}

struct Run<'a> {
    inputs: &'a Inputs,
    facts: &'a Facts,
    host: &'a dyn Host,
    out: RefCell<Vec<Record>>,
    cd: RefCell<Option<String>>,
    /// Set once anything irreversible has run. A fact demanded after that
    /// point cannot be asked for (the verb would be re-run over a half-done
    /// cleanup), so it resolves to the fail-safe answer instead.
    mutated: RefCell<bool>,
}

/// Split a rendered `verdict\nLEVEL\tmsg\n…` protocol blob into its first line
/// and records. Records without a tab are dropped, as the shell's
/// `[[ -n "$level" ]]` guard dropped blank lines.
fn parse(rendered: &str) -> (&str, Vec<Record>) {
    let (first, rest) = rendered.split_once('\n').unwrap_or((rendered, ""));
    let records = rest
        .lines()
        .filter_map(|l| {
            let (level, text) = l.split_once('\t')?;
            (!level.is_empty()).then(|| (level.to_string(), text.to_string()))
        })
        .collect();
    (first, records)
}

impl Run<'_> {
    fn push(&self, level: &str, text: impl Into<String>) {
        self.out.borrow_mut().push((level.to_string(), text.into()));
    }
    fn info(&self, t: impl Into<String>) {
        self.push("INFO", t);
    }
    fn warning(&self, t: impl Into<String>) {
        self.push("WARNING", t);
    }
    fn extend(&self, records: Vec<Record>) {
        self.out.borrow_mut().extend(records);
    }

    /// Ask for a fact, or hand back the fail-safe `None` when it is too late
    /// to ask. `Err` only ever happens before the first mutation.
    fn need<T>(&self, have: Option<T>, need: &Need) -> Result<Option<T>, Need> {
        match have {
            Some(v) => Ok(Some(v)),
            None if *self.mutated.borrow() => Ok(None),
            None => Err(need.clone()),
        }
    }

    fn run(&mut self) -> Result<(), Need> {
        let i = self.inputs;
        if !i.cleanup_worktree {
            self.info(
                "Worktree cleanup skipped (--no-cleanup-worktree) — local branch left in place",
            );
            return Ok(());
        }
        if i.preserve_env {
            self.info(
                "Worktree cleanup skipped (LOOM_PRESERVE_WORKTREE=1) — local branch left in place",
            );
        } else if !i.worktree_path_override.is_empty() {
            self.info(format!(
                "Cleanup target overridden by --worktree-path: {}",
                i.worktree_path_override
            ));
            self.remove_loom_worktree(&i.worktree_path_override, true);
        } else {
            self.convention_cleanup()?;
        }
        Ok(())
    }

    /// The default arm: name the targets, decide each, then delete the branch.
    fn convention_cleanup(&mut self) -> Result<(), Need> {
        let i = self.inputs;
        let targets = self
            .host
            .cleanup_targets(&i.repo_root, &i.pr_branch, &i.pr_number);
        let (default_wt, issue_num, judge_wt) = match targets {
            Some(t) => t,
            None => {
                // Nothing is removed rather than guessed: an empty default
                // path gates BOTH removal sites off (see the retired shell's
                // long comment — discovery with no plan would be MORE
                // destructive than the healthy path).
                self.warning(format!(
                    "Post-merge worktree cleanup for PR #{} did not run — the resolved worktree root contains a tab or newline, so which worktree paths this merge owns cannot be framed unambiguously. Nothing is removed rather than guessed — the merge itself already succeeded and is unaffected. Clean up by hand: .loom/scripts/worktree.sh remove <issue>, or loom-clean.",
                    i.pr_number
                ));
                self.delete_branch(&i.pr_branch, &i.pr_head_sha);
                return Ok(());
            }
        };

        if self.host.is_dir(&default_wt) {
            self.cleanup_decide(worktree_preserve::Kind::Default, &default_wt, &issue_num)?;
        } else {
            self.discovery_fallback(&default_wt, &issue_num)?;
        }

        // #6264: a co-existing Judge/Doctor review worktree, by PATH.
        if !judge_wt.is_empty() && self.host.is_dir(&judge_wt) {
            self.cleanup_decide(worktree_preserve::Kind::JudgePr, &judge_wt, &issue_num)?;
        }
        self.delete_branch(&i.pr_branch, &i.pr_head_sha);
        Ok(())
    }

    /// The Loom-convention path is missing: look for a worktree tracking the
    /// branch. Never auto-removed unless it is Loom-managed (#3334).
    fn discovery_fallback(&mut self, default_wt: &str, issue_num: &str) -> Result<(), Need> {
        let i = self.inputs;
        let porcelain = self.host.worktree_list(&i.repo_root);
        let Some(found) = worktrees::find_by_branch(&porcelain, &i.pr_branch)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
        else {
            self.info(format!(
                "No worktree found at {default_wt} (and none tracking '{}' in 'git worktree list')",
                i.pr_branch
            ));
            return Ok(());
        };
        let primary = self.is_primary(&porcelain, &found);
        let (action, lines) = discovered_worktree::decide(
            &i.pr_branch,
            &found,
            primary,
            self.host.sentinel_present(&found),
        );
        let rendered = discovered_worktree::render(action, &lines);
        let (_, records) = parse(&rendered);
        self.extend(records);
        if action == discovered_worktree::Action::Decide {
            self.cleanup_decide(worktree_preserve::Kind::Discovered, &found, issue_num)?;
        }
        Ok(())
    }

    /// `_is_primary_worktree_path`: does `path` resolve to the first
    /// `git worktree list` entry? Only chooses remediation text.
    fn is_primary(&self, porcelain: &str, path: &str) -> bool {
        let real = self
            .host
            .canonical(path)
            .unwrap_or_else(|| path.to_string());
        worktrees::primary_path(porcelain).is_some_and(|p| !p.is_empty() && p == real)
    }

    /// `_issue_is_closed_for_cleanup`: true = safe to clean up.
    fn issue_is_closed(&self, issue: &str) -> Result<bool, Need> {
        let Some(targets) = self.need(self.facts.close_targets.clone(), &Need::CloseTargets)?
        else {
            return Ok(false);
        };
        use issue_close_gate::Decision;
        match issue_close_gate::decide(&targets, issue, None) {
            Decision::CloseTarget | Decision::StateClosed => Ok(true),
            Decision::Preserve => Ok(false),
            Decision::NeedState => {
                let Some(state) = self
                    .need(self.facts.issue_state.clone(), &Need::IssueState(issue.to_string()))?
                else {
                    return Ok(false);
                };
                Ok(matches!(
                    issue_close_gate::decide(&targets, issue, Some(&state)),
                    Decision::CloseTarget | Decision::StateClosed
                ))
            }
        }
    }

    /// `_worktree_cleanup_decide`.
    fn cleanup_decide(
        &mut self,
        kind: worktree_preserve::Kind,
        path: &str,
        issue_num: &str,
    ) -> Result<(), Need> {
        let i = self.inputs;
        let mut preserve_check = false;
        let mut landed = Landed::default();
        if !issue_num.is_empty() && !self.issue_is_closed(issue_num)? {
            preserve_check = true;
            // Unfetched-after-mutation resolves to "not landed" (preserve).
            landed = self
                .need(self.facts.landed.clone(), &Need::Landed)?
                .unwrap_or_default();
        }
        let ctx = worktree_preserve::Context {
            kind,
            path,
            repo_root: &i.repo_root,
            pr_number: &i.pr_number,
            branch: &i.pr_branch,
            issue_num,
            preserve_check,
            landed: landed.landed,
            landed_verdict: &landed.verdict,
            landed_evidence: &landed.evidence,
        };
        let (action, lines) = worktree_preserve::decide(&ctx);
        let rendered = worktree_preserve::render(action, &lines);
        let (_, records) = parse(&rendered);
        self.extend(records);
        if action == worktree_preserve::Action::Remove {
            self.remove_loom_worktree(path, false);
        }
        Ok(())
    }

    fn delete_branch(&self, branch: &str, expected: &str) {
        let mut buf = Vec::new();
        self.host
            .delete_branch(self.inputs, branch, expected, &mut buf);
        self.extend(buf);
    }

    /// `_remove_loom_worktree <path> [allow_unmanaged]`. Best-effort: every
    /// refusal is a warning, because the merge already succeeded.
    fn remove_loom_worktree(&mut self, path: &str, allow_unmanaged: bool) {
        let i = self.inputs;
        if !self.host.is_dir(path) {
            self.info(format!("No worktree found at {path}"));
            return;
        }
        let real = self
            .host
            .canonical(path)
            .unwrap_or_else(|| path.to_string());

        // #3710 primary-worktree guard + .loom-managed sentinel guard.
        let porcelain = self.host.worktree_list(&i.repo_root);
        let gate_ctx = remove_gate::Context {
            porcelain: &porcelain,
            path,
            real: &real,
            allow_unmanaged,
            sentinel_present: self.host.sentinel_present(path),
        };
        let (verdict, lines) = remove_gate::decide(&gate_ctx);
        let (_, records) = parse(&remove_gate::render(verdict, &lines));
        self.extend(records);
        if verdict != remove_gate::Verdict::Proceed {
            return;
        }

        // Recorded BEFORE removal: the porcelain entry vanishes with the tree.
        let attached_branch = if allow_unmanaged {
            let list = self.host.worktree_list(&i.repo_root);
            let target = self
                .host
                .canonical(path)
                .unwrap_or_else(|| path.to_string());
            worktrees::branch_for_path(&list, &target).unwrap_or_default()
        } else {
            String::new()
        };

        // If our shell is inside the worktree we are removing, hop out first.
        let in_worktree = self
            .host
            .cwd()
            .is_some_and(|cwd| cwd.starts_with(real.as_str()));
        if in_worktree {
            self.host.chdir(&i.repo_root);
        }

        // #5031 data-loss guard: never force-remove uncommitted work.
        let status = self.host.git_status(path);
        let status = status.trim_end_matches('\n');
        let list = self.host.worktree_list(&i.repo_root);
        let live_branch = worktrees::branch_for_path(&list, &real).unwrap_or_default();
        let dirty_ctx = dirty_guard::Context {
            worktree_path: path,
            repo_root: &i.repo_root,
            branch: &live_branch,
        };
        if let Some(refusal) = dirty_guard::assess(&dirty_ctx, &format!("{status}\n")) {
            for (level, text) in refusal {
                self.push(level.token(), text);
            }
            return;
        }

        // Resolved BEFORE removal: `cargo metadata` needs the manifest.
        let resolved = self.host.resolve_cargo_target(path);
        self.info(format!("Removing worktree: {path}"));
        *self.mutated.borrow_mut() = true;
        let torn = self.host.teardown(&i.repo_root, path);
        let (verdict_line, records) = parse(&torn);
        self.extend(records);
        if verdict_line != super::worktree_teardown::REMOVED {
            return;
        }

        let ledger_branch = if !attached_branch.is_empty() {
            Some(attached_branch.as_str())
        } else if !i.pr_branch.is_empty() {
            Some(i.pr_branch.as_str())
        } else {
            None
        };
        self.host.record_removal(&i.repo_root, path, ledger_branch);
        if in_worktree {
            self.push("PLAIN", "");
            self.warning("Your shell's working directory was inside the removed worktree.");
            self.warning("Run this command to fix:");
            self.push("PLAIN", format!("  cd {}", i.repo_root));
            *self.cd.borrow_mut() = Some(i.repo_root.clone());
        }
        // The override path also tidies the attached branch, AFTER the
        // worktree's checkout lock is released; it passes no head SHA.
        if allow_unmanaged && !attached_branch.is_empty() {
            self.delete_branch(&attached_branch, "");
        }
        // #7239: reclaim a redirected cargo target dir now the tree is gone.
        if !resolved.is_empty() {
            if let Some((level, text)) =
                self.host
                    .reclaim_cargo_target(&i.repo_root, path, &resolved)
            {
                self.push(&level, text);
            }
        }
    }
}

/// Production wiring.
pub struct SystemHost;

impl Host for SystemHost {
    fn is_dir(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }

    fn worktree_list(&self, repo_root: &str) -> String {
        git_stdout(&["-C", repo_root, "worktree", "list", "--porcelain"])
    }

    fn git_status(&self, path: &str) -> String {
        git_stdout(&["-C", path, "status", "--porcelain"])
    }

    fn sentinel_present(&self, path: &str) -> bool {
        std::path::Path::new(path).join(".loom-managed").is_file()
    }

    fn canonical(&self, path: &str) -> Option<String> {
        std::fs::canonicalize(path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn cwd(&self) -> Option<String> {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn chdir(&self, path: &str) {
        let _ = std::env::set_current_dir(path);
    }

    fn cleanup_targets(
        &self,
        repo_root: &str,
        branch: &str,
        pr: &str,
    ) -> Option<(String, String, String)> {
        let root = crate::worktree_root::worktree_root_readable(std::path::Path::new(repo_root));
        let line = cleanup_paths::render(&cleanup_paths::plan(branch, pr, &root))?;
        let mut f = line.trim_end_matches('\n').split('\t').skip(1);
        Some((
            f.next()?.to_string(),
            f.next().unwrap_or_default().to_string(),
            f.next().unwrap_or_default().to_string(),
        ))
    }

    fn resolve_cargo_target(&self, worktree: &str) -> String {
        crate::worktree_ops::cargo_target::resolve_for_worktree(std::path::Path::new(worktree))
            .display()
            .to_string()
    }

    fn teardown(&self, repo_root: &str, path: &str) -> String {
        super::worktree_teardown::teardown(repo_root, path)
    }

    fn reclaim_cargo_target(
        &self,
        repo_root: &str,
        worktree: &str,
        resolved: &str,
    ) -> Option<Record> {
        use std::path::Path;
        crate::worktree_ops::cargo_target::reclaim(
            Path::new(repo_root),
            Path::new(worktree),
            Path::new(resolved),
            false,
        )
        .report_record()
        .map(|(level, message)| (level.to_string(), message.to_string()))
    }

    fn record_removal(&self, repo_root: &str, worktree: &str, branch: Option<&str>) {
        crate::worktree_ops::removal_log::record(
            std::path::Path::new(repo_root),
            "merge-pr.sh",
            std::path::Path::new(worktree),
            branch,
            "post_merge_cleanup",
        );
    }

    fn delete_branch(
        &self,
        inputs: &Inputs,
        branch: &str,
        expected_head_sha: &str,
        out: &mut Vec<Record>,
    ) {
        use crate::worktree_cli::branch_delete::{maybe_delete_local_branch, DeleteContext};
        use crate::worktree_cli::wip::Sink;

        struct Collect(RefCell<Vec<Record>>);
        impl Collect {
            /// One record per line, each at the message's own level, so a
            /// multi-line git error never reaches the shell without a LEVEL.
            fn add(&self, level: &str, msg: &str) {
                let mut v = self.0.borrow_mut();
                let mut any = false;
                for line in msg.lines() {
                    any = true;
                    v.push((level.to_string(), line.to_string()));
                }
                if !any {
                    v.push((level.to_string(), String::new()));
                }
            }
        }
        impl Sink for Collect {
            fn info(&self, msg: &str) {
                self.add("INFO", msg);
            }
            fn warning(&self, msg: &str) {
                self.add("WARNING", msg);
            }
            fn success(&self, msg: &str) {
                self.add("SUCCESS", msg);
            }
        }

        let sink = Collect(RefCell::new(Vec::new()));
        let repo_root = std::path::PathBuf::from(&inputs.repo_root);
        let ctx = DeleteContext {
            repo_root: &repo_root,
            default_branch: inputs.default_branch.as_deref(),
            cleanup_primary_checkout: inputs.cleanup_primary,
        };
        let _ = maybe_delete_local_branch(&ctx, &sink, branch, expected_head_sha);
        out.extend(sink.0.into_inner());
    }
}

/// `git <args>` stdout with trailing newlines stripped (as `$(…)` does), or
/// empty on any failure.
fn git_stdout(args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_end_matches('\n')
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
