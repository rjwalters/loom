# Daemon Roll: Pause-and-Resume Contract for In-Flight Work (D2)

Design spike for #10714, part of tracker #10698 (Phase 2, step 2). It implements
operator decision **D2** (2026-10-07): a roll does not wait for in-flight work
to reach zero, whatever triggered it. The daemon pauses its in-flight work,
restarts onto the new binary, and the new process resumes that work.

**Status: proposal, revised for the operator decisions of 2026-10-07 (recorded
on #10714 and #10698).** Those decisions answer every §12 question (Q1-Q7).
The operator must review this doc before #10715 (implementation) is curated. No runtime behaviour changes until #10715 lands.

This is a repo-local design doc under `docs/design/`. It is never installed
into consumer repos.

Citations are `path:line` against `main` at `74721c1c4` (2026-10-07). Paths
without a prefix are under `loom-daemon/src/`.

**Contents**

- [1. Summary of decisions](#1-summary-of-decisions)
- [2. What "pause" means](#2-what-pause-means)
- [3. Facts the design rests on](#3-facts-the-design-rests-on)
- [4. Per-kind contract](#4-per-kind-contract)
- [5. Child processes and dev servers: who owns and reaps them](#5-child-processes-and-dev-servers-who-owns-and-reaps-them)
- [6. The pause manifest (schema v1)](#6-the-pause-manifest-schema-v1)
- [7. State transitions: H3 Staged, H4 Pausing, H5 Verifying](#7-state-transitions-h3-staged-h4-pausing-h5-verifying)
- [8. Rollback (#9735)](#8-rollback-9735)
- [9. Requeue-and-record rules](#9-requeue-and-record-rules)
- [10. Survey: existing machinery, reuse vs new](#10-survey-existing-machinery-reuse-vs-new)
- [11. Step 3 touch list (#10715)](#11-step-3-touch-list-10715)
- [12. Operator questions (all answered)](#12-operator-questions-all-answered)

## 1. Summary of decisions

1. **Pause means every daemon-dispatched agent is brought to a resumable
   stop.** No agent keeps running across the restart; there is no "carry"
   (operator decision, 2026-10-07). At H4 the daemon asks each agent to stop at
   its next **safe point**, which is a moment between tool calls when no tool is
   executing (§2). It then stops the agent's whole process tree, including tool
   subprocesses and dev servers, and writes the agent's **resume handle** to the
   pause manifest. The handle is the runtime session id plus the worktree,
   claim, lease and sweep checkpoint. After the new daemon passes health (H5), it
   relaunches each agent from its saved session (Claude Code `--resume
   <session>`, Codex `exec resume <session>`) in the same worktree. The tool call
   the agent was about to make when it was parked did not run, and the resumed
   agent re-runs it.
2. **Young agents are reset, not paused.** An agent that has been running for
   less than `minResumableAgeSecs` (default **300 s = 5 min**, configurable) is
   killed at H4. Its claim is released, its issue is requeued, and the reason
   `young-agent-reset` is recorded (§9). The age is measured from the session's
   first start, so an agent that was already resumed once keeps its full age.
3. **No special cases by runtime or kind.** Containerized Codex sessions follow
   the same pause/resume path as Claude Code sessions (answers Q2). Role runs
   follow the same rule as sweeps, including the 5-minute rule (answers Q7).
4. **Anything that cannot be resumed is requeued and recorded, never silently
   lost.** That covers an agent that misses the pause budget, an agent whose
   session id was never captured, and a session that fails to resume at H5. The
   closed reason set is in §9.
5. **H4 leaves every stopped agent looking like a crashed daemon-owned agent.**
   Its lock, journal entry and checkpoint stay in place, and its pid is dead. So
   a binary that cannot read the manifest (including a rollback to a binary older
   than #10715) recovers through today's restart recovery: it resumes or
   requeues from the checkpoint, or it reclaims the claim once the lease ages
   out. No claim is stranded (§8). The manifest adds the same-session resume and
   the audit trail. Claim safety does not depend on it.
6. **The H4→H5 window is bounded well below the lease TTL** (15 min,
   `claim_reconciliation.rs:551`). Stopping an agent also stops its lease
   renewal loop, so H4 refreshes each paused item's lease once, at pause time. A
   manifest older than the TTL is stale: its items are requeued, never resumed.
7. **One roll machine for every trigger.** Floor, repo-ahead,
   restart-only-config and ordinary autoUpdate rolls all share the same
   H3→H4→H5 pause-and-roll path (operator decision, 2026-10-07, Q3). There is
   no separate wait-for-zero drain path. Today's `drain_roll` and `roll_stall`
   machinery is replaced (§10, §11).
8. **Budgets are starting values.** The pause budget, verify probation, resume
   budget, manifest max age and `minResumableAgeSecs` ship with the defaults
   below. Each is configurable and is reported in telemetry (§7, §9), so they
   can be tuned from experience (Q4).
9. **No transition guards.** Until #10716 lands, a resumed agent is assumed
   compatible with the new daemon and the workspace. The design adds no
   machinery to guard that window (Q5).

## 2. What "pause" means

D2 left open what "pause" means for each kind of work. These meanings were
considered:

| Meaning | Verdict |
|---|---|
| **Freeze** (`SIGSTOP` the tree, `SIGCONT` after the roll) | **Rejected.** A frozen agent still holds its token, its forge claim and its API connection, and the model provider times out the stream anyway. A frozen process that does not survive the daemon's exit dies frozen. |
| **Carry**: let it run, and the new daemon adopts it | **Rejected by the operator (2026-10-07).** Every agent is stopped. This also removes the per-process survival classification (systemd scope vs daemon cgroup vs session-exec owner) from the correctness path. |
| **Stop at a safe point and resume by session id** | **Adopted for every agent** that is old enough and has a resume handle. The new process continues the same conversation, so in-phase progress is kept. |
| **Kill and reset**: stop now, release the claim, requeue | For agents younger than `minResumableAgeSecs`. Little progress is lost, and the issue goes back to the queue. |
| **Requeue**: release the claim and record why | For anything that cannot be resumed (§9). Never silent. |

### The safe point

A **safe point** is a moment when the agent's session has no leaf tool call
executing. Between tool calls the only live state is the conversation, and the
runtime has already persisted that to its session store, so stopping the
process there loses nothing that `--resume` cannot restore.

The mechanism is a Loom-managed **pause hook** on the harness's pre-tool-use
event, plus a post-tool-use ledger:

1. Every daemon-dispatched agent is launched with `LOOM_DAEMON_ITEM_ID` in its
   environment. In-session and attended agents never have it, so they are never
   paused (§4).
2. The ledger hook increments a per-session in-flight count on pre-tool-use and
   decrements it on post-tool-use. Subagent container calls (`Task`/`Agent`) are
   excluded, because they stay "executing" for as long as their subagent runs.
3. At H4 the daemon writes a pause request for each item. From then on, the
   pause hook **parks** every new tool call: the call does not run. When the
   in-flight count reaches zero, the hook writes a safe-point record (the time
   and the name and summary of the parked call) that the daemon polls.
4. A parked hook waits for the daemon to stop the tree. If the hook's own
   timeout runs out first, it returns **deny** with the reason "paused for a
   daemon roll". No tool call ever runs after the safe point, and an agent that
   then ends its turn has still stopped resumably.
5. The `Stop` guard `guard-background-subagents.sh` (#6645) must let a session
   stop while a pause request is active. Its background children are about to
   be stopped with the tree anyway.

On resume, the relaunch prompt tells the agent that it was paused for a roll
(`from → to`), that its last tool call (named in the record) **did not run**,
and that it should re-run that call if it is still needed and then continue.
The harness leaves the transcript with a tool call that has no result. Claude
Code records an interrupted result. #10715 PR 1 must confirm the exact behaviour
of both runtimes (§11).

**Known cost: interrupted subagents.** A sweep's builder runs as a `Task`
subagent inside the sweep session. If the safe point lands while a subagent is
mid-task, the parent's `Task` call has no result when the session resumes. The
parent re-issues it, and the subagent starts over. Its edits are still in the
worktree, but its conversation is lost. This is still far cheaper than a
requeue. PR 1's live test measures it.

### The minimum-age rule

At H4, an item whose `agent_started_at` is less than `minResumableAgeSecs` ago
is **reset**: its tree is torn down at once, without waiting for a safe point,
and it is requeued with the reason `young-agent-reset`. `agent_started_at` is
the first start of the item's session, and it is carried across earlier roll
resumes (the lineage is in `owner.json`). A `Pending` dispatch that settles to
`Running` during H4 is always young, so it is always reset.

Config key: `autonomous.autoUpdate.pauseRoll.minResumableAgeSecs`, default
`300`. `0` disables the rule. Resolution is env > config > default, like the
other `autoUpdate` knobs.

## 3. Facts the design rests on

| Fact | Consequence | Evidence |
|---|---|---|
| Claude Code sessions can be pinned at launch (`claude --session-id <uuid>`) and resumed headless (`claude -p --resume <id>`). The transcript lives in the user's `~/.claude/projects/<encoded cwd>/`, so a resume must run in the same cwd (the worktree) under the same `HOME`. The OAuth token comes from `CLAUDE_CODE_OAUTH_TOKEN`, so the resume may use a different pool account. | The daemon can know the session id **before** launch. Resume works on any healthy account. | `defaults/scripts/spawn-claude.sh:9`, `:1108`. `spawn-claude.sh` does not pass `--session-id` today, so this is new. |
| Codex prints `session id: <uuid>` on stderr when a session starts. `spawn-codex.sh` parses it only **after** exit. The rollout file is under `$CODEX_HOME/sessions/…`, and `CODEX_HOME` is per account, so a resume (`codex exec resume <id>`) must use the same account's `CODEX_HOME`. | #10715 must capture the id while the session is running (the stderr is already tee'd to a file). A resume pins the account. If that account is unusable, the item is requeued with `session-store-unavailable`. | `defaults/scripts/spawn-codex.sh:1200-1224`, `:727` |
| Both runtimes already run Loom hooks before tool use. Claude Code has `PreToolUse` entries in `.claude/settings.json` that dispatch through `.loom/hooks/hook-wiring.sh`. Codex has Loom's managed `pre_tool_use` entry, installed by `provision-codex-hooks.sh` (`guard-codex-bridge.sh`), and Codex dispatch fails closed when that hook is not trusted. | The pause hook rides on existing wiring. The current matchers are per tool (`Bash`, …), so the pause hook needs a match-all entry. Codex needs a post-tool-use entry for the ledger, or a ledger built on the bridge's own pre/post signals. PR 1 confirms which. | `.claude/settings.json:3-31`; `defaults/scripts/provision-codex-hooks.sh:1-60` |
| A containerized session invocation is owned by the daemon (`--owner-pid "$PPID"` on the exec chain). When the owner exits, the transport revokes the invocation and the worker cancels within 2 s. The session **container** survives (`unless-stopped`). | This is no longer a hazard: every agent is stopped before the daemon exits. H4 stops a session-exec item through the `.cancel` marker after its safe point. H5 resumes it by exec'ing into the still-running container. | `defaults/scripts/spawn-codex.sh:1129`; `session_exec/owner.rs:1-3`; `session_exec/host.rs:190-196`; `session_reconcile.rs:1-50` |
| A daemon-dispatched sweep's lease renewal loop is detached and watches the **sweep's** pid. | Stopping the agent stops its renewals. H4 refreshes each paused item's lease once, and H5's resumed dispatch starts a new loop. | `sweep_registry/dispatch.rs:21`, `:3151`; `defaults/docs/lease-renewal.md` |
| Dead-pid recovery already exists. `reconstruct` removes a stale daemon-owned lock and turns a checkpoint into a `Crashed` entry. The reaper resumes it if the phase is in `RESUMABLE_CHECKPOINT_PHASES` and requeues it otherwise. Claim reconciliation reclaims a `loom:building` claim whose journal pid is dead, but only once the lease is no longer fresh. | This is the fallback for a binary that cannot read the manifest (§8). | `sweep_registry/locks.rs:96`, `:1026-1110`; `sweep_registry/reaper.rs:1163-1305`; `claim_reconciliation.rs:1-60`, `:540-551` |
| Survival across the daemon's exit differs per process. launchd children lead their own process group. systemd children in a `loom-agent-*.scope` survive. Children left in the daemon unit's cgroup are SIGKILLed by `KillMode=mixed`. | Survival no longer decides any disposition. It matters only for **teardown completeness**: anything that survives the exit must be stopped explicitly at H4 (§5). | `sweep_registry/spawn_process.rs:294`; `role_runner/launch.rs:242`; `defaults/scripts/spawn-claude.sh:354-366`; `restart_verify.rs:82-98`; `ipc.rs:131-183` |

The rows about runtime CLI flags and hook semantics were taken from the
runtimes' documented behaviour and Loom's wiring. They have not been observed
on a live host for this use. #10715 PR 1's live test must confirm them (§11).

## 4. Per-kind contract

Every row assumes H4 has already set the dispatch-pause flag (§7). "Persisted"
lists the manifest item fields from §6, beyond the common `id`, `kind`, `repo`,
`disposition`, `status` and `reason`. Every row is subject to the minimum-age
rule (§2) and the requeue rules (§9).

| Kind | Quiesce point | Persisted fields | Resume procedure (H5) | Cross-version safety check | Unresumable rule |
|---|---|---|---|---|---|
| **Daemon-dispatched sweep** (Claude Code or Codex; host process or session container) | Pause request, then the **safe point** (§2), then a tree stop (§5). Bounded by the pause budget. | `issue`, `pr`, `pid`, `pid_started_at`, `pgid`, `scope_unit`, `agent_started_at`, `resume_handle` (runtime, session id, session store, account, model, effort, cwd, container), `safe_point`, `checkpoint_phase`, `worktree`, `claim`, `lease_comment_id`, `lease_refreshed_at`, `log_path`, `overflow`, `stopped_at` | Before the work finder's first tick, relaunch through the spawn script in the recorded worktree with `--resume <session_id>` (Codex: `exec resume <session_id>`, inside the recorded container for session-exec items) and the resume prompt (§2). It gets a new registry entry (`Running`, owned `Child`, `resume_of = <old id>`), a new lease renewal loop and a new journal entry. The claim is kept. | The manifest is no older than `roll.max_age_secs`. The issue's freshest lease record is still this host's. The issue is open and not parked (dispatch guards 2.5/2.7). The worktree exists at `worktree.path` on `worktree.branch`, at `worktree.head`. The session store is reachable. For session-exec items, the container is up and its mounts match (`session_exec/host.rs:226-252`). No compatibility check before #10716 (§12, Q5). | Young: reset. Missed the pause budget: requeue. No session id captured: requeue. Any check fails, or the relaunch exits without starting the session: requeue with the matching reason. |
| **Role run** (`role_runner`: Champion, Curator, Judge, Auditor, Guide, Doctor) | Same as a sweep. New ticks are already skipped while the drain flag is set (`role_runner.rs:3245-3255`). | `role`, `root`, `pid`, `pid_started_at`, `pgid`, `scope_unit`, `agent_started_at`, `resume_handle`, `safe_point`, `timeout_remaining_secs`, `claim` (from the role's claim breadcrumb, if any), `holds_issue_creation_mutex`, `log_path`, `stopped_at` | Relaunch from the session id in `root`, as for a sweep. Seed the role runner's `InProgressGuard` with `(root, role)` so no duplicate run starts. The new run's timeout is `timeout_remaining_secs`: the roll does not count against the run's budget. If the run held the issue-creation mutex, seed the mutex as held by the resumed run (#3707). Role runs have no registry entry today (`ipc.rs:152-154`); this is the first place they are counted across a restart. | The session store is reachable, and the role is still enabled for `root` in the new config. | As for a sweep. A requeued role run releases the claim named in its breadcrumb (§9). With no breadcrumb, the role's own staleness rule (for example `LOOM_STALE_TREATING_MINUTES`) releases it. In both cases the next scheduled tick redoes the work. |
| **Epic supervisor** | Not an agent. It stops ticking (drain flag, `epic_supervisor.rs:476-480`). | Nothing of its own. Its issue-creation mutex holder is a role run (row above). | Re-derive every epic from the forge (monotone derived state, `epic_supervisor.rs:43-50`). | n/a | n/a. If the mutex holder was requeued, the mutex is free and the next tick's derivation recovers. The machine-wide filing lock (`filing_lock.rs`) is file-based and survives the restart. |
| **Worktree** (`.loom/worktrees/issue-N`) | None. It is on-disk state and is never paused or touched by the roll. | `path`, `branch`, `head`, `dirty`, carried on the owning item | Reused by the resumed agent. Until H5 completes, the worktree reaper and the orphan-process reaper must treat every worktree named in a live manifest as **owned** (`orphan_process_reaper.rs` fail-safes). | None needed. Git state does not depend on the binary version. | Never deleted by the roll. If its item was reset or requeued, it becomes ordinary stale state under the existing worktree reaper's rules. A later dispatch reuses it under `worktree.sh`'s existing rules. |
| **Pending dispatch** (spawn in progress: the child is spawned and `finish_issue_dispatch` has not recorded its entry yet. Nothing constructs `SweepState::Pending`, so the count is `sweep_registry::roll_gate`'s, #10974) | Close dispatch, then wait for it to be recorded `Running` or fail (bounded by the existing startup-race window). It is then young, so it is reset. New IPC dispatches are refused while paused (`ipc.rs:706`, `ipc.rs:1913`). | As a sweep | n/a (reset) | n/a | `young-agent-reset` |
| **In-session sweep / attended builder** (not daemon-owned) | None. It is not the daemon's work, and it has no `LOOM_DAEMON_ITEM_ID`, so the pause hook ignores it. | Not in the manifest. | Nothing to do. Two caveats: during the window, IPC-backed `loom-daemon` CLI calls fail and callers must retry. After the roll, the CLI on `PATH` is the new version. | CLI flag and IPC compatibility. That is the existing release contract, not this design. | None. |
| **Auto-update state** (settle ceiling, floor alert) | Not work. | Persisted by #10713 in `auto_update_state.json`, not in this manifest. | Loaded by #10713. | #10713's typed load outcome. | n/a |

An agent that **exits by itself** during H4, before its safe point, is not
paused. If it exited cleanly, its status is `completed`. Otherwise its status is
`exited`, and the existing crash path decides between resume and requeue, as it
would without a roll.

### Watchdog grace (#9452): no longer a prerequisite

The earlier draft required a watchdog grace for carried sweeps, because the
stale-untracked-sweep watchdog judges an *adopted* sweep by its log mtime alone
(`sweep_registry/watchdog.rs:236`). With carry gone, the roll adopts nothing. A
resumed agent is a new dispatch that the new daemon owns, so the ordinary
tracked-sweep watchdog judges it. The #9452 fix leaves #10715's scope. It is
still a bug for ordinary (non-roll) restarts that adopt live sweeps, and it is
tracked there.

## 5. Child processes and dev servers: who owns and reaps them

Finding from 2AMLogic/2am#3255 (loom-worker-1, 2026-10-07): four agent scopes
were still alive a week after their agents had exited. The only processes left
in them were orphaned vite and workerd dev servers. Process-group reaping cannot
reach them, because dev servers often `setsid` or double-fork. The orphan reaper
keys on `.loom/worktrees/issue-N` (`orphan_process_reaper.rs:1-60`), so it does
not cover role sessions, which have no worktree, or deregistered workspaces.

The contract:

1. **A child process belongs to the agent item that started it**, identified by
   that item's scope unit (systemd) or by attribution to its worktree
   (cwd/argv) plus its descendant tree (launchd). It has no disposition of its
   own. Dev servers, watchers, background shells and simulators are never
   paused, resumed or requeued by themselves. A resumed agent starts its own
   again, and the resume prompt tells it that its background processes were
   stopped.
2. **Every paused, reset or requeued item loses its whole tree at H4**, and the
   old binary owns that teardown. Nothing from an agent's tree may outlive H4.
   On systemd it is `systemctl --user stop <scope_unit>`, which kills the whole
   cgroup, including `setsid` descendants. On launchd it is the orphan reaper's
   freeze-first tree kill over the worktree-attributed and pgid-attributed
   seeds. A pgid-only `kill(-pgid)` is not enough
   (`orphan_process_reaper.rs:16-35`). For session-exec items it is the `.cancel`
   marker (`session_exec/host.rs:190-195`): the in-container worker cancels the
   invocation's tree, and the container keeps running.
3. **Order: safe point first, then teardown.** For a paused item the tree stop
   happens only after the safe-point record exists, or after the pause budget
   has run out (the item is then requeued). The agent process is stopped
   together with its children, never before them, so no child is reparented
   into a gap.
4. **H5 verifies the teardown.** For each item it checks the recorded
   `scope_unit` and the worktree attribution. Anything still alive is reaped
   and recorded as `roll.item.residue_reaped` before the item is resumed. This
   is why the manifest records `scope_unit`, which neither the lock nor the
   journal records today.
5. **Out of scope here:** the general case from 2am#3255, where an agent exits
   without any roll and its dev servers keep the scope alive. It has its own
   issue, rjwalters/loom#10802 ("General cleanup: reap agent process residue
   (dev servers, orphaned children) after any agent exit"), which specifies the
   general reaper. This design only tears down and verifies trees for items in
   the manifest (§12, Q6).

## 6. The pause manifest (schema v1)

**Path:** `$LOOM_AUTO_UPDATE_STATE_DIR/roll-pause-manifest.json`, falling back to
`~/.loom/roll-pause-manifest.json`. This is the state-dir convention already
used by `auto-update-artifact-roll.json` (`auto_update.rs:621-640`), and the
same directory as #10713's `auto_update_state.json`. It is machine-level and
spans every managed root, like `~/.loom/sweeps.json` (`sweep_journal.rs:110`).

**Write discipline:** write a temp file in the same directory, `fsync`, then
`rename`. The same rule as #10713 and #10708. At most one live manifest exists
at a time. A finished manifest is renamed to
`roll-pause-manifest.<manifest_id>.done.json`, and the newest 10 are kept for
audit.

```jsonc
{
  "schema_version": 1,                       // integer; see compatibility rules below
  "manifest_id": "rp-20261007T171500Z-4f2a", // unique per pause
  "phase": "pausing",                        // pausing | paused | resuming | resumed | abandoned
  "written_by": {
    "version": "0.19.831",                   // CARGO_PKG_VERSION of the PAUSING process
    "artifact_sha256": "…",                  // its own installed artifact identity (#9735 rollback key)
    "pid": 4123,
    "host": "host-d9142cf3",
    "supervisor": "systemd"                  // launchd | systemd
  },
  "roll": {
    "from_version": "0.19.831",
    "to_version": "0.19.840",
    "to_artifact_sha256": "…",
    "target_source": "floor",                // floor | repo_ahead | config_restart | autoupdate
    "staged_at": "2026-10-07T17:14:58Z",     // H3 entry
    "pause_started_at": "2026-10-07T17:15:00Z",
    "pause_completed_at": null,              // set when phase -> paused
    "pause_budget_secs": 120,
    "min_resumable_age_secs": 300,           // the 5-minute rule in force for this pause
    "max_age_secs": 900                      // = lease TTL at write time; age = now - pause_started_at
  },
  "items": [
    {
      "id": "sweep-issue-10714-…",           // sweep_id, or a synthetic id for a role run
      "kind": "sweep",                       // sweep | role_run
      "repo": "/Users/…/loom",               // owning workspace root
      "disposition": "resume",               // resume | requeue
      "status": "planned",                   // planned | stopping | paused | requeued | resumed
                                             //   | completed | exited | failed
      "reason": null,                        // required when disposition = requeue, or status = failed
      "issue": 10714, "pr": null,
      "pid": 51234, "pid_started_at": "…", "pgid": 51234,
      "scope_unit": "loom-agent-51230-1234.scope",
      "agent_started_at": "2026-10-07T16:40:12Z", // first start of the session (5-minute rule)
      "run_started_at": "2026-10-07T16:40:12Z",   // start of this process (differs after a resume)
      "resume_handle": {
        "runtime": "claude",                 // claude | codex
        "session_id": "4b1d…",               // null => not resumable (session-not-resumable)
        "session_store": "~/.claude/projects/…", // or the account's CODEX_HOME
        "account": "acct-3",                 // token_name / Codex account
        "model": "opus", "effort": "high",
        "cwd": "…/issue-10714",              // worktree path for sweeps, root for role runs
        "container": null,                   // {name, account} for session-exec items
        "resume_count": 0,                   // roll resumes of this session so far
        "resume_of": null                    // previous item id when this run was itself a resume
      },
      "safe_point": null,                    // {reached_at, parked_tool, parked_summary}
      "checkpoint_phase": "builder",
      "worktree": { "path": "…/issue-10714", "branch": "feature/issue-10714",
                    "head": "abc123…", "dirty": true },
      "claim": { "label": "loom:building", "on": "issue" }, // null if unknown (role run, no breadcrumb)
      "lease_comment_id": 5822662982,
      "lease_refreshed_at": null,            // set by H4's one-shot refresh
      "log_path": "…/.loom/logs/sweep-issue-10714.log",
      "overflow": false,
      "role": null, "timeout_remaining_secs": null,      // role_run only
      "holds_issue_creation_mutex": false,               // role_run only
      "stopped_at": null
    }
  ],
  "events": [                                // append-only audit trail; both processes append
    { "at": "…", "by_version": "0.19.831", "item": "…", "event": "safe_point", "detail": "parked Bash" }
  ]
}
```

**Compatibility rules.** The rollback requirement drives these: after a
rollback, an older binary must be able to read a manifest that a newer binary
may have appended to.

- **Additive within a version.** New fields are optional. Readers ignore fields
  they do not know (no `deny_unknown_fields`). Writers never repurpose a field.
- **Unknown enum values.** If a reader meets an unknown `kind`, `disposition`,
  `status` or `resume_handle.runtime`, it treats the item as `requeue` with
  reason `unknown-<field>-<value>` and records that. It never drops the item
  silently.
- **Newer `schema_version` than the reader knows.** The reader resumes nothing
  from the manifest. Every item is left to today's restart recovery, which H4
  set up for exactly this case (§8). The reader emits
  `roll.manifest.unreadable`.
- **Bump `schema_version` only for a change that breaks those rules.**
- **Fields the rolled-back binary must be able to read** (the frozen v1 core):
  `schema_version`, `manifest_id`, `phase`, `written_by.version`,
  `roll.to_version`, `roll.to_artifact_sha256`, `roll.max_age_secs`, and on
  every item `id`, `kind`, `repo`, `disposition`, `status`, `reason`, `issue`,
  `pid`, `pid_started_at`, `pgid`, `scope_unit`, `agent_started_at`,
  `resume_handle.runtime`, `resume_handle.session_id`,
  `resume_handle.session_store`, `resume_handle.cwd`, `resume_handle.container`,
  `worktree.path`, `claim`, `lease_comment_id`.
- **Load outcomes are typed**, mirroring #10713: `Loaded | Missing | Corrupt |
  UnknownVersion | Stale`. Every outcome other than `Loaded` logs once and falls
  back to plain restart recovery. None of them panics.

## 7. State transitions: H3 Staged, H4 Pausing, H5 Verifying

The host states come from #10698's state machine. Every timeout below is a
config key under `autonomous.autoUpdate.pauseRoll.*` (env > config > default,
matching the other `autoUpdate` knobs).

### H3 Staged

- **Entry:** the binary on disk equals the pinned target (#10709) and the running
  `CARGO_PKG_VERSION` is below it (#10710). Every `target_source` enters here:
  `floor`, `repo_ahead`, `config_restart` and `autoupdate`.
- **Exit to H4:** immediately, in the same tick, if all of these hold: a
  supervisor is detected (`daemon_update/supervisor.rs`), with the same
  precondition `handle_drain_request` already enforces for relaunch drains
  (`ipc/drain_supervisor.rs:44-75`); no operator drain or operator stop is
  active; no manifest for this host is still being resumed.
- **Failure edges:**
  - No supervisor: go to H7 `unsupervised` and alert. Nothing would relaunch
    the daemon.
  - An operator then-exit drain is active: it wins (as #4521 does today). The
    host stops, and the next start resumes from the manifest as an ordinary
    H5.
  - There is no drain fallback for any `target_source`. A roll that cannot
    proceed (no supervisor, pause failure) goes to H7 and alerts. It never waits
    for the in-flight count to reach zero.

### H4 Pausing

- **Entry:** from H3. Set the shared dispatch-pause flag (`ipc/drain_state.rs:443`)
  with a new `DrainOrigin::PauseRoll` (`ipc/drain_state.rs:65`). The work finder
  (`work_finder.rs:3102`), the role runner (`role_runner.rs:3248`), the epic
  supervisor (`epic_supervisor.rs:480`) and IPC dispatch (`ipc.rs:706`) all
  already honour this flag. Open the paused-time ledger (`ipc/drain_ledger.rs`).
- **Steps, in order:**
  1. Wait for `Pending` dispatches to settle (bounded by the startup-race
     window).
  2. Snapshot every in-flight agent from every managed root (the same root walk
     as `count_in_flight_sweeps`, `ipc.rs:285`) plus every role run. Classify
     each one. If it is younger than `minResumableAgeSecs`, it is `requeue`
     with `young-agent-reset`. If it has no session id, it is `requeue` with
     `session-not-resumable`. Otherwise it is `resume`.
  3. **Write the manifest with `phase = pausing`, before any agent is signalled
     or stopped.** If the process dies after this point, the next start knows
     everything that was in flight.
  4. Tear down the `requeue` items at once (§5), and mark them `stopped_at`.
  5. Raise the pause request for the `resume` items (§2) and mark them
     `stopping`. Poll for safe-point records. When an item's record appears,
     stop its tree (§5), and record `safe_point`, `stopped_at` and
     `status = paused`. An item that exits by itself is `completed` or `exited`
     (§4).
  6. Refresh the lease of each `paused` item once, and set `lease_refreshed_at`.
     This gives H5 a full lease TTL.
  7. **Budget deadline.** Any `resume` item still `stopping` when
     `pauseBudgetSecs` runs out is torn down and converted to `requeue` with
     `pause-budget-missed`.
  8. Do the requeue forge writes (§9) for every `requeue` item. Each `gh` call
     is bounded by `reap_gh_timeout` (`sweep_registry/reaper.rs:90`).
  9. **Do not remove** the lock (`owner.json`), journal entry or checkpoint of
     any `paused` item. They are the fallback for a binary that cannot read the
     manifest (§8). Rewrite the manifest with `phase = paused` and set
     `pause_completed_at`.
  10. Exit `EXIT_RESTART` through the drain supervisor's existing exit path
      (`ipc/drain_supervisor.rs:562`), so the supervisor relaunches the daemon
      onto the staged binary.
- **Exit condition:** step 10. It does **not** depend on the in-flight count.
  When the daemon exits, no agent process is running.
- **Timeout:** `pauseBudgetSecs`, default **120 s**. It covers the safe-point
  wait (steps 4-7). The forge writes in steps 6 and 8 get the remainder of the
  budget, with a floor of 30 s. Every budget in this section is reported in the
  `daemon.roll.item` events and in `status --json`, next to the observed
  durations, so the defaults can be tuned (Q4). A forge write not done in that time stays in
  the manifest as `status = planned` and H5 finishes it. A slow forge must not
  block the roll. Under stop semantics the budget sets the trade-off: any tool
  call that runs longer than the budget (a long build or test run, for example)
  makes its agent miss the safe point, and that agent is requeued. The default is
  a starting value, tuned from the `pause-budget-missed` counts (§12, Q4).
- **Failure edges:**
  - The manifest write in step 3 fails (disk full, permissions): **abort the
    pause.** Nothing has been signalled yet, so clear the flag, resume
    dispatch, go to H7 `pause-failed`, and alert. Never stop work that has not
    been recorded.
  - The process dies inside H4: the supervisor relaunches it, normally still on
    the old binary if the swap has not happened. The new process finds
    `phase = pausing` and treats the manifest as authoritative. It finishes the
    steps that were not done. Items still `stopping` whose pid is alive go
    through steps 5-7 again. Items whose pid is dead without a safe-point
    record are requeued with `pause-budget-missed`.
  - An operator `--abort-drain` before step 4: same as a manifest-write
    failure, but delete the manifest. From step 4 on, the abort is refused,
    because work has already been stopped; the roll completes. One exception:
    if the abort arrives during step 5 and **no** item has been stopped yet, the
    pause requests are withdrawn (parked hooks return allow), and the abort is
    honoured.

### H5 Verifying

- **Entry:** process start finds a manifest with `phase` of `paused`, `pausing`
  or `resuming`. The new process holds dispatch paused (the flag is set at
  startup whenever a live manifest exists) until H5 exits. It also suppresses
  `reconstruct`'s crash recovery and claim reconciliation's dead-pid reclaim
  for the manifest's items, so the fallback path does not race the resume.
- **Steps, in order:**
  1. Load the manifest (typed outcome, §6). If its age (now minus
     `roll.pause_started_at`) exceeds `roll.max_age_secs`, the outcome is `Stale`: downgrade every `resume` item
     to `requeue` with reason `manifest-stale`. If `phase = pausing`, finish H4
     first: no agent is running any more, so every `stopping` item without a
     safe-point record becomes `requeue` with `pause-budget-missed`.
  2. Verify the teardown and reap any residue (§5).
  3. Startup health, owned by #9735: the running version equals
     `roll.to_version`, IPC is responsive, and the heartbeat is sustained for
     `verifyProbationSecs` (default **90 s**, matching
     `DEFAULT_STARTUP_GRACE_SECS`, `daemon_install_state.rs:118`). Nothing is
     relaunched before this passes.
  4. On a pass: set `phase = resuming`. Relaunch the `resume` items on this host
     in manifest order, before the work finder's first tick, each after the
     checks in §4. An item is `resumed` once its new process has started the
     saved session and its new lease renewal loop is running. Finish any requeue
     forge writes that H4 left as `planned`.
  5. Mark the manifest `phase = resumed`, archive it, clear the dispatch flag
     and the recovery suppression, and go to H0.
- **Timeout:** `verifyProbationSecs` for step 3. Steps 4-5 are bounded by
  `resumeBudgetSecs` (default 120 s). Items still unresumed at that deadline
  are requeued with reason `resume-timeout`.
- **Failure edges:**
  - Health fails: rollback (#9735, §8), quarantine `roll.to_artifact_sha256`, go
    to H7. The manifest is not modified except for an appended `events` entry,
    so the rolled-back binary resumes it.
  - The process crashes during step 4: the next start sees
    `phase = resuming`. Items already marked `resumed` are not relaunched.
    An item whose relaunch had started but was not yet marked is checked by
    session: if a live process is running that session (identity-paired pid
    from the new lock), it is marked `resumed`; otherwise it is relaunched.
    Idempotency comes from per-item `status` plus the dispatch guards' existing
    live-claim checks (`sweep_registry/locks.rs:812`).
  - A relaunch fails (the session id is unknown to the store, or the harness
    exits before the session starts): requeue with `session-resume-failed`.
    `resume_count` is capped at 3 roll resumes per session. Past the cap the
    item is requeued with `resume-attempts-exhausted`.

**Bound on the window.** The worst case from the H4 lease refresh to the
end of resume relaunches is the rest of the pause budget, plus the supervisor
relaunch, plus probation, plus the resume budget: about 120 + 10 + 90 + 120 ≈
340 s by default, well under the 15 min lease TTL. Manifest age is measured from
`roll.pause_started_at`, which precedes every lease refresh, so the check is
conservative. Config validation must reject any combination of
`pauseBudgetSecs`, `verifyProbationSecs` and `resumeBudgetSecs` whose sum
reaches 2/3 of the lease TTL.

## 8. Rollback (#9735)

**Contract: paused work resumes on whichever binary passes health, if that
binary can read the manifest. A binary that cannot read it still releases or
recovers every claim.**

- #9735 owns readiness, the rollback mechanics and quarantine. #9735 is open
  (`loom:triage`) and depends on #9734. Until it lands, an H5 health failure
  leaves the host wherever the existing supervisor and restart-verify behaviour
  (`restart_verify.rs`) leave it. The manifest stays in place, so whichever
  binary next starts successfully runs H5.
- **Rollback to a binary at or after #10715.** The rolled-back binary is
  normally the one that **wrote** the manifest (`written_by`), so it reads its
  own schema. The compatibility rules in §6 cover the case where the new binary
  appended `events`, or where rollback lands on an even older binary, such as
  the last healthy version after a quarantine chain. It runs H5 normally, and
  agents resume from their sessions. Resume relaunches the runtime CLI (Claude
  Code or Codex), which does not depend on the daemon version. H5 compares
  `running_version` with `roll.to_version`. When they differ, it records
  `resumed_on = rollback` in `events`, and the `roll.paused.resumed` telemetry
  carries both versions.
- **Rollback to a binary older than #10715** (it does not know the manifest
  exists). Every paused agent is a **stopped-not-running** agent: its pid is
  dead, and its lock, journal entry and checkpoint are intact, because H4
  step 9 left them so. To that binary it looks exactly like a daemon-owned
  sweep that crashed, and today's recovery handles each claim:
  - **Sweep with a checkpoint.** `reconstruct` removes the stale lock and
    records the issue as daemon-owned and dead. It then turns the checkpoint
    into a `Crashed` entry (`sweep_registry/locks.rs:1026-1110`). The reaper
    resumes it (a fresh session that starts from the checkpoint, keeps the
    claim, and counts toward `MAX_RESUME_ATTEMPTS`) if the phase is in
    `RESUMABLE_CHECKPOINT_PHASES`. Otherwise it requeues it through
    `restore_label_to_ready` (`sweep_registry/reaper.rs:1163-1305`,
    `guards.rs:1938`).
  - **Sweep without a checkpoint** (an early phase). No registry entry is
    created, and its `loom:building` claim stays on the issue. Claim
    reconciliation finds the journal entry with a dead pid and decides
    `Reclaim(DeadPid)`, on both the startup and the periodic pass. The lease
    gate holds that reclaim until the lease is no longer fresh, which is at
    most one lease TTL (15 min) after H4's refresh. The claim is then released
    and the issue requeued (`claim_reconciliation.rs:1-60`, `:540-551`). The
    delay is bounded, and nothing is stranded.
  - **Role run.** There is no registry or journal entry for it. A claim it
    took (`loom:reviewing`, `loom:treating`, …) is released by that role's own
    staleness rule, and the next tick redoes the work. This is slower than a
    sweep, bounded by the role's staleness threshold, and is the same as a role
    run that crashes today.
  - **Requeued and reset items.** The old binary already did their forge
    writes at H4 (step 8), so there is nothing left to recover. Any write that
    H4 left as `planned` is picked up by the paths above: a lock and journal
    entry with a dead pid are still in place for that item.
  - **Containerized sessions.** Their invocations were cancelled at H4, and the
    container is still up. They recover like any other sweep, as above.
  - **What is lost:** the same-session resume (in-phase conversation progress)
    and the audit trail. Worktrees and session transcripts stay on disk.
  - **A later binary that can read the manifest** may start after that
    recovery has run. It will then find a stale or already-handled manifest.
    Every H5 action re-checks the live state first: is the lease still this
    host's, is the claim still held, is the issue still open. So it does not
    double-dispatch, and `restore_label_to_ready` is idempotent. The manifest is
    archived with `phase = abandoned`.
- **Requirement on #10715, so that this holds:** H4 must never delete a paused
  item's lock, journal entry or checkpoint. H5 alone removes them, after the
  item is resumed or requeued. PR 3's test suite includes a pre-#10715 binary
  (or a test double of its recovery passes) recovering every claim from a
  paused host (§11).

## 9. Requeue-and-record rules

No item leaves the manifest without a terminal `status`. Any status other than
`resumed` or `completed` also needs a `reason`. Each requeue does all three of
the following:

1. **Label.** For an issue sweep: `restore_label_to_ready` (`loom:building` →
   `loom:issue`, `sweep_registry/guards.rs:1938`). This keeps the #9463 rule
   (`sweep_registry/restore_to_ready.rs`) that a closed issue is never
   re-queued, and the #4206 park check. For a role run: release the label named
   in its `claim` breadcrumb. The breadcrumb is new: the role writes it when it
   takes a claim label (§11, PR 3). With no breadcrumb, the role's own
   staleness rule releases the claim, and the record below is still written.
   The sweep checkpoint is kept, so a requeued sweep's next dispatch skips the
   phases it already finished.
2. **Comment.** One forge comment on the issue, or on the PR if the item has
   one. It names the roll (`from → to`), the item's phase and age, the reason,
   and whether a worktree with uncommitted edits remains on the host. The
   issue's own lease record is left alone: it ages out once nothing renews it.
3. **Telemetry.** A `daemon.roll.item` event on the event bus with
   `{manifest_id, item_id, kind, runtime, disposition, status, reason,
   agent_age_secs, from_version, to_version}`, plus a counter by reason in
   `status --json`.

Reasons are a closed set:

| Reason | Meaning |
|---|---|
| `young-agent-reset` | the agent had run less than `minResumableAgeSecs` at H4 (§2), so it was killed and reset |
| `pause-budget-missed` | the agent had not reached a safe point when `pauseBudgetSecs` ran out (or its pid died before one) |
| `session-not-resumable` | no session id was captured for the agent, or its runtime has no resume support |
| `session-store-unavailable` | at H5 the session store (`~/.claude/projects/…`, or the account's `CODEX_HOME`) or the pinned account could not be used |
| `session-resume-failed` | the relaunch with the saved session id failed to start the session |
| `resume-attempts-exhausted` | the session had already been roll-resumed 3 times (`resume_count`) |
| `manifest-stale` | H5 loaded the manifest after `roll.max_age_secs`; the claim may already have been reclaimed elsewhere |
| `lease-lost` | the issue's freshest lease record is no longer this host's |
| `issue-closed` | the issue closed during the window (#9463: it is never requeued, only recorded) |
| `issue-parked` | the issue gained `loom:blocked` or `loom:operator-only` |
| `worktree-changed` | the recorded worktree is missing, on another branch, or at an unexpected HEAD |
| `session-down` | the session container refused the resumed exec |
| `role-disabled` | the role is no longer enabled for its root in the new config |
| `guard-refused:<step>` | a dispatch guard refused the resume; `<step>` names it |
| `resume-timeout` | the item was still unresumed when `resumeBudgetSecs` ran out |
| `unknown-<field>-<value>` | the reader did not recognise an enum value (§6) |

## 10. Survey: existing machinery, reuse vs new

| Machinery | Where | Use in this design |
|---|---|---|
| Dispatch-pause flag and its producers | `ipc/drain_state.rs:28`, `:443`; `work_finder.rs:3102`; `role_runner.rs:3248`; `epic_supervisor.rs:476-480`; `ipc.rs:706` | **Reuse as is.** H4 and H5 hold dispatch with this flag. |
| `DrainOrigin` (Operator / AutoUpdate) | `ipc/drain_state.rs:65` | **Extend.** Add `PauseRoll`, whose completion condition is "every item stopped or requeued, manifest written" instead of "in-flight == 0". It is the origin for every automatic roll. The operator origin and its then-exit drain are unchanged. |
| Drain supervisor, exit codes, supervisor detection, then-exit precedence | `ipc/drain_supervisor.rs:44`, `:308`, `:498-562` | **Reuse.** The exit path and the then-exit escalation (#4521) are unchanged. |
| Wait-for-zero roll policy (#6007 re-arm, abandon budget) and stall suppression | `ipc/drain_roll.rs:32-45`, `:139`; `auto_update/roll_stall.rs:127`, `:167` | **Replaced.** No roll waits for zero any more (Q3). Pause-and-roll's bounded budgets and H7 alerts take over the job of the abandon budget and stall suppression. Remove it (§11, PR 2). |
| Roll trigger trait | `auto_update/drain_trigger.rs:22` (`DrainTrigger`), `:123` (`IpcDrainTrigger::trigger`) | **Replace** the drain trigger with `trigger_pause_roll(target)`, used by every `target_source`. Supersede (#8514) keeps working: a newer target during H3 replaces the old one, and once H4 has started it is too late to supersede. |
| Claude Code launch | `defaults/scripts/spawn-claude.sh` | **Extend.** Pin `--session-id` (a uuid generated at dispatch and recorded in `owner.json`). Add the resume launch mode (`--resume <id>` plus the resume prompt). Export `LOOM_DAEMON_ITEM_ID`. |
| Codex launch | `defaults/scripts/spawn-codex.sh:1200-1224` | **Extend.** Capture `session id:` while the session runs and write it into the item's handle file. Add the resume mode (`exec resume <id>`) with the account's `CODEX_HOME`. |
| Hook wiring (Claude `PreToolUse`; Codex managed `pre_tool_use`) | `.claude/settings.json`; `.loom/hooks/hook-wiring.sh`; `defaults/scripts/provision-codex-hooks.sh` | **Extend.** Add a match-all pause hook and the post-tool-use ledger (§2). Hook timeouts must exceed the park window. |
| Background-subagent `Stop` guard | `defaults/hooks/guard-background-subagents.sh` (#6645) | **Change.** Allow the stop while a pause request is active. |
| Lock-based restart adoption and journal | `sweep_registry/locks.rs:865` (`reconstruct`), `:1163`; `startup_adoption.rs:112`; `sweep_journal.rs` | **Reuse as the fallback** (§8). H5 suppresses it for manifest items while the resume runs. |
| Sweep checkpoints and crash-resume | `.loom/sweep-checkpoint/issue-N.json` (#3373); `sweep_registry/dispatch.rs:1568`; `locks.rs:96`; `reaper.rs:48` | **Reuse** as the fallback, and to give requeued sweeps a head start. The roll's own resume is by session, not by checkpoint. |
| Requeue and claim restore | `sweep_registry/guards.rs:1938`; `sweep_registry/restore_to_ready.rs` | **Reuse.** |
| Lease records and renewal | `defaults/docs/lease-record.md`; `defaults/docs/lease-renewal.md`; `sweep_registry/dispatch.rs:3151`; TTL at `claim_reconciliation.rs:551` | **Reuse.** **New:** a one-shot refresh at H4 for paused items. |
| Startup and periodic claim reconciliation (dead pid → reclaim, gated on lease freshness) | `claim_reconciliation.rs`; `daemon_startup_reconciliation.rs` | **Reuse as the backstop** for anything the manifest path misses, and for the pre-#10715 rollback (§8). **Extend:** skip manifest items while H5 is running. |
| Stale-untracked-sweep watchdog | `sweep_registry/watchdog.rs:236`, `:380-400` | **Unchanged by #10715.** Resumed agents are tracked dispatches. #9452 remains a separate fix (§4). |
| Orphan-process reaper (worktree-attributed, freeze-first tree kill) | `orphan_process_reaper.rs:1-60` | **Reuse** for tree teardown on launchd. **Extend** its fail-safes so a worktree in a live manifest counts as owned. |
| Agent scopes (`loom-agent-*.scope`, `loom-agents.slice`) | `defaults/scripts/spawn-claude.sh:340-490` | **Reuse.** The scope is the teardown unit on systemd. **New:** record the scope unit in `owner.json` at dispatch. |
| Session-exec transport (lease, owner, cancel marker) | `session_exec.rs`; `session_exec/owner.rs`; `session_exec/host.rs:184-260` | **Reuse.** The `.cancel` marker stops a session-exec item after its safe point. Ownership is unchanged. |
| Session container reconcile | `session_reconcile.rs` | **Reuse unchanged.** It never interrupts work, and the containers survive, so H5 can exec into them. |
| Role-run admission guard | `role_runner.rs:2378-2420` | **Extend.** Seed it from the manifest for resumed role runs. |
| Issue-creation mutex (in memory) | `issue_creation_mutex.rs:137` | **Extend.** Seed it from the manifest when its holder is resumed. |
| Relaunch verification and supervisor self-heal | `restart_verify.rs`; `daemon_update/restart_flow.rs`; `daemon_update/supervisor.rs`; `daemon_update/relaunch.rs` | **Reuse** for H4→H5. |
| Persisted update state (state dir, atomic write, typed load) | `auto_update.rs:600-640` (`ArtifactRollRecord`); #10713 | **Reuse the pattern and directory.** |
| Paused-time ledger | `ipc/drain_ledger.rs` | **Reuse.** Open it at H4 and close it at the end of H5. |
| Rollback, readiness and quarantine | #9735 (open), #9734 (open) | **Dependency.** §8 says what happens before they land. |

**Net new:** the manifest module; the H4 classifier and orchestrator; the
safe-point pause hook and tool-call ledger for both runtimes; session-id pinning
(Claude) and live capture (Codex); the resume launch mode in both spawn scripts;
the H5 consumer; `DrainOrigin::PauseRoll`; the guard and mutex seed points; the
role claim breadcrumb; `scope_unit` and the session id in `owner.json`; and the
`daemon.roll.*` events.

## 11. Step 3 touch list (#10715)

The scope is large, so #10715 should be curated as **three PRs in order**. Each
PR may merge once the Judge approves it, one at a time, under the operator's
2026-10-07 ruling on #10698.

**PR 1: resume handles, the safe-point hook, and the manifest (no roll
behaviour change).** This PR proves that the mechanism works before anything
depends on it.

- `defaults/scripts/spawn-claude.sh`: `--session-id` pinning, `LOOM_DAEMON_ITEM_ID`,
  and the resume launch mode.
- `defaults/scripts/spawn-codex.sh`: live session-id capture and the resume
  launch mode (host process and session-exec).
- `loom-daemon/src/sweep_registry/locks.rs`, `dispatch.rs`, `mod.rs`: record
  `scope_unit`, `agent_started_at` and the resume handle in `owner.json`. Add an
  in-flight snapshot API for the classifier.
- New pause hook and ledger (`defaults/hooks/`), wired for Claude
  (`.claude/settings.json`, `hook-wiring.sh`) and Codex
  (`provision-codex-hooks.sh`). Inert unless a pause request exists for the
  agent's `LOOM_DAEMON_ITEM_ID`.
- New `loom-daemon/src/auto_update/pause_manifest.rs`: types, typed load
  outcome, atomic save, compatibility rules (§6).
- New `loom-daemon/src/auto_update/pause_classify.rs`: pure classification
  from `{kind, age, resume handle}` to a disposition and reason, including the
  `minResumableAgeSecs` boundary.
- Tests:
  - **Live stop-and-resume test, required for PR 1 acceptance:** one Claude Code
    session and one Codex session (inside a session container), each started
    headless through the spawn scripts on a scripted multi-tool task. Each one:
    - gets a pause request mid-task;
    - is confirmed to park at a safe point, with no tool executing;
    - has its process tree stopped, with a `setsid`'d dummy dev server confirmed
      gone;
    - is resumed by session id in the same cwd.

    The test then asserts that the resumed session (a) recalls a nonce from
    before the pause, (b) re-runs the parked tool call exactly once, and
    (c) completes the task. It also records the hook-timeout behaviour, the
    dangling-tool-call handling of each runtime, and the subagent restart cost
    (§2). It needs real credentials, so it is an opt-in script
    (`scripts/tests/test-roll-pause-resume-live.sh`), run on the gate host, with
    its output attached to the PR.
  - Hermetic tests of the hook and ledger with a fake harness: park, deny on
    hook timeout, excluded `Task` calls, in-session agents unaffected.
  - Manifest round-trip; unknown fields ignored; unknown enum values become
    requeue; a newer `schema_version` gives `UnknownVersion`; corrupt, missing
    and stale manifests.
  - The classification matrix: the 5-minute boundary, a missing session id,
    sweep and role run, Claude and Codex.

**PR 2: H3/H4 pause (old-binary side).**

- `loom-daemon/src/auto_update.rs`: route every target (`floor`, `repo_ahead`,
  `config_restart`, `autoupdate`) to the pause path.
- `loom-daemon/src/auto_update/drain_trigger.rs`: replace the drain trigger
  (`DrainTrigger`, `IpcDrainTrigger::trigger`) with `trigger_pause_roll`.
- **Remove the replaced wait-for-zero machinery:** `loom-daemon/src/ipc/drain_roll.rs`
  (the #6007 re-arm and abandon budget) and
  `loom-daemon/src/auto_update/roll_stall.rs` (stall suppression), plus their
  call sites and config keys. Keep the operator drain and its then-exit path
  (`ipc/drain_supervisor.rs`) unchanged.
- `loom-daemon/src/ipc/drain_state.rs`, `ipc/drain_supervisor.rs`:
  `DrainOrigin::PauseRoll` and its completion condition.
- `loom-daemon/src/auto_update/pause_roll.rs` (new): H4 steps 1-10, the budget,
  and the one-shot lease refresh.
- `loom-daemon/src/sweep_registry/reaper.rs`, `orphan_process_reaper.rs`: tree
  teardown (scope stop, freeze-first kill, the `.cancel` marker).
- `loom-daemon/src/sweep_registry/guards.rs`, `restore_to_ready.rs`: requeue
  with a reason, plus the comment.
- `loom-daemon/src/role_runner.rs`, `issue_creation_mutex.rs`: export role-run
  and mutex-holder snapshots.
- `defaults/hooks/guard-background-subagents.sh`: honour an active pause request.
- Tests:
  - Integration with fake agents (`sweep_registry/test_support.rs`): a pause
    with a paused item, a young item that is reset, and an item that misses the
    budget and is requeued.
  - After H4, no process from any item's tree is alive, and every paused
    item's lock, journal entry and checkpoint is still present.
  - A manifest write failure aborts the pause with nothing signalled.
  - A forge write that runs past the budget is deferred to H5.
  - A then-exit drain wins.
  - The operator-drain tests stay green, unchanged:
    `loom-daemon/tests/integration_drain_then_exit.rs`,
    `integration_drain_exit_then_watchdog_recovers.rs`,
    `src/ipc/drain_state_tests.rs`. Tests that cover only the removed
    `drain_roll` and `roll_stall` behaviour are deleted, and
    `src/auto_update/tests/supersede_tick.rs` is adapted to the pause trigger.

**PR 3: H5 resume (new-binary side) and rollback safety.**

- `loom-daemon/src/daemon_service.rs`: load the manifest before
  `spawn_startup_passes` and `seed_capacity_from_journal`; hold the dispatch
  flag and suppress fallback recovery for manifest items while a manifest is
  live.
- New `loom-daemon/src/auto_update/pause_resume.rs`: H5 steps 1-5, the teardown
  check, relaunch by session, `resume_count`, archiving.
- `loom-daemon/src/sweep_registry/dispatch.rs`: the resume-dispatch entry point
  (session id, same worktree, claim kept, lease checked, `resume_of` lineage).
- `loom-daemon/src/role_runner.rs`, `issue_creation_mutex.rs`: resume role runs
  with the remaining timeout, and seed the guard and the mutex. Role claim
  breadcrumb: written when a role takes a claim label, and read by requeue.
- `loom-daemon/src/claim_reconciliation.rs`, `sweep_registry/locks.rs`: skip
  manifest items while H5 runs.
- `loom-daemon/src/orphan_process_reaper.rs`, `worktree_reaper.rs`: worktrees in
  the manifest count as owned.
- `loom-daemon/src/types.rs`, `ipc.rs` status: the pause/resume state in
  `status --json`.
- Tests:
  - Resume of paused items (sweep and role run, Claude and Codex fakes), and
    requeue of each H5 reason.
  - A crash during H4 (`phase = pausing`).
  - A crash during H5 (`phase = resuming`, no double launch).
  - A stale manifest.
  - A rolled-back binary at or after #10715 resumes the manifest.
  - **A pre-#10715 binary (or a test double of its recovery passes) recovers
    every claim from a paused host:** checkpoint sweeps through
    `reconstruct` and the reaper; checkpoint-less sweeps through claim
    reconciliation once the lease ages out; and a later manifest-aware start
    does not double-dispatch.

**Not in #10715:** the H5 health and rollback mechanics (#9735); general
agent-exit residue reaping (rjwalters/loom#10802); the #9452 watchdog fix for
ordinary restarts (§4).

## 12. Operator questions (all answered)

1. **Carry as "pause".** **Answered 2026-10-07: no carry.** Pause means every
   agent is brought to a resumable stop at a safe point and resumed from its
   saved session after H5. Agents running less than 5 minutes are killed and
   reset. Anything unresumable is requeued and recorded. Reflected in §1, §2,
   §4 and §7.
2. **Containerized Codex sessions.** **Answered 2026-10-07: same path, no
   special case.** They pause at a safe point and resume by session id inside
   the still-running container. Session-exec ownership is unchanged, because
   the agent is stopped before the daemon exits anyway.
3. **Ordinary autoUpdate rolls.** **Answered 2026-10-07: unify.** Ordinary
   autoUpdate rolls also use pause-and-roll. There is no separate wait-for-zero
   drain path. Floor, repo-ahead, restart-only-config and autoUpdate rolls all
   share one H3→H4→H5 machine. The old `drain_roll` and `roll_stall` machinery is
   replaced and removed (§10, §11 PR 2).
4. **Defaults.** **Answered 2026-10-07: starting values.** Pause budget 120 s,
   verify probation 90 s, resume budget 120 s, manifest max age equal to the
   lease TTL (15 min), and `minResumableAgeSecs = 300` ship as proposed. All are
   configurable and are reported in telemetry, and are tuned from experience
   (§7, §9). The `pause-budget-missed` counts are the main signal.
5. **Compatibility before #10716.** **Answered 2026-10-07: transition breakage
   is acceptable.** Assume compatibility until #10716 lands. This design adds
   no machinery to guard the transition (§1, §4).
6. **Dev-server residue (2am#3255).** **Answered 2026-10-07: yes, its own
   issue.** General cleanup of leftover agent processes is
   rjwalters/loom#10802. This design covers only the manifest items' trees
   (§5).
7. **Role runs.** **Answered 2026-10-07: same rule as every other agent.** Role
   runs pause at a safe point and resume from their session, with the guard,
   the remaining timeout and the issue-creation mutex seeded. They are subject
   to the 5-minute rule and the requeue rules.

## 13. When a roll starts: the floor drives, no roll windows (#10885)

Operator ruling, 2026-10-08. With H3→H4→H5 in place a roll is a short pause, a
restart and a resume. The roll window (#9132) existed because drain-based
rolls were expensive, so it is removed, along with its per-host offset. This
section records what replaced it. It does not change the roll machine above.

**Every fleet host has a floor.** `loom_min_version` is a required field of
the fleet store. On startup and on every tick the daemon compares it with the
version it is running and acts on that tick, through `trigger_pause_roll`.

| Floor knowledge | Running vs floor | Behaviour |
|---|---|---|
| no fleet store (not a fleet host) | n/a | Opt-in autoUpdate, unchanged apart from the window: artifact path and source path behind the settle gate and its `6 × settleSecs` ceiling. `target_source = autoupdate`. |
| unknown | n/a | No roll for the floor. The tick's note says the floor is not known and why. Fail closed: a host that may have a floor must not chase the latest release. A workspace that needs a newer daemon (`repo_ahead`, below) still rolls the host, to a real published release. |
| set | below; the newest release is at or above the floor | Floor roll on this tick. No settle. The target is the newest release at or above the floor, at its exact tag. |
| set | below; the newest release is below the floor | The `FloorStallReport` ERROR alert; dispatch continues. The host does not roll to the newest release instead. |
| set | below; no release resolved this tick | No roll; the next tick asks again. The source-rebuild path is not used. |
| set | at or above | No roll, whatever newer release, re-published same-version artifact or newer source HEAD exists. Nothing is tracked, so no settle clock accumulates. |
| set, but a version does not parse | n/a | No version roll, one WARN. Not reachable with a release build. |

Consequences:

- **A fleet host moves only when the floor moves**, or on the two triggers
  that are independent of this table and act in every row: a repo ahead of
  this daemon (`repo_ahead`, #10719) and a restart-only config change
  (`config_restart`, #10720). Neither has a producer in this change.
- **Settle, the settle ceiling (#10418), chase-latest and automatic source
  rebuilds do not exist for a fleet host.** They remain only for a host with
  no fleet store.
- **"Unknown" is three-valued on purpose.** Before #10885 "no store", "a
  store with no floor" and "no pass has completed yet" all read as "no floor",
  which then meant "chase latest". They are now `NoStore`, `Unknown` and
  `Unknown`. A store without the field, a startup pass that hit its cap with
  no earlier snapshot, and a store that could not be started are all
  `Unknown`. Before the first pass of a new process completes, the floor the
  previous process recorded counts as set.
- **The floor is a lower bound, not a pin.** A host below it installs the
  newest release at or above it. A release counts only once it publishes this
  platform's binary and checksum; a tag with no assets is never a target.
  The release a floor bump installs may therefore be minutes old, which is why
  rollback (#9735, §8) follows the resume side.
- **A floor change is acted on at once.** The fleet-sync pass that resolves a
  floor different from the previous pass's wakes the self-update loop, so the
  worst case is one `fleet.syncIntervalSecs`, not that plus
  `autoUpdate.intervalSecs`. The wake fires on a change of value only: a floor
  roll that keeps failing is paced by its backoff and by the failed-roll guard
  in H3 (§8), not by the sync cadence.
- **No jitter.** Hosts already tick at different phases, a roll is a short
  pause, and the artifact fetch is a handful of hosts against the release CDN.
  A floor bump rolls every host within about one sync interval, and that is
  accepted. Two hosts that roll minutes apart across a new release can land on
  different versions, both at or above the floor.
- **Old settings and state.** `rollWindowSecs`, `rollWindowOffsetSecs`,
  `launchdLiveReload` and their env vars are accepted and ignored with one
  WARN at startup. `auto_update_state.json` stays at schema 1: an older
  record's `window` object is ignored on load and dropped on the next write,
  and a binary from before this change reads the new record (its `window`
  defaults to absent), so a rollback keeps the settle clocks.
- **Unchanged.** The self-update loop, and so the floor check, still runs
  only when `autonomous.autoUpdate.enabled` is true.
