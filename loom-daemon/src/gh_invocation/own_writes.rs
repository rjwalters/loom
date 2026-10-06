//! Issue and PR numbers this process has just written through the facade
//! (W9 of the forge API reduction plan).
//!
//! The dispatch guards read `repos/{o}/{r}/issues/{n}` conditionally
//! (`sweep_registry::guards::issue_snapshot`). A read that follows this
//! daemon's own write to the same number must neither ride a reader App,
//! which may lag the write (W4-C), nor send an `If-None-Match` a lagging
//! replica could answer `304`. Hooking every write site by hand drifts: issue
//! writes live in the guards, the claim label helpers, quarantine, restore,
//! the PR-less hold, claim reconciliation and more. So
//! [`super::GhInvocation::execute`] records the number of **every**
//! write-intent invocation here, and the guard asks [`written_within`].
//!
//! Keyed by number alone, not repo: a number written in another repo only
//! makes one extra read unconditional and writer-pinned. That direction is
//! the safe one. The record is taken before the write runs, so a write that
//! fails still pins the next read.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::{Duration, Instant};

/// Entries older than this are dropped on the next write.
const KEEP: Duration = Duration::from_secs(3600);

/// Record the issue/PR numbers `args` (a `gh` argv) writes.
pub(crate) fn note(args: &[OsString]) {
    let numbers = written_numbers(args);
    if numbers.is_empty() {
        return;
    }
    let now = Instant::now();
    with(|m| {
        m.retain(|_, at| now.duration_since(*at) < KEEP);
        for n in numbers {
            m.insert(n, now);
        }
    });
}

/// Whether this process wrote number `n` within the last `window`.
#[must_use]
pub(crate) fn written_within(n: u32, window: Duration) -> bool {
    with(|m| m.get(&n).is_some_and(|at| at.elapsed() < window))
}

/// The issue/PR numbers a write argv targets: a `gh api` path
/// `repos/{o}/{r}/(issues|pulls)/{n}[/…]`, or the selector of
/// `gh issue|pr <verb> <n|url>`. Anything else (GraphQL, a comment edited by
/// id, a repo-level write) names no number.
#[must_use]
pub(crate) fn written_numbers(args: &[OsString]) -> Vec<u32> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match args.first().map(String::as_str) {
        Some("api") => args
            .iter()
            .skip(1)
            .filter_map(|a| api_path_number(a))
            .collect(),
        Some("issue" | "pr") => args
            .iter()
            .skip(2)
            .find_map(|a| selector_number(a))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn api_path_number(arg: &str) -> Option<u32> {
    let path = arg.split('?').next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let i = segs.iter().position(|s| *s == "repos")?;
    if !matches!(segs.get(i + 3).copied(), Some("issues" | "pulls")) {
        return None;
    }
    segs.get(i + 4)?.parse().ok()
}

fn selector_number(arg: &str) -> Option<u32> {
    if arg.starts_with('-') {
        return None;
    }
    if let Ok(n) = arg.trim_start_matches('#').parse() {
        return Some(n);
    }
    let segs: Vec<&str> = arg.split('/').collect();
    let i = segs
        .iter()
        .rposition(|s| matches!(*s, "issues" | "pull" | "pulls"))?;
    segs.get(i + 1)?.parse().ok()
}

type Store = HashMap<u32, Instant>;

#[cfg(not(test))]
fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static WRITES: OnceLock<Mutex<Store>> = OnceLock::new();
    let lock = WRITES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Test builds: per thread, so parallel tests never pin each other's reads.
/// Every facade execution runs on its caller's thread.
#[cfg(test)]
fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    thread_local! {
        static WRITES: std::cell::RefCell<Store> = std::cell::RefCell::new(HashMap::new());
    }
    WRITES.with(|w| f(&mut w.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn api_paths_name_the_issue_or_pr_they_write() {
        for (args, want) in [
            (&["api", "-X", "POST", "repos/acme/app/issues/12/labels"][..], vec![12]),
            (
                &[
                    "api",
                    "--method",
                    "DELETE",
                    "/repos/acme/app/issues/7/labels/x",
                ][..],
                vec![7],
            ),
            (&["api", "-X", "PATCH", "repos/acme/app/issues/9"][..], vec![9]),
            (
                &[
                    "api",
                    "-X",
                    "POST",
                    "repos/acme/app/issues/3/comments",
                    "-f",
                    "body=b",
                ][..],
                vec![3],
            ),
            (&["api", "-X", "PATCH", "repos/acme/app/pulls/40"][..], vec![40]),
            // A comment edited by id names no issue number.
            (&["api", "-X", "PATCH", "repos/acme/app/issues/comments/9911"][..], vec![]),
            (&["api", "graphql", "-f", "query=mutation{}"][..], vec![]),
        ] {
            assert_eq!(written_numbers(&argv(args)), want, "{args:?}");
        }
    }

    #[test]
    fn issue_and_pr_verbs_name_their_selector() {
        for (args, want) in [
            (&["issue", "edit", "12", "--add-label", "loom:building"][..], vec![12]),
            (&["issue", "edit", "--repo", "acme/app", "12"][..], vec![12]),
            (
                &[
                    "issue",
                    "comment",
                    "https://github.com/acme/app/issues/5",
                    "-b",
                    "x",
                ][..],
                vec![5],
            ),
            (&["pr", "edit", "#33", "--remove-label", "loom:pr"][..], vec![33]),
            (&["issue", "create", "--title", "t"][..], vec![]),
            (&["label", "create", "x"][..], vec![]),
        ] {
            assert_eq!(written_numbers(&argv(args)), want, "{args:?}");
        }
    }

    #[test]
    fn a_noted_write_pins_only_its_own_number_for_the_window() {
        note(&argv(&["issue", "edit", "4711", "--add-label", "loom:building"]));
        assert!(written_within(4711, Duration::from_secs(600)));
        assert!(!written_within(4712, Duration::from_secs(600)));
        assert!(!written_within(4711, Duration::ZERO), "outside the window it is not pinned");
    }
}
