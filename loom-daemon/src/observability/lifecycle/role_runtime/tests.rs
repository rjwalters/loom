//! #12342: a role tick's runtime, engine and session container ride its
//! `loom.role_attempt` span; a host-process tick carries no container keys.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE;

fn container_launch() -> Launch {
    Launch {
        runtime: Some("codex".into()),
        container: Some("loom-codex-session-a".into()),
        image_digest: Some("sha256:abc".into()),
    }
}

#[test]
fn container_run_names_runtime_engine_container_and_image() {
    let attrs = attributes(&container_launch());
    assert_eq!(attrs[RUNTIME], "codex");
    assert_eq!(attrs[ENGINE], "codex");
    assert_eq!(attrs[CONTAINER], "loom-codex-session-a");
    assert_eq!(attrs[IMAGE_DIGEST], "sha256:abc");
}

#[test]
fn host_process_run_carries_no_container_keys() {
    let attrs = attributes(&Launch {
        runtime: Some("claude".into()),
        ..Launch::default()
    });
    assert_eq!(attrs[ENGINE], "claude");
    assert!(!attrs.contains_key(CONTAINER));
    assert!(!attrs.contains_key(IMAGE_DIGEST));
}

#[test]
fn unknown_digest_is_absent_and_unknown_runtime_has_no_engine() {
    let attrs = attributes(&Launch {
        runtime: Some("opencode".into()),
        container: Some("loom-codex-session-a".into()),
        image_digest: None,
    });
    assert_eq!(attrs[RUNTIME], "opencode");
    assert!(!attrs.contains_key(ENGINE));
    assert!(!attrs.contains_key(IMAGE_DIGEST));
    assert!(attributes(&Launch::default()).is_empty());
}

#[test]
fn over_long_digest_is_bounded_to_256_bytes() {
    let mut launch = container_launch();
    launch.image_digest = Some("d".repeat(1000));
    assert_eq!(attributes(&launch)[IMAGE_DIGEST].len(), 256);
}

#[test]
#[serial_test::serial] // reads `LOOM_CODEX_SESSION_EXEC`
fn session_container_needs_a_codex_run_on_an_adopted_profile() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path();
    assert_eq!(session_container(Some("codex"), Some(profile), Some("a")), None);
    std::fs::write(profile.join(SESSION_MARKER_FILE), "{}").unwrap();
    assert_eq!(
        session_container(Some("codex"), Some(profile), Some("a")).as_deref(),
        Some("loom-codex-session-a")
    );
    assert_eq!(session_container(Some("claude"), Some(profile), Some("a")), None);
    assert_eq!(session_container(Some("codex"), Some(profile), None), None);
    assert_eq!(session_container(Some("codex"), None, Some("a")), None);
}

#[test]
fn note_is_taken_once_and_cleared() {
    clear();
    let mut command = std::process::Command::new("true");
    note_launch(&command, Some("claude"));
    assert_eq!(take()[ENGINE], "claude");
    assert!(take().is_empty(), "taken once");
    command.env(ACCOUNT_ENV, "a");
    note_launch(&command, Some("claude"));
    clear();
    assert!(take().is_empty());
}

#[test]
fn gateway_collector_keeps_every_role_runtime_span_attribute() {
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

    fn launch(runtime: &str) {
        let mut command = Command::new("true");
        note_launch(&command, Some(runtime));
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
    fn a_launched_tick_opens_with_runtime_and_engine_on_its_root_span() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch("codex");
            RoleTickOutcome::Success
        });
        let span = root_span(dir.path(), &trace.unwrap());
        assert_eq!(span.attributes[RUNTIME], "codex");
        assert_eq!(span.attributes[ENGINE], "codex");
        assert!(!span.attributes.contains_key(CONTAINER), "host-process run");
        // Version and revision come from the journal's provenance stamp.
        assert!(span
            .attributes
            .contains_key(crate::telemetry::trace::provenance::DAEMON_VERSION));
        // The export path bounds a span to the daemon allowlist first.
        let bounded = span.bounded();
        assert_eq!(bounded.attributes[ENGINE], "codex");
    }

    #[test]
    #[serial_test::serial]
    fn a_note_left_by_an_earlier_tick_never_describes_the_next() {
        let dir = traced_root();
        note_launch(&Command::new("true"), Some("codex"));
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            let mut command = Command::new("true");
            role_command(&mut command);
            let mut child = command.spawn().unwrap();
            role_child_spawned(child.id());
            child.wait().unwrap();
            role_child_exited("success");
            RoleTickOutcome::Success
        });
        let span = root_span(dir.path(), &trace.unwrap());
        assert!(!span.attributes.contains_key(ENGINE));
    }
}
