use super::*;
use std::io::Write;

const HEADER: &str = "==== loom-daemon dispatch: now sweep_id=sweep-42 issue=42 ====\n";
const LAUNCH: &str = "# LOOM_LAUNCH {\"runtime\":\"opencode\",\"provider\":\"zai-coding-plan\",\"model\":\"glm-5.3\",\"profile\":\"private-profile\",\"credentialAccount\":\"private-account\"}\n";

#[test]
fn unchanged_identity_replays_after_start_or_phase_without_rereading_launch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let queue = DurableQueue::open(root.join("queue.jsonl"), 100);
    let mut sampler = Sampler::default();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "{HEADER}{LAUNCH}").unwrap();
    let key = (root.clone(), "sweep-42".to_owned());
    let reader = sampler.readers.entry(key.clone()).or_default();
    let launch = reader.read(file.path(), "sweep-42").unwrap();
    let offset = reader.offset;
    let record = identity_record(
        "example/project".into(),
        RepoVisibility::Private,
        42,
        "sweep-42".into(),
        "opencode",
        Some(&launch),
    );
    sampler.enqueue(&queue, "host-a", root.clone(), record.clone());
    sampler.enqueue(&queue, "host-a", root.clone(), record.clone());
    assert_eq!(queue.len(), 1);
    // The first record may have been acknowledged but dropped by the DO because
    // no lifecycle row existed yet. A later start or adopted phase must replay.
    queue.ack(1);
    for event in [
        Event::SweepGlobalDispatch {
            sweep_id: "sweep-42".into(),
            kind: SweepKind::Issue(42),
            runtime: None,
            runtime_source: None,
            repo: None,
        },
        Event::SweepPhase {
            issue: 42,
            phase: "builder".into(),
            pr_number: None,
            repo: None,
        },
    ] {
        let other_root = root.join("other-repo");
        sampler.enqueue(&queue, "host-a", other_root.clone(), record.clone());
        queue.ack(1);
        sampler.observe(&event, &root);
        sampler.enqueue(&queue, "host-a", root.clone(), record.clone());
        assert_eq!(queue.len(), 1, "unchanged identity must replay after {event:?}");
        assert_eq!(sampler.readers[&key].resolved, Some(launch.clone()));
        assert_eq!(sampler.readers[&key].offset, offset);
        // Another repo's same-numbered issue is not invalidated.
        sampler.enqueue(&queue, "host-a", other_root, record.clone());
        assert_eq!(queue.len(), 1);
        queue.ack(1);
    }
}

#[test]
fn delayed_launch_survives_partial_reads_and_produces_dashboard_fixture() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let mut reader = LaunchReader::default();
    write!(file, "{HEADER}# LOOM_LAUNCH {{\"runtime\":").unwrap();
    assert!(reader.read(file.path(), "sweep-42").is_none());
    write!(file, "{}", &LAUNCH["# LOOM_LAUNCH {\"runtime\":".len()..]).unwrap();
    let launch = reader.read(file.path(), "sweep-42").unwrap();
    let record = identity_record(
        "example/project".into(),
        RepoVisibility::Private,
        42,
        "sweep-42".into(),
        "opencode",
        Some(&launch),
    );
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::SweepIdentity(record));
    assert_eq!(envelope.schema_version, 4);
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../dashboard/test/fixtures/sweep-identity.json"
    ))
    .unwrap();
    assert_eq!(serde_json::to_value(envelope.record).unwrap(), expected);
    // Later child role launches must not relabel the sweep's own launch.
    writeln!(file, "# LOOM_LAUNCH {{\"runtime\":\"codex\",\"model\":\"other\"}}").unwrap();
    assert_eq!(reader.read(file.path(), "sweep-42"), Some(launch));
}

#[test]
fn previous_dispatch_prefix_matches_and_transcript_mentions_are_not_attributed() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "==== loom-daemon dispatch: now sweep_id=sweep-420 issue=42 ====\n{LAUNCH}\ntranscript mentions {HEADER}{LAUNCH}").unwrap();
    let mut reader = LaunchReader::default();
    assert!(reader.read(file.path(), "sweep-42").is_none());
    write!(file, "{HEADER}transcript mentions {LAUNCH}").unwrap();
    assert!(reader.read(file.path(), "sweep-42").is_none());
    // Crossing into a new dispatch ends this sweep's attribution region.
    write!(file, "==== loom-daemon dispatch: now sweep_id=next issue=42 ====\n{LAUNCH}").unwrap();
    assert!(reader.read(file.path(), "sweep-42").is_none());
}

#[test]
fn reads_are_bounded_but_can_recover_launches_after_large_prior_logs() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&vec![b'x'; READ_BYTES as usize + 20])
        .unwrap();
    write!(file, "\n{HEADER}{LAUNCH}").unwrap();
    let mut reader = LaunchReader::default();
    assert!(reader.read(file.path(), "sweep-42").is_none());
    assert_eq!(reader.offset, READ_BYTES);
    assert!(reader.partial.len() < MAX_LINE_BYTES);
    assert_eq!(
        reader
            .read(file.path(), "sweep-42")
            .unwrap()
            .model
            .as_deref(),
        Some("glm-5.3")
    );
    // A fresh collector after restart can recover from the same registry/log.
    let mut adopted = LaunchReader::default();
    adopted.read(file.path(), "sweep-42");
    assert_eq!(adopted.read(file.path(), "sweep-42"), reader.resolved);
}

#[test]
fn missing_or_malformed_identity_is_unknown_and_has_no_fabricated_provider() {
    for runtime in ["claude", "codex", "unknown", ""] {
        let record = identity_record(
            "example/project".into(),
            RepoVisibility::Private,
            42,
            "sweep-42".into(),
            runtime,
            None,
        );
        assert_eq!(
            record.runtime.as_deref(),
            (!runtime.is_empty() && runtime != "unknown").then_some(runtime)
        );
        let value = serde_json::to_value(record).unwrap();
        assert!(value.get("provider").is_none());
        assert!(value.get("model").is_none());
    }
    let parsed =
        parse_launch_runtime(r#"{"runtime":"opencode","provider":7,"model":"   "}"#).unwrap();
    assert_eq!(parsed.provider, None);
    assert_eq!(parsed.model, None);
    assert!(parse_launch_runtime(r#"{"runtime":" "}"#).is_none());
}

/// Issue #8720 guardrail: restoring lifecycle correlation for an ADOPTED sweep
/// must not turn identity enrichment into a row creator. A phase event only
/// invalidates the replay cache so an already-resolved identity can be re-sent
/// once a lifecycle row exists — on its own it enqueues nothing, opens no
/// launch reader, and therefore cannot create or resurrect a row for an event
/// with no authoritative registry evidence behind it (the issue's own
/// hand-injected `unknown-issue-8715` example).
#[test]
fn a_phase_event_alone_never_produces_an_identity_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let queue = DurableQueue::open(root.join("queue.jsonl"), 100);
    let mut sampler = Sampler::default();

    sampler.observe(
        &Event::SweepPhase {
            issue: 8715,
            phase: "builder".into(),
            pr_number: None,
            repo: Some(root.display().to_string()),
        },
        &root,
    );

    assert_eq!(queue.len(), 0, "a phase event must not enqueue an identity record");
    assert!(sampler.sent.is_empty());
    assert!(
        sampler.readers.is_empty(),
        "identity is sampled from live registry entries only, never from an event"
    );
}
