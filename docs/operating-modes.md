# Loom operating modes

Operator direction, 2026-10-17. This document defines the intended support
contract for three operating modes. It does not claim that every current
installer or helper already satisfies that contract; known gaps are below.

The modes differ in who drives the work and which infrastructure is required.
Sweeps and individual roles are work units available within these modes, not
additional deployment modes.

| | Fleet | Local daemon | Operator-directed sessions |
| --- | --- | --- | --- |
| Work coordinator | Captain coordinating crew machines | One machine's daemon work engine | Operator directing sessions and subagents |
| Typical use | Shared, continuously operated fleet | Self-contained project in an isolated environment, especially a sandbox with sensitive data | Interactive work using sweeps or individual roles |
| Daemon | Part of the fleet work engine | Required | Must work without a daemon installed or running |
| Fleet configuration | Shared `fleet-gitops` repository | Local configuration; no fleet enrollment | Repository/session configuration; no fleet enrollment |
| Forge access | Cloudflare GitHub Workers gate fleet GitHub operations | User's local GitHub credential, or configured Gitea credential | User's local GitHub credential, or configured Gitea credential |
| Cloudflare and GitHub token load balancing | Required fleet infrastructure | Not required | Not required |
| Telemetry | Required fleet infrastructure | Not a prerequisite | Not a prerequisite |

## Fleet mode

A captain and crew machines cooperate through a fleet GitOps repository.
Fleet telemetry and Cloudflare Workers are part of this operating environment.
GitHub traffic is intended to pass through the Cloudflare proxies so the fleet
can enforce its access policy and manage shared GitHub capacity centrally.

That target does not mean all deployed traffic has already migrated. Existing
direct-credential fallback remains during migration. Retiring that fleet path
requires verified proxy coverage and reliability plus the operator's cutover
decision. Its eventual retirement does not remove direct forge access from
either local mode.

Fleet admission, credentials, telemetry destinations and deployment policy
belong to the fleet operator. Selecting a local mode does not authorize an
already managed fleet worker to bypass those policies.

## Local daemon mode

One machine runs the daemon work engine independently of the fleet. The primary
use case is a self-contained project in a sandbox with sensitive data, with
local orchestration and potentially a local/private Gitea instance.

The daemon and its agents may use the user's configured GitHub account directly.
Gitea deployments use their configured Gitea credentials. Neither requires a
captain, fleet GitOps repository, Cloudflare proxy, GitHub token pool or fleet
telemetry service. A locally unattended daemon is still this mode; unattended
execution alone does not make it a fleet worker.

Self-contained means independent of fleet services, not a promise of air-gapped
execution. The chosen forge and model runtime determine the actual network
dependencies. Sensitive-data deployments need an explicit boundary for model
requests, forge traffic and any exports. Fleet telemetry and transcript uploads
must not become implicit requirements for this mode. Local diagnostic output
can remain useful without a remote telemetry service.

## Operator-directed session mode

The operator directs one or more agent sessions and subagents, using
`/loom:sweep` for a lifecycle or the Loom roles independently. The session owns
dispatch; there is no requirement to install or run the daemon.

Loom's shell helpers can use the user's local `gh` authentication in this mode.
That is a supported direct-access path, not a temporary fleet fallback. The
same principle applies to configured Gitea access. Cloudflare, fleet telemetry
and GitHub token load balancing are not prerequisites.

Existing role approvals, repository instructions and review/merge rules still
apply. Mode selection changes orchestration and infrastructure dependencies;
it does not waive workflow gates.

## CLI packaging: proposal, not a selected implementation

Consolidating helper logic in Rust must preserve operation without the daemon
installed. One option raised by the operator is a single **`loom-tools`**
executable containing the agent-facing CLI. Under that proposal, sessions and
shell wrappers would invoke the tools directly, while `loom-daemon` would own
the continuously running work engine. Shared Rust libraries could avoid
duplicating workflow logic between them.

The executable name, command layout, extraction scope and migration plan remain
open. This direction does not require replacing every shell script now, adding
a daemon token-vending service, or moving local users onto Cloudflare. Current
language-policy guidance is not evidence that the proposed package exists.

## Current implementation gaps and follow-up checks

Several current helpers require the `loom-daemon` executable even when no
daemon service is running. For example,
[`worktree.sh`](../defaults/scripts/worktree.sh) delegates worktree operations
to it, and [`forge-helpers.sh`](../defaults/scripts/lib/forge-helpers.sh) requires
it for some writes. Consequently, “no service running” is not yet proof of
“no daemon installed.” Audit required helper commands and installation paths
before claiming complete session-mode support.

[`sweep-lease-renew.sh`](../defaults/scripts/sweep-lease-renew.sh) already calls
`loom-daemon forge token` and implements credential fallback. Its existence
does not select token vending as the future fleet architecture, nor make App
tokens necessary for local modes.

Future implementation verification should cover all three modes separately:

- Fleet work through the required proxy and telemetry services, including
  explicit migration/failure behavior.
- Local daemon work with user GitHub credentials and with Gitea, without fleet
  configuration or services.
- Operator-directed sweeps and individual roles on a machine without the
  daemon installed, without requiring fleet services or telemetry exports.

These are verification targets, not results from an end-to-end test performed
for this documentation change.
