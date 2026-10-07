//! Issue #10661 items 3 and 4: holds in another root's profile for the same
//! account, read and lifted through [`SessionLifecycle::with_peer_roots`].
//!
//! Each extra root here is a workspace whose own account registry maps
//! `alice` to a different profile directory under the same profile root —
//! the case `session_hold::account_profiles` exists for.

use super::*;
use crate::tokens_pool::session_hold_roots;

/// A workspace whose registry resolves `alice` to `<root>/<dir>`, with a hold
/// written in that profile. Returns the workspace and the profile directory.
fn root_holding_alice_in(root: &Path, dir: &str) -> (tempfile::TempDir, PathBuf) {
    let workspace = tempfile::tempdir().unwrap();
    let registry = crate::tokens_pool::paths::per_repo_accounts_file(workspace.path());
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(
        &registry,
        format!(
            r#"{{"version":1,"accounts":[{{"provider":"codex","name":"alice","credential_kind":"codex_home","credential_reference":"{dir}","enabled":true}}]}}"#
        ),
    )
    .unwrap();
    let profile = root.join(dir);
    std::fs::create_dir_all(&profile).unwrap();
    session_hold::write_hold(&profile, session_hold::now_unix_ms()).unwrap();
    (workspace, profile)
}

fn held(profile: &Path) -> bool {
    profile.join(session_hold::HOLD_FILE).exists()
}

/// Item 4: a registered peer's hold is shown by `status` and lifted by an
/// operator `start` through `with_peer_roots`, and only through it.
#[test]
#[serial]
fn a_peer_roots_hold_is_shown_and_lifted_through_with_peer_roots() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let (peer, peer_profile) = root_holding_alice_in(root.path(), "alice-peer");

    let alone = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    assert!(!alone.status("alice").unwrap().held, "the peer's hold is invisible without it");

    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None)
        .with_peer_roots(vec![peer.path().to_path_buf()]);
    assert!(lifecycle.status("alice").unwrap().held);
    let status = lifecycle.start("alice").unwrap();
    assert!(status.running && !status.held, "{status:?}");
    assert!(!held(&peer_profile), "the operator start lifted the peer's hold");

    // A later stop holds it again (in the account's own profile), and the
    // next start lifts that one too.
    assert!(lifecycle.stop("alice", false).unwrap().held);
    assert!(!lifecycle.start("alice").unwrap().held);
}

/// Item 3: a hold kept only in the daemon's fallback root (unregistered, the
/// registry empty) is shown by `accounts session status` and lifted by an
/// operator start, because the CLI's peers come from the same hold root set
/// the reconcile pass reads, with the fallback root recorded by the daemon.
#[test]
#[serial]
fn a_hold_in_the_daemons_fallback_root_is_shown_and_lifted_by_the_cli() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let (daemon_root, fallback_profile) = root_holding_alice_in(root.path(), "alice-daemon");

    // The daemon records its fallback root; the CLI reads it back.
    let record = tempfile::tempdir().unwrap();
    let file = record.path().join("fallback-root.json");
    session_hold_roots::record_fallback_root(&file, daemon_root.path()).unwrap();
    let fallback = session_hold_roots::read_fallback_root(&file);
    let registered: Vec<PathBuf> = Vec::new();
    let peers =
        session_hold_roots::cli_peer_roots(workspace.path(), &registered, fallback.as_deref());

    // The pass reads exactly these roots' profiles for the hold.
    let pass_roots = session_hold_roots::hold_roots(&registered, Some(daemon_root.path()));
    assert_eq!(peers, pass_roots);

    let lifecycle =
        SessionLifecycle::new(workspace.path(), FakeRunner::default(), None).with_peer_roots(peers);
    let status = lifecycle.status("alice").unwrap();
    assert!(status.held, "the fallback root's hold is shown: {status:?}");
    assert!(!lifecycle.start("alice").unwrap().held);
    assert!(!held(&fallback_profile), "and lifted by the operator start");
}
