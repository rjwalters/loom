//! Command assembly, isolated from claim bookkeeping.
use super::dispatch::child_env_markers;
use super::*;

impl SweepRegistry {
    /// Build the child's `Command` and spawn it — everything `spawn_child`
    /// used to do EXCEPT the account-selection poll (Issue #6592). Fast:
    /// `Command::spawn()` forks+execs without waiting for the child to do
    /// anything. Returns the live [`Child`] handle plus the `header_anchor`
    /// (`sweep_id=<id>`) the caller needs to pass to
    /// [`poll_and_classify_spawned_child`] next.
    ///
    /// Deliberately still `&self` (no registry mutation) so this can run
    /// under the SAME lock scope the guard chain above it uses — preserving
    /// the #3887 dispatch-stagger invariant (`apply_dispatch_stagger` is
    /// called by the caller just before this, still lock-serialized) — while
    /// the *poll* that follows can be released to run unlocked.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_child_process(
        &self,
        kind: &SweepKind,
        log_path: &Path,
        sweep_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        runtime_admission: Option<&crate::runtime_admission::ResolvedRuntime>,
        selection: Option<&crate::tokens_pool::private_workspace::dispatch::Selection>,
    ) -> Result<(Child, String)> {
        let spawn_bin = self.config.resolve_spawn_bin()?;

        // Ensure log dir exists.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir {}", parent.display()))?;
        }

        // Header target description (Issue #5342): `Issue` names its single
        // issue number; `PrSet` has none, so it names the whole PR list.
        let target_desc = match kind {
            SweepKind::Issue(n) => format!("issue={n}"),
            SweepKind::PrSet(prs) => format!("prs={prs:?}"),
        };

        // Append a header so reruns are distinguishable. Mirrors
        // spawn-loop.sh:377-380.
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(
                    f,
                    "\n==== loom-daemon dispatch: {} sweep_id={sweep_id} {target_desc} ====",
                    Utc::now().to_rfc3339()
                );
            }
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("failed to open log {}", log_path.display()))?;
        let log_clone = log_file.try_clone()?;

        // Daemon self-claim marker, positional form (issue #4111): embed
        // `--claim-owned <N>` INSIDE the `-p` prompt text so it becomes part of
        // the `/loom:sweep` skill's own `$ARGUMENTS`, exactly like every other
        // skill-consumed flag (`--dry-run`, `--no-daemon`, `--depends-on`,
        // `--auto-stack`, `--prs`). It MUST NOT be appended as a sibling
        // `cmd.arg()`: `spawn-claude.sh` forwards every non-wrapper token
        // verbatim to the real `claude` CLI (`exec claude "$@"`), and none of
        // these are `claude` CLI flags — a sibling arg makes `claude` exit 1
        // (`error: unknown option '...'`) before any session starts, turning
        // every daemon dispatch into an immediate crash. Only text inside the
        // single `-p "<prompt>"` string ever reaches the skill's pre-flight.
        //
        // `Issue` claims exactly one issue (`--claim-owned <N>`, env var kept
        // for backward compatibility below — #3823/#3967) and optionally
        // chains a stacked-PR parent (`--depends-on <N>`, issue #3729 v1;
        // sibling-arg bug fixed in #4121). `PrSet` (Mode C, issue #5342) has
        // no issue to claim and no stacking — it drives `--prs <n1> <n2>
        // ...` against an existing PR set instead.
        let prompt = match kind {
            SweepKind::Issue(issue) => {
                let mut p = format!("/loom:sweep {issue} --claim-owned {issue}");
                if let Some(parent) = depends_on {
                    p.push_str(&format!(" --depends-on {parent}"));
                }
                p
            }
            SweepKind::PrSet(prs) => {
                let joined = prs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("/loom:sweep --prs {joined}")
            }
        };
        let mut cmd = Command::new(&spawn_bin);
        cmd.arg("-p").arg(&prompt);
        // Model selection (issue #3477, Phase 1): the dispatch-param tier of
        // the precedence chain. Appended as an explicit `--model` arg (which
        // beats any ambient LOOM_MODEL env inside spawn-claude.sh). Empty
        // strings are treated as unset — `--model ""` must never be emitted.
        if let Some(m) = model {
            if !m.is_empty() {
                cmd.arg("--model").arg(m);
            }
        }
        // Reasoning-effort selection (issue #3716): the dispatch-param tier,
        // mirroring `--model` exactly. Appended as an explicit `--effort` arg
        // (which beats any ambient LOOM_EFFORT env inside spawn-claude.sh).
        // Empty strings are treated as unset — `--effort ""` must never be
        // emitted, so the session-default effort is preserved end-to-end.
        if let Some(e) = effort {
            if !e.is_empty() {
                cmd.arg("--effort").arg(e);
            }
        }
        // (Daemon self-claim marker `--claim-owned <N>` and the stacked-PR
        // `--depends-on <N>` marker are both embedded in the `-p` prompt text
        // above, not appended as sibling args — see #4111 / #4121.)
        // Unattended-permissions flag (issue #3824): a daemon-dispatched child
        // is a detached, non-interactive `claude -p` process — there is no
        // human to answer a permission prompt, so any tool call needing
        // approval (`.loom/` writes, `sweep-run-registry.sh`, the
        // `mcp__loom__list_sweeps` daemon probe) auto-denies and stalls the
        // build. Append `--dangerously-skip-permissions` so the child runs
        // non-interactively with hooks still firing — mirroring the established
        // unattended cron pattern (`.github/workflows/loom-*.yml`, which spawn
        // `claude -p "/<role>" --dangerously-skip-permissions`). Scoped to this
        // daemon-only dispatch path; `spawn-claude.sh` stays a generic
        // pass-through and never adds a permission flag of its own. Appended
        // AFTER `--model`/`--effort` (and the prompt-embedded `--claim-owned`
        // / `--depends-on`) so the positional argv contract is unchanged.
        cmd.arg("--dangerously-skip-permissions");
        // Transient-error recovery (issue #4255): route the child through
        // `claude-wrapper.sh` so a transient API death (rate-limit storm, 5xx,
        // overloaded, or the CLI's bare `Execution error`) is retried with
        // exponential backoff per `LOOM_MAX_RETRIES` instead of killing the
        // whole sweep on the first failure — the daemon dispatch path is the
        // unattended path that most needs it (21% of sweep logs died this way
        // before this flag). `spawn-claude.sh` consumes `--use-wrapper` (it is
        // NOT forwarded to `claude`) and execs the wrapper, which forwards the
        // daemon's `-p/--model/--effort/--dangerously-skip-permissions` argv
        // verbatim. Appended AFTER `--dangerously-skip-permissions` so the
        // positional prompt contract (#4111/#4121) is unchanged and existing
        // argv-prefix assertions still hold. Operators can force the legacy
        // single-shot path with `LOOM_USE_WRAPPER=0` (see
        // `wrapper_dispatch_enabled`).
        if wrapper_dispatch_enabled() {
            cmd.arg("--use-wrapper");
        }
        cmd.env("LOOM_TERMINAL_ID", format!("daemon-{sweep_id}"));
        // Issue #8835: the sweep's own id, bare and unprefixed, so anything
        // the sweep spawns can attribute its work back to the sweep that
        // caused it — see [`child_env_markers::SWEEP_ID_ENV`] for why it is a
        // second variable rather than a derivation of LOOM_TERMINAL_ID, and
        // why it is set for every dispatch kind while the two `Issue`-scoped
        // markers below are not.
        cmd.env(child_env_markers::SWEEP_ID_ENV, sweep_id);
        // The two `Issue`-scoped child markers — `LOOM_SWEEP_CLAIM_OWNED`
        // (#3823/#4111/#5342) and the lease-renewal capability marker
        // (#7672) — are set for an `Issue` dispatch and *cleared* for a
        // `PrSet` one. Both the rationale and the clearing (#7915) live in
        // [`child_env_markers::apply_issue_scoped_markers`].
        child_env_markers::apply_issue_scoped_markers(&mut cmd, kind);
        crate::observability::tracing::prepare_child(
            &mut cmd,
            &self.config.workspace_root,
            sweep_id,
        );
        cmd
            // Always pin LOOM_WORKSPACE to the registry's configured root so
            // spawn-claude.sh resolves `.loom/tokens/` from the same place
            // the daemon thinks the workspace is — never inheriting an
            // ambient value that might point elsewhere.
            .env(WORKSPACE_ENV, &self.config.workspace_root)
            // Issue #3943: the child is a headless `claude -p "/loom:sweep N"`
            // session. In print mode the Claude Code harness terminates
            // still-running background tasks — the sweep's dispatched
            // Builder/Judge subagents — after a 600s ceiling and exits the
            // session, killing any role phase that runs >10 minutes mid-build
            // and causing loom:building<->loom:issue label ping-pong. Disable
            // the ceiling (0 = no cap) explicitly on the child env so a long
            // Builder/Judge phase runs to completion. `spawn-claude.sh` also
            // sets this (belt-and-suspenders), but we pin it here too so the
            // daemon dispatch path does not depend on the wrapper doing it.
            .env(BG_WAIT_CEILING_ENV, "0")
            // Issue #3730: pin the child's cwd to the resolved workspace root
            // so the child's relative `.loom/config.json` read
            // (loom_tools/sweep_experiment.py) and archive-transcripts.sh's
            // cwd-slug resolve deterministically, rather than depending on the
            // daemon's own cwd happening to be the workspace root.
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));
        // Per-owner credential routing (#5401/#5431, gap closed for
        // role-runner children by #5508/#5522; this is the THIRD dispatch
        // path with the same gap, #6529): this sweep child is spawned with
        // `current_dir(&self.config.workspace_root)` above, so it must carry
        // the SAME per-owner `GH_CONFIG_DIR` every other per-repo `gh`/`git`
        // child-spawn call site already does — otherwise a workspace
        // registered under a non-default owner inherits whatever
        // `GH_CONFIG_DIR` the daemon process (or a PREVIOUSLY dispatched
        // sweep child for a DIFFERENT workspace) happens to have set, and
        // every forge call the spawned `/loom:sweep` session makes 404s —
        // silently, before the sweep's first checkpoint is written. A total
        // no-op for a single-owner fleet or the root owner's own repos.
        //
        // #6722: the plain lookup (`apply_gh_config_for_root`) is a pure
        // registry read, so it is also a silent no-op for a cross-owner root
        // that genuinely needs a per-owner credential but was never
        // registered — `daemon_service.rs`'s registration pass runs exactly
        // ONCE, at startup, so a workspace root added to an already-running
        // daemon (`loom-daemon workspace add`) is invisible to it, and the
        // spawn path here has no `gh` call of its own to 404 on and trigger
        // the existing `forge_listing.rs`-only reactive recovery. Use the
        // recovery-aware wrapper instead, which attempts one eager mint
        // before falling through when the registry misses AND the root's
        // owner genuinely differs from this daemon's own — see
        // `apply_gh_config_for_root_with_recovery`'s doc comment.
        crate::credential_preflight::apply_gh_config_for_root_with_recovery(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Some(admission) = runtime_admission {
            cmd.env("LOOM_RUNTIME", &admission.runtime);
            // Issue #4768: pin the ALREADY-ADMITTED role alongside the runtime
            // it was admitted for. Without this, a Codex-runtime sweep child
            // reaches `spawn-codex.sh` with no `LOOM_ROLE` at all (bash env
            // vars only propagate what the parent process actually set — this
            // `Command` never set one), which `spawn-codex.sh` treats as an
            // ambiguous/unknown role and silently takes the READ-ONLY
            // sandbox-fallback path instead of the mutable-role hook-trust
            // preflight. `admission.role` is always `"sweep-lifecycle"` here
            // (a full sweep is modelled as one launch, admitted against
            // Builder's requirements — see runtime_admission.rs's module
            // doc), which `spawn-codex.sh` maps onto `builder` for its own
            // mutable-role check.
            cmd.env("LOOM_ROLE", &admission.role);
            log::info!(
                "sweep_registry: admitted role={} runtime={} source={}",
                admission.role,
                admission.runtime,
                admission.source
            );
            // #6201: same loud, at-selection divergence diagnostic as
            // `role_runner`'s standalone role ticks — see
            // `suggested_worker_type_mismatch_warning`'s doc comment.
            if let Some(msg) =
                crate::runtime_admission::suggested_worker_type_mismatch_warning(admission)
            {
                log::warn!("{msg}");
            }
        }

        // Issue #3800: put the sweep child in its OWN process group
        // (`setpgid(0, 0)` runs post-fork/pre-exec via `process_group(0)`,
        // stable since Rust 1.64). spawn-claude.sh ends in `exec claude`, so
        // the tracked PID becomes the `claude` process itself AND the leader
        // of a fresh group. `claude` forks real OS subprocesses for tool
        // execution (Bash-tool commands, MCP servers, git clones, …); those
        // descendants inherit this group. Making the child a group leader lets
        // `cancel()` signal the WHOLE group (`kill(-pgid, sig)`) so the entire
        // sweep subtree is torn down — instead of leaving orphans behind when
        // only the top-level PID is signalled.
        #[cfg(unix)]
        cmd.process_group(0);

        // Issue #3730: explicitly forward the experiment-related env vars to
        // the detached child via an EXPLICIT ALLOWLIST — never a blanket
        // env_clear/copy. Without this, `LOOM_MODEL_EXPERIMENT` /
        // `LOOM_MODEL_EXPERIMENT_CANARY` / `LOOM_TRANSCRIPT_ARCHIVE` only reach
        // the child if the daemon *itself* was launched with them; an operator
        // exporting them before dispatching would get a silent no-effect.
        //
        // `var_os` guards each name: an UNSET var is not forwarded, and an
        // empty-string value is not forwarded either (no empty-string
        // forwarding — mirrors the archiver / experiment-parser treatment of
        // empty as "unset"). This keeps the spawn a byte-for-byte no-op when
        // none of the vars are set.
        //
        // Issue #6667 adds the build-cache group on the same terms: a fleet
        // host sets them on the daemon's supervisor (launchd/systemd), and a
        // sweep's `cargo build` reuses S3-cached objects instead of
        // cold-compiling the workspace in every fresh worktree. Forwarding is
        // explicit here rather than left to plain process inheritance so the
        // guarantee survives any future `env_clear()` on this `Command` — and
        // so the set of names that may cross into a sweep child stays
        // reviewable in one place.
        for name in EXPERIMENT_ENV_ALLOWLIST
            .iter()
            .chain(BUILD_CACHE_ENV_ALLOWLIST.iter())
        {
            if let Some(val) = std::env::var_os(name) {
                if !val.is_empty() {
                    cmd.env(name, val);
                }
            }
        }

        if let Some(selection) = selection {
            selection.apply(&mut cmd);
        }
        let child = crate::observability::lifecycle::spawn_child(
            &mut cmd,
            &self.config.workspace_root,
            sweep_id,
        )
        .with_context(|| format!("failed to spawn {} -p '{}'", spawn_bin.display(), prompt))?;
        // Issue #3801: we RETAIN the `Child` handle (returned to `dispatch`,
        // which stores it in `self.children`) instead of dropping it. The
        // reaper `try_wait()`s it each tick so an exited child is reaped
        // (no `<defunct>` zombie) and the registry transitions to a terminal
        // state with the real exit status.
        //
        // Issue #3802: the caller polls this log for the `using OAuth
        // account '<name>'` marker via `poll_and_classify_spawned_child`
        // (Issue #6592 — split out of this method so that potentially
        // multi-second poll can run without the registry mutex held). The
        // scan is anchored to THIS dispatch's header line (`sweep_id=<id>`)
        // so a stale line from a previous dispatch appended to the same
        // per-issue log is never mistaken for the current selection.
        if let Some(selection) = selection {
            selection.spawned();
        }
        let header_anchor = format!("sweep_id={sweep_id}");
        Ok((child, header_anchor))
    }
}
