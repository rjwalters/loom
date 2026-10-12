//! The one "every dirty tracked file is already identical to origin"
//! predicate (#11539, #11553), shared by the two fast-forward paths:
//!
//! * `daemon_update::sync` — `loom-daemon-update.sh`'s fast-forward of the
//!   Loom source checkout (unbounded git calls);
//! * `fleet_sync::checkout_ff` — the daemon's per-tick fast-forward of every
//!   registered main checkout (every git call timeout-bounded).
//!
//! The predicate is git-runner-agnostic: each caller hands in its own
//! `git_ok(args) -> bool`, so `checkout_ff` keeps its startup budget and
//! `sync` keeps its plain `git -C`. A runner that times out or errors must
//! answer `false`, which makes the predicate fail closed.
//!
//! # Paths are exact, never patterns
//!
//! The dirty paths come from `git status --porcelain -z` ([`STATUS_ARGS`],
//! parsed by [`tracked_paths_from_porcelain_z`]): NUL-separated and never
//! C-quoted, so a non-ASCII name or one with spaces or quotes round-trips
//! byte for byte. Every git command that is handed one of those paths runs
//! with [`LITERAL_PATHSPECS`], so `a[1].txt` names that file and never globs
//! onto `a1.txt`: the probes here and the discard ([`discard_args`]) alike.
//!
//! A path that matches nothing must never be vouched for (`git diff --quiet
//! <ref> -- <nothing>` exits 0), so each path must first exist in `HEAD`,
//! which is also exactly what the `checkout HEAD --` discard needs.
//!
//! # The filesystem is checked too, without following symlinks
//!
//! Git's tracked diff cannot see what is on disk where a tracked path used to
//! be. A tracked file deleted locally and upstream, with a directory of
//! untracked or ignored files now in its place, diffs as "deleted" and so
//! looks identical to origin; `checkout HEAD -- <path>` would then remove that
//! whole directory. Likewise an untracked file sitting on a path that was
//! `git rm --cached` is invisible to `git diff <ref>`, and the discard would
//! overwrite it. So [`worktree_entry_is_restorable`] (an `lstat` walk) and an
//! index-entry probe fail closed on anything but an absent path or a tracked
//! regular file or symlink under real directories.

/// The global option that makes every pathspec literal. Goes before the
/// subcommand.
pub const LITERAL_PATHSPECS: &str = "--literal-pathspecs";

use std::path::{Component, Path};

/// `git <STATUS_ARGS>`: the tracked changes, NUL-separated and unquoted, for
/// [`tracked_paths_from_porcelain_z`]. Takes no optional lock.
pub const STATUS_ARGS: [&str; 5] = [
    "--no-optional-locks",
    "status",
    "--porcelain",
    "-z",
    "--untracked-files=no",
];

/// `git status --porcelain -z` (v1) output -> tracked paths.
///
/// Each record is `XY<space><path>` and ends in NUL; a rename or copy (`R` or
/// `C` in either column) is followed by one more NUL field holding its
/// source path. Both paths are returned: the source of a rename changed too,
/// and it is only ever more conservative to verify one path more. `??` and
/// `!!` records are skipped.
///
/// `None` (fail closed) when a record is malformed or a path is not UTF-8:
/// a path that cannot be handed to git exactly is never vouched for.
pub fn tracked_paths_from_porcelain_z(raw: &[u8]) -> Option<Vec<String>> {
    let mut fields = raw.split(|b| *b == 0);
    let mut paths = Vec::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            // Only the split after the final NUL is empty.
            if fields.next().is_some() {
                return None;
            }
            break;
        }
        if record.len() < 4 || record[2] != b' ' {
            return None;
        }
        let (x, y) = (record[0], record[1]);
        let two_paths = matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C');
        let path = std::str::from_utf8(&record[3..]).ok()?.to_string();
        let source = if two_paths {
            let from = fields.next().filter(|f| !f.is_empty())?;
            Some(std::str::from_utf8(from).ok()?.to_string())
        } else {
            None
        };
        if (x, y) == (b'?', b'?') || (x, y) == (b'!', b'!') {
            continue;
        }
        paths.push(path);
        paths.extend(source);
    }
    Some(paths)
}

/// `git <discard_args(paths)>`: `checkout HEAD -- <paths>` with literal
/// pathspecs, the discard both callers run on paths
/// [`all_dirty_tracked_identical_to_origin`] vouched for.
pub fn discard_args(paths: &[String]) -> Vec<&str> {
    let mut args = vec![LITERAL_PATHSPECS, "checkout", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    args
}

/// `Some(paths)` iff `HEAD` is an ANCESTOR of `origin_ref`, at least one
/// tracked path is dirty, and EVERY dirty tracked path exists in `HEAD` and
/// — in the working tree AND in the index — already holds exactly its
/// `origin_ref` content (#11539).
///
/// `origin_ref` is the remote-tracking ref to compare with (`origin/<b>` or
/// `refs/remotes/origin/<b>`). `paths` are the dirty tracked paths, exact
/// (see [`tracked_paths_from_porcelain_z`]). `git_ok(args)` runs `git <args>`
/// in the repo and answers whether it exited 0; the caller chooses the bound.
///
/// What a discard of these paths costs: after a successful fast-forward each
/// path holds that exact content anyway. If the merge then fails (a file in
/// the way, a lock taken in between), the local edits are gone from the
/// working tree and the merge has not happened, but their content is still
/// in `origin_ref`, so it is recoverable and no local work is lost. Each
/// guard is load-bearing:
///
/// 1. **`--is-ancestor`.** On a diverged tree a discard would be followed by a
///    fast-forward that still fails, losing the edit for nothing. Kept even
///    for a caller that already established "behind only": it is cheap, and
///    it keeps the helper safe for any caller.
/// 2. **The path exists in `HEAD`.** `git diff --quiet <ref> -- <p>` exits 0
///    when `<p>` matches nothing, which would vouch for a path that was not
///    checked at all; and `checkout HEAD -- <p>` can only restore a path
///    `HEAD` has (so a rename's new name, absent from `HEAD`, stays dirty).
/// 3. **Working tree AND index.** `git diff origin/<b> -- <p>` compares the
///    working tree only. The index must hold origin's content or HEAD's
///    (nothing staged); a staged blob that differs from both is real local
///    work that `checkout HEAD --` would discard, so it is not vouched for.
/// 4. **Literal pathspecs, conjunctive, plain `--quiet`, no rename
///    detection.** One mismatch answers `None`; a mode-only difference
///    (including file <-> symlink) counts as non-identical.
/// 5. **The filesystem (#11558 P1).** `worktree` is the checkout's root. On
///    disk each path must be absent ([`worktree_entry_is_restorable`]), or a
///    regular file or symlink that is in the index, so the tree diff in (3)
///    actually saw it. A directory, any other type, a symlinked or
///    non-directory parent: not identical. `HEAD`'s entry must be a blob
///    (file or symlink), never a tree or gitlink.
pub fn all_dirty_tracked_identical_to_origin(
    worktree: &Path,
    origin_ref: &str,
    paths: Vec<String>,
    mut git_ok: impl FnMut(&[&str]) -> bool,
) -> Option<Vec<String>> {
    if paths.is_empty() {
        return None;
    }
    if !git_ok(&["merge-base", "--is-ancestor", "HEAD", origin_ref]) {
        return None;
    }
    let lit = LITERAL_PATHSPECS;
    let identical = paths.iter().all(|p| {
        let Some(on_disk) = worktree_entry_is_restorable(worktree, p) else {
            return false;
        };
        // `cat-file blob` fails for a tree or a gitlink; `:0:<p>` is an exact
        // index lookup (revision syntax, never a pathspec).
        git_ok(&["cat-file", "blob", &format!("HEAD:{p}")])
            && (on_disk == OnDisk::Absent || git_ok(&["cat-file", "-e", &format!(":0:{p}")]))
            && git_ok(&[lit, "diff", "--quiet", origin_ref, "--", p])
            && (git_ok(&[lit, "diff", "--cached", "--quiet", origin_ref, "--", p])
                || git_ok(&[lit, "diff", "--cached", "--quiet", "HEAD", "--", p]))
    });
    identical.then_some(paths)
}

/// What [`worktree_entry_is_restorable`] found at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDisk {
    /// Nothing there (a deletion): the discard creates, never removes.
    Absent,
    /// A regular file or a symlink (the link itself, never its target).
    FileOrSymlink,
}

/// `lstat` walk of `rel` under `worktree`, never following a symlink:
/// `Some(kind)` only when `checkout HEAD -- <rel>` can touch nothing but the
/// one entry git's diff already compared.
///
/// Every existing parent component must be a real directory (not a symlink,
/// not a file); the first absent component makes the whole path absent. The
/// final component must be absent, a regular file or a symlink. A directory
/// (the #11558 P1: untracked or ignored files where a deleted file was), any
/// other type, an unreadable entry, or a path that is not a plain relative
/// one (`..`, `.`, absolute, empty) answers `None`.
///
/// A symlink, dangling or not, is accepted here: the discard replaces the
/// link itself, and its target text is what the tree diff compared.
pub fn worktree_entry_is_restorable(worktree: &Path, rel: &str) -> Option<OnDisk> {
    let comps: Vec<Component> = Path::new(rel).components().collect();
    if rel.is_empty() || rel.ends_with('/') || comps.is_empty() {
        return None;
    }
    let mut cur = worktree.to_path_buf();
    for (i, comp) in comps.iter().enumerate() {
        let Component::Normal(name) = comp else {
            return None;
        };
        cur.push(name);
        let meta = match std::fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(OnDisk::Absent),
            Err(_) => return None,
        };
        let ft = meta.file_type();
        if i + 1 < comps.len() {
            if !ft.is_dir() {
                return None;
            }
        } else if ft.is_file() || ft.is_symlink() {
            return Some(OnDisk::FileOrSymlink);
        } else {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn porcelain_z_records_are_exact_and_renames_carry_two_paths() {
        let raw = b" M README.md\0M  staged.txt\0?? untracked.txt\0R  new.txt\0old.txt\0 M with space.txt\0 M \xc3\xa9.txt\0 M \"q\".txt\0";
        assert_eq!(
            tracked_paths_from_porcelain_z(raw),
            Some(
                [
                    "README.md",
                    "staged.txt",
                    "new.txt",
                    "old.txt",
                    "with space.txt",
                    "é.txt",
                    "\"q\".txt"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        assert_eq!(tracked_paths_from_porcelain_z(b""), Some(Vec::new()));
    }

    #[test]
    fn porcelain_z_fails_closed_on_malformed_or_non_utf8() {
        assert_eq!(tracked_paths_from_porcelain_z(b" M \xff.txt\0"), None);
        assert_eq!(tracked_paths_from_porcelain_z(b"R  new.txt\0"), None, "missing source");
        assert_eq!(tracked_paths_from_porcelain_z(b"XY\0"), None);
        assert_eq!(tracked_paths_from_porcelain_z(b" M a\0\0 M b\0"), None);
    }

    #[test]
    fn no_dirty_paths_is_never_identical() {
        let mut calls = 0;
        let got = all_dirty_tracked_identical_to_origin(
            Path::new("/nonexistent-11558"),
            "origin/main",
            Vec::new(),
            |_| {
                calls += 1;
                true
            },
        );
        assert_eq!(got, None);
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_failing_runner_fails_closed() {
        let paths = vec!["a".to_string()];
        assert_eq!(
            all_dirty_tracked_identical_to_origin(
                Path::new("/nonexistent-11558"),
                "origin/main",
                paths,
                |_| { false }
            ),
            None
        );
    }

    #[test]
    fn every_path_probe_is_literal() {
        let paths = vec!["a[1].txt".to_string()];
        let mut seen = Vec::new();
        all_dirty_tracked_identical_to_origin(
            Path::new("/nonexistent-11558"),
            "origin/main",
            paths,
            |a| {
                seen.push(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                true
            },
        );
        for argv in seen.iter().filter(|a| a.contains(&"a[1].txt".to_string())) {
            assert_eq!(argv[0], LITERAL_PATHSPECS, "{argv:?}");
        }
        assert_eq!(discard_args(&["a[1].txt".to_string()])[0], LITERAL_PATHSPECS);
    }

    #[test]
    fn index_may_match_head_instead_of_origin() {
        let paths = vec!["a".to_string()];
        // Everything succeeds except the index-vs-origin probe.
        let got = all_dirty_tracked_identical_to_origin(
            Path::new("/nonexistent-11558"),
            "origin/main",
            paths,
            |a| !(a.contains(&"--cached") && a.contains(&"origin/main")),
        );
        assert_eq!(got, Some(vec!["a".to_string()]));
    }

    // ── Real git ──────────────────────────────────────────────────────────

    fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = git_out(dir, args);
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo at commit 1 with `refs/remotes/origin/main` one commit ahead,
    /// where `files` hold `"two\n"` (each was `"one\n"` at commit 1).
    fn repo(files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        git(d, &["init", "--quiet", "--initial-branch=main"]);
        git(d, &["config", "user.name", "T"]);
        git(d, &["config", "user.email", "t@example.invalid"]);
        git(d, &["config", "commit.gpgsign", "false"]);
        for f in files {
            fs::write(d.join(f), "one\n").unwrap();
        }
        git(d, &["add", "-A"]);
        git(d, &["commit", "--quiet", "-m", "one"]);
        for f in files {
            fs::write(d.join(f), "two\n").unwrap();
        }
        git(d, &["add", "-A"]);
        git(d, &["commit", "--quiet", "-m", "two"]);
        git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(d, &["reset", "--quiet", "--hard", "HEAD~1"]);
        tmp
    }

    fn check(d: &Path, paths: Vec<String>) -> Option<Vec<String>> {
        all_dirty_tracked_identical_to_origin(d, "refs/remotes/origin/main", paths, |a| {
            git_out(d, a).status.success()
        })
    }

    fn dirty(d: &Path) -> Vec<String> {
        let out = git_out(d, &STATUS_ARGS);
        assert!(out.status.success());
        tracked_paths_from_porcelain_z(&out.stdout).unwrap()
    }

    #[test]
    fn git_ff_identical_non_ascii_path_round_trips() {
        let tmp = repo(&["é.txt"]);
        let d = tmp.path();
        fs::write(d.join("é.txt"), "two\n").unwrap();
        assert_eq!(dirty(d), vec!["é.txt".to_string()]);
        assert_eq!(check(d, dirty(d)), Some(vec!["é.txt".to_string()]));

        fs::write(d.join("é.txt"), "mine\n").unwrap();
        assert_eq!(check(d, dirty(d)), None, "a differing non-ASCII path is not vouched for");
    }

    #[test]
    fn git_ff_identical_glob_path_never_reaches_its_lookalike() {
        let tmp = repo(&["a[1].txt", "a1.txt"]);
        let d = tmp.path();
        fs::write(d.join("a[1].txt"), "two\n").unwrap();
        fs::write(d.join("a1.txt"), "mine\n").unwrap();
        assert_eq!(check(d, dirty(d)), None);

        // a1.txt dirtied after status (the TOCTOU window): only a[1].txt is
        // vouched for, and the discard leaves a1.txt alone.
        let vouched = check(d, vec!["a[1].txt".to_string()]).unwrap();
        git(d, &discard_args(&vouched));
        assert_eq!(fs::read_to_string(d.join("a1.txt")).unwrap(), "mine\n");
        assert_eq!(fs::read_to_string(d.join("a[1].txt")).unwrap(), "one\n");
    }

    #[test]
    fn git_ff_identical_a_path_that_matches_nothing_fails_closed() {
        let tmp = repo(&["x.txt"]);
        let d = tmp.path();
        fs::write(d.join("x.txt"), "two\n").unwrap();
        // The old C-quoted form of a non-ASCII name: matches no file.
        assert_eq!(check(d, vec!["\\303\\251.txt".to_string()]), None);
        assert_eq!(check(d, vec!["x.txt".to_string(), "nope.txt".to_string()]), None);
    }

    #[test]
    fn git_ff_identical_rename_stays_not_identical() {
        let tmp = repo(&["r.txt"]);
        let d = tmp.path();
        git(d, &["mv", "r.txt", "moved.txt"]);
        let paths = dirty(d);
        assert_eq!(paths, vec!["moved.txt".to_string(), "r.txt".to_string()]);
        assert_eq!(check(d, paths), None, "moved.txt is not in HEAD");
    }

    #[cfg(unix)]
    #[test]
    fn git_ff_identical_worktree_entry_lstat_walk_fails_closed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::create_dir_all(d.join("dir/sub")).unwrap();
        fs::write(d.join("file"), "x").unwrap();
        fs::write(d.join("dir/f"), "x").unwrap();
        std::os::unix::fs::symlink("dir", d.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere", d.join("dangling")).unwrap();
        let at = |p: &str| worktree_entry_is_restorable(d, p);

        assert_eq!(at("gone"), Some(OnDisk::Absent));
        assert_eq!(at("gone/deeper"), Some(OnDisk::Absent));
        assert_eq!(at("file"), Some(OnDisk::FileOrSymlink));
        assert_eq!(at("dir/f"), Some(OnDisk::FileOrSymlink));
        assert_eq!(at("dangling"), Some(OnDisk::FileOrSymlink), "the link itself");
        assert_eq!(at("dir"), None, "a directory");
        assert_eq!(at("dir/sub"), None);
        assert_eq!(at("link/f"), None, "a symlinked parent is never followed");
        assert_eq!(at("file/x"), None, "a non-directory parent");
        for bad in ["", "../x", "./file", "/etc/passwd", "dir/../file", "file/"] {
            assert_eq!(at(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn git_ff_identical_deleted_path_now_a_directory_is_not_vouched_for() {
        let tmp = repo(&["victim"]);
        let d = tmp.path();
        fs::remove_file(d.join("victim")).unwrap();
        fs::create_dir(d.join("victim")).unwrap();
        fs::write(d.join("victim/local-only.txt"), "precious\n").unwrap();
        assert_eq!(check(d, vec!["victim".to_string()]), None);
        assert!(d.join("victim/local-only.txt").exists());
    }
}
