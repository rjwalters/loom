use super::*;

const ID: &str = "4910f978-64b9-4654-942a-dae6514e859c";
const CODEX_ID: &str = "01a118db-198d-79e3-9f0c-a1b28c60cea4";

#[test]
fn claude_args_pin_a_fresh_launch_and_resume_a_paused_one() {
    assert_eq!(claude_args(None, None, None).unwrap(), Vec::<String>::new());
    assert_eq!(claude_args(None, None, Some(ID)).unwrap(), vec!["--session-id", ID]);
    assert_eq!(
        claude_args(Some(ID), Some("go on"), Some(ID)).unwrap(),
        vec!["--resume", ID, "go on"],
        "resume wins over the pin"
    );
    assert!(claude_args(Some(ID), None, None).is_err(), "resume needs a prompt");
    assert!(claude_args(Some("nope"), Some("p"), None).is_err());
    assert!(claude_args(None, None, Some("--evil")).is_err());
}

#[test]
fn a_codex_resume_must_pin_the_account() {
    assert!(codex_resume_prompt(CODEX_ID, Some("p"), None).is_err());
    assert!(codex_resume_prompt(CODEX_ID, None, Some("/h")).is_err());
    assert!(codex_resume_prompt("x", Some("p"), Some("/h")).is_err());
    assert_eq!(codex_resume_prompt(CODEX_ID, Some("p"), Some("/h")).unwrap(), "p");
}

#[test]
fn the_resume_prompt_names_the_roll_the_parked_call_and_the_stopped_processes() {
    let p = resume_prompt(&ResumePromptInput {
        from_version: Some("0.19.876".into()),
        to_version: Some("0.19.877".into()),
        parked_tool: Some("Bash".into()),
        parked_summary: Some("echo three >> steps.log".into()),
    });
    assert!(p.contains("(0.19.876 -> 0.19.877)"), "{p}");
    assert!(p.contains("(Bash: echo three >> steps.log) did NOT run"), "{p}");
    assert!(p.contains("Background processes"), "{p}");
    let bare = resume_prompt(&ResumePromptInput::default());
    assert!(bare.contains("did NOT run"), "{bare}");
}

#[test]
fn the_codex_session_banner_is_parsed() {
    let text = format!("OpenAI Codex v0.160\n--------\nworkdir: /x\nsession id: {CODEX_ID}\n");
    assert_eq!(parse_codex_session_id(&text).as_deref(), Some(CODEX_ID));
    assert_eq!(parse_codex_session_id("session id: zzz\n"), None);
    assert_eq!(parse_codex_session_id(""), None);
}

#[test]
fn capture_writes_the_handle_while_the_session_is_still_running() {
    let tmp = tempfile::tempdir().unwrap();
    let stderr = tmp.path().join("stderr");
    let handle = tmp.path().join("item").join("handle.json");
    std::fs::write(&stderr, "starting\n").unwrap();
    let spec = CaptureSpec {
        stderr_file: stderr.clone(),
        handle_file: handle.clone(),
        watch_pid: Some(std::process::id()),
        poll: Duration::from_millis(20),
        timeout: Duration::from_secs(10),
        template: CapturedHandle {
            session_store: Some("/codex-home".into()),
            account: Some("agent-3".into()),
            container: Some("loom-codex-session-agent-3".into()),
            ..CapturedHandle::default()
        },
    };
    let worker = std::thread::spawn(move || capture_codex(&spec));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!handle.exists());
    std::fs::write(&stderr, format!("starting\nsession id: {CODEX_ID}\nmore output\n")).unwrap();
    assert_eq!(worker.join().unwrap(), CaptureOutcome::Captured(CODEX_ID.into()));
    let h = read_handle(&handle).unwrap();
    assert_eq!(h.session_id, CODEX_ID);
    assert_eq!(h.runtime, "codex");
    assert_eq!(h.account.as_deref(), Some("agent-3"));
}

#[test]
fn capture_stops_when_the_watched_process_is_gone_or_time_runs_out() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = CaptureSpec {
        stderr_file: tmp.path().join("missing"),
        handle_file: tmp.path().join("h.json"),
        watch_pid: Some(u32::MAX - 1),
        poll: Duration::from_millis(10),
        timeout: Duration::from_secs(5),
        template: CapturedHandle::default(),
    };
    assert_eq!(capture_codex(&spec), CaptureOutcome::WatchedExited);
    spec.watch_pid = None;
    spec.timeout = Duration::from_millis(50);
    assert_eq!(capture_codex(&spec), CaptureOutcome::TimedOut);
}

/// Whether a fresh open of `path` can take the account lock right now.
fn lock_is_free(path: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;
    let probe = std::fs::File::open(path).unwrap();
    // SAFETY: `probe` is an open file for the whole call.
    unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[test]
fn the_capture_watcher_gives_up_an_inherited_account_lease() {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("account.lock");
    let owner = std::fs::File::create(&lock).unwrap();
    // SAFETY: `owner` is an open file for the whole call.
    assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    // The watcher's copy: a second descriptor on the same open file
    // description, which is what a child inherits.
    let inherited = owner.try_clone().unwrap().into_raw_fd();
    // The spawn script has exited; only the watcher's copy keeps the lock.
    drop(owner);
    assert!(!lock_is_free(&lock), "the inherited copy alone holds the lease");

    assert!(!release_inherited_lease(None));
    assert!(!release_inherited_lease(Some("not-a-descriptor")));
    assert!(!release_inherited_lease(Some("2")), "never closes stdio");
    assert!(!lock_is_free(&lock));

    // #10832: only the dispatcher's own descriptor number is ever closed. The
    // watcher's copy here is some other number, so the public entry point
    // (which accepts the dispatcher's 198 only) leaves it alone.
    assert_ne!(inherited, crate::tokens_pool::private_workspace::dispatch::LEASE_FD);
    assert!(!release_inherited_lease(Some(&inherited.to_string())));
    assert!(!lock_is_free(&lock), "a descriptor the dispatcher never names is not closed");
    assert!(!release_lease_descriptor(Some(&inherited.to_string()), inherited + 1));
    assert!(!release_lease_descriptor(Some("2"), 2), "never closes stdio");

    assert!(release_lease_descriptor(Some(&inherited.to_string()), inherited));
    assert!(lock_is_free(&lock), "the lease is free once the watcher closed its copy");
}

const STORE_SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";

/// #10832: H5 only resumes a session the runtime will be able to find.
#[test]
fn a_claude_session_is_reachable_only_with_its_transcript_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_string_lossy().into_owned();
    let missing = session_store_reachable("claude", STORE_SID, Some(&store)).unwrap_err();
    assert!(missing.contains("no transcript"), "{missing}");
    let project = tmp.path().join("projects").join("-r-repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(format!("{STORE_SID}.jsonl")), "{}").unwrap();
    session_store_reachable("claude", STORE_SID, Some(&store)).unwrap();
    assert!(session_store_reachable("claude", "not-a-session", Some(&store)).is_err());
}

#[test]
fn a_codex_session_needs_its_recorded_home_with_usable_auth_and_its_rollout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agent-3");
    let store = home.to_string_lossy().into_owned();
    let no_home = session_store_reachable("codex", STORE_SID, None).unwrap_err();
    assert!(no_home.contains("no CODEX_HOME"), "{no_home}");
    assert!(session_store_reachable("codex", STORE_SID, Some(&store))
        .unwrap_err()
        .contains("is gone"));
    std::fs::create_dir_all(&home).unwrap();
    assert!(session_store_reachable("codex", STORE_SID, Some(&store))
        .unwrap_err()
        .contains("auth.json"));
    std::fs::write(home.join("auth.json"), "{}").unwrap();
    // No `sessions/` tree on this host (it lives in the session container).
    session_store_reachable("codex", STORE_SID, Some(&store)).unwrap();
    // A visible tree must hold the rollout.
    let day = home.join("sessions").join("2026").join("10").join("08");
    std::fs::create_dir_all(&day).unwrap();
    assert!(session_store_reachable("codex", STORE_SID, Some(&store))
        .unwrap_err()
        .contains("no rollout"));
    std::fs::write(day.join(format!("rollout-2026-10-08T00-00-00-{STORE_SID}.jsonl")), "").unwrap();
    session_store_reachable("codex", STORE_SID, Some(&store)).unwrap();
}
