//! Make a temp directory a workspace this installation may write to (#9548).
//!
//! Role ticks and sweep dispatch refuse a root outside the write scope: its
//! `gh` target must be its `origin`, the repo must be managed here, and the
//! credential must have WRITE. A test that launches through those paths on a
//! bare temp directory is (correctly) refused. This builds what a real
//! workspace is: a git checkout with an `origin`, a `.loom/`, and, through
//! `LOOM_GH_BIN`, a fake `gh` that reports push permission. The permission
//! cache goes to the directory's own `.write-scope-cache`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Turn `root` into a writable managed checkout and return the environment a
/// child process needs to see it that way.
pub fn writable_env(root: &Path) -> Vec<(&'static str, PathBuf)> {
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/role-probe.git",
        ],
    ] {
        let ok = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git {args:?} in {}", root.display());
    }
    std::fs::create_dir_all(root.join(".loom")).expect("create .loom");
    let fake_gh = root.join("fake-gh");
    std::fs::write(
        &fake_gh,
        "#!/bin/sh\n[ \"$1 $2\" = \"api repos/acme/role-probe\" ] && echo '{\"push\":true}'\nexit 0\n",
    )
    .expect("write fake gh");
    std::fs::set_permissions(&fake_gh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod fake gh");
    vec![
        ("LOOM_GH_BIN", fake_gh),
        // Keep a host egress policy's launcher from outranking the fake (#9995).
        ("LOOM_GH_NO_POLICY_LAUNCHER", PathBuf::from("1")),
        ("LOOM_WRITE_SCOPE_CACHE_DIR", root.join(".write-scope-cache")),
    ]
}
