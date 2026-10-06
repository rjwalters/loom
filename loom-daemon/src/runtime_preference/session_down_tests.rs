//! #10454: the ordered resolver passes over a Codex pool of down session
//! containers — split from `tests.rs` (file-size ratchet).

use super::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use crate::runtime_preference::{resolve_runtime, Decision};
use std::fs;
use std::path::Path;

/// Provision a Codex account `name` under the guard's isolated profile root,
/// session-managed (adopted by `accounts session start`) or bare-metal.
fn provision_codex_account(name: &str, session_managed: bool) {
    let root = std::env::var_os("LOOM_CODEX_PROFILE_ROOT").expect("ClearedRuntimeEnv sets it");
    let dir = Path::new(&root).join(name);
    fs::create_dir_all(&dir).unwrap();
    if session_managed {
        fs::write(dir.join(crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE), "{}")
            .unwrap();
    }
}

/// The curator walk for `[codex, claude]`: the chosen runtime, and the skip
/// summaries recorded on the way.
fn curator_walk(root: &Path) -> (String, Vec<String>) {
    let Decision::Preference { resolution, .. } =
        resolve_runtime(root, "curator", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap().admitted.runtime.clone();
    let skipped = resolution
        .skipped
        .iter()
        .map(|s| s.reason.summary())
        .collect();
    (chosen, skipped)
}

/// All four AC cases against the ordered resolver: every session down falls
/// through to Claude naming `SessionDown`; some up keeps Codex; an unqueryable
/// Docker keeps Codex (fail open); and a bare-metal account is never gated on
/// a container at all.
#[test]
#[serial_test::serial]
fn a_codex_pool_of_down_sessions_falls_through_but_fails_open() {
    use crate::tokens_pool::session_lifecycle::liveness::test_support::{running, set};
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["codex", "claude"]}}),
    );
    provision_codex_account("agent-1", true);
    provision_codex_account("agent-2", true);

    {
        let _down = set(running(&[]));
        let (chosen, skipped) = curator_walk(dir.path());
        assert_eq!(chosen, "claude");
        assert!(skipped[0].contains("SessionDown") && skipped[0].contains("2/2"), "{skipped:?}");
    }
    {
        let _some_up = set(running(&["agent-2"]));
        assert_eq!(curator_walk(dir.path()), ("codex".to_string(), vec![]));
    }
    {
        let _unobservable = set(None);
        assert_eq!(curator_walk(dir.path()), ("codex".to_string(), vec![]), "fail open");
    }
    provision_codex_account("bare-metal", false);
    let _down = set(running(&[]));
    assert_eq!(curator_walk(dir.path()), ("codex".to_string(), vec![]), "bare metal");
}
