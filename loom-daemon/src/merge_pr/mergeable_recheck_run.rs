//! The stale-cached-mergeable recheck *loop* (#6104, slice of #8191).
//!
//! [`super::mergeable_recheck`] owns the terminal classification. This module
//! owns what the shell function `_recheck_mergeable_before_refusal` still did
//! around it: backoff, the uncached PR re-reads, the ref-name vet, the
//! `git fetch` and the local `git merge-tree` corroboration. The shell keeps a
//! wrapper of the same name and signature that calls the verb and echoes its
//! one `<action>:<reason>` line.
//!
//! # Order is the contract
//!
//! Per attempt: sleep, then read. The read is `forge_get_pr_nocache` run from
//! `lib/forge-helpers.sh` in a child `bash` (the same bridge slices 2-3 use),
//! so the Gitea branch, the `gh` / `gh-cached` `--no-cache` split (#3547) and
//! the `gh` argv stay in exactly one place. Corroboration runs only if no
//! attempt resolved: empty refs, then the #9106 ref vet (base first, short
//! circuit), then `git fetch -q origin -- <base> <head>`, then
//! `git merge-tree --write-tree origin/<base> origin/<head>`.
//!
//! # Fail direction
//!
//! Unchanged: every unanswered question degrades toward a refusal
//! (`refuse-stale`), never toward `merge`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::mergeable_recheck::{decide, Evidence, TreeOutcome};
use crate::refname::check_refname;

/// What the pass knows.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub pr_number: String,
    pub base_ref: String,
    pub head_ref: String,
    pub retries: u32,
    pub delay_secs: u64,
}

/// Everything the pass does to the outside world.
pub trait Host {
    fn sleep(&self, secs: u64);
    /// The uncached PR JSON, `{}` when unreadable.
    fn read_pr(&self) -> String;
    /// `git fetch -q origin -- <base> <head>`; true on exit 0.
    fn fetch(&self, base: &str, head: &str) -> bool;
    /// `git merge-tree --write-tree origin/<base> origin/<head>`; true when clean.
    fn merge_tree_clean(&self, base: &str, head: &str) -> bool;
    /// A refused ref name goes to stderr, as `check_branch_name` printed it.
    fn warn(&self, msg: &str);
}

/// jq's `.mergeable // empty` compared to `"true"`.
fn mergeable_true(json: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => match v.get("mergeable") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "true",
            _ => false,
        },
        Err(_) => false,
    }
}

/// Run the loop and return the one `<action>:<reason>` line.
pub fn run(i: &Inputs, host: &dyn Host) -> String {
    let mut resolved = None;
    for attempt in 1..=i.retries {
        host.sleep(i.delay_secs);
        if mergeable_true(&host.read_pr()) {
            resolved = Some(attempt);
            break;
        }
    }
    let mut e = Evidence {
        resolved_attempt: resolved,
        retries: i.retries,
        refs_available: true,
        fetch_ok: true,
        tree: TreeOutcome::NotRun,
        base_ref: i.base_ref.clone(),
        head_ref: i.head_ref.clone(),
    };
    if resolved.is_none() {
        if i.base_ref.is_empty() || i.head_ref.is_empty() {
            e.refs_available = false;
        } else if let Some(msg) = unsafe_ref(i, host) {
            return msg;
        } else if host.fetch(&i.base_ref, &i.head_ref) {
            e.tree = if host.merge_tree_clean(&i.base_ref, &i.head_ref) {
                TreeOutcome::Clean
            } else {
                TreeOutcome::Conflict
            };
        } else {
            e.fetch_ok = false;
        }
    }
    decide(&e)
}

/// #9106: refuse before the fetch could parse a forge-controlled ref as a switch.
fn unsafe_ref(i: &Inputs, host: &dyn Host) -> Option<String> {
    let pr = &i.pr_number;
    for (what, name) in [("base", &i.base_ref), ("head", &i.head_ref)] {
        if let Err(err) = check_refname(name) {
            host.warn(&format!("check_branch_name: REFUSING this {what} ref of PR #{pr} — {err}"));
            return Some(format!(
                "refuse-stale:PR #{pr} carries a ref name that is not a safe git operand \
                 (base='{}' head='{}') — refused before 'git fetch' could parse it as a \
                 switch (#9106). Rename the branch on the PR and re-run.",
                i.base_ref, i.head_ref
            ));
        }
    }
    None
}

/// The real world.
pub struct SystemHost {
    pub repo: String,
    pub pr_number: String,
    pub gh_cmd: String,
    pub repo_root: String,
    pub forge_helpers: String,
}

impl Host for SystemHost {
    fn sleep(&self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }

    fn read_pr(&self) -> String {
        let out = Command::new("bash")
            .arg("-c")
            .arg(r#"source "$1" || exit 127; shift; "$@""#)
            .arg("_")
            .arg(&self.forge_helpers)
            .arg("forge_get_pr_nocache")
            .arg(&self.repo)
            .arg(&self.pr_number)
            .arg(&self.gh_cmd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => "{}".to_string(),
        }
    }

    fn fetch(&self, base: &str, head: &str) -> bool {
        git_ok(&self.repo_root, &["fetch", "-q", "origin", "--", base, head])
    }

    fn merge_tree_clean(&self, base: &str, head: &str) -> bool {
        git_ok(
            &self.repo_root,
            &[
                "merge-tree",
                "--write-tree",
                &format!("origin/{base}"),
                &format!("origin/{head}"),
            ],
        )
    }

    fn warn(&self, msg: &str) {
        eprintln!("{msg}");
    }
}

fn git_ok(root: &str, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(Path::new(root))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        reads: RefCell<Vec<&'static str>>,
        fetch_ok: bool,
        clean: bool,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(reads: &[&'static str], fetch_ok: bool, clean: bool) -> Self {
            Fake {
                reads: RefCell::new(reads.iter().rev().copied().collect()),
                fetch_ok,
                clean,
                log: RefCell::new(vec![]),
            }
        }
    }

    impl Host for Fake {
        fn sleep(&self, _: u64) {
            self.log.borrow_mut().push("sleep".into());
        }
        fn read_pr(&self) -> String {
            self.log.borrow_mut().push("read".into());
            self.reads.borrow_mut().pop().unwrap_or("{}").to_string()
        }
        fn fetch(&self, _: &str, _: &str) -> bool {
            self.log.borrow_mut().push("fetch".into());
            self.fetch_ok
        }
        fn merge_tree_clean(&self, _: &str, _: &str) -> bool {
            self.log.borrow_mut().push("tree".into());
            self.clean
        }
        fn warn(&self, _: &str) {
            self.log.borrow_mut().push("warn".into());
        }
    }

    fn inputs(base: &str, head: &str, retries: u32) -> Inputs {
        Inputs {
            pr_number: "7".into(),
            base_ref: base.into(),
            head_ref: head.into(),
            retries,
            delay_secs: 0,
        }
    }

    #[test]
    fn resolves_on_second_read_without_touching_git() {
        let h = Fake::new(&[r#"{"mergeable":false}"#, r#"{"mergeable":true}"#], true, false);
        let out = run(&inputs("main", "f", 3), &h);
        assert!(out.starts_with("merge:") && out.contains("recheck #2"));
        assert_eq!(*h.log.borrow(), ["sleep", "read", "sleep", "read"]);
    }

    #[test]
    fn null_and_unreadable_never_resolve() {
        let h = Fake::new(&[r#"{"mergeable":null}"#, "not json"], true, true);
        let out = run(&inputs("main", "f", 2), &h);
        assert!(out.starts_with("merge:") && out.contains("is clean"));
        assert_eq!(*h.log.borrow(), ["sleep", "read", "sleep", "read", "fetch", "tree"]);
    }

    #[test]
    fn conflict_refuses_and_fetch_failure_refuses_stale() {
        let h = Fake::new(&[], true, false);
        assert!(run(&inputs("main", "f", 1), &h).starts_with("refuse-conflict:"));
        let h = Fake::new(&[], false, true);
        let out = run(&inputs("main", "f", 1), &h);
        assert!(out.starts_with("refuse-stale:") && out.contains("could not fetch"));
        assert!(!h.log.borrow().contains(&"tree".to_string()));
    }

    #[test]
    fn empty_ref_skips_corroboration() {
        let h = Fake::new(&[], true, true);
        let out = run(&inputs("", "f", 1), &h);
        assert!(out.contains("ref unavailable"));
        assert_eq!(*h.log.borrow(), ["sleep", "read"]);
    }

    #[test]
    fn switch_shaped_ref_is_refused_before_any_git() {
        let h = Fake::new(&[], true, true);
        let out = run(&inputs("main", "--upload-pack=/tmp/x", 1), &h);
        assert!(out.starts_with("refuse-stale:PR #7 carries a ref name"));
        assert_eq!(*h.log.borrow(), ["sleep", "read", "warn"]);
    }

    #[test]
    fn zero_retries_goes_straight_to_corroboration() {
        let h = Fake::new(&[], true, true);
        run(&inputs("main", "f", 0), &h);
        assert_eq!(*h.log.borrow(), ["fetch", "tree"]);
    }
}
