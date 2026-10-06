//! #10125 regressions:
//!
//! 1. A start's diagnostic reached nobody on the main path, because
//!    `worktree.sh` discards `lease ensure`'s stderr.
//! 2. A finished foreground subagent's run stayed open until the 30-minute
//!    idle limit, because the watched pid is the operator's session.
//! 3. A failed spawn left its probe lock, and a killed tailer left its lock
//!    and queue files, for good.

use super::*;
use crate::telemetry::kinds::session_output::OutputCategory;

fn append(path: &Path, lines: &[String]) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(lines.concat().as_bytes()).unwrap();
}

fn texts(published: &[SessionOutputRecord]) -> Vec<String> {
    published
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .filter_map(|r| r.text.clone())
        .collect()
}

/// A subagent transcript with the given `.meta.json`.
fn subagent_with_meta(
    session: &Session,
    agent: &str,
    meta: &serde_json::Value,
    lines: &[String],
) -> PathBuf {
    let path = session.subagents.join(format!("agent-{agent}.jsonl"));
    std::fs::write(&path, lines.concat()).unwrap();
    std::fs::write(session.subagents.join(format!("agent-{agent}.meta.json")), meta.to_string())
        .unwrap();
    path
}

fn foreground(tool_use_id: &str) -> serde_json::Value {
    serde_json::json!({
        "agentType": "loom-builder",
        "toolUseId": tool_use_id,
        "requestShape": "foreground"
    })
}

fn a_dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

// ---------------------------------------------------------------------------
// 1. The last start outcome is kept where an operator can read it
// ---------------------------------------------------------------------------

#[test]
fn the_start_outcome_is_recorded_in_last_start_log() {
    let scratch = Scratch::new("last-start");
    let root = checkout(&scratch, None);
    let worktree = root.join("sub").join("dir");
    std::fs::create_dir_all(&worktree).unwrap();
    let outcome = Outcome::NotConfigured("observability is not enabled".to_string());
    upkeep::record_start(&worktree, ISSUE, &outcome);
    let path = state_dir(&root).join(upkeep::LAST_START_FILE);
    assert_eq!(upkeep::last_start_path(&worktree).as_deref(), Some(path.as_path()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with(&format!("{}\n", outcome.describe(ISSUE))), "{text}");
    assert_eq!(text.lines().count(), 1);

    // The next start replaces it: the file says what happened last.
    upkeep::record_start(&root, 42, &Outcome::AlreadyRunning);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("issue #42: a live-output tailer already follows"), "{text}");
    let leftovers: Vec<_> = std::fs::read_dir(state_dir(&root))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(leftovers, [upkeep::LAST_START_FILE], "no staging file is left");
}

#[test]
fn outside_a_checkout_no_start_outcome_is_written() {
    let scratch = Scratch::new("last-start-none");
    upkeep::record_start(&scratch.0, ISSUE, &Outcome::NoSession);
    assert!(upkeep::last_start_path(&scratch.0).is_none());
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------
// 3. Stale state files
// ---------------------------------------------------------------------------

#[test]
fn stale_files_are_swept_and_a_running_tailers_are_kept() {
    let scratch = Scratch::new("sweep-stale");
    let dir = scratch.path("state");
    std::fs::create_dir_all(&dir).unwrap();
    let dead = a_dead_pid();
    let mut running = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let live = running.id();
    let file = |name: &str| {
        let path = dir.join(name);
        std::fs::write(&path, "{}\n").unwrap();
        path
    };
    let old = |path: &Path| {
        let when = SystemTime::now() - Duration::from_secs(DEFAULT_MAX_AGE_SECS + 60);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    };

    // A killed tailer's files, for two different transcripts.
    let free_lock = file("s1-agent-a1.lock");
    let dead_queue = file(&format!("s1-agent-a1.{dead}.otlp-0.jsonl"));
    let other_dead_queue = file(&format!("s2-agent-b2.{dead}.otlp-1.jsonl"));
    let old_claim = file("s1-agent-a1.claim");
    old(&old_claim);
    // A running tailer's files: its lock is held.
    let held_lock = dir.join("s3-agent-c3.lock");
    let held = StreamLock::try_acquire(&held_lock).unwrap().unwrap();
    let held_claim = file("s3-agent-c3.claim");
    old(&held_claim);
    let live_queue = file(&format!("s3-agent-c3.{live}.otlp-0.jsonl"));
    // A claim a just-spawned tailer is about to use: recent, lock not yet taken.
    let fresh_claim = file("s4-agent-d4.claim");
    let last_start = file(upkeep::LAST_START_FILE);

    upkeep::sweep_stale(&dir);
    let _ = running.kill();
    let _ = running.wait();

    assert!(!free_lock.exists(), "a lock nobody holds is removed");
    assert!(
        !dead_queue.exists() && !other_dead_queue.exists(),
        "dead tailers' queues are removed"
    );
    assert!(!old_claim.exists(), "an old claim with a free lock is removed");
    assert!(held_lock.exists(), "a held lock is never removed");
    assert!(held_claim.exists(), "a running tailer's claim is kept, however old");
    assert!(live_queue.exists(), "a running tailer's queue is kept");
    assert!(fresh_claim.exists(), "a recent claim is kept");
    assert!(!dir.join("s4-agent-d4.lock").exists(), "checking a claim leaves no lock behind");
    assert!(last_start.exists());
    // The held lock still excludes a second tailer.
    assert!(StreamLock::try_acquire(&held_lock).unwrap().is_none());
    held.release();
}

/// A lock taken on a file that was unlinked between its open and its `flock`
/// excludes nobody, so acquiring always ends on the file now at the path.
#[test]
fn a_lock_is_always_on_the_file_at_its_path() {
    let scratch = Scratch::new("lock-inode");
    let path = scratch.path("s.lock");
    let first = StreamLock::try_acquire(&path).unwrap().unwrap();
    // A sweep (or a finishing holder) removes it while held.
    first.release();
    assert!(!path.exists());
    let second = StreamLock::try_acquire(&path).unwrap().unwrap();
    assert!(path.exists(), "the new holder locks a file that is at the path");
    assert!(StreamLock::try_acquire(&path).unwrap().is_none());
    second.release();
}

#[cfg(feature = "otlp")]
#[test]
fn a_failed_spawn_leaves_no_probe_lock() {
    let scratch = Scratch::new("spawn-fails");
    let root = checkout(&scratch, Some(&live_config(&scratch)));
    let projects = scratch.path("projects");
    let session = session(&projects, "-op", &[user_text("hello")]);
    let caller = subagent(
        &session,
        "a4555677bacc80e00",
        Some("loom-builder"),
        &[bash_call("toolu_wt", "./.loom/scripts/worktree.sh 10116")],
    );
    let located = Located::from_path(&caller).unwrap();
    // A retried claim step: its claim is already recorded, so the start
    // probes the lock before spawning.
    segment::write_claim(
        &claim_path(&root, &located),
        segment::Claim {
            issue: ISSUE,
            from: 0,
        },
    )
    .unwrap();
    let outcome = start_with(
        &request(&root),
        &attended_env(),
        Some(&projects),
        running("./.loom/scripts/worktree.sh 10116"),
        |_| Err(std::io::Error::other("no such binary")),
    );
    assert!(matches!(outcome, Outcome::SpawnFailed(_)), "{outcome:?}");
    assert!(!lock_path(&root, &located).exists(), "the probe lock is removed");
}

// ---------------------------------------------------------------------------
// 2. A foreground subagent's run ends at its parent's result
// ---------------------------------------------------------------------------

#[test]
fn only_a_foreground_subagent_is_watched_for_its_result() {
    let scratch = Scratch::new("return-attach");
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[agent_call("toolu_fg", "Build #42")]);
    let fg = subagent_with_meta(&session, "afg", &foreground("toolu_fg"), &[user_text("go")]);
    let mut bg_meta = foreground("toolu_bg");
    bg_meta["requestShape"] = "background".into();
    let bg = subagent_with_meta(&session, "abg", &bg_meta, &[user_text("go")]);
    let mut unknown_meta = foreground("toolu_old");
    unknown_meta.as_object_mut().unwrap().remove("requestShape");
    let unknown = subagent_with_meta(&session, "aold", &unknown_meta, &[user_text("go")]);

    assert!(returned::ReturnWatch::attach(&fg).is_some());
    // A background agent's result is written when it is launched.
    assert!(returned::ReturnWatch::attach(&bg).is_none());
    assert!(returned::ReturnWatch::attach(&unknown).is_none());
    assert!(returned::ReturnWatch::attach(&session.main).is_none(), "a top-level session");
}

#[test]
fn the_parents_result_is_seen_only_once_it_is_written_after_attaching() {
    let scratch = Scratch::new("return-poll");
    let projects = scratch.path("projects");
    // A result for this very call is already in the parent: the agent was
    // resumed after an earlier task, so it does not end this run.
    let session =
        session(&projects, "-p", &[agent_call("toolu_fg", "Build #42"), tool_result("toolu_fg")]);
    let transcript =
        subagent_with_meta(&session, "afg", &foreground("toolu_fg"), &[user_text("go")]);
    let mut watch = returned::ReturnWatch::attach(&transcript).unwrap();
    assert!(!watch.poll());
    append(
        &session.main,
        &[
            tool_result("toolu_other"),
            assistant_text("toolu_fg mentioned"),
        ],
    );
    assert!(!watch.poll(), "another call's result, or the id in plain text, is not this one");
    append(&session.main, &[tool_result("toolu_fg")]);
    assert!(watch.poll());

    // A nested subagent returns to the subagent that started it.
    let nested_meta = serde_json::json!({
        "agentType": "loom-builder", "toolUseId": "toolu_nested",
        "requestShape": "foreground", "parentAgentId": "afg"
    });
    let nested = subagent_with_meta(&session, "anest", &nested_meta, &[user_text("go")]);
    let mut watch = returned::ReturnWatch::attach(&nested).unwrap();
    append(&session.main, &[tool_result("toolu_nested")]);
    assert!(!watch.poll(), "the top-level transcript is not a nested agent's parent");
    append(&transcript, &[tool_result("toolu_nested")]);
    assert!(watch.poll());
}

#[test]
fn a_returned_segment_ends_where_the_subagent_stopped() {
    let mut segment = segment::Segment::starting_at(10);
    segment.returned(100);
    assert_eq!(segment.finished(99), None);
    assert_eq!(segment.finished(100), Some(EndReason::Returned));
    // An earlier end stands.
    let mut segment = segment::Segment::starting_at(10);
    segment.supersede(50);
    segment.returned(100);
    assert_eq!(segment.finished(50), Some(EndReason::Superseded));
}

/// End to end: the run publishes everything the subagent wrote, then ends
/// within a pass of the parent's result, with the session still alive and the
/// transcript nowhere near idle.
#[tokio::test]
async fn a_foreground_subagents_run_ends_when_its_parent_has_its_result() {
    let scratch = Scratch::new("return-drive");
    let projects = scratch.path("projects");
    let session = session(&projects, "-p", &[agent_call("toolu_fg", "Build #10116")]);
    let lines = [
        user_text("Build #10116"),
        bash_call("toolu_wt", "./.loom/scripts/worktree.sh 10116"),
        assistant_text("Worktree ready."),
    ];
    let transcript = subagent_with_meta(&session, "afg", &foreground("toolu_fg"), &lines);
    let located = Located {
        from: lines[0].len() as u64,
        ..Located::from_path(&transcript).unwrap()
    };
    let queue = Arc::new(DurableQueue::open(scratch.path("queue.jsonl"), 1_000));
    let sink = SessionOutputSink::new(vec![queue.clone()], "host-test").unwrap();
    let live = ResolvedLiveOutput {
        interval: Duration::from_millis(5),
        heartbeat: Duration::from_secs(30),
        max_runs: 1,
    };
    let limits = Limits {
        max_age: Duration::from_secs(5),
        idle_exit: Duration::from_secs(600),
    };
    let (sub, parent) = (transcript.clone(), session.main.clone());
    let finish = async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        append(&sub, &[assistant_text("PR opened.")]);
        append(&parent, &[tool_result("toolu_fg")]);
    };
    let began = Instant::now();
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
    let (reason, ()) = tokio::join!(run, finish);
    assert_eq!(reason, EndReason::Returned);
    assert!(began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
    let published = records(&queue);
    assert_eq!(texts(&published), ["Worktree ready.", "PR opened."]);
    assert_eq!(published.last().unwrap().coverage, Coverage::Ended);
}
