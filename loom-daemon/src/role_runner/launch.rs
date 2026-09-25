//! Blocking role launch; runtime decision polling never performs Docker I/O.
use super::*;

/// Run `spawn-claude.sh -p "<prompt>" --model <model> [--effort <level>]
/// --dangerously-skip-permissions` in `workspace_root`, appending combined
/// output to `<logs_dir>/role-<role>.log` (never a pipe — avoids the pipe-buffer
/// deadlock pattern documented in [`crate::main_health_gate`] /
/// [`crate::token_ranking_refresh`]) and killing it after `timeout`.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_role_with_timeout(
    script: &Path,
    workspace_root: &Path,
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
) -> RoleTickOutcome {
    let selection = match crate::tokens_pool::private_workspace::dispatch::Selection::prepare(
        workspace_root,
        admission.map_or("claude", |a| a.runtime.as_str()),
        Some(model),
        crate::tokens_pool::private_workspace::JobKind::Role,
        None,
        &format!("role-{role}-{}", uuid::Uuid::new_v4()),
    ) {
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
    cmd.arg("-p").arg(prompt);
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
    if let Some(admission) = admission {
        // Pin the already-admitted choice so spawn-worker cannot re-resolve a
        // different runtime after the pre-spawn decision.
        cmd.env("LOOM_RUNTIME", &admission.runtime);
        // Issue #4768: pin the admitted role too, mirroring
        // `sweep_registry::spawn_child`. Without it, a Codex-runtime role
        // child (e.g. `LOOM_ROLE` unset for a champion/curator/judge/auditor/
        // guide tick) reaches `spawn-codex.sh` with no role signal at all,
        // which is indistinguishable from an unrecognized role there.
        cmd.env("LOOM_ROLE", &admission.role);
        log::info!(
            "role_runner: admitted role={} runtime={} source={}",
            admission.role,
            admission.runtime,
            admission.source
        );
        // #6201: loud, at-selection diagnostic when the admitted runtime
        // diverges from the role's own declared `suggestedWorkerType` — the
        // signal the filed incident (curator declared `claude`, silently
        // ran on Codex for 9 days) had nowhere to surface.
        if let Some(msg) =
            crate::runtime_admission::suggested_worker_type_mismatch_warning(admission)
        {
            log::warn!("{msg}");
        }
    }

    // Run the child as its own process-group leader so a timeout can tear
    // down the whole subtree (the `claude` session's tool-call
    // subprocesses), not just the top-level `spawn-claude.sh` PID — mirrors
    // `sweep_registry::spawn_child`'s `process_group(0)` treatment.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    crate::observability::lifecycle::role_command(&mut cmd);
    if let Some(selection) = &selection {
        selection.apply(&mut cmd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RoleTickOutcome::Failure(format!("could not spawn `{}`: {e}", script.display()))
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
                    return RoleTickOutcome::Failure(detail);
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
                let detail = describe_role_failure(&full_log, &log_path);
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
                    return terminate_timed_out(&mut child, pid, script, &log_path, load_per_core);
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(e) => {
                return RoleTickOutcome::Failure(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ))
            }
        }
    }
}
