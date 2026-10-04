//! Regressions for Judge's two blocking findings on PR #10121 (head
//! `80172f47`), one test per repro Judge measured with scratch tests:
//!
//! 1. A run kept publishing after the agent's work on the issue ended: an
//!    unrelated reply was published under the issue, and a second claim in
//!    the same session was refused, so its work went to the first issue.
//! 2. `locate` accepted a single wrong match: a Doctor whose claim step never
//!    spells its issue number, next to a sibling whose running command
//!    happened to contain it.

use super::*;

/// The Doctor's documented claim step (`defaults/roles/doctor.md`): the
/// issue number comes from `BASH_REMATCH`, so it never appears literally.
const DOCTOR_CLAIM: &str = "PR_BRANCH=$(gh pr view 10121 --json headRefName --jq '.headRefName')\n\
                            if [[ \"$PR_BRANCH\" =~ ^feature/issue-([0-9]+)$ ]]; then\n  \
                            ISSUE_NUM=\"${BASH_REMATCH[1]}\"\n  \
                            ./.loom/scripts/worktree.sh \"$ISSUE_NUM\"\nfi";

/// A sibling Judge partway through a CI wait: its command names 60 by chance.
const JUDGE_CI_WAIT: &str = "while ci_still_pending 700; do sleep 60; done";

const SECOND_ISSUE: u32 = 10117;
const SECOND_CLAIM: &str = "./.loom/scripts/worktree.sh 10117";

fn coordinator_message(text: &str) -> String {
    line(&serde_json::json!({
        "type": "user",
        "isMeta": true,
        "origin": {"kind": "coordinator"},
        "message": {"role": "user", "content": format!(
            "The coordinator sent a message while you were working:\n{text}"
        )}
    }))
}

fn task_notification() -> String {
    line(&serde_json::json!({
        "type": "user",
        "isMeta": true,
        "origin": {"kind": "task-notification"},
        "message": {"role": "user", "content": "[SYSTEM NOTIFICATION - NOT USER INPUT]"}
    }))
}

fn skill_expansion() -> String {
    line(&serde_json::json!({
        "type": "user",
        "isMeta": true,
        "sourceToolUseID": "toolu_skill",
        "message": {"role": "user", "content": [{"type": "text", "text": "# Pull Request Judge"}]}
    }))
}

fn stop_hook_feedback() -> String {
    line(&serde_json::json!({
        "type": "user",
        "isMeta": true,
        "message": {"role": "user", "content": "Stop hook feedback:\nSTOP BLOCKED"}
    }))
}

fn interrupt() -> String {
    line(&serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "[Request interrupted by user]"}
        ]}
    }))
}

fn append(path: &Path, lines: &[String]) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(lines.concat().as_bytes()).unwrap();
}

/// Byte offset where `path`'s last line starts.
fn last_line_at(path: &Path) -> u64 {
    let text = std::fs::read_to_string(path).unwrap();
    let body = text.strip_suffix('\n').unwrap();
    body.rfind('\n').map_or(0, |at| at as u64 + 1)
}

fn quick() -> (ResolvedLiveOutput, Limits) {
    (
        ResolvedLiveOutput {
            interval: Duration::from_millis(5),
            heartbeat: Duration::from_secs(30),
            max_runs: 1,
        },
        // Short enough that a missed boundary fails the test instead of
        // hanging it.
        Limits {
            max_age: Duration::from_secs(5),
            idle_exit: Duration::from_secs(600),
        },
    )
}

fn texts(published: &[SessionOutputRecord]) -> Vec<String> {
    published
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .filter_map(|r| r.text.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Finding 1: a run covers only its own agent's work on its own issue
// ---------------------------------------------------------------------------

const SLASH_CLAIM: &str = "loom-daemon lease ensure 10116 --watch-pid \"$CLAUDE_PID\"";

/// `/loom:builder 42` typed into a top-level session: the session's own
/// transcript is the caller's.
fn slash_command_session(projects: &Path) -> Session {
    session(
        projects,
        "-Users-op-dev-loom",
        &[
            user_text(
                "<command-name>/loom:builder</command-name><command-args>10116</command-args>",
            ),
            bash_call("toolu_claim", SLASH_CLAIM),
        ],
    )
}

#[test]
fn a_slash_command_caller_is_located_as_a_top_level_session() {
    let scratch = Scratch::new("top-level-locate");
    let projects = scratch.path("projects");
    let session = slash_command_session(&projects);
    let located = locate(&projects, SESSION, &caller_running(SLASH_CLAIM)).unwrap();
    assert_eq!(located.path, session.main);
    assert!(located.is_top_level());
}

/// Judge's repro: `/loom:builder 42` typed into a top-level session, whose
/// transcript goes on to hold the operator's later, unrelated work.
#[cfg(feature = "otlp")]
#[test]
fn a_top_level_session_is_refused_with_a_recorded_reason() {
    let scratch = Scratch::new("top-level");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let claim = SLASH_CLAIM;
    let session = slash_command_session(&projects);

    let outcome =
        start_with(&request(&root), &attended_env(), Some(&projects), running(claim), |_| {
            panic!("a top-level session must not be followed")
        });
    assert_eq!(outcome, Outcome::TopLevelSession);
    let why = outcome.describe(ISSUE);
    assert!(why.contains("top-level session") && why.contains("#10129"), "{why}");
    assert!(!state_dir(&root).exists(), "a refused start records nothing on disk");

    // Naming the session's own file explicitly is refused the same way.
    let explicit = StartRequest {
        transcript: Some(session.main.clone()),
        ..request(&root)
    };
    let outcome = start_with(&explicit, &attended_env(), Some(&projects), never_read, |_| {
        panic!("a top-level transcript must not be followed")
    });
    assert_eq!(outcome, Outcome::TopLevelSession);
}

/// The detached tailer itself refuses a top-level transcript too, so no path
/// into it publishes one.
#[cfg(feature = "otlp")]
#[tokio::test]
async fn the_foreground_tailer_refuses_a_top_level_transcript() {
    let scratch = Scratch::new("top-level-fg");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let session = session(&scratch.path("projects"), "-p", &[user_text("hello")]);
    let outcome = run_foreground(
        &StartRequest {
            transcript: Some(session.main),
            ..request(&root)
        },
        &attended_env(),
    )
    .await;
    assert_eq!(outcome, Outcome::TopLevelSession);
}

/// The subagent form of Judge's "unrelated post-role reply": after the claimed
/// work, a coordinator hands the agent new, unrelated work and it replies.
/// That reply, and anything before the claim, is never published as the
/// issue. The harness's own lines inside the task do not end the run.
#[tokio::test]
async fn a_reply_after_the_agents_next_prompt_is_never_published() {
    let scratch = Scratch::new("next-task");
    let session = session(&scratch.path("projects"), "-p", &[user_text("hello")]);
    let transcript = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[
            user_text("Build rjwalters/loom issue #10116"),
            assistant_text("Before the claim."),
            bash_call("toolu_wt", "./.loom/scripts/worktree.sh 10116"),
        ],
    );
    let from = last_line_at(&transcript);
    append(
        &transcript,
        &[
            tool_result("toolu_wt"),
            assistant_text("Worktree ready."),
            task_notification(),
            skill_expansion(),
            stop_hook_feedback(),
            assistant_text("Still on 10116."),
        ],
    );
    let located = Located {
        from,
        ..Located::from_path(&transcript).unwrap()
    };
    let queue = Arc::new(DurableQueue::open(scratch.path("queue.jsonl"), 1_000));
    let sink = SessionOutputSink::new(vec![queue.clone()], "host-test").unwrap();
    let (live, limits) = quick();
    // The new task arrives while the run is live.
    let late = transcript.clone();
    let arrive = async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        append(
            &late,
            &[
                coordinator_message("New work: review 2AMLogic/2am#2114."),
                assistant_text("UNRELATED: reviewing 2am#2114."),
            ],
        );
    };
    let run = drive(
        identity(&located, ISSUE, None, None),
        &located,
        &scratch.0,
        &sink,
        live,
        limits,
        Watch {
            session_alive: || true,
            newer_claim: || None,
        },
    );
    let (reason, ()) = tokio::join!(run, arrive);
    assert_eq!(reason, EndReason::NextTask);

    let published = records(&queue);
    assert_eq!(texts(&published), ["Worktree ready.", "Still on 10116."]);
    let wire = serde_json::to_string(&queue.peek_batch(1_000)).unwrap();
    assert!(!wire.contains("UNRELATED"), "the next task's reply was published as #{ISSUE}");
    assert!(!wire.contains("Before the claim"), "a line before the claim was published");
    // Lines before the claim are not this run's, so they are not reported as
    // a backlog it skipped either.
    assert!(published.iter().all(|r| r.category != OutputCategory::Gap), "{published:#?}");
    assert_eq!(published.last().unwrap().coverage, Coverage::Ended);
}

/// Judge's "second claim in the same session": one agent claims #10116, then
/// #10117. The #10116 run ends at the #10117 claim's line, and the #10117 run
/// starts there. Neither publishes the other's lines.
#[tokio::test]
async fn a_second_claim_takes_the_transcript_over_at_its_own_line() {
    let scratch = Scratch::new("second-claim");
    let session = session(&scratch.path("projects"), "-p", &[user_text("hello")]);
    let transcript = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[
            user_text("Build #10116, then #10117"),
            bash_call("toolu_wt", "./.loom/scripts/worktree.sh 10116"),
        ],
    );
    let first = Located {
        from: last_line_at(&transcript),
        ..Located::from_path(&transcript).unwrap()
    };
    append(
        &transcript,
        &[
            tool_result("toolu_wt"),
            assistant_text("Built 10116."),
            bash_call("toolu_wt2", SECOND_CLAIM),
        ],
    );
    let second = Located {
        from: last_line_at(&transcript),
        ..Located::from_path(&transcript).unwrap()
    };
    append(&transcript, &[tool_result("toolu_wt2"), assistant_text("Built 10117.")]);

    // What the second start records.
    let claim_file = scratch.path("state/stream.claim");
    let first_claim = segment::Claim {
        issue: ISSUE,
        from: first.from,
    };
    let second_claim = segment::Claim {
        issue: SECOND_ISSUE,
        from: second.from,
    };
    segment::write_claim(&claim_file, second_claim).unwrap();

    let (live, limits) = quick();
    let first_queue = Arc::new(DurableQueue::open(scratch.path("first.jsonl"), 1_000));
    let first_sink = SessionOutputSink::new(vec![first_queue.clone()], "host-test").unwrap();
    let reason = drive(
        identity(&first, ISSUE, None, None),
        &first,
        &scratch.0,
        &first_sink,
        live,
        limits,
        Watch {
            session_alive: || true,
            newer_claim: || segment::newer_claim(&claim_file, first_claim),
        },
    )
    .await;
    assert_eq!(reason, EndReason::Superseded);
    let published = records(&first_queue);
    assert_eq!(texts(&published), ["Built 10116."]);
    assert!(published.iter().all(|r| r.identity.issue == Some(ISSUE)));

    let second_queue = Arc::new(DurableQueue::open(scratch.path("second.jsonl"), 1_000));
    let second_sink = SessionOutputSink::new(vec![second_queue.clone()], "host-test").unwrap();
    let mut polls = 0;
    let reason = drive(
        identity(&second, SECOND_ISSUE, None, None),
        &second,
        &scratch.0,
        &second_sink,
        live,
        limits,
        Watch {
            session_alive: move || {
                polls += 1;
                polls <= 3
            },
            newer_claim: || segment::newer_claim(&claim_file, second_claim),
        },
    )
    .await;
    assert_eq!(reason, EndReason::SessionExited);
    let published = records(&second_queue);
    assert_eq!(texts(&published), ["Built 10117."]);
    assert!(published
        .iter()
        .all(|r| r.identity.issue == Some(SECOND_ISSUE)));
}

/// The same handover through the real start path: a second claim on a
/// transcript a tailer still holds is started, not refused as
/// `AlreadyRunning`, and is recorded as the transcript's newest claim.
#[cfg(feature = "otlp")]
#[test]
fn a_second_claim_is_started_not_refused() {
    let scratch = Scratch::new("second-claim-start");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-op", &[user_text("hello")]);
    let transcript = subagent(
        &session,
        "a1",
        Some("loom-builder"),
        &[bash_call("toolu_wt", "./.loom/scripts/worktree.sh 10116")],
    );
    let first = start_with(
        &request(&root),
        &attended_env(),
        Some(&projects),
        running("./.loom/scripts/worktree.sh 10116"),
        |_| Ok(31337),
    );
    assert!(matches!(first, Outcome::Started { .. }), "{first:?}");
    let located = Located::from_path(&transcript).unwrap();
    // The first claim's tailer is running and holds the transcript.
    let held = StreamLock::try_acquire(&lock_path(&root, &located))
        .unwrap()
        .unwrap();

    append(
        &transcript,
        &[
            tool_result("toolu_wt"),
            bash_call("toolu_wt2", SECOND_CLAIM),
        ],
    );
    let second_from = last_line_at(&transcript);
    let mut argv: Vec<String> = Vec::new();
    let second = start_with(
        &StartRequest {
            issue: SECOND_ISSUE,
            ..request(&root)
        },
        &attended_env(),
        Some(&projects),
        running(SECOND_CLAIM),
        |command| {
            argv = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            Ok(31338)
        },
    );
    assert!(matches!(second, Outcome::Started { pid: 31338, .. }), "{second:?}");
    let from_arg = argv.iter().position(|a| a == "--from-offset").unwrap() + 1;
    assert_eq!(argv[from_arg], second_from.to_string());
    let claim_file = claim_path(&root, &located);
    assert_eq!(
        segment::read_claim(&claim_file),
        Some(segment::Claim {
            issue: SECOND_ISSUE,
            from: second_from
        })
    );
    // So the first run, which owns everything from offset 0, sees where the
    // agent moved on.
    let first_claim = segment::Claim {
        issue: ISSUE,
        from: 0,
    };
    assert_eq!(segment::newer_claim(&claim_file, first_claim), Some(second_from));
    held.release();
}

/// A tailer whose claim was already overtaken before it got the transcript
/// stops at once instead of waiting out the handover.
#[cfg(feature = "otlp")]
#[tokio::test]
async fn a_tailer_overtaken_before_it_starts_stops_at_once() {
    let scratch = Scratch::new("overtaken");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let session = session(&scratch.path("projects"), "-p", &[user_text("hello")]);
    let transcript = subagent(&session, "a1", Some("loom-builder"), &[assistant_text("x")]);
    let located = Located::from_path(&transcript).unwrap();
    let held = StreamLock::try_acquire(&lock_path(&root, &located))
        .unwrap()
        .unwrap();
    segment::write_claim(
        &claim_path(&root, &located),
        segment::Claim {
            issue: SECOND_ISSUE,
            from: 10,
        },
    )
    .unwrap();
    let began = Instant::now();
    let outcome = run_foreground(
        &StartRequest {
            transcript: Some(transcript),
            from_offset: Some(0),
            ..request(&root)
        },
        &attended_env(),
    )
    .await;
    assert!(
        matches!(
            outcome,
            Outcome::Ended {
                reason: EndReason::Superseded,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
    held.release();
}

// ---------------------------------------------------------------------------
// Finding 2: the caller is proven through its process tree, never guessed
// ---------------------------------------------------------------------------

/// Judge's repro: a Doctor running its documented claim step, which never
/// spells its issue number, next to a sibling Judge whose CI wait happens to
/// contain it. The text match took the Judge; the process tree takes the
/// Doctor.
#[test]
fn a_doctor_is_found_by_its_own_command_not_by_a_sibling_naming_its_issue() {
    let scratch = Scratch::new("doctor-sibling");
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hello")]);
    let doctor = subagent(
        &session,
        "adoctor",
        Some("loom-doctor"),
        &[
            user_text("Fix PR #10121"),
            bash_call("toolu_doctor", DOCTOR_CLAIM),
        ],
    );
    let judge =
        subagent(&session, "ajudge", Some("loom-judge"), &[bash_call("toolu_ci", JUDGE_CI_WAIT)]);

    let located = locate(&projects, SESSION, &caller_running(DOCTOR_CLAIM)).unwrap();
    assert_eq!(located.path, doctor);
    assert_ne!(located.path, judge);
    assert_eq!(located.role.as_deref(), Some("doctor"));

    // A caller whose own command is in no transcript gets nothing, even
    // though a sibling's running command names the number.
    let error =
        locate(&projects, SESSION, &caller_running("./.loom/scripts/worktree.sh 60")).unwrap_err();
    assert!(error.contains("none of"), "{error}");
}

/// The same Doctor through the real start path: the tailer it starts follows
/// the Doctor's transcript.
#[cfg(feature = "otlp")]
#[test]
fn a_doctors_claim_starts_a_tailer_on_the_doctors_transcript() {
    let scratch = Scratch::new("doctor-start");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hello")]);
    let doctor = subagent(
        &session,
        "adoctor",
        Some("loom-doctor"),
        &[bash_call("toolu_doctor", DOCTOR_CLAIM)],
    );
    subagent(&session, "ajudge", Some("loom-judge"), &[bash_call("toolu_ci", JUDGE_CI_WAIT)]);
    let mut transcript = String::new();
    let outcome = start_with(
        &StartRequest {
            issue: 60,
            ..request(&root)
        },
        &attended_env(),
        Some(&projects),
        running(DOCTOR_CLAIM),
        |command| {
            let argv: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            transcript = argv[argv.iter().position(|a| a == "--transcript").unwrap() + 1].clone();
            Ok(1)
        },
    );
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
    assert_eq!(transcript, doctor.display().to_string());
}

/// When the process tree cannot be read, nothing is published and the reason
/// is recorded.
#[cfg(feature = "otlp")]
#[test]
fn an_unreadable_process_tree_publishes_nothing() {
    let scratch = Scratch::new("no-tree");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[user_text("hello")]);
    subagent(&session, "a1", Some("loom-builder"), &[bash_call("t", "worktree.sh 10116")]);
    let outcome = start_with(
        &request(&root),
        &attended_env(),
        Some(&projects),
        || Err("parent shells could not be read".to_string()),
        |_| panic!("an unconfirmed caller must not be followed"),
    );
    assert_eq!(outcome, Outcome::NotLocated("parent shells could not be read".to_string()));
}

// ---------------------------------------------------------------------------
// The segment rules
// ---------------------------------------------------------------------------

#[test]
fn only_direction_from_outside_starts_the_agents_next_task() {
    let bytes = |text: String| text.trim_end().as_bytes().to_vec();
    for (shape, ends) in [
        (coordinator_message("new work"), true),
        (interrupt(), true),
        (user_text("a person's prompt"), true),
        (
            line(&serde_json::json!({
                "type": "user", "isMeta": true, "origin": {"kind": "some-future-kind"},
                "message": {"content": "?"}
            })),
            true,
        ),
        (tool_result("toolu_x"), false),
        (task_notification(), false),
        (skill_expansion(), false),
        (stop_hook_feedback(), false),
        (assistant_text("hello"), false),
        (bash_call("t", "ls"), false),
        ("not json".to_string(), false),
    ] {
        assert_eq!(segment::starts_next_task(&bytes(shape.clone())), ends, "{shape}");
    }
}

#[test]
fn a_segment_reads_nothing_past_its_end() {
    let scratch = Scratch::new("segment");
    let path = scratch.path("t.jsonl");
    let lines = [
        assistant_text("one"),
        assistant_text("two"),
        coordinator_message("next"),
        assistant_text("three"),
    ];
    std::fs::write(&path, lines.concat()).unwrap();
    let boundary = (lines[0].len() + lines[1].len()) as u64;

    let mut segment = Segment::starting_at(0);
    segment.scan(&path);
    assert_eq!(segment.limit(), boundary);
    assert_eq!(segment.finished(boundary - 1), None);
    assert_eq!(segment.finished(boundary), Some(EndReason::NextTask));

    // A newer claim before the boundary ends the segment sooner; one after it
    // changes nothing.
    let mut segment = Segment::starting_at(0);
    segment.supersede(lines[0].len() as u64);
    segment.scan(&path);
    assert_eq!(segment.limit(), lines[0].len() as u64);
    assert_eq!(segment.finished(lines[0].len() as u64), Some(EndReason::Superseded));
    let mut segment = Segment::starting_at(0);
    segment.scan(&path);
    segment.supersede(boundary + 5);
    assert_eq!(segment.finished(boundary), Some(EndReason::NextTask));

    // A partial trailing line is not checked, so it cannot be read either.
    let partial = scratch.path("partial.jsonl");
    std::fs::write(&partial, format!("{}{{\"type\":\"user\"", lines[0])).unwrap();
    let mut segment = Segment::starting_at(0);
    segment.scan(&partial);
    assert_eq!(segment.limit(), lines[0].len() as u64);
}

#[test]
fn a_finished_run_leaves_a_newer_claim_in_place() {
    let scratch = Scratch::new("claims");
    let path = scratch.path("state/x.claim");
    let older = segment::Claim { issue: 1, from: 0 };
    let newer = segment::Claim { issue: 2, from: 99 };
    segment::write_claim(&path, newer).unwrap();
    segment::clear_claim(&path, older);
    assert_eq!(segment::read_claim(&path), Some(newer));
    assert_eq!(segment::newer_claim(&path, older), Some(99));
    assert_eq!(segment::newer_claim(&path, newer), None);
    segment::clear_claim(&path, newer);
    assert_eq!(segment::read_claim(&path), None);
}

/// Queue files are per tailer process, so a tailer that died before its own
/// cleanup would otherwise leave its queue behind for good. The next tailer of
/// the same transcript removes it, and leaves a running one's alone.
#[test]
fn a_dead_tailers_queue_is_swept_and_a_live_ones_kept() {
    let scratch = Scratch::new("sweep");
    let dir = scratch.path("state");
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let mut running = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let live = running.id();
    let file = |name: String| {
        let path = dir.join(name);
        std::fs::write(&path, "{}\n").unwrap();
        path
    };
    let dead_queue = file(format!("s1-agent-a1.{dead}.otlp-0.jsonl"));
    let live_queue = file(format!("s1-agent-a1.{live}.otlp-0.jsonl"));
    let other_stream = file(format!("s1-agent-a12.{dead}.otlp-0.jsonl"));
    let claim = file("s1-agent-a1.claim".to_string());
    sweep_dead_queues(&dir, "s1-agent-a1");
    let _ = running.kill();
    let _ = running.wait();
    assert!(!dead_queue.exists());
    assert!(live_queue.exists(), "a running tailer's queue is kept");
    assert!(other_stream.exists(), "another transcript's files are not this tailer's");
    assert!(claim.exists());
}
