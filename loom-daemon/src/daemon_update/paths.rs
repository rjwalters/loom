//! Repo-root discovery and the lifecycle-script/state-file resolution that
//! hangs off it.
//!
//! `find_repo_root()` here is **not** `daemon_start`'s: this script's version
//! requires BOTH a `.git` entry and a `.loom/` directory (#5140). That pairing
//! is load-bearing, not defensive tidiness — every fleet host that has run
//! `loom-daemon tokens bootstrap` keeps machine-level state in `~/.loom`, so a
//! `.loom`-only walk matched `$HOME` on its first iteration and then refused
//! with the misleading "No loom-daemon/Cargo.toml found at $HOME/loom-daemon".
//! `.git` alone is any git checkout; `.loom/` alone is machine state; only the
//! pair is a Loom repo.

use std::path::{Component, Path, PathBuf};

use super::util;

/// `find_repo_root [dir]` — walk up from `dir` (default `$PWD`) to the nearest
/// ancestor holding both `.git` and `.loom/`, following a linked worktree's
/// `.git` **file** to its main checkout.
///
/// `None` where the shell echoed the empty string, including for a `dir` that
/// cannot be `cd`-ed into (the shell's `cd … || { echo ""; return 0; }`).
#[must_use]
pub fn find_repo_root(start: Option<&Path>) -> Option<PathBuf> {
    let start = match start {
        Some(p) => logical_dir(p)?,
        None => logical_cwd()?,
    };
    let mut dir = start.as_path();
    loop {
        if dir.as_os_str().is_empty() || dir == Path::new("/") {
            return None;
        }
        if dir.join(".git").is_dir() && dir.join(".loom").is_dir() {
            return Some(dir.to_path_buf());
        }
        let dot_git = dir.join(".git");
        if dot_git.is_file() {
            if let Ok(text) = std::fs::read_to_string(&dot_git) {
                // `sed 's/^gitdir: //'` is per-line and unanchored at the end;
                // `$( )` then strips the trailing newline.
                let gitdir = text
                    .lines()
                    .map(|l| l.strip_prefix("gitdir: ").unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                let main_repo = PathBuf::from(&gitdir)
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf);
                if let Some(main_repo) = main_repo {
                    if main_repo.join(".loom").is_dir() {
                        return Some(main_repo);
                    }
                }
            }
        }
        dir = dir.parent()?;
    }
}

/// bash's `dir="$(cd "$dir" 2>/dev/null && pwd)" || { echo ""; return 0; }` —
/// **logical**, not physical.
///
/// `pwd` in bash defaults to `-L`: after a `cd` to an absolute path it reports
/// the path it was HANDED, with `.`/`..` folded lexically, and does **not**
/// resolve symlinked components the way `realpath`/[`std::fs::canonicalize`]
/// would. Using `canonicalize` here was a real divergence, not a nicety: on
/// macOS every `mktemp -d` path is under `/var/folders/…`, a symlink to
/// `/private/var/folders/…`, so the #5140 self-location fallback announced
/// `using this script's own checkout: /private/var/…` — a directory the
/// operator never typed and cannot find in their own shell's `$PWD`. The
/// retained suite's `wm2` case is what caught it.
///
/// The risk direction of keeping the logical form is the one the shell chose:
/// a message names the path the caller used, and every consumer of the result
/// (the repo-root walk, `loom-daemon/Cargo.toml`, the lifecycle scripts) goes
/// back through the filesystem, which follows the symlink anyway.
///
/// `None` where `cd` would have failed — anything that is not a traversable
/// directory.
fn logical_dir(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        logical_cwd()?.join(dir)
    };
    let mut out = PathBuf::new();
    for comp in absolute.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// bash's `$PWD` when it names the directory we are actually in, else the
/// resolved physical path. Same reasoning as `daemon_start::paths`: the
/// operator's logical path is what every message and state file should name.
fn logical_cwd() -> Option<PathBuf> {
    let physical = std::env::current_dir().ok()?;
    if let Some(pwd) = std::env::var_os("PWD") {
        let logical = PathBuf::from(pwd);
        if logical.is_absolute() && same_dir(&logical, &physical) {
            return Some(logical);
        }
    }
    Some(physical)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ma.dev() == mb.dev() && ma.ino() == mb.ino()
            }
            #[cfg(not(unix))]
            {
                let _ = (ma, mb);
                a == b
            }
        }
        _ => false,
    }
}

/// `is_loom_source_checkout <root>` — does it hold the crate this script
/// rebuilds?
#[must_use]
pub fn is_loom_source_checkout(root: Option<&Path>) -> bool {
    root.is_some_and(|r| r.join("loom-daemon/Cargo.toml").is_file())
}

/// `resolve_lifecycle_script <rel>` — the INSTALLED copy under
/// `<root>/.loom/scripts/cli/` first, then the shipped
/// `<root>/defaults/scripts/cli/`.
///
/// The order matters for a self-hosted checkout (which has both) and the
/// fallback matters for a fresh clone that has never been installed onto
/// itself — machine mode may point at exactly that.
#[must_use]
pub fn resolve_lifecycle_script(root: &Path, rel: &str) -> Option<PathBuf> {
    [
        root.join(".loom/scripts/cli").join(rel),
        root.join("defaults/scripts/cli").join(rel),
    ]
    .into_iter()
    .find(|candidate| util::is_executable(candidate))
}

/// `_ff_abort_resolve_resync_script <root>` — the same installed-then-defaults
/// precedence, adjusted for `resync-installed.sh` living directly under
/// `scripts/`, not `scripts/cli/`.
#[must_use]
pub fn resolve_resync_script(root: &Path) -> Option<PathBuf> {
    [
        root.join(".loom/scripts/resync-installed.sh"),
        root.join("defaults/scripts/resync-installed.sh"),
    ]
    .into_iter()
    .find(|candidate| util::is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_loom_directory_is_not_a_repo_root() {
        // #5140: `~/.loom` (the token pool / machine state) has no `.git`
        // sibling, and matching it is precisely the bug this pairing fixed.
        let tmp = std::env::temp_dir().join(format!("loom-update-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home/.loom")).unwrap();
        std::fs::create_dir_all(tmp.join("home/sub")).unwrap();
        assert!(find_repo_root(Some(&tmp.join("home/sub"))).is_none());

        std::fs::create_dir_all(tmp.join("home/.git")).unwrap();
        assert_eq!(find_repo_root(Some(&tmp.join("home/sub"))), Some(tmp.join("home")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The walk reports the LOGICAL path, the way `cd … && pwd` does — it must
    /// not silently re-route the answer through a symlink the caller did not
    /// name.
    ///
    /// This is the property the #5140 self-location message depends on, and
    /// the one a `std::fs::canonicalize` here breaks. On macOS it broke on
    /// every run, because `$TMPDIR` is itself `/var/folders/…` → `/private/var/
    /// folders/…`; this test reproduces that shape explicitly so it fails on
    /// Linux too, where no such ambient symlink exists.
    #[cfg(unix)]
    #[test]
    fn the_walk_answers_with_the_logical_path_not_the_symlink_target() {
        let tmp = std::env::temp_dir().join(format!("loom-update-logical-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let real = tmp.join("real");
        std::fs::create_dir_all(real.join("checkout/.git")).unwrap();
        std::fs::create_dir_all(real.join("checkout/.loom")).unwrap();
        std::fs::create_dir_all(real.join("checkout/sub")).unwrap();
        // `link` is another name for `real`, exactly as `/var` is for
        // `/private/var`.
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            find_repo_root(Some(&link.join("checkout/sub"))),
            Some(link.join("checkout")),
            "the answer named the symlink TARGET; the caller asked about the link"
        );
        // `..` is still folded lexically, the way `cd -L` folds it — popping
        // the component, not the directory it resolves to.
        assert_eq!(
            find_repo_root(Some(&link.join("checkout/sub/.."))),
            Some(link.join("checkout"))
        );
        // A path `cd` could not enter is the shell's empty-string case.
        assert!(find_repo_root(Some(&link.join("checkout/nope"))).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_source_checkout_is_one_with_the_crate_this_script_rebuilds() {
        let tmp = std::env::temp_dir().join(format!("loom-update-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("loom-daemon")).unwrap();
        assert!(!is_loom_source_checkout(Some(&tmp)));
        std::fs::write(tmp.join("loom-daemon/Cargo.toml"), "[package]\n").unwrap();
        assert!(is_loom_source_checkout(Some(&tmp)));
        assert!(!is_loom_source_checkout(None));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
