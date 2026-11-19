//! Unit tests for the front's pure helpers (#10331). The end-to-end
//! behaviour (ETag reads, passthrough fidelity, sentinel) is exercised
//! against the built binary in `tests/agent_gh_front.rs`.

#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{implicit_repo, next_gh, prepend_path, session_env};

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

#[cfg(unix)]
#[test]
fn session_env_line_is_written_once_and_prepends_once_when_sourced() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("env.sh");
    let prefix = OsStr::new("/opt/it's here:/f");
    let current = OsStr::new("/usr/bin:/bin");
    let w = session_env::append_env(&file, prefix, Some(current)).unwrap();
    assert_eq!(w, session_env::Written::Appended);
    let w = session_env::append_env(&file, prefix, Some(current)).unwrap();
    assert_eq!(w, session_env::Written::AlreadyPresent);
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.matches(session_env::MARKER).count(), 1);
    // Sourced twice (Claude Code sources it per Bash call): one prepend.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(". '{0}'; . '{0}'; printf %s \"$PATH\"", file.display()))
        .env("PATH", current)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "/opt/it's here:/f:/usr/bin:/bin");
}

#[test]
fn session_env_skips_a_path_that_already_starts_with_the_prefix() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("env.sh");
    let worker = OsStr::new("/l:/f:/usr/bin");
    let w = session_env::append_env(&file, OsStr::new("/l:/f"), Some(worker)).unwrap();
    assert_eq!(w, session_env::Written::AlreadyFirst);
    assert!(!file.exists());
    // Present but not first still prepends.
    let later = OsStr::new("/usr/bin:/l:/f");
    let w = session_env::append_env(&file, OsStr::new("/l:/f"), Some(later)).unwrap();
    assert_eq!(w, session_env::Written::Appended);
}

#[cfg(unix)]
#[test]
fn status_classifies_front_and_bypass() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let daemon = d.path().join("bin").join("loom-daemon");
    let other = d.path().join("managed").join("gh");
    for f in [&daemon, &other] {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let front = d.path().join("front");
    std::fs::create_dir_all(&front).unwrap();
    std::os::unix::fs::symlink(&daemon, front.join("gh")).unwrap();
    let path = std::env::join_paths([d.path().join("empty"), front.clone()]).unwrap();
    let gh = session_env::first_gh(Some(&path)).unwrap();
    assert_eq!(gh, front.join("gh"));
    assert_eq!(session_env::classify_gh(&gh), "front");
    assert_eq!(session_env::classify_gh(&other), "bypassed");
    assert_eq!(session_env::first_gh(Some(OsStr::new("/nonexistent-dir"))), None);
}

/// A gateway attempt killed by a signal exits as a shell would report the
/// exec'd `gh`: 128 + the signal, not a flat 1 (2am#4367 P4).
#[test]
fn a_signalled_gateway_attempt_reports_128_plus_the_signal() {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(super::exit_code(std::process::ExitStatus::from_raw(9)), 137);
    assert_eq!(super::exit_code(std::process::ExitStatus::from_raw(15)), 143);
    assert_eq!(super::exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
}

#[test]
fn a_shed_for_an_owner_no_reader_serves_does_not_say_retry_later() {
    let uncovered = |o: &str| o == "third-party";
    let msg = super::shed_message(Some("third-party"), &uncovered);
    assert!(msg.contains("no reader App on this host serves owner third-party"), "{msg}");
    assert!(msg.contains("Retrying will not help"), "{msg}");
    assert!(!msg.contains("Retry later"), "{msg}");
    assert!(msg.ends_with(crate::gh_invocation::SHED_MARKER), "{msg}");
    // A covered owner (out of budget, withdrawn) or no certain owner: retry.
    for owner in [Some("acme"), None] {
        let msg = super::shed_message(owner, &uncovered);
        assert!(msg.contains("Retry later"), "{msg}");
        assert!(msg.ends_with(crate::gh_invocation::SHED_MARKER), "{msg}");
    }
}
