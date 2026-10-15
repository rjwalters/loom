//! 2AMLogic/2am#4393: a role tick a `roleDraw` table decided carries the
//! draw's grouping keys on its exported `loom.role_attempt` span; a tick
//! `rolePreference` decided carries none.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::runtime_preference::role_draw::{DrawWeight, Layer};

pub(crate) fn draw(pick: Option<&str>, model: Option<&str>) -> RoleDrawRecord {
    RoleDrawRecord {
        role: "judge".into(),
        attempt: 2,
        table_attempt: 2,
        layer: Layer::FleetRepo,
        table: vec![DrawWeight {
            candidate: "codex".into(),
            weight: 100,
        }],
        off: Vec::new(),
        seed: "00000000000000ff".into(),
        seed_inputs: None,
        steps: Vec::new(),
        passed_over: Vec::new(),
        pick: pick.map(str::to_owned),
        model: model.map(str::to_owned),
    }
}

#[test]
fn attributes_name_the_pick_attempt_table_and_layer() {
    let attrs = attributes(&draw(Some("claude/opus"), Some("opus")));
    assert_eq!(attrs[PICK], "claude/opus");
    assert_eq!(attrs[ATTEMPT], "2");
    assert_eq!(attrs[TABLE_ATTEMPT], "2");
    assert_eq!(attrs[LAYER], "fleet-repo");
    assert_eq!(attrs[MODEL], "opus");
    let unpinned = attributes(&draw(None, None));
    assert_eq!(unpinned[PICK], "none", "same spelling as the role_tick.outcome mapping");
    assert!(!unpinned.contains_key(MODEL));
}

#[test]
fn gateway_collector_keeps_every_role_draw_span_attribute() {
    const CONFIG: &str =
        include_str!("../../../../../defaults/observability/collector/config.yaml");
    let mut current = String::new();
    let mut keep = std::collections::BTreeSet::new();
    for line in CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == "span" && trimmed.contains("keep_keys(") {
            keep.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    for key in KEYS {
        assert!(keep.contains(key), "collector span keep_keys lacks {key}");
    }
}

#[test]
fn a_note_is_taken_once_and_none_clears_it() {
    note_role_draw(Some(&draw(Some("codex"), None)));
    assert_eq!(take().unwrap()[PICK], "codex");
    assert!(take().is_none());
    note_role_draw(Some(&draw(Some("codex"), None)));
    note_role_draw(None);
    assert!(take().is_none());
}

mod role_invocation_close {
    use super::super::super::{
        role_child_exited, role_child_spawned, role_command, role_invocation, Journal, RoleTrace,
    };
    use super::*;
    use crate::role_runner::RoleTickOutcome;
    use crate::telemetry::trace::{store::TraceStore, SpanRecord};
    use std::path::Path;
    use std::process::Command;

    fn traced_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
        )
        .unwrap();
        dir
    }

    fn launch() {
        let mut command = Command::new("true");
        role_command(&mut command);
        let mut child = command.spawn().unwrap();
        role_child_spawned(child.id());
        child.wait().unwrap();
        role_child_exited("success");
    }

    fn root_span(root: &Path, trace: &RoleTrace) -> SpanRecord {
        let store = TraceStore::new(root);
        let journal = Journal::for_context(&store.path(root, &trace.execution));
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        spans
            .into_iter()
            .find(|s| s.context == trace.context)
            .unwrap()
    }

    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn a_drawn_tick_closes_with_the_draw_on_its_root_span() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            note_role_draw(Some(&draw(Some("claude/opus"), Some("opus"))));
            launch();
            RoleTickOutcome::Success
        });
        let span = root_span(dir.path(), &trace.unwrap());
        assert_eq!(span.attributes[PICK], "claude/opus");
        assert_eq!(span.attributes[ATTEMPT], "2");
        assert_eq!(span.attributes[TABLE_ATTEMPT], "2");
        assert_eq!(span.attributes[LAYER], "fleet-repo");
        assert_eq!(span.attributes[MODEL], "opus");
        // The export path bounds a span to the daemon allowlist first.
        let bounded = span.bounded();
        for key in KEYS {
            assert!(bounded.attributes.contains_key(key), "daemon allowlist drops {key}");
        }
    }

    /// A `rolePreference` tick notes nothing, and a note an earlier tick left
    /// on the thread never describes it.
    #[test]
    #[serial_test::serial]
    fn a_role_preference_tick_carries_no_draw_keys() {
        let dir = traced_root();
        note_role_draw(Some(&draw(Some("codex"), None)));
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch();
            RoleTickOutcome::Success
        });
        let span = root_span(dir.path(), &trace.unwrap());
        for key in KEYS {
            assert!(!span.attributes.contains_key(key), "{key} on an undrawn tick");
        }
    }
}
