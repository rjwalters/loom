# Daemon Roll: Pause-and-Resume Contract for In-Flight Work (D2)

Design spike for #10714, part of tracker #10698 (Phase 2, step 2). It implements
operator decision **D2** (2026-10-07): a floor-driven or repo-ahead roll does not
wait for in-flight work to reach zero. The daemon pauses its in-flight work,
restarts onto the new binary, and the new process resumes that work.

**Status: proposal. The operator must review it before #10715 (implementation)
is curated.** No runtime behaviour changes until #10715 lands.

This is a repo-local design doc (it is on the `ORPHAN_ALLOWLIST` in
`scripts/check-docs-defaults-parity.sh`). It is never installed into consumer
repos.

Citations are `path:line` against `main` at `74721c1c4` (2026-10-07). Paths
without a prefix are under `loom-daemon/src/`.

**Contents**

- [1. Summary of decisions](#1-summary-of-decisions)
- [2. What "pause" means](#2-what-pause-means)
- [3. Survival facts the design rests on](#3-survival-facts-the-design-rests-on)
- [4. Per-kind contract](#4-per-kind-contract)
- [5. Child processes and dev servers: who owns and reaps them](#5-child-processes-and-dev-servers-who-owns-and-reaps-them)
- [6. The pause manifest (schema v1)](#6-the-pause-manifest-schema-v1)
- [7. State transitions: H3 Staged, H4 Pausing, H5 Verifying](#7-state-transitions-h3-staged-h4-pausing-h5-verifying)
- [8. Rollback (#9735)](#8-rollback-9735)
- [9. Requeue-and-record rules](#9-requeue-and-record-rules)
- [10. Survey: existing machinery, reuse vs new](#10-survey-existing-machinery-reuse-vs-new)
- [11. Step 3 touch list (#10715)](#11-step-3-touch-list-10715)
- [12. Open questions for the operator](#12-open-questions-for-the-operator)

## 1. Summary of decisions

1. **"Pause" means the work stops depending on the daemon process for the
   roll. It does not mean the agent stops.** Every in-flight item gets one of
   three dispositions at H4: **carry** (it keeps running and the new process
   adopts it), **interrupt-and-resume** (it is stopped and the new process
   re-dispatches it from its checkpoint, on this host, with the claim kept),
   or **requeue** (its claim is released and the release is recorded). There
   is no `SIGSTOP` freeze (see §2).
2. **Carry is the default wherever the process provably survives the daemon's
   exit.** On launchd that is every daemon child (each leads its own process
   group). On systemd it is every child running in its own `loom-agent-*.scope`.
   The daemon already adopts such survivors after a restart, so carry mostly
   reuses existing code.
3. **Containerized Codex sessions do not survive a daemon exit today, by
   design.** The session-exec transport treats the daemon as its owner and
   revokes the invocation when the daemon exits. In #10715 they are
   interrupt-and-resume. Making them carry-able is a separate question for the
   operator (§12, Q2).
4. **Forge writes happen at H4, before the exit, by the old binary.** The new
   process only adopts carried work and re-dispatches resumable work. A binary
   that cannot read the manifest therefore falls back to today's
   restart-recovery behaviour and loses no work. The manifest speeds up resume
   and records what happened. Correctness does not depend on it.
5. **The H4→H5 window is bounded below the lease TTL** (15 min,
   `claim_reconciliation.rs:551`). Carried and resumable claims keep fresh
   lease records through the roll, so no peer reclaims them. A manifest older
   than the TTL is treated as stale: its resumable items are requeued, never
   resumed.
6. **Ordinary autoUpdate rolls keep today's wait-for-zero drain.** Pause-and-roll
   applies only to floor-driven, repo-ahead and restart-only-config rolls
   (§12, Q3).

## 2. What "pause" means

D2 leaves open what "pause" means for each kind of work. Four candidate
meanings were considered:

| Meaning | Verdict |
|---|---|
| **Freeze** (`SIGSTOP` the tree, `SIGCONT` after the roll) | **Rejected.** A frozen agent still holds its token, its forge claim and its API connection. The model provider times out the stream anyway. Freezing gains nothing over carry for processes that survive the exit, and processes that do not survive die frozen. |
| **Carry**: let it run, and the new daemon adopts it | **Default** for survivable work. The daemon stops supervising for a bounded window and the work does not notice. The existing restart path already does this (`sweep_registry/locks.rs:865`, `startup_adoption.rs`). |
| **Interrupt-and-resume**: stop it now and re-dispatch from the checkpoint after the roll | For work the daemon's exit would kill anyway. Reuses sweep checkpoints (#3373) and the reaper's crash-resume path (#4256). |
| **Requeue**: release the claim and record why | For anything that cannot be resumed safely. Never silent (§9). |

So "pause the work" in D2 becomes: **pause the daemon's supervision of the work,
and stop only the work that cannot outlive the daemon.**

## 3. Survival facts the design rests on

Whether an item can be carried depends only on whether its process outlives the
daemon process. Today that is:

| Host / process shape | Survives daemon exit? | Evidence |
|---|---|---|
| launchd, any daemon child (sweep or role run) | **Yes.** Each child leads its own process group and reparents to launchd. | `sweep_registry/spawn_process.rs:294` and `role_runner/launch.rs:242` (`process_group(0)`); `ipc.rs:162-168` |
| systemd, child exec'd inside a `systemd-run --user --scope` unit (`loom-agent-*.scope` in `loom-agents.slice`) | **Yes.** The scope is a sibling cgroup owned by the user manager, outside the daemon unit's cgroup. | `defaults/scripts/spawn-claude.sh:354-366`, `:475-476`; `ipc.rs:131-148` |
| systemd, child left in the daemon unit's cgroup (the scope probe failed, quota disabled, or a harness that does not wrap) | **No.** `KillMode=mixed` SIGKILLs the remaining cgroup as soon as the main process exits. | `restart_verify.rs:82-98`; `ipc.rs:170-183` |
| Containerized session invocation (`loom-daemon session-exec host`) dispatched by the daemon | **No.** The transport is given `--owner-pid "$PPID"`. On the daemon's dispatch path that is the daemon, because `spawn-worker.sh` → `loom-daemon spawn-worker` → harness is an exec chain. When the owner exits, the transport revokes the heartbeat, and the in-container worker cancels within `LEASE_MS` (2 s). | `defaults/scripts/spawn-codex.sh:1129`; `defaults/scripts/spawn-worker.sh:15`; `worker_spawn/mod.rs:701`; `session_exec/owner.rs:1-3`; `session_exec/host.rs:196`; `session_exec.rs:16` |
| The session **container** itself (`loom-codex-session-<acct>`) | **Yes.** Docker `unless-stopped`; the reconcile pass never interrupts work. | `session_reconcile.rs:1-50` |
| Lease renewal loop for a daemon-dispatched sweep | **Yes.** It is detached and watches the sweep's pid, not the daemon. | `sweep_registry/dispatch.rs:21`, `:3151`; `defaults/docs/lease-renewal.md` |
| In-session (operator-attended) `/loom:sweep` and its Task builders | **Yes.** They were never daemon children. | `cli/lease_ensure.rs:1-20` |

The classification is per process, not per host. A single systemd host can run
scoped Claude sweeps and unscoped ones side by side. #10715 must classify each
item by reading `/proc/<pid>/cgroup` at H4, and must not infer survival from the
platform. The Codex row was traced from code and has not yet been observed on a
live host. #10715's first test must confirm it (§11).

## 4. Per-kind contract

Every row assumes H4 has already set the dispatch-pause flag (§7). "Persisted"
lists the manifest item fields from §6, beyond the common `kind`, `id`, `repo`
and `disposition`.

| Kind | Quiesce point | Persisted fields | Resume procedure (H5) | Cross-version safety check | Unresumable rule |
|---|---|---|---|---|---|
| **Daemon-dispatched sweep, survivable** (Claude or other host-process harness; launchd, or a systemd scope) | None. **Carry.** The process keeps running. | `sweep_id`, `issue`, `pid`, `pid_started_at`, `pgid`, `scope_unit`, `runtime`, `token_name`, `model`, `effort`, `overflow`, `log_path`, `checkpoint_phase`, `worktree`, `lease_comment_id` | Adopt through the existing lock reconstruction (`sweep_registry/locks.rs:865`) and journal adoption (`:1163`). The manifest fills in fields neither source records (scope unit, lease id) and marks the entry `roll_carried`, which gives it watchdog grace (#9452, see below). | Identity-paired liveness: `pid_identity::owner_pid_alive_since` and the recorded pgid still matching (`locks.rs:906-960`). Workspace compatibility: the new daemon's `supports_installed` ≤ the repo's installed `loom_version` (#10716). Until #10716, assume compatible, as every restart does today. | Pid dead at H5: the existing `Crashed` path, which resumes from the checkpoint if the phase is in `RESUMABLE_CHECKPOINT_PHASES` (`locks.rs:96`) and requeues through `restore_label_to_ready` (`guards.rs:1938`) otherwise. Plus `roll.item.lost` (§9). |
| **Daemon-dispatched sweep, not survivable** (systemd, unscoped) | **Interrupt** at H4 after the manifest is written: wait up to the remaining pause budget (max 60 s) for a natural exit, then SIGTERM the tree, wait 30 s, then SIGKILL (§5). The worktree is left untouched. | Same as above, plus `interrupted_at`, `worktree_head`, `worktree_dirty` | Re-dispatch on **this host** before the work finder runs, through the crash-resume entry point (`sweep_registry/dispatch.rs:1568`, `dispatch_resume_after_crash`). The claim is kept, because its lease is still fresh. The sweep skill reads its checkpoint and skips completed phases. `worktree.sh` reuses the worktree, and uncommitted edits are still on disk. | The manifest is no older than the lease TTL. The live lease record on the issue is still this host's. The issue is open and not parked (the dispatch guards' step 2.5/2.7 checks). Counts toward `MAX_RESUME_ATTEMPTS` (`reaper.rs:48`). | No checkpoint, a lease taken over by a peer, a closed or parked issue, or a guard refusal: requeue (§9). |
| **Containerized agent session** (session-exec transport: Codex today, and any harness that uses it) | **Interrupt** at H4: write the `.cancel` marker the transport already honours (`session_exec/host.rs:190-195`), then interrupt the owning sweep as in the row above. The daemon's exit would revoke the invocation anyway; doing it at H4 makes it ordered and recorded. | Above, plus `session_container`, `session_account`, `codex_session_id` when captured (it is only captured at exit, `spawn-codex.sh:1201-1207`) | As the row above. The container is still running (§3), so a resumed dispatch execs into it. | As above. Also the container must be up and its mounts must match (the existing session preflight, `session_exec/host.rs:226-252`). | As above. A session-down refusal requeues with the reason `session-down`. |
| **Role tick** (`role_runner`: Champion, Curator, Judge, Auditor, Guide, Doctor) | Survivable: **carry**. Not survivable: wait up to the pause budget, then **interrupt**. New ticks are already skipped while the drain flag is set (`role_runner.rs:3245-3255`). | `role`, `root`, `pid`, `pid_started_at`, `pgid`, `scope_unit`, `started_at`, `timeout_at`, `log_path` | Carry: seed the role runner's `InProgressGuard` with `(root, role)` for the adopted pid, so the new process does not start a duplicate of that role, and enforce the remaining timeout by pid (today the timeout is enforced through an owned `Child`, `role_runner.rs:1179`). Release the guard entry when the pid exits, with the outcome `adopted-exit-unknown`. Role runs have no registry entry today (`ipc.rs:152-154`); this is the first place they are counted across a restart. | Identity-paired pid liveness only. A role run carries no daemon-version-specific state. | Interrupted runs are **not** re-dispatched: the next scheduled tick does the work again. The claim the role took (`loom:reviewing`, `loom:treating`, …) is chosen inside the agent, so the daemon cannot name it. It is released by that role's own staleness rule (for example `LOOM_STALE_TREATING_MINUTES`). Record `roll.item.requeued` with reason `role-interrupted`. |
| **Epic supervisor** | Stop ticking (drain flag, `epic_supervisor.rs:476-480`). State is derived from the forge each tick, so there is nothing else to quiesce. | Only if the in-memory issue-creation mutex (`issue_creation_mutex.rs:137`) is held: `holder_role`, `holder_pid`, `holder_pid_started_at`, `epic` | Re-derive every epic from the forge (monotone derived state, `epic_supervisor.rs:43-50`). If a carried holder pid is still alive, seed the mutex as held until that pid exits, so a second `creates_issues` burst cannot interleave (#3707). | Pid identity. | Holder interrupted mid-burst: the next tick's derivation recovers. The machine-wide filing lock (`filing_lock.rs`) is file-based and survives the restart. Record `roll.item.requeued` with reason `epic-burst-interrupted`. |
| **Worktree** (`.loom/worktrees/issue-N`) | None. It is on-disk state and is never paused or touched by the roll. | `path`, `branch`, `head`, `dirty`, and the owning item's `id` | Reused by the carried or resumed sweep. Until H5 completes, the worktree reaper and the orphan-process reaper must treat every worktree named in a live manifest as **owned** (`orphan_process_reaper.rs` fail-safes). | None needed. Git state does not depend on the binary version. | Never deleted by the roll. If its issue was requeued it becomes ordinary stale state, handled by the existing worktree reaper's rules. |
| **Pending dispatch** (spawn in progress, `SweepState::Pending`) | Wait for it to become `Running` or fail (bounded by the existing startup-race window), then classify it as a sweep. New IPC dispatches are refused while paused (`ipc.rs:706`, `ipc.rs:1913`). | As a sweep | As a sweep | As a sweep | As a sweep |
| **In-session sweep / attended builder** (not daemon-owned) | None. Not the daemon's work. | Not in the manifest. | Nothing to do. Two caveats: during the window, IPC-backed `loom-daemon` CLI calls fail and callers must retry; after the roll the CLI on `PATH` is the new version. | CLI flag and IPC compatibility. That is the existing release contract, not this design. | None. |
| **Auto-update state** (settle ceiling, window, stall) | Not work. | Persisted by #10713 in `auto_update_state.json`, not in this manifest. | Loaded by #10713. | #10713's typed load outcome. | n/a |

### Watchdog grace for carried sweeps (#9452)

After a restart, the stale-untracked-sweep watchdog judges an adopted sweep by
its log mtime alone (`sweep_registry/watchdog.rs:236`). It killed a live adopted
sweep 30 s after a launchd restart (#9452). Carry makes every roll go through
this path, so #10715 must ship this fix with it: an entry marked `roll_carried`
gets at least one lease TTL of grace. After the grace, its liveness evidence is
the freshness of its lease record or its process-tree activity, never the log
mtime alone. Without this fix, carry is unsafe.

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
   own. Dev servers, watchers and simulators are never carried, resumed or
   requeued by themselves: they are not work, and a resumed agent starts its own
   again.
2. **A carried item keeps its children.** The roll does not touch them.
3. **An interrupted item loses its whole tree**, and the daemon that interrupts
   it (the old binary, at H4) owns that teardown. On systemd it is
   `systemctl --user stop <scope_unit>`, which kills the whole cgroup including
   `setsid` descendants. On launchd it is the orphan reaper's freeze-first tree
   kill over the worktree-attributed and pgid-attributed seeds. A pgid-only
   `kill(-pgid)` is not enough (`orphan_process_reaper.rs:16-35`).
4. **Residue of an agent that has exited belongs to the daemon that is running
   when the agent exits.** For items in the manifest, the H5 process checks each
   recorded `scope_unit`. If the agent pid has exited but the scope still has
   processes, it reaps them and records `roll.item.residue_reaped`. This is why
   the manifest records `scope_unit`, which neither the lock nor the journal does
   today.
5. **Out of scope for #10715, but the same rule applies:** the general case
   from 2am#3255, where an agent exits without any roll and its dev servers keep
   the scope alive. It needs a periodic pass ("scope whose main pid is gone →
   reap") that does not depend on the manifest (§12, Q6).

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
    "max_age_secs": 900                      // = lease TTL at write time
  },
  "items": [
    {
      "id": "sweep-issue-10714-…",           // sweep_id, or a synthetic id for role/epic items
      "kind": "sweep",                       // sweep | role_run | epic_mutex | worktree
      "repo": "/Users/…/loom",               // owning workspace root
      "disposition": "carry",                // carry | resume | requeue
      "status": "planned",                   // planned | interrupted | requeued | adopted | resumed | lost | failed
      "reason": null,                        // required when disposition = requeue, or status in {lost, failed}
      "issue": 10714, "pr": null,
      "pid": 51234, "pid_started_at": "…", "pgid": 51234,
      "scope_unit": "loom-agent-51230-1234.scope",
      "session": null,                       // {container, account, codex_session_id} for session-exec items
      "runtime": "claude", "token_name": "acct-3", "model": "opus", "effort": "high",
      "overflow": false,
      "log_path": "…/.loom/logs/sweep-issue-10714.log",
      "checkpoint_phase": "builder",
      "worktree": { "path": "…/issue-10714", "branch": "feature/issue-10714",
                    "head": "abc123…", "dirty": true },
      "lease_comment_id": 5822662982,
      "role": null, "timeout_at": null,      // role_run only
      "interrupted_at": null
    }
  ],
  "events": [                                // append-only audit trail; both processes append
    { "at": "…", "by_version": "0.19.831", "item": "…", "event": "interrupted", "detail": "…" }
  ]
}
```

**Compatibility rules.** The rollback requirement drives these: after a
rollback, an older binary must be able to read a manifest that a newer binary
may have appended to.

- **Additive within a version.** New fields are optional. Readers ignore fields
  they do not know (no `deny_unknown_fields`). Writers never repurpose a field.
- **Unknown enum values.** If a reader meets an unknown `kind`, `disposition` or
  `status`, it treats the item as `requeue` with reason
  `unknown-<field>-<value>` and records that. It never drops the item silently.
- **Newer `schema_version` than the reader knows.** The reader does no resume
  work from the manifest. Every carried pid is still adopted through the
  existing lock and journal paths, which do not need the manifest. Every other
  item is left to today's restart recovery. The reader emits
  `roll.manifest.unreadable`.
- **Bump `schema_version` only for a change that breaks those rules.**
- **Fields the rolled-back binary must be able to read** (the frozen v1 core):
  `schema_version`, `manifest_id`, `phase`, `written_by.version`,
  `roll.to_version`, `roll.to_artifact_sha256`, `roll.max_age_secs`, and on
  every item `id`, `kind`, `repo`, `disposition`, `status`, `issue`, `pid`,
  `pid_started_at`, `pgid`, `scope_unit`, `worktree.path`, `lease_comment_id`.
- **Load outcomes are typed**, mirroring #10713: `Loaded | Missing | Corrupt |
  UnknownVersion | Stale`. Every outcome other than `Loaded` logs once and falls
  back to plain restart recovery. None of them panics.

## 7. State transitions: H3 Staged, H4 Pausing, H5 Verifying

The host states come from #10698's state machine. Every timeout below is a
config key under `autonomous.autoUpdate.pauseRoll.*` (env > config > default,
matching the other `autoUpdate` knobs).

### H3 Staged

- **Entry:** the binary on disk equals the pinned target (#10709), the running
  `CARGO_PKG_VERSION` is below it (#10710), and `target_source` is `floor`,
  `repo_ahead` or `config_restart`.
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
  - `target_source = autoupdate`: today's drain path, unchanged
    (`auto_update/drain_trigger.rs:123`, `ipc/drain_roll.rs`,
    `auto_update/roll_stall.rs:127,167`).

### H4 Pausing

- **Entry:** from H3. Set the shared dispatch-pause flag (`ipc/drain_state.rs:443`)
  with a new `DrainOrigin::PauseRoll` (`ipc/drain_state.rs:65`). The work finder
  (`work_finder.rs:3102`), the role runner (`role_runner.rs:3248`), the epic
  supervisor (`epic_supervisor.rs:480`) and IPC dispatch (`ipc.rs:706`) all
  already honour this flag. Open the paused-time ledger (`ipc/drain_ledger.rs`).
- **Steps, in order:**
  1. Wait for `Pending` dispatches to settle (bounded by the startup-race
     window).
  2. Classify every in-flight item from every managed root (the same root walk
     as `count_in_flight_sweeps`, `ipc.rs:285`) plus every role run and any
     epic-mutex holder (§4).
  3. **Write the manifest with `phase = pausing`, before anything is
     interrupted.** If the process dies after this point, the next start knows
     everything that was in flight.
  4. Interrupt the items marked `resume`, or marked `requeue` because they
     cannot survive. Do the tree teardown from §5 and append an `events` entry
     for each.
  5. Do the requeue forge writes (§9). Each `gh` call is bounded by
     `reap_gh_timeout` (`sweep_registry/reaper.rs:90`).
  6. Rewrite the manifest with `phase = paused` and set `pause_completed_at`.
  7. Exit `EXIT_RESTART` through the drain supervisor's existing exit path
     (`ipc/drain_supervisor.rs:562`), so the supervisor relaunches the daemon
     onto the staged binary.
- **Exit condition:** step 7. It does **not** depend on the in-flight count.
- **Timeout:** `pauseBudgetSecs`, default **120 s**, covering steps 1-6. When the
  budget runs out, step 6 runs anyway. Any forge write not yet done stays in the
  manifest as `status = planned, disposition = requeue`, and H5 finishes it. A
  slow forge must not block the roll.
- **Failure edges:**
  - The manifest write in step 3 fails (disk full, permissions): **abort the
    pause.** Nothing has been interrupted yet, so clear the flag, resume
    dispatch, go to H7 `pause-failed`, and alert. Never interrupt work that has
    not been recorded.
  - The process dies inside H4: the supervisor relaunches it. The new process
    finds `phase = pausing` and treats the manifest as authoritative for every
    item, finishing the steps that were not done.
  - An operator `--abort-drain` before step 4: same as a manifest-write failure,
    but delete the manifest. After step 4 the abort is refused, because work
    has already been interrupted; the roll completes.

### H5 Verifying

- **Entry:** process start finds a manifest with `phase` of `paused` or
  `pausing`. The new process holds dispatch paused (the flag is set at startup
  whenever a live manifest exists) until H5 exits.
- **Steps, in order:**
  1. Load the manifest (typed outcome, §6). If it is older than
     `roll.max_age_secs`, the outcome is `Stale`: downgrade every `resume` item
     to `requeue` with reason `manifest-stale`.
  2. **Adopt carried items at once.** This is not gated on health, because
     capacity accounting needs them before any dispatch producer starts. It is
     the existing startup ordering (`daemon_service.rs:1274`,
     `startup_adoption.rs:112`), with the manifest supplying the extra fields.
  3. Startup health, owned by #9735: the running version equals
     `roll.to_version`, IPC is responsive, and the heartbeat is sustained for
     `verifyProbationSecs` (default **90 s**, matching
     `DEFAULT_STARTUP_GRACE_SECS`, `daemon_install_state.rs:118`).
  4. On a pass: dispatch the `resume` items on this host, in manifest order,
     before the work finder's first tick (§4). Finish any requeue forge writes
     H4 left as `planned`. Reap the residue of exited carried items (§5).
  5. Mark the manifest `phase = resumed`, archive it, clear the dispatch flag,
     and go to H0.
- **Timeout:** `verifyProbationSecs` for step 3. Steps 4-5 are bounded by
  `resumeBudgetSecs` (default 120 s). Items still unresumed at that deadline
  are requeued with reason `resume-timeout`.
- **Failure edges:**
  - Health fails: rollback (#9735, §8), quarantine `roll.to_artifact_sha256`, go
    to H7. The manifest is not modified except for an appended `events` entry,
    so the rolled-back binary resumes it.
  - The process crashes during steps 4-5: the next start sees
    `phase = resuming`. Items already marked `resumed` are not dispatched again.
    Idempotency comes from per-item `status` plus the dispatch guards' existing
    live-claim checks (`sweep_registry/locks.rs:812`).

**Bound on the window.** The worst case from the start of H4 to the start of
resume dispatch is pause budget + supervisor relaunch + probation:
about 120 + 10 + 90 ≈ 220 s by default, well under the 15 min lease TTL. Config
validation must reject any combination of `pauseBudgetSecs`,
`verifyProbationSecs` and `resumeBudgetSecs` whose sum reaches 2/3 of the lease
TTL.

## 8. Rollback (#9735)

**Contract: paused work resumes on whichever binary passes health, including the
rolled-back one.**

- #9735 owns readiness, the rollback mechanics and quarantine. #9735 is open
  (`loom:triage`) and depends on #9734. Until it lands, an H5 health failure
  leaves the host wherever the existing supervisor and restart-verify behaviour
  (`restart_verify.rs`) leave it. The manifest stays in place, so whichever
  binary next starts successfully runs H5.
- The rolled-back binary is the one that **wrote** the manifest (`written_by`),
  so it reads its own schema. The compatibility rules in §6 matter when the new
  binary appended `events`, or when rollback lands on an even older binary, such
  as the last healthy version after a quarantine chain. Hence the frozen v1 core
  fields.
- **A rollback to a binary older than #10715 is safe**, because of decision 4 in
  §1. That binary ignores the manifest. Carried sweeps are adopted through
  `owner.json` and the journal. Interrupted sweeps leave a lock whose pid is
  dead, which `reconstruct` turns into a `Crashed` entry with its checkpoint
  phase (`locks.rs:1030-1110`). The reaper's existing crash-resume path then
  resumes or requeues them. All H4 requeue writes are already on the forge. The
  only losses are the faster same-host resume and the audit trail.
- On rollback, the H5 that runs on the rolled-back binary must compare
  `running_version` with `roll.to_version`. When they differ, it records
  `resumed_on = rollback` in `events`, and the `roll.paused.resumed` telemetry
  carries both versions.

## 9. Requeue-and-record rules

No item leaves the manifest without a terminal `status` and, for anything other
than `adopted` or `resumed`, a `reason`. Each requeue does all three of:

1. **Label.** For an issue sweep: `restore_label_to_ready` (`loom:building` →
   `loom:issue`, `sweep_registry/guards.rs:1938`). This keeps the #9463 rule
   (`sweep_registry/restore_to_ready.rs`) that a closed issue is never
   re-queued, and the #4206 park check. For a role run or an epic burst there is
   no daemon-known label (§4): the role's own staleness rule handles it, and the
   record below is still written.
2. **Comment.** One forge comment on the issue, or on the PR if the item has
   one, naming the roll (`from → to`), the item's phase, the reason, and whether
   a worktree with uncommitted edits remains on the host. The issue's own lease
   record is left alone: it ages out once the renewal loop stops.
3. **Telemetry.** A `daemon.roll.item` event on the event bus with
   `{manifest_id, item_id, kind, disposition, status, reason, from_version,
   to_version}`, plus a counter by reason in `status --json`.

Reasons are a closed set:

| Reason | Meaning |
|---|---|
| `manifest-stale` | H5 loaded the manifest after `roll.max_age_secs`; the claim may already have been reclaimed elsewhere |
| `lease-lost` | the issue's freshest lease record is no longer this host's |
| `issue-closed` | the issue closed during the window (#9463: it is never requeued, only recorded) |
| `issue-parked` | the issue gained `loom:blocked` or `loom:operator-only` |
| `no-checkpoint` | an interrupted sweep left no checkpoint to resume from |
| `resume-attempts-exhausted` | the `MAX_RESUME_ATTEMPTS` cap was reached |
| `session-down` | the session container refused the resumed exec |
| `guard-refused:<step>` | a dispatch guard refused the resume; `<step>` names it |
| `resume-timeout` | the item was still unresumed when `resumeBudgetSecs` ran out |
| `role-interrupted` | a role run was interrupted at H4 |
| `epic-burst-interrupted` | the issue-creation mutex holder was interrupted mid-burst |
| `unknown-<field>-<value>` | the reader did not recognise an enum value (§6) |
| `pid-dead-at-adopt` | a carried item's pid was gone at H5 (status `lost`) |

## 10. Survey: existing machinery, reuse vs new

| Machinery | Where | Use in this design |
|---|---|---|
| Dispatch-pause flag and its producers | `ipc/drain_state.rs:28`, `:443`; `work_finder.rs:3102`; `role_runner.rs:3248`; `epic_supervisor.rs:476-480`; `ipc.rs:706` | **Reuse as is.** H4 and H5 hold dispatch with this flag. |
| `DrainOrigin` (Operator / AutoUpdate) | `ipc/drain_state.rs:65` | **Extend.** Add `PauseRoll`, whose completion condition is "manifest written" instead of "in-flight == 0". |
| Drain supervisor, exit codes, supervisor detection, then-exit precedence | `ipc/drain_supervisor.rs:44`, `:308`, `:498-562` | **Reuse.** The exit path and the then-exit escalation (#4521) are unchanged. |
| Wait-for-zero roll policy (#6007 re-arm, abandon budget) and stall suppression | `ipc/drain_roll.rs:32-45`, `:139`; `auto_update/roll_stall.rs:127`, `:167` | **Unchanged**, for `autoupdate` rolls only. A pause roll bypasses it. |
| Roll trigger trait | `auto_update/drain_trigger.rs:22` (`DrainTrigger`), `:123` (`IpcDrainTrigger::trigger`) | **Extend** with `trigger_pause_roll(target)`. Supersede (#8514) keeps working: a newer target during H3 replaces the old one, and once H4 has started it is too late to supersede. |
| Lock-based restart adoption (pid identity, pgid re-verification, token and runtime recovery) | `sweep_registry/locks.rs:865` (`reconstruct`) | **Reuse** for carry. Add a manifest overlay for `scope_unit`, `lease_comment_id` and `roll_carried`. |
| Journal adoption and startup capacity seeding | `sweep_registry/locks.rs:1163`; `startup_adoption.rs:112`; `daemon_service.rs:1274`; `sweep_journal.rs` | **Reuse.** The manifest closes a known gap: the journal records no pgid (#9452 fix 4). |
| Sweep checkpoints and crash-resume | `.loom/sweep-checkpoint/issue-N.json` (#3373, `defaults/.claude/commands/loom/sweep-reference.md:43`); `sweep_registry/dispatch.rs:1568`; `locks.rs:96`; `reaper.rs:48` | **Reuse** for interrupt-and-resume. **New:** a manifest-driven entry point that also resumes pre-PR phases on the same host (the reaper resumes only `RESUMABLE_CHECKPOINT_PHASES` when a PR is open). |
| Requeue and claim restore | `sweep_registry/guards.rs:1938`; `sweep_registry/restore_to_ready.rs` | **Reuse.** |
| Lease records and renewal | `defaults/docs/lease-record.md`; `defaults/docs/lease-renewal.md`; `sweep_registry/dispatch.rs:3151`; TTL at `claim_reconciliation.rs:551` | **Reuse.** Leases protect claims through the window, so it must stay under the TTL. |
| Startup and periodic claim reconciliation (dead pid → reclaim, gated on lease freshness) | `claim_reconciliation.rs`; `daemon_startup_reconciliation.rs` | **Reuse as the backstop** for anything the manifest path misses. |
| Stale-untracked-sweep watchdog | `sweep_registry/watchdog.rs:236`, `:380-400` | **Change.** Grace for `roll_carried` entries (#9452). Required before carry is safe. |
| Orphan-process reaper (worktree-attributed, freeze-first tree kill) | `orphan_process_reaper.rs:1-60` | **Reuse** for interrupted-tree teardown on launchd. **Extend** its fail-safes so a worktree in a live manifest counts as owned. |
| Agent scopes (`loom-agent-*.scope`, `loom-agents.slice`) | `defaults/scripts/spawn-claude.sh:340-490` | **Reuse.** The scope is the survival and teardown unit. **New:** record the scope unit in `owner.json` at dispatch. |
| Session-exec transport (lease, owner, cancel marker) | `session_exec.rs`; `session_exec/owner.rs`; `session_exec/host.rs:184-260` | **Constraint.** The daemon is the owner, so these invocations are interrupted. The `.cancel` marker gives an ordered interrupt. |
| Session container reconcile | `session_reconcile.rs` | **Reuse unchanged.** It never interrupts work, and the containers survive. |
| Role-run admission guard | `role_runner.rs:2378-2420` | **Extend.** Seed it from the manifest for carried role runs. |
| Issue-creation mutex (in memory) | `issue_creation_mutex.rs:137` | **Extend.** Seed it from the manifest when its holder is carried. |
| Relaunch verification and supervisor self-heal | `restart_verify.rs`; `daemon_update/restart_flow.rs`; `daemon_update/supervisor.rs`; `daemon_update/relaunch.rs` | **Reuse** for H4→H5. |
| Persisted update state (state dir, atomic write, typed load) | `auto_update.rs:600-640` (`ArtifactRollRecord`); #10713 | **Reuse the pattern and directory.** |
| Paused-time ledger | `ipc/drain_ledger.rs` | **Reuse.** Open it at H4 and close it at the end of H5. |
| Rollback, readiness and quarantine | #9735 (open), #9734 (open) | **Dependency.** §8 says what happens before they land. |

**Net new:** the manifest module, the H4 classifier and orchestrator, the H5
consumer, `DrainOrigin::PauseRoll`, three guard/mutex seed points, the watchdog
grace, `scope_unit` in `owner.json`, and the `daemon.roll.*` events.

## 11. Step 3 touch list (#10715)

The scope is large, so #10715 should be curated as **three PRs in order**. Each
PR is held for operator merge under the tracker's constraint.

**PR A: manifest and classification (no behaviour change).**

- New `loom-daemon/src/auto_update/pause_manifest.rs`: types, typed load
  outcome, atomic save, compatibility rules (§6).
- New `loom-daemon/src/auto_update/pause_classify.rs`: pure classification from
  `{kind, platform, cgroup, owner}` to a disposition, with per-pid cgroup
  probing on Linux.
- `loom-daemon/src/sweep_registry/locks.rs`, `dispatch.rs`, `mod.rs`: record
  `scope_unit` in `owner.json`; add an in-flight snapshot API for the
  classifier.
- Tests:
  - Manifest round-trip.
  - Unknown fields ignored.
  - Unknown enum values become requeue.
  - Newer `schema_version` gives `UnknownVersion`.
  - Corrupt, missing and stale manifests.
  - The classification matrix: launchd, systemd scoped, systemd unscoped,
    session-exec.
  - A Linux-only test that confirms the Codex owner finding in §3 against a
    fake owner pid.

**PR B: H3/H4 pause (old-binary side).**

- `loom-daemon/src/auto_update.rs`: route `floor`, `repo_ahead` and
  `config_restart` targets to the pause path. `autoupdate` is unchanged.
- `loom-daemon/src/auto_update/drain_trigger.rs`: `trigger_pause_roll`.
- `loom-daemon/src/ipc/drain_state.rs`, `ipc/drain_supervisor.rs`:
  `DrainOrigin::PauseRoll` and its completion condition.
- `loom-daemon/src/auto_update/pause_roll.rs` (new): the H4 steps 1-7 and the
  budget.
- `loom-daemon/src/sweep_registry/reaper.rs`, `orphan_process_reaper.rs`: tree
  teardown for interrupted items (scope stop / freeze-first).
- `loom-daemon/src/sweep_registry/guards.rs`, `restore_to_ready.rs`: requeue
  with a reason, plus the comment.
- `loom-daemon/src/role_runner.rs`, `issue_creation_mutex.rs`: export role-run
  and mutex-holder snapshots.
- Tests:
  - Integration with fake sweeps (`sweep_registry/test_support.rs`): a pause
    with a carried item, an interrupted item and a requeued item.
  - A manifest write failure aborts the pause with no interruption.
  - The budget runs out and the forge writes are deferred.
  - A then-exit drain wins.
  - The existing drain tests stay green, unchanged:
    `loom-daemon/tests/integration_drain_then_exit.rs`,
    `integration_drain_exit_then_watchdog_recovers.rs`,
    `src/auto_update/tests/supersede_tick.rs`, `src/ipc/drain_state_tests.rs`.

**PR C: H5 resume (new-binary side).**

- `loom-daemon/src/daemon_service.rs`: load the manifest before
  `spawn_startup_passes` and `seed_capacity_from_journal`; hold the dispatch
  flag while a manifest is live.
- `loom-daemon/src/startup_adoption.rs`, `sweep_registry/locks.rs`: the
  manifest overlay on adoption.
- New `loom-daemon/src/auto_update/pause_resume.rs`: H5 steps 1-5, residue
  reaping, archiving.
- `loom-daemon/src/sweep_registry/dispatch.rs`: same-host resume entry point
  with the claim kept and the lease checked.
- `loom-daemon/src/sweep_registry/watchdog.rs`: `roll_carried` grace (#9452).
- `loom-daemon/src/role_runner.rs`, `issue_creation_mutex.rs`: seed from the
  manifest.
- `loom-daemon/src/orphan_process_reaper.rs`, `worktree_reaper.rs`: worktrees in
  the manifest count as owned.
- `loom-daemon/src/types.rs`, `ipc.rs` status: the pause/resume state in
  `status --json`.
- Tests:
  - Resume of carried, resumed and requeued items.
  - A crash during H4 (`phase = pausing`).
  - A crash during H5 (`phase = resuming`, no double dispatch).
  - A stale manifest.
  - A rolled-back binary resumes the manifest.
  - A pre-#10715 binary ignores the manifest and still recovers through
    reconstruct.
  - A carried sweep survives the watchdog (#9452 regression).

**Not in #10715:** making session-exec ownership survive the daemon (§12, Q2),
general agent-exit residue reaping (§12, Q6), the H5 health and rollback
mechanics (#9735).

## 12. Open questions for the operator

1. **Carry as "pause".** Do you accept that, for work that survives the
   daemon's exit, "pause" means the agent keeps running and only the daemon's
   supervision pauses? The alternative is to interrupt everything and resume it
   from checkpoints. That is simpler to reason about, but it throws away all
   in-phase progress on every roll, which is the same cost the current drain
   was built to avoid.
2. **Containerized Codex sessions.** In #10715 they are interrupt-and-resume,
   because the session-exec owner is the daemon. Should a follow-up move
   ownership to a process that survives the roll (for example the
   `spawn-codex.sh` shell)? That weakens the rule in `session_exec/owner.rs`
   that a dying daemon revokes its invocations. Recommendation: keep
   interrupt-and-resume, and revisit only if Codex share and roll frequency make
   the lost work material.
3. **Ordinary autoUpdate rolls.** D2 says the drain stays for them "if at all".
   Recommendation: keep the drain in #10715. After one release of fleet soak on
   floor rolls, flip autoUpdate to pause-and-roll behind
   `autonomous.autoUpdate.rollMode = drain | pause`, and later retire
   `roll_stall` for that path.
4. **Defaults.** Pause budget 120 s, verify probation 90 s, resume budget 120 s,
   and a manifest max age equal to the lease TTL (15 min). Are these
   acceptable?
5. **Compatibility before #10716.** Until `supports_installed` exists, carried
   work is assumed compatible with the new daemon, which is what every restart
   assumes today. Is that acceptable, or should pause-and-roll wait for #10716?
6. **Dev-server residue (2am#3255).** #10715 reaps residue only for items in the
   manifest. The general "agent exited, scope kept alive by its dev servers"
   reaper, including deregistered workspaces, is a separate issue.
   Recommendation: file it now, independent of #10698.
7. **Role runs.** Carry and adopt them, seeding the guard and enforcing the
   remaining timeout by pid, as proposed? Or interrupt every role run at H4 and
   let the next tick redo the work, which is simpler and wastes at most one
   tick per role?
