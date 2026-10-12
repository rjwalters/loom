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
            .env_remove("LOOM_OPERATOR_PRESENCE");
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
