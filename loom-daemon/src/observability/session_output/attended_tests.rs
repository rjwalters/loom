//! Attended-run tests (#10116): the launch detection, the silent no-op when
//! nothing is configured, locating the caller's own transcript, the detached
//! command, and an end-to-end run whose records carry the correlation
//! attributes and the `attended` marker.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::observability::queue::DurableQueue;
use crate::telemetry::kinds::session_output::SessionOutputRecord;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

const SESSION: &str = "755d0cb5-f9ec-464b-8d7a-c134ee5a1ca9";
const ISSUE: u32 = 10116;
/// A GitHub-token-shaped canary. It must never appear in an exported record.
const CANARY: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "loom-attended-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A minimal Loom checkout (`.git` + `.loom/`), optionally with a config.
fn checkout(scratch: &Scratch, config: Option<&str>) -> PathBuf {
    let root = scratch.path("checkout");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    if let Some(config) = config {
        std::fs::write(root.join(".loom/config.json"), config).unwrap();
    }
    root
}

/// A config that turns on everything an attended tailer needs, pointing at an
/// OTLP endpoint nothing listens on and an ingest key file under `scratch`.
fn live_config(scratch: &Scratch) -> String {
    let key = scratch.path("ingest.key");
    std::fs::write(&key, "test-ingest-key\n").unwrap();
    serde_json::json!({
        "observability": {
            "enabled": true,
            "exporter": "otlp",
            "endpoint": "http://127.0.0.1:9",
            "ingestKeyFile": key,
            "flushIntervalSecs": 1,
            "liveOutput": { "enabled": true, "intervalMs": 20 }
        }
    })
    .to_string()
}

fn attended_env() -> AttendEnv {
    AttendEnv {
        session_id: Some(SESSION.to_string()),
        daemon_launched: false,
    }
}

fn request(workspace: &Path) -> StartRequest {
    StartRequest {
        issue: ISSUE,
        role: None,
        watch_pid: Some(4242),
        workspace: workspace.to_path_buf(),
        transcript: None,
        from_offset: None,
        max_age_secs: DEFAULT_MAX_AGE_SECS,
        idle_exit_secs: DEFAULT_IDLE_EXIT_SECS,
    }
}

fn line(value: &serde_json::Value) -> String {
    format!("{value}\n")
}

fn user_text(text: &str) -> String {
    line(&serde_json::json!({"type": "user", "message": {"role": "user", "content": text}}))
}

fn assistant_text(text: &str) -> String {
    line(&serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-10-03T21:30:00.000Z",
        "message": {"content": [{"type": "text", "text": text}]}
    }))
}

fn bash_call(id: &str, command: &str) -> String {
    line(&serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-10-03T21:30:01.000Z",
        "message": {"content": [{
            "type": "tool_use", "id": id, "name": "Bash",
            "input": {"command": command, "description": "run it"}
        }]}
    }))
}

fn agent_call(id: &str, prompt: &str) -> String {
    line(&serde_json::json!({
        "type": "assistant",
        "message": {"content": [{
            "type": "tool_use", "id": id, "name": "Agent",
            "input": {"subagent_type": "loom-builder", "prompt": prompt}
        }]}
    }))
}

fn tool_result(id: &str) -> String {
    line(&serde_json::json!({
        "type": "user",
        "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]}
    }))
}

/// The process tree of a call running `command`: the shell Claude Code 2.1.288
/// wraps every `Bash` call in (`ps` from inside a tool call on macOS showed
/// this shape), which is an ancestor of `lease ensure`.
fn caller_running(command: &str) -> Caller {
    let quoted = command.replace('\'', r#"'"'"'"#);
    Caller::from_argvs(&[
        vec![
            "bash".to_string(),
            "./.loom/scripts/worktree.sh".to_string(),
            "10116".to_string(),
        ],
        vec![
            "/bin/zsh".to_string(),
            "-c".to_string(),
            format!(
                "source /Users/op/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true \
                 && eval '{quoted}' < /dev/null && pwd -P >| /tmp/claude-e0c2-cwd"
            ),
        ],
    ])
}

/// A caller closure for `start_with`, for a call running `command`.
#[cfg(feature = "otlp")]
fn running(command: &str) -> impl FnOnce() -> Result<Caller, String> {
    let caller = caller_running(command);
    move || Ok(caller)
}

/// For paths that must decide before ever reading the process tree.
fn never_read() -> Result<Caller, String> {
    panic!("the process tree must not be read on this path")
}

/// `<projects>/<slug>/<SESSION>.jsonl` plus `<SESSION>/subagents/`.
struct Session {
    main: PathBuf,
    subagents: PathBuf,
}

fn session(projects: &Path, slug: &str, main_lines: &[String]) -> Session {
    let dir = projects.join(slug);
    let subagents = dir.join(SESSION).join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let main = dir.join(format!("{SESSION}.jsonl"));
    std::fs::write(&main, main_lines.concat()).unwrap();
    Session { main, subagents }
}

fn subagent(session: &Session, agent: &str, agent_type: Option<&str>, lines: &[String]) -> PathBuf {
    let path = session.subagents.join(format!("agent-{agent}.jsonl"));
    std::fs::write(&path, lines.concat()).unwrap();
    if let Some(agent_type) = agent_type {
        std::fs::write(
            session.subagents.join(format!("agent-{agent}.meta.json")),
            serde_json::json!({"agentType": agent_type, "toolUseId": "toolu_parent"}).to_string(),
        )
        .unwrap();
    }
    path
}

// ---------------------------------------------------------------------------
// Who counts as attended
// ---------------------------------------------------------------------------

#[test]
fn a_daemon_launched_process_tree_is_never_attended() {
    for (key, value) in [
        ("LOOM_WORK_ORIGIN", "autonomous"),
        ("LOOM_SWEEP_ID", "sweep-issue-42-1790000000"),
        ("GITHUB_ACTIONS", "true"),
    ] {
        let env = AttendEnv::from_lookup(|k| (k == key).then(|| value.to_string()));
        assert!(env.daemon_launched, "{key}={value}");
    }
    let attended = AttendEnv::from_lookup(|k| match k {
        SESSION_ID_ENV => Some(SESSION.to_string()),
        "LOOM_WORK_ORIGIN" => Some("interactive".to_string()),
        "CLAUDE_PID" => Some("57971".to_string()),
        _ => None,
    });
    assert!(!attended.daemon_launched);
    assert_eq!(attended.session_id.as_deref(), Some(SESSION));
}

#[test]
fn a_daemon_launched_agent_starts_nothing() {
    let scratch = Scratch::new("daemon");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let env = AttendEnv {
        daemon_launched: true,
        ..attended_env()
    };
    let outcome = start_with(&request(&root), &env, Some(&scratch.0), never_read, |_| {
        panic!("a daemon-launched agent must not spawn a tailer")
    });
    assert_eq!(outcome, Outcome::DaemonLaunched);
}

// ---------------------------------------------------------------------------
// No collector configured: a silent, immediate no-op
// ---------------------------------------------------------------------------

#[test]
fn with_no_collector_configured_starting_is_a_fast_no_op() {
    let configs = [
        (None, "observability is not enabled"),
        (
            Some(
                r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:9"}}"#,
            ),
            "live output is not enabled",
        ),
        (
            Some(r#"{"observability":{"enabled":true,"liveOutput":{"enabled":true}}}"#),
            "no usable OTLP exporter",
        ),
        (
            // A reserved placeholder is "not configured", exactly as the daemon
            // treats it: the ingest key must never be sent there.
            Some(
                r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"https://collector.example.com","liveOutput":{"enabled":true}}}"#,
            ),
            "no usable OTLP exporter",
        ),
    ];
    for (config, why) in configs {
        let scratch = Scratch::new("unconfigured");
        let root = checkout(&scratch, config);
        let began = Instant::now();
        let outcome =
            start_with(&request(&root), &attended_env(), Some(&scratch.0), never_read, |_| {
                panic!("nothing may be spawned when export is off")
            });
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "the no-op path must not wait on anything"
        );
        match outcome {
            Outcome::NotConfigured(reason) => assert!(reason.contains(why), "{reason}"),
            other => panic!("expected NotConfigured({why}), got {other:?}"),
        }
        assert!(!state_dir(&root).exists(), "an unconfigured start leaves nothing behind");
    }
}

#[test]
fn outside_a_loom_checkout_starting_is_a_no_op() {
    let scratch = Scratch::new("no-checkout");
    let outcome = start_with(&request(&scratch.0), &attended_env(), None, never_read, |_| {
        panic!("nothing may be spawned outside a checkout")
    });
    assert!(matches!(outcome, Outcome::NotConfigured(_)), "{outcome:?}");
}

/// A binary built without the `otlp` feature has no exporter for an
/// OTLP-only kind, so even a fully configured claim starts nothing.
#[cfg(not(feature = "otlp"))]
#[test]
fn a_build_without_otlp_never_starts_a_tailer() {
    let scratch = Scratch::new("no-otlp-build");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let outcome =
        start_with(&request(&root), &attended_env(), Some(&scratch.0), never_read, |_| {
            panic!("no exporter, no tailer")
        });
    match outcome {
        Outcome::NotConfigured(reason) => assert!(reason.contains("OTLP"), "{reason}"),
        other => panic!("expected NotConfigured, got {other:?}"),
    }
}

// The tests below need a configuration this binary can export under, which
// takes the `otlp` feature.
#[cfg(feature = "otlp")]
#[test]
fn without_a_session_id_nothing_is_guessed() {
    let scratch = Scratch::new("no-session");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let env = AttendEnv::default();
    let outcome = start_with(&request(&root), &env, Some(&scratch.0), never_read, |_| {
        panic!("no session, no tailer")
    });
    assert_eq!(outcome, Outcome::NoSession);
}

// ---------------------------------------------------------------------------
// Locating the caller's own transcript
// ---------------------------------------------------------------------------

const BUILDER_CLAIM: &str = "cd ~/dev/loom && ./.loom/scripts/worktree.sh 10116";

#[test]
fn the_subagent_running_this_processs_own_command_is_the_caller() {
    let scratch = Scratch::new("locate-subagent");
    let projects = scratch.path("projects");
    // The parent is waiting on its subagent: a pending Agent call, not a
    // Bash one, so it never matches.
    let session = session(
        &projects,
        "-Users-op-loom-ui",
        &[
            user_text("Build rjwalters/loom issue #10116"),
            agent_call("toolu_agent", "Build rjwalters/loom issue #10116"),
        ],
    );
    let caller = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[
            user_text("Build rjwalters/loom issue #10116"),
            assistant_text("Creating the worktree."),
            bash_call("toolu_wt", BUILDER_CLAIM),
        ],
    );
    // A sibling working on another issue, a sibling running a command that
    // names this issue, and one whose identical command already finished.
    subagent(
        &session,
        "a6c7b9ad2124db5ef",
        Some("loom-builder"),
        &[bash_call("toolu_other", "./.loom/scripts/worktree.sh 1325")],
    );
    subagent(
        &session,
        "afeedfacecafe0002",
        Some("loom-judge"),
        &[bash_call("toolu_view", "gh issue view 10116")],
    );
    subagent(
        &session,
        "afeedfacecafe0001",
        Some("loom-judge"),
        &[
            bash_call("toolu_done", BUILDER_CLAIM),
            tool_result("toolu_done"),
        ],
    );

    let located = locate(&projects, SESSION, &caller_running(BUILDER_CLAIM)).unwrap();
    assert_eq!(located.path, caller);
    assert_eq!(located.stream_id, format!("{SESSION}/agent-a4555677bacc80e00"));
    assert_eq!(located.agent_id.as_deref(), Some("a4555677bacc80e00"));
    assert_eq!(located.role.as_deref(), Some("builder"));
    assert_eq!(located.sweep_id(), "attended-755d0cb5-a4555677bacc80e00");
    // The run begins at the claim call's own line.
    let transcript = std::fs::read_to_string(&caller).unwrap();
    let claim_line = transcript.lines().last().unwrap();
    assert_eq!(located.from as usize, transcript.len() - claim_line.len() - 1);
}

#[test]
fn two_candidates_or_none_is_refused_rather_than_guessed() {
    let scratch = Scratch::new("locate-ambiguous");
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hi")]);
    let caller = caller_running("worktree.sh 10116");
    assert!(locate(&projects, SESSION, &caller).is_err(), "no candidate");
    subagent(&session, "a1", None, &[bash_call("t1", "worktree.sh 10116")]);
    subagent(&session, "a2", None, &[bash_call("t2", "worktree.sh 10116")]);
    let error = locate(&projects, SESSION, &caller).unwrap_err();
    assert!(error.contains("not guessing"), "{error}");
}

#[test]
fn a_session_id_that_is_not_a_plain_token_is_refused() {
    let scratch = Scratch::new("locate-bad-id");
    let caller = caller_running("worktree.sh 10116");
    assert!(locate(&scratch.0, "../../etc", &caller).is_err());
    assert!(locate(&scratch.0, "", &caller).is_err());
}

#[test]
fn an_explicit_transcript_path_is_described_without_locating() {
    let located = Located::from_path(Path::new(
        "/h/.claude/projects/-x/755d0cb5-f9ec/subagents/agent-a99.jsonl",
    ))
    .unwrap();
    assert_eq!(located.session_id, "755d0cb5-f9ec");
    assert_eq!(located.stream_id, "755d0cb5-f9ec/agent-a99");
    assert_eq!(located.sweep_id(), "attended-755d0cb5-a99");
    assert!(!located.is_top_level());
}

// ---------------------------------------------------------------------------
// The detached tailer
// ---------------------------------------------------------------------------

#[cfg(feature = "otlp")]
#[test]
fn a_configured_attended_claim_detaches_one_tailer_for_its_own_transcript() {
    let scratch = Scratch::new("spawn");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-op", &[user_text("hello")]);
    let caller = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[bash_call("toolu_wt", BUILDER_CLAIM)],
    );
    let mut argv: Vec<String> = Vec::new();
    let outcome = start_with(
        &request(&root),
        &attended_env(),
        Some(&projects),
        running(BUILDER_CLAIM),
        |command| {
            argv = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            Ok(31337)
        },
    );
    assert_eq!(
        outcome,
        Outcome::Started {
            pid: 31337,
            sweep_id: "attended-755d0cb5-a4555677bacc80e00".to_string()
        }
    );
    let arg = |flag: &str| {
        let at = argv.iter().position(|a| a == flag).unwrap();
        argv[at + 1].clone()
    };
    assert_eq!(argv[0], "live-output-attend");
    assert!(argv.contains(&"--foreground".to_string()));
    assert_eq!(arg("--issue"), "10116");
    assert_eq!(arg("--transcript"), caller.display().to_string());
    assert_eq!(arg("--from-offset"), "0");
    assert_eq!(arg("--watch-pid"), "4242");
    // No credential is ever handed over on the command line: the tailer
    // reads the key file itself.
    assert!(argv.iter().all(|a| !a.contains("test-ingest-key")), "{argv:?}");

    // The same claim step again while its tailer holds the transcript starts
    // nothing.
    let located = Located::from_path(&caller).unwrap();
    let lock = StreamLock::try_acquire(&lock_path(&root, &located))
        .unwrap()
        .unwrap();
    let again = start_with(
        &request(&root),
        &attended_env(),
        Some(&projects),
        running(BUILDER_CLAIM),
        |_| panic!("one tailer per claim"),
    );
    assert_eq!(again, Outcome::AlreadyRunning);
    lock.release();
}

#[test]
fn the_run_ends_when_the_session_exits_goes_idle_or_ages_out() {
    let limits = Limits {
        max_age: Duration::from_secs(100),
        idle_exit: Duration::from_secs(10),
    };
    let fresh = Some(Duration::from_secs(1));
    assert_eq!(end_reason(Duration::ZERO, true, fresh, limits), None);
    assert_eq!(end_reason(Duration::ZERO, false, fresh, limits), Some(EndReason::SessionExited));
    assert_eq!(
        end_reason(Duration::ZERO, true, Some(Duration::from_secs(10)), limits),
        Some(EndReason::Idle)
    );
    assert_eq!(end_reason(Duration::ZERO, true, None, limits), Some(EndReason::TranscriptGone));
    assert_eq!(
        end_reason(Duration::from_secs(100), true, fresh, limits),
        Some(EndReason::MaxAge)
    );
}

// ---------------------------------------------------------------------------
// End to end: what an attended run actually publishes
// ---------------------------------------------------------------------------

fn records(queue: &DurableQueue) -> Vec<SessionOutputRecord> {
    queue
        .peek_batch(1_000)
        .into_iter()
        .map(|envelope: TelemetryEnvelope| match envelope.record {
            TelemetryRecord::SessionOutput(record) => record,
            other => panic!("unexpected kind {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn an_attended_run_publishes_correlated_records_marked_attended() {
    let scratch = Scratch::new("drive");
    let projects = scratch.path("projects");
    let session = session(&projects, "-Users-op-loom-ui", &[user_text("hello")]);
    let transcript = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[
            user_text("Build rjwalters/loom issue #10116"),
            bash_call("toolu_wt", &format!("GH_TOKEN={CANARY} ./.loom/scripts/worktree.sh 10116")),
            tool_result("toolu_wt"),
            assistant_text("Reading the issue first."),
            assistant_text(&format!("Exported GH_TOKEN={CANARY} for the push.")),
        ],
    );
    // The run's lines begin at the claim call, right after the task prompt.
    let located = Located {
        from: user_text("Build rjwalters/loom issue #10116").len() as u64,
        ..Located::from_path(&transcript).unwrap()
    };
    let identity = identity(&located, ISSUE, None, Some("rjwalters/loom".to_string()));

    let queue = Arc::new(DurableQueue::open(scratch.path("queue.jsonl"), 1_000));
    let sink = SessionOutputSink::new(vec![queue.clone()], "host-test").unwrap();
    let live = ResolvedLiveOutput {
        interval: Duration::from_millis(5),
        heartbeat: Duration::from_secs(30),
        max_runs: 1,
    };
    let limits = Limits {
        max_age: Duration::from_secs(60),
        idle_exit: Duration::from_secs(600),
    };
    // Alive for the first two passes, then the session exits.
    let mut polls = 0;
    let watch = Watch {
        session_alive: move || {
            polls += 1;
            polls <= 2
        },
        newer_claim: || None,
    };
    let reason = drive(identity, &located, &scratch.0, &sink, live, limits, watch).await;
    assert_eq!(reason, EndReason::SessionExited);

    let published = records(&queue);
    let outputs: Vec<_> = published
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .collect();
    assert_eq!(outputs.len(), 2, "both assistant lines are published: {published:#?}");
    assert_eq!(outputs[0].text.as_deref(), Some("Reading the issue first."));
    assert!(
        published
            .iter()
            .any(|r| r.category == OutputCategory::ToolStart && r.tool.as_deref() == Some("Bash")),
        "tool metadata is published"
    );

    // Every record, content and status alike, carries the run's correlation
    // keys and the attended marker.
    for record in &published {
        let id = &record.identity;
        assert_eq!(id.issue, Some(ISSUE));
        assert_eq!(id.repo.as_deref(), Some("rjwalters/loom"));
        assert_eq!(id.role.as_deref(), Some("builder"));
        assert_eq!(id.sweep_id.as_deref(), Some("attended-755d0cb5-a4555677bacc80e00"));
        assert_eq!(
            id.session_id.as_deref(),
            Some(format!("{SESSION}/agent-a4555677bacc80e00").as_str())
        );
        assert_eq!(id.launch, Launch::Attended);
        assert_eq!(id.runtime, "claude");
        assert!(!record.event_id.is_empty());
    }
    // Content records are keyed by the transcript's own line numbers.
    assert!(outputs
        .iter()
        .all(|r| r.stream_id == format!("{SESSION}/agent-a4555677bacc80e00")));
    assert!(outputs[0].sequence < outputs[1].sequence);

    // The run opens and closes explicitly.
    let first = published.first().unwrap();
    assert_eq!(first.category, OutputCategory::Coverage);
    let last = published.last().unwrap();
    assert_eq!(last.category, OutputCategory::Coverage);
    assert_eq!(last.coverage, Coverage::Ended);

    // No credential value crosses: not from assistant text (redacted), and
    // not from the tool call's input (never read into a record at all).
    let wire = serde_json::to_string(&queue.peek_batch(1_000)).unwrap();
    assert!(!wire.contains(CANARY), "a credential reached the export queue");
    assert!(!wire.contains("worktree.sh"), "tool input reached the export queue");
}

#[tokio::test]
async fn an_attended_run_starts_from_its_retained_tail_and_says_what_it_skipped() {
    let scratch = Scratch::new("drive-backlog");
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hello")]);
    let mut history: Vec<String> = (0..40)
        .map(|n| assistant_text(&format!("step {n}")))
        .collect();
    history.push(bash_call("toolu_wt", "worktree.sh 10116"));
    let transcript = subagent(&session, "a1", Some("loom-doctor"), &history);
    let located = Located::from_path(&transcript).unwrap();
    let queue = Arc::new(DurableQueue::open(scratch.path("queue.jsonl"), 1_000));
    let sink = SessionOutputSink::new(vec![queue.clone()], "host-test").unwrap();
    let live = ResolvedLiveOutput {
        interval: Duration::from_millis(5),
        heartbeat: Duration::from_secs(30),
        max_runs: 1,
    };
    let limits = Limits {
        max_age: Duration::from_secs(60),
        idle_exit: Duration::from_secs(600),
    };
    let mut polls = 0;
    drive(
        identity(&located, ISSUE, None, None),
        &located,
        &scratch.0,
        &sink,
        live,
        limits,
        Watch {
            session_alive: move || {
                polls += 1;
                polls <= 1
            },
            newer_claim: || None,
        },
    )
    .await;
    let published = records(&queue);
    let gap = published
        .iter()
        .find(|r| r.category == OutputCategory::Gap)
        .expect("an explicit backlog gap");
    assert_eq!(gap.gap_reason.as_deref(), Some("backlog_skipped"));
    assert!(published
        .iter()
        .all(|r| r.identity.role.as_deref() == Some("doctor")));
    // The repo stays unscoped rather than guessed when the checkout has no
    // `origin` remote.
    assert!(published.iter().all(|r| r.identity.repo.is_none()));
}

/// The whole detached-tailer path against a collector that is not there: it
/// must finish promptly and quietly, not hang on export.
#[cfg(feature = "otlp")]
#[tokio::test]
#[serial_test::serial]
async fn an_unreachable_collector_never_holds_the_tailer_up() {
    let scratch = Scratch::new("unreachable");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hello")]);
    let transcript = subagent(
        &session,
        "a1",
        Some("loom-builder"),
        &[
            assistant_text("working"),
            bash_call("toolu_wt", "worktree.sh 10116"),
        ],
    );
    // A pid that has already exited, so the run ends on its first pass.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    let began = Instant::now();
    let outcome = run_foreground(
        &StartRequest {
            transcript: Some(transcript),
            watch_pid: Some(dead_pid),
            ..request(&root)
        },
        &attended_env(),
    )
    .await;
    assert!(
        matches!(
            &outcome,
            Outcome::Ended {
                reason: EndReason::SessionExited,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(
        began.elapsed() < FINAL_FLUSH + Duration::from_secs(5),
        "the final drain is bounded: {:?}",
        began.elapsed()
    );
    let leftovers: Vec<_> = std::fs::read_dir(state_dir(&root))
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "queue and lock files are removed: {leftovers:?}");
}

/// The scoping regressions from Judge's review of PR #10121: which agent is
/// calling, and which of its lines belong to the claimed issue.
#[path = "attended_scope_tests.rs"]
mod scope;
