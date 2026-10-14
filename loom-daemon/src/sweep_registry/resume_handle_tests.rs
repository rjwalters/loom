use super::*;
use crate::sweep_registry::test_support::fixture_registry;

const CODEX_ID: &str = "01a118db-198d-79e3-9f0c-a1b28c60cea4";

fn env_of(cmd: &Command, key: &str) -> Option<Option<String>> {
    cmd.get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new(key))
        .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

#[test]
fn a_claude_dispatch_gets_a_pinned_session_id_and_the_pause_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let s = DispatchSession::new("sweep-issue-7-abc", tmp.path(), None).unwrap();
    assert_eq!(s.runtime, "claude");
    let id = s.claude_session_id.clone().expect("pinned at dispatch");
    assert!(resume::valid_session_id(&id), "{id}");
    let mut cmd = Command::new("true");
    s.apply_env(&mut cmd);
    assert_eq!(env_of(&cmd, roll_pause::ITEM_ENV), Some(Some("sweep-issue-7-abc".into())));
    assert_eq!(env_of(&cmd, resume::CLAUDE_SESSION_ENV), Some(Some(id)));
    let dir = roll_pause::default_pause_root(tmp.path());
    assert_eq!(env_of(&cmd, roll_pause::DIR_ENV), Some(Some(dir.display().to_string())));
    // A fresh dispatch is never a resume, whatever the daemon inherited.
    assert_eq!(env_of(&cmd, resume::RESUME_SESSION_ENV), Some(None));
    assert_eq!(env_of(&cmd, resume::SCOPE_UNIT_ENV).is_some(), cfg!(target_os = "linux"));
}

#[test]
fn a_codex_dispatch_has_no_pinned_id_but_a_handle_file() {
    let tmp = tempfile::tempdir().unwrap();
    let s = DispatchSession::new("sweep-issue-8-x", tmp.path(), Some("codex")).unwrap();
    assert_eq!(s.claude_session_id, None);
    let mut cmd = Command::new("true");
    s.apply_env(&mut cmd);
    assert_eq!(env_of(&cmd, resume::CLAUDE_SESSION_ENV), Some(None));
    let handle = s.item_dir().join(roll_pause::HANDLE_FILE);
    assert_eq!(env_of(&cmd, resume::HANDLE_FILE_ENV), Some(Some(handle.display().to_string())));
}

#[test]
fn an_unusable_item_id_leaves_the_dispatch_untouched() {
    assert!(DispatchSession::new("../escape", Path::new("/w"), None).is_none());
    assert!(DispatchSession::new("", Path::new("/w"), None).is_none());
}

#[test]
fn an_owner_json_from_an_older_binary_still_parses() {
    let old = r#"{"issue":5,"owner_pid":42,"acquired_at":"2026-01-01T00:00:00Z","sweep_id":"s","pgid":42}"#;
    let tmp = tempfile::tempdir().unwrap();
    let agent = agent_from_owner(old, tmp.path()).expect("parses");
    assert_eq!((agent.issue, agent.pid, agent.pgid), (5, 42, Some(42)));
    assert_eq!(agent.resume_handle, None);
    assert_eq!(agent.agent_started_at, None);
    // And an older binary reading a NEW owner.json ignores the new keys:
    // serde never denies unknown fields on LockOwner.
}

#[test]
fn stamping_and_the_snapshot_round_trip_through_owner_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (registry, _log) = fixture_registry(tmp.path());
    let lock = registry.config.locks_dir().join("issue-11");
    std::fs::create_dir_all(&lock).unwrap();
    let owner = LockOwner::new(11, std::process::id(), "sweep-issue-11-a".to_string());
    std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();

    let s =
        DispatchSession::new("sweep-issue-11-a", &registry.config.workspace_root, None).unwrap();
    registry.stamp_resume_handle_in_lock(11, &s, Some("opus"), Some("high"));
    // The child-pid stamp that follows preserves the new fields.
    registry
        .record_child_pid_in_lock(11, 999, Some(999), Some("opus"), Some("high"))
        .unwrap();

    let raw = std::fs::read_to_string(lock.join("owner.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(written["item_id"], "sweep-issue-11-a");
    assert_eq!(
        written["resume_handle"]["session_id"],
        s.claude_session_id.clone().unwrap().as_str()
    );
    assert_eq!(written["resume_handle"]["model"], "opus");
    assert!(written["agent_started_at"].is_string());

    let snap = registry.in_flight_snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].pid, 999);
    assert_eq!(snap[0].resume_handle.as_ref().unwrap().session_id, s.claude_session_id);
    let input = snap[0].classify_input(chrono::Utc::now() + chrono::Duration::seconds(600));
    assert!(input.agent_age_secs.unwrap() >= 599);
    assert_eq!(input.session_id, s.claude_session_id);
}

#[test]
fn the_snapshot_merges_a_live_captured_codex_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (registry, _log) = fixture_registry(tmp.path());
    let lock = registry.config.locks_dir().join("issue-12");
    std::fs::create_dir_all(&lock).unwrap();
    let owner = LockOwner::new(12, 1, "sweep-issue-12-b".to_string());
    std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
    let s =
        DispatchSession::new("sweep-issue-12-b", &registry.config.workspace_root, Some("codex"))
            .unwrap();
    registry.stamp_resume_handle_in_lock(12, &s, None, None);
    assert_eq!(
        registry.in_flight_snapshot()[0]
            .resume_handle
            .as_ref()
            .unwrap()
            .session_id,
        None
    );

    let captured = resume::CapturedHandle {
        runtime: "codex".into(),
        session_id: CODEX_ID.into(),
        session_store: Some("/profiles/agent-3".into()),
        account: Some("agent-3".into()),
        container: Some("loom-codex-session-agent-3".into()),
        ..resume::CapturedHandle::default()
    };
    let file = s.item_dir().join(roll_pause::HANDLE_FILE);
    roll_pause::write_atomic(&file, &serde_json::to_vec(&captured).unwrap()).unwrap();
    let h = registry.in_flight_snapshot()[0]
        .resume_handle
        .clone()
        .unwrap();
    assert_eq!(h.session_id.as_deref(), Some(CODEX_ID));
    assert_eq!(h.account.as_deref(), Some("agent-3"));
    assert_eq!(h.container.as_deref(), Some("loom-codex-session-agent-3"));
    assert_eq!(h.runtime, "codex");
}
