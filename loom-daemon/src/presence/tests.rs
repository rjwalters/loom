//! Tests for #11563, mapped to its acceptance criteria.

use super::beat::{self, BeatOutcome, Ctx};
use super::journal::{self, ExportReport, NoTransport, Transport};
use crate::telemetry::{SweepResult, TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::path::Path;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 8, 0, 0).unwrap()
}

fn ctx(dir: &Path, now: DateTime<Utc>) -> Ctx {
    Ctx {
        root: dir.to_path_buf(),
        journal: dir.join(".loom/logs/operator-presence.jsonl"),
        session_id: "sess-1".to_string(),
        host: "host-a".to_string(),
        operator: Some("rjwalters".to_string()),
        now,
    }
}

fn read(ctx: &Ctx) -> Vec<TelemetryEnvelope> {
    journal::read_pending(&ctx.journal)
}

/// Acceptance 1: a typed prompt journals one record with `last_input_at` set
/// and no prompt text.
#[test]
fn a_typed_prompt_writes_one_record_without_the_prompt_text() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    let payload = r#"{"session_id":"sess-1","hook_event_name":"UserPromptSubmit",
        "prompt":"DISTINCTIVE-SECRET-PROMPT-7731","transcript_path":"/home/x/t.jsonl"}"#;
    let fields = beat::parse_hook_fields(payload);
    assert_eq!(fields.session_id.as_deref(), Some("sess-1"));
    let out = beat::beat(&c, fields.hook_event_name.as_deref().unwrap()).unwrap();
    assert_eq!(out, BeatOutcome::Emitted);

    let records = read(&c);
    assert_eq!(records.len(), 1);
    let TelemetryRecord::OperatorPresence(r) = &records[0].record else {
        panic!("not a presence record");
    };
    assert_eq!(r.last_input_at, Some(t0()));
    for file in [&c.journal, &dir.path().join(".loom/presence/sess-1.json")] {
        let text = std::fs::read_to_string(file).unwrap();
        assert!(!text.contains("DISTINCTIVE-SECRET-PROMPT"), "{}", file.display());
        assert!(!text.contains("transcript"), "{}", file.display());
    }
}

#[test]
fn non_prompt_beats_are_throttled_but_a_prompt_always_writes() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    assert_eq!(beat::beat(&c, "PostToolUse").unwrap(), BeatOutcome::Emitted);
    let soon = Ctx {
        now: t0() + Duration::seconds(60),
        ..c.clone()
    };
    assert_eq!(beat::beat(&soon, "Stop").unwrap(), BeatOutcome::Throttled);
    assert_eq!(read(&c).len(), 1);
    assert_eq!(beat::beat(&soon, "UserPromptSubmit").unwrap(), BeatOutcome::Emitted);
    assert_eq!(read(&c).len(), 2);
    let later = Ctx {
        now: t0() + Duration::seconds(60 + 301),
        ..c.clone()
    };
    assert_eq!(beat::beat(&later, "PostToolUse").unwrap(), BeatOutcome::Emitted);
    // The throttled Stop still advanced last_activity_at in state.
    let all = read(&c);
    let TelemetryRecord::OperatorPresence(last) = &all.last().unwrap().record else {
        panic!()
    };
    assert_eq!(last.last_input_at, Some(t0() + Duration::seconds(60)));
}

/// Acceptance 2: an in-session sweep yields `sweep.started` and
/// `sweep.outcome` with `dispatch_origin=operator` and the lease's sweep id,
/// and the presence beat reports the lease as driven while it is held.
#[test]
fn an_in_session_sweep_journals_operator_origin_facts_with_the_lease_id() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    beat::sweep_started(&c, "rjwalters/loom", "issue", 42, "sweep-issue-42-9").unwrap();
    beat::beat(&c, "UserPromptSubmit").unwrap();
    let end = Ctx {
        now: t0() + Duration::seconds(90),
        ..c.clone()
    };
    beat::sweep_outcome(&end, None, 42, "sweep-issue-42-9", SweepResult::Success, Some(77))
        .unwrap();
    beat::beat(
        &Ctx {
            now: t0() + Duration::seconds(100),
            ..c.clone()
        },
        "UserPromptSubmit",
    )
    .unwrap();

    let records = read(&c);
    assert_eq!(records.len(), 4);
    let TelemetryRecord::SweepStarted(s) = &records[0].record else {
        panic!()
    };
    assert_eq!(s.sweep_id, "sweep-issue-42-9");
    assert_eq!(s.facts.dispatch_origin.as_deref(), Some("operator"));
    let TelemetryRecord::OperatorPresence(p) = &records[1].record else {
        panic!()
    };
    assert_eq!(p.driving.len(), 1);
    assert_eq!(p.driving[0].number, 42);
    assert_eq!(p.driving[0].sweep_id.as_deref(), Some("sweep-issue-42-9"));
    let TelemetryRecord::SweepOutcome(o) = &records[2].record else {
        panic!()
    };
    assert_eq!(o.sweep_id, "sweep-issue-42-9");
    assert_eq!(o.dispatch_origin.as_deref(), Some("operator"));
    assert_eq!(o.repo.as_deref(), Some("rjwalters/loom"));
    assert_eq!(o.total_duration_sec, 90);
    assert_eq!(o.pr_number, Some(77));
    assert!(o.disposition_invariants_hold());
    let TelemetryRecord::OperatorPresence(after) = &records[3].record else {
        panic!()
    };
    assert!(after.driving.is_empty(), "a finished sweep is no longer driven");
}

struct Fake {
    fail: bool,
    got: usize,
}

impl Transport for Fake {
    fn send(&mut self, batch: &[TelemetryEnvelope]) -> anyhow::Result<()> {
        if self.fail {
            anyhow::bail!("down");
        }
        self.got += batch.len();
        Ok(())
    }
}

/// Acceptance 3: with no transport the records stay in the journal; a later
/// export sends every one of them and the count matches.
#[test]
fn export_sends_every_journaled_record_and_a_failure_loses_none() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    for i in 0..5 {
        let at = Ctx {
            now: t0() + Duration::seconds(i),
            ..c.clone()
        };
        beat::beat(&at, "UserPromptSubmit").unwrap();
    }
    beat::sweep_started(&c, "rjwalters/loom", "issue", 1, "s-1").unwrap();
    assert_eq!(journal::pending_count(&c.journal), 6);

    assert!(journal::export(&c.journal, &mut NoTransport).is_err());
    assert_eq!(journal::pending_count(&c.journal), 6, "no transport: nothing dropped");
    let mut down = Fake { fail: true, got: 0 };
    assert!(journal::export(&c.journal, &mut down).is_err());
    assert_eq!(journal::pending_count(&c.journal), 6, "failed send: nothing dropped");

    // A fact appended after a failed export is kept and sent in order.
    beat::beat(
        &Ctx {
            now: t0() + Duration::seconds(30),
            ..c.clone()
        },
        "UserPromptSubmit",
    )
    .unwrap();
    let mut up = Fake {
        fail: false,
        got: 0,
    };
    let report = journal::export(&c.journal, &mut up).unwrap();
    assert_eq!(
        report,
        ExportReport {
            exported: 7,
            rejected: 0
        }
    );
    assert_eq!(up.got, 7);
    assert_eq!(journal::pending_count(&c.journal), 0);
}

#[test]
fn a_malformed_line_is_set_aside_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    beat::beat(&c, "UserPromptSubmit").unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&c.journal)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"not json\n"))
        .unwrap();
    let mut up = Fake {
        fail: false,
        got: 0,
    };
    let report = journal::export(&c.journal, &mut up).unwrap();
    assert_eq!(
        report,
        ExportReport {
            exported: 1,
            rejected: 1
        }
    );
    let rejected = dir
        .path()
        .join(".loom/logs/operator-presence.jsonl.rejected");
    assert!(std::fs::read_to_string(rejected)
        .unwrap()
        .contains("not json"));
}

/// Acceptance 4 (docs half): the kind is documented in the schema doc.
#[test]
fn the_schema_doc_documents_operator_presence() {
    const DOC: &str = include_str!("../../../defaults/docs/telemetry-schema.md");
    assert!(DOC.contains("### `operator.presence`"));
    assert!(DOC.contains("loom.dispatch_origin=operator"));
    assert!(DOC.contains("presence-beat.sh"));
}

/// Acceptance 4 (wiring half): the hook is wired only on opt-in.
#[test]
fn the_presence_hook_is_wired_only_when_the_operator_opts_in() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/install/provision-hooks.sh");
    if std::process::Command::new("jq")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let wired = |opt_in: bool| -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!("source '{script}'; provision_loom_hooks '{}'", dir.path().display()))
            .env_remove("LOOM_OPERATOR_PRESENCE")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_PRESENCE_OPTOUT_FILE", dir.path().join("no-such-optout"));
        if opt_in {
            cmd.env("LOOM_OPERATOR_PRESENCE", "1");
        }
        cmd.output().unwrap();
        std::fs::read_to_string(dir.path().join("settings.json")).unwrap()
    };
    assert!(!wired(false).contains("presence-beat.sh"), "wired by default");
    let on = wired(true);
    assert!(on.contains("presence-beat.sh"), "opt-in did not wire it");
    assert!(on.contains("skill-router.sh"), "existing hooks must stay");
}

#[test]
fn session_ids_are_filename_safe() {
    assert_eq!(beat::sanitize_session("../../etc/passwd"), "etcpasswd");
    assert_eq!(beat::sanitize_session(""), "unknown");
}

/// A lease filed under `unknown` (started with no session id) and finished
/// under a known session id is pruned from `unknown`, so later beats on the
/// host stop reporting it as driven. Its recorded start still gives the
/// outcome a duration.
#[test]
fn an_outcome_under_a_known_session_prunes_an_unknown_session_lease() {
    let dir = tempfile::tempdir().unwrap();
    let anon = Ctx {
        session_id: beat::UNKNOWN_SESSION.to_string(),
        ..ctx(dir.path(), t0())
    };
    beat::sweep_started(&anon, "rjwalters/loom", "issue", 7, "sweep-issue-7-1").unwrap();
    let known = ctx(dir.path(), t0() + Duration::seconds(30));
    beat::sweep_outcome(&known, None, 7, "sweep-issue-7-1", SweepResult::Success, None).unwrap();
    assert!(beat::load_state(dir.path(), beat::UNKNOWN_SESSION)
        .driving
        .is_empty());

    beat::beat(&ctx(dir.path(), t0() + Duration::seconds(40)), "UserPromptSubmit").unwrap();
    let records = read(&known);
    let TelemetryRecord::SweepOutcome(o) = &records[1].record else {
        panic!()
    };
    assert_eq!(o.total_duration_sec, 30);
    assert_eq!(o.repo.as_deref(), Some("rjwalters/loom"));
    let TelemetryRecord::OperatorPresence(p) = &records[2].record else {
        panic!()
    };
    assert!(p.driving.is_empty(), "a finished unknown-session lease is still driven");
}

/// A failed journal write is not silent: the error lands in the breadcrumb
/// file beside the journal, and the summary `presence status` prints counts it.
#[test]
fn a_failed_write_leaves_a_durable_breadcrumb() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    assert_eq!(journal::error_summary(&c.journal), (0, None));
    // A directory where the journal file should be: the append must fail.
    std::fs::create_dir_all(&c.journal).unwrap();
    let err = beat::beat(&c, "UserPromptSubmit").unwrap_err();
    assert!(journal::record_error(&c.journal, t0(), &format!("{err:#}\nsecond line")));
    assert!(journal::record_error(&c.journal, t0(), "later failure"));

    let crumbs = journal::errors_path(&c.journal);
    assert_eq!(crumbs, dir.path().join(".loom/logs/operator-presence.errors"));
    let text = std::fs::read_to_string(&crumbs).unwrap();
    assert!(text.contains("presence journal"), "{text}");
    assert_eq!(text.lines().count(), 2, "one line per failure: {text}");
    let (count, last) = journal::error_summary(&c.journal);
    assert_eq!(count, 2);
    assert!(last.unwrap().ends_with("\tlater failure"));
}

/// Acceptance 2, automatic end: the sweep's own end signal (no explicit
/// `sweep-outcome` call) journals `sweep.outcome` with the lease id, under the
/// session that started it, exactly once however many end signals follow.
#[test]
fn the_end_of_a_driven_sweep_journals_its_outcome_once() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    beat::sweep_started(&c, "rjwalters/loom", "issue", 42, "sweep-issue-42-9").unwrap();
    // The end signal arrives from a process that knows no session id.
    let end = Ctx {
        session_id: beat::UNKNOWN_SESSION.to_string(),
        now: t0() + Duration::seconds(120),
        ..c.clone()
    };
    let done = beat::finish_driven(&end, 42, SweepResult::Success, Some(77)).unwrap();
    assert_eq!(done, vec!["sweep-issue-42-9".to_string()]);
    // The release that follows the merge finds nothing left to finish.
    assert!(beat::finish_driven(&end, 42, SweepResult::Failure, None)
        .unwrap()
        .is_empty());

    let records = read(&c);
    assert_eq!(records.len(), 2, "one start, one outcome");
    let TelemetryRecord::SweepOutcome(o) = &records[1].record else {
        panic!("not an outcome")
    };
    assert_eq!(o.sweep_id, "sweep-issue-42-9");
    assert_eq!(o.dispatch_origin.as_deref(), Some("operator"));
    assert_eq!(o.result, SweepResult::Success);
    assert_eq!(o.pr_number, Some(77));
    assert_eq!(o.repo.as_deref(), Some("rjwalters/loom"));
    assert_eq!(o.total_duration_sec, 120);
    assert!(beat::load_state(dir.path(), "sess-1").driving.is_empty());
}

/// A sweep this host never recorded as started (a daemon-dispatched one, or a
/// different issue) gets no outcome from an end signal.
#[test]
fn an_end_signal_for_an_undriven_issue_journals_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(dir.path(), t0());
    assert!(beat::finish_driven(&c, 42, SweepResult::Success, None)
        .unwrap()
        .is_empty());
    beat::sweep_started(&c, "rjwalters/loom", "issue", 41, "sweep-issue-41-1").unwrap();
    beat::sweep_started(&c, "rjwalters/loom", "pr", 42, "sweep-pr-42-1").unwrap();
    assert!(beat::finish_driven(&c, 42, SweepResult::Success, None)
        .unwrap()
        .is_empty());
    assert_eq!(read(&c).len(), 2, "only the two starts");
    assert_eq!(beat::load_state(dir.path(), "sess-1").driving.len(), 2);
}

/// A lease filed under `unknown` and the same lease under a named session
/// are one sweep: one outcome, and both copies stop being driven.
#[test]
fn an_end_signal_finishes_a_lease_held_under_two_sessions_once() {
    let dir = tempfile::tempdir().unwrap();
    let named = ctx(dir.path(), t0());
    let anon = Ctx {
        session_id: beat::UNKNOWN_SESSION.to_string(),
        ..named.clone()
    };
    beat::sweep_started(&anon, "rjwalters/loom", "issue", 9, "sweep-issue-9-1").unwrap();
    beat::sweep_started(&named, "rjwalters/loom", "issue", 9, "sweep-issue-9-1").unwrap();
    let done = beat::finish_driven(&anon, 9, SweepResult::Failure, None).unwrap();
    assert_eq!(done.len(), 1);
    let outcomes = read(&named)
        .into_iter()
        .filter(|e| matches!(e.record, TelemetryRecord::SweepOutcome(_)))
        .count();
    assert_eq!(outcomes, 1);
    for session in ["sess-1", beat::UNKNOWN_SESSION] {
        assert!(beat::load_state(dir.path(), session).driving.is_empty(), "{session}");
    }
}
