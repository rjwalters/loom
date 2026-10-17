# Daemon roll: restore the previous binary when the candidate fails to start

**Status:** design only (issue #11433, operator ruling 2026-10-17). Nothing here is implemented, and approving
this document does not approve the implementation: that needs its own `loom:issue`.
**Traced on:** `origin/main` @ `00c23e593`, 2026-10-17. Line-level facts are cited as `file::symbol`; re-verify
before relying on them.

The ruling: keep it simple. The roll keeps a backup and rolls back on failure. This document does not revive the
#9735 quarantine, fallback-release selector or state machine, or the #11434/#11435/#11436 criteria.

## 1. Owners of the operation

There is no separate "roll script" with its own backup logic.

| Piece | What it is |
|---|---|
| `defaults/scripts/cli/loom-daemon-update.sh` | A stub that execs `loom-daemon daemon-update`. |
| `loom-daemon/src/daemon_update/mod.rs::stage_two` | The whole update: fetch or rebuild, provision, verify, then `restart_flow::run`. Its exit codes are the contract (table in the module doc). |
| `daemon_update/provision.rs::{stage, publish, install_to}` and `provision/txn.rs` | Stage a copy beside `dest`, keep the live binary as `<dest>.previous`, write `<dest>.install-state.json` (phases `staged` → `published` → `committed`), `rename(2)` over `dest`. |
| `daemon_update/restart_flow.rs` | Supervised restart plus a new-pid check; the source of exit 6 and 7. |
| `daemon_update/verify.rs` | Post-provision checks; the source of exit 5. |
| `loom-daemon/src/fleet/roll.rs` | SSH fan-out of the script, with a measured verdict (process start time vs binary mtime). It observes the result and does not own the binary. |

**Correction to the issue's curation notes.** The curator wrote that `install_to` does not use `txn.rs`. It does:
`install_to` is `stage(..)` then `publish(..)` (`provision.rs::install_to`), and `publish` calls `txn::begin`, so
the `LOOM_DAEMON_BIN` path and the machine-level path both leave `<dest>.previous` and the record. Reproduce with
`git grep -n "txn::begin" loom-daemon/src/daemon_update/provision.rs`.

## 2. What exists and what does not

- `txn.rs` module doc: "It never restores anything", and `committed` "says nothing about whether the new binary
  starts or is healthy".
- Invariant already guaranteed from the first record write: when `previous` is present, `<dest>.previous` holds
  bytes with the recorded sha256 (`txn.rs` module doc, "One invariant holds…").
- Exactly one previous binary is kept, and it is only "the binary that was live before", not a proven-healthy one.
  A candidate byte-identical to the live binary keeps the older `.previous` rather than overwriting it.
- Nothing reads `<dest>.previous` back.

## 3. Failures the updater can observe, and whether it is alive to act

`daemon-update` is its own process. It outlives the candidate daemon only on the paths where it is not the
daemon's descendant.

| Failure | Where | Updater alive after it? | State of `dest` |
|---|---|---|---|
| Provision fails (`install_to` false, `ProvisionOutcome::Failed`) → exit 1 | `mod.rs::stage_two` | Yes | Unchanged: the rename is the last step and nothing past it ran. Nothing to restore. |
| Post-provision verify fails → exit 5 | `verify.rs` | Yes | `dest` is not what this run produced (missing, wrong commit, or a concurrent install replaced it). **Do not restore:** the file may be someone else's install. |
| Running old binary refuses supervised restart → exit 6 | `restart_flow.rs` (launchd and systemd branches) | Yes | Candidate on disk; the old process is still serving. No candidate ever started, so there is no failure to roll back. |
| Restart ack'd, no new live pid, self-heal also failed → exit 7 | `restart_flow.rs::{launchd_restart, systemd_restart}` | Yes, **when run by an operator or by `fleet roll` over SSH** | Candidate on disk; daemon possibly down. **This is the restore case.** |
| Drain fail-safe → exit 8 | `restart_flow.rs::report_drain_failsafe` | Yes | Candidate on disk, old binary still running. Not a failure. |
| Candidate starts a pid, then crashes later | none: the updater saw a new live pid and exited 0 | Already gone | Out of scope (§7). |

### The daemon-tick path is different, and restore is not reachable there

The auto-update tick spawns the script as `--no-restart` (`auto_update.rs`, the `command.arg("--no-restart")` in
the rebuild helper), so `daemon-update` provisions and exits before any restart. The restart is then done by the
daemon itself through pause-and-roll (`auto_update/pause_roll`, `supervise.rs`). The daemon exits and the supervisor
relaunches it onto `dest`. There is no updater process left to restore anything, and the module doc of
`restart_verify.rs` records why a supervisor can fail to relaunch (launchd #4232; systemd stop-job timeout #4950).
The tick path already has a different guard: the `roll_attempt` record (#10880, `auto_update/roll_attempt.rs`)
holds the failed target back for 15 min doubling to 6 h once a lower-versioned binary comes back, and
`daemon-roll-pause-resume.md` §8 describes what the host does until #9735 lands.

**Consequence:** the smallest change restores on the operator and `fleet roll` paths only. The tick path keeps
today's behavior (a failed candidate leaves the host where the supervisor leaves it, and `roll_attempt` backs off
the retry). Closing that gap needs a restorer that outlives the daemon, which is a new component. This design
deliberately does not add one (§7).

## 4. The single smallest change

Add a `restore` function beside `publish` in `provision/txn.rs`, and call it from the exit-7 branches in
`restart_flow.rs`. Nothing else changes.

**Preconditions, all required (otherwise the outcome is "nothing to restore" and exit 7 is unchanged):**

1. `read_record(dest)` returns a usable record with phase `committed`.
2. `record.previous` is `Some`, and sha256 of `<dest>.previous` equals `record.previous.sha256`.
3. sha256 of `dest` equals `record.target.sha256`. This is the guard against clobbering a binary a concurrent
   install put there; if `dest` is not the candidate this run published, leave it.

**Action:** copy `<dest>.previous` to a fresh staging file with `provision::stage`'s mechanics (mode 755, fsync),
then `rename(2)` it over `dest`. Copy rather than rename `.previous` itself, so the recovery copy survives a failed
restore and a second attempt. Installs are not serialized against each other today (`provision.rs::publish` leaves that to #9734), so a
restore can race a concurrent install; precondition 3 is the only guard, and it narrows the window without closing
it. Then re-run the existing supervised restart exactly once against the restored `dest`, through the same
`restart_flow` branch, so drain, compatibility checks and paused-work recovery behave as they do for any restart.

**No new persistent state.** The record is not rewritten. A restored `dest` is recognizable because its sha256 no
longer equals `record.target.sha256`; precondition 3 therefore also makes a second restore a no-op.

## 5. Outcomes and exit codes

Existing codes keep their meaning for every current caller. `classify_exit` (`auto_update.rs`) treats an unknown
nonzero code as `Retryable`, and the tick never reaches the restart, so it is unaffected. `fleet roll` trusts only
its measured verdict, not the script's exit code.

| Outcome | Meaning | Exit |
|---|---|---|
| nothing-to-restore | A precondition in §4 failed (fresh install, missing or mismatched `.previous`, `dest` not the candidate) | **7**, unchanged, with one added line saying which precondition failed |
| restored | `dest` is back to the previous bytes and the supervised restart produced a new live pid | **9** (new). Nonzero: the update did not take |
| restore-failed | Restore or the second restart failed; the host may be down | **10** (new). The message names `<dest>.previous` and the exact manual command |

Callers that branch on 7 today see 7 in every case that is not newly restored. Only a caller that opts into
reading 9 or 10 sees a difference. The module-doc exit table gets two rows.

## 6. Reconciliation with other work

- **#12610** (a floor rollback does not cancel an armed pause-and-roll). That is about disarming a roll before H4
  step 5 on the tick path; this design restores a binary after a failed restart on the script path. They share no
  code and do not conflict. A restore only ever writes the retained previous bytes, so it cannot revive a withdrawn
  version. If #12610 lands a "disarm" verb, it needs no change here.
- **#11706** (hold on flow-changing requests). The change touches only the script and `fleet roll` path and adds
  two exit codes. It does not alter the tick's pause-and-roll flow, the label state machine or any dispatch
  decision, so it is not a governed flow change. If review reads two new exit codes as one, the implementation
  issue should carry that hold until the operator rules.
- **Pause-and-roll §8 / #10880 / #10832.** Unchanged. A restored binary is the one that wrote the pause manifest in
  the common case, which §8 already covers, and a restore does not clear `roll_attempt`.

## 7. Remaining limitations (stated, not fixed)

- **Daemon-tick path is not covered** (§3). A candidate that fails after the tick's pause-and-roll leaves the host
  down or on a backoff, as today.
- **Interrupted updater or reboot between publish and restore.** The updater dying, or the host rebooting, after the
  candidate is published and before the restore leaves the candidate in place. `.previous` and the record remain
  for a manual restore.
- **Fresh install:** `previous` is `None`, nothing to restore.
- **`.previous` is the prior binary, not a proven-healthy one,** and only one is kept. Restoring twice in a row
  returns to the first candidate's predecessor, not further back.
- **Health is a new pid, nothing more** (`restart::wait_for_new_*_pid`). A candidate that starts and then fails
  readiness, or regresses at runtime, is neither detected nor restored.
- **Restore reuses the same restart machinery,** so it fails the same ways: a supervisor that does not relaunch,
  a drain that times out.
- This is not crash recovery, not a watchdog, and not a quarantine.

## 8. Verification plan (isolated fixtures only, no live fleet drill)

Unit tests beside `txn_tests.rs`, which already has the seams (`an_install_keeps_the_previous_binary_with_its_sha256_and_version`,
`a_first_ever_install_succeeds_with_no_previous_copy`, `only_the_most_recent_previous_binary_is_kept`):

1. restore with valid record and `.previous` → `dest` bytes equal previous; `.previous` still present and
   unchanged.
2. fresh install (`previous = None`) → nothing-to-restore.
3. `.previous` sha256 mismatch, and `.previous` missing → nothing-to-restore, `dest` untouched.
4. `dest` sha256 not equal to `target.sha256` (concurrent install) → nothing-to-restore, `dest` untouched.
5. restore called twice → second is nothing-to-restore.
6. restore I/O failure (unwritable directory) → restore-failed, `dest` untouched, `.previous` intact.

Script-level cases in `defaults/scripts/tests/test-loom-daemon-update.sh`, using
`defaults/scripts/tests/lib/daemon-update-fixtures.sh`, which already builds fake daemon binaries and a fake
supervisor:

7. successful roll: unchanged, exit 0, no restore attempted.
8. candidate that never gets a new pid (existing exit-7 fixture) with a valid `.previous`: exit 9, `dest` holds the
   previous bytes.
9. same fixture with `.previous` missing or corrupt: exit 7 unchanged plus the precondition line.
10. restored binary also fails to come up: exit 10.

Each traced claim in §1–§3 can be reproduced with the cited `git grep` or file read at the traced SHA.

## 9. Not done here

Implementation, a live roll, deployment, credentials and spend. No change to the tick, to `fleet roll`, or to any
existing exit code.
