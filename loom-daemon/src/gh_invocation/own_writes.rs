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
//! `--repo`/`-R` flag (a `HOST/owner/repo` or URL form is cut to its last two
//! segments), a URL selector, or the invocation's explicit target, lower-cased.
//! A write whose repo none of those name (one `gh` resolves from the working
//! directory, or an API path with `{owner}`/`{repo}` placeholders and no
//! target) is recorded repo-less, a wildcard that pins that number in every
//! repo. The parse is deliberately loose: for `gh issue|pr`, every
//! numeric-looking argument (`N`, `#N`, or a URL ending `/issues/N` or
//! `/pull/N`) is pinned unless it is the value of a flag that never takes a
//! number (`--repo`, `--body`, `--add-label`, …), so `--milestone 3 12` pins
//! both 3 and 12. A mis-parse can only add a pin, never drop one: one extra
//! unconditional read is the safe direction. The record is taken before the
//! write runs, so a write that fails still pins the next read.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::{Duration, Instant};

/// Entries older than this are dropped on the next write.
const KEEP: Duration = Duration::from_secs(3600);

/// `gh issue|pr <verb>` flags that take a value in **every** verb and whose
/// value is never an issue or PR number. Their spaced value is skipped; every
/// other numeric-looking argument is pinned. A flag that is boolean in some
/// verb (`-r`, `-c`, `-m`, `-d`, …) must never be listed here: its "value"
/// could be the selector, and skipping it would drop a pin.
const NUMBER_FREE_VALUE_FLAGS: &[&str] = &[
    "--add-assignee",
    "--add-label",
    "--add-project",
    "--add-reviewer",
    "--assignee",
    "--author-email",
    "--base",
    "--body",
    "--body-file",
    "--head",
    "--label",
    "--match-head-commit",
    "--remove-assignee",
    "--remove-label",
    "--remove-project",
    "--remove-reviewer",
    "--repo",
    "--subject",
    "--title",
    "-B",
    "-F",
    "-H",
    "-R",
    "-a",
    "-b",
    "-l",
    "-t",
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
/// `repos/{o}/{r}/(issues|pulls)/{n}[/…]`, or every numeric-looking argument
/// of `gh issue|pr <verb> …` that is not the value of a number-free flag.
/// Anything else (GraphQL, a comment edited by id, a repo-level write) names
/// no number. The repo is lower-cased; `None` (the wildcard) when neither the
/// argv nor `target` names it.
#[must_use]
pub(crate) fn written_targets(args: &[OsString], target: Option<&str>) -> Vec<Key> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let target = target.and_then(normalise_repo);
    let mut out: Vec<Key> = Vec::new();
    match args.first().map(String::as_str) {
        Some("api") => {
            for a in args.iter().skip(1) {
                if let Some(key) = api_path_target(a, target.as_deref()) {
                    out.push(key);
                }
            }
        }
        Some("issue" | "pr") => {
            let rest = args.get(2..).unwrap_or_default();
            let flag_repo = repo_flag(rest);
            for a in numeric_candidates(rest) {
                let Some(n) = selector_number(a) else {
                    continue;
                };
                let repo = flag_repo
                    .clone()
                    .or_else(|| url_repo(a))
                    .or_else(|| target.clone());
                out.push((repo, n));
            }
        }
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|k| seen.insert(k.clone()));
    out
}

fn api_path_target(arg: &str, target: Option<&str>) -> Option<Key> {
    let path = arg.split('?').next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let i = segs.iter().position(|s| *s == "repos")?;
    if !matches!(segs.get(i + 3).copied(), Some("issues" | "pulls")) {
        return None;
    }
    let n = segs.get(i + 4)?.parse().ok()?;
    // `{owner}`/`:owner` placeholders are filled in by `gh` from the working
    // directory; the literal never matches a real slug, so use the target or
    // fall back to the wildcard.
    let repo = slug(segs.get(i + 1)?, segs.get(i + 2)?).or_else(|| target.map(str::to_string));
    Some((repo, n))
}

/// Every argument that could be a number this write targets: all of them
/// except flags and the spaced value of a [`NUMBER_FREE_VALUE_FLAGS`] flag.
/// After `--` everything is a candidate.
fn numeric_candidates(rest: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            out.extend(it.by_ref());
            break;
        }
        if a.starts_with('-') && a != "-" {
            if NUMBER_FREE_VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
            continue;
        }
        out.push(a);
    }
    out
}

/// The normalised value of `--repo`/`-R`: spaced, `=`, or glued (`-Racme/app`).
fn repo_flag(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        let value = if a == "--repo" || a == "-R" {
            it.next().map(String::as_str)
        } else if let Some(v) = a.strip_prefix("--repo=") {
            Some(v)
        } else if let Some(v) = a.strip_prefix("-R") {
            Some(v.strip_prefix('=').unwrap_or(v))
        } else {
            continue;
        };
        return value.and_then(normalise_repo);
    }
    None
}

/// `owner/repo`, `HOST/owner/repo` or a repo URL, cut to its last two path
/// segments and lower-cased. `None` for anything shorter or a placeholder.
fn normalise_repo(raw: &str) -> Option<String> {
    let raw = raw.split(['?', '#']).next()?;
    let segs: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
    let [.., owner, repo] = segs.as_slice() else {
        return None;
    };
    slug(owner, repo.strip_suffix(".git").unwrap_or(repo))
}

/// `owner/repo`, lower-cased, unless either part is a `{…}`/`:…` placeholder.
fn slug(owner: &str, repo: &str) -> Option<String> {
    let placeholder = |s: &str| s.is_empty() || s.starts_with('{') || s.starts_with(':');
    if placeholder(owner) || placeholder(repo) {
        return None;
    }
    Some(format!("{owner}/{repo}").to_ascii_lowercase())
}

/// `owner/repo` of a `https://host/owner/repo/(issues|pull)/n` selector.
fn url_repo(selector: &str) -> Option<String> {
    let segs: Vec<&str> = selector.split('/').collect();
    let i = segs
        .iter()
        .rposition(|s| matches!(*s, "issues" | "pull" | "pulls"))?;
    if i < 2 {
        return None;
    }
    slug(segs[i - 2], segs[i - 1])
}

/// `N`, `#N`, or a URL whose `issues|pull|pulls` segment is followed by `N`.
fn selector_number(arg: &str) -> Option<u32> {
    if let Ok(n) = arg.strip_prefix('#').unwrap_or(arg).parse() {
        return Some(n);
    }
    let path = arg.split(['?', '#']).next()?;
    let segs: Vec<&str> = path.split('/').collect();
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
