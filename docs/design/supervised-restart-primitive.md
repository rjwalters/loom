# Supervised restart primitive (Issue #4054, Phase 2 of #4017)

**Status:** implemented
**Decision record for:** how `loom-daemon` deliberately ends and reliably comes
back, so #4017 Phase 3 (auto-rebuild-and-restart-when-stale) has a *proven*
restart primitive to call.

## Problem

#4017 identified the hard part precisely: **a process cannot reliably restart
itself.** The macOS LaunchAgent that runs `loom-daemon` (`render_launchd_plist`
in `defaults/scripts/cli/loom-daemon-start.sh`) had `KeepAlive: false`, so there
was no supervisor that would bring the daemon back after it exited. If the daemon
spawned `loom-daemon-update.sh` as a child, that helper lives inside the launchd
job's process tree, and its call to `loom-daemon-stop.sh` → `launchctl bootout`
tears the tree down — killing the very helper meant to start the replacement. The
failure mode is *worse than the status quo*: an unattended daemon that takes
itself down permanently.

## Chosen mechanism: Option 1 — split rebuild from restart, supervised by launchd

The daemon does the safe part (rebuild/provision — #4053) in-process and then only
needs to **end**. Restart becomes the supervisor's job:

- **Plist:** `KeepAlive: { SuccessfulExit: true }` — launchd relaunches the job
  **only** when it exits with status `0`, and leaves it down on any non-zero exit.
- **Trigger:** a new `RestartDaemon` IPC request (`loom-daemon restart`). It is the
  **only** path that exits the process with status `0`, so it is the only thing
  that trips a relaunch. It never fires on its own — nothing in the daemon issues
  it. #4017 Phase 3 will call it after a successful rebuild.
- **Exit code carries intent** (Curator Finding 1, the load-bearing fix):

  | Shutdown cause                | Exit code | launchd action under `SuccessfulExit: true` |
  |-------------------------------|-----------|---------------------------------------------|
  | `RestartDaemon` primitive     | `0`       | relaunch — the desired path                 |
  | SIGTERM (operator stop)       | `143`     | **no relaunch**                             |
  | SIGINT (interactive Ctrl-C)   | `130`     | no relaunch                                 |
  | IPC `Shutdown` request        | `143`     | no relaunch                                 |
  | crash / panic                 | non-zero  | no relaunch (preserves no-crash-loop)       |

- **Supervision proof:** the daemon only ends its process for a restart when it can
  prove it is supervised. `loom-daemon-start.sh` bakes
  `LOOM_DAEMON_SUPERVISOR=launchd` into the plist `EnvironmentVariables` (so it
  survives a relaunch). On an unsupervised host (nohup / Linux / `--foreground`)
  the var is absent, and `RestartDaemon` **refuses**: it logs loudly, leaves the
  daemon running, and returns `DaemonRestart { scheduled: false }`. This is #4017's
  "log loudly, leave the daemon running, do not restart" for the no-supervisor case.

### Why the exit-code change is the crux (Curator Finding 1)

Before #4054 the daemon exited `0` on **both** SIGINT and SIGTERM. Under
`SuccessfulExit: true`, an operator stop (`loom-daemon-stop.sh` sends SIGTERM and
only *then*, after the process is dead, calls `bootout`) would be a **clean exit
while the job is still loaded** — so launchd would relaunch the daemon in the
window before the bootout lands. Because `launchd_bootout_if_loaded` swallows all
errors and the stop script only re-checked the *original* pid, a failed bootout
would print `loom-daemon stopped` with a relaunched daemon still dispatching — the
#4011 silent-divergence shape, inverted.

Making the exit code carry intent removes the race entirely: a SIGTERM'd daemon
exits `143`, which is **not** a successful exit, so launchd never relaunches it
during an operator stop. "An operator stop stays stopped" now holds **without
depending on bootout timing**; the bootout is demoted to belt-and-braces (it still
unloads the definition so the job does not come back at the next login). This also
preserves the pre-#4054 no-crash-loop semantics of `KeepAlive: false`: a crashed
or SIGKILL'd daemon terminates non-cleanly and is **not** respawned.

`loom-daemon-stop.sh` additionally re-verifies, **scoped to its launchd label**,
that no daemon is still alive after the stop and exits non-zero if one is — closing
the inverted-#4011 silent-success hole. The check is label-scoped (not a global
`pgrep loom-daemon`) so a test daemon under a non-default `LOOM_LAUNCHD_LABEL`
never false-positives against a separate production daemon.

## Alternatives rejected

### Option 2 — `exec` self-replacement

After installing the new binary, the daemon `execve()`s it in place: same pid,
launchd never notices, no plist change. Rejected for this primitive because:

- **It does not prove what this issue asks.** The issue is titled "prove the daemon
  can end and *reliably come back under launchd*." Option 2 deliberately *sidesteps*
  launchd — it proves in-place replacement, not that the supervisor brings the
  daemon back. Phase 2 is exactly the phase that must retire the "can a process
  restart itself?" unknown by handing the job to a real supervisor.
- **Concrete hazards (Curator Finding 2).** The singleton guard is **socket-based**
  (`ipc.rs::socket_has_live_listener`), so an `exec` must `remove_file` the socket
  before re-exec or the re-exec'd image trips its own guard on the socket it still
  owns; the listening fd must be `FD_CLOEXEC` so it is not inherited across the
  exec; and in-flight IPC/registry state must be handled. Each is a sharp edge on
  the *supervision* path, where the blast radius of a mistake is total unattended
  autonomy loss.
- Its genuine upside (same pid ⇒ no plist change ⇒ sidesteps Finding 1) is real but
  does not outweigh "doesn't actually exercise the supervisor." It remains a
  reasonable future optimization for a rebuild that wants zero relaunch latency.

### Option 3 — detached helper

A helper process that genuinely escapes the launchd job tree and starts the
replacement. Rejected: strictly more complex than Option 1, and it requires proving
the helper survives `bootout` — the exact fragility #4017 flagged. Only worth it if
1 and 2 both failed; 1 works.

## Non-macOS / unsupervised

The `nohup` path (`loom-daemon-start.sh` when `launchctl` is absent, and Linux)
has no supervisor. Per #4017 the daemon degrades to "log loudly, leave the daemon
running, do not restart": `RestartDaemon` returns `DaemonRestart { scheduled:
false }` and the process keeps running, because exiting with nothing to relaunch it
would be strictly worse than the status quo.

## Verification

Automated: `cargo test --workspace`; the three `defaults/scripts/tests/
test-loom-daemon-*.sh` suites (plist pins the new `KeepAlive`/supervisor values;
stop-path asserts bootout unchanged + the label-scoped still-alive guard); and a
serde round-trip + supervisor-gating unit test in `loom-daemon/src/ipc.rs`.

Manual (live launchd, against an **isolated** `LOOM_LAUNCHD_LABEL` +
`LOOM_SOCKET_PATH`, never the production `com.rjwalters.loom-daemon`):

- Restart primitive: daemon exited `0`, launchd relaunched it with a **new pid**,
  and the relaunched process kept `LOOM_DAEMON_SUPERVISOR=launchd`.
- Operator stop: SIGTERM → daemon did **not** come back across the relaunch window;
  job unloaded; stop exited `0`.
- Crash: `SIGKILL` → **not** respawned (no crash loop).
- Exit-code contract: SIGTERM → `143`, SIGINT → `130`.

In-flight-sweep survival was **not** exercised live (dispatching a real sweep runs
an actual, side-effecting Claude session), but it holds by construction: the restart
is a plain process exit + launchd relaunch, and sweep children are independent
detached processes the daemon never cancels on shutdown (the documented "survive,
don't drain" decision). Recommended as a manual follow-up on a canary before Phase 3
builds on top of it.
