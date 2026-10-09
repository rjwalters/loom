#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn completion_markers_preserve_each_repair_attempt_without_invented_duration() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    for (phase, attempt) in [
        ("builder-done", 1),
        ("judge-rejected", 1),
        ("doctor-done", 2),
        ("judge-done", 2),
        ("merge-done", 2),
    ] {
        checkpoint_observation(
            &journal,
            &root,
            None,
            18,
            phase,
            Some(attempt),
            Some("glm-5.3-flash"),
            Some(42),
            "checkpoint_write_observed",
        );
    }
    assert!(journal.has_checkpoint_observations().unwrap());
    let mut spans = Vec::new();
    assert_eq!(
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap(),
        10
    );
    let judges: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.role"] == "judge")
        .collect();
    assert_eq!(judges.len(), 2);
    assert_eq!(judges[0].attributes["loom.judge_verdict"], "rejected");
    assert_eq!(judges[0].status, SpanStatus::Error);
    assert_eq!(judges[1].attributes["loom.judge_verdict"], "approved");
    assert_eq!(judges[1].status, SpanStatus::Ok);
    assert_ne!(judges[0].context.span_id, judges[1].context.span_id);
    for span in &spans {
        assert_eq!(span.started_at, span.ended_at);
        assert_eq!(span.context.trace_id, root.trace_id);
        assert_eq!(span.attributes["loom.pr_number"], "42");
    }
    assert_eq!(judges[0].attributes["loom.attempt"], "1");
    assert_eq!(judges[1].attributes["loom.attempt"], "2");
}

#[test]
fn checkpoint_completes_existing_started_attempt_without_double_counting() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let attrs = attributes(&[("loom.role", "judge")]);
    let start = Utc::now() - chrono::Duration::seconds(10);
    let phase = journal
        .start(root.child(), Some(&root), SpanName::Phase, start, attrs.clone())
        .unwrap();
    let attempt = journal
        .start(
            phase.record.context.child(),
            Some(&phase.record.context),
            SpanName::RoleAttempt,
            start,
            attrs,
        )
        .unwrap();
    checkpoint_observation(
        &journal,
        &root,
        None,
        18,
        "judge-rejected",
        Some(1),
        None,
        Some(42),
        "checkpoint_write_observed",
    );
    let mut spans = Vec::new();
    assert_eq!(
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap(),
        2
    );
    let observed = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(observed.context, attempt.record.context);
    assert_eq!(observed.started_at, start);
    assert!(observed.ended_at > start);
    assert_eq!(observed.status, SpanStatus::Error);
    assert_eq!(observed.attributes["loom.timing_source"], "owned_start_checkpoint_completion");
}

#[cfg(unix)]
#[test]
fn restart_closes_only_provably_gone_processes_with_unknown_execution_result() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let context = TraceContext::root(true);
    let span = journal
        .start(context.clone(), None, SpanName::RoleAttempt, Utc::now(), Default::default())
        .unwrap();
    recover_orphans(&journal);
    assert_eq!(journal.active().unwrap().len(), 1, "live owner is retained");
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert!(process_gone(pid));
    journal.set_owner(&context, pid).unwrap();
    journal.set_supervisor(&context, pid).unwrap();
    recover_orphans(&journal);
    assert!(journal.active().unwrap().is_empty());
    journal
        .drain(|s| {
            assert_eq!(s.context, span.record.context);
            assert_eq!(s.status, SpanStatus::Unset);
            assert_eq!(s.attributes["loom.result"], "process_lost");
            assert_eq!(s.attributes["loom.failure_class"], "supervisor-lost");
            assert_eq!(s.attributes["loom.recovered"], "true");
            Ok(())
        })
        .unwrap();
    assert!(!process_gone(0), "container/unknown PID namespaces cannot be guessed");
}

#[cfg(target_os = "linux")]
#[test]
fn recycled_owner_is_recovered_without_mistaking_delayed_spawn_for_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let semantic_start = Utc::now() - chrono::Duration::hours(6);
    let span = journal
        .start(
            TraceContext::root(true),
            None,
            SpanName::Sweep,
            semantic_start,
            Default::default(),
        )
        .unwrap();
    journal
        .set_owner(&span.record.context, std::process::id())
        .unwrap();
    recover_orphans(&journal);
    assert_eq!(journal.active().unwrap().len(), 1, "a delayed spawn is still its real owner");
    // Replay an old journal whose former owner's PID now belongs to this much
    // newer process. The persisted owner observation, not span time, proves reuse.
    let mut records: Vec<serde_json::Value> = std::fs::read_to_string(journal.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for record in &mut records {
        if record["event"] == "Owner" {
            record["span"]["observed_at"] = serde_json::json!(semantic_start);
        } else if record["event"] == "Started" {
            record["span"]["supervisor_observed_at"] = serde_json::json!(semantic_start);
        }
    }
    std::fs::write(journal.path(), records.iter().map(|r| format!("{r}\n")).collect::<String>())
        .unwrap();
    recover_orphans(&journal);
    assert!(journal.active().unwrap().is_empty());
    let mut results = Vec::new();
    journal
        .drain(|record| {
            results.push(record);
            Ok(())
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].attributes["loom.result"], "process_lost");
    assert_eq!(results[0].status, SpanStatus::Unset);
}

/// A role tick's trace is keyed on role + start instant (+ the repo key), never
/// a random id: the same tick recomputes to the same root, and two roles
/// starting in the same instant do not collide.
#[test]
#[serial_test::serial] // the repo key reads the process-global `LOOM_REPO`
fn role_invocation_ids_derive_from_role_and_start_instant() {
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-26T12:00:00.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    let judge = role_execution_id("judge", at);
    assert_eq!(judge, "role-judge-2026-09-26T12:00:00.123456789Z");
    assert_eq!(judge, role_execution_id("judge", at), "recomputable");
    let curator = role_execution_id("curator", at);
    let ws = std::path::Path::new("/nonexistent/loom");
    let (a, b) = (
        TraceStore::root_context(ws, &judge, None),
        TraceStore::root_context(ws, &curator, None),
    );
    assert_eq!(a, TraceStore::root_context(ws, &judge, None));
    assert_ne!(a.trace_id, b.trace_id);
    assert_ne!(a.span_id, b.span_id);
}

/// Tool spans carry no role, so the tool name joins the child key: two tools
/// opening the same span name in one clock tick stay distinct.
#[test]
fn tool_spans_in_the_same_instant_key_on_tool_name() {
    let parent = TraceContext::derived("test", &["parent"]);
    let at = Utc::now();
    let read =
        child_context(&parent, SpanName::Tool, at, &attributes(&[("loom.tool.name", "read")]));
    let write =
        child_context(&parent, SpanName::Tool, at, &attributes(&[("loom.tool.name", "write")]));
    assert_ne!(read.span_id, write.span_id);
    assert_eq!(read.trace_id, write.trace_id);
    let untagged = child_context(&parent, SpanName::Tool, at, &TraceAttributes::new());
    assert_eq!(
        untagged,
        parent.derived_child(&[
            SpanName::Tool.as_str(),
            "",
            &crate::telemetry::trace::instant(at)
        ]),
        "spans without a tool name keep their original key"
    );
}

#[test]
fn host_and_admission_attributes_survive_the_span_allowlist() {
    // The two attribute sets `role_invocation` stamps on every role attempt
    // must pass the span journal's allowlist intact — and nothing free-form
    // the underlying outcome carries (failure text, a role-log tail) may
    // leak into span attributes under any key.
    use crate::role_runner::RoleTickOutcome;
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    // One host sample for both the span start and the assertion, so
    // monotonically climbing counters (swap in/out totals) cannot make the
    // comparison self-defeating.
    let host = host_attributes();
    let mut start = attributes(&[("loom.role", "judge")]);
    start.extend(host.clone());
    let outcome = RoleTickOutcome::LoadSkipped {
        load_per_core: 4.2,
        detail: "FREE-FORM-ROLE-LOG-TAIL".to_string(),
    };
    let active = journal
        .start(root.child(), Some(&root), SpanName::RoleAttempt, Utc::now(), start)
        .unwrap();
    journal
        .finish(&active, Utc::now(), SpanStatus::Error, admission_attributes(&outcome))
        .unwrap();
    let mut spans = Vec::new();
    journal
        .drain(|record| {
            spans.push(record);
            Ok(())
        })
        .unwrap();
    assert_eq!(spans.len(), 1);
    let attributes = &spans[0].attributes;
    // Admission side: the fixed reason plus the measured value against the
    // threshold — the whole point of the closed taxonomy.
    assert_eq!(attributes.get("loom.admission.reason"), Some(&"load-ceiling".to_string()));
    assert_eq!(attributes.get("loom.admission.load_per_core"), Some(&"4.200".to_string()));
    assert_eq!(attributes.get("loom.admission.load_threshold"), Some(&"1.000".to_string()));
    // The free-form detail rides the outcome for daemon/role logs only.
    assert!(
        attributes
            .values()
            .all(|value| !value.contains("FREE-FORM-ROLE-LOG-TAIL")),
        "role-log tail leaked into span attributes: {attributes:?}"
    );
    // Host side: every key the probe measured on this host appears on the
    // span unchanged — nothing silently dropped by the allowlist.
    for (key, value) in &host {
        assert_eq!(
            attributes.get(key),
            Some(value),
            "host attribute {key} was dropped or mangled by the allowlist"
        );
    }
}

// ---------------------------------------------------------------------------
// #9420: loom.attempt.worked — the dwell-conditioning flag
// ---------------------------------------------------------------------------

/// Every `RoleTickOutcome` shape, and whether its attempt's interval measures
/// the stage's work. The `false` rows are the population that drags the
/// unconditioned `loom.role_attempt` median to milliseconds: 80.4% of this
/// host's 130,657 ticks over the 2026-09-18…10-02 `role_tick.outcome`
/// journals, 92,869 of them `PoolExhausted`.
#[test]
fn an_attempt_that_never_spawned_a_session_closes_as_not_worked() {
    use crate::role_runner::{CredentialPool, PoolHold, RoleTickOutcome};
    let exhausted = RoleTickOutcome::PoolExhausted {
        total: 20,
        next_clear_at: Utc::now() + chrono::Duration::minutes(15),
        pool: CredentialPool::ClaudeTokens,
        hold: PoolHold::SelfHealing,
    };
    let cases: Vec<(RoleTickOutcome, &str)> = vec![
        (RoleTickOutcome::Success, "true"),
        (RoleTickOutcome::Failure("boom".into()), "true"),
        (
            RoleTickOutcome::LoadSkipped {
                load_per_core: 4.2,
                detail: "deferred".into(),
            },
            "true",
        ),
        (exhausted, "false"),
        (RoleTickOutcome::NoTokenPool, "false"),
        (RoleTickOutcome::QueueEmpty, "false"),
    ];
    for (outcome, expected) in cases {
        let attrs = admission_attributes(&outcome);
        assert_eq!(
            attrs.get(ATTEMPT_WORKED),
            Some(&expected.to_string()),
            "{outcome:?} must close with {ATTEMPT_WORKED}={expected}"
        );
    }
}

/// A synthetic completion span is emitted with `started_at == ended_at`, so
/// its duration measures nothing at all — the shape behind the 0 ms builder
/// row. It must close `worked=false`; an attempt whose begin Loom itself
/// recorded closes `worked=true` on the very same code path.
#[test]
fn only_an_observed_start_makes_a_checkpoint_completion_count_as_worked() {
    let dir = tempfile::tempdir().unwrap();
    for source in SYNTHETIC_TIMING_SOURCES {
        let journal = Journal::for_context(&dir.path().join(format!("{source}.json")));
        let root = TraceContext::root(true);
        checkpoint_observation(
            &journal,
            &root,
            None,
            9420,
            "builder-done",
            Some(1),
            None,
            None,
            source,
        );
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        let attempt = spans
            .iter()
            .find(|s| s.name == SpanName::RoleAttempt)
            .unwrap();
        assert_eq!(attempt.started_at, attempt.ended_at, "synthetic spans carry no duration");
        assert_eq!(attempt.attributes[ATTEMPT_WORKED], "false", "{source}");
        // The parent phase span carries the same verdict, so a phase-level
        // dwell query conditions identically.
        let phase = spans.iter().find(|s| s.name == SpanName::Phase).unwrap();
        assert_eq!(phase.attributes[ATTEMPT_WORKED], "false", "{source}");
    }

    let journal = Journal::for_context(&dir.path().join("owned.json"));
    let root = TraceContext::root(true);
    let attrs = attributes(&[("loom.role", "builder")]);
    let start = Utc::now() - chrono::Duration::seconds(1450);
    let phase = journal
        .start(root.child(), Some(&root), SpanName::Phase, start, attrs.clone())
        .unwrap();
    journal
        .start(
            phase.record.context.child(),
            Some(&phase.record.context),
            SpanName::RoleAttempt,
            start,
            attrs,
        )
        .unwrap();
    checkpoint_observation(
        &journal,
        &root,
        None,
        9420,
        "builder-done",
        Some(1),
        None,
        None,
        "checkpoint_write_observed",
    );
    let mut spans = Vec::new();
    journal
        .drain(|s| {
            spans.push(s);
            Ok(())
        })
        .unwrap();
    let attempt = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(attempt.attributes["loom.timing_source"], "owned_start_checkpoint_completion");
    assert_eq!(attempt.attributes[ATTEMPT_WORKED], "true");
    assert!(attempt.ended_at - attempt.started_at >= chrono::Duration::seconds(1450));
}

/// #10637: a checkpoint observation names an issue; it must also name the
/// repository and sweep, copied from its execution's root so they join that
/// root exactly. Covers both synthetic sources (phase and attempt), the
/// owned-start completion, and a root that lacks the keys (left absent).
#[test]
fn checkpoint_spans_carry_their_executions_repo_and_sweep_id() {
    let dir = tempfile::tempdir().unwrap();
    let scope = attributes(&[
        ("loom.repo", "TwoAM-Fixture/Loom-UI"),
        ("loom.sweep_id", "sweep-issue-18-1790000000"),
    ]);
    let observed = |name: &str, root_attrs: TraceAttributes, owned: bool, source: &str| {
        let journal = Journal::for_context(&dir.path().join(format!("{name}.json")));
        let root = TraceContext::root(true);
        journal
            .start(root.clone(), None, SpanName::Sweep, Utc::now(), root_attrs)
            .unwrap();
        if owned {
            let attrs = attributes(&[("loom.role", "builder")]);
            let phase = journal
                .start(root.child(), Some(&root), SpanName::Phase, Utc::now(), attrs.clone())
                .unwrap();
            journal
                .start(
                    phase.record.context.child(),
                    Some(&phase.record.context),
                    SpanName::RoleAttempt,
                    Utc::now(),
                    attrs,
                )
                .unwrap();
        }
        checkpoint_observation(&journal, &root, None, 18, "builder-done", None, None, None, source);
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        assert_eq!(spans.len(), 2, "{name}: the phase and its attempt, root still open");
        spans
    };
    let cases = [
        ("write", SYNTHETIC_TIMING_SOURCES[0], false),
        ("poll", SYNTHETIC_TIMING_SOURCES[1], false),
        ("owned", "checkpoint_write_observed", true),
    ];
    for (name, source, owned) in cases {
        for span in observed(name, scope.clone(), owned, source) {
            assert!(matches!(span.name, SpanName::Phase | SpanName::RoleAttempt));
            for (key, value) in &scope {
                assert_eq!(
                    span.attributes.get(key),
                    Some(value),
                    "{name}: {key} on {:?}",
                    span.name
                );
            }
            assert_eq!(span.attributes["loom.issue"], "18");
        }
    }
    for span in observed("bare-root", TraceAttributes::new(), false, SYNTHETIC_TIMING_SOURCES[0]) {
        for key in scope.keys() {
            assert!(!span.attributes.contains_key(key), "{key} must stay absent, never guessed");
        }
    }
}

/// `finish_attempt` writes the flag onto both the attempt and its phase, and
/// `None` leaves the key **absent** — the "unknown != zero" half of the
/// contract. A caller that cannot tell must not publish a guess.
#[test]
fn an_undetermined_attempt_leaves_the_worked_key_absent() {
    let dir = tempfile::tempdir().unwrap();
    for (worked, expected) in [
        (None, None),
        (Some(false), Some("false")),
        (Some(true), Some("true")),
    ] {
        let journal = Journal::for_context(&dir.path().join(format!("{worked:?}.json")));
        let root = TraceContext::root(true);
        let attrs = attributes(&[("loom.role", "judge")]);
        let phase = journal
            .start(root.child(), Some(&root), SpanName::Phase, Utc::now(), attrs.clone())
            .unwrap();
        let attempt = journal
            .start(
                phase.record.context.child(),
                Some(&phase.record.context),
                SpanName::RoleAttempt,
                Utc::now(),
                attrs,
            )
            .unwrap();
        Span {
            journal: journal.clone(),
            active: attempt,
        }
        .finish_attempt("rejected", SpanStatus::Error, worked);
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        assert_eq!(spans.len(), 2, "attempt and phase both close");
        for span in &spans {
            assert_eq!(
                span.attributes.get(ATTEMPT_WORKED).map(String::as_str),
                expected,
                "{:?} on {:?}",
                worked,
                span.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// #9438: a role tick that never launches emits no `loom.role_attempt` span
// ---------------------------------------------------------------------------

#[cfg(feature = "otlp")]
mod role_tick_spans {
    use super::*;
    use crate::role_runner::{CredentialPool, PoolHold, RoleTickOutcome};

    fn traced_root() -> tempfile::TempDir {
        // Mixed case on purpose: with no origin and no `LOOM_REPO`, the
        // basename is the tick root's repo name (#10637).
        let dir = tempfile::Builder::new()
            .prefix("Loom-UI-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
        )
        .unwrap();
        dir
    }

    /// Every file under a workspace-relative directory, so "nothing was
    /// journalled" is asserted against the disk, not just the return value.
    fn files_under(root: &Path, relative: &str) -> Vec<PathBuf> {
        std::fs::read_dir(root.join(relative))
            .map(|dir| {
                dir.flatten()
                    .map(|e| e.path())
                    .filter(|p| !p.file_name().is_some_and(|n| n == ".lock"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn drained(root: &Path, trace: &RoleTrace) -> Vec<crate::telemetry::trace::SpanRecord> {
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
    }

    /// Stand-in for `role_runner::launch`: open the span at the launch, spawn
    /// a real child, and report its observed exit.
    fn launch(result: &str) {
        let mut command = Command::new("true");
        role_command(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(k, v)| k == TRACEPARENT_ENV && v.is_some()),
            "the launch carries the tick's traceparent"
        );
        let mut child = command.spawn().unwrap();
        role_child_spawned(child.id());
        child.wait().unwrap();
        role_child_exited(result);
    }

    /// Every outcome a tick reaches **without launching**: the pre-spawn
    /// skips, a runtime rejection, and a failure raised before the launch
    /// (`spawn-bin unresolved`, a preflight error). None may leave a span, a
    /// trace context, or a join entry behind — and none returns a
    /// [`RoleTrace`], so the tick's `role_tick.outcome` record carries no
    /// `trace_context` pointing at a span that was never exported.
    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn a_tick_that_never_launches_emits_no_role_attempt_span() {
        let skips = vec![
            RoleTickOutcome::PoolExhausted {
                total: 20,
                next_clear_at: Utc::now() + chrono::Duration::minutes(15),
                pool: CredentialPool::ClaudeTokens,
                hold: PoolHold::SelfHealing,
            },
            RoleTickOutcome::QueueEmpty,
            RoleTickOutcome::NoTokenPool,
            RoleTickOutcome::RuntimeRejected(crate::runtime_admission::RuntimeRejection {
                role: "doctor".into(),
                runtime: "codex".into(),
                source: crate::runtime_admission::RuntimeSource::RoleConfig,
                unmet_capabilities: vec!["isolation".into()],
                reason: "unmet".into(),
            }),
            RoleTickOutcome::ModelRuntimeMismatch(crate::role_runner::ModelRuntimeMismatch {
                role: "doctor".into(),
                runtime: "codex".into(),
                model: "opus".into(),
                model_source: "default".into(),
                reason: "family conflict".into(),
            }),
            RoleTickOutcome::Failure("spawn-bin unresolved".into()),
        ];
        for outcome in skips {
            let dir = traced_root();
            let label = format!("{outcome:?}");
            let (returned, trace) = role_invocation(dir.path(), "doctor", || outcome);
            assert_eq!(format!("{returned:?}"), label, "the outcome passes through");
            assert!(trace.is_none(), "{label}: a never-launched tick returns no trace");
            assert!(
                files_under(dir.path(), ".loom/logs/trace-context").is_empty(),
                "{label}: nothing journalled"
            );
            assert!(
                files_under(dir.path(), crate::observability::runtime_usage::join::JOIN_DIR)
                    .is_empty(),
                "{label}: no join entry left open"
            );
            ROLE_CONTEXT.with(|slot| assert!(slot.borrow().is_none(), "{label}: slot cleared"));
        }
    }

    /// A tick that launched keeps its `loom.role_attempt` root, named and
    /// attributed exactly as before, started at the tick's own instant (so the
    /// pre-spawn preparation stays inside the interval) — including the
    /// `LoadSkipped` shape, which is a session that ran to the wall-clock
    /// ceiling, not a pre-spawn skip.
    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn a_tick_that_launches_keeps_its_role_attempt_span() {
        std::env::remove_var("LOOM_REPO");
        let cases = vec![
            (RoleTickOutcome::Success, "success", "success", SpanStatus::Ok),
            (
                RoleTickOutcome::Failure("exit 1".into()),
                "failure",
                "failure",
                SpanStatus::Error,
            ),
            (
                RoleTickOutcome::LoadSkipped {
                    load_per_core: 4.2,
                    detail: "deferred".into(),
                },
                "failure",
                "skipped_load",
                SpanStatus::Error,
            ),
        ];
        for (outcome, child, result, status) in cases {
            let dir = traced_root();
            let (_, trace) = role_invocation(dir.path(), "judge", || {
                launch(child);
                outcome
            });
            let trace = trace.expect("a launched tick returns its trace");
            assert_eq!(trace.execution, role_execution_id("judge", trace.started_at));
            let spans = drained(dir.path(), &trace);
            let roots: Vec<_> = spans
                .iter()
                .filter(|s| s.context == trace.context)
                .collect();
            assert_eq!(roots.len(), 1, "{result}: exactly one root");
            let root = roots[0];
            assert_eq!(root.name, SpanName::RoleAttempt);
            assert_eq!(root.status, status);
            assert_eq!(root.started_at, trace.started_at, "starts at the tick, not the launch");
            assert_eq!(root.attributes["loom.result"], result);
            assert_eq!(root.attributes["loom.role"], "judge");
            assert_eq!(root.attributes["loom.sweep_id"], trace.execution);
            assert_eq!(root.attributes["loom.timing_source"], "owned_boundary");
            assert_eq!(root.attributes[ATTEMPT_WORKED], "true");
            // #10637: `loom.repo` keeps the repo's own spelling, and the trace
            // ID still derives from its lowercase, so the span carries its
            // own derivation input.
            let repo = dir.path().file_name().unwrap().to_str().unwrap();
            assert!(repo.starts_with("Loom-UI-"), "{repo}");
            assert_eq!(root.attributes["loom.repo"], repo);
            assert_eq!(
                trace.context,
                TraceContext::derived("execution", &[&repo.to_ascii_lowercase(), &trace.execution]),
            );
            assert!(
                !files_under(dir.path(), crate::observability::runtime_usage::join::JOIN_DIR)
                    .is_empty(),
                "{result}: the launched tick's join entry is open for transcript ingest"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// #9935: `sweep-checkpoint begin` gives a sweep phase an observed start
// ---------------------------------------------------------------------------

fn drained_spans(journal: &Journal) -> Vec<crate::telemetry::trace::SpanRecord> {
    let mut spans = Vec::new();
    journal
        .drain(|s| {
            spans.push(s);
            Ok(())
        })
        .unwrap();
    spans
}

/// Begin then done: one attempt, completed with a real duration and
/// `worked=true`, carrying the execution scope and the begin's attributes.
#[test]
fn a_begun_phase_completes_with_its_real_duration() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let scope = attributes(&[("loom.repo", "o/r"), ("loom.sweep_id", "sweep-1")]);
    journal
        .start(root.clone(), None, SpanName::Sweep, Utc::now(), scope)
        .unwrap();
    for role in ["curator", "builder", "judge", "doctor"] {
        let begun =
            checkpoint_begin_observation(&journal, &root, None, 9935, role, Some(1), Some("opus"))
                .unwrap();
        assert_eq!(begun.record.attributes["loom.timing_source"], CHECKPOINT_BEGIN_SOURCE);
        assert!(!begun.record.attributes.contains_key(ATTEMPT_WORKED));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let phase = if role == "judge" {
            "judge-done".to_owned()
        } else {
            format!("{role}-done")
        };
        checkpoint_observation(
            &journal,
            &root,
            None,
            9935,
            &phase,
            Some(1),
            None,
            Some(7),
            "checkpoint_write_observed",
        );
    }
    let spans = drained_spans(&journal);
    assert_eq!(spans.len(), 8, "one phase + one attempt per role, never a second");
    for span in &spans {
        assert_eq!(span.attributes["loom.timing_source"], "owned_start_checkpoint_completion");
        assert_eq!(span.attributes[ATTEMPT_WORKED], "true");
        assert!(span.ended_at - span.started_at >= chrono::Duration::milliseconds(5));
        assert_eq!(span.attributes["loom.sweep_id"], "sweep-1");
        assert_eq!(span.attributes["loom.configured_model"], "opus");
        assert_eq!(span.attributes["loom.issue"], "9935");
    }
}

/// Begin with no done: the attempt stays open (no fabricated close) until the
/// execution ends, which closes it `exit_unobserved` with `worked` absent.
#[cfg(feature = "otlp")]
#[test]
fn a_begun_phase_with_no_completion_closes_unobserved_at_execution_end() {
    let dir = tempfile::tempdir().unwrap();
    let root_dir = dir.path();
    std::fs::create_dir_all(root_dir.join(".loom")).unwrap();
    std::fs::write(
        root_dir.join(".loom/config.json"),
        r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
    )
    .unwrap();
    let span = begin(root_dir, "sweep-9935", SpanName::Sweep, TraceAttributes::new()).unwrap();
    checkpoint_begin_observation(&span.journal, span.context(), None, 9935, "builder", None, None)
        .unwrap();
    assert_eq!(span.journal.active().unwrap().len(), 3, "root, phase, attempt all open");
    finish_execution(root_dir, "sweep-9935", "failure", TraceAttributes::new()).unwrap();
    let spans = drained_spans(&span.journal);
    let attempt = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(attempt.attributes["loom.result"], "exit_unobserved");
    assert_eq!(attempt.attributes["loom.timing_source"], "terminal_observed");
    assert!(!attempt.attributes.contains_key(ATTEMPT_WORKED));
}

/// Parallel builders share one orchestrator journal: each issue's checkpoint
/// completes its own begun attempt. A re-dispatch supersedes the earlier begin
/// with `worked` absent, and a done with no begin stays synthetic.
#[test]
fn begun_attempts_are_matched_per_issue_and_superseded_on_redispatch() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let first_a =
        checkpoint_begin_observation(&journal, &root, None, 1, "builder", None, None).unwrap();
    let b = checkpoint_begin_observation(&journal, &root, None, 2, "builder", None, None).unwrap();
    let second_a =
        checkpoint_begin_observation(&journal, &root, None, 1, "builder", Some(2), None).unwrap();
    for issue in [1, 2] {
        checkpoint_observation(
            &journal,
            &root,
            None,
            issue,
            "builder-done",
            None,
            None,
            None,
            "checkpoint_write_observed",
        );
    }
    // No begin for issue 3: the synthetic zero-duration completion is unchanged.
    checkpoint_observation(
        &journal,
        &root,
        None,
        3,
        "builder-done",
        None,
        None,
        None,
        "checkpoint_write_observed",
    );
    let spans = drained_spans(&journal);
    let attempt = |context: &TraceContext| spans.iter().find(|s| s.context == *context).unwrap();
    let superseded = attempt(&first_a.record.context);
    assert_eq!(superseded.attributes["loom.result"], "superseded");
    assert!(!superseded.attributes.contains_key(ATTEMPT_WORKED));
    for (owned, issue) in [(&second_a, "1"), (&b, "2")] {
        let span = attempt(&owned.record.context);
        assert_eq!(span.attributes["loom.issue"], issue);
        assert_eq!(span.attributes["loom.timing_source"], "owned_start_checkpoint_completion");
        assert_eq!(span.attributes[ATTEMPT_WORKED], "true");
    }
    let synthetic = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.issue"] == "3")
        .unwrap();
    assert_eq!(synthetic.started_at, synthetic.ended_at);
    assert_eq!(synthetic.attributes["loom.timing_source"], "checkpoint_write_observed");
    assert_eq!(synthetic.attributes[ATTEMPT_WORKED], "false");
    assert!(checkpoint_begin_observation(&journal, &root, None, 1, "sweep", None, None).is_none());
}
