//! Issue and PR numbers this process has just written through the facade
//! (W9 of the forge API reduction plan).
//!
//! The dispatch guards read `repos/{o}/{r}/issues/{n}` conditionally
//! (`sweep_registry::guards::issue_snapshot`). A read that follows this
//! daemon's own write to the same issue must not send an `If-None-Match` a
//! lagging replica could answer `304`, and the 2.7 label read must not ride a
//! reader App, which may lag the write (W4-C). Hooking every write site by
//! hand drifts: issue writes live in the guards, the claim label helpers,
//! quarantine, restore, the PR-less hold, claim reconciliation and more. So
//! [`super::GhInvocation::execute`] records **every** write-intent invocation
//! here, and the guard asks [`written_within`].
//!
//! Keyed by `(repo, number)`. The repo comes from the API path, the
//! `--repo`/`-R` flag, a URL selector, or the invocation's explicit target.
//! A write whose repo none of those name (one `gh` resolves from the working
//! directory) is recorded repo-less and pins that number in every repo: one
//! extra unconditional read is the safe direction. The record is taken before
//! the write runs, so a write that fails still pins the next read.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::{Duration, Instant};

/// Entries older than this are dropped on the next write.
const KEEP: Duration = Duration::from_secs(3600);

/// `gh issue|pr <verb>` flags that take no value. Every other flag written
/// without `=` consumes the next argument, so the selector is found
/// positionally (`issue edit --milestone 3 12` writes #12, not #3).
const BOOL_FLAGS: &[&str] = &[
    "--admin",
    "--auto",
    "--comments",
    "--create-if-none",
    "--delete-branch",
    "--delete-last",
    "--disable-auto",
    "--draft",
    "--edit-last",
    "--force",
    "--merge",
    "--rebase",
    "--remove-milestone",
    "--squash",
    "--undo",
    "--web",
    "--yes",
    "-c",
    "-d",
    "-m",
    "-r",
    "-s",
    "-w",
    "-y",
];

/// One recorded write: the repo slug (lower-cased) when known, and the number.
type Key = (Option<String>, u32);

/// Record the issue/PR numbers `args` (a `gh` argv) writes. `target` is the
/// invocation's explicit `owner/repo`, used when the argv names none.
pub(crate) fn note(args: &[OsString], target: Option<&str>) {
    let written = written_targets(args, target);
    if written.is_empty() {
        return;
    }
    let now = Instant::now();
    with(|m| {
        m.retain(|_, at| now.duration_since(*at) < KEEP);
        for key in written {
            m.insert(key, now);
        }
    });
}

/// Whether this process wrote number `n` of `repo` (`owner/repo`) within the
/// last `window`. A repo-less record of `n` counts for every repo.
#[must_use]
pub(crate) fn written_within(repo: &str, n: u32, window: Duration) -> bool {
    let fresh = |m: &Store, key: &Key| m.get(key).is_some_and(|at| at.elapsed() < window);
    with(|m| fresh(m, &(Some(repo.to_ascii_lowercase()), n)) || fresh(m, &(None, n)))
}

/// The `(repo, number)` pairs a write argv targets: a `gh api` path
/// `repos/{o}/{r}/(issues|pulls)/{n}[/…]`, or the selector of
/// `gh issue|pr <verb> <n|url>`. Anything else (GraphQL, a comment edited by
/// id, a repo-level write) names no number. The repo is lower-cased; `None`
/// when neither the argv nor `target` names it.
#[must_use]
pub(crate) fn written_targets(args: &[OsString], target: Option<&str>) -> Vec<Key> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let target = target.map(str::to_ascii_lowercase);
    match args.first().map(String::as_str) {
        Some("api") => args
            .iter()
            .skip(1)
            .filter_map(|a| api_path_target(a))
            .collect(),
        Some("issue" | "pr") => {
            let rest = args.get(2..).unwrap_or_default();
            let Some(selector) = positional_selector(rest)
                .or_else(|| rest.iter().find(|a| selector_number(a).is_some()))
            else {
                return Vec::new();
            };
            let Some(n) = selector_number(selector) else {
                return Vec::new();
            };
            let repo = repo_flag(rest)
                .or_else(|| url_repo(selector))
                .or(target)
                .map(|r| r.to_ascii_lowercase());
            vec![(repo, n)]
        }
        _ => Vec::new(),
    }
}

fn api_path_target(arg: &str) -> Option<Key> {
    let path = arg.split('?').next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let i = segs.iter().position(|s| *s == "repos")?;
    if !matches!(segs.get(i + 3).copied(), Some("issues" | "pulls")) {
        return None;
    }
    let n = segs.get(i + 4)?.parse().ok()?;
    let repo = format!("{}/{}", segs.get(i + 1)?, segs.get(i + 2)?).to_ascii_lowercase();
    Some((Some(repo), n))
}

/// The first positional argument after the verb, skipping flags and the
/// values of flags that take one.
fn positional_selector(rest: &[String]) -> Option<&String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return it.next();
        }
        if !a.starts_with('-') || a == "-" {
            return Some(a);
        }
        if !a.contains('=') && !BOOL_FLAGS.contains(&a.as_str()) {
            it.next();
        }
    }
    None
}

/// The value of `--repo`/`-R`, in either the spaced or the `=` form.
fn repo_flag(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--repo" || a == "-R" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--repo=").or_else(|| a.strip_prefix("-R=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// `owner/repo` of a `https://host/owner/repo/(issues|pull)/n` selector.
fn url_repo(selector: &str) -> Option<String> {
    let segs: Vec<&str> = selector.split('/').collect();
    let i = segs
        .iter()
        .rposition(|s| matches!(*s, "issues" | "pull" | "pulls"))?;
    (i >= 2).then(|| format!("{}/{}", segs[i - 2], segs[i - 1]))
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

type Store = HashMap<Key, Instant>;

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
#[path = "own_writes_tests.rs"]
mod tests;
