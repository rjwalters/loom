# Operator presence (#11563, #12121)

`operator.presence` tells the ETA fit whether a human is driving an issue, and
in-session sweeps report `sweep.started` / `sweep.outcome` with
`dispatch_origin=operator`. The **daemon derives all of it**; no hook, agent or
CLI command has to run (the #11821 principle). The old
`loom-daemon presence …` subcommands and `presence-beat.sh` hook are gone.

## What the daemon reads

The observability collector ticks every 60 seconds (`observability/presence_derive.rs`):

- **Presence** — session transcripts under `${CLAUDE_CONFIG_DIR:-~/.claude}/projects`.
  A session counts when its records carry `entrypoint: "cli"` (an interactive
  session; a dispatched `claude -p` run is `sdk-cli` and is ignored). A human
  typed when a non-meta, non-subagent `user` record holds text rather than tool
  results. One record per active session, at most every 5 minutes unless the
  human typed again.
- **In-session sweeps** — the lease renewer's ownership records
  (`~/.loom/lease-renew/*.owner`, `$LOOM_LEASE_RENEW_STATE_DIR`), which hold the
  lease comment's own sweep id. A live record is `sweep.started`; a released,
  vanished or dead-owner record ends it. The outcome is `success` (with the PR
  number) only when the sweep's checkpoint
  `.loom/sweep-checkpoint/issue-N.json` was seen and has since been deleted by
  the merge; any other end is `failure`. Leases being driven ride on the most
  recently active session's `driving[]`.
- **No double emission.** A sweep id the daemon's own registry tracks is a
  dispatched sweep with its own emitter and is skipped (so is a `daemon-*` id).

**Privacy.** Only the session id and timestamps are read out of a transcript.
`OperatorPresenceRecord` has no field for prompt text, and a test pins that a
distinctive prompt never reaches the serialized record.

## Scope: hosts that run a daemon

Derivation needs a running daemon that can read the session's transcript
directory. **A daemon-less operator host emits nothing**: that is a deliberate
loss, accepted so there is one writer (the daemon) instead of two. Making the
transcript scan runnable one-shot is out of scope.

Limits: a sweep that starts and ends between two ticks is seen only as an
already-ended record (reported `failure` unless its checkpoint was caught), and
a sweep first seen live is dated from its owner record's mtime.

## Turning it off

On by default wherever a daemon runs. The privacy off-switch, per operator:

- `touch ~/.loom/operator-presence.optout` (path overridable with
  `LOOM_PRESENCE_OPTOUT_FILE`), or
- `LOOM_OPERATOR_PRESENCE=0` in the daemon's environment.

The env var wins over the file; both are read every tick, so no restart is
needed. The retired fleet key `operatorPresence.enabled` is no longer read.

## Retiring the old wiring

`resync-installed.sh` (via `provision_operator_presence`) strips the
`presence-beat.sh` hook entries and `env.LOOM_OPERATOR_PRESENCE = "1"` that the
earlier design merged into `~/.claude/settings.json`, leaving everything else
(including `LOOM_OPERATOR_PRESENCE=0`) alone. It takes effect per host on its
next resync (see [fleet-config-lifecycle](fleet-config-lifecycle.md): landed is
not effective).

## Verifying

Run a session on a daemon host, then look for `operator.presence` in SigNoz
(attributes `loom.presence.*`); for an in-session sweep, `sweep.started` /
`sweep.outcome` with `loom.dispatch_origin=operator`. The sweep ledger the
daemon keeps is `.loom/presence/derived-sweeps.json`.
