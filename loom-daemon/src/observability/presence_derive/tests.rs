use super::*;

const SECRET: &str = "xyzzy-distinctive-prompt-text";

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn transcript(dir: &Path, name: &str, entrypoint: &str) -> PathBuf {
    let lines = [
        serde_json::json!({"type":"user","entrypoint":entrypoint,"sessionId":"sess-1",
            "timestamp":"2026-10-15T10:00:00Z","message":{"role":"user","content":SECRET}}),
        serde_json::json!({"type":"assistant","entrypoint":entrypoint,"sessionId":"sess-1",
            "timestamp":"2026-10-15T10:00:05Z","message":{"role":"assistant","content":[]}}),
        serde_json::json!({"type":"user","entrypoint":entrypoint,"sessionId":"sess-1",
            "timestamp":"2026-10-15T10:00:09Z",
            "message":{"role":"user","content":[{"type":"tool_result","content":SECRET}]}}),
    ];
    let path = dir.join(name);
    let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
    std::fs::write(&path, text.join("\n")).unwrap();
    path
}

fn owner(sweep: &str, issue: u64, released: bool) -> Owner {
    Owner {
        repo: "o/r".into(),
        sweep: sweep.into(),
        issue,
        pid: 1,
        released,
        mtime: Some(ts("2026-10-15T10:00:00Z")),
    }
}

fn run(
    ledger: &mut Ledger,
    owners: &[Owner],
    dispatched: &[&str],
    live: bool,
    cp: Checkpoint,
) -> Vec<TelemetryRecord> {
    let dispatched: HashSet<String> = dispatched.iter().map(|s| (*s).to_string()).collect();
    step(
        ledger,
        &Inputs {
            owners,
            dispatched: &dispatched,
            now: ts("2026-10-15T10:30:00Z"),
            is_live: &|_| live,
            checkpoint: &|_, _| cp,
        },
    )
}

#[test]
fn the_opt_out_beats_the_default_and_the_env_beats_the_file() {
    assert!(enabled_with(None, false));
    assert!(!enabled_with(None, true));
    assert!(!enabled_with(Some("0"), false));
    assert!(enabled_with(Some("1"), true));
}

#[test]
fn an_interactive_transcript_yields_ids_and_times_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = transcript(dir.path(), "a.jsonl", "cli");
    let facts = session_facts(&path).unwrap();
    assert_eq!(facts.session_id, "sess-1");
    assert_eq!(facts.last_input_at, Some(ts("2026-10-15T10:00:00Z")));
    // The tool result is activity, not a human prompt.
    assert_eq!(facts.last_activity_at, ts("2026-10-15T10:00:09Z"));
}

#[test]
fn a_dispatched_sdk_session_is_not_presence() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(session_facts(&transcript(dir.path(), "b.jsonl", "sdk-cli")), None);
}

#[test]
fn a_prompt_never_reaches_the_emitted_record() {
    let projects = tempfile::tempdir().unwrap();
    let proj = projects.path().join("-home-x");
    std::fs::create_dir_all(&proj).unwrap();
    transcript(&proj, "a.jsonl", "cli");
    let ws = tempfile::tempdir().unwrap();
    let mut d = Deriver::new("host-a".into(), ws.path());
    d.projects_dir = projects.path().to_path_buf();
    d.owner_dir = ws.path().join("none");
    let envelopes = d.tick(Utc::now(), &HashSet::new(), None);
    // The fixture file's mtime is "now", so the session is active.
    assert_eq!(envelopes.len(), 1);
    let wire = serde_json::to_string(&envelopes[0]).unwrap();
    assert!(wire.contains("sess-1"));
    assert!(!wire.contains(SECRET), "prompt text leaked: {wire}");
    // Throttled on the next tick: no new input, inside the window.
    assert!(d.tick(Utc::now(), &HashSet::new(), None).is_empty());
}

#[test]
fn a_live_lease_is_one_sweep_started_with_the_leases_own_id() {
    let mut ledger = Ledger::default();
    let owners = [owner("lease-sweep-1", 7, false)];
    let got = run(&mut ledger, &owners, &[], true, Checkpoint::Absent);
    assert_eq!(got.len(), 1);
    let TelemetryRecord::SweepStarted(s) = &got[0] else {
        panic!("expected sweep.started");
    };
    assert_eq!((s.sweep_id.as_str(), s.issue), ("lease-sweep-1", 7));
    assert_eq!(s.facts.dispatch_origin.as_deref(), Some("operator"));
    // A second tick re-emits nothing.
    assert!(run(&mut ledger, &owners, &[], true, Checkpoint::Absent).is_empty());
}

#[test]
fn a_dispatched_sweep_is_never_double_emitted() {
    let mut ledger = Ledger::default();
    let owners = [
        owner("step-builder-7-1", 7, false),
        owner("daemon-abc", 8, false),
    ];
    let got = run(&mut ledger, &owners, &["step-builder-7-1"], true, Checkpoint::Absent);
    assert!(got.is_empty());
    assert!(ledger.sweeps.is_empty());
}

#[test]
fn a_merge_ends_the_sweep_as_success_with_the_checkpoints_pr() {
    let mut ledger = Ledger::default();
    let owners = [owner("s1", 7, false)];
    run(&mut ledger, &owners, &[], true, Checkpoint::Present { pr: Some(42) });
    // The merge deleted the checkpoint; the renewer then released.
    let owners = [owner("s1", 7, true)];
    let got = run(&mut ledger, &owners, &[], true, Checkpoint::Absent);
    let TelemetryRecord::SweepOutcome(o) = &got[0] else {
        panic!("expected sweep.outcome");
    };
    assert_eq!(o.sweep_id, "s1");
    assert_eq!(o.dispatch_origin.as_deref(), Some("operator"));
    assert_eq!(o.pr_number, Some(42));
    assert!(matches!(o.result, SweepResult::Success));
    assert!(ledger.sweeps.is_empty());
}

#[test]
fn a_release_with_the_checkpoint_still_present_is_a_failure() {
    let mut ledger = Ledger::default();
    run(
        &mut ledger,
        &[owner("s1", 7, false)],
        &[],
        true,
        Checkpoint::Present { pr: None },
    );
    let got = run(&mut ledger, &[], &[], true, Checkpoint::Present { pr: None });
    let TelemetryRecord::SweepOutcome(o) = &got[0] else {
        panic!("expected sweep.outcome");
    };
    assert!(matches!(o.result, SweepResult::Failure));
}

#[test]
fn old_ended_records_are_history_not_news() {
    let mut ledger = Ledger::default();
    let mut stale = owner("old", 3, true);
    stale.mtime = Some(ts("2026-10-14T00:00:00Z"));
    assert!(run(&mut ledger, &[stale], &[], false, Checkpoint::Absent).is_empty());
}
