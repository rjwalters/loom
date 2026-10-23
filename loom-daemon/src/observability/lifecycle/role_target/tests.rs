//! #11855: a Judge/Doctor tick's `loom.role_attempt` span names its PR.
#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn an_explicit_judge_or_doctor_prompt_names_its_pr() {
    assert_eq!(explicit_pr("judge", "/loom:judge 179"), Some(179));
    assert_eq!(explicit_pr("doctor", "/loom:doctor #42 extra"), Some(42));
    assert_eq!(explicit_pr("judge", "/loom:judge"), None);
    assert_eq!(explicit_pr("judge", "review 179"), None);
    assert_eq!(explicit_pr("builder", "/loom:builder 7"), None);
}

#[test]
fn a_prompt_note_is_taken_once() {
    note_prompt("judge", "/loom:judge 7");
    assert_eq!(take(), Some(7));
    assert_eq!(take(), None);
    note_prompt("judge", "/loom:judge 7");
    note_prompt("judge", "/loom:judge");
    assert_eq!(take(), None);
}

#[test]
#[serial_test::serial] // `record_claim` reads the process-global env
fn a_claim_handed_back_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new("true");
    export(&mut cmd, dir.path(), "role-judge-2026:10:14T06/00");
    let path = cmd
        .get_envs()
        .find(|(k, _)| *k == PR_FILE_ENV)
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .unwrap();
    assert!(path.starts_with(dir.path().join(".loom/state/role-target")));
    std::env::set_var(PR_FILE_ENV, &path);
    record_claim(152);
    std::env::remove_var(PR_FILE_ENV);
    assert_eq!(take_handoff(dir.path(), "role-judge-2026:10:14T06/00"), Some(152));
    assert_eq!(take_handoff(dir.path(), "role-judge-2026:10:14T06/00"), None);
    assert!(!path.exists());
}

mod role_invocation_close {
    use super::super::super::{
        role_child_exited, role_child_spawned, role_command, role_invocation, Journal,
    };
    use super::super::*;
    use crate::role_runner::RoleTickOutcome;
    use crate::telemetry::trace::store::TraceStore;

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

    /// Launch a child that claims nothing.
    fn launch() {
        let mut command = Command::new("true");
        role_command(&mut command);
        let mut child = command.spawn().unwrap();
        role_child_spawned(child.id());
        child.wait().unwrap();
        role_child_exited("success");
    }

    fn pr_on_root_span(root: &std::path::Path, run: impl FnOnce()) -> Option<String> {
        let (_, trace) = role_invocation(root, "judge", || {
            run();
            RoleTickOutcome::Success
        });
        let trace = trace.unwrap();
        let store = TraceStore::new(root);
        let journal = Journal::for_context(&store.path(root, &trace.execution));
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        let span = spans
            .into_iter()
            .find(|s| s.context == trace.context)
            .unwrap();
        // The export path bounds a span to the daemon allowlist first.
        let kept = span.clone().bounded().attributes.get(PR_NUMBER).cloned();
        assert_eq!(kept.as_ref(), span.attributes.get(PR_NUMBER));
        span.attributes.get(PR_NUMBER).cloned()
    }

    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn an_assigned_tick_carries_the_prompt_pr() {
        let dir = traced_root();
        let pr = pr_on_root_span(dir.path(), || {
            note_prompt("judge", "/loom:judge 179");
            launch();
        });
        assert_eq!(pr.as_deref(), Some("179"));
    }

    #[test]
    #[serial_test::serial]
    fn an_unassigned_tick_carries_the_pr_its_claim_handed_back() {
        let dir = traced_root();
        let root = dir.path().to_path_buf();
        let pr = pr_on_root_span(dir.path(), || {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("printf 152 > \"${PR_FILE_ENV}\"")]);
            let execution = role_command(&mut command).unwrap();
            export(&mut command, &root, &execution);
            std::fs::create_dir_all(root.join(".loom/state/role-target")).unwrap();
            let mut child = command.spawn().unwrap();
            role_child_spawned(child.id());
            child.wait().unwrap();
            role_child_exited("success");
        });
        assert_eq!(pr.as_deref(), Some("152"));
    }

    #[test]
    #[serial_test::serial]
    fn a_tick_that_claimed_nothing_carries_no_pr() {
        let dir = traced_root();
        note_prompt("judge", "/loom:judge 9");
        let pr = pr_on_root_span(dir.path(), || launch());
        assert_eq!(pr, None, "an earlier tick's note never describes this one");
    }
}

#[test]
fn an_explicit_builder_or_curator_prompt_names_its_issue() {
    assert_eq!(explicit_issue("builder", "/loom:builder 11982"), Some(11982));
    assert_eq!(explicit_issue("curator", "/loom:curator #7 extra"), Some(7));
    assert_eq!(explicit_issue("curator", "/loom:curator"), None);
    assert_eq!(explicit_issue("judge", "/loom:judge 179"), None);
    note_prompt("builder", "/loom:builder 9");
    assert_eq!(take_issue(), Some(9));
    assert_eq!(take_issue(), None);
    assert_eq!(take(), None);
}
