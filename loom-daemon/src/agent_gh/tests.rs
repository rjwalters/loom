//! Unit tests for the front's pure helpers (#10331). The end-to-end
//! behaviour (ETag reads, passthrough fidelity, sentinel) is exercised
//! against the built binary in `tests/agent_gh_front.rs`.

#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{implicit_repo, next_gh, prepend_path};

#[test]
fn implicit_repo_is_the_lone_github_origin() {
    let one = "remote.origin.url git@github.com:rjwalters/loom.git\n\
               remote.origin.fetch +refs/heads/*:refs/remotes/origin/*\n";
    assert_eq!(implicit_repo(one).as_deref(), Some("rjwalters/loom"));
    let https = "remote.origin.url https://github.com/o/r\nremote.origin.gh-resolved base\n";
    assert_eq!(implicit_repo(https).as_deref(), Some("o/r"));
}

#[test]
fn implicit_repo_declines_anything_gh_might_resolve_differently() {
    for config in [
        "",
        // A second remote: gh prefers `upstream`/`github`.
        "remote.origin.url git@github.com:me/r.git\nremote.upstream.url git@github.com:o/r.git\n",
        // `gh repo set-default` to another repo.
        "remote.origin.url git@github.com:me/r.git\nremote.origin.gh-resolved o/r\n",
        // Not github.com.
        "remote.origin.url git@ghe.example.com:o/r.git\n",
        // Not a GitHub URL at all.
        "remote.origin.url /srv/git/r.git\n",
        // Two URLs on one remote.
        "remote.origin.url git@github.com:o/r.git\nremote.origin.url git@github.com:o/s.git\n",
        // A section-less `remote.*` key.
        "remote.pushdefault origin\nremote.origin.url git@github.com:o/r.git\n",
    ] {
        assert_eq!(implicit_repo(config), None, "{config:?}");
    }
}

#[test]
fn prepend_path_puts_the_shim_first_once() {
    let shim = Path::new("/tmp/loom-gh-shim-x");
    let current = std::env::join_paths(["/usr/bin", "/tmp/loom-gh-shim-x", "/bin"]).unwrap();
    let got = prepend_path(shim, Some(&current)).unwrap();
    let dirs: Vec<PathBuf> = std::env::split_paths(&got).collect();
    assert_eq!(dirs, [shim, Path::new("/usr/bin"), Path::new("/bin")].map(Path::to_path_buf));
    let alone = prepend_path(shim, None).unwrap();
    assert_eq!(alone, OsStr::new("/tmp/loom-gh-shim-x"));
}

#[cfg(unix)]
#[test]
fn next_gh_skips_this_binary_and_any_loom_daemon_front() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let (front_a, front_b, real) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
    for dir in [&front_a, &front_b, &real] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let me = d.path().join("me").join("loom-daemon");
    let other = d.path().join("other").join("loom-daemon");
    for bin in [&me, &other] {
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::os::unix::fs::symlink(&me, front_a.join("gh")).unwrap();
    std::os::unix::fs::symlink(&other, front_b.join("gh")).unwrap();
    let gh = real.join("gh");
    std::fs::write(&gh, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = [front_a.clone(), front_b, real];
    let me = me.canonicalize().unwrap();
    assert_eq!(next_gh::resolve_from(None, &path, Some(&me)), Some(gh.clone()));
    // LOOM_GH_BIN wins, unless it names a front.
    let stub = d.path().join("stub");
    std::fs::write(&stub, "").unwrap();
    assert_eq!(next_gh::resolve_from(stub.to_str(), &path, Some(&me)), Some(stub));
    let front = front_a.join("gh");
    assert_eq!(next_gh::resolve_from(front.to_str(), &path, Some(&me)), Some(gh));
    assert_eq!(next_gh::resolve_from(None, &path[..2], Some(&me)), None);
}
