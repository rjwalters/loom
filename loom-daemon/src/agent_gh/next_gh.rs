//! Find `next_gh`, the `gh` the agent front stands in front of (#10331).
//!
//! The front is `loom-daemon` reached through a `gh` symlink placed first on
//! the agent's `PATH`, so "the next `gh`" must skip anything that resolves to
//! this very binary. Order:
//!
//! 1. `LOOM_GH_BIN` — the existing override every Loom `gh` resolver honours
//!    (and the composition point for the 2am managed launcher, #9987);
//! 2. the first executable `gh` on `PATH` that is not this binary — which is
//!    the managed launcher itself when it is installed as `gh`, so the shim
//!    composes with it rather than replacing its policy/telemetry.
//!
//! #9983's policy launcher (resolver rung 1) is still a stub; when it lands it
//! outranks `LOOM_GH_BIN` for the daemon's own reads, not for this ladder.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The underlying `gh`, or `None` when there is none.
#[must_use]
pub fn resolve() -> Option<PathBuf> {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    resolve_from(std::env::var("LOOM_GH_BIN").ok().as_deref(), &dirs, me.as_deref())
}

/// The pure ladder, for tests.
#[must_use]
pub fn resolve_from(
    override_bin: Option<&str>,
    path: &[PathBuf],
    me: Option<&Path>,
) -> Option<PathBuf> {
    // "Me" is this binary *or any* `loom-daemon` front: two fronts from
    // different builds on one `PATH` would otherwise pass through to each
    // other forever.
    let is_me = |p: &Path| {
        let canon = p.canonicalize().ok();
        (me.is_some() && canon.as_deref() == me)
            || canon.as_deref().and_then(Path::file_name) == Some(OsStr::new("loom-daemon"))
    };
    if let Some(bin) = override_bin.filter(|b| !b.is_empty()) {
        let found = if bin.contains('/') {
            Some(PathBuf::from(bin))
        } else {
            search(bin, path, &is_me)
        };
        if let Some(p) = found.filter(|p| !is_me(p)) {
            return Some(p);
        }
    }
    search("gh", path, &is_me)
}

fn search(name: &str, path: &[PathBuf], is_me: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    path.iter()
        .map(|d| d.join(name))
        .find(|c| is_executable(c) && !is_me(c))
}

pub(crate) fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}
