//! Shared test fixtures for the `sweep_registry` test suites: registry
//! builders, wait/poll helpers, and other cross-cutting test scaffolding.
//! Only compiled under `#[cfg(test)]` (see the `mod` declaration in
//! `sweep_registry/mod.rs`).

use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;
use tempfile::tempdir;

// ---- is_pid_alive: ESRCH vs EPERM (#4691) ------------------------------

/// POSIX `ESRCH` ("no such process") — 3 on Linux and macOS alike.
pub(crate) const ESRCH: i32 = 3;

/// The `api graphql` arm of a fake `gh`, answering the open-linked-PR probe.
///
/// Emits the RAW closes-graph payload the probe parses as of #5511 (the
/// `state == "OPEN"` filter moved off the wire `--jq` and into
/// `worktree_ops::gh::parse_open_linked_pr`, where it is unit-testable),
/// synthesized from `prs` — whitespace-separated PR numbers, each rendered as
/// an `OPEN` node; empty for "no open linked PR" — and exiting `exit_code`.
///
/// A non-numeric `prs` token deliberately synthesizes MALFORMED JSON, which is
/// how fixtures still exercise the `ProbeFailed`-on-garbled-output leg.
/// Requires `bash` (`[[`), like every fixture that embeds it.
pub(crate) fn fake_gh_graphql_arm(prs: &str, exit_code: i32) -> String {
    format!(
        "if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
         nodes=\"\"\n\
         for n in {prs}; do\n\
         [[ -n \"$nodes\" ]] && nodes=\"$nodes,\"\n\
         nodes=\"$nodes{{\\\"number\\\":$n,\\\"state\\\":\\\"OPEN\\\"}}\"\n\
         done\n\
         printf '{{\"data\":{{\"repository\":{{\"issue\":{{\"closedByPullRequestsReferences\":{{\"nodes\":[%s]}}}}}}}}}}\\n' \"$nodes\"\n\
         exit {exit_code}\n\
         fi\n"
    )
}

/// The `api repos/.../issues/<n>/timeline` arm of a fake `gh`, answering the
/// #5911 REST fallback for the open-linked-PR probe. Must be spliced into a
/// fixture script BEFORE any generic `$2 == repos/*` arm (e.g. the
/// closed-issue-state probe) — the timeline endpoint's path also matches that
/// glob, so ordering is load-bearing, exactly like [`fake_gh_graphql_arm`]'s
/// own placement note.
///
/// Unlike `fake_gh_graphql_arm` (which emits the RAW closes-graph payload,
/// parsed in Rust), the real `gh` invocation here carries `--jq`, so `gh`
/// itself applies the filter before this fixture would ever see output — the
/// fixture therefore emits the POST-filter shape directly: `pr` is either a
/// single bare PR number (an open cross-referenced PR was found) or empty
/// (none).
pub(crate) fn fake_gh_timeline_rest_arm(pr: &str, exit_code: i32) -> String {
    format!(
        "if [[ \"$1\" == \"api\" && \"$*\" == *timeline* ]]; then\n\
         printf '%s\\n' \"{pr}\"\n\
         exit {exit_code}\n\
         fi\n"
    )
}

/// Install the `/loom:sweep` command marker under `workspace` (Issue
/// #4027) so the workspace-commands guard in `dispatch()` treats it as
/// initialized. Only tests that run with `skip_label_flip = false` need
/// this — the guard itself is skipped when label flips are disabled
/// (see `dispatch()`'s 2.4 comment), which covers the overwhelming
/// majority of fixtures in this module.
pub(crate) fn touch_sweep_command(workspace: &Path) {
    let dir = workspace.join(".claude").join("commands").join("loom");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sweep.md"), "# /loom:sweep (test fixture)\n").unwrap();
    install_runtime_admission_fixture(workspace);
}

/// Install the minimum zero-config Claude admission surface used by real
/// dispatch fixtures. Tests exercise admission rather than bypassing it,
/// preserving the ordering contract of the established typed guards.
pub(crate) fn install_runtime_admission_fixture(workspace: &Path) {
    let roles = workspace.join(".loom/roles");
    let runtimes = workspace.join(".loom/runtimes");
    let scripts = workspace.join(".loom/scripts");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::create_dir_all(&runtimes).unwrap();
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(
        roles.join("builder.json"),
        r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
    )
    .unwrap();
    std::fs::write(
        runtimes.join("claude.json"),
        r#"{"runtime":"claude","capabilities":{"worktreeIsolation":"yes","mcp":"yes"}}"#,
    )
    .unwrap();
    let adapter = scripts.join("spawn-claude.sh");
    if !adapter.exists() {
        std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
    }
    let mut perms = std::fs::metadata(&adapter).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(adapter, perms).unwrap();
}

pub(crate) fn fixture_registry(workspace: &Path) -> (SweepRegistry, PathBuf) {
    let record_log = workspace.join("fake-spawn.log");
    // We use /bin/bash as the spawn binary, and the dispatch path appends
    // "-p <prompt>" — we ignore the args via `--`, then run an inline
    // recording script.
    let scripts_dir = workspace.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    // Use exec on bash directly with arguments: we write a wrapper that
    // bash will invoke. The wrapper is small enough that a bad chmod
    // would be a system-level problem, not a test-flake.
    let script = format!(
        r#"#!/usr/bin/env bash
# Test fixture: record dispatch args + selected env into a log.
{{
  printf 'argv: %s\n' "$*"
  # Also record each argv TOKEN on its own line (issue #4111): `$*` flattens a
  # single `-p "<prompt with spaces>"` arg into space-joined text that is
  # indistinguishable from sibling args, which is exactly why a sibling
  # `--claim-owned` (rejected by the real `claude` CLI) slipped past the
  # `$*`-substring tests. A per-token record lets a test assert the flag lands
  # INSIDE the `-p` prompt value, not as its own argv token.
  for tok in "$@"; do printf 'arg: %s\n' "$tok"; done
  printf 'CLAUDE_CODE_OAUTH_TOKEN=%s\n' "${{CLAUDE_CODE_OAUTH_TOKEN:-unset}}"
  printf 'LOOM_WORKSPACE=%s\n' "${{LOOM_WORKSPACE:-unset}}"
  printf 'PWD=%s\n' "$(pwd -P)"
  printf 'LOOM_MODEL_EXPERIMENT=%s\n' "${{LOOM_MODEL_EXPERIMENT:-unset}}"
  printf 'LOOM_MODEL_EXPERIMENT_CANARY=%s\n' "${{LOOM_MODEL_EXPERIMENT_CANARY:-unset}}"
  printf 'LOOM_TRANSCRIPT_ARCHIVE=%s\n' "${{LOOM_TRANSCRIPT_ARCHIVE:-unset}}"
  printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}"
  printf 'LOOM_TERMINAL_ID=%s\n' "${{LOOM_TERMINAL_ID:-unset}}"
  printf 'CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=%s\n' "${{CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS:-unset}}"
}} >> "{rec}" 2>&1
exit 0
"#,
        rec = record_log.display()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();
    // Sync the perms change to the filesystem so the child sees it.
    // On macOS APFS under heavy load, posix_spawn occasionally exec's
    // before the chmod is visible to the child process.
    if let Ok(f) = std::fs::File::open(&fake_bin) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    // Confine the #3953 sweep journal to this test's tempdir — never the
    // real machine-level `~/.loom/sweeps.json`.
    config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
    // Confine the #4644 durable terminal-outcomes journal to this test's
    // tempdir too, so tests can assert against a known path.
    config.outcomes_journal_path = Some(workspace.join("test-sweep-outcomes.jsonl"));
    // Confine the #4704 sweep.outcome telemetry journal likewise.
    config.outcome_telemetry_path = Some(workspace.join("test-sweep-outcome-telemetry.jsonl"));
    (SweepRegistry::new(config), record_log)
}

/// Wait until `path` exists AND contains `needle`. Returns true on
/// success, false on timeout.
/// Generous wall-clock budget for waiting on a fixture child that normally
/// completes near-instantly (writes its record file / exits). The build
/// gate historically ran the workspace suite at ~100% duty cycle (#3984),
/// and under that self-inflicted CPU starvation a fixture child that
/// "exits immediately" could still miss a 5–10s deadline purely because it
/// was never scheduled — reddening the gate for a host-load reason, not a
/// code regression (#3985). A 60s budget is orders of magnitude over the
/// real completion time on an idle host, so it never slows a green run,
/// but it absorbs even severe scheduler contention. Prefer bumping this
/// (or restructuring to await a signal) over tightening it.
///
/// NOTE: on macOS, spawning the fixture child execs a *freshly written*
/// `spawn-claude.sh`, which Gatekeeper (`syspolicyd`/`amfid`) must assess
/// before the first exec — and that assessment can stall for tens of
/// seconds when a background AV/backup (Backblaze, XProtect) has the
/// security daemons pegged. That is a host condition, not a code fault, so
/// the budget is set well above the worst stall observed in the wild
/// (#3985). Because every caller *polls* and returns the instant the
/// condition is met, a large budget costs nothing on a healthy host — it
/// only widens the tolerance before a genuinely stuck child is declared
/// failed.
pub(crate) const FIXTURE_CHILD_WAIT_MS: u64 = 120_000;

pub(crate) fn wait_for_contents(path: &Path, needle: &str, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < u128::from(timeout_ms) {
        if let Ok(s) = std::fs::read_to_string(path) {
            if s.contains(needle) {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Wait (up to `FIXTURE_CHILD_WAIT_MS`, #4044) for the fixture's fake
/// `spawn-claude.sh` to write `needle` into `record_log`, then return the
/// file's full contents for the caller's own content assertions.
///
/// Panics with a message that distinguishes "the child never started /
/// never wrote anything" from "the child started but wrote the wrong
/// thing" — the third acceptance criterion of #4044 — rather than the
/// old bare "did not finish writing within 10s".
pub(crate) fn assert_child_wrote(record_log: &Path, needle: &str) -> String {
    let wrote = wait_for_contents(record_log, needle, FIXTURE_CHILD_WAIT_MS);
    let recorded = std::fs::read_to_string(record_log).unwrap_or_default();
    assert!(
            wrote,
            "fake spawn-claude.sh did not write expected contents within {}ms\n  needle: {needle}\n  record_log exists: {}\n  record_log contents: {recorded}",
            FIXTURE_CHILD_WAIT_MS,
            record_log.exists(),
        );
    recorded
}

/// Poll `.loom/scripts/spawn-claude.sh` fixture installation: write a
/// custom script body + exec bit into `workspace` and return a registry
/// configured to spawn it. Used by the child-process-lifecycle tests
/// (#3800/#3801) which need a long-lived / tree-forking child rather than
/// the record-and-exit fake in `fixture_registry`.
pub(crate) fn lifecycle_registry(workspace: &Path, script_body: &str) -> SweepRegistry {
    let scripts_dir = workspace.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&fake_bin, script_body).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_bin) {
        let _ = f.sync_all();
    }
    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
    SweepRegistry::new(config)
}

/// Poll until `pid` becomes alive (via `kill(pid, 0)`), up to
/// `timeout_ms`. Returns true once alive, false on timeout.
pub(crate) fn wait_until_alive(pid: u32, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < u128::from(timeout_ms) {
        if is_pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    is_pid_alive(pid)
}

/// Poll until `pid` is no longer alive (via `kill(pid, 0)`), up to
/// `timeout_ms`. Returns the final liveness (false = dead = success).
pub(crate) fn wait_until_dead(pid: u32, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < u128::from(timeout_ms) {
        if !is_pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    !is_pid_alive(pid)
}

/// Read a PID written to `path` by a fixture child, polling until a
/// parseable integer appears (or timeout). Returns `None` on timeout.
pub(crate) fn read_pid_file(path: &Path, timeout_ms: u64) -> Option<u32> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < u128::from(timeout_ms) {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(p) = s.trim().parse::<u32>() {
                return Some(p);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    None
}

/// Poll until `condition` returns true or `timeout_ms` elapses. Mirrors
/// `wait_for_contents` but for an arbitrary predicate (used by the
/// journal wiring test above to wait for the fixture child's PID to die).
pub(crate) fn wait_for_condition(timeout_ms: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < u128::from(timeout_ms) {
        if condition() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

// ------------------------------------------------------------------------
// No-progress backstop (Issue #4366)
// ------------------------------------------------------------------------

/// Build a registry whose fake `gh` answers the issue-state probe
/// (`api repos/<owner>/<repo>/issues/<n>`, #4504) with `issue_state`
/// (`"OPEN"` or `"CLOSED"`, always as a NON-PR node) and `api graphql` (the
/// open-linked-PR probe) with a synthesized closes-graph payload listing
/// `graphql_prs` (whitespace-separated PR numbers, each as an `OPEN` node;
/// empty for "no open PR"). Unlike [`open_pr_guard_registry`] (which
/// hardcodes an open issue), this lets #4366's no-progress tests exercise
/// the issue-closed exemption too.
///
/// The probe consumes the RAW GraphQL payload as of #5511 (the `state ==
/// "OPEN"` filter moved from a wire `--jq` into
/// `worktree_ops::gh::parse_open_linked_pr` so it can be unit-tested), so a
/// non-numeric `graphql_prs` token synthesizes MALFORMED JSON — which is how
/// the `ProbeFailed`-on-garbled-output leg is still exercised here.
pub(crate) fn no_progress_test_registry(
    ws: &Path,
    issue_state: &str,
    graphql_prs: &str,
    skip_label_flip: bool,
) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-no-progress.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        state = state_probe_json(issue_state, false),
        gql = fake_gh_graphql_arm(graphql_prs, 0),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = skip_label_flip;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    SweepRegistry::new(config)
}

/// Insert a `Running` entry backed by a REAL, retained clean-exit child
/// (mirrors [`reaper_real_clean_exit_does_not_count_as_insta_crash`]) so
/// `poll_liveness` reports `exit_code == Some(0)` rather than the
/// no-handle fallback's `None` — required to exercise the `exit_code ==
/// Some(0)` leg of the #4366 no-progress predicate.
pub(crate) fn insert_clean_exit_running(
    registry: &mut SweepRegistry,
    issue: u32,
    seq: u32,
) -> String {
    let child = Command::new("true")
        .spawn()
        .expect("spawn `true` fixture child");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));
    let sweep_id = format!("sweep-issue-{issue}-no-progress-{seq}");
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            pid,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(issue),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    registry.children.insert(sweep_id.clone(), child);
    sweep_id
}

/// Like [`no_progress_test_registry`], but BOTH the `api graphql`
/// (open-linked-PR) probe AND its #5911 REST timeline fallback exit
/// non-zero — simulating a full forge outage (not just GraphQL quota
/// exhaustion, which the REST fallback would otherwise recover from) where
/// the PR probe fails while `issue view` still answers (`issue_state`). Used
/// by the #4452 regression: the old `Option<u32>` probe collapsed this
/// failure into `None` (indistinguishable from a verified "no open PR"), so
/// the no-progress predicate's `is_none()` was satisfied and a benign
/// self-skip wrongly accrued toward quarantine.
pub(crate) fn no_progress_pr_probe_fail_registry(ws: &Path, issue_state: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-pr-probe-fail.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"view\" ]]; then\n\
             printf '%s\\n' \"{state}\"\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
             printf 'gh: rate limit exceeded\\n' >&2\n\
             exit 1\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$*\" == *timeline* ]]; then\n\
             printf 'gh: rate limit exceeded\\n' >&2\n\
             exit 1\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        state = issue_state,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    SweepRegistry::new(config)
}

// ========================================================================
// Issue #3944 — dispatch-model resolution (precedence + source)
// ========================================================================

/// Write a `.loom/config.json` under `root` with the given JSON body.
pub(crate) fn write_config(root: &Path, body: &str) {
    let loom = root.join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    std::fs::write(loom.join("config.json"), body).unwrap();
}

// --- config_resolver migration (#4058) — tier precedence -------------- //

pub(crate) fn write_project_model_config(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

// ------------------------------------------------------------------------
// Live phase overlay in `list()` (Issue #4328)
// ------------------------------------------------------------------------

/// Insert a `Running` entry for `issue` with no `latest_phase` set (the
/// shape every live dispatch has today), with an explicit `started_at`.
pub(crate) fn insert_running_at(
    registry: &mut SweepRegistry,
    issue: u32,
    seq: u32,
    started_at: DateTime<Utc>,
) -> String {
    let sweep_id = format!("sweep-issue-{issue}-{seq}");
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            pid: std::process::id(), // any live-looking pid; list() never probes liveness
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(issue),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    sweep_id
}

/// Write a checkpoint JSON file for `issue` under `registry`'s checkpoint
/// dir, then set its mtime explicitly (so tests can position it before or
/// after a run's `started_at`).
pub(crate) fn write_checkpoint_with_mtime(
    registry: &SweepRegistry,
    issue: u32,
    phase: &str,
    mtime: SystemTime,
) {
    let dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("issue-{issue}.json"));
    std::fs::write(&path, format!(r#"{{"phase":"{phase}","issue":{issue}}}"#)).unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(mtime).unwrap();
}

// ------------------------------------------------------------------------
// Insta-crash quarantine (Issue #3939)
// ------------------------------------------------------------------------

/// Insert a fresh `Running` entry for `issue` with a guaranteed-dead PID and
/// `started_at` at `now` (so the reaper classifies its death as within the
/// insta-crash window). Returns the synthetic sweep_id.
pub(crate) fn insert_dead_running(registry: &mut SweepRegistry, issue: u32, seq: u32) -> String {
    insert_dead_running_at(registry, issue, seq, Utc::now())
}

/// Like [`insert_dead_running`] but with an explicit `started_at`, so a test
/// can position the run's start relative to a checkpoint's mtime (#4009): a
/// checkpoint written *after* `started_at` is progress by this run, one
/// written *before* it is a stale artifact of an earlier dispatch.
pub(crate) fn insert_dead_running_at(
    registry: &mut SweepRegistry,
    issue: u32,
    seq: u32,
    started_at: DateTime<Utc>,
) -> String {
    let sweep_id = format!("sweep-issue-{issue}-{seq}");
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(issue),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    sweep_id
}

// ------------------------------------------------------------------------
// Per-issue dispatch backoff / flap breaker (Issue #4485)
// ------------------------------------------------------------------------

/// A registry with an explicit, fast backoff config so a test never waits on
/// the 60s shipped default.
pub(crate) fn backoff_registry(workspace: &Path, base_secs: u64, max_secs: u64) -> SweepRegistry {
    let (mut registry, _record_log) = fixture_registry(workspace);
    registry.set_dispatch_backoff_config(DispatchBackoffConfig {
        enabled: true,
        base: Duration::from_secs(base_secs),
        max: Duration::from_secs(max_secs),
    });
    registry
}

// --- synchronous token-selection-failure fast path (Issue #4689) ---

/// Install a fake `gh` (every dispatch-path guard probe answers "open,
/// not a PR, no park label, no open linked PR" so dispatch reaches
/// `spawn_child`) and a fake `spawn-claude.sh` that reproduces exactly
/// what the real script logs immediately before its token-selection
/// `exit 78` (`EX_CONFIG`) — no `# CLAUDE_CLI_START` anywhere, matching
/// [`preflight_death_signatures`]'s `preflight-token-selection-failed`
/// row. Every `gh` invocation is recorded so tests can assert on the
/// claim-flip / claim-revert sequence.
pub(crate) fn token_selection_failure_registry(ws: &Path) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    // `api repos/*` covers the 2.5 closed-issue guard, the 2.7 park-label
    // probe, AND `restore_label_to_ready`'s PR-ness probe — all answer
    // "open issue, no labels" via the SAME line (`--jq` is ignored by this
    // fixture, matching `closed_guard_registry`'s established pattern:
    // the harmless JSON payload never happens to equal a park label or a
    // "true"/"false" pull_request answer, so every reader of it fails
    // open exactly as intended). `api graphql` (2.6 open-PR guard) answers the
    // closes-graph probe with an EMPTY node list ⇒ a verified `NoneOpen`
    // (before #5511 this fixture had no `graphql` arm at all and relied on the
    // fallthrough's empty stdout, which the raw-payload parser now — correctly —
    // reads as `ProbeFailed`).
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{{\"state\":\"open\",\"is_pr\":false}}'\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        gql = fake_gh_graphql_arm("", 0),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    // The exact prose `defaults/scripts/spawn-claude.sh` logs immediately
    // before `exit 78` when `loom-daemon tokens select` itself fails.
    std::fs::write(
        &spawn,
        "#!/usr/bin/env bash\n\
             echo '[2026-07-30T00:00:00Z] ERROR Token selection failed:' >&2\n\
             echo '[2026-07-30T00:00:00Z] ERROR All 2 tokens in .loom/tokens are marked bad \
             or empty.' >&2\n\
             exit 78\n",
    )
    .unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real flip + revert path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- spawn_child hard failure (Issue #5236) ---

/// Install a fake `gh` (identical contract to
/// [`token_selection_failure_registry`] — every dispatch-path guard probe
/// answers "open, not a PR, no park label, no open linked PR") and point
/// `spawn_bin` at a file that does not exist, so `spawn_child`'s
/// `resolve_spawn_bin()` resolves the override (an explicit override is
/// trusted verbatim, unlike the installed/defaults fallback probes) but
/// `Command::spawn()` then fails at the OS level (`ENOENT`) — the exact
/// repro this issue's Test Plan calls for: "point a registered workspace's
/// spawn-bin resolution at a nonexistent file... and dispatch twice".
///
/// Unlike [`token_selection_failure_registry`] (a child that starts, then
/// dies immediately) this never spawns a process at all — `dispatch_inner`'s
/// `spawn_child` call itself returns `Err`, exercising the earlier failure
/// path this issue's AC #1 covers.
pub(crate) fn spawn_bin_missing_registry(ws: &Path) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{{\"state\":\"open\",\"is_pr\":false}}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    // Deliberately nonexistent — never written to disk.
    config.spawn_bin = Some(ws.join(".loom/scripts/does-not-exist-spawn-worker.sh"));
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real flip + revert path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// ------------------------------------------------------------------------
// Account-exhaustion attribution at insta-crash time (Issue #4122)
// ------------------------------------------------------------------------

/// Seed a per-repo token pool under `workspace/.loom/tokens` so
/// `bad_tokens::{mark_bad,is_bad}` resolve to this test's tempdir rather
/// than the host's real shared pool (`resolve_tokens_dir` only selects the
/// per-repo pool when it holds at least one `*.token` file).
pub(crate) fn seed_token_pool(workspace: &Path, token: &str) {
    let dir = workspace.join(".loom").join("tokens");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{token}.token")), "sk-ant-oat01-fake").unwrap();
}

/// Insert a dead `Running` entry for `issue` whose captured spawn account is
/// `token` and whose log contains `log_body`. Returns the sweep_id.
pub(crate) fn insert_dead_running_with_log(
    registry: &mut SweepRegistry,
    issue: u32,
    seq: u32,
    token: &str,
    log_body: &str,
) -> String {
    let sweep_id = insert_dead_running(registry, issue, seq);
    let log_path = {
        let info = registry.entries.get_mut(&sweep_id).unwrap();
        info.token_name = token.to_string();
        info.log_path.clone()
    };
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(&log_path, log_body).unwrap();
    sweep_id
}

// ====================================================================
// Cross-host collision detection (Issue #4085, Phase 0 of #4028)
// ====================================================================

/// Install a fake `gh` that logs its argv to `gh_log`, emits `stdout` on
/// stdout, and exits with `exit_code`. Returns the fake binary path. Reuses
/// the established bash-stub pattern (the `--json labels` payload is the one
/// addition the collision tests need over the flip/restore stubs).
pub(crate) fn install_fake_gh(dir: &Path, gh_log: &Path, stdout: &str, exit_code: i32) -> PathBuf {
    let fake_gh = dir.join("fake-gh.sh");
    let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\nprintf '%s' '{out}'\nexit {code}\n",
            log = gh_log.display(),
            out = stdout.replace('\'', "'\\''"),
            code = exit_code,
        );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    fake_gh
}

pub(crate) fn collision_registry(
    dir: &Path,
    gh_log: &Path,
    stdout: &str,
    exit_code: i32,
) -> SweepRegistry {
    let fake_gh = install_fake_gh(dir, gh_log, stdout, exit_code);
    let mut config = SweepRegistryConfig::new(dir.to_path_buf());
    config.gh_bin = Some(fake_gh);
    SweepRegistry::new(config)
}

/// Like [`install_fake_gh`], but also appends the child's own `GH_CONFIG_DIR`
/// (or the literal `<unset>`) to `gh_log` on its own line before the argv
/// line (Issue #5431) — used to verify a call site actually threaded
/// `credential_preflight::apply_gh_config_for_root`/`_cwd`'s env override
/// through to the real child `Command`, not just that the helper function
/// itself is correct (already covered by `credential_preflight`'s own unit
/// tests).
pub(crate) fn install_fake_gh_env_logger(
    dir: &Path,
    gh_log: &Path,
    stdout: &str,
    exit_code: i32,
) -> PathBuf {
    let fake_gh = dir.join("fake-gh-env-logger.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf 'GH_CONFIG_DIR=%s\\n' \"${{GH_CONFIG_DIR:-<unset>}}\" >> \
         \"{log}\"\nprintf '%s\\n' \"$*\" >> \"{log}\"\nprintf '%s' '{out}'\nexit {code}\n",
        log = gh_log.display(),
        out = stdout.replace('\'', "'\\''"),
        code = exit_code,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    fake_gh
}

/// RAII guard that scrubs `GH_CONFIG_DIR` from the *test process's own*
/// environment for the lifetime of the guard, restoring whatever value (or
/// absence) preceded it on drop.
///
/// Mirrors `role_runner.rs`'s private `ClearedGhConfigDirEnv` (added for
/// #5508), shared here for the `sweep_registry` "unregistered workspace root
/// is a no-op" tests (guards.rs / quarantine.rs / watchdog.rs) that spawn
/// [`install_fake_gh_env_logger`]'s fake `gh` and assert its child inherits
/// `GH_CONFIG_DIR=<unset>`.
///
/// Without this guard, the assertion only holds on a clean CI runner. A real
/// Loom fleet worker host runs the daemon (and any `cargo nextest` invoked
/// directly on that host) with `GH_CONFIG_DIR` already exported
/// host/process-wide (#4458) — that ambient value leaks straight through
/// [`install_fake_gh_env_logger`]'s fake `gh` script, which logs its own
/// *inherited* `GH_CONFIG_DIR`, and the "unset" assertion fails even though
/// the production no-op behavior it exercises (`credential_preflight.rs`'s
/// "child inherits the process-global `GH_CONFIG_DIR` untouched" contract)
/// is unchanged (#5651). Every call site pairs this with the existing
/// `#[serial]` attribute so the process-global scrub cannot race a
/// concurrent test that depends on `GH_CONFIG_DIR`.
pub(crate) struct ClearedGhConfigDirEnv(Option<String>);

impl ClearedGhConfigDirEnv {
    pub(crate) fn new() -> Self {
        let prior = std::env::var("GH_CONFIG_DIR").ok();
        std::env::remove_var("GH_CONFIG_DIR");
        Self(prior)
    }
}

impl Drop for ClearedGhConfigDirEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("GH_CONFIG_DIR", v),
            None => std::env::remove_var("GH_CONFIG_DIR"),
        }
    }
}

// --- dispatch-time live-claim guard (Issue #4556) ---

/// Seed the machine-level sweep journal (confined to this fixture's tempdir)
/// with a `(repo, issue, pid)` record — the "a sweep is alive but the lock
/// and label are already gone" state that #4275's re-dispatch paths created.
pub(crate) fn write_journal_entry(reg: &SweepRegistry, repo: &str, issue: u32, pid: u32) {
    let path = reg.config.resolve_journal_path().unwrap();
    let journal = crate::sweep_journal::SweepJournal {
        version: crate::sweep_journal::JOURNAL_VERSION,
        entries: vec![crate::sweep_journal::JournalEntry {
            repo: repo.to_string(),
            issue,
            pid,
            started_at: Utc::now(),
        }],
    };
    std::fs::write(&path, serde_json::to_string(&journal).unwrap()).unwrap();
}

/// A stand-in for a real sweep child, killed on drop: a long-lived process
/// whose argv contains `/loom:sweep <issue>` (`sh -c <cmd> <argv0>` puts
/// the third arg in `$0`, so the needle lands in the process's argv without
/// being interpreted). Mirrors what [`SweepRegistry::spawn_child`] produces
/// — `spawn-worker.sh -p "/loom:sweep <N> --claim-owned <N>"`, which
/// `exec`s through to `claude` keeping both the PID and the needle — so the
/// #4556 guard's argv verification sees a realistic claim holder rather
/// than the test binary's own PID.
pub(crate) struct FakeSweep(std::process::Child);

impl FakeSweep {
    pub(crate) fn spawn(issue: u32) -> Self {
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 120")
            .arg(format!("/loom:sweep {issue} --claim-owned {issue}"))
            .spawn()
            .unwrap();
        // `spawn` returns at fork, not exec — wait for the argv to become
        // visible or every argv-verifying assertion below is a load-flake.
        crate::live_claim::wait_until_argv_visible(child.id(), issue);
        Self(child)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for FakeSweep {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// --- worktree_dirty / clean_worktree filesystem probes ---

/// Create a git repo at `.loom/worktrees/issue-<N>` with one commit plus an
/// untracked file, so `worktree_dirty` reports it dirty. Returns the path.
pub(crate) fn make_dirty_git_worktree(ws: &Path, issue: u32) -> PathBuf {
    let wt = ws
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{issue}"));
    std::fs::create_dir_all(&wt).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(wt.join("committed.txt"), "base\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    // Now dirty it with an untracked file (mimics mid-build edits).
    std::fs::write(wt.join("dirty.txt"), "uncommitted mid-build edit\n").unwrap();
    wt
}

/// Insert a terminal (`Exited`) Issue entry directly, mimicking the state
/// the reaper leaves after a dead child is reaped.
pub(crate) fn insert_terminal_issue(
    reg: &mut SweepRegistry,
    sid: &str,
    issue: u32,
    pr: Option<i32>,
) {
    reg.entries.insert(
        sid.to_string(),
        SweepInfo {
            pgid: None,
            sweep_id: sid.to_string(),
            kind: SweepKind::Issue(issue),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: reg.compute_log_path(issue),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Exited {
                code: None,
                at: Utc::now(),
            },
            latest_phase: None,
            pr_number: pr,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
}

// --- #4449: the live-use veto on the destructive recovery path -----------

/// Assert the watchdog refused to touch issue `N`'s dirty worktree: nothing
/// recovered, the uncommitted edit still on disk, and — critically — the
/// single recovery retry NOT consumed (the worktree may be free later).
pub(crate) fn assert_midbuild_refused(reg: &mut SweepRegistry, ws: &Path, issue: u32, why: &str) {
    assert_eq!(reg.midbuild_watchdog_once(), 0, "no recovery when {why}");
    assert!(
        ws.join(format!(".loom/worktrees/issue-{issue}/dirty.txt"))
            .exists(),
        "uncommitted work MUST survive when {why} (#4449)"
    );
    assert!(
        !reg.midbuild_retried.contains(&issue),
        "a refusal must NOT consume the single recovery retry when {why}"
    );
    assert!(
        reg.midbuild_inuse.contains(&issue),
        "the refusal is recorded (and logged once) when {why}"
    );
}

// --- watchdog_once: bounded auto-restart end-to-end ---

/// A hung-child fixture: emits the account-selection line quickly (so
/// token-name capture returns fast) then sleeps, producing NO progress
/// (no worktree/checkpoint, log stuck at the spawn header).
pub(crate) fn hung_child_registry(ws: &Path) -> SweepRegistry {
    let body = "#!/usr/bin/env bash\n\
                    echo \"spawn-claude: using OAuth account 'faketok' (mode=random)\"\n\
                    sleep 30\n";
    lifecycle_registry(ws, body)
}

/// Backdate a running entry's `started_at` so the watchdog sees it as past
/// the no-progress timeout.
pub(crate) fn backdate(reg: &mut SweepRegistry, sweep_id: &str, secs: i64) {
    if let Some(info) = reg.entries.get_mut(sweep_id) {
        info.started_at = Utc::now() - chrono::Duration::seconds(secs);
    }
}

pub(crate) fn running_issue_sweep_id(reg: &SweepRegistry, issue: u32) -> Option<String> {
    reg.entries
        .values()
        .find(|i| {
            matches!(i.state, SweepState::Running | SweepState::Pending)
                && matches!(i.kind, SweepKind::Issue(n) if n == issue)
        })
        .map(|i| i.sweep_id.clone())
}

// --- closed-issue dispatch guard (Issue #4088, AC6; widened by #4504) ---

/// Render the post-`--jq` payload of the #4504 issue-state probe
/// (`gh api repos/{owner}/{repo}/issues/{N} --jq '{state, is_pr: …}'`).
/// `is_pr` is REST's structural PR discriminator — present for a pull request
/// number in ANY state (open, closed, or merged) and absent for an issue.
pub(crate) fn state_probe_json(state: &str, is_pr: bool) -> String {
    format!("{{\"state\":\"{state}\",\"is_pr\":{is_pr}}}")
}

/// Install a fake `gh` that answers the #4504 issue-state probe
/// (`api repos/<owner>/<repo>/issues/<n>`) with a fixed payload and records
/// every invocation, returning `(registry, gh_log)`. `repo view` resolves the
/// owner/repo (the probe rides `gh api`, which cannot infer it from the
/// working directory) so the fixture works with or without `LOOM_REPO` set.
/// `spawn-claude.sh` is a benign echo-and-exit so a dispatch that passes the
/// guard still spawns.
pub(crate) fn closed_guard_registry(
    ws: &Path,
    probe_stdout: &str,
    probe_exit: i32,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit {exit}\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        state = probe_stdout,
        exit = probe_exit,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    // This helper runs with `skip_label_flip = false`, so it also
    // exercises the #4027 workspace-commands guard — install the marker
    // so a dispatch that clears the closed-issue guard reaches spawn.
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real guard + flip path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- open-PR dispatch guard (Issue #4123) ---

/// Install a fake `gh` for the open-PR guard: the #4504 issue-state probe
/// (`api repos/<owner>/<repo>/issues/<n>`) always reports an **open,
/// non-PR** node (so the 2.5 closed-issue guard passes and the 2.6 open-PR
/// guard is reached), `api graphql` answers the closes-graph probe with
/// `graphql_prs` (whitespace-separated open PR numbers, empty for none — see
/// [`fake_gh_graphql_arm`]) and exits `graphql_exit`, and `repo view`
/// resolves the owner/repo. Every invocation is logged. `spawn-claude.sh` is
/// a benign echo-and-exit so a dispatch that passes the guard still spawns.
pub(crate) fn open_pr_guard_registry(
    ws: &Path,
    graphql_prs: &str,
    graphql_exit: i32,
    skip_label_flip: bool,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        state = state_probe_json("open", false),
        gql = fake_gh_graphql_arm(graphql_prs, graphql_exit),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = skip_label_flip;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- open-PR dispatch guard: #5911 REST fallback ---

/// Like [`open_pr_guard_registry`], but `api graphql` ALWAYS exits non-zero
/// (simulating GraphQL quota exhaustion, the documented recurring failure
/// mode behind #5911) and the `issues/<n>/timeline` REST endpoint answers
/// instead: `timeline_pr` is a single bare PR number (an open, non-closing OR
/// closing cross-referenced PR was found) or empty (verified none), exiting
/// `timeline_exit`. Used to prove the #4123 dispatch guard now recovers from
/// a GraphQL-only outage instead of silently falling open.
pub(crate) fn open_pr_guard_rest_fallback_registry(
    ws: &Path,
    timeline_pr: &str,
    timeline_exit: i32,
    skip_label_flip: bool,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             {timeline}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
             printf 'gh: rate limit exceeded\\n' >&2\n\
             exit 1\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        timeline = fake_gh_timeline_rest_arm(timeline_pr, timeline_exit),
        state = state_probe_json("open", false),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = skip_label_flip;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- open-PR dispatch guard: #6058 bounded whole-probe retry ---

/// Simulates the #6058 production shape: BOTH transports (`api graphql` and
/// its `issues/<n>/timeline` REST fallback) fail on the probe's first
/// attempt — a transient `gh` blip, e.g. the intermittent TLS
/// certificate-verification errors observed bursting across otherwise
/// unrelated invocations in the same daemon tick — then the underlying
/// transport recovers, so a SECOND attempt's `api graphql` call succeeds and
/// reports `open_pr` as an open linked PR. Tracks invocation count via a
/// counter file on disk (persists across the fake `gh` process's own
/// short-lived subprocess lifetime, unlike an in-memory counter) so the first
/// `fail_calls` invocations of either transport fail and every call after
/// that succeeds. Every invocation is also appended to `gh_log` so a test can
/// assert exactly how many `gh` calls the retry made.
pub(crate) fn open_pr_guard_transient_failure_registry(
    ws: &Path,
    open_pr: u32,
    fail_calls: u32,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let counter = ws.join("gh-call-count");
    let fake_gh = ws.join("fake-gh-transient.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
             n=$(( $(cat \"{counter}\" 2>/dev/null || echo 0) + 1 ))\n\
             printf '%s' \"$n\" > \"{counter}\"\n\
             if [[ \"$n\" -le {fail_calls} ]]; then\n\
             printf 'gh: transient tls error\\n' >&2\n\
             exit 1\n\
             fi\n\
             printf '{{\"data\":{{\"repository\":{{\"issue\":{{\"closedByPullRequestsReferences\":\
{{\"nodes\":[{{\"number\":{open_pr},\"state\":\"OPEN\"}}]}}}}}}}}}}\\n'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$*\" == *timeline* ]]; then\n\
             n=$(( $(cat \"{counter}\" 2>/dev/null || echo 0) + 1 ))\n\
             printf '%s' \"$n\" > \"{counter}\"\n\
             printf 'gh: transient tls error\\n' >&2\n\
             exit 1\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        counter = counter.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- park-label dispatch guard (Issue #4444) ---

/// Install a fake `gh` for the park-label guard (step 2.7):
///
/// - `issue view --json labels` reports whether `loom:blocked` is present (so
///   the reap path's `issue_has_blocked_label` — still used by
///   `restore_label_to_ready` — behaves faithfully);
/// - `api graphql` prints `graphql_pr` (the post-`--jq` open linked PR
///   number, empty for none) so the 2.6 open-PR guard can be steered;
/// - `api repos/<owner>/<repo>/issues/<n> --jq '{state, is_pr: …}'` reports an
///   open, non-PR node so the 2.5 closed-issue guard (#4088/#4504) passes —
///   it now rides the same REST endpoint as the park probe, discriminated by
///   the `--jq` expression;
/// - `api repos/<owner>/<repo>/issues/<n> --jq .labels[].name` prints
///   `rest_labels` (whitespace-separated label names, one per output line) and
///   exits `rest_exit` — the REST probe the park guard consults;
/// - `repo view` resolves the owner/repo.
///
/// Every invocation is logged so a test can assert which probes ran.
pub(crate) fn park_guard_registry(
    ws: &Path,
    rest_labels: &str,
    rest_exit: i32,
    graphql_pr: &str,
    skip_label_flip: bool,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let blocked = rest_labels.split_whitespace().any(|l| l == "loom:blocked");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"view\" ]]; then\n\
             printf '%s\\n' \"{blocked}\"\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* && \"$*\" == *is_pr* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' {labels}\n\
             exit {rest_exit}\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        blocked = blocked,
        gql = fake_gh_graphql_arm(graphql_pr, 0),
        state = state_probe_json("open", false),
        labels = rest_labels,
        rest_exit = rest_exit,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = skip_label_flip;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

// --- forge-label collision enforcement at full dispatch() (Issue #5789) ---

/// Install a fake `gh` that clears every pre-flip guard 2.4-2.7 (open, non-PR
/// issue; no open linked PR; no park label) and answers `issue view --json
/// labels` — the [`super::guards::SweepRegistry::classify_preflip_labels`]
/// probe 4a rides — with `preflip_labels`. `preflip_labels` should be a full
/// `{"labels":[...]}` JSON payload so a caller can steer the collision
/// classification (`loom:building` present, or `loom:issue` absent, is a
/// collision; `loom:issue` present and `loom:building` absent is clean).
/// `spawn-claude.sh` records to `spawn_log` so a test can assert whether the
/// child was ever launched. Every `gh` invocation is logged to `gh_log` so a
/// test can assert whether `issue edit` (the label flip) was reached.
pub(crate) fn collision_dispatch_registry(
    ws: &Path,
    preflip_labels: &str,
) -> (SweepRegistry, PathBuf, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"view\" ]]; then\n\
             printf '%s\\n' '{preflip_labels}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" ]]; then\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* && \"$*\" == *is_pr* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '\\n'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        preflip_labels = preflip_labels.replace('\'', "'\\''"),
        gql = fake_gh_graphql_arm("", 0),
        state = state_probe_json("open", false),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    let spawn_log = ws.join("spawn-invocations.log");
    std::fs::write(
        &spawn,
        format!(
            "#!/usr/bin/env bash\nprintf 'spawned\\n' >> \"{}\"\nexit 0\n",
            spawn_log.display()
        ),
    )
    .unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real 4a guard + flip path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log, spawn_log)
}

// --- reaper-driven resume (Issue #4256) ---

/// Write a `.loom/locks/issue-<N>/owner.json` for `issue` claimed by
/// `sweep_id` with `owner_pid` (Issue #4463 test fixture). Returns the lock
/// dir path so callers can assert on its survival.
pub(crate) fn write_lock_owner(
    reg: &SweepRegistry,
    issue: u32,
    sweep_id: &str,
    owner_pid: u32,
) -> PathBuf {
    let locks = reg.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join(format!("issue-{issue}"));
    std::fs::create_dir_all(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        issue,
        owner_pid,
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: sweep_id.to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();
    lock
}

/// Write a `sweep-checkpoint.sh`-style checkpoint file for `issue` with
/// `phase`, matching the on-disk shape `read_checkpoint_phase` parses.
pub(crate) fn write_checkpoint(reg: &SweepRegistry, issue: u32, phase: &str) {
    let dir = reg.config.checkpoint_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("issue-{issue}.json")),
        format!(r#"{{"phase":"{phase}","issue":{issue}}}"#),
    )
    .unwrap();
}

/// Insert a `Running` entry with a guaranteed-dead PID (Issue #4256 test
/// fixture), mirroring `reaper_emits_crashed_event_with_checkpoint_phase`'s
/// pattern: no retained `Child` handle, so `poll_liveness` falls back to
/// the `kill(pid, 0)` probe, which reports dead for this bogus PID.
pub(crate) fn insert_dead_running_entry(reg: &mut SweepRegistry, issue: u32, sweep_id: &str) {
    reg.entries.insert(
        sweep_id.to_string(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.to_string(),
            kind: SweepKind::Issue(issue),
            pid: 2_147_483_641,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: reg.compute_log_path(issue),
            idempotency_key: None,
            started_at: Utc::now() - chrono::Duration::seconds(5),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
}

// --- config resolution: env > config > default ---

pub(crate) fn write_cfg(dir: &Path, body: &str) {
    let loom = dir.join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    std::fs::write(loom.join("config.json"), body).unwrap();
}

// ===================================================================
// config_resolver migration (#4058) — tier precedence
// ===================================================================

pub(crate) fn write_project_cfg(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

pub(crate) fn write_local_cfg(dir: &Path, body: &str) {
    let full = dir.join(crate::config_resolver::LOCAL_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}
