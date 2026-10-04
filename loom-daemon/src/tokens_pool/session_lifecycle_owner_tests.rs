//! Issue #10103: `accounts session start` mounts the daemon root's GitHub App
//! token dirs even when `LOOM_WORKSPACE` is unset, because the accounts
//! registry's own workspace is one of the credential owners.

use super::*;

#[test]
#[serial]
fn start_hands_create_the_registry_workspace_as_the_daemon_root() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    assert_eq!(
        *lifecycle.runner.daemon_roots.lock().unwrap(),
        vec![workspace.path().to_path_buf()]
    );
}

#[test]
#[serial]
fn start_with_a_mount_workspace_still_hands_create_the_registry_daemon_root() {
    // The operator's shape: `cd ~/GitHub/loom && loom-daemon accounts session
    // start agent-3 --mount-workspace ~/GitHub` (issue #10103).
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let parent = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle
        .start_with_workspace("alice", Some(parent.path()))
        .unwrap();
    assert_eq!(lifecycle.runner.creates.lock().unwrap()[0].3, parent.path());
    assert_eq!(
        *lifecycle.runner.daemon_roots.lock().unwrap(),
        vec![workspace.path().to_path_buf()]
    );
}

#[test]
fn gh_credential_owners_include_the_registry_daemon_root_without_loom_workspace() {
    // Issue #10103: an operator runs `cd ~/GitHub/loom && loom-daemon accounts
    // session start agent-3 --mount-workspace ~/GitHub` with no
    // LOOM_WORKSPACE. The mount (~/GitHub) has no `.loom/gh-config`; the
    // accounts registry's workspace (~/GitHub/loom) does. Before the fix only
    // the mount and LOOM_WORKSPACE were owners, so nothing was mounted and
    // posture reported gh=skip.
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("GitHub");
    let daemon = parent.join("loom");
    for dir in [
        daemon.join(".loom/gh-config"),
        daemon.join(".loom/gh-config-by-owner/2AMLogic"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let owners = gh_credential_owners(&parent, &daemon, None);
    assert_eq!(owners, vec![parent.clone(), daemon.clone()]);
    assert_eq!(
        gh_credential_dirs(&owners, None),
        vec![
            daemon.join(".loom/gh-config"),
            daemon.join(".loom/gh-config-by-owner")
        ]
    );
    // The pre-fix owner set finds nothing: the regression this guards.
    assert!(gh_credential_dirs(std::slice::from_ref(&parent), None).is_empty());

    // LOOM_WORKSPACE still counts, after the two roots, without duplicates.
    let other = tmp.path().join("loom-daemon");
    assert_eq!(
        gh_credential_owners(&parent, &daemon, Some(&other)),
        vec![parent.clone(), daemon.clone(), other]
    );
    assert_eq!(gh_credential_owners(&daemon, &daemon, Some(&daemon)), vec![daemon.clone()]);
    // An empty LOOM_WORKSPACE is not an owner.
    assert_eq!(
        gh_credential_owners(&parent, &daemon, Some(Path::new(""))),
        vec![parent, daemon]
    );
}

/// A symlinked token dir, or a symlinked `.loom`, is never mounted: an owner
/// root usually sits inside a read-write session mount, so a session could
/// otherwise aim it at any host directory (~/.ssh) for the next start.
#[cfg(unix)]
#[test]
fn gh_credential_dirs_never_follow_a_symlinked_loom_or_token_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let personal = tmp.path().join(".ssh");
    let repo = tmp.path().join("GitHub/loom");
    std::fs::create_dir_all(&personal).unwrap();
    std::fs::create_dir_all(repo.join(".loom/gh-config")).unwrap();
    assert_eq!(
        gh_credential_dirs(std::slice::from_ref(&repo), None),
        vec![repo.join(".loom/gh-config")]
    );

    let planted = tmp.path().join("GitHub/planted");
    std::fs::create_dir_all(planted.join(".loom")).unwrap();
    std::os::unix::fs::symlink(&personal, planted.join(".loom/gh-config")).unwrap();
    assert!(gh_credential_dirs(std::slice::from_ref(&planted), None).is_empty());

    let aliased = tmp.path().join("GitHub/aliased");
    std::fs::create_dir_all(&aliased).unwrap();
    std::os::unix::fs::symlink(repo.join(".loom"), aliased.join(".loom")).unwrap();
    assert!(gh_credential_dirs(std::slice::from_ref(&aliased), None).is_empty());
}
