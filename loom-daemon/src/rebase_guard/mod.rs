//! `loom-daemon forge rebase-guard --base <ref> [--old <ref>] [--new <ref>]`
//! (Issue #9282) — would force-pushing `<new>` over `<old>` discard work that
//! nothing else will report?
//!
//! # Why this exists
//!
//! Eight role paths rewrite a PR branch with `git rebase` and then
//! `git push --force-with-lease`. `--force-with-lease` protects against a push
//! the caller never saw; it does nothing about discarding a head the caller
//! *did* fetch. Two losses were possible, both with a clean tree and green CI:
//!
//! 1. **A discarded resolution.** A plain rebase flattens merge commits, so
//!    whatever lived only in a merge's hand resolution is dropped or
//!    re-resolved by someone without the original reasoning. A conflict
//!    resolution is knowledge, and the rebase throws it away.
//! 2. **An emptied payload** (#8298, #9001). A `merge=ours` driver makes the
//!    replayed commit empty, the rebased tree equals the base, and the push
//!    publishes a branch with nothing in it.
//!
//! # What it reads, and what it never does
//!
//! Local git objects only: no fetch, no forge call, no ref update, no index or
//! working-tree write. `merge-tree --write-tree` and `--remerge-diff` both
//! create objects, so every git child runs with `GIT_OBJECT_DIRECTORY`
//! pointed at a private temp directory (the repository's own object store is
//! the read-only alternate). Nothing is added to the repository.
//!
//! # The two findings
//!
//! - `EMPTIED`: `<new>`'s tree equals `<base>`'s, although **merging `<base>`
//!   into `<old>`** would conflict or produce a tree that differs from
//!   `<base>`'s. The test is on content, not on patch history: a commit that
//!   was dropped because the base still carries the identical change lost
//!   nothing (the merge yields the base's tree), so it is clean, and refusing
//!   it would strand a branch whose only possible rebase result is the same
//!   empty one. A change that landed in the base and was then **reverted**
//!   there is not in the base, whatever `git rebase` skipped as "already
//!   applied", so it is `EMPTIED`.
//! - `RESOLUTION_DISCARDED <merge-sha>`: a merge commit reachable from `<old>`
//!   but from neither `<base>` nor `<new>` has a non-empty `--remerge-diff`
//!   (a hand resolution, not a mechanical merge), and `<new>` does not carry
//!   that resolution. "Carries" is measured against what **merging `<base>`
//!   into `<old>`** would produce, restricted to the paths the resolution
//!   touched, so a base that moved one of those paths cleanly is not mistaken
//!   for a discard. A resolution path that conflicts in that merge counts as
//!   discarded: the rebase necessarily re-resolved it without the merge. So
//!   does a path the merge commit holds that `<new>` no longer has under that
//!   name (the base, or a later branch commit, renamed it), unless `<new>`'s
//!   whole tree is exactly that merge result: a comparison by path cannot
//!   follow the rename, and what cannot be shown to be carried is not safe.
//!
//! # Exit-code contract (same shape as `forge check-claim`)
//!
//! | Exit | Meaning | stdout |
//! |---|---|---|
//! | `0` | BLOCKED: do not push | `EMPTIED` or `RESOLUTION_DISCARDED <sha>` |
//! | [`EX_SAFE`] (1) | verified safe | empty |
//! | [`EX_UNANSWERED`] (5) | no verdict (unresolvable ref, git error, git too old) | empty |
//!
//! `0` is the blocked state so the unsafe state is the one a bare `if` fires
//! on. **Unanswered does not block** at the call sites, by design: a role
//! prompt newer than the installed binary (clap answers 2), or a missing
//! binary (the shell answers 127), must not strand every rebase in the fleet.
//! Every caller prints a loud warning instead and pushes unchecked.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(test)]
mod callsite_tests;
#[cfg(test)]
mod tests;

/// Exit code for a verified "nothing is discarded".
pub const EX_SAFE: i32 = 1;
/// Exit code for "the question could not be answered". Non-blocking.
pub const EX_UNANSWERED: i32 = 5;

/// More merge commits than this in `<old>` outside `<base>` and `<new>` means
/// the refs are not a PR branch and its base; answer nothing rather than run
/// an unbounded number of re-merges.
const MAX_MERGES: usize = 64;
/// How many pathspecs one `git diff` invocation is handed.
const PATH_CHUNK: usize = 200;

/// The default `--old`: the head a bare `git push` would replace.
pub const DEFAULT_OLD: &str = "@{push}";

/// `forge rebase-guard` arguments.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The base the branch was rebased onto (for example `origin/main`).
    #[arg(long, value_name = "REF")]
    pub base: String,

    /// The remote head the push would replace. Default: the current branch's
    /// push destination as last fetched, else `origin/<current branch>`.
    #[arg(long, value_name = "REF", default_value = DEFAULT_OLD)]
    pub old: String,

    /// The local head about to be pushed.
    #[arg(long, value_name = "REF", default_value = "HEAD")]
    pub new: String,
}

/// What replacing `<old>` with `<new>` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing is discarded. The string says why, for the log.
    Clean(&'static str),
    /// `<new>` equals `<base>`; `commit` is a payload commit that vanished
    /// (`<old>` itself when no single non-merge commit can be named).
    Emptied { commit: String },
    /// `merge` carried a hand resolution over `paths` that `<new>` lacks.
    ResolutionDiscarded { merge: String, paths: Vec<String> },
    /// No verdict; the string is the reason.
    Unanswered(String),
}

/// One git invocation's outcome: the exit code and stdout, or why it could
/// not run at all.
struct Out {
    code: i32,
    stdout: String,
}

/// A repository plus the environment every git child runs under.
struct Git<'a> {
    repo: &'a Path,
    env: Vec<(String, String)>,
}

impl Git<'_> {
    fn run(&self, args: &[&str]) -> Result<Out, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.repo)
            .args(["-c", "core.quotepath=off"])
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("could not run git: {e}"))?;
        Ok(Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    /// Stdout of a git command that must exit 0.
    fn ok(&self, args: &[&str]) -> Result<String, String> {
        let out = self.run(args)?;
        if out.code == 0 {
            Ok(out.stdout)
        } else {
            Err(format!("`git {}` exited {}", args.join(" "), out.code))
        }
    }

    /// A yes/no git question: exit 0 is yes, 1 is no, anything else an error.
    fn ask(&self, args: &[&str]) -> Result<bool, String> {
        match self.run(args)?.code {
            0 => Ok(true),
            1 => Ok(false),
            code => Err(format!("`git {}` exited {code}", args.join(" "))),
        }
    }

    fn commit(&self, flag: &str, rev: &str) -> Result<String, String> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])?;
        let sha = out.stdout.trim();
        if out.code == 0 && !sha.is_empty() {
            Ok(sha.to_string())
        } else {
            Err(format!("{flag} `{rev}` does not name a commit in this repository"))
        }
    }

    fn tree(&self, commit: &str) -> Result<String, String> {
        Ok(self
            .ok(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?
            .trim()
            .to_string())
    }
}

fn nul_list(s: &str) -> Vec<String> {
    s.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Answer for `repo` with the process's own environment.
pub fn assess(repo: &Path, base: &str, old: &str, new: &str) -> Verdict {
    assess_with(repo, &[], base, old, new)
}

/// [`assess`], with extra environment for every git child. Tests use it to
/// isolate git from the host's configuration.
pub fn assess_with(
    repo: &Path,
    extra_env: &[(&str, &str)],
    base: &str,
    old: &str,
    new: &str,
) -> Verdict {
    match decide(repo, extra_env, base, old, new) {
        Ok(v) => v,
        Err(reason) => Verdict::Unanswered(reason),
    }
}

/// The quarantine: git children write new objects into `scratch`, and read
/// the repository's real object store as an alternate.
fn quarantine(plain: &Git<'_>, scratch: &Path) -> Result<Vec<(String, String)>, String> {
    let objects = plain.ok(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "objects",
    ])?;
    let objects = PathBuf::from(objects.trim_end_matches('\n'));
    let mut alternates = objects.to_string_lossy().into_owned();
    if !objects.is_dir() || alternates.contains(':') || alternates.starts_with('"') {
        return Err(format!("cannot quarantine object writes for `{alternates}`"));
    }
    if let Ok(existing) = std::env::var("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        if !existing.is_empty() {
            alternates = format!("{alternates}:{existing}");
        }
    }
    Ok(vec![
        ("GIT_OBJECT_DIRECTORY".to_string(), scratch.to_string_lossy().into_owned()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES".to_string(), alternates),
    ])
}

fn decide(
    repo: &Path,
    extra_env: &[(&str, &str)],
    base: &str,
    old: &str,
    new: &str,
) -> Result<Verdict, String> {
    let caller: Vec<(String, String)> = extra_env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let plain = Git {
        repo,
        env: caller.clone(),
    };
    let scratch = tempfile::tempdir().map_err(|e| format!("no scratch directory: {e}"))?;
    let mut env = caller;
    env.extend(quarantine(&plain, scratch.path())?);
    // Pathspecs are relative to the directory git runs in, and every path this
    // module hands back to git is relative to the top of the tree. Run from
    // the top, or a call from a subdirectory compares nothing and answers
    // "safe". A bare repository has no top level and no prefix either.
    let top = plain
        .ok(&["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .ok()
        .map(|t| PathBuf::from(t.trim_end_matches('\n')))
        .filter(|t| t.is_dir());
    let git = Git {
        repo: top.as_deref().unwrap_or(repo),
        env,
    };

    let base = git.commit("--base", base)?;
    let old = match git.commit("--old", old) {
        Ok(sha) => sha,
        // A branch whose upstream is the base (how `worktree.sh` creates one)
        // has no `@{push}` even after it was pushed. Never fall back to the
        // upstream itself: that would compare the base with the base.
        Err(unresolved) if old == DEFAULT_OLD => {
            let branch = git.ok(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
            let tracking = branch.map(|b| format!("refs/remotes/origin/{}", b.trim()));
            tracking
                .and_then(|r| git.commit("--old", &r))
                .map_err(|_| {
                    format!("{unresolved} (and the current branch has no `origin/` ref)")
                })?
        }
        Err(unresolved) => return Err(unresolved),
    };
    let new = git.commit("--new", new)?;

    if old == new {
        return Ok(Verdict::Clean("the push changes nothing"));
    }
    if git.ask(&["merge-base", "--is-ancestor", &old, &new])? {
        return Ok(Verdict::Clean("fast-forward: the replaced head stays in history"));
    }
    if let Some(commit) = emptied(&git, &base, &old, &new)? {
        return Ok(Verdict::Emptied { commit });
    }
    if let Some((merge, paths)) = discarded_resolution(&git, &base, &old, &new)? {
        return Ok(Verdict::ResolutionDiscarded { merge, paths });
    }
    Ok(Verdict::Clean("no payload emptied and no hand resolution dropped"))
}

/// The #8298 predicate, decided on content: `<new>` is tree-identical to
/// `<base>` although merging `<base>` into `<old>` would not be.
///
/// Patch-equivalence (`rev-list --cherry-pick`) is deliberately not consulted:
/// it says a patch once landed in the base, not that the base still holds it.
fn emptied(git: &Git<'_>, base: &str, old: &str, new: &str) -> Result<Option<String>, String> {
    let base_tree = git.tree(base)?;
    if git.tree(new)? != base_tree {
        return Ok(None);
    }
    // When the merge cannot be computed the reference is `<old>`'s own tree,
    // so anything `<old>` holds that the base does not is reported: blocked,
    // never "safe".
    let (merged, conflicted) = merge_reference(git, old, base)?;
    if conflicted.is_empty() && merged == base_tree {
        return Ok(None);
    }
    // Name a payload commit for the explanation: the newest non-merge commit
    // of the branch that changes anything.
    let range = format!("{base}..{old}");
    let ahead = git.ok(&["rev-list", "--no-merges", &range])?;
    for commit in ahead.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parent = format!("{commit}^");
        // A root commit has no parent to diff against; it is payload.
        let has_parent = git
            .run(&["rev-parse", "--verify", "--quiet", &parent])?
            .code
            == 0;
        if !has_parent || !git.ask(&["diff", "--quiet", &parent, commit])? {
            return Ok(Some(commit.to_string()));
        }
    }
    Ok(Some(old.to_string()))
}

/// The tree merging `<base>` into `<old>` would produce, and the paths that
/// merge conflicts on. When the merge cannot be computed at all, `<old>`'s own
/// tree is the reference.
fn merge_reference(git: &Git<'_>, old: &str, base: &str) -> Result<(String, Vec<String>), String> {
    let out = git.run(&[
        "merge-tree",
        "--write-tree",
        "--name-only",
        "-z",
        "--no-messages",
        old,
        base,
    ])?;
    if out.code == 0 || out.code == 1 {
        let mut parts = nul_list(&out.stdout).into_iter();
        if let Some(tree) = parts.next() {
            return Ok((tree, parts.collect()));
        }
    }
    Ok((git.tree(old)?, Vec::new()))
}

/// Which of `paths` exist, under exactly that name, in `treeish`.
fn present(git: &Git<'_>, treeish: &str, paths: &[String]) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for chunk in paths.chunks(PATH_CHUNK) {
        let mut args = vec![
            "ls-tree",
            "-r",
            "--full-tree",
            "--name-only",
            "-z",
            treeish,
            "--",
        ];
        args.extend(chunk.iter().map(String::as_str));
        found.extend(nul_list(&git.ok(&args)?));
    }
    Ok(found)
}

fn discarded_resolution(
    git: &Git<'_>,
    base: &str,
    old: &str,
    new: &str,
) -> Result<Option<(String, Vec<String>)>, String> {
    let not_base = format!("^{base}");
    let not_new = format!("^{new}");
    let merges = git.ok(&["rev-list", "--merges", old, &not_base, &not_new])?;
    let merges: Vec<&str> = merges
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if merges.is_empty() {
        return Ok(None);
    }
    if merges.len() > MAX_MERGES {
        return Err(format!(
            "{} merge commits would be replaced (limit {MAX_MERGES}); --base/--old do not look \
             like a PR branch and its base",
            merges.len()
        ));
    }

    let mut reference: Option<(String, Vec<String>)> = None;
    for merge in merges {
        // Non-empty only when the recorded merge differs from what git would
        // have produced mechanically: a hand resolution.
        let touched = nul_list(&git.ok(&[
            "show",
            "--remerge-diff",
            "--no-renames",
            "--format=",
            "--name-only",
            "-z",
            merge,
        ])?);
        if touched.is_empty() {
            continue;
        }
        if reference.is_none() {
            reference = Some(merge_reference(git, old, base)?);
        }
        let Some((tree, conflicted)) = reference.as_ref() else {
            continue;
        };
        let mut lost: Vec<String> = touched
            .iter()
            .filter(|p| conflicted.contains(p))
            .cloned()
            .collect();
        for chunk in touched.chunks(PATH_CHUNK) {
            let mut args = vec![
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                tree.as_str(),
                new,
                "--",
            ];
            args.extend(chunk.iter().map(String::as_str));
            for path in nul_list(&git.ok(&args)?) {
                if !lost.contains(&path) {
                    lost.push(path);
                }
            }
        }
        // A path the merge commit holds but `<new>` does not have under that
        // name was renamed or removed on the way (by the base, or by a later
        // branch commit), and the comparison above cannot see through that:
        // when the reference lacks the name too, it reports nothing. Such a
        // path is carried only if `<new>` is exactly the merge result.
        let in_merge = present(git, merge, &touched)?;
        let in_new = present(git, new, &touched)?;
        if in_merge.iter().any(|p| !in_new.contains(p)) && git.tree(new)? != *tree {
            for path in in_merge {
                if !in_new.contains(&path) && !lost.contains(&path) {
                    lost.push(path);
                }
            }
        }
        if !lost.is_empty() {
            lost.sort();
            return Ok(Some((merge.to_string(), lost)));
        }
    }
    Ok(None)
}

/// The exit code, the stdout token (if any) and the stderr explanation for a
/// verdict. Pure, so the wording is testable.
pub fn render(v: &Verdict, args: &Args) -> (i32, Option<String>, String) {
    let Args { base, old, new } = args;
    match v {
        Verdict::Clean(why) => (EX_SAFE, None, format!("rebase-guard: safe to push ({why}).")),
        Verdict::Emptied { commit } => (
            0,
            Some("EMPTIED".to_string()),
            format!(
                "BLOCKER: rebase-guard (#9282): do NOT push. `{new}` has the same tree as \
                 `{base}`, but `{old}` carried commit {commit}, whose change is not in `{base}`. \
                 The rebase emptied it (a `merge=ours` path, or a replay that resolved to \
                 nothing), so the push would publish a PR with no content.\nFix: leave the \
                 remote head as it is and report this on the PR. Reapply the lost change on top \
                 of `{base}` as a new commit instead of pushing the emptied branch; if `{base}` \
                 already holds this work in another form (a squash-merge, say), close the PR \
                 rather than push an empty branch."
            ),
        ),
        Verdict::ResolutionDiscarded { merge, paths } => (
            0,
            Some(format!("RESOLUTION_DISCARDED {merge}")),
            format!(
                "BLOCKER: rebase-guard (#9282): do NOT push. `{old}` contains merge commit \
                 {merge}, whose conflict resolution was made by hand, and `{new}` does not \
                 carry it in: {}. A conflict resolution is knowledge and a rebase throws it \
                 away: both results can be clean and CI-green while only one is correct.\nFix: \
                 put the branch back at `{old}` and merge `{base}` into it instead of rebasing \
                 (the merge commit stays in history, the push is a fast-forward, and this check \
                 passes), or carry the resolution forward by hand and say so on the PR. Inspect \
                 it with: git show --remerge-diff {merge}",
                paths.join(", ")
            ),
        ),
        Verdict::Unanswered(reason) => (
            EX_UNANSWERED,
            None,
            format!(
                "WARNING: rebase-guard (#9282) could NOT answer: {reason}. This is not an \
                 all-clear: nothing checked whether replacing `{old}` with `{new}` discards a \
                 hand-made conflict resolution or empties the PR."
            ),
        ),
    }
}

/// `loom-daemon forge rebase-guard` entry point. Never returns.
pub fn cli_entrypoint(args: &Args) -> ! {
    let verdict = match std::env::current_dir() {
        Ok(cwd) => assess(&cwd, &args.base, &args.old, &args.new),
        Err(e) => Verdict::Unanswered(format!("no working directory: {e}")),
    };
    let (code, token, explanation) = render(&verdict, args);
    if let Some(token) = token {
        println!("{token}");
    }
    eprintln!("{explanation}");
    std::process::exit(code)
}
