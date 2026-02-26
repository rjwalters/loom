"""Main daemon event loop."""

from __future__ import annotations

import datetime
import os
import shutil
import signal
import subprocess
import sys
import time
from typing import Any

from loom_tools.common.github import gh_issue_view
from loom_tools.common.issue_failures import load_failure_log, merge_into_daemon_state
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.daemon_v2.command_poller import CommandPoller
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.exit_codes import DaemonExitCode
from loom_tools.daemon_v2.iteration import run_iteration
from loom_tools.daemon_v2.signals import (
    check_existing_pid,
    check_session_conflict,
    check_stop_signal,
    cleanup_on_exit,
    clear_stop_signal,
    write_pid_file,
)
from loom_tools.daemon_cleanup import handle_daemon_shutdown, handle_daemon_startup, load_config
from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds


def run(ctx: DaemonContext) -> int:
    """Run the daemon main loop.

    Returns an exit code from DaemonExitCode.
    """
    # 1. Check for existing daemon instance
    is_running, existing_pid = check_existing_pid(ctx)
    if is_running:
        log_error(f"Daemon loop already running (PID: {existing_pid})")
        log_info("Use --status to check status or stop the existing daemon first")
        return DaemonExitCode.SESSION_CONFLICT

    # 2. Run pre-flight checks
    preflight_errors = _run_preflight_checks(ctx)
    if preflight_errors:
        for err in preflight_errors:
            log_error(err)
        return DaemonExitCode.STARTUP_FAILED

    # 3. Write PID file
    write_pid_file(ctx)

    # 4. Setup signal handlers
    def signal_handler(signum: int, frame: Any) -> None:
        ctx.running = False

    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    try:
        # 5. Rotate existing state file
        _rotate_state_file(ctx)

        # 6. Initialize state and metrics files
        _init_state_file(ctx)
        _init_metrics_file(ctx)

        # 7. Clear any existing stop signal
        clear_stop_signal(ctx)

        # 8. Print startup header
        _print_header(ctx)

        # 9. Run startup cleanup
        log_info("Running startup cleanup...")
        cleanup_config = load_config()
        handle_daemon_startup(ctx.repo_root, cleanup_config)

        # 10. Initialize CommandPoller for signal-based IPC with /loom skill
        command_poller = CommandPoller(ctx.repo_root)
        log_info(f"CommandPoller initialized: signals_dir={ctx.signals_dir}")

        # 11. Compute deadline for timeout
        deadline: float | None = None
        if ctx.config.timeout_min > 0:
            deadline = time.time() + ctx.config.timeout_min * 60

        # 12. Main loop
        while ctx.running:
            ctx.iteration += 1

            # Check for stop signal
            if check_stop_signal(ctx):
                log_info(f"Iteration {ctx.iteration}: SHUTDOWN_SIGNAL detected")
                break

            # Check for timeout
            if deadline is not None and time.time() >= deadline:
                log_info(
                    f"Iteration {ctx.iteration}: TIMEOUT reached "
                    f"({ctx.config.timeout_min} minutes elapsed)"
                )
                break

            # Check for session conflict
            if check_session_conflict(ctx):
                log_warning("Yielding to other daemon instance. Exiting.")
                break

            # Poll and process IPC commands from /loom skill BEFORE iteration.
            # The /loom skill writes spawn_shepherd/stop/etc. signals here.
            commands = command_poller.poll()
            if commands:
                log_info(f"Iteration {ctx.iteration}: Processing {len(commands)} signal command(s)")
                _process_commands(ctx, commands, command_poller)
                # Update signal_queue_depth in state file after processing
                _update_signal_queue_depth(ctx, command_poller.queue_depth())

            # Retry any pending spawn signals that were queued when all slots were full.
            # Runs before each iteration so reclaimed slots are immediately reused.
            if ctx.pending_spawns:
                _retry_pending_spawns(ctx)

            # Run iteration (only when orchestration is active — activated by /loom)
            if ctx.orchestration_active:
                log_info(f"Iteration {ctx.iteration}: Starting...")
                start_time = time.time()
                result = run_iteration(ctx)
                duration = int(time.time() - start_time)

                # Log result
                log_info(f"Iteration {ctx.iteration}: {result.summary} ({duration}s)")

                # Update metrics
                _update_metrics(ctx, result.status, duration, result.summary)

                # Check for shutdown in result
                if result.status == "shutdown" or "SHUTDOWN" in result.summary:
                    break
            else:
                log_info(
                    f"Iteration {ctx.iteration}: Standby — "
                    "waiting for /loom to activate orchestration"
                )

            # Check stop signal again before sleeping
            if check_stop_signal(ctx):
                log_info("SHUTDOWN_SIGNAL detected after iteration")
                break

            # Responsive sleep: wake every 2s to process IPC commands and check
            # for stop signals. This keeps the daemon responsive to /loom signals
            # without requiring the full poll_interval to elapse.
            log_info(f"Sleeping {ctx.config.poll_interval}s until next iteration...")
            _responsive_sleep(ctx, command_poller, ctx.config.poll_interval)

        # 11. Run shutdown cleanup
        log_info("Running shutdown cleanup...")
        handle_daemon_shutdown(ctx.repo_root, cleanup_config)

        log_success("Daemon loop completed gracefully")
        return DaemonExitCode.SUCCESS

    except Exception as e:
        log_error(f"Daemon error: {e}")
        return DaemonExitCode.ERROR

    finally:
        cleanup_on_exit(ctx)


def _responsive_sleep(
    ctx: DaemonContext,
    command_poller: CommandPoller,
    total_seconds: int,
    tick: int = 2,
) -> None:
    """Sleep for total_seconds but wake every tick seconds to process IPC.

    During each tick we:
    - Check for stop signal (exit early if found)
    - Poll CommandPoller for new signal commands
    - Run fast-path shepherd assignment when idle slots + cached ready issues exist
    - Keep the daemon responsive without busy-waiting
    """
    elapsed = 0
    while elapsed < total_seconds and ctx.running:
        time.sleep(min(tick, total_seconds - elapsed))
        elapsed += tick

        if check_stop_signal(ctx):
            log_info("SHUTDOWN_SIGNAL detected during sleep")
            ctx.running = False
            break

        # Process any IPC commands that arrived during the sleep tick
        commands = command_poller.poll()
        if commands:
            log_info(f"Sleep-tick: processing {len(commands)} signal command(s)")
            _process_commands(ctx, commands, command_poller)
            _update_signal_queue_depth(ctx, command_poller.queue_depth())

        # Retry pending spawns that were queued when all slots were full.
        # Doing this during sleep ticks means a freed slot triggers a spawn
        # within 2s rather than waiting for the next full iteration.
        if ctx.pending_spawns:
            _retry_pending_spawns(ctx)

        # Fast-path assignment: if idle shepherd slots exist and the cached
        # snapshot shows ready issues, spawn immediately without waiting for
        # the next full iteration. This eliminates poll_interval latency when
        # a shepherd completes and ready issues are already queued.
        _fast_path_assign(ctx)


def _process_commands(
    ctx: DaemonContext,
    commands: list[dict],
    command_poller: CommandPoller | None = None,
) -> None:
    """Process IPC command signals from the /loom Claude Code skill.

    Commands are JSON dicts consumed from .loom/signals/ by CommandPoller.
    Each command has an "action" field that determines handling.

    Supported actions:
    - spawn_shepherd: Request daemon to spawn a shepherd for an issue.
      The daemon's normal iteration loop also spawns shepherds autonomously;
      this allows the /loom skill to trigger spawning explicitly.
    - stop: Request graceful daemon shutdown (same effect as stop-daemon file).
    - pause_shepherd: Pause a specific shepherd by shepherd_id.
    - resume_shepherd: Resume a paused shepherd by shepherd_id.
    - set_max_shepherds: Adjust the maximum concurrent shepherd count.
    """
    for cmd in commands:
        action = cmd.get("action", "")

        if action == "spawn_shepherd":
            issue = cmd.get("issue")
            mode = cmd.get("mode", "default")
            flags = cmd.get("flags", [])
            if issue is None:
                log_warning("spawn_shepherd command missing 'issue' field, skipping")
                continue
            log_info(f"Signal: spawn_shepherd issue=#{issue} mode={mode} flags={flags}")
            _spawn_shepherd_from_signal(ctx, issue, mode, flags, command_poller)

        elif action == "stop":
            log_info("Signal: stop — initiating graceful daemon shutdown")
            ctx.running = False

        elif action == "pause_shepherd":
            shepherd_id = cmd.get("shepherd_id")
            if shepherd_id:
                log_info(f"Signal: pause_shepherd shepherd_id={shepherd_id}")
                _pause_shepherd(ctx, shepherd_id)
            else:
                log_warning("pause_shepherd command missing 'shepherd_id', skipping")

        elif action == "resume_shepherd":
            shepherd_id = cmd.get("shepherd_id")
            if shepherd_id:
                log_info(f"Signal: resume_shepherd shepherd_id={shepherd_id}")
                _resume_shepherd(ctx, shepherd_id)
            else:
                log_warning("resume_shepherd command missing 'shepherd_id', skipping")

        elif action == "set_max_shepherds":
            count = cmd.get("count")
            if isinstance(count, int) and count > 0:
                log_info(f"Signal: set_max_shepherds count={count}")
                ctx.config.max_shepherds = count
            else:
                log_warning(f"set_max_shepherds invalid count={count!r}, skipping")

        elif action == "start_orchestration":
            mode = cmd.get("mode", "default")
            if mode == "force":
                ctx.config.force_mode = True
            ctx.orchestration_active = True
            log_success(
                f"Signal: start_orchestration mode={mode} — "
                "orchestration activated"
            )
            _update_orchestration_active(ctx)

        else:
            log_warning(f"Signal: unknown action={action!r}, skipping")


def _spawn_shepherd_from_signal(
    ctx: DaemonContext,
    issue: int,
    mode: str,
    flags: list[str],
    command_poller: CommandPoller | None = None,
) -> None:
    """Spawn a shepherd for an issue in response to a spawn_shepherd signal.

    Spawns loom-shepherd.sh as a direct subprocess of the daemon process.
    This makes the shepherd (and its worker claude sessions) children of the
    daemon rather than descendants of any Claude Code session.

    The daemon is the natural parent:
        init/launchd → loom-daemon → loom-shepherd.sh → claude /builder

    State tracking: on success, writes a ShepherdEntry with execution_mode
    "subprocess" and the process PID to daemon-state.json.
    """
    import hashlib

    from loom_tools.models.daemon_state import ShepherdEntry

    # Check slot availability FIRST so we can queue for retry even when the
    # shepherd script is missing (it will be present when the slot eventually
    # opens in a healthy deployment, and the error will surface then).
    if ctx.state is None:
        # Daemon state not yet loaded (e.g. first iteration hasn't run).
        # Re-queue the signal so it is retried on the next sleep-tick poll
        # rather than being silently dropped.
        cmd = {"action": "spawn_shepherd", "issue": issue, "mode": mode, "flags": flags}
        if command_poller is not None and command_poller.requeue(cmd):
            log_warning(
                f"Signal spawn: no daemon state loaded, re-queued spawn_shepherd "
                f"for issue #{issue}"
            )
        else:
            log_warning(
                f"Signal spawn: no daemon state loaded and re-queue failed, "
                f"spawn_shepherd for issue #{issue} dropped"
            )
        return

    # Reject signal early if the target issue is closed — retrying won't help.
    issue_data = gh_issue_view(issue, ["state"], cwd=ctx.repo_root)
    if issue_data is None:
        log_warning(
            f"Signal spawn: issue #{issue} not found — cannot spawn shepherd"
        )
        return
    if issue_data.get("state", "").upper() != "OPEN":
        log_warning(
            f"Signal spawn: issue #{issue} is closed — reopen it to shepherd"
        )
        return

    shepherd_name = _find_idle_shepherd_slot(ctx)
    if shepherd_name is None:
        # No slot available — enqueue for retry on the next iteration or
        # sleep-tick rather than silently dropping the signal.
        pending = {"issue": issue, "mode": mode, "flags": flags}
        if pending not in ctx.pending_spawns:
            ctx.pending_spawns.append(pending)
            log_warning(
                f"Signal spawn: no idle shepherd slot available for issue #{issue} "
                f"— queued for retry (pending={len(ctx.pending_spawns)})"
            )
        return

    shepherd_script = ctx.repo_root / ".loom" / "scripts" / "loom-shepherd.sh"
    if not shepherd_script.exists():
        log_error(f"Signal spawn: loom-shepherd.sh not found at {shepherd_script}")
        return

    # Log file for this subprocess shepherd
    log_dir = ctx.repo_root / ".loom" / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    task_id = hashlib.sha256(
        f"{shepherd_name}-{issue}-{time.time()}".encode()
    ).hexdigest()[:7]
    log_file_path = log_dir / f"loom-shepherd-signal-issue-{issue}-{task_id}.log"

    # Build command arguments (task_id must be computed first)
    args = [str(issue)]
    if mode == "force":
        args.append("--merge")
    args.extend(["--task-id", task_id])
    args.extend(flags)

    try:
        with open(log_file_path, "w") as log_file:
            proc = subprocess.Popen(
                [str(shepherd_script)] + args,
                cwd=ctx.repo_root,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                # No stdin — detached from any terminal
                stdin=subprocess.DEVNULL,
            )

        entry = ShepherdEntry(
            status="working",
            issue=issue,
            task_id=task_id,
            output_file=str(log_file_path),
            started=timestamp,
            last_phase="started",
            execution_mode="subprocess",
            pid=proc.pid,
        )
        ctx.state.shepherds[shepherd_name] = entry

        # Persist state update
        _write_state(ctx)
        log_success(
            f"Signal spawn: {shepherd_name} (PID {proc.pid}) started for issue #{issue}"
        )

    except OSError as exc:
        log_error(f"Signal spawn: failed to start shepherd for issue #{issue}: {exc}")


def _find_idle_shepherd_slot(ctx: DaemonContext) -> str | None:
    """Find an idle shepherd slot name, or allocate a new one if under capacity."""
    from loom_tools.models.daemon_state import ShepherdEntry

    if ctx.state is None:
        return None

    for name, entry in ctx.state.shepherds.items():
        if entry.status == "idle":
            return name

    current_count = len(ctx.state.shepherds)
    if current_count < ctx.config.max_shepherds:
        new_name = f"shepherd-{current_count + 1}"
        ctx.state.shepherds[new_name] = ShepherdEntry(status="idle")
        return new_name

    return None


def _retry_pending_spawns(ctx: DaemonContext) -> None:
    """Attempt to spawn shepherds from the pending queue.

    Called at the start of each iteration and during responsive-sleep ticks.
    Drains ``ctx.pending_spawns`` by attempting to fulfil each queued spawn
    signal.  Entries that still cannot be fulfilled (slots still full) remain
    in the queue for the next retry.

    Algorithm:
    1. Snapshot and clear the pending list (so _spawn_shepherd_from_signal
       can re-queue items to ctx.pending_spawns without interfering).
    2. For each snapshot item, attempt the spawn.
       - Success: item is consumed.
       - Failure (slot gone again): _spawn_shepherd_from_signal re-queues
         the item into ctx.pending_spawns automatically.
    3. Any items added to ctx.pending_spawns by step 2 are already there;
       no further merging is needed.
    """
    if not ctx.pending_spawns:
        return

    # Take a snapshot and clear, so re-queuing by _spawn_shepherd_from_signal
    # goes into a clean list.
    to_retry = ctx.pending_spawns[:]
    ctx.pending_spawns = []

    log_info(f"Retrying {len(to_retry)} pending spawn(s)...")
    for pending in to_retry:
        issue = pending["issue"]
        mode = pending["mode"]
        flags = pending["flags"]
        _spawn_shepherd_from_signal(ctx, issue, mode, flags)

    if ctx.pending_spawns:
        log_info(
            f"Pending spawn queue: {len(ctx.pending_spawns)} spawn(s) still "
            f"waiting for an idle shepherd slot"
        )


def _fast_path_assign(ctx: DaemonContext) -> None:
    """Fast-path shepherd assignment during sleep ticks.

    When a shepherd completes its issue, the slot becomes idle but the next
    full iteration (with snapshot rebuild) won't run until poll_interval
    elapses. This function checks whether idle slots exist AND the cached
    snapshot still shows ready issues, and if so spawns immediately.

    Uses the cached snapshot from the most recent full iteration — no GitHub
    API call is needed. The snapshot may be slightly stale (up to poll_interval
    seconds old) but this is acceptable: if a ready issue was present at last
    snapshot time and an idle slot is now available, spawning is safe.

    Called during every sleep tick so a completed shepherd is reassigned within
    tick seconds (default 2s) rather than waiting up to poll_interval seconds.
    """
    if ctx.state is None or ctx.snapshot is None:
        return

    # Check for idle shepherd slots directly from state (no API call needed)
    has_idle_slot = any(e.status == "idle" for e in ctx.state.shepherds.values())
    if not has_idle_slot:
        # Also check if we have room to create a new shepherd slot
        has_idle_slot = len(ctx.state.shepherds) < ctx.config.max_shepherds

    if not has_idle_slot:
        return

    # Check cached snapshot for ready issues (no API call needed)
    ready_issues = ctx.get_ready_issues()
    if not ready_issues:
        return

    # Idle slot + ready issues in cached snapshot: spawn immediately
    spawned = spawn_shepherds(ctx)
    if spawned > 0:
        log_success(
            f"Fast-path assignment: spawned {spawned} shepherd(s) "
            f"without waiting for next full iteration"
        )
        _write_state(ctx)


def _pause_shepherd(ctx: DaemonContext, shepherd_id: str) -> None:
    """Pause a shepherd by writing a stop file to its worktree."""
    if ctx.state is None:
        return
    entry = ctx.state.shepherds.get(shepherd_id)
    if entry is None:
        log_warning(f"pause_shepherd: no shepherd named {shepherd_id!r}")
        return
    if entry.status != "working":
        log_warning(f"pause_shepherd: {shepherd_id} is not working (status={entry.status!r})")
        return

    # Write a stop-shepherd signal file for the shepherd's worktree
    if entry.issue is not None:
        stop_file = (
            ctx.repo_root / ".loom" / "worktrees" / f"issue-{entry.issue}" / ".stop-shepherd"
        )
        try:
            stop_file.parent.mkdir(parents=True, exist_ok=True)
            stop_file.touch()
            entry.status = "paused"
            log_info(f"pause_shepherd: wrote {stop_file}")
        except OSError as exc:
            log_warning(f"pause_shepherd: could not write stop file: {exc}")
    else:
        log_warning(f"pause_shepherd: {shepherd_id} has no issue assigned")


def _resume_shepherd(ctx: DaemonContext, shepherd_id: str) -> None:
    """Resume a paused shepherd by removing its stop file."""
    if ctx.state is None:
        return
    entry = ctx.state.shepherds.get(shepherd_id)
    if entry is None:
        log_warning(f"resume_shepherd: no shepherd named {shepherd_id!r}")
        return
    if entry.status != "paused":
        log_warning(f"resume_shepherd: {shepherd_id} is not paused (status={entry.status!r})")
        return

    if entry.issue is not None:
        stop_file = (
            ctx.repo_root / ".loom" / "worktrees" / f"issue-{entry.issue}" / ".stop-shepherd"
        )
        try:
            stop_file.unlink(missing_ok=True)
            entry.status = "working"
            log_info(f"resume_shepherd: removed {stop_file}")
        except OSError as exc:
            log_warning(f"resume_shepherd: could not remove stop file: {exc}")
    else:
        log_warning(f"resume_shepherd: {shepherd_id} has no issue assigned")


def _write_state(ctx: DaemonContext) -> None:
    """Write current daemon state to disk (best-effort)."""
    if ctx.state is None:
        return
    try:
        data = read_json_file(ctx.state_file) if ctx.state_file.exists() else {}
        if not isinstance(data, dict):
            data = {}
        data["shepherds"] = {k: v.to_dict() for k, v in ctx.state.shepherds.items()}
        data["orchestration_active"] = ctx.orchestration_active
        write_json_file(ctx.state_file, data)
    except Exception as exc:
        log_warning(f"_write_state: failed to persist state: {exc}")


def _update_orchestration_active(ctx: DaemonContext) -> None:
    """Persist orchestration_active flag to daemon-state.json (best-effort)."""
    try:
        data = read_json_file(ctx.state_file) if ctx.state_file.exists() else {}
        if not isinstance(data, dict):
            data = {}
        data["orchestration_active"] = ctx.orchestration_active
        data["force_mode"] = ctx.config.force_mode
        write_json_file(ctx.state_file, data)
    except Exception as exc:
        log_warning(f"_update_orchestration_active: {exc}")


def _update_signal_queue_depth(ctx: DaemonContext, depth: int) -> None:
    """Update signal_queue_depth in daemon-state.json."""
    try:
        data = read_json_file(ctx.state_file) if ctx.state_file.exists() else {}
        if not isinstance(data, dict):
            return
        data["signal_queue_depth"] = depth
        write_json_file(ctx.state_file, data)
    except Exception as exc:
        log_warning(f"_update_signal_queue_depth: {exc}")


def _run_preflight_checks(ctx: DaemonContext) -> list[str]:
    """Run pre-flight dependency checks.

    Returns a list of error messages. Empty list means all checks passed.
    """
    failures: list[str] = []

    # Check 1: claude CLI available
    if not shutil.which("claude"):
        failures.append("Error: 'claude' CLI not found in PATH")
        failures.append("Install Claude Code CLI: https://claude.ai/code")

    # Check 2: loom_tools module importable
    try:
        result = subprocess.run(
            [sys.executable, "-c", "import loom_tools"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            failures.append("Error: 'loom_tools' Python module not importable")
            failures.append(f"Run: pip install -e {ctx.repo_root / 'loom-tools'}")
    except (subprocess.TimeoutExpired, FileNotFoundError):
        failures.append("Error: Failed to verify loom_tools module")

    # Check 3: gh CLI available and authenticated
    if not shutil.which("gh"):
        failures.append("Error: 'gh' CLI not found in PATH")
        failures.append("Install GitHub CLI: https://cli.github.com/")
    else:
        try:
            result = subprocess.run(
                ["gh", "auth", "status"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                failures.append("Error: 'gh' CLI not authenticated")
                failures.append("Run: gh auth login")
        except (subprocess.TimeoutExpired, FileNotFoundError):
            failures.append("Warning: Could not verify gh authentication")

    # Check 4: tmux available
    if not shutil.which("tmux"):
        failures.append("Error: 'tmux' not found in PATH")
        failures.append("Install tmux: brew install tmux (macOS)")

    return failures


def _rotate_state_file(ctx: DaemonContext) -> None:
    """Rotate existing state file if present."""
    if not ctx.state_file.exists():
        return

    log_info("Rotating previous daemon state...")

    # Try shell script first
    rotate_script = ctx.repo_root / ".loom" / "scripts" / "rotate-daemon-state.sh"
    if rotate_script.exists():
        try:
            result = subprocess.run(
                [str(rotate_script)],
                capture_output=True,
                timeout=30,
                cwd=ctx.repo_root,
            )
            if result.returncode == 0:
                log_info("State rotation complete (shell)")
                return
        except (subprocess.TimeoutExpired, Exception):
            pass

    # Fallback to Python-native rotation
    _rotate_state_python(ctx)


def _rotate_state_python(ctx: DaemonContext) -> None:
    """Python-native state rotation fallback."""
    loom_dir = ctx.repo_root / ".loom"
    max_archived = int(os.environ.get("LOOM_MAX_ARCHIVED_SESSIONS", "10"))

    try:
        data = read_json_file(ctx.state_file)
    except Exception:
        log_warning("State file unreadable, skipping rotation")
        return

    if not isinstance(data, dict):
        return

    # Skip if file has no useful data
    file_size = ctx.state_file.stat().st_size
    if file_size < 50:
        log_info(f"State file too small ({file_size} bytes), skipping rotation")
        return

    iteration = data.get("iteration", 0)
    has_shepherds = any(
        isinstance(v, dict) and v.get("issue") is not None
        for v in data.get("shepherds", {}).values()
    )
    has_completed = len(data.get("completed_issues", []))

    if iteration == 0 and not has_shepherds and has_completed == 0:
        log_info("State file has no useful data, skipping rotation")
        return

    # Find next session number
    session_num = 0
    while (loom_dir / f"{session_num:02d}-daemon-state.json").exists():
        session_num += 1
        if session_num >= 100:
            session_num = 0
            break

    # Prune old sessions
    archives = sorted(loom_dir.glob("[0-9][0-9]-daemon-state.json"))
    to_delete = len(archives) - max_archived + 1
    if to_delete > 0:
        for archive in archives[:to_delete]:
            archive.unlink(missing_ok=True)
            log_info(f"Pruned old archive: {archive.name}")

    # Add session summary before archiving
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    data["session_summary"] = {
        "session_id": session_num,
        "archived_at": timestamp,
        "issues_completed": has_completed,
        "prs_merged": data.get("total_prs_merged", 0),
        "total_iterations": iteration,
    }
    write_json_file(ctx.state_file, data)

    # Rename to archive
    archive_name = f"{session_num:02d}-daemon-state.json"
    archive_path = loom_dir / archive_name
    ctx.state_file.rename(archive_path)
    log_info(f"Archived: daemon-state.json -> {archive_name}")


def _init_state_file(ctx: DaemonContext) -> None:
    """Initialize or update the daemon state file."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Compute timeout_at if configured
    timeout_at: str | None = None
    if ctx.config.timeout_min > 0:
        deadline_dt = now_utc() + datetime.timedelta(minutes=ctx.config.timeout_min)
        timeout_at = deadline_dt.strftime("%Y-%m-%dT%H:%M:%SZ")

    if ctx.state_file.exists():
        try:
            data = read_json_file(ctx.state_file)
            if isinstance(data, dict):
                data["force_mode"] = ctx.config.force_mode
                data["orchestration_active"] = False
                data["started_at"] = timestamp
                data["running"] = True
                data["iteration"] = 0
                data["daemon_session_id"] = ctx.session_id
                data["daemon_pid"] = os.getpid()
                data["signal_queue_depth"] = 0
                data["execution_mode"] = "direct"
                data["timeout_at"] = timeout_at
                # Merge persistent failure history into existing state
                _merge_persistent_failures(ctx, data)
                write_json_file(ctx.state_file, data)
                return
        except Exception:
            pass

    # Create fresh state file
    data = {
        "started_at": timestamp,
        "last_poll": None,
        "running": True,
        "iteration": 0,
        "orchestration_active": False,
        "force_mode": ctx.config.force_mode,
        "execution_mode": "direct",
        "daemon_session_id": ctx.session_id,
        "daemon_pid": os.getpid(),
        "signal_queue_depth": 0,
        "timeout_at": timeout_at,
        "shepherds": {},
        "support_roles": {},
        "completed_issues": [],
        "total_prs_merged": 0,
        "systematic_failure": {
            "active": False,
            "pattern": "",
            "count": 0,
            "probe_count": 0,
        },
        "blocked_issue_retries": {},
        "recent_failures": [],
    }
    ctx.state_file.parent.mkdir(parents=True, exist_ok=True)

    # Merge persistent failure history into fresh state
    _merge_persistent_failures(ctx, data)

    write_json_file(ctx.state_file, data)


def _merge_persistent_failures(ctx: DaemonContext, data: dict) -> None:
    """Merge persistent failure log into daemon state on startup.

    Loads .loom/issue-failures.json and merges failure counts into
    blocked_issue_retries so cross-session failure history is preserved.
    """
    try:
        failure_log = load_failure_log(ctx.repo_root)
        if failure_log.entries:
            retries = data.get("blocked_issue_retries", {})
            data["blocked_issue_retries"] = merge_into_daemon_state(
                ctx.repo_root, retries
            )
            log_info(
                f"Merged {len(failure_log.entries)} persistent failure entries "
                f"into daemon state"
            )
    except Exception as e:
        log_warning(f"Failed to merge persistent failure log: {e}")


def _init_metrics_file(ctx: DaemonContext) -> None:
    """Initialize the metrics file."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    data = {
        "session_start": timestamp,
        "total_iterations": 0,
        "successful_iterations": 0,
        "failed_iterations": 0,
        "timeout_iterations": 0,
        "iteration_durations": [],
        "average_iteration_seconds": 0,
        "last_iteration": None,
        "health": {
            "status": "healthy",
            "consecutive_failures": 0,
            "last_success": None,
        },
    }
    write_json_file(ctx.metrics_file, data)


def _update_metrics(ctx: DaemonContext, status: str, duration: int, summary: str) -> None:
    """Update the metrics file after an iteration."""
    try:
        data = read_json_file(ctx.metrics_file)
        if not isinstance(data, dict):
            data = {}
    except Exception:
        data = {}

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    data["total_iterations"] = data.get("total_iterations", 0) + 1
    data["last_iteration"] = {
        "timestamp": timestamp,
        "duration_seconds": duration,
        "status": status,
        "summary": summary,
    }

    if status == "success":
        data["successful_iterations"] = data.get("successful_iterations", 0) + 1
        health = data.get("health", {})
        health["consecutive_failures"] = 0
        health["last_success"] = timestamp
        health["status"] = "healthy"
        data["health"] = health
    else:
        data["failed_iterations"] = data.get("failed_iterations", 0) + 1
        health = data.get("health", {})
        health["consecutive_failures"] = health.get("consecutive_failures", 0) + 1
        if health["consecutive_failures"] >= 3:
            health["status"] = "unhealthy"
        data["health"] = health

    # Update rolling average (keep last 100 durations)
    durations = data.get("iteration_durations", [])
    durations = (durations + [duration])[-100:]
    data["iteration_durations"] = durations
    if durations:
        data["average_iteration_seconds"] = sum(durations) // len(durations)

    write_json_file(ctx.metrics_file, data)


def _print_header(ctx: DaemonContext) -> None:
    """Print startup header."""
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    log_info("")
    log_info("=" * 67)
    log_info("  LOOM DAEMON - PYTHON IMPLEMENTATION")
    log_info("=" * 67)
    log_info(f"  Started: {timestamp}")
    log_info(f"  PID: {os.getpid()}")
    log_info(f"  Session ID: {ctx.session_id}")
    log_info(f"  Mode: {ctx.config.mode_display()} (standby — run /loom to activate)")
    log_info(f"  Poll interval: {ctx.config.poll_interval}s")
    log_info(f"  Max shepherds: {ctx.config.max_shepherds}")
    if ctx.config.timeout_min > 0:
        deadline_dt = now_utc() + datetime.timedelta(minutes=ctx.config.timeout_min)
        log_info(f"  Timeout: {ctx.config.timeout_min}min (until {deadline_dt.strftime('%H:%M:%S UTC')})")
    log_info(f"  PID file: {ctx.pid_file}")
    log_info(f"  State file: {ctx.state_file}")
    log_info(f"  Stop signal: {ctx.stop_signal}")
    log_info("=" * 67)
    log_info("")
