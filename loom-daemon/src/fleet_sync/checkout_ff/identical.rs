//! A file in the way of the fast-forward that holds nothing (#13580).
//!
//! `merge --no-overwrite-ignore` refuses to replace an untracked or ignored
//! file, and never compares content. When the file is byte-identical to the
//! blob the merge would write, replacing it loses nothing: its content is in
//! `origin/<branch>`. This removes such files so the caller can retry once.
//!
//! Fail closed: if any path git named is tracked, not a regular file (a
//! symlink, a directory), unreadable, absent from the incoming tree or
//! different by a single byte, **nothing** is removed.

use std::path::{Component, Path};
use std::time::Duration;

use super::git;

/// Remove every file in `named` (the paths git refused on) when all of them
/// are untracked regular files identical to `origin/<branch>`'s blob. Returns
/// the removed paths, or `None` when the hold must stay (nothing was removed).
pub(super) fn remove_identical_collisions(
    root: &Path,
    branch: &str,
    named: &[String],
    timeout: Duration,
) -> Option<Vec<String>> {
    if named.is_empty() || named.iter().any(|p| !is_plain_relative(p)) {
        return None;
    }
    if any_tracked(root, named, timeout)? {
        return None;
    }
    let tracking = git::tracking(branch);
    for path in named {
        let full = root.join(path);
        let meta = std::fs::symlink_metadata(&full).ok()?;
        if !meta.is_file() {
            return None;
        }
        let blob =
            git::run(root, &[], &["cat-file", "blob", &format!("{tracking}:{path}")], timeout)
                .ok()
                .filter(|out| out.status.success())?;
        if blob.stdout.len() as u64 != meta.len() || std::fs::read(&full).ok()? != blob.stdout {
            return None;
        }
    }
    // Every path is proven identical; only now is anything written.
    let mut removed = Vec::new();
    for path in named {
        if std::fs::remove_file(root.join(path)).is_err() {
            return (!removed.is_empty()).then_some(removed);
        }
        log::info!(
            "checkout_ff: {}: removed `{path}`, identical to origin/{branch}",
            root.display()
        );
        removed.push(path.clone());
    }
    Some(removed)
}

/// `Some(true)` when the index knows any of `paths`. `None` when it could not
/// be asked.
fn any_tracked(root: &Path, paths: &[String], timeout: Duration) -> Option<bool> {
    let literal: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    let mut args = vec!["ls-files", "-z", "--cached", "--"];
    args.extend(literal.iter().map(String::as_str));
    let out = git::run(root, &[], &args, timeout).ok()?;
    out.status.success().then_some(!out.stdout.is_empty())
}

fn is_plain_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
