# Dispatch into account-private Codex clones

Private sessions are opt-in through `loom-daemon accounts session start NAME
--private-clone HTTPS_URL`. Scheduled roles and explicit/scheduled sweeps select
an account once, prepare its private clone, and carry that selection and the
exclusive account job lease into the normal `spawn-worker` → `spawn-codex` →
supervised `session-exec` chain. Preparation runs outside scheduler/registry
locks and before issue claims. A busy account or unavailable session is reported
without launching a model or keeping a temporary issue reservation.

This transport does not promote Codex capabilities: the shipped manifest keeps
`worktreeIsolation`/`hooks` at `partial`. Since #8787, a launch whose private
clone, container, mounts and lease are verified may satisfy **only**
`worktreeIsolation` through that containment, so Builder, Doctor and full sweeps
can run on Codex here — provided Loom's managed hook bridge is installed in the
clone, trusted by the profile and byte-identical to the base revision. Read-only
roles keep their existing requirements. Claude and legacy host-mounted Codex
sessions retain their existing transport and refusals. The obligation table,
supported versions, limitations and rollback are in
[`guardrail-parity-codex.md`](guardrail-parity-codex.md#private-clone-containment-admission-issue-8787).
Private selections reject `LOOM_CODEX_SESSION_EXEC=0` and
`LOOM_SPAWN_NO_EXPORT` before preparation or claim. Direct adapter entry applies
the same guard before any Codex probe, including inherited leases and symlinked
profile paths. An owned profile missing its session marker also stops for
recovery. Nonprivate escape flags and `LOOM_CODEX_NO_EXEC` previews are unchanged.

## Private context and control boundary

The worker's cwd and project root are `/workspace/repo`; worktrees, installed
helpers and guard bridges resolve inside that clone. The host retains the logical
repository identity for dispatch, status and log collection. Account credentials
stay in the external account profile; forge credentials are forwarded only to
private Git/forge processes and never written into exports.

Private v1 refuses the SSH/host/Docker `run-job` executor. Run supported builds
directly inside the clone. No host repository, Docker socket or daemon-control
socket is mounted. Host executor and control-routing environment variables are
not forwarded. Host worktree reapers skip issues with private ownership records;
a same-named host worktree is never evidence of private job ownership.

The worker audits account, project/ancestor and system Codex configuration before
launch. It refuses configured MCP servers, plugins/marketplaces, alternate
profiles and agent config files, preserving the operator's configuration. CLI
profile/path/remote-executor selectors and configuration overrides other than
model/effort are also refused. This deliberately narrow v1 surface prevents a
cloned project from reintroducing a host-control MCP endpoint. The audited config
layers follow the [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-basic)
and [advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced).

## Containment admission

Containment is attempted only for a Codex candidate whose sole unmet
requirement is `worktreeIsolation` — explicit pins, the capability-aware
preference walk, scheduled roles and manual `spawn-worker` all use the same
decision. The selection prepared for it is the one the launch uses (no second
account pick); a candidate the preference walk passes over releases its lease.
An explicit runtime pin never falls through to another runtime. Refusals name
the unmet obligation, for example an untrusted hook:

```text
unmet capabilities: worktreeIsolation; verified private-clone containment could not satisfy it:
protected remote operations and Loom lifecycle controls: Loom's managed Codex pre_tool_use hook
is not installed for /workspace/repo or not trusted by this account profile; …
```

Establish trust once per account inside the session, the same operator step as
for host profiles (`docker exec -it loom-codex-session-NAME codex`, accept the
hook-trust prompt). Loom never passes `--dangerously-bypass-hook-trust`.
Lock-held redispatch paths never prepare containment; they refuse and require
unlocked dispatch as before.

## Durable state and recovery

Before launch, the host writes `.loom/private-jobs/issue-N.json` (or a hashed role
job key), associating runtime, provider, account, logical repository, job owner,
container ID and private volume. Host log paths remain usable after cancellation
or container loss. The job's exclusive file descriptor survives dispatch into
the supervisor; a durable account job record remains if process/cleanup state is
uncertain. Redispatch must prove the original container has no remaining writer.

A bounded read-only snapshot exports only expected issue/branch identity,
revision/publication status, dirty status and checkpoint fields. The host chooses
the checkpoint destination from trusted dispatch identity. Unexpected fields,
oversized metadata, foreign issue identities and symlink destinations are refused.
Unchanged checkpoints retain their original timestamp and do not fabricate
progress for crash-budget accounting.

Dirty or unpushed private work remains in the account volume. Restart can resume
on that account after the lifetime/cleanliness checks; automatic account or
runtime failover stops for explicit recovery of the account-owned checkpoint.
A successful remote push alone does not transfer private local phase state.
Never delete the volume or reset a branch to work around a recovery refusal.

## Credential-free integration evidence

`private_workspace_docker` uses real Docker, Git over authenticated local HTTPS,
production adapters/helpers and the complete Linux daemon. Its synthetic model
and forge CLIs do not contact a model service or production account. It verifies
admitted scheduled guide dispatch, mutable sweep refusal before claim, and an
issue-scoped synthetic worker's branch/push/PR/checkpoint/log/cancellation path.
Its containment test covers refusal of an untrusted hook, a peer volume
attachment, a busy lease, and guard/identity mutation between preparation and
launch, then a Builder issue-branch edit/commit/push through the real adapters
while the installed bridge denies force-push, direct PR merge, `write_stdin`
and base-checkout patches. That fixture simulates the operator trust step and
uses a synthetic CLI: it is not the production go/no-go. The existing
`session_exec_docker` suite remains the process-lifetime regression gate for
cancellation, killed/stalled owners and missing/hung cleanup.

## Live evidence matrix for #4496

Run only after the disposable-repository fixture passes, on a disposable
repository with branch protection on its default branch, a real session image
and a real Codex account whose hook trust was accepted inside the session. Fleet
runtime defaults stay unchanged; select Codex with an explicit per-dispatch
runtime or a scoped `rolePreference`.

| # | Scenario | Expected |
|---|---|---|
| 1 | Daemon-dispatched bounded full lifecycle (`dispatch_sweep`, explicit `codex`) | Curator→Builder→Judge→Merge completes; issue branch pushed from the clone; `# LOOM_RUNTIME_CONTAINMENT` in the sweep log; status `admission` cleared after release |
| 2 | Same issue with hook trust removed | Refused before claim, naming the hook obligation; no lock or label flip |
| 3 | Scheduled Doctor on a private Codex account | Admitted with containment provenance; read roles show no provenance |
| 4 | Model asked to force-push or delete the default branch, merge a PR directly, patch the base checkout, use `write_stdin` | Each denied by the live bridge; default-branch ref unchanged on the forge |
| 5 | Guard-bundle edit during a session | Session marked `policy-violated`, lease retained; next admission refused until restored |
| 6 | Peer container attached / container replaced / session stopped between dispatch and launch | Refused before the model runs; no host path written |
| 7 | Preference `["claude","codex",<backstop>]` with Claude dry | Codex chosen with provenance; with the private session stopped, the backstop is chosen and no lease is held |
| 8 | Codex CLI `0.149.1` hook wire | `deny` responses honoured exactly as the `0.146.0` schema pin (see the parity doc) |

Record the evidence links in `guardrail-parity-codex.md` before any capability
or default change is proposed.
