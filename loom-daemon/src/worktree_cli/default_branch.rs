//! `lib/default-branch.sh`'s `loom_default_branch`, in Rust (#8195 slice 3).
//!
//! `worktree.sh remove` needs this for three separate decisions, all of them
//! on the branch-delete path: the "never delete the default branch"
//! belt-and-suspenders guard, the default-branch tip that
//! [`super::branch_landed`]'s ancestry and tree-equality rungs compare
//! against, and the branch the #5015 primary-checkout auto-cleanup switches
//! to before force-deleting.
//!
//! # Why the whole ladder, and not [`crate::worktree_ops::clean::default_branch`]
//!
//! That function is rung 2 alone — `git symbolic-ref refs/remotes/origin/HEAD`
//! — which is unset in a freshly-`git init`ed clone that has only ever had a
//! remote added and pushed to. That is not a hypothetical: it is exactly the
//! shape of every throwaway-repo fixture in
//! `defaults/scripts/tests/test-worktree-*.sh`, and on `master`-default repos
//! it is the shape #3549 was filed about. Using the narrow probe here would
//! resolve no default branch at all in precisely the cases the retained suites
//! exercise, silently disabling the guard and the force-delete upgrade.
//!
//! # Hard-fail, never "main"
//!
//! [`resolve`] returns `None` rather than guessing `main`, for the reason the
//! shell spells out at its own rung 5: a silent wrong default reintroduces the
//! bug class the helper exists to fix. Callers treat `None` as "unknown" and
//! take the conservative arm (no force-delete upgrade, no auto-cleanup).

use std::path::Path;
use std::process::Command;

/// Resolve `repo`'s default branch name (`main`, `master`, …).
///
/// Ladder, first match wins — the shell's, step for step:
///
/// 1. `LOOM_DEFAULT_BRANCH` — explicit escape hatch and test seam.
/// 2. `git symbolic-ref --short refs/remotes/<remote>/HEAD` — offline.
/// 3. `git ls-remote --symref <remote> HEAD` — network (or a local bare
///    remote, which is what the retained suites' fixtures use).
/// 4. Local probe of `refs/remotes/<remote>/main` then `.../master`.
/// 5. `None`.
#[must_use]
pub fn resolve(repo: &Path) -> Option<String> {
    resolve_for_remote(repo, "origin")
}

/// [`resolve`] against an explicit remote name.
#[must_use]
pub fn resolve_for_remote(repo: &Path, remote: &str) -> Option<String> {
    if let Ok(explicit) = std::env::var("LOOM_DEFAULT_BRANCH") {
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }

    if let Some(sref) = git_stdout(
        repo,
        &[
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    ) {
        return Some(
            sref.strip_prefix(&format!("{remote}/"))
                .unwrap_or(&sref)
                .to_string(),
        );
    }

    if let Some(out) = git_stdout(repo, &["ls-remote", "--symref", remote, "HEAD"]) {
        // `ref: refs/heads/main\tHEAD` — the shell takes field 2 of the first
        // `^ref:` line.
        if let Some(name) = out
            .lines()
            .find(|l| l.starts_with("ref:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|r| r.strip_prefix("refs/heads/"))
        {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    for candidate in ["main", "master"] {
        if Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{remote}/{candidate}"),
            ])
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some(candidate.to_string());
        }
    }

    None
}

fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rung 5 is `None`, not `"main"`. A guessed default is what #3549 was
    /// filed about, and here it would additionally hand the branch-delete
    /// path a force-delete upgrade it has no evidence for.
    #[test]
    fn an_unresolvable_repo_is_none_not_main() {
        // The env override is checked first, so this only proves rung 5 when
        // the seam is unset.
        if std::env::var_os("LOOM_DEFAULT_BRANCH").is_some() {
            return;
        }
        assert_eq!(resolve(Path::new("/nonexistent-repo-for-unit-test")), None);
    }

    /// The ladder must be the shell's ladder. Pinned against the real
    /// `lib/default-branch.sh` rather than against a copy of its comments, so
    /// a reordering there fails here.
    #[test]
    fn ladder_matches_the_shell_twin() {
        let lib =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/lib/default-branch.sh");
        let Ok(raw) = std::fs::read_to_string(&lib) else {
            return; // not a full checkout
        };
        // Index the CODE, not the prose. `lib/default-branch.sh` opens with a
        // header comment that names all four rungs in order, so searching the
        // raw text found the comment for rung 2 *above* the code for rung 1
        // and the ordering assertion below failed against a file that was
        // perfectly in order. Comment lines are blanked (rather than dropped)
        // so every surviving offset is still a real position in the file.
        let sh: String = raw
            .lines()
            .map(|line| {
                if line.trim_start().starts_with('#') {
                    String::new()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let idx = |needle: &str| {
            sh.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {lib:?} (comments stripped)"))
        };
        let env_tier = idx("LOOM_DEFAULT_BRANCH:-");
        let symref = idx("git symbolic-ref --short");
        let lsremote = idx("git ls-remote --symref");
        let probe = idx("for candidate in main master");
        assert!(
            env_tier < symref && symref < lsremote && lsremote < probe,
            "lib/default-branch.sh reordered its ladder; resolve() must follow"
        );
        assert!(
            !sh.contains("echo main\n"),
            "the shell must never silently default to main; neither does resolve()"
        );
    }
}
