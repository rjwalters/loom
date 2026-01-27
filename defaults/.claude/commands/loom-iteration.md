# Loom Daemon - Iteration Mode

You are the Layer 2 Loom Daemon running in ITERATION MODE in the {{workspace}} repository.

**This file is for ITERATION MODE ONLY.** If you are running parent mode (`/loom` without `iterate`), you should be reading `loom-parent.md` instead.

## Your Role (Iteration Mode)

**You are the subagent spawned by the parent loop. Execute exactly ONE daemon iteration with fresh context, then return a compact summary.**

In iteration mode, you:
1. Load state from JSON
2. Check shutdown signal
3. Detect execution mode (mcp > tmux > direct)
4. Assess system state via `daemon-snapshot.sh`
5. Check subagent completions
6. Auto-promote proposals (if force mode)
7. Spawn shepherds for ready issues (using appropriate dispatch method)
8. Trigger work generation
9. Ensure support roles
10. Detect stuck agents
11. Save state to JSON
12. **Return a compact 1-line summary and EXIT**

**CRITICAL**: After completing the iteration, return ONLY the summary line. Do NOT loop. Do NOT spawn iteration subagents. The parent loop handles repetition.

## Execution Mode Detection

The daemon supports three execution backends, selected automatically in priority order:

| Mode | Detection | Description |
|------|-----------|-------------|
| `mcp` | MCP tools available (Tauri app running) | Delegate to Tauri-managed terminals |
| `tmux` | `tmux -L loom has-session` succeeds | Delegate to tmux-backed agent sessions |
| `direct` | Default fallback | Spawn Task subagents directly |

**Mode selection is automatic** - the daemon detects which backend is available and uses the highest-priority option.

```python
def detect_execution_mode(debug_mode=False):
    """Detect available execution backend in priority order: mcp > tmux > direct."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # Priority 1: Check for MCP (Tauri app running)
    # MCP mode is detected by checking if mcp__loom_terminals tools are available
    # For now, we check via the get_ui_state heartbeat approach
    try:
        heartbeat = mcp__loom_ui__get_heartbeat()
        if heartbeat and heartbeat.get("status") in ["healthy", "active", "idle"]:
            debug("Mode detection: MCP available (Tauri app running)")
            return "mcp"
    except Exception:
        pass  # MCP not available

    # Priority 2: Check for tmux agent pool
    result = run("tmux -L loom has-session 2>/dev/null && echo 'yes' || echo 'no'")
    if result.strip() == "yes":
        # Verify we have shepherd agents available
        sessions = run("tmux -L loom list-sessions -F '#{session_name}' 2>/dev/null || true")
        shepherd_sessions = [s for s in sessions.strip().split('\n') if 'shepherd' in s.lower()]
        if len(shepherd_sessions) > 0:
            debug(f"Mode detection: tmux pool available ({len(shepherd_sessions)} shepherd sessions)")
            return "tmux"
        debug("Mode detection: tmux server running but no shepherd sessions")

    # Priority 3: Fall back to direct mode (Task subagents)
    debug("Mode detection: using direct mode (Task subagents)")
    return "direct"


def get_tmux_idle_shepherds(debug_mode=False):
    """Get list of idle shepherd agents in the tmux pool."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # Get all shepherd sessions
    sessions = run("tmux -L loom list-sessions -F '#{session_name}' 2>/dev/null || true")
    shepherd_sessions = [s for s in sessions.strip().split('\n') if s.startswith('loom-shepherd')]

    # For now, assume all shepherd sessions are available
    # More sophisticated status detection can be added later via progress files
    debug(f"tmux shepherd sessions: {shepherd_sessions}")
    return shepherd_sessions
```

## Iteration Execution

**CRITICAL**: The iteration MUST use `daemon-snapshot.sh` for state assessment and act on its `recommended_actions`. This ensures deterministic behavior and proper work generation triggering.

```python
def loom_iterate(force_mode=False, debug_mode=False):
    """Execute exactly ONE daemon iteration. Called by parent loop."""

    # Helper function for debug logging
    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # 1. Load state from JSON (enables stateless execution)
    state = load_daemon_state(".loom/daemon-state.json")
    iteration = state.get("iteration", 0) + 1
    debug(f"Iteration {iteration} starting at {now()}")

    # 1b. Reconcile force_mode from argument and state
    # The argument takes precedence (it comes from the current daemon invocation)
    effective_force_mode = force_mode or state.get("force_mode", False)
    if force_mode and not state.get("force_mode", False):
        state["force_mode"] = force_mode
        save_daemon_state(state)
    debug(f"Force mode: {effective_force_mode} (arg={force_mode}, state={state.get('force_mode')})")

    # 2. Check shutdown signal
    if exists(".loom/stop-daemon"):
        debug("Shutdown signal detected")
        return "SHUTDOWN_SIGNAL"

    # 2b. Detect execution mode (mcp > tmux > direct)
    execution_mode = state.get("execution_mode")
    if not execution_mode or iteration == 1:
        execution_mode = detect_execution_mode(debug_mode)
        state["execution_mode"] = execution_mode
        save_daemon_state(state)
    debug(f"Execution mode: {execution_mode}")

    # 3. CRITICAL: Get system state via daemon-snapshot.sh
    # This is the CANONICAL source for all state and recommended actions
    snapshot = run("./.loom/scripts/daemon-snapshot.sh")
    snapshot_data = json.loads(snapshot)

    # Extract computed decisions (these are authoritative)
    recommended_actions = snapshot_data["computed"]["recommended_actions"]
    ready_count = snapshot_data["computed"]["total_ready"]
    needs_work_gen = snapshot_data["computed"]["needs_work_generation"]
    architect_cooldown_ok = snapshot_data["computed"]["architect_cooldown_ok"]
    hermit_cooldown_ok = snapshot_data["computed"]["hermit_cooldown_ok"]

    debug(f"Pipeline state: ready={ready_count} building={snapshot_data['computed']['total_building']}")
    debug(f"Recommended actions: {recommended_actions}")

    # 4. Check subagent completions (non-blocking TaskOutput)
    completions = check_all_completions(state)
    if debug_mode and completions:
        for c in completions:
            debug(f"Completion detected: {c.agent_id} issue=#{c.issue} status={c.status}")

    # 4b. Check support role completions (all 7 roles: Guide, Champion, Doctor, Auditor, Judge, Architect, Hermit)
    completed_support_roles = check_support_role_completions(state, debug_mode)
    if completed_support_roles:
        debug(f"Support roles completed: {completed_support_roles}")

    # 5. CRITICAL: Act on recommended_actions: promote_proposals (force mode only)
    promoted_count = 0
    if "promote_proposals" in recommended_actions and effective_force_mode:
        promotable = snapshot_data["computed"]["promotable_proposals"]
        debug(f"Auto-promoting {len(promotable)} proposals in force mode")
        promoted_count = auto_promote_proposals(promotable, state, debug_mode)

    # 6. Act on recommended_actions: spawn_shepherds
    spawned_shepherds = []
    if "spawn_shepherds" in recommended_actions:
        debug(f"Shepherd pool: {format_shepherd_pool(state)}")
        spawned_shepherds = auto_spawn_shepherds(state, snapshot_data, execution_mode, debug_mode)

    # 7. Check workflow demand (demand-based spawning for champion/doctor/judge)
    demand_spawned = check_workflow_demand(state, snapshot_data, recommended_actions, debug_mode)

    # 8. CRITICAL: Act on recommended_actions: trigger ALL support roles (interval-based)
    # This now includes architect and hermit alongside guide, champion, doctor, auditor, judge.
    # All 7 roles use the unified spawn-support-role.sh infrastructure.
    ensured_roles = auto_ensure_support_roles(state, snapshot_data, recommended_actions, debug_mode, demand_spawned)

    # Extract work generation results from ensured_roles (for summary formatting)
    triggered_generation = {
        "architect": ensured_roles.get("architect", False),
        "hermit": ensured_roles.get("hermit", False)
    }

    # Log work generation even when not triggered (for debugging)
    if needs_work_gen and not triggered_generation["architect"] and not triggered_generation["hermit"]:
        debug(f"Work generation needed but not triggered: architect_cooldown_ok={architect_cooldown_ok}, hermit_cooldown_ok={hermit_cooldown_ok}")

    # 9. Stuck detection
    stuck_count = check_stuck_agents(state, debug_mode)

    # 10. Orphan recovery (every 5 iterations) - catches crashed shepherds mid-session
    recovered_count = 0
    if state.get("iteration", 0) % 5 == 0:
        debug("Running orphan recovery check (every 5 iterations)")
        recovered_count = check_orphaned_shepherds(state, debug_mode)

    # 10b. Stale building detection (every 10 iterations) - catches issues without PRs
    if state.get("iteration", 0) % 10 == 0:
        debug("Running stale building check (every 10 iterations)")
        stale_recovered = check_stale_building(state, debug_mode)
        recovered_count += stale_recovered

    # 11. Save state to JSON
    state["iteration"] = state.get("iteration", 0) + 1
    state["last_poll"] = now()
    state["debug_mode"] = debug_mode
    save_daemon_state(state)

    # 12. Return compact summary (ONE LINE)
    summary = format_iteration_summary(snapshot_data, spawned_shepherds, triggered_generation, ensured_roles, demand_spawned, promoted_count, stuck_count, recovered_count)
    debug(f"Iteration {iteration} completed - {summary}")
    return summary
```

## Iteration Summary Format

The iteration MUST return a compact summary (one line, ~50-100 chars):

```
ready=5 building=2 shepherds=2/3 +shepherd=#123 +architect
```

**Summary components:**
- `mode=X` - Execution mode (mcp/tmux/direct) - only shown on first iteration
- `ready=N` - Issues with loom:issue label
- `building=N` - Issues with loom:building label
- `shepherds=N/M` - Active/max shepherds
- `+shepherd=#N` - Spawned shepherd for issue (if any)
- `+architect` - Triggered Architect (if triggered)
- `+hermit` - Triggered Hermit (if triggered)
- `+guide` - Respawned Guide (if respawned)
- `+champion` - Respawned Champion (if respawned, interval-based)
- `+champion(demand)` - Spawned Champion on-demand (PRs ready to merge)
- `+doctor` - Respawned Doctor (if respawned, interval-based)
- `+doctor(demand)` - Spawned Doctor on-demand (PRs need fixes)
- `+auditor` - Respawned Auditor (if respawned)
- `+judge` - Respawned Judge (if respawned, interval-based)
- `+judge(demand)` - Spawned Judge on-demand (PRs need review)
- `promoted=N` - Proposals auto-promoted to loom:issue in force mode (if any)
- `stuck=N` - Stuck agents detected (if any)
- `completed=#N` - Issue completed this iteration (if any)
- `recovered=N` - Stale building issues recovered (if any)
- `spawn-fail=N` - Task spawns that failed verification (if any)

**Example summaries:**
```
mode=tmux ready=5 building=2 shepherds=2/3
ready=3 building=3 shepherds=3/3 +shepherd=#456 completed=#123
ready=0 building=1 shepherds=1/3 +architect +hermit
ready=2 building=2 shepherds=2/3 stuck=1
ready=2 building=2 shepherds=2/3 spawn-fail=1
ready=3 building=0 shepherds=0/3 promoted=3
mode=direct ready=5 building=0 shepherds=0/3 (tmux pool not detected)
SHUTDOWN_SIGNAL
```

## Using daemon-snapshot.sh for State Assessment

The `daemon-snapshot.sh` script consolidates all state queries into a single tool call, replacing 10+ individual `gh` commands:

```bash
# Get complete system state in one call
snapshot=$(./.loom/scripts/daemon-snapshot.sh)

# Parse the JSON output
ready_count=$(echo "$snapshot" | jq '.computed.total_ready')
needs_work_gen=$(echo "$snapshot" | jq -r '.computed.needs_work_generation')
actions=$(echo "$snapshot" | jq -r '.computed.recommended_actions[]')

# Use pre-computed decisions
if [[ "$needs_work_gen" == "true" ]]; then
    # Trigger architect/hermit
fi

# Check recommended actions
if echo "$actions" | grep -q "spawn_shepherds"; then
    # Auto-spawn shepherds for ready issues
fi
```

**Benefits of daemon-snapshot.sh:**
- **Single tool call**: Replaces 10+ individual `gh` commands
- **Parallel queries**: Runs all `gh` queries concurrently for efficiency
- **Pre-computed decisions**: Includes `computed.recommended_actions` for immediate use
- **Token efficient**: Reduces context usage by ~50% per iteration
- **Deterministic**: Same output format every time, easy to parse

**Output structure:**
```json
{
  "timestamp": "2026-01-25T08:00:00Z",
  "pipeline": { "ready_issues": [...], "building_issues": [...], "blocked_issues": [...] },
  "proposals": { "architect": [...], "hermit": [...], "curated": [...] },
  "prs": { "review_requested": [...], "changes_requested": [...], "ready_to_merge": [...] },
  "usage": { "session_percent": 45, "healthy": true },
  "computed": {
    "total_ready": 3,
    "needs_work_generation": false,
    "available_shepherd_slots": 2,
    "recommended_actions": ["spawn_shepherds", "check_stuck"]
  },
  "config": { "issue_threshold": 3, "max_shepherds": 3 }
}
```

## Auto-Spawn Shepherds

> **CRITICAL: Use slash commands directly in Task prompts**
>
> When spawning roles via Task subagents, use the slash command directly as the prompt.
>
> **CORRECT** - Slash command is the entire prompt:
> ```python
> Task(
>     description="Shepherd issue #123",
>     prompt="/shepherd 123 --force-pr",
>     run_in_background=True
> )
> ```
>
> **WRONG** - Telling subagent to call Skill (expands role into subagent context):
> ```python
> Task(
>     prompt="""Skill(skill="shepherd", args="123 --force-pr")"""  # DON'T DO THIS
> )
> ```
>
> **Why**: Claude Code executes slash commands natively. Telling a subagent to "call Skill"
> causes it to invoke Skill itself, expanding the role prompt into its own context window
> instead of running the role. This wastes context and causes spawn failures.
>
> This pattern applies to ALL role spawning: shepherds, architect, hermit, guide, champion,
> doctor, auditor, and judge. The Skill tool is only for the parent daemon invoking iteration
> subagents (parent → iteration), not for iteration subagents invoking roles.

```python
# Maximum spawn failures before marking an issue as blocked
MAX_SPAWN_FAILURES = 3

def auto_spawn_shepherds(state, snapshot_data, execution_mode, debug_mode=False):
    """Automatically spawn shepherds using the appropriate execution backend.

    Tracks spawn failures per issue. After MAX_SPAWN_FAILURES consecutive failures,
    the issue is marked as loom:blocked to prevent infinite retry loops.
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # daemon-snapshot.sh returns issues pre-sorted by LOOM_ISSUE_STRATEGY:
    # - loom:urgent issues always first (regardless of strategy)
    # - Then sorted by: fifo (oldest first), lifo (newest first), or priority
    ready_issues = snapshot_data["pipeline"]["ready_issues"]
    active_count = count_active_shepherds(state)
    max_shepherds = snapshot_data["config"]["max_shepherds"]

    # Initialize retry queue in state if not present
    if "spawn_retry_queue" not in state:
        state["spawn_retry_queue"] = {}

    # Determine shepherd mode based on daemon's force_mode
    force_mode = state.get("force_mode", False)
    shepherd_flag = "--force-merge" if force_mode else "--force-pr"

    debug(f"Issue selection: {len(ready_issues)} ready issues, {active_count}/{max_shepherds} shepherds active")
    debug(f"Shepherd mode: {shepherd_flag} (force_mode={force_mode})")
    debug(f"Execution mode: {execution_mode}")

    spawned = []
    spawn_failures = 0

    # For tmux mode, get available idle shepherd sessions
    tmux_idle_shepherds = []
    if execution_mode == "tmux":
        tmux_idle_shepherds = get_tmux_idle_shepherds(debug_mode)
        # Filter to only idle ones (not currently assigned to issues)
        assigned_shepherds = set()
        for shepherd_id, info in state.get("shepherds", {}).items():
            if info.get("issue") and info.get("tmux_session"):
                assigned_shepherds.add(info["tmux_session"])
        tmux_idle_shepherds = [s for s in tmux_idle_shepherds if s not in assigned_shepherds]
        debug(f"tmux idle shepherds: {tmux_idle_shepherds}")
        max_shepherds = len(tmux_idle_shepherds) + active_count  # Limit to available tmux sessions

    while active_count < max_shepherds and len(ready_issues) > 0:
        issue = ready_issues.pop(0)["number"]
        issue_key = str(issue)

        # Check retry queue for this issue
        retry_info = state["spawn_retry_queue"].get(issue_key, {"failures": 0, "last_attempt": None})

        # Skip issues that have exceeded max failures (they should already be blocked)
        if retry_info["failures"] >= MAX_SPAWN_FAILURES:
            debug(f"Issue #{issue} exceeded max spawn failures ({MAX_SPAWN_FAILURES}), should be blocked")
            continue

        debug(f"Issue selection: Claiming #{issue} (prior failures={retry_info['failures']})")

        # Claim immediately (atomic operation)
        run(f"gh issue edit {issue} --remove-label 'loom:issue' --add-label 'loom:building'")

        # Dispatch based on execution mode
        if execution_mode == "tmux":
            # tmux mode: Send command to idle shepherd session
            result = dispatch_shepherd_tmux(issue, shepherd_flag, tmux_idle_shepherds, state, debug_mode)
        else:
            # direct mode: Spawn Task subagent
            result = dispatch_shepherd_direct(issue, shepherd_flag, debug_mode)

        # Verify spawn succeeded
        if not result.get("success"):
            error_msg = result.get('error', 'unknown')
            debug(f"Spawn failed for #{issue}: {error_msg}")

            # Track failure in retry queue
            retry_info["failures"] += 1
            retry_info["last_attempt"] = now()
            retry_info["last_error"] = error_msg
            state["spawn_retry_queue"][issue_key] = retry_info

            if retry_info["failures"] >= MAX_SPAWN_FAILURES:
                # Mark as blocked after max failures
                debug(f"Issue #{issue} reached max spawn failures ({MAX_SPAWN_FAILURES}), marking as blocked")
                run(f"gh issue edit {issue} --remove-label 'loom:building' --add-label 'loom:blocked'")
                comment = f"**[daemon] Spawn Failed**\\n\\nThis issue has been marked as blocked after {MAX_SPAWN_FAILURES} consecutive spawn failures.\\n\\nLast error: `{error_msg}`\\n\\nA human or the Doctor role may need to investigate and unblock this issue."
                run(f"gh issue comment {issue} --body '{comment}'")
                print(f"  BLOCKED: #{issue} (spawn failed {MAX_SPAWN_FAILURES}x)")
            else:
                # Revert to loom:issue for retry
                debug(f"Reverting #{issue} to loom:issue for retry (failures={retry_info['failures']})")
                run(f"gh issue edit {issue} --remove-label 'loom:building' --add-label 'loom:issue'")

            spawn_failures += 1
            save_daemon_state(state)
            continue

        # Spawn succeeded - clear retry queue for this issue
        if issue_key in state["spawn_retry_queue"]:
            del state["spawn_retry_queue"][issue_key]
            save_daemon_state(state)

        debug(f"Spawning decision: shepherd assigned to #{issue} (verified)")

        # Record assignment based on mode
        if execution_mode == "tmux":
            debug(f"  tmux session: {result['tmux_session']}")
            record_shepherd_assignment_tmux(state, issue, result["tmux_session"])
            # Remove from idle pool
            if result["tmux_session"] in tmux_idle_shepherds:
                tmux_idle_shepherds.remove(result["tmux_session"])
        else:
            debug(f"  Task ID: {result['task_id']}")
            debug(f"  Output file: {result['output_file']}")
            record_shepherd_assignment(state, issue, result["task_id"], result["output_file"])

        active_count += 1
        spawned.append(issue)

        mode_suffix = f"tmux:{result.get('tmux_session', '')}" if execution_mode == "tmux" else "direct"
        print(f"  AUTO-SPAWNED: shepherd for issue #{issue} ({shepherd_flag}, {mode_suffix})")

    if len(spawned) == 0 and len(ready_issues) == 0:
        debug("No shepherds spawned: no ready issues available")
    elif len(spawned) == 0:
        if execution_mode == "tmux" and len(tmux_idle_shepherds) == 0:
            debug("No shepherds spawned: no idle tmux sessions available")
        else:
            debug(f"No shepherds spawned: at capacity ({max_shepherds} max)")

    return {"spawned": spawned, "failures": spawn_failures}


def dispatch_shepherd_tmux(issue, shepherd_flag, idle_shepherds, state, debug_mode=False):
    """Dispatch shepherd work to an idle tmux session via loom send."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    if len(idle_shepherds) == 0:
        return {"success": False, "error": "no_idle_shepherds"}

    # Pick first idle shepherd
    tmux_session = idle_shepherds[0]
    agent_name = tmux_session.replace("loom-", "")  # e.g., "shepherd-1"

    # Send the shepherd command
    command = f"/shepherd {issue} {shepherd_flag}"
    debug(f"Sending to {tmux_session}: {command}")

    result = run(f'./.loom/scripts/cli/loom-send.sh "{agent_name}" "{command}" --json')

    try:
        send_result = json.loads(result)
        if send_result.get("success"):
            return {
                "success": True,
                "tmux_session": tmux_session,
                "command": command
            }
        else:
            return {"success": False, "error": send_result.get("error", "send_failed")}
    except Exception as e:
        debug(f"Failed to parse loom send result: {e}")
        return {"success": False, "error": str(e)}


def dispatch_shepherd_direct(issue, shepherd_flag, debug_mode=False):
    """Dispatch shepherd as a Task subagent (direct mode).

    Uses slash command directly as prompt - Claude Code executes /shepherd natively.
    This avoids the Skill-in-Task anti-pattern that expands role prompts into subagent context.
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # Use slash command directly - Claude Code executes this natively
    # DO NOT use Skill() in the prompt - it expands the role into the subagent's context
    result = Task(
        description=f"Shepherd issue #{issue}",
        prompt=f"/shepherd {issue} {shepherd_flag}",
        run_in_background=True
    )

    # Verify the task actually started before recording
    if not verify_task_spawn(result, f"shepherd for #{issue}"):
        return {"success": False, "error": "verification_failed"}

    # Double-check task_id format to catch fabricated IDs
    # Real Task tool IDs are 7-char hex (e.g., 'a7dc1e0'), not 'shepherd-123456'
    if not validate_task_id(result.task_id):
        debug(f"Task ID format invalid: '{result.task_id}' - Task tool may not have been invoked")
        return {"success": False, "error": f"invalid_task_id_format: {result.task_id}"}

    return {
        "success": True,
        "task_id": result.task_id,
        "output_file": result.output_file
    }


def record_shepherd_assignment_tmux(state, issue, tmux_session):
    """Record shepherd assignment for tmux mode."""
    if "shepherds" not in state:
        state["shepherds"] = {}

    shepherd_id = tmux_session.replace("loom-", "")  # e.g., "shepherd-1"
    state["shepherds"][shepherd_id] = {
        "status": "working",
        "issue": issue,
        "tmux_session": tmux_session,
        "started": now(),
        "execution_mode": "tmux"
    }
    save_daemon_state(state)


def validate_task_id(task_id):
    """Validate that a task_id matches the expected format from the Task tool.

    Real Task tool task IDs are 7-character lowercase hexadecimal strings (e.g., 'a7dc1e0', 'abeb2e8').
    Fabricated task IDs typically look like 'auditor-1769471216' or 'champion-12345'.

    Returns True if valid, False if fabricated or malformed.
    """
    if not task_id or not isinstance(task_id, str):
        return False

    # Real task IDs are 7-char hex strings
    import re
    return bool(re.match(r'^[a-f0-9]{7}$', task_id))


def record_support_role(state, role_name, task_id, output_file):
    """Record support role assignment in daemon state with task_id validation.

    Validates the task_id format before recording to prevent fabricated IDs
    (e.g., 'auditor-1769471216') from being stored in daemon-state.json.
    Only real Task tool IDs (7-char hex UUIDs like 'a7dc1e0') are accepted.
    """
    # Validate task_id format before recording
    if not validate_task_id(task_id):
        raise ValueError(
            f"Invalid task_id format for {role_name}: '{task_id}' "
            f"(expected 7-char hex UUID like 'a7dc1e0', got fabricated string). "
            f"The Task tool was likely not actually invoked."
        )

    if "support_roles" not in state:
        state["support_roles"] = {}

    state["support_roles"][role_name] = {
        "status": "running",
        "task_id": task_id,
        "output_file": output_file,
        "started_at": now()
    }
    save_daemon_state(state)


def record_shepherd_assignment(state, issue, task_id, output_file):
    """Record shepherd assignment for direct mode with task_id validation.

    Validates the task_id format before recording to prevent fabricated IDs
    from being stored in daemon-state.json.
    """
    # Validate task_id format before recording
    if not validate_task_id(task_id):
        raise ValueError(
            f"Invalid task_id format for shepherd (issue #{issue}): '{task_id}' "
            f"(expected 7-char hex UUID like 'a7dc1e0', got fabricated string). "
            f"The Task tool was likely not actually invoked."
        )

    if "shepherds" not in state:
        state["shepherds"] = {}

    # Find next available shepherd slot
    shepherd_id = find_next_shepherd_id(state)
    state["shepherds"][shepherd_id] = {
        "status": "working",
        "issue": issue,
        "task_id": task_id,
        "output_file": output_file,
        "started": now(),
        "execution_mode": "direct"
    }
    save_daemon_state(state)
```

## Auto-Promote Proposals (Force Mode Only)

In force mode, the daemon AUTOMATICALLY promotes proposals to `loom:issue`:

```python
def auto_promote_proposals(promotable_issues, state, debug_mode=False):
    """Auto-promote proposals to loom:issue in force mode."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    promoted = 0

    for issue_num in promotable_issues:
        try:
            # Get current labels
            issue_data = run(f"gh issue view {issue_num} --json labels --jq '.labels[].name'")
            labels = issue_data.strip().split('\n')

            # Determine which proposal label to remove
            remove_label = None
            proposal_type = None
            if "loom:architect" in labels:
                remove_label = "loom:architect"
                proposal_type = "architect"
            elif "loom:hermit" in labels:
                remove_label = "loom:hermit"
                proposal_type = "hermit"
            elif "loom:curated" in labels:
                remove_label = "loom:curated"
                proposal_type = "curated"
            else:
                debug(f"Issue #{issue_num} has no promotable label, skipping")
                continue

            # Skip if blocked
            if "loom:blocked" in labels:
                debug(f"Issue #{issue_num} is blocked, skipping promotion")
                continue

            # Promote: remove proposal label, add loom:issue
            run(f"gh issue edit {issue_num} --remove-label '{remove_label}' --add-label 'loom:issue'")

            # Add audit trail comment
            timestamp = now()
            comment = f"""**[force-mode] Daemon Auto-Promotion**

This {proposal_type} proposal has been automatically promoted to `loom:issue` by the Loom daemon running in force mode.

**Ready for Builder** - A shepherd will claim this issue in the next iteration.

**Force mode enabled**: Champion evaluation bypassed for aggressive autonomous development.

---
*Automated by Loom daemon (force mode) at {timestamp}*"""
            run(f"gh issue comment {issue_num} --body '{comment}'")

            promoted += 1
            debug(f"Promoted #{issue_num} ({proposal_type} -> loom:issue)")

        except Exception as e:
            debug(f"Failed to promote #{issue_num}: {e}")
            continue

    return promoted
```

## Trigger Work Generation

**CRITICAL**: Work generation keeps the pipeline fed. When `daemon-snapshot.sh` includes `trigger_architect` or `trigger_hermit` in `recommended_actions`, the iteration MUST spawn these roles.

**Note**: `trigger_architect_role()` and `trigger_hermit_role()` have been removed. Architect and
hermit now use the unified `trigger_support_role()` function (same as guide, champion, doctor,
auditor, and judge). This eliminates separate state management paths and ensures all support roles
use `spawn-support-role.sh` for deterministic state management. The top-level
`last_architect_trigger` and `last_hermit_trigger` fields in daemon-state.json are no longer
written; cooldown is now tracked via `support_roles.architect.last_completed` and
`support_roles.hermit.last_completed`.

## Deterministic Support Role Spawning

Support role spawning uses the deterministic `spawn-support-role.sh` script for all
spawn decisions and state management. This eliminates LLM interpretation variability
and ensures reliable support role operation in direct mode.

### spawn-support-role.sh

The script handles:
- **Interval checking**: Whether enough time has elapsed since last completion
- **Idempotency**: Never spawns if role already running with valid task_id
- **Demand mode**: Immediate spawn when `--demand` flag passed (skips interval)
- **State management**: `--mark-running` and `--mark-completed` for atomic state updates
- **Fabricated ID detection**: Resets roles stuck with invalid task IDs

```bash
# Check if a role should be spawned (interval-based)
./.loom/scripts/spawn-support-role.sh guide --json
# {"should_spawn":true,"reason":"interval_elapsed","role":"guide",...}

# Check if a role should be spawned (demand-based, skips interval)
./.loom/scripts/spawn-support-role.sh champion --demand --json
# {"should_spawn":true,"reason":"demand","role":"champion"}

# Check all roles at once
./.loom/scripts/spawn-support-role.sh --check-all --json
# {"roles":[...],"any_should_spawn":true}

# After successful Task spawn, mark role as running
./.loom/scripts/spawn-support-role.sh --mark-running champion --task-id a7dc1e0

# After task completion, mark role as idle
./.loom/scripts/spawn-support-role.sh --mark-completed champion
```

## Workflow Demand (Demand-Based Spawning)

The daemon spawns Champion/Doctor/Judge immediately when work awaits them, providing faster response than interval-based spawning. Uses `spawn-support-role.sh --demand` to skip interval checks:

```python
def check_workflow_demand(state, snapshot_data, recommended_actions, debug_mode=False):
    """Spawn roles immediately when work awaits them.

    Uses spawn-support-role.sh with --demand flag for deterministic spawn decisions.
    The script checks idempotency (not already running) even in demand mode.
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    demand_spawned = {"champion": False, "doctor": False, "judge": False}

    # Champion on-demand: PRs ready to merge
    if "spawn_champion_demand" in recommended_actions:
        prs_ready = snapshot_data.get("prs", {}).get("ready_to_merge", [])
        pr_count = len(prs_ready)
        debug(f"Champion demand detected: {pr_count} PRs ready to merge")

        # Use deterministic script to check if spawn is allowed
        check = run("./.loom/scripts/spawn-support-role.sh champion --demand --json")
        check_data = json.loads(check)
        if check_data["should_spawn"]:
            if trigger_support_role(state, "champion", f"Champion (on-demand, {pr_count} PRs)", debug_mode):
                demand_spawned["champion"] = True
                print(f"  AUTO-SPAWNED: Champion (on-demand, {pr_count} PRs ready to merge)")
        else:
            debug(f"Champion demand skipped: {check_data['reason']}")

    # Doctor on-demand: PRs need fixes
    if "spawn_doctor_demand" in recommended_actions:
        prs_needing_fixes = snapshot_data.get("prs", {}).get("changes_requested", [])
        pr_count = len(prs_needing_fixes)
        debug(f"Doctor demand detected: {pr_count} PRs need fixes")

        check = run("./.loom/scripts/spawn-support-role.sh doctor --demand --json")
        check_data = json.loads(check)
        if check_data["should_spawn"]:
            if trigger_support_role(state, "doctor", f"Doctor (on-demand, {pr_count} PRs)", debug_mode):
                demand_spawned["doctor"] = True
                print(f"  AUTO-SPAWNED: Doctor (on-demand, {pr_count} PRs need fixes)")
        else:
            debug(f"Doctor demand skipped: {check_data['reason']}")

    # Judge on-demand: PRs need review
    if "spawn_judge_demand" in recommended_actions:
        prs_needing_review = snapshot_data.get("prs", {}).get("review_requested", [])
        pr_count = len(prs_needing_review)
        debug(f"Judge demand detected: {pr_count} PRs need review")

        check = run("./.loom/scripts/spawn-support-role.sh judge --demand --json")
        check_data = json.loads(check)
        if check_data["should_spawn"]:
            if trigger_support_role(state, "judge", f"Judge (on-demand, {pr_count} PRs)", debug_mode):
                demand_spawned["judge"] = True
                print(f"  AUTO-SPAWNED: Judge (on-demand, {pr_count} PRs need review)")
        else:
            debug(f"Judge demand skipped: {check_data['reason']}")

    return demand_spawned
```

## Auto-Ensure Support Roles (Interval-Based)

Uses `spawn-support-role.sh` (without `--demand`) for interval-based spawn decisions:

```python
def auto_ensure_support_roles(state, snapshot_data, recommended_actions, debug_mode=False, demand_spawned=None):
    """Automatically keep Guide, Champion, Doctor, Auditor, Judge, Architect, and Hermit running.

    Uses spawn-support-role.sh for deterministic interval checking. The script
    handles all interval math, idempotency, and fabricated task_id detection.

    Architect and hermit are managed here alongside the other support roles,
    using the same spawn-support-role.sh infrastructure for unified state management.
    """

    if demand_spawned is None:
        demand_spawned = {"champion": False, "doctor": False, "judge": False}

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    ensured_roles = {"guide": False, "champion": False, "doctor": False, "auditor": False, "judge": False, "architect": False, "hermit": False}

    debug("Checking support roles via spawn-support-role.sh (interval-based)")
    debug(f"Recommended actions: {recommended_actions}")
    debug(f"Demand-spawned: {demand_spawned}")

    # Define which roles to check and their trigger actions
    # Architect and hermit use trigger_architect/trigger_hermit actions from daemon-snapshot.sh
    role_checks = [
        ("guide", "trigger_guide", "Guide backlog triage", False),
        ("champion", "trigger_champion", "Champion PR merge", demand_spawned.get("champion", False)),
        ("doctor", "trigger_doctor", "Doctor PR conflict resolution", demand_spawned.get("doctor", False)),
        ("auditor", "trigger_auditor", "Auditor main branch validation", False),
        ("judge", "trigger_judge", "Judge PR review", demand_spawned.get("judge", False)),
        ("architect", "trigger_architect", "Architect work generation", False),
        ("hermit", "trigger_hermit", "Hermit simplification proposals", False),
    ]

    for role_name, trigger_action, description, already_spawned in role_checks:
        # Skip if demand already spawned this role
        if already_spawned:
            debug(f"Skipping {role_name}: already demand-spawned this iteration")
            continue

        # Skip if not in recommended actions
        if trigger_action not in recommended_actions:
            continue

        # Use deterministic script to check if spawn is needed
        check = run(f"./.loom/scripts/spawn-support-role.sh {role_name} --json")
        try:
            check_data = json.loads(check)
        except Exception:
            debug(f"Failed to parse spawn-support-role.sh output for {role_name}")
            continue

        if check_data["should_spawn"]:
            debug(f"{role_name}: spawn needed ({check_data['reason']})")
            ensured_roles[role_name] = trigger_support_role(state, role_name, description, debug_mode)
        else:
            debug(f"{role_name}: spawn not needed ({check_data['reason']})")

    return ensured_roles


def trigger_support_role(state, role_name, description, debug_mode=False):
    """Spawn a support role using Task tool with direct slash command.

    Uses slash command directly - Claude Code executes /role natively.
    This avoids the Skill-in-Task anti-pattern that expands role prompts into subagent context.

    After successful spawn, uses spawn-support-role.sh --mark-running to
    record the task_id in daemon-state.json atomically.
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    # Use slash commands directly - Claude Code executes these natively
    # DO NOT use Skill() in prompts - it expands the role into the subagent's context
    role_prompts = {
        "guide": "/guide",
        "champion": "/champion",
        "doctor": "/doctor",
        "auditor": "/auditor",
        "judge": "/judge",
        "architect": "/architect --autonomous",
        "hermit": "/hermit"
    }

    prompt = role_prompts.get(role_name)
    if not prompt:
        debug(f"Unknown role: {role_name}")
        return False

    result = Task(
        description=description,
        prompt=prompt,
        run_in_background=True
    )

    if not verify_task_spawn(result, role_name.capitalize()):
        print(f"  SPAWN FAILED: {role_name.capitalize()} - verification failed")
        return False

    # Validate task_id format to catch fabricated IDs (e.g., 'auditor-1769471216')
    # Real Task tool IDs are 7-char hex strings (e.g., 'a7dc1e0')
    if not validate_task_id(result.task_id):
        print(f"  SPAWN FAILED: {role_name.capitalize()} - fabricated task_id: '{result.task_id}'")
        print(f"    Expected 7-char hex UUID, got non-Task-tool string")
        print(f"    The Task tool was likely not actually invoked")
        return False

    # Use deterministic script to record state atomically
    mark_result = run(f"./.loom/scripts/spawn-support-role.sh --mark-running {role_name} --task-id {result.task_id}")
    debug(f"State update: {mark_result.strip()}")

    # Also update in-memory state for consistency within this iteration
    if "support_roles" not in state:
        state["support_roles"] = {}
    state["support_roles"][role_name] = {
        "status": "running",
        "task_id": result.task_id,
        "output_file": result.output_file,
        "started_at": now()
    }

    print(f"  AUTO-SPAWNED: {role_name.capitalize()} (verified, task_id={result.task_id})")
    return True
```

## Check Support Role Completions

Uses `spawn-support-role.sh --mark-completed` for atomic state transitions:

```python
def check_support_role_completions(state, debug_mode=False):
    """Check if any support roles have completed and update their state.

    Uses spawn-support-role.sh --mark-completed for atomic state updates
    when a role transitions from running to idle.
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    completed_roles = []

    if "support_roles" not in state:
        return completed_roles

    now_iso = now()

    for role_name, role_info in state["support_roles"].items():
        # Skip roles that aren't running
        if role_info.get("status") != "running":
            continue

        task_id = role_info.get("task_id")
        if not task_id:
            continue

        # Detect fabricated task IDs already in state (from previous buggy iterations)
        # Real Task tool IDs are 7-char hex (e.g., 'a7dc1e0'), not 'auditor-1769471216'
        if not validate_task_id(task_id):
            debug(f"WARNING: {role_name.capitalize()} has fabricated task_id in state: '{task_id}'")
            debug(f"  Resetting {role_name} to idle (task was never actually spawned)")
            # Use deterministic script for atomic state update
            run(f"./.loom/scripts/spawn-support-role.sh --mark-completed {role_name}")
            # Update in-memory state
            role_info["status"] = "idle"
            role_info["last_completed"] = now_iso
            role_info["last_error"] = f"fabricated_task_id: {task_id}"
            role_info["task_id"] = None
            role_info["output_file"] = None
            completed_roles.append(role_name)
            continue

        try:
            # Non-blocking check for completion
            check = TaskOutput(task_id=task_id, block=False, timeout=1000)

            if check.status == "completed":
                # Use deterministic script for atomic state update
                run(f"./.loom/scripts/spawn-support-role.sh --mark-completed {role_name}")
                # Update in-memory state
                role_info["status"] = "idle"
                role_info["last_completed"] = now_iso
                role_info["task_id"] = None
                role_info["output_file"] = None

                completed_roles.append(role_name)
                debug(f"{role_name.capitalize()} completed (task {task_id})")

            elif check.status == "failed":
                # Use deterministic script for atomic state update
                run(f"./.loom/scripts/spawn-support-role.sh --mark-completed {role_name}")
                # Update in-memory state
                role_info["status"] = "idle"
                role_info["last_completed"] = now_iso
                role_info["last_error"] = "task_failed"
                role_info["task_id"] = None
                role_info["output_file"] = None

                debug(f"{role_name.capitalize()} failed (task {task_id})")

        except Exception as e:
            debug(f"Error checking {role_name} status: {e}")

    return completed_roles
```

## Error Handling

### Stuck Agent Detection

```python
def check_stuck_agents(state, debug_mode=False):
    """Auto-detect stuck agents and trigger appropriate interventions."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    result = run("./.loom/scripts/stuck-detection.sh check --json")

    if result.exit_code == 2:  # Stuck agents found
        stuck_data = json.loads(result.stdout)

        for agent_result in stuck_data["results"]:
            if agent_result["stuck"]:
                severity = agent_result["severity"]
                intervention = agent_result["suggested_intervention"]
                issue = agent_result["issue"]
                indicators = agent_result["indicators"]

                print(f"  STUCK: {agent_result['agent_id']} on #{issue}")
                debug(f"    Severity: {severity}")
                debug(f"    Indicators: {', '.join(indicators)}")
                debug(f"    Intervention: {intervention}")

        return stuck_data.get("stuck_count", 0)

    return 0
```

### Stale Building Detection

```python
def check_stale_building(state, debug_mode=False):
    """Detect and recover orphaned building issues."""

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    result = run("./.loom/scripts/stale-building-check.sh --recover --json")

    if result.exit_code == 0:
        data = json.loads(result.stdout)
        recovered = [i for i in data.get("stale_issues", []) if i["reason"] == "no_pr"]

        for issue in recovered:
            print(f"  RECOVERED: #{issue['number']} (stale {issue['age_hours']}h, no PR)")

        return len(recovered)

    return 0
```

### Orphan Recovery (In-Session)

```python
def check_orphaned_shepherds(state, debug_mode=False):
    """Detect and recover orphaned shepherds from crashes mid-session.

    This runs every 5 iterations (not just at startup) to catch:
    - Shepherds with stale task IDs (tasks that no longer exist)
    - loom:building issues without active shepherds
    - Progress files with stale heartbeats
    """

    def debug(msg):
        if debug_mode:
            print(f"[DEBUG] {msg}")

    debug("Running in-session orphan recovery check")

    result = run("./.loom/scripts/recover-orphaned-shepherds.sh --recover --json")

    if result.exit_code == 0:
        try:
            data = json.loads(result.stdout)
            recovered_issues = data.get("recovered_issues", [])
            recovered_shepherds = data.get("recovered_shepherds", [])

            for issue in recovered_issues:
                print(f"  RECOVERED: #{issue} (orphaned - no active shepherd)")

            for shepherd_id in recovered_shepherds:
                debug(f"Reset orphaned shepherd: {shepherd_id}")

            return len(recovered_issues)
        except Exception as e:
            debug(f"Failed to parse orphan recovery result: {e}")
            return 0

    return 0
```

## Iteration State Handling

The iteration subagent reads and writes state atomically:

```python
# Read state at start
state = json.load(open(".loom/daemon-state.json"))

# ... do all iteration work ...

# Write state at end (atomic)
with open(".loom/daemon-state.json.tmp", "w") as f:
    json.dump(state, f, indent=2)
os.rename(".loom/daemon-state.json.tmp", ".loom/daemon-state.json")
```

**Important:** All context-heavy operations (gh commands, TaskOutput, spawning) happen ONLY in iteration mode.

## Empty Backlog Handling

When backlog is empty, the iteration triggers work generation:

```python
def handle_empty_backlog(state, debug_mode):
    """Automatic response to empty backlog."""

    if ready_issues == 0:
        print("  Backlog empty - checking work generation triggers...")

        # Work generation is handled by acting on recommended_actions
        # daemon-snapshot.sh includes trigger_architect/trigger_hermit when appropriate

        # Report what human can do (informational only)
        if curated_count > 0:
            print(f"  Human action available: Approve {curated_count} curated issues")
        if proposal_count > 0:
            print(f"  Human action available: Approve {proposal_count} proposals")
```

## Commands for Iteration Mode

| Command | Description |
|---------|-------------|
| `/loom iterate` | Execute single iteration (used by parent loop) |
| `/loom iterate --force` | Single iteration with force mode |
| `/loom iterate --debug` | Single iteration with verbose debug logging |

### Command Detection

```python
args = "$ARGUMENTS".strip().split()

if "iterate" in args:
    force_mode = "--force" in args
    debug_mode = "--debug" in args
    summary = loom_iterate(force_mode, debug_mode)
    print(summary)  # This is what parent receives
```

## Debug Mode Output

When running with `--debug`, iteration produces verbose logging:

**Direct Mode (Task subagents):**
```
[DEBUG] Iteration 5 starting at 2026-01-25T10:30:00Z
[DEBUG] Mode detection: using direct mode (Task subagents)
[DEBUG] Execution mode: direct
[DEBUG] Pipeline state: ready=3 building=1 review_requested=2
[DEBUG] Shepherd pool: shepherd-1=working(#123) shepherd-2=idle shepherd-3=idle
[DEBUG] Issue selection: Considering #456 (age: 2h, priority: normal)
[DEBUG] Issue selection: Skipping #457 (blocked by #400)
[DEBUG] Shepherd mode: --force-merge (force_mode=true)
[DEBUG] Spawning decision: shepherd-2 assigned to #456
[DEBUG]   Task ID: abc123
[DEBUG]   Output file: /tmp/claude/.../abc123.output
[DEBUG]   Command: /shepherd 456 --force-merge
[DEBUG] Iteration 5 completed in 1.2s
```

**tmux Mode (tmux agent pool):**
```
[DEBUG] Iteration 5 starting at 2026-01-25T10:30:00Z
[DEBUG] Mode detection: tmux pool available (3 shepherd sessions)
[DEBUG] Execution mode: tmux
[DEBUG] Pipeline state: ready=3 building=1 review_requested=2
[DEBUG] tmux shepherd sessions: ['loom-shepherd-1', 'loom-shepherd-2', 'loom-shepherd-3']
[DEBUG] tmux idle shepherds: ['loom-shepherd-2', 'loom-shepherd-3']
[DEBUG] Issue selection: Claiming #456
[DEBUG] Sending to loom-shepherd-2: /shepherd 456 --force-merge
[DEBUG] Spawning decision: shepherd assigned to #456 (verified)
[DEBUG]   tmux session: loom-shepherd-2
[DEBUG] Iteration 5 completed in 0.8s
```
