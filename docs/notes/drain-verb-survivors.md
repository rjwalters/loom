# Surviving users of the `restart --drain` family (#12280, for #12148)

Traced on `origin/main` @ `45a7b7ff3`, 2026-10-16, with
`git grep -nE 'then_exit|force_after_timeout|abort_drain|AbortDrain|--drain|--then-exit|--abort-drain|--force-after-timeout|DrainState'`
over `*.rs` and `defaults/`. Tests, doc comments and help text are not listed
as users; they follow their owner.

## Why almost everything is `keep`

`restart --pause` (#11812) **requires `--then-exit`** and conflicts with
`--drain`, `--timeout` and `--force-after-timeout`
(`loom-daemon/src/cli/restart.rs`). It stops the daemon and leaves a pause
manifest for the next start's H5 to resume. It has no relaunch form and no
timeout/force semantics, so a caller that needs a relaunch or a teardown that
requeues claims cannot be expressed as `--pause`. The relaunch-with-pause path
(H4) is driven by the auto-update roll, not by a CLI verb.

## Table

| Use (non-test) | Verdict | Reason |
|---|---|---|
| `fleet/drain.rs:543` `fleet drain` runs `restart --drain --then-exit --timeout [--force-after-timeout]` | keep | Decommission/teardown: it then resets captured claims so other hosts pick the work up. `--pause` leaves paused work for the *same* host to resume, which is the wrong contract here. |
| `fleet/drain.rs` fail-safe text naming `--force-after-timeout` / `--abort-drain` | keep | Follows the row above. |
| `cli/workspace_fleet.rs` (`fleet drain --force-after-timeout` flag) | keep | CLI surface of `fleet drain`. |
| `daemon_update/restart.rs` `build_restart_invoke_args` (`--drain`, `--timeout`, `--force-after-timeout`) | keep | `daemon-update` needs a drain-then-**relaunch**; `--pause` has no relaunch form. |
| `daemon_update/restart_flow.rs`, `daemon_update/args.rs`, `cli/daemon_update.rs` (timeout fail-safe text, `--force-after-timeout`, `--abort-drain` guidance) | keep | Follows `build_restart_invoke_args`. |
| `auto_update/supersede.rs` (`then_exit` on an armed roll) | keep | Models the armed roll's own `then_exit`; the supersede rule keeps an operator `fleet drain` teardown from being overridden. Not a CLI caller. |
| `auto_update/pause_roll.rs`, `pause_roll/supervise.rs`, `pause_roll/maintenance.rs`, `roll_trigger.rs` (`then_exit`, `DrainState`) | keep | These *are* the pause path; `then_exit` is how host maintenance stays down. `--abort-drain` is refused here by design. |
| `auto_update/loop_mode.rs`, `watchdog/mod.rs`, `operator_stop.rs`, `daemon_start/{autonomy,render}.rs` (operator-stop marker written by `--drain --then-exit` or `--pause --then-exit`, cleared by `--abort-drain`) | keep | Hold semantics shared by both stay-down verbs. |
| `fleet_state.rs` (`paused` hold = `DrainState::hold_for_fleet_state`, `then_exit = true`, `force_after_timeout = false`) | keep | `begin_host_maintenance`'s fleet hold reuses `DrainState`; removing `DrainState` removes it. |
| `ipc/drain_state.rs`, `drain_pause.rs`, `drain_resume.rs`, `drain_status.rs`, `drain_ledger.rs`, `drain_supervisor.rs`, `ipc.rs`, `types.rs` (`DrainAndRestartDaemon`, `AbortDrain`) | keep | Wire and state layer under every row above. `DrainState` hold semantics are shared by pause. |
| `launchd_reload.rs:314` message advising `restart --drain` after `--reload-supervisor` | keep | Relaunch guidance. |
| `fleet/add_worker.rs:1249,1284` comments/unit text about `restart --drain` exiting for relaunch | keep | Describes the supervised-relaunch contract. |
| `defaults/scripts/spawn-claude.sh:634` comment naming `restart --drain` as the recommended stop | **move to pause path** (done in this PR) | Comment now also names `restart --pause --then-exit`. No behaviour change. |

## Residual deletion scope for #12148

Nothing in the table has *no* survivor: every drain verb and IPC request still
has a live caller (`fleet drain` for `--drain --then-exit`/`--force-after-timeout`/
`--abort-drain`, `daemon-update` for `--drain` relaunch), and `DrainState` is
shared with the pause path. #12148 can only delete if it first removes
`fleet drain` or `daemon-update --drain`, or gives `--pause` a relaunch form
and a claim-reset mode. That is a design decision, not a mechanical deletion.
