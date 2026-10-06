//! Tests for the batched session-container liveness read (Issue #10454).
//!
//! The selection-side tests live here rather than in `select_tests.rs`, which
//! is at the file-size threshold — and Codex selection is
//! `account_registry::select_codex_account_where`, not `select.rs` (the Claude
//! token selector).

use super::test_support::{running, set};
use super::*;
use crate::tokens_pool::account_registry::{select_account, AccountId, InventoryProvenance};
use crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE;
use crate::tokens_pool::CredentialKind;
use serial_test::serial;
use std::fs;
use std::path::Path;

fn account(profiles: &Path, name: &str, session_managed: bool) -> AccountDescriptor {
    let dir = profiles.join(name);
    fs::create_dir_all(&dir).unwrap();
    if session_managed {
        fs::write(dir.join(SESSION_MARKER_FILE), "{}").unwrap();
    }
    AccountDescriptor {
        id: AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: dir,
        enabled: true,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

#[test]
fn parse_ps_keeps_only_session_containers() {
    let live = SessionLiveness::parse_ps(
        "loom-codex-session-agent-1\nunrelated\n  loom-codex-session-agent-3 \n\nloom-codex-session-\n",
    );
    assert!(live.is_running("agent-1"));
    assert!(live.is_running("agent-3"));
    assert!(!live.is_running("agent-2"));
    assert!(!live.is_running(""), "a bare prefix names no account: {live:?}");
}

#[test]
fn session_down_needs_both_a_session_marker_and_a_successful_listing() {
    let profiles = tempfile::tempdir().unwrap();
    let session = account(profiles.path(), "agent-1", true);
    let bare_metal = account(profiles.path(), "agent-2", false);
    let nothing_running = Some(SessionLiveness::default());
    assert!(is_session_down(&session, nothing_running.as_ref()));
    assert!(!is_session_down(&session, running(&["agent-1"]).as_ref()));
    // Fail open: Docker could not be queried, so nothing is "down".
    assert!(!is_session_down(&session, None));
    // A bare-metal account has no container to be down.
    assert!(!is_session_down(&bare_metal, nothing_running.as_ref()));
}

#[test]
fn live_preferred_narrows_only_when_something_is_down_and_something_is_live() {
    let profiles = tempfile::tempdir().unwrap();
    let inventory = vec![
        account(profiles.path(), "agent-1", true),
        account(profiles.path(), "agent-2", true),
    ];
    let live = set(running(&["agent-2"]));
    let names: Vec<_> = live_preferred(&inventory)
        .unwrap()
        .into_iter()
        .map(|a| a.id.name)
        .collect();
    assert_eq!(names, ["agent-2"]);
    drop(live);
    // All up, all down, or Docker unobservable: nothing to prefer.
    for liveness in [running(&["agent-1", "agent-2"]), running(&[]), None] {
        let _guard = set(liveness.clone());
        assert_eq!(live_preferred(&inventory), None, "{liveness:?}");
    }
}

/// AC2: with one session down and one up, Codex selection hands out the live
/// account every time instead of round-robining onto the down one (which
/// spawn-codex.sh would refuse with exit 78). With Docker unobservable the
/// round-robin is untouched (fail open).
#[test]
#[serial]
fn selection_prefers_a_live_session_account() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _root = crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());
    let prior_probe = std::env::var_os("LOOM_CODEX_SESSION_PROBE");
    // The in-container auth probe is a separate concern and would shell out.
    std::env::set_var("LOOM_CODEX_SESSION_PROBE", "0");
    account(profiles.path(), "agent-1", true);
    account(profiles.path(), "agent-2", true);

    let pick = || {
        select_account(workspace.path(), AccountProvider::Codex, None)
            .unwrap()
            .id
            .name
    };
    {
        let _live = set(running(&["agent-2"]));
        for _ in 0..4 {
            assert_eq!(pick(), "agent-2");
        }
    }
    let unobservable: std::collections::HashSet<_> = (0..4).map(|_| pick()).collect();
    assert_eq!(unobservable.len(), 2, "fail open keeps both accounts in rotation");

    match prior_probe {
        Some(value) => std::env::set_var("LOOM_CODEX_SESSION_PROBE", value),
        None => std::env::remove_var("LOOM_CODEX_SESSION_PROBE"),
    }
}
