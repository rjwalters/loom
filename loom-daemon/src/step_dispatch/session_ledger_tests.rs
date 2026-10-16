#![allow(clippy::unwrap_used)]

use super::*;
use chrono::TimeZone;

const SID: &str = "0d3b6f0e-1111-4222-8333-444455556666";
const HOST: &str = "host-a";

fn at(mins: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).unwrap() + chrono::Duration::minutes(mins)
}

fn record(item: u32, role: StepRole, host: &str, ended: i64, size: u64) -> SessionRecord {
    SessionRecord {
        item,
        role,
        host: host.into(),
        runtime: "claude".into(),
        session_id: SID.into(),
        account: None,
        session_store: None,
        ended_at: at(ended),
        context_size: size,
    }
}

fn on() -> Config {
    Config {
        enabled: true,
        ..Config::default()
    }
}

fn ledger(records: Vec<SessionRecord>) -> Ledger {
    Ledger { records }
}

fn run(l: &Ledger, role: StepRole, now: i64, cfg: &Config, reachable: bool) -> Decision {
    decide(l, 7, role, at(now), HOST, cfg, &|_| reachable)
}

fn reason(d: &Decision) -> Option<FreshReason> {
    match d {
        Decision::Fresh { reason, .. } => Some(*reason),
        Decision::Resume(_) => None,
    }
}

#[test]
fn off_by_default() {
    assert!(!Config::default().enabled);
    let l = ledger(vec![record(7, StepRole::Judge, HOST, 0, 10)]);
    assert_eq!(
        reason(&run(&l, StepRole::Judge, 5, &Config::default(), true)),
        Some(FreshReason::Disabled)
    );
}

#[test]
fn config_precedence_is_env_over_block_over_default() {
    let block =
        serde_json::json!({"enabled": true, "ttlSecs": 60, "graceSecs": 5, "contextCapBytes": 9});
    let c = Config::from_parts(Some(&block), None);
    assert!(c.enabled);
    assert_eq!(
        (c.ttl, c.grace, c.context_cap),
        (Duration::from_secs(60), Duration::from_secs(5), 9)
    );
    assert!(!Config::from_parts(Some(&block), Some("0")).enabled);
    assert!(Config::from_parts(None, Some("true")).enabled);
    assert!(!Config::from_parts(None, None).enabled);
}

#[test]
fn judge_re_review_resumes_the_prior_judge() {
    let l = ledger(vec![record(7, StepRole::Judge, HOST, 0, 10)]);
    let d = run(&l, StepRole::Judge, 30, &on(), true);
    assert!(matches!(d, Decision::Resume(r) if r.role == StepRole::Judge));
}

#[test]
fn builder_retry_resumes_its_own() {
    let l = ledger(vec![record(7, StepRole::Builder, HOST, 0, 10)]);
    assert!(matches!(run(&l, StepRole::Builder, 5, &on(), true), Decision::Resume(_)));
}

#[test]
fn a_judge_never_resumes_an_author() {
    assert_eq!(
        may_resume(StepRole::Builder, StepRole::Judge),
        Err(Refusal::JudgeMayNotResumeAuthor)
    );
    assert_eq!(
        may_resume(StepRole::Doctor, StepRole::Judge),
        Err(Refusal::JudgeMayNotResumeAuthor)
    );
    // Even with only author sessions on the item, the Judge runs fresh.
    let l = ledger(vec![
        record(7, StepRole::Builder, HOST, 0, 10),
        record(7, StepRole::Doctor, HOST, 1, 10),
    ]);
    assert_eq!(reason(&run(&l, StepRole::Judge, 5, &on(), true)), Some(FreshReason::NoRecord));
}

#[test]
fn doctor_prefers_its_own_last_session_then_the_builders() {
    let mut doctor = record(7, StepRole::Doctor, HOST, 2, 10);
    doctor.session_id = "aaaaaaaa-1111-4222-8333-444455556666".into();
    let both = ledger(vec![record(7, StepRole::Builder, HOST, 0, 10), doctor]);
    let Decision::Resume(r) = run(&both, StepRole::Doctor, 5, &on(), true) else {
        panic!("expected a resume");
    };
    assert_eq!(r.role, StepRole::Doctor);
    let builder_only = ledger(vec![record(7, StepRole::Builder, HOST, 0, 10)]);
    let Decision::Resume(r) = run(&builder_only, StepRole::Doctor, 5, &on(), true) else {
        panic!("expected a resume");
    };
    assert_eq!(r.role, StepRole::Builder);
}

#[test]
fn roles_that_resume_nothing_run_fresh() {
    assert!(sources(StepRole::Curator).is_empty());
    assert_eq!(
        may_resume(StepRole::Curator, StepRole::Curator),
        Err(Refusal::RoleDoesNotResume)
    );
}

#[test]
fn ttl_cap_host_and_store_each_force_a_fresh_step_with_a_handoff() {
    let cfg = on();
    let l = ledger(vec![record(7, StepRole::Judge, HOST, 0, 10)]);
    // Past the 1 h TTL.
    let d = run(&l, StepRole::Judge, 61, &cfg, true);
    assert_eq!(reason(&d), Some(FreshReason::Expired));
    assert!(matches!(d, Decision::Fresh { handoff: Some(h), .. } if h.contains("#7")));
    // Inside it.
    assert!(matches!(run(&l, StepRole::Judge, 60, &cfg, true), Decision::Resume(_)));
    // Over the context cap.
    let big = ledger(vec![record(7, StepRole::Judge, HOST, 0, cfg.context_cap + 1)]);
    assert_eq!(
        reason(&run(&big, StepRole::Judge, 5, &cfg, true)),
        Some(FreshReason::OverContextCap)
    );
    // Another host's record (same host only).
    let other = ledger(vec![record(7, StepRole::Judge, "host-b", 0, 10)]);
    assert_eq!(
        reason(&run(&other, StepRole::Judge, 5, &cfg, true)),
        Some(FreshReason::OtherHost)
    );
    // A store that cannot be read.
    assert_eq!(
        reason(&run(&l, StepRole::Judge, 5, &cfg, false)),
        Some(FreshReason::StoreUnreachable)
    );
}

#[test]
fn an_unavailable_holder_never_blocks_eligibility_past_the_grace_window() {
    let cfg = on();
    let held = record(7, StepRole::Judge, "host-b", 0, 10);
    // Inside the 10 min grace the step still waits for the holder.
    assert_eq!(affinity_wait(&held, at(4), HOST, &cfg), Duration::from_secs(6 * 60));
    // At and past it the wait is zero: the step is eligible and runs fresh.
    assert_eq!(affinity_wait(&held, at(10), HOST, &cfg), Duration::ZERO);
    assert_eq!(affinity_wait(&held, at(500), HOST, &cfg), Duration::ZERO);
    let l = ledger(vec![held]);
    assert_eq!(reason(&run(&l, StepRole::Judge, 500, &cfg, true)), Some(FreshReason::OtherHost));
    // The holder's own host never waits; neither does a disabled ledger.
    let mine = record(7, StepRole::Judge, HOST, 0, 10);
    assert_eq!(affinity_wait(&mine, at(1), HOST, &cfg), Duration::ZERO);
    let theirs = record(7, StepRole::Judge, "host-b", 0, 10);
    assert_eq!(affinity_wait(&theirs, at(1), HOST, &Config::default()), Duration::ZERO);
}

#[test]
fn upsert_replaces_the_same_pair_and_prune_drops_the_expired() {
    let mut l = Ledger::default();
    l.upsert(record(7, StepRole::Judge, HOST, 0, 1));
    l.upsert(record(7, StepRole::Judge, HOST, 5, 2));
    l.upsert(record(7, StepRole::Builder, HOST, 1, 3));
    assert_eq!(l.records.len(), 2);
    assert_eq!(
        l.records
            .iter()
            .find(|r| r.role == StepRole::Judge)
            .unwrap()
            .context_size,
        2
    );
    l.prune(at(62), Duration::from_secs(3600));
    assert_eq!(
        l.records.len(),
        1,
        "the builder record (ended at +1) is past 1 h at +62, the judge (+5) is not"
    );
}

#[test]
fn the_ledger_round_trips_through_its_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(dir.path()).records.is_empty());
    let l = ledger(vec![record(7, StepRole::Judge, HOST, 0, 10)]);
    save(dir.path(), &l).unwrap();
    assert_eq!(load(dir.path()), l);
}

#[test]
fn a_noted_session_is_taken_once() {
    let root = Path::new("/tmp/session-ledger-note-test");
    note_session(
        root,
        "step-1",
        NotedSession {
            session_id: SID.into(),
        },
    );
    assert_eq!(take_session(root, "step-1").unwrap().session_id, SID);
    assert!(take_session(root, "step-1").is_none());
}

#[test]
fn transcript_size_is_read_from_the_projects_tree() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("projects").join("-repo");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join(format!("{SID}.jsonl")), b"12345").unwrap();
    assert_eq!(claude_transcript_size(dir.path(), SID), 5);
    assert_eq!(claude_transcript_size(dir.path(), "missing"), 0);
}

#[test]
fn the_resume_prompt_names_the_role_and_item() {
    let r = record(7, StepRole::Judge, HOST, 0, 10);
    let p = resume_prompt(&r, StepRole::Judge);
    assert!(p.contains("#7") && p.contains("judge"));
}
