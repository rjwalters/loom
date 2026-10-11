//! Resume-dispatch tests (#10832): a real registry, a real child process
//! behind a fake spawn script, and fake `gh` fixtures for the forge checks.

use super::*;
use crate::sweep_registry::resume_handle::DispatchSession;
use crate::sweep_registry::test_support;
use std::os::unix::fs::PermissionsExt;

const SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";
const OLD: &str = "sweep-issue-77-1000";

/// A spawn script that records its argv and the resume environment, then
/// stays up like a session would.
fn recording_spawn(ws: &Path) -> (PathBuf, PathBuf) {
    let record = ws.join("resume-spawn.log");
    let scripts = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let bin = scripts.join("spawn-claude.sh");
    std::fs::write(
        &bin,
        format!(
            "#!/usr/bin/env bash\n{{\n  printf 'argv: %s\\n' \"$*\"\n  for v in LOOM_RESUME_SESSION_ID \
             LOOM_RESUME_PROMPT LOOM_CLAUDE_SESSION_ID LOOM_DAEMON_ITEM_ID LOOM_SWEEP_ID \
             LOOM_CODEX_HOME LOOM_CODEX_SANDBOX LOOM_CODEX_SESSION_EXEC LOOM_ACCOUNT_NAME \
             LOOM_SWEEP_CLAIM_OWNED; do\n    printf '%s=%s\\n' \"$v\" \"${{!v-unset}}\"\n  done\n  \
             printf 'PWD=%s\\n' \"$(pwd -P)\"\n  printf 'done\\n'\n}} >> \"{}\"\n{{BODY}}\n",
            record.display()
        )
        .replace("{BODY}", "sleep 300"),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Ok(f) = std::fs::File::open(&bin) {
        let _ = f.sync_all();
    }
    (bin, record)
}

fn registry(ws: &Path) -> (SweepRegistry, PathBuf) {
    let (bin, record) = recording_spawn(ws);
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(bin);
    config.skip_label_flip = true;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), record)
}

/// Leave issue `issue` as H4 leaves a paused sweep: a lock naming the paused
/// run with a dead pid and its resume handle, a checkpoint, a journal entry.
fn pause_issue(reg: &SweepRegistry, ws: &Path, issue: u32, old: &str, runtime: &str) {
    test_support::write_lock_owner(reg, issue, old, 999_999);
    let session = DispatchSession::new(old, ws, Some(runtime)).unwrap();
    reg.stamp_resume_handle_in_lock(issue, &session, Some("opus"), Some("high"));
    test_support::write_checkpoint(reg, issue, "builder");
    test_support::write_journal_entry(reg, &ws.display().to_string(), issue, 999_999);
}

fn launch(runtime: &str) -> RollResumeLaunch {
    RollResumeLaunch {
        runtime: runtime.to_string(),
        session_id: SID.to_string(),
        prompt: "Loom: this session was paused for a daemon roll".to_string(),
        resume_of: OLD.to_string(),
        resume_count: 1,
        agent_started_at: Some("2026-10-07T16:40:12+00:00".to_string()),
        ..RollResumeLaunch::default()
    }
}

fn spec(issue: u32, launch: RollResumeLaunch) -> RollResumeSpec {
    RollResumeSpec {
        issue,
        old_sweep_id: OLD.to_string(),
        launch,
        model: Some("opus".to_string()),
        effort: None,
        worktree: None,
    }
}

fn owner(reg: &SweepRegistry, issue: u32) -> LockOwner {
    reg.roll_lock_owner(issue).expect("the lock is in place")
}

/// Run the three resume steps the way H5's host does.
fn resume(reg: &mut SweepRegistry, spec: &RollResumeSpec) -> ResumedSweep {
    let mut prepared = reg.begin_roll_resume(spec).expect("the resume begins");
    let (token, runtime, death) = poll_and_classify_spawned_child(
        &mut prepared.child,
        &prepared.log_path,
        &prepared.header_anchor,
    );
    reg.finish_roll_resume(prepared, token, runtime, death)
        .expect("the resume finishes")
}

/// AC: a paused sweep is resumed as a new process in the same workspace, with
/// the claim kept, `resume_of` lineage, the carried first start, and no
/// `/loom:sweep` prompt (the saved session holds the task).
#[test]
fn a_paused_claude_sweep_is_resumed_from_its_session_with_its_claim_kept() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, record) = registry(&ws);
    pause_issue(&reg, &ws, 77, OLD, "claude");
    let spec = spec(77, launch("claude"));
    reg.roll_resume_checks(&spec)
        .expect("nothing changed while it was paused");
    assert_eq!(reg.live_roll_resume_of(77, OLD), None, "nothing resumes it yet");

    let resumed = resume(&mut reg, &spec);

    // A new run, the same claim.
    assert!(resumed.sweep_id.starts_with("sweep-issue-77-") && resumed.sweep_id.ends_with("-r1"));
    assert_ne!(resumed.sweep_id, OLD);
    let lock = owner(&reg, 77);
    assert_eq!(
        (lock.sweep_id.as_str(), lock.owner_pid),
        (resumed.sweep_id.as_str(), resumed.pid)
    );
    assert_eq!(lock.item_id.as_deref(), Some(resumed.sweep_id.as_str()));
    assert_eq!(lock.agent_started_at.as_deref(), Some("2026-10-07T16:40:12+00:00"));
    let handle = lock.resume_handle.expect("the lineage is recorded");
    assert_eq!(handle.session_id.as_deref(), Some(SID), "the same session");
    assert_eq!((handle.resume_of.as_deref(), handle.resume_count), (Some(OLD), 1));
    // A tracked, running entry and a journal record of the new process.
    let entry = reg
        .entries
        .get(&resumed.sweep_id)
        .expect("a registry entry");
    assert_eq!(entry.state, SweepState::Running);
    assert_eq!(entry.model.as_deref(), Some("opus"));
    assert!(!reg.entries.contains_key(OLD));
    let journal = crate::sweep_journal::load(&ws.join("test-sweeps-journal.json"));
    assert!(journal
        .entries
        .iter()
        .any(|e| e.issue == 77 && e.pid == resumed.pid));
    assert_eq!(reg.roll_resume_child(&resumed.sweep_id), ResumeChild::Running);
    assert_eq!(reg.live_roll_resume_of(77, OLD).as_deref(), Some(resumed.sweep_id.as_str()));

    // What the child was launched with.
    let recorded = test_support::assert_child_wrote(&record, "done");
    assert!(
        recorded.contains("argv: -p --model opus --dangerously-skip-permissions"),
        "{recorded}"
    );
    assert!(!recorded.contains("/loom:sweep"), "a resume passes no sweep prompt: {recorded}");
    assert!(recorded.contains(&format!("LOOM_RESUME_SESSION_ID={SID}")), "{recorded}");
    assert!(
        recorded.contains("LOOM_RESUME_PROMPT=Loom: this session was paused"),
        "{recorded}"
    );
    assert!(
        recorded.contains("LOOM_CLAUDE_SESSION_ID=unset"),
        "never pinned again: {recorded}"
    );
    assert!(
        recorded.contains(&format!("LOOM_DAEMON_ITEM_ID={}", resumed.sweep_id)),
        "{recorded}"
    );
    assert!(recorded.contains("LOOM_SWEEP_CLAIM_OWNED=77"), "{recorded}");
    assert!(recorded.contains(&format!("PWD={}", ws.display())), "{recorded}");

    // A resume that must be undone leaves nothing running and no entry.
    reg.abandon_roll_resume(&resumed.sweep_id);
    assert!(test_support::wait_until_dead(resumed.pid, 20_000));
    assert!(!reg.entries.contains_key(&resumed.sweep_id));
    assert_eq!(reg.roll_resume_child(&resumed.sweep_id), ResumeChild::Untracked);
    // Releasing the item removes its lock and journal entry, keeps the checkpoint.
    reg.release_roll_item(77, &[OLD]);
    assert!(reg.roll_lock_owner(77).is_none());
    assert!(reg.config.checkpoint_dir().join("issue-77.json").is_file());
    let journal = crate::sweep_journal::load(&ws.join("test-sweeps-journal.json"));
    assert!(!journal.entries.iter().any(|e| e.issue == 77));
}

/// AC: a Codex resume takes no `-p`, pins the session's own `CODEX_HOME`, and
/// asks for the sandbox the session was started under.
#[test]
fn a_paused_codex_sweep_is_resumed_in_its_own_account_under_its_recorded_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, record) = registry(&ws);
    pause_issue(&reg, &ws, 78, OLD, "codex");
    let mut l = launch("codex");
    l.session_store = Some("/profiles/agent-3".to_string());
    l.account = Some("agent-3".to_string());
    l.sandbox = Some("workspace-write".to_string());
    l.lease_sweep_id = Some("sweep-issue-78-1".to_string());
    let spec = spec(78, l);
    let resumed = resume(&mut reg, &spec);

    let recorded = test_support::assert_child_wrote(&record, "done");
    assert!(
        recorded.contains("argv: --model opus --dangerously-skip-permissions"),
        "{recorded}"
    );
    assert!(recorded.contains("LOOM_CODEX_HOME=/profiles/agent-3"), "{recorded}");
    assert!(recorded.contains("LOOM_ACCOUNT_NAME=agent-3"), "{recorded}");
    assert!(recorded.contains("LOOM_CODEX_SANDBOX=workspace-write"), "{recorded}");
    assert!(recorded.contains("LOOM_CODEX_SESSION_EXEC=unset"), "{recorded}");
    let handle = owner(&reg, 78).resume_handle.unwrap();
    assert_eq!(handle.session_store.as_deref(), Some("/profiles/agent-3"));
    assert_eq!(handle.sandbox.as_deref(), Some("workspace-write"));
    assert_eq!(
        handle.lease_sweep_id.as_deref(),
        Some("sweep-issue-78-1"),
        "the lease record is kept"
    );
    assert_eq!(spec.lease_sweep_id(), "sweep-issue-78-1");
    reg.abandon_roll_resume(&resumed.sweep_id);
}

/// A session-exec item recorded `danger-full-access` because its container
/// was the boundary. The resume asks for nothing and forces the container, so
/// the posture check grants that mode again or refuses the launch.
#[test]
fn a_container_granted_sandbox_is_never_turned_into_a_request() {
    let mut l = launch("codex");
    l.sandbox = Some("danger-full-access".to_string());
    assert_eq!(l.requested_sandbox(), Some("danger-full-access"), "bare metal: as recorded");
    l.container = Some("loom-codex-session-agent-3".to_string());
    assert_eq!(l.requested_sandbox(), None);
    l.sandbox = Some("workspace-write".to_string());
    assert_eq!(l.requested_sandbox(), Some("workspace-write"));

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, record) = registry(&ws);
    pause_issue(&reg, &ws, 79, OLD, "codex");
    l.sandbox = Some("danger-full-access".to_string());
    l.session_store = Some("/profiles/agent-3".to_string());
    let resumed = resume(&mut reg, &spec(79, l));
    let recorded = test_support::assert_child_wrote(&record, "done");
    assert!(recorded.contains("LOOM_CODEX_SANDBOX=unset"), "{recorded}");
    assert!(recorded.contains("LOOM_CODEX_SESSION_EXEC=1"), "{recorded}");
    reg.abandon_roll_resume(&resumed.sweep_id);
}

#[test]
fn the_launched_sandbox_is_read_from_this_dispatchs_own_log_region() {
    let log = "==== loom-daemon dispatch: t sweep_id=old ====\n\
               [INFO] spawn-codex: sandbox=read-only source=adapter-default\n\
               ==== loom-daemon dispatch: t sweep_id=new ====\n\
               [INFO] spawn-codex: sandbox=workspace-write source=LOOM_CODEX_SANDBOX\n\
               [INFO] spawn-codex: sandbox=danger-full-access source=session-container-boundary\n";
    assert_eq!(launched_sandbox(log, "sweep_id=new").as_deref(), Some("danger-full-access"));
    assert_eq!(launched_sandbox("no such line", "sweep_id=new"), None);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sweep.log");
    std::fs::write(&path, log).unwrap();
    assert!(sandbox_mismatch(&path, "sweep_id=new", Some("workspace-write"))
        .unwrap()
        .contains("danger-full-access"));
    assert_eq!(sandbox_mismatch(&path, "sweep_id=new", Some("danger-full-access")), None);
    assert_eq!(sandbox_mismatch(&path, "sweep_id=new", None), None, "nothing recorded");
}

/// A launch whose child dies in its preflight is `session-resume-failed`, and
/// the lock is still releasable as the paused run's.
#[test]
fn a_resume_whose_spawn_fails_puts_the_lock_back_for_the_requeue() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, _) = registry(&ws);
    pause_issue(&reg, &ws, 80, OLD, "claude");
    reg.config.spawn_bin = Some(ws.join("no-such-spawn-script.sh"));
    let refusal = reg
        .begin_roll_resume(&spec(80, launch("claude")))
        .unwrap_err();
    assert_eq!(refusal.reason, REASON_RESUME_FAILED, "{refusal}");
    assert_eq!(owner(&reg, 80).sweep_id, OLD, "the lock is the paused run's again");
    assert!(reg.entries.is_empty());

    // A session id that is not one never reaches a spawn.
    let mut bad = launch("claude");
    bad.session_id = "not-a-session".to_string();
    let (bin, _) = recording_spawn(&ws);
    reg.config.spawn_bin = Some(bin);
    let refusal = reg.begin_roll_resume(&spec(80, bad)).unwrap_err();
    assert_eq!(refusal.reason, REASON_RESUME_FAILED);
    assert_eq!(owner(&reg, 80).sweep_id, OLD);
}

/// §9 `guard-refused:<step>`: the claim lock is gone, or another run's.
#[test]
fn a_lock_that_is_gone_or_another_runs_refuses_the_resume() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, _) = registry(&ws);
    let spec = spec(81, launch("claude"));
    let gone = reg.roll_resume_checks(&spec).unwrap_err();
    assert_eq!(gone.reason, "guard-refused:claim-lock", "{gone}");
    assert_eq!(reg.begin_roll_resume(&spec).unwrap_err().reason, "guard-refused:claim-lock");

    test_support::write_lock_owner(&reg, 81, "sweep-issue-81-someone-else", 999_999);
    let other = reg.roll_resume_checks(&spec).unwrap_err();
    assert_eq!(other.reason, "guard-refused:claim-lock");
    assert!(other.detail.contains("someone-else"), "{other}");
    // Releasing the paused item never removes another run's lock.
    reg.release_roll_item(81, &[OLD]);
    assert_eq!(owner(&reg, 81).sweep_id, "sweep-issue-81-someone-else");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// §9 `worktree-changed`: missing, on another branch, or at another HEAD.
#[test]
fn a_worktree_that_moved_refuses_the_resume() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (reg, _) = registry(&ws);
    pause_issue(&reg, &ws, 82, OLD, "claude");
    let wt = ws.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "feature/issue-82"]);
    std::fs::write(wt.join("a"), "1").unwrap();
    git(&wt, &["add", "a"]);
    git(&wt, &["commit", "-q", "-m", "one"]);
    let head = git(&wt, &["rev-parse", "HEAD"]);
    let mut spec = spec(82, launch("claude"));
    let expect = |branch: &str, head: &str, path: &Path| {
        Some(WorktreeExpectation {
            path: path.to_path_buf(),
            branch: Some(branch.to_string()),
            head: Some(head.to_string()),
        })
    };
    spec.worktree = expect("feature/issue-82", &head, &wt);
    reg.roll_resume_checks(&spec).expect("unchanged");

    // Uncommitted edits are not a change: the agent left them there.
    std::fs::write(wt.join("a"), "dirty").unwrap();
    reg.roll_resume_checks(&spec)
        .expect("a dirty tree is the agent's own work");

    spec.worktree = expect("feature/other", &head, &wt);
    let branch = reg.roll_resume_checks(&spec).unwrap_err();
    assert_eq!(branch.reason, REASON_WORKTREE_CHANGED, "{branch}");
    spec.worktree = expect("feature/issue-82", "0000000000000000000000000000000000000000", &wt);
    assert_eq!(reg.roll_resume_checks(&spec).unwrap_err().reason, REASON_WORKTREE_CHANGED);
    spec.worktree = expect("feature/issue-82", &head, &ws.join("gone"));
    let missing = reg.roll_resume_checks(&spec).unwrap_err();
    assert!(missing.detail.contains("missing"), "{missing}");
}

fn lease_line(host: &str, sweep: &str) -> String {
    let now = Utc::now().to_rfc3339();
    format!(
        r#"{{"id":1,"created_at":"{now}","updated_at":"{now}","body":"<!-- loom:lease host={host} sweep={sweep} -->"}}"#
    )
}

/// §9 `lease-lost`: the freshest lease record is another host's, or another
/// sweep's on this host. This host's own record (under the paused run's id, or
/// the id its first dispatch published) passes.
#[test]
fn a_lease_that_is_no_longer_this_runs_refuses_the_resume() {
    let check = |line: &dyn Fn(&str) -> String, lease_id: Option<&str>| {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        // The published host id is a function of this host alone.
        let host = SweepRegistry::new(SweepRegistryConfig::new(ws.clone())).published_host_id();
        let reg = test_support::fixture_registry_with_lease_gh(&ws, &line(&host), 0);
        test_support::write_lock_owner(&reg, 83, OLD, 999_999);
        let mut l = launch("claude");
        l.lease_sweep_id = lease_id.map(str::to_string);
        reg.roll_resume_checks(&spec(83, l))
    };
    let peer = check(&|_| lease_line("peer-host", "sweep-peer"), None).unwrap_err();
    assert_eq!(peer.reason, REASON_LEASE_LOST, "{peer}");
    assert!(peer.detail.contains("peer-host"), "{peer}");
    let other = check(&|h| lease_line(h, "sweep-insession-1"), None).unwrap_err();
    assert_eq!(other.reason, REASON_LEASE_LOST, "{other}");
    check(&|h| lease_line(h, OLD), None).expect("this run's own lease");
    check(&|h| lease_line(h, "sweep-issue-83-first"), Some("sweep-issue-83-first"))
        .expect("the record its first dispatch published");
}

/// A lease read that fails is not a refusal: the manifest's age bounds the
/// window, and a forge hiccup must not requeue work nothing else could take.
#[test]
fn an_unanswered_lease_read_does_not_refuse_the_resume() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let reg = test_support::fixture_registry_with_lease_gh(&ws, "", 1);
    test_support::write_lock_owner(&reg, 84, OLD, 999_999);
    reg.roll_resume_checks(&spec(84, launch("claude")))
        .expect("fail-open");
}

/// §9 `issue-closed`.
#[test]
fn an_issue_that_closed_while_paused_refuses_the_resume() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (reg, _) = test_support::closed_guard_registry(
        &ws,
        &test_support::state_probe_json("closed", false),
        0,
    );
    test_support::write_lock_owner(&reg, 85, OLD, 999_999);
    let closed = reg
        .roll_resume_checks(&spec(85, launch("claude")))
        .unwrap_err();
    assert_eq!(closed.reason, REASON_ISSUE_CLOSED, "{closed}");
}

/// §9 `issue-parked`, and the claim label gone (`guard-refused:claim-label`).
#[test]
fn an_issue_that_was_parked_or_lost_its_claim_label_refuses_the_resume() {
    let check = |labels: &str| {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let (reg, _) = test_support::park_guard_registry(&ws, labels, 0, "", false);
        test_support::write_lock_owner(&reg, 86, OLD, 999_999);
        reg.roll_resume_checks(&spec(86, launch("claude")))
    };
    let parked = check("loom:building loom:blocked").unwrap_err();
    assert_eq!(parked.reason, REASON_ISSUE_PARKED, "{parked}");
    assert!(parked.detail.contains("loom:blocked"), "{parked}");
    let unclaimed = check("loom:issue").unwrap_err();
    assert_eq!(unclaimed.reason, "guard-refused:claim-label", "{unclaimed}");
    check("loom:building").expect("open, claimed and not parked");
}

/// A crash during H5: only a LIVE process resuming the item counts.
#[test]
fn only_a_live_resume_of_the_item_is_found_already_running() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, _) = registry(&ws);
    pause_issue(&reg, &ws, 87, OLD, "claude");
    let spec = spec(87, launch("claude"));
    let resumed = resume(&mut reg, &spec);
    assert_eq!(reg.live_roll_resume_of(87, OLD).as_deref(), Some(resumed.sweep_id.as_str()));
    assert_eq!(reg.live_roll_resume_of(87, "sweep-issue-87-unrelated"), None);
    reg.abandon_roll_resume(&resumed.sweep_id);
    assert!(test_support::wait_until_dead(resumed.pid, 20_000));
    assert_eq!(reg.live_roll_resume_of(87, OLD), None, "its process is gone");
    // The dead resume's lock is still the paused item's to resume again or release.
    reg.roll_resume_checks(&spec)
        .expect("a resume of the item still owns the lock");
    reg.release_roll_item(87, &[OLD]);
    assert!(reg.roll_lock_owner(87).is_none());
}

/// A paused PR-set sweep is never resumed; its per-PR locks are released.
#[test]
fn a_pr_set_sweeps_locks_are_released() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, _) = registry(&ws);
    let mine = "sweep-prs-5-6-1000";
    reg.acquire_pr_lock(5, mine).unwrap();
    reg.acquire_pr_lock(6, mine).unwrap();
    reg.acquire_pr_lock(7, "sweep-prs-7-2000").unwrap();
    assert_eq!(reg.release_roll_prset(mine), vec![5, 6]);
    let locks = reg.config.locks_dir();
    assert!(!locks.join("pr-5").exists() && !locks.join("pr-6").exists());
    assert!(locks.join("pr-7").exists(), "another sweep's PR lock is left alone");
}

/// `reconstruct_issues` recovers one issue the way a restart would, and
/// leaves every other lock alone.
#[test]
fn restart_recovery_can_be_run_for_one_released_issue() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (mut reg, _) = registry(&ws);
    pause_issue(&reg, &ws, 88, OLD, "claude");
    pause_issue(&reg, &ws, 89, "sweep-issue-89-1000", "claude");
    let only = HashSet::from([88]);
    assert_eq!(reg.reconstruct_issues(Some(&only)).unwrap(), 1);
    assert!(reg.roll_lock_owner(88).is_none(), "the stale lock is dropped");
    assert!(reg.roll_lock_owner(89).is_some(), "the other issue is untouched");
    let crashed: Vec<_> = reg
        .entries
        .values()
        .filter(|e| matches!(e.state, SweepState::Crashed { .. }))
        .collect();
    assert_eq!(crashed.len(), 1);
    assert_eq!(crashed[0].kind, SweepKind::Issue(88));
}
