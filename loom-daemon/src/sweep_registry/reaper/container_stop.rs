//! Container teardown for cancelled containerized sweeps (issue #8435 — the
//! ADR-0017 obligation #7429/#7430 deferred and #8403 inherited).
//!
//! # The leak this closes
//!
//! A containerized sweep is dispatched by `exec`ing `docker run --rm …` —
//! `defaults/scripts/spawn-claude.sh` for the Claude ephemeral shape
//! (#7429/#7430), `worker_spawn::containment` for the native one (#8403). The
//! process the registry supervises is the docker CLI **client**, not the
//! container: dockerd owns the container's lifetime, and `docker run` needs no
//! host-side supervising process to keep it alive. So when
//! [`begin_cancel`](super::SweepRegistry::begin_cancel) SIGTERMs (and
//! [`finish_cancel`](super::SweepRegistry::finish_cancel) SIGKILLs) the sweep's
//! process group, that kills only the client — the container keeps running to
//! completion, burning the resource caps it was dispatched under and holding
//! the worktree.
//!
//! # One mechanism, both shapes
//!
//! Every containerized dispatch stamps the same labels on its container, so
//! identification needs no new bookkeeping and no per-shape branch:
//!
//! - `loom.sweep.issue=<N>` — the issue the sweep was dispatched for (applied
//!   whenever the daemon-set `LOOM_SWEEP_CLAIM_OWNED` is in the child env);
//! - `loom.dispatch=container` — marks a loom dispatch container;
//! - `loom.containment=<kind>` — names the shape (`claude-ephemeral` /
//!   `native-ephemeral`) for logging.
//!
//! The two halves of the cancel split call in symmetrically, so the explicit
//! `cancel_sweep` verb (both the blocking and the #3807 non-blocking
//! orchestration) AND every watchdog/deadline-driven cancel — which all compose
//! the same [`begin_cancel`](super::SweepRegistry::begin_cancel) →
//! [`finish_cancel`](super::SweepRegistry::finish_cancel) pair — stop the
//! container:
//!
//! 1. **begin** (before/alongside the process-group SIGTERM): `docker stop
//!    --time <grace>` on a detached thread. `docker stop` blocks up to its
//!    grace while dockerd runs the container-side SIGTERM → wait → SIGKILL
//!    escalation, and begin_cancel must stay quick and lock-scoped (#3807), so
//!    the wait happens off-thread. `--rm` (both dispatch shapes pass it) makes
//!    dockerd remove the stopped container — no `docker rm` bookkeeping here.
//! 2. **finish** (on expiry — the same condition that escalates the host-side
//!    SIGKILL): re-list by label and `docker kill` whatever is still running,
//!    covering a `docker stop` client that died before dockerd received or
//!    completed the request.
//!
//! # Fail-safe contract (#8435 AC3)
//!
//! A docker CLI failure during cancellation must not abort the rest of the
//! cancellation sequence: every failure mode — missing binary, spawn error,
//! timeout, non-zero exit, unparseable output — is logged and skipped, and the
//! ordinary no-container case (a bare-metal sweep, a container that already
//! exited, a host with no docker at all) is a debug-level no-op that neither
//! fails nor logs an error.

use super::{output_with_timeout, reap_gh_timeout, REAP_GH_TIMEOUT};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Env var overriding the docker CLI binary the cancellation path invokes
/// (`LOOM_DOCKER_BIN`). Mirrors [`SPAWN_BIN_ENV`](crate::sweep_registry::SPAWN_BIN_ENV)'s
/// role for the spawn path: production resolves `docker` from `PATH`; tests
/// inject a fake that records its argv.
pub(crate) const DOCKER_BIN_ENV: &str = "LOOM_DOCKER_BIN";

/// Docker label naming the issue a containerized sweep was dispatched for.
pub(crate) const SWEEP_ISSUE_LABEL: &str = "loom.sweep.issue";

/// Docker label marking a container as a loom dispatch (as opposed to any
/// other container on the host wearing a `loom.sweep.issue` label).
pub(crate) const DISPATCH_LABEL: &str = "loom.dispatch";

/// The `loom.dispatch` value both containerized dispatch shapes apply.
pub(crate) const DISPATCH_LABEL_CONTAINER: &str = "container";

/// Docker label naming the containment shape (`claude-ephemeral` /
/// `native-ephemeral`). Purely informational for this module — identification
/// keys off the issue + dispatch labels above; the kind only enriches logs.
pub(crate) const CONTAINMENT_KIND_LABEL: &str = "loom.containment";

/// `docker ps --format` template: container id + the containment-kind label
/// (empty when the label is absent), tab-separated — one line per container.
/// Built from [`CONTAINMENT_KIND_LABEL`] so the label name is single-sourced
/// between this template and any future reader.
fn ps_format() -> String {
    format!("{{{{.ID}}}}\t{{{{.Label \"{CONTAINMENT_KIND_LABEL}\"}}}}")
}

/// One running container identified for a cancelled issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueContainer {
    /// Container id (short form — `docker ps` prints the 12-char form by
    /// default, and every subsequent `docker stop`/`docker kill` accepts it).
    pub(crate) id: String,
    /// The `loom.containment` label value, when the dispatch stamped one.
    pub(crate) containment: Option<String>,
}

/// Resolve the docker CLI binary: [`DOCKER_BIN_ENV`] override, else `docker`
/// from `PATH`.
pub(crate) fn docker_program() -> PathBuf {
    std::env::var(DOCKER_BIN_ENV)
        .ok()
        .map(|v| PathBuf::from(v.trim().to_string()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("docker"))
}

/// argv (after the program) identifying a cancelled issue's container(s):
/// `ps --filter label=loom.sweep.issue=<N> --filter label=loom.dispatch=container
/// --format <template>`.
pub(crate) fn ps_args(issue: u32) -> Vec<String> {
    vec![
        "ps".to_string(),
        "--filter".to_string(),
        format!("label={SWEEP_ISSUE_LABEL}={issue}"),
        "--filter".to_string(),
        format!("label={DISPATCH_LABEL}={DISPATCH_LABEL_CONTAINER}"),
        "--format".to_string(),
        ps_format(),
    ]
}

/// argv (after the program) for the graceful half: `stop --time <grace_secs>
/// <id…>`. A sub-second grace rounds to `--time 0`, which is `docker stop`'s
/// own "skip SIGTERM, SIGKILL now" — matching a host-side cancel whose grace
/// is already that tight.
pub(crate) fn stop_args(grace: Duration, containers: &[IssueContainer]) -> Vec<String> {
    let mut args = vec![
        "stop".to_string(),
        "--time".to_string(),
        grace.as_secs().to_string(),
    ];
    args.extend(containers.iter().map(|c| c.id.clone()));
    args
}

/// argv (after the program) for the expiry escalation: `kill <id…>` (docker's
/// default signal is SIGKILL).
pub(crate) fn kill_args(containers: &[IssueContainer]) -> Vec<String> {
    let mut args = vec!["kill".to_string()];
    args.extend(containers.iter().map(|c| c.id.clone()));
    args
}

fn command_with(program: &Path, args: Vec<String>) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// Parse one `docker ps --format` output line into an [`IssueContainer`].
/// `None` for blank lines, lines with no tab, or an empty id — never panics.
pub(crate) fn parse_ps_line(line: &str) -> Option<IssueContainer> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (id, kind) = line.split_once('\t')?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let kind = kind.trim();
    Some(IssueContainer {
        id: id.to_string(),
        containment: if kind.is_empty() {
            None
        } else {
            Some(kind.to_string())
        },
    })
}

/// List the running container(s) dispatched for `issue`, bounded by the same
/// reaper call budget the `gh` probes use. Fail-safe: any failure yields an
/// empty list (with a log line at the appropriate level), never an error.
pub(crate) fn list_issue_containers(program: &Path, issue: u32) -> Vec<IssueContainer> {
    let cmd = command_with(program, ps_args(issue));
    match output_with_timeout(cmd, reap_gh_timeout()) {
        Ok(Some(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(parse_ps_line)
            .collect(),
        Ok(Some(out)) => {
            log::warn!(
                "cancel_sweep: `docker ps` for issue #{issue} exited {:?} — assuming no \
                 container to stop (#8435)",
                out.status.code()
            );
            Vec::new()
        }
        Ok(None) => {
            log::warn!(
                "cancel_sweep: `docker ps` for issue #{issue} timed out — assuming no \
                 container to stop (#8435)"
            );
            Vec::new()
        }
        Err(e) => {
            // Spawn failure — most commonly no docker binary on this host at
            // all (a bare-metal fleet host). The ordinary no-container case is
            // debug-level by contract: not an error, not a warning.
            log::debug!(
                "cancel_sweep: `docker ps` for issue #{issue} could not run ({e}) — no \
                 container to stop (#8435)"
            );
            Vec::new()
        }
    }
}

fn describe(containers: &[IssueContainer]) -> String {
    containers
        .iter()
        .map(|c| match &c.containment {
            Some(kind) => format!("{} [{}]", c.id, kind),
            None => c.id.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Begin half of the container teardown, called from
/// [`begin_cancel`](super::SweepRegistry::begin_cancel) BEFORE/ALONGSIDE the
/// process-group SIGTERM (issue #8435): list the issue's container(s) by label
/// and issue `docker stop --time <grace>` on a detached thread so the
/// lock-scoped cancel step never blocks on dockerd's grace.
///
/// No containers found — including no docker binary, a failed or timed-out
/// `docker ps` — is a debug-level no-op; a stop that does run logs what it
/// stopped and how it ended. Nothing here can fail the cancel itself.
pub(crate) fn begin_container_stop(sweep_id: &str, issue: u32, grace: Duration) {
    begin_container_stop_with(&docker_program(), sweep_id, issue, grace);
}

/// [`begin_container_stop`] with an explicit docker program — the seam the
/// unit tests use to inject a fake CLI (mirroring how `spawn_bin` injects a
/// fake spawn script elsewhere in the registry) without touching the
/// process-global [`DOCKER_BIN_ENV`], which parallel non-serial tests would
/// race.
pub(crate) fn begin_container_stop_with(
    program: &Path,
    sweep_id: &str,
    issue: u32,
    grace: Duration,
) {
    let program = program.to_path_buf();
    let containers = list_issue_containers(&program, issue);
    if containers.is_empty() {
        log::debug!(
            "cancel_sweep: no running container labelled for issue #{issue} (sweep \
             {sweep_id}) — nothing to stop (#8435)"
        );
        return;
    }
    log::info!(
        "cancel_sweep: stopping {} container(s) for issue #{issue} (sweep {sweep_id}): \
         {} — docker stop --time {}s (#8435)",
        containers.len(),
        describe(&containers),
        grace.as_secs()
    );
    let spawn_result = std::thread::Builder::new()
        .name("loom-container-stop".to_string())
        .spawn(move || {
            // `docker stop` blocks up to its own --time grace while dockerd
            // escalates SIGTERM → SIGKILL container-side. Bound the CLIENT to
            // grace + the reaper's subprocess budget so a wedged CLI cannot
            // outlive the cancel either; dockerd keeps executing an in-flight
            // stop after its client dies, and the finish half below (plus
            // `--rm`'s daemon-side removal) covers the rest.
            let bound = grace + REAP_GH_TIMEOUT;
            let cmd = command_with(&program, stop_args(grace, &containers));
            match output_with_timeout(cmd, bound) {
                Ok(Some(out)) if out.status.success() => {
                    log::info!(
                        "cancel_sweep: docker stop for issue #{issue} completed ({}) (#8435)",
                        String::from_utf8_lossy(&out.stdout).trim()
                    );
                }
                Ok(Some(out)) => {
                    log::warn!(
                        "cancel_sweep: docker stop for issue #{issue} exited {:?} — the \
                         container may outlive the cancel (#8435)",
                        out.status.code()
                    );
                }
                Ok(None) => {
                    log::warn!(
                        "cancel_sweep: docker stop client for issue #{issue} exceeded {}s \
                         and was killed — dockerd may still complete the stop (#8435)",
                        bound.as_secs()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "cancel_sweep: docker stop for issue #{issue} could not run ({e}) \
                         (#8435)"
                    );
                }
            }
        });
    if let Err(e) = spawn_result {
        log::warn!(
            "cancel_sweep: could not spawn the docker stop thread for issue #{issue}: {e} \
             (#8435)"
        );
    }
}

/// Finish half of the container teardown, called from
/// [`finish_cancel`](super::SweepRegistry::finish_cancel) (issue #8435).
/// `escalate` is the same "did not exit within grace" condition that fires
/// the host-side SIGKILL: on expiry, re-list the issue's container(s) by label
/// and `docker kill` whatever is still running — a `docker stop` client that
/// died before dockerd acted, a wedged stop, a container the stop never
/// reached. When the sweep DID exit within grace, nothing to do: a
/// `docker run` client returns exactly when its container exits, so the
/// container is already down (and `--rm` has removed it); any stop issued by
/// the begin half finishes on its own thread.
///
/// No containers still running is a debug-level no-op; every failure is
/// logged and skipped — nothing here can fail the cancel itself.
pub(crate) fn finish_container_stop(sweep_id: &str, issue: u32, escalate: bool) {
    finish_container_stop_with(&docker_program(), sweep_id, issue, escalate);
}

/// [`finish_container_stop`] with an explicit docker program (test seam, see
/// [`begin_container_stop_with`]).
pub(crate) fn finish_container_stop_with(
    program: &Path,
    sweep_id: &str,
    issue: u32,
    escalate: bool,
) {
    if !escalate {
        return;
    }
    let containers = list_issue_containers(program, issue);
    if containers.is_empty() {
        log::debug!(
            "cancel_sweep: no container left to kill for issue #{issue} (sweep {sweep_id}) \
             (#8435)"
        );
        return;
    }
    log::warn!(
        "cancel_sweep: container(s) for issue #{issue} (sweep {sweep_id}) survived the \
         grace window — docker kill: {} (#8435)",
        describe(&containers)
    );
    let cmd = command_with(program, kill_args(&containers));
    match output_with_timeout(cmd, reap_gh_timeout()) {
        Ok(Some(out)) if out.status.success() => {
            log::info!(
                "cancel_sweep: docker kill for issue #{issue} completed ({}) (#8435)",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        Ok(Some(out)) => {
            log::warn!(
                "cancel_sweep: docker kill for issue #{issue} exited {:?} — the container \
                 may outlive the cancel (#8435)",
                out.status.code()
            );
        }
        Ok(None) => {
            log::warn!(
                "cancel_sweep: docker kill for issue #{issue} timed out — the container may \
                 outlive the cancel (#8435)"
            );
        }
        Err(e) => {
            log::warn!("cancel_sweep: docker kill for issue #{issue} could not run ({e}) (#8435)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::wait_for_condition;

    fn container(id: &str, kind: Option<&str>) -> IssueContainer {
        IssueContainer {
            id: id.to_string(),
            containment: kind.map(ToString::to_string),
        }
    }

    // --- argv shape ------------------------------------------------------

    #[test]
    fn ps_args_filter_by_issue_and_dispatch_labels() {
        assert_eq!(
            ps_args(8435),
            vec![
                "ps",
                "--filter",
                "label=loom.sweep.issue=8435",
                "--filter",
                "label=loom.dispatch=container",
                "--format",
                "{{.ID}}\t{{.Label \"loom.containment\"}}",
            ]
        );
    }

    #[test]
    fn stop_args_carry_grace_then_ids() {
        assert_eq!(
            stop_args(
                Duration::from_secs(5),
                &[container("deadbeef9c01", Some("claude-ephemeral")),]
            ),
            vec!["stop", "--time", "5", "deadbeef9c01"]
        );
        assert_eq!(
            stop_args(
                Duration::from_secs(3),
                &[
                    container("aaaa00000001", None),
                    container("bbbb00000002", Some("native-ephemeral")),
                ]
            ),
            vec!["stop", "--time", "3", "aaaa00000001", "bbbb00000002"]
        );
    }

    #[test]
    fn stop_args_round_a_sub_second_grace_to_zero() {
        // The drain path cancels with a 500ms grace; `docker stop --time 0`
        // is docker's own "SIGKILL now", matching that intent.
        assert_eq!(
            stop_args(Duration::from_millis(500), &[container("c1", None)]),
            vec!["stop", "--time", "0", "c1"]
        );
    }

    #[test]
    fn kill_args_are_the_bare_escalation() {
        assert_eq!(
            kill_args(&[container("c1", None), container("c2", None)]),
            vec!["kill", "c1", "c2"]
        );
    }

    // --- `docker ps` output parsing --------------------------------------

    #[test]
    fn parse_ps_line_splits_id_and_kind() {
        assert_eq!(
            parse_ps_line("deadbeef9c01\tclaude-ephemeral\n"),
            Some(container("deadbeef9c01", Some("claude-ephemeral")))
        );
    }

    #[test]
    fn parse_ps_line_without_kind_label_yields_none_kind() {
        assert_eq!(parse_ps_line("deadbeef9c01\t"), Some(container("deadbeef9c01", None)));
    }

    #[test]
    fn parse_ps_line_rejects_blank_and_untabbed_lines() {
        assert_eq!(parse_ps_line(""), None);
        assert_eq!(parse_ps_line("   "), None);
        assert_eq!(parse_ps_line("notabshere"), None);
        assert_eq!(parse_ps_line("\tkindonly"), None);
    }

    // --- fail-safe listing ------------------------------------------------

    #[test]
    fn list_with_an_unrunnable_program_is_an_empty_noop() {
        // No docker binary at this path: spawn fails, the list comes back
        // empty, nothing panics — the bare-metal-host case.
        let listed = list_issue_containers(Path::new("/nonexistent/loom-docker-8435"), 4242);
        assert!(listed.is_empty());
    }

    // --- behavioral (fake docker injected via the `_with` seam) -----------

    /// Write a fake docker CLI that appends `"$@"` to `<dir>/invocations`
    /// before dispatching on its first argument: `ps` prints `ps_stdout`
    /// (empty ⇒ no containers), everything else exits 0 quietly.
    fn fake_docker(dir: &Path, ps_stdout: &str) -> PathBuf {
        let script = dir.join("fake-docker");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> {}/invocations\nif [[ \"$1\" == ps ]]; \
                 then printf '%s' '{}'\nfi\nexit 0\n",
                dir.display(),
                ps_stdout.replace('\'', "'\\''"),
            ),
        )
        .unwrap();
        make_executable(&script);
        script
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn invocations(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("invocations")).unwrap_or_default()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loom-container-stop-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn begin_with_no_listed_containers_stops_nothing() {
        let dir = tempdir("noop");
        let fake = fake_docker(&dir, "");
        begin_container_stop_with(&fake, "sweep-issue-4242-0", 4242, Duration::from_secs(2));
        // Give a wrongly-spawned stop thread every chance to misfire.
        std::thread::sleep(Duration::from_millis(400));
        let log = invocations(&dir);
        assert_eq!(
            log.lines().count(),
            1,
            "a no-container cancel must invoke exactly the ps probe: {log}"
        );
        assert!(
            log.starts_with("ps --filter label=loom.sweep.issue=4242 --filter"),
            "unexpected probe argv: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_stops_listed_containers_and_finish_escalates_to_kill() {
        let dir = tempdir("stop-kill");
        let fake = fake_docker(&dir, "deadbeef9c01\tclaude-ephemeral\n");
        begin_container_stop_with(&fake, "sweep-issue-4242-0", 4242, Duration::from_secs(5));
        let stop_seen =
            wait_for_condition(5_000, || invocations(&dir).contains("stop --time 5 deadbeef9c01"));
        assert!(stop_seen, "docker stop never ran: {}", invocations(&dir));
        // Grace expired without the sweep exiting → finish escalates.
        finish_container_stop_with(&fake, "sweep-issue-4242-0", 4242, true);
        let kill_seen =
            wait_for_condition(5_000, || invocations(&dir).contains("kill deadbeef9c01"));
        assert!(kill_seen, "docker kill never ran: {}", invocations(&dir));
        // The kill escalation must re-list first (label-keyed), so `ps`
        // appears twice before the kill.
        let log = invocations(&dir);
        assert_eq!(
            log.matches("ps --filter").count(),
            2,
            "finish must re-list by label before killing: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_without_expiry_never_kills() {
        let dir = tempdir("no-escalate");
        let fake = fake_docker(&dir, "deadbeef9c01\tclaude-ephemeral\n");
        // exited within grace: the container is already down (a `docker run`
        // client returns exactly when its container does) — no escalation,
        // and finish is not the stopping half either.
        finish_container_stop_with(&fake, "sweep-issue-4242-0", 4242, false);
        std::thread::sleep(Duration::from_millis(300));
        let log = invocations(&dir);
        assert!(log.is_empty(), "nothing should run: {log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_docker_ps_leaves_the_cancel_a_clean_noop() {
        let dir = tempdir("failing-ps");
        let script = dir.join("fake-docker");
        std::fs::write(&script, "#!/bin/bash\nexit 1\n").unwrap();
        make_executable(&script);
        // Both halves ran to completion, treated the failure as "no
        // containers", and neither panicked nor surfaced an error.
        begin_container_stop_with(&script, "sweep-issue-4242-0", 4242, Duration::from_secs(2));
        finish_container_stop_with(&script, "sweep-issue-4242-0", 4242, true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
