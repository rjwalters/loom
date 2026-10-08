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
