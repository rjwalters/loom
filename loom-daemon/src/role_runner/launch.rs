//! Blocking role launch; runtime decision polling never performs Docker I/O.
use super::*;

// #10003: an exit-0 Codex tick whose sandbox refused every tool call is a
// failure, and arms the host-wide hold the preference walk falls through on.
pub(super) mod sandbox_noop;
// #10455: a tick refused because the session container was down gets its own
// failure reason (and `loom.admission.reason`), not a bare RECOVERABLE/78.
mod session_down;
// #10364: likewise a tick refused because the running container does not
// mount the tick's working directory (a repo registered after it was created).
mod session_mount_stale;
// #10743: the launch's trace context + opt-in Claude Code OTel env.
#[cfg(test)]
mod observability_tests;

/// Stamp a role tick's launch with its `loom.role_attempt` trace context
/// (#9438), then — opt-in, default-off — the Claude Code OTel env (#10743), in
/// that order, exactly as the sweep path does: the `TRACEPARENT` exported
/// first is what parents the session's own spans inside this tick's trace.
/// With the opt-in off this only clears the variables Loom owns there.
///
/// The tick's context is mirrored to the standard `TRACEPARENT` (the variable
/// Claude Code's `-p` sessions read) exactly as the sweep path's
/// `observability::tracing::prepare_child` does, and removed when the tick has
/// none, so an ambient daemon value can never parent this session.
pub(super) fn apply_role_observability(cmd: &mut Command, workspace_root: &Path, role: &str) {
    use crate::telemetry::trace::store::{TRACEPARENT_ENV, W3C_TRACEPARENT_ENV};
    let execution = crate::observability::lifecycle::role_command(cmd);
    let context = cmd
        .get_envs()
        .find(|(name, _)| *name == TRACEPARENT_ENV)
        .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string));
    match context {
        Some(context) => cmd.env(W3C_TRACEPARENT_ENV, context),
        None => cmd.env_remove(W3C_TRACEPARENT_ENV),
    };
    crate::observability::claude_code_telemetry::prepare_scheduled_child(
        cmd,
        workspace_root,
        role,
        execution.as_deref(),
    );
}

// #10640: why a launched tick failed, for its `loom.role_attempt` span.
mod failure_class;
use crate::observability::lifecycle::note_role_failure;

/// Run `spawn-claude.sh -p "<prompt>" --model <model> [--effort <level>]
/// --dangerously-skip-permissions` in `workspace_root`, appending combined
/// output to `<logs_dir>/role-<role>.log` (never a pipe — avoids the pipe-buffer
/// deadlock pattern documented in [`crate::main_health_gate`] /
/// [`crate::token_ranking_refresh`]) and killing it after `timeout`.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_role_with_timeout(
    script: &Path,
    workspace_root: &Path,
    gh: &Path,
    role: &str,
    prompt: &str,
    logs_dir: PathBuf,
    timeout: Duration,
    model: &str,
    model_source: &str,
    effort: &str,
    effort_source: &str,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    load_per_core_override: Option<f64>,
    backstop: Option<crate::runtime_preference::Reservation>,
    contained: Option<crate::tokens_pool::private_workspace::dispatch::Selection>,
    resume: Option<&roll_resume::RoleRollResume>,
) -> RoleTickOutcome {
    // #10832: a saved session only resumes on the runtime that owns it. A role
    // whose binding moved to another runtime since the pause is refused here,
    // before anything is spawned, and H5 requeues the run.
    if let Some(refused) = resume.and_then(|r| r.refuse(admission)) {
        note_pre_spawn_skip(&logs_dir, role, &refused);
        return RoleTickOutcome::Failure(refused);
    }
    // #9548: every scheduled role (Champion, Curator, Judge, Doctor, ...)
    // writes labels and control markers on this workspace's repo, through
    // `gh` calls that resolve it the way `gh` does, an `upstream` remote
    // first. Refuse to launch one where this installation may not write,
    // exactly as `private_dispatch::prepare` refuses a sweep. `gate_root_with`
    // logs the reason once per change; the role log records each skip. `gh`
    // is the binary the permission probe runs (the runner's, which is
    // `write_scope::default_gh()` in production).
    if !crate::write_scope::gate_root_with(workspace_root, gh, &format!("the {role} role tick")) {
        let reason = "write scope refused (#9548): this installation may not write to the \
                      workspace's repository; see the write_scope log line for why";
        note_pre_spawn_skip(&logs_dir, role, reason);
        return RoleTickOutcome::Failure(reason.to_string());
    }
    // #8787: an admission that relied on private-clone containment must launch
    // through the EXACT selection that proved it — never a freshly prepared
    // one (which would be a second account pick against an unproven boundary),
    // and never without one at all.
    let contained_admission = admission.is_some_and(|a| a.execution.is_some());
    let prepared = match contained {
        Some(selection) if contained_admission => Ok(Some(selection)),
        _ if contained_admission => Err(anyhow::anyhow!(
            "containment admission has no prepared private selection; refusing launch"
        )),
        _ => crate::tokens_pool::private_workspace::dispatch::Selection::prepare(
            workspace_root,
            admission.map_or("claude", |a| a.runtime.as_str()),
            Some(model),
            crate::tokens_pool::private_workspace::JobKind::Role,
            None,
            &format!("role-{role}-{}", uuid::Uuid::new_v4()),
        ),
    };
    let selection = match prepared {
        Ok(selection) => selection,
        Err(error) => {
            note_pre_spawn_skip(&logs_dir, role, &error.to_string());
            return RoleTickOutcome::Failure(error.to_string());
        }
    };
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        return RoleTickOutcome::Failure(format!(
            "could not create logs dir {}: {e}",
            logs_dir.display()
        ));
    }
    let log_path = role_log_path(&logs_dir, role);
    // Issue #8443: this tick's own unique anchor into its per-role log — the
    // timestamp opening this tick's header line below, reused after exit to
    // scope `provider_health_feedback`'s terminal-result scan to only this
    // dispatch, mirroring the sweep path's `sweep_id=` anchor (a fresh log
    // line above the anchor never leaks into an OLDER tick's scan, and this
    // tick's own scan never reads a STALE record left by a previous one).
    // Issue #8504: `Z`, not `+00:00`, matching every other UTC stamp this
    // crate writes — `tick_anchor` is used purely as an opaque string anchor
    // (never re-parsed as a datetime), so this is a pure formatting change.
    let tick_anchor = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);

    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            // Rendered by `model_resolution::role_log_header` (#4501/#8054) —
            // the module that resolved the pair owns how it is reported.
            let _ = writeln!(
                f,
                "\n{}",
                model_resolution::role_log_header(
                    &tick_anchor,
                    role,
                    model,
                    model_source,
                    effort,
                    effort_source,
                )
            );
        }
    }

    let out_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            return RoleTickOutcome::Failure(format!(
                "could not open log {}: {e}",
                log_path.display()
            ))
        }
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => return RoleTickOutcome::Failure(format!("could not clone log handle: {e}")),
    };

    let mut cmd = Command::new(script);
    cmd.env(crate::provenance::origin::ENV, "autonomous");
    // #10832: a roll resume passes no prompt of its own (see
    // `sweep_registry::spawn_process`'s identical branch).
    match resume.map(|r| r.launch.runtime.as_str()) {
        None => {
            cmd.arg("-p").arg(prompt);
        }
        Some("codex") => {}
        Some(_) => {
            cmd.arg("-p");
        }
    }
    // Model pin (issue #4501): appended immediately after the prompt, exactly as
    // `sweep_registry::spawn_child` does, so a role child never inherits the
    // account's interactive CLI default (`fable` on the affected host — the most
    // constrained quota tier, and the escalation ceiling rather than the floor).
    // An empty value is treated as unset — `--model ""` must never be emitted —
    // mirroring the same guard on the sweep-dispatch path; `resolve_role_runner_model`
    // already filters blanks at every tier, so this is belt-and-braces.
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    // Reasoning-effort pin (issue #8054): appended immediately after `--model`,
    // exactly as `sweep_registry::dispatch`'s spawn does (#3716), so the two
    // dispatch surfaces share one positional argv contract
    // (`-p`, `--model`, `--effort`, `--dangerously-skip-permissions`). An empty
    // value is treated as unset — `--effort ""` must NEVER be emitted: it would
    // clobber the session-default effort with nothing. Unconfigured is the
    // normal case, and it must leave the argv byte-identical to the pre-#8054
    // argv, which is why there is no shipped default effort to fall back on.
    if !effort.is_empty() {
        cmd.arg("--effort").arg(effort);
    }
    cmd.arg("--dangerously-skip-permissions");
    // Transient-error recovery (issue #4255): scheduled role spawns are the
    // same unattended class as daemon-dispatched sweeps, so route them through
    // `claude-wrapper.sh` (retry/backoff/classification, bounded by
    // `LOOM_MAX_RETRIES`) instead of running bare `claude` that dies on the
    // first transient API failure. `spawn-claude.sh` consumes `--use-wrapper`
    // (not forwarded to `claude`) and execs the wrapper. Operators can force
    // the legacy single-shot path with `LOOM_USE_WRAPPER=0`.
    if sweep_registry::wrapper_dispatch_enabled() {
        cmd.arg("--use-wrapper");
    }
    cmd.current_dir(workspace_root)
        .env(sweep_registry::WORKSPACE_ENV, workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file));
    // Per-owner credential routing (#5401/#5431, gap closed by #5508): a
    // role-runner child is spawned with `current_dir(workspace_root)` above,
    // so it must carry the SAME per-owner `GH_CONFIG_DIR` every other
    // per-repo `gh`/`git` child-spawn call site already does — otherwise a
    // workspace registered under a non-default owner (e.g. `2AMLogic/*`)
    // gets the daemon's own process-global `GH_CONFIG_DIR` (an installation
    // token scoped only to the root owner's repos) and every forge call the
    // spawned Champion/Judge/etc. session makes 404s. A total no-op for a
    // single-owner fleet or the root owner's own repos — see
    // `apply_gh_config_for_root`'s doc comment.
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, workspace_root);
    // Pin the already-admitted runtime/role (and, since #8599, the
    // ordered-preference marker when a walk chose this tick's tap) so
    // spawn-worker cannot re-resolve a different runtime after the pre-spawn
    // decision, log the admission, and warn on a `suggestedWorkerType`
    // divergence. Shared with `sweep_registry::spawn_process`'s identical
    // block — the rationale for each pin lives in `launch_env`'s module doc.
    crate::launch_env::apply_launch_env(&mut cmd, admission, "role_runner");
    // #9473: same LLM-gateway guard as the sweep spawn — a role tick admitted
    // for Claude or Codex never carries the gateway contract.
    crate::worker_spawn::llm_gateway::guard_dispatch(
        &mut cmd,
        script,
        admission.map(|a| a.runtime.as_str()),
    );

    // Run the child as its own process-group leader so a timeout can tear
    // down the whole subtree (the `claude` session's tool-call
    // subprocesses), not just the top-level `spawn-claude.sh` PID — mirrors
    // `sweep_registry::spawn_child`'s `process_group(0)` treatment.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    apply_role_observability(&mut cmd, workspace_root, role);
    // #10830: a role run is paused and resumed like a sweep (design Q7), so it
    // gets the same pause-and-roll identity. Role runs have no claim lock; the
    // item id is synthetic (`role-<role>-<time>-<rand>`).
    let item = format!(
        "role-{role}-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let runtime = admission.map(|a| a.runtime.as_str());
    let session = match resume {
        None => sweep_registry::resume_handle::DispatchSession::new(&item, workspace_root, runtime),
        Some(r) => r.session(&item, workspace_root),
    };
    if resume.is_some() && session.is_none() {
        let reason = "roll resume refused: the saved session id is not usable (#10832)";
        note_pre_spawn_skip(&logs_dir, role, reason);
        return RoleTickOutcome::Failure(reason.to_string());
    }
    if let Some(session) = &session {
        session.apply_env(&mut cmd);
    }
    // #10432: the agent's pick journal, read back into this tick's `pick.decision`.
    crate::observability::pick_journal::attach(&mut cmd, workspace_root, role);
    if let Some(selection) = &selection {
        selection.apply(&mut cmd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            note_role_failure(failure_class::launch_failed(&e));
            return RoleTickOutcome::Failure(format!(
                "could not spawn `{}`: {e}",
                script.display()
            ));
        }
    };
    if let Some(selection) = &selection {
        selection.spawned();
    }
    let pid = child.id();
    // #8555: hand this tick's metered backstop slot (the one `runtime_preflight`
    // took, carried here by value) to the child that will spend it. No-op when
    // no ceiling is configured or the tick did not fall through to a governed
    // tap; every bail-out before this point dropped it, releasing it.
    crate::runtime_preference::handoff::attach(backstop, pid);
    crate::observability::lifecycle::role_child_spawned(pid);
    // #10831: list this run for the pause-and-roll H4 snapshot until it ends
    // (every return below drops the guard).
    let _live_run = session.as_ref().map(|s| {
        crate::roll_pause::live_runs::register(crate::roll_pause::live_runs::LiveRun {
            item_id: s.item_id.clone(),
            role: role.to_string(),
            root: workspace_root.to_path_buf(),
            pid,
            // The session's FIRST start: carried across a roll resume.
            started_at: chrono::DateTime::parse_from_rfc3339(&s.agent_started_at)
                .map_or_else(|_| chrono::Utc::now(), |t| t.with_timezone(&chrono::Utc)),
            runtime: s.runtime.clone(),
            claude_session_id: s
                .claude_session_id
                .clone()
                .or_else(|| s.resume.as_ref().map(|l| l.session_id.clone())),
            scope_unit: s.scope_unit.clone(),
            pause_root: s.pause_root.clone(),
            model: (!model.is_empty()).then(|| model.to_string()),
            timeout,
            started_mono: Instant::now(),
            resume: s.resume.clone(),
        })
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                crate::observability::lifecycle::role_child_exited("success");
                // Issue #8443: feed this tick's own terminal record back into
                // account health BEFORE reporting success — mirrors the
                // sweep reaper calling `apply_provider_health_feedback`
                // ahead of any re-dispatch decision.
                provider_health_feedback::apply_role_tick_provider_health_feedback(
                    workspace_root,
                    &log_path,
                    admission,
                    &tick_anchor,
                    status.code(),
                );
                // Issue #8448: exit 0 is not, by itself, evidence that a
                // GUARDED NATIVE launch did anything. When this tick's own
                // native event stream shows the `loom_*` binding was never
                // offered, report the failed launch it actually was instead
                // of a healthy `Success`. A no-opinion result (any non-native
                // runtime, an unreadable log, an unparseable stream) leaves
                // the pre-#8448 behaviour byte-identical — see
                // `toolless_launch`'s module doc for the four conditions.
                if let Some(detail) = toolless_launch::detect(&log_path, admission, &tick_anchor) {
                    log::warn!("role_runner: {detail}");
                    note_role_failure(failure_class::toolless_launch());
                    return RoleTickOutcome::Failure(detail);
                }
                // Issue #10003: exit 0 is not evidence a CODEX tick did
                // anything either. When this tick's own terminal record says
                // the sandbox refused every tool call, report the failed tick
                // it was and arm the hold that routes the next ticks to the
                // next preference tap; a record of SUCCESS clears that hold.
                if let Some(admitted) = admission {
                    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
                    let verdict = sandbox_noop::detect(&log_path, admission, &tick_anchor);
                    if let Some(detail) = sandbox_noop::apply(verdict, &admitted.runtime, now) {
                        log::warn!("role_runner: role={role} {detail}");
                        note_role_failure(failure_class::sandbox_unavailable());
                        return RoleTickOutcome::Failure(detail);
                    }
                }
                return RoleTickOutcome::Success;
            }
            Ok(Some(status)) => {
                crate::observability::lifecycle::role_child_exited(if status.code().is_some() {
                    "failure"
                } else {
                    "signal"
                });
                // Issues #6757/#8123: prefer a purpose-built failure sentinel
                // (naming the real cause and the role's own log path) over an
                // arbitrary tail-window fragment of stderr, when one is
                // present — see `describe_role_failure`.
                let full_log = read_role_log(&log_path);
                let detail = describe_role_failure(&full_log, &log_path, &tick_anchor);
                note_role_failure(failure_class::exited(status, &full_log, &tick_anchor));
                // Issue #8443: same terminal-record feedback on a non-zero
                // exit — this is the path a `TOKEN_EXHAUSTED` death actually
                // takes.
                provider_health_feedback::apply_role_tick_provider_health_feedback(
                    workspace_root,
                    &log_path,
                    admission,
                    &tick_anchor,
                    status.code(),
                );
                if let Some(reason) = session_down::reason_in(&full_log, &tick_anchor)
                    .or_else(|| session_mount_stale::reason_in(&full_log, &tick_anchor))
                {
                    // No per-tick WARN here (#10455 N2): the failure goes
                    // through the per-root edge/repeat machine like any other,
                    // and the undemoted signal is the per-account container
                    // WARN in `observability::ops::codex_session`.
                    return RoleTickOutcome::Failure(format!(
                        "{reason}: `{}` exited with {status}: {detail}",
                        script.display()
                    ));
                }
                return RoleTickOutcome::Failure(format!(
                    "`{}` exited with {status}: {detail}",
                    script.display()
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Issue #6637: sample load-per-core AT the moment the
                    // ceiling fires (not after termination — killing the
                    // child would itself relieve load, understating what the
                    // invocation was actually contending with). A test
                    // override takes precedence over the live host read, so
                    // a fake saturated/unsaturated host can be asserted
                    // deterministically.
                    let load_per_core =
                        load_per_core_override.or_else(crate::cpu_headroom::load_per_core);
                    let outcome =
                        terminate_timed_out(&mut child, pid, script, &log_path, load_per_core);
                    // A load-saturated ceiling is `skipped_load`, not a failure.
                    if matches!(outcome, RoleTickOutcome::Failure(_)) {
                        note_role_failure(failure_class::timed_out(timeout));
                    }
                    return outcome;
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(e) => {
                note_role_failure(failure_class::wait_failed(&e));
                return RoleTickOutcome::Failure(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // #9548: gate-reaching tests hold the default serial key; see `crate::write_scope_test_support`.

    /// #8787: an admission satisfied by private-clone containment can only
    /// launch through the selection that proved it. Without one, the tick
    /// fails before any spawn, any account pick, or any Docker call — it must
    /// never silently fall back to preparing a fresh selection against a
    /// boundary nothing has verified for this launch.
    #[test]
    #[serial_test::serial]
    fn contained_admission_without_its_selection_refuses_before_spawn() {
        let root = tempfile::tempdir().unwrap();
        let proof = crate::runtime_admission::ContainmentProof::fixture("seat");
        let admitted = crate::runtime_admission::ResolvedRuntime {
            role: "doctor".into(),
            runtime: "codex".into(),
            source: crate::runtime_admission::RuntimeSource::RoleConfig,
            adapter: PathBuf::from("/nonexistent/spawn-codex.sh"),
            role_manifest: PathBuf::from("/nonexistent/doctor.json"),
            runtime_manifest: PathBuf::from("/nonexistent/codex.json"),
            suggested_worker_type: None,
            preference: None,
            execution: Some(proof.provenance(
                vec![crate::runtime_admission::CONTAINMENT_SATISFIES.into()],
                std::collections::BTreeMap::new(),
            )),
        };
        let marker = root.path().join("spawned");
        let script = root.path().join("spawn.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        let ws = crate::write_scope_test_support::WritableRoot::register(root.path());
        let outcome = run_role_with_timeout(
            &script,
            root.path(),
            &ws.gh,
            "doctor",
            "/loom:doctor",
            root.path().join("logs"),
            Duration::from_secs(5),
            "",
            "default",
            "",
            "default",
            Some(&admitted),
            None,
            None,
            None,
            None,
        );
        match outcome {
            RoleTickOutcome::Failure(reason) => {
                assert!(reason.contains("no prepared private selection"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!marker.exists());
    }

    /// #9548 negative control: the same registered root, but the credential
    /// only has `pull`. The real gate refuses the tick before any spawn.
    #[test]
    #[serial_test::serial]
    fn a_role_tick_on_a_read_only_root_is_refused_before_spawn() {
        let root = tempfile::tempdir().unwrap();
        let ws = crate::write_scope_test_support::WritableRoot::read_only(root.path(), None);
        let marker = root.path().join("spawned");
        let script = root.path().join("spawn.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        let outcome = run_role_with_timeout(
            &script,
            root.path(),
            &ws.gh,
            "doctor",
            "/loom:doctor",
            root.path().join("logs"),
            Duration::from_secs(5),
            "",
            "default",
            "",
            "default",
            None,
            None,
            None,
            None,
            None,
        );
        match outcome {
            RoleTickOutcome::Failure(reason) => {
                assert!(reason.contains("write scope refused (#9548)"), "{reason}");
            }
            other => panic!("expected a write-scope refusal, got {other:?}"),
        }
        assert!(!marker.exists(), "nothing may be spawned on a refused root");
    }
}
