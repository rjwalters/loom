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

/// Porcelain (`git status --porcelain`, v1) lines -> tracked paths: `??`
/// skipped, a rename reduced to its destination, and one surrounding quote
/// pair dropped.
///
/// The lines must be **untrimmed**: the path starts at byte 3, after the
/// two-column `XY` status and a space.
pub fn tracked_paths_from_porcelain<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut paths = Vec::new();
    for line in lines {
        if line.is_empty() || &line[..2.min(line.len())] == "??" {
            continue;
        }
        let mut path = line.get(3..).unwrap_or("").to_string();
        if let Some(idx) = path.find(" -> ") {
            path = path[idx + " -> ".len()..].to_string();
        }
        // `${path%\"}` then `${path#\"}` — one trailing quote, then one
        // leading quote, in that order.
        if path.ends_with('"') {
            path.pop();
        }
        if path.starts_with('"') {
            path.remove(0);
        }
        paths.push(path);
    }
    paths
}

/// `Some(paths)` iff `HEAD` is an ANCESTOR of `origin_ref`, at least one
/// tracked path is dirty, and EVERY dirty tracked path — in the working tree
/// AND in the index — already holds exactly its `origin_ref` content
/// (#11539).
///
/// `origin_ref` is the remote-tracking ref to compare with (`origin/<b>` or
/// `refs/remotes/origin/<b>`). `paths` are the dirty tracked paths (see
/// [`tracked_paths_from_porcelain`]). `git_ok(args)` runs `git <args>` in the
/// repo and answers whether it exited 0; the caller chooses the bound.
///
/// Safe by construction: after the fast-forward each path holds that exact
/// content anyway, so discarding the local edit loses nothing. Each guard is
/// load-bearing:
///
/// 1. **`--is-ancestor`.** On a diverged tree a discard would be followed by a
///    fast-forward that still fails, losing the edit for nothing. Kept even
///    for a caller that already established "behind only": it is cheap, and
///    it keeps the helper safe for any caller.
/// 2. **Working tree AND index.** `git diff origin/<b> -- <p>` compares the
///    working tree only. The index must hold origin's content or HEAD's
///    (nothing staged); a staged blob that differs from both is real local
///    work that `checkout HEAD --` would discard, so it is not vouched for.
/// 3. **Conjunctive, plain `--quiet`, no rename detection.** One mismatch
///    answers `None`; a mode-only difference counts as non-identical.
pub fn all_dirty_tracked_identical_to_origin(
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
    let identical = paths.iter().all(|p| {
        git_ok(&["diff", "--quiet", origin_ref, "--", p])
            && (git_ok(&["diff", "--cached", "--quiet", origin_ref, "--", p])
                || git_ok(&["diff", "--cached", "--quiet", "HEAD", "--", p]))
    });
    identical.then_some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_paths_drop_a_rename_arrow_and_one_quote_pair() {
        let lines = [
            " M README.md",
            "M  staged.txt",
            "?? untracked.txt",
            "R  old.txt -> new.txt",
            " M \"with space.txt\"",
            "",
        ];
        assert_eq!(
            tracked_paths_from_porcelain(lines),
            vec!["README.md", "staged.txt", "new.txt", "with space.txt"]
        );
    }

    #[test]
    fn no_dirty_paths_is_never_identical() {
        let mut calls = 0;
        let got = all_dirty_tracked_identical_to_origin("origin/main", Vec::new(), |_| {
            calls += 1;
            true
        });
        assert_eq!(got, None);
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_failing_runner_fails_closed() {
        let paths = vec!["a".to_string()];
        assert_eq!(all_dirty_tracked_identical_to_origin("origin/main", paths, |_| false), None);
    }

    #[test]
    fn index_may_match_head_instead_of_origin() {
        let paths = vec!["a".to_string()];
        // Everything succeeds except the index-vs-origin probe.
        let got = all_dirty_tracked_identical_to_origin("origin/main", paths, |a| {
            !(a.starts_with(&["diff", "--cached"]) && a.contains(&"origin/main"))
        });
        assert_eq!(got, Some(vec!["a".to_string()]));
    }
}
