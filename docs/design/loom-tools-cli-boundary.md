# Loom tools CLI boundary and migration map

This is the Phase 1 design artifact for epic #12708 (issue #12840). It records
which agent-facing operations become standalone `loom-tools` commands, which
stay in `loom-daemon`, where the shared-code seam sits, and how mode,
write admission and diagnostics behave on the tools path. It is design and
inventory only: it changes no source, no Cargo manifest, no config and no
deployed daemon behavior.

**Status: proposal for review.** The operating-mode contract it serves is
[`docs/operating-modes.md`](../operating-modes.md) (#12706); the approved
direction is in the epic's body and its two Fable 5.1 review comments.

**Evidence convention.** A statement about code carries the file (and, where
it matters, the symbol) that establishes it, or the command that was run, or
the marker **not traced**. Nothing is copied from the epic's prose without
re-reading the source it names. Counts are from the pinned tree below.

## Contents

1. [Pinned inputs](#1-pinned-inputs)
2. [Decisions in one page](#2-decisions-in-one-page)
3. [Inventory](#3-inventory)
4. [Retained shell dependencies](#4-retained-shell-dependencies)
5. [Command mapping](#5-command-mapping)
6. [The shared CLI seam](#6-the-shared-cli-seam)
7. [Mode resolution and rollout](#7-mode-resolution-and-rollout)
8. [Standalone write admission](#8-standalone-write-admission)
9. [Diagnostics versus telemetry](#9-diagnostics-versus-telemetry)
10. [The first slice](#10-the-first-slice)
11. [Phase 2 hand-off and serialization](#11-phase-2-hand-off-and-serialization)
12. [Not traced, and open questions](#12-not-traced-and-open-questions)
13. [Phase 3a: forge verbs (#14395, #14519)](#13-phase-3a-forge-verbs-14395-14519)
14. [Phase 3b: park records and operator decisions (#14464)](#14-phase-3b-park-records-and-operator-decisions-14464)
15. [Phase 3c: issue leases (#14488)](#15-phase-3c-issue-leases-14488)
16. [Phase 3b: the remaining merge-queue actions (#14520)](#16-phase-3b-the-remaining-merge-queue-actions-14520)

## 1. Pinned inputs

| Input | Value |
| --- | --- |
| Source tree | `origin/main` at `13c4e291c23b485a4e671845ebf2dd4876e3a1e5` (workspace version 0.19.1193, `Cargo.toml` `[workspace.package]`) |
| Epic body | #12708 as read 2026-10-19; it cites `b6bab097048198435a46fc32c620fcaa93c6ba32`, which this document re-verified rather than trusted |
| Top-level command list | `loom-daemon --help` of the installed 0.19.1191 binary (two patch releases behind the tree; the tree's `Commands` enum and the flattened `ScriptPortCommand` enum are the authority, the help listing is a cross-check) |

Re-run before acting on any number here: `git grep` the symbol named in the
row. The counts below move with every merged port.

## 2. Decisions in one page

1. **One package, two binaries.** `loom-tools` is a second `[[bin]]` in the
   `loom-daemon` package, as the epic requires. The current package has no
   explicit `[[bin]]` section (`loom-daemon/Cargo.toml` has none; cargo infers
   `src/main.rs`). Phase 2 adds a `[[bin]] name = "loom-daemon"` with the
   current path and a `[[bin]] name = "loom-tools"` beside it.
2. **The seam is "everything under `cli/` plus the clap tree moves into the
   library".** Today the clap tree (`Cli`, `Commands`, twelve `*Action`
   enums) and `handle_cli_command` live in `loom-daemon/src/main.rs`, and
   the 19 files under `loom-daemon/src/cli/` (14 non-test) that name
   `crate::Commands`, `crate::Cli` or a `crate::*Action` enum cannot compile
   outside that binary crate (section 6). Moving those definitions into `loom_daemon::cli` is the
   mechanical, behavior-preserving change Phase 2 makes.
3. **`loom-tools` gets its own, smaller clap root.** It composes handler
   functions and argument structs from the library. It does not link
   `daemon_service` and it does not carry the daemon-only variants.
4. **First slice: `strip-ansi` and `checkpoint`.** Both are pure
   filesystem/text over code that already lives in the library
   (`script_helpers::log_filter`, `script_helpers::checkpoints`). One
   sub-operation, `checkpoint read` with an issue number, makes a forge read
   and is documented as such (section 10).
5. **Mode is resolved before routing, and an unset mode with inherited fleet
   routing variables is a configuration error on the tools path.** No new
   control system: the key goes through `config_resolver` tiers and fleet
   rendering (section 7). No `mode` key exists in the tree today (section 7.1).
6. **Standalone write admission is the existing `write_scope` rule, unchanged.**
   The registry is consulted only as a second way to be "managed" and its
   absence already degrades to the checkout rule (section 8).
7. **Diagnostics stay as they are.** The two temp-directory sinks and their
   `off` opt-outs are documented, not changed (section 9).

## 3. Inventory

Classification vocabulary, used in every table below:

- **S** standalone operation: needs only the checkout, the user's forge
  credential and local files.
- **C** explicit service client: talks to a running daemon (Unix socket) or
  edits daemon-owned state; must be distinguishable from S and may never be a
  hidden prerequisite of a session.
- **D** daemon-only behavior: the continuous engine or a service it hosts.
- **B** bootstrap/platform glue: finds, installs, versions or hooks the
  binaries themselves.

### 3.1 Rust CLI surface

`loom-daemon` has 156 top-level subcommands in the installed 0.19.1191 help
(`loom-daemon --help | awk '/^Commands:/{f=1;next}/^Options:/{f=0}f&&/^  [a-z]/'`).
They come from three enums: `Commands` in `loom-daemon/src/main.rs` (55
variants), the flattened `ScriptPortCommand` in
`loom-daemon/src/cli/script_ports.rs` (the `main.rs` comment there says the
flattening exists so `main.rs` stays frozen), and the flattened
`TelemetryCommand` (`cli/telemetry.rs`). Classification by family; a command
not named below is **not traced** individually and is placed with its family
by name and module only.

| Family | Commands | Class | Basis |
| --- | --- | --- | --- |
| Text/filesystem helpers | `strip-ansi`, `checkpoint`, `resolve-model`, `sweep-experiment`, `claim`, `cargo-target-dir`, `skip-labels`, `git-blob-lines`, `host-id`, `classify-error`, `retry-classify` | S | handlers in `cli/legacy_script_cmds.rs`, `script_helpers/*`; no socket use found. `sweep-experiment` reaches `workspace_registry` (`script_helpers/fleet_experiment.rs`), so its `plan\|start\|stop` verbs are C for registry reasons (see 3.6) |
| Worktree operations | `worktree-lock`, `worktree-wip`, `worktree-remove`, `worktree-link`, `worktree-cleanup`, `worktree-reset`, `worktree-branch-conflict`, `worktree-submodules`, `worktree-upstream`, `worktree-stale-ref`, `worktree-closed-pr-branch`, `worktree-sparse`, `worktree-base`, `worktree-check`, `worktree-existing`, `worktree-branch-reuse`, `worktree-open-pr`, `worktree-state`, `stack-parent`, `reconcile-stack`, `orphan-replace`, `push-lease`, `branch-update` | S | modules `cli/worktree_*.rs`; none of them appears in the `workspace_registry` grep over `cli/` (3.6). Several make forge reads (`worktree-open-pr`, `worktree-closed-pr-branch`): S with a forge dependency, not offline |
| Forge reads/writes | `forge` (all verbs), `merge-pr`, `merge-pr-refs`, `merge-group-ci`, `labels`, `pr-queue`, `park-record`, `operator-decision`, `champion-hold`, `notify-cleared-blockers`, `check-stale-blocked`, `premise-check`, `duplicate-scan`, `curation-complete`, `check-points-marker`, `label-duplicates`, `forge-probe`, `forge-inventory`, `affected-files` | S | go through `gh_invocation`/`forge_cmd`; writes are gated by `write_scope` (section 8). Gitea: `forge` declines with `EX_FORGE_DECLINED = 3` (`forge_cmd.rs:94`) and the shell adapter carries Gitea |
| Claims and leases | `lease` (ensure/release/renewer/pr/co-occupancy), `inflight`, `noop-cooldown`, `dispatch-backoff`, `quarantine`, `stashes`, `peer-claims` | mixed | `lease` is forge-lease S (ADR-0025). `peer-claims` and `queue` do a daemon `DaemonStatus` round trip (`daemon_service.rs`, `Commands::PeerClaims`, `Commands::Queue`): C. `noop-cooldown`, `dispatch-backoff`, `quarantine`: **not traced** |
| Phase validation | `validate-phase`, `preflight`, `sweep-checkpoint`, `sweep-outcomes`, `overlap-replay` | S (forge-calling) | `validate_phase.rs` runs `gh` reads; it is not offline (section 10.3). `sweep-outcomes` reads the registry (`cli/sweep_outcomes_cli.rs:253`) |
| Config inspection | `config`, `journal`, `fleet-config`, `install-compat`, `guards`, `check-guard-wiring`, `check-private-control-version`, `check-renovate-labels`, `role-tool-policy`, `roles`, `runtime-launch-env`, `ram-scope-limit`, `secret-scan`, `hooks-home`, `generate-agent-skills` | S | read files; `fleet-config` is C-for-registry (`cli/fleet_config.rs` appears in the registry grep) |
| Release/install | `release-fetch`, `release-resolve`, `release-explain`, `install-binary`, `daemon-update`, `resync-pin`, `resync-payload`, `init`, `update-gitignore` | B | install and self-update paths; `init` carries the embedded payload (3.5) |
| Daemon lifecycle and engine | `serve`, `daemon-start`, `daemon-watchdog`, `dispatch`, `cancel`, `restart`, `watch`, `status`, `health`, `calibrate`, `workspace`, `fleet`, `fleet-dispatch`, `fleet-captain`, `fleet-send`, `tokens`, `api-keys`, `accounts`, `claude-config`, `worker`, `spawn-worker`, `agent-spawn`, `agent-wait`, `session-exec`, `private-workspace`, `hyperparams`, `runtime-tool`, `native-mcp`, `agent-resume`, `roll-pause`, `codex-hooks`, `host`, `jev-merge-risk` | C or D | socket round trips, registry edits, worker supervision. `dispatch` and `cancel` load the registry (`cli/dispatch.rs:228`, `cli/cancel.rs:106`) |
| Telemetry and transcripts | `telemetry*`, `usage`, `usage-report`, `ingest-transcripts`, `archive-transcripts`, `ci-telemetry`, `record-rework`, `opencode-usage`, `codex-usage`, `pi-usage` | D (export) | section 9: these are exports, not local diagnostics |
| Cleanup | `clean`, `cleanup`, `recover-orphans`, `target-dir-gc`, `tmpfs-scratch-gc`, `work-docs` | S | `clean`, `cleanup`, `recover-orphans` take `crate::CleanupAction` (a binary-private type) |

The agent `gh` front is a second entry into the same binary: `dispatch_if_front`
(`agent_gh/mod.rs`) runs before clap and fires when argv[0] is `gh` or the
subcommand is `gh`/`gh-shim`. It is **B** (section 4.2).

### 3.2 Shell wrappers

`defaults/scripts/` holds 111 `*.sh` files (`ls defaults/scripts/*.sh | wc -l`).
79 mention `loom-daemon`, `LOOM_DAEMON_BIN` or `loom_exec_script_helper`; 17
call `loom_exec_script_helper` directly (one-line stubs). The rest are
orchestration scripts that delegate pieces. Shapes:

| Shape | Example | Behavior with no daemon | Class |
| --- | --- | --- | --- |
| **A. pure stub** | `checkpoint.sh`, `strip-ansi.sh` | `lib/script-helper.sh` prints an actionable error and exits `LOOM_SCRIPT_HELPER_MISSING_RC` (default 1; stubs whose subcommand uses nonzero as data set 2) | S, exec only |
| **B. orchestrator with soft delegation** | `worktree.sh` | worktree cleanup is skipped silently when no daemon is found (`worktree.sh:450-473`, "NO DAEMON MEANS NO CLEANUP, deliberately"); branch-reuse steps are skipped (`:525-545`) | S, degraded |
| **C. orchestrator with hard delegation** | `forge-helpers.sh` write helpers | `loom_write_repo` falls back to "origin only, only from a checkout whose one remote is origin" (`forge-helpers.sh:1104`); comment posting requires a minimum daemon (`:1416`) | S, with a documented weaker fallback |
| **D. hook** | `defaults/hooks/*.sh` | see 3.3 | B |

Shape B and C are why the epic says moving a command name does not remove a
dependency. Each needs its own line in Phase 3's caller-chain trace; this
document does not trace them individually (section 12).

### 3.3 Hooks

`defaults/hooks/` has 13 hook files. Daemon mentions per file
(`grep -c loom-daemon defaults/hooks/*.sh`): `guard-loom-workflow.sh` 21,
`guard-destructive-generic.sh` 8, `guard-background-subagents.sh` 3,
`guard-uncommitted-work.sh` 3, `guard-worktree-paths.sh` 2, `guard-mcp-tools.sh`
2, `roll-pause.sh` 2, `gh-front-env.sh` 2, `guard-codex-bridge.sh` 1, the rest 0.

| Hook | Daemon use | Failure mode without daemon | Class |
| --- | --- | --- | --- |
| `gh-front-env.sh` (SessionStart) | `exec loom-daemon gh-shim session-env` | exits 0 silently when no binary is located (`[[ -n "$BIN" && -x "$BIN" ]] \|\| exit 0`); the hook "must never fail a session" | B |
| `guard-loom-workflow.sh` | `role-tool-policy check`, `cargo-target-dir guard`, and an ask on registry-mutating `workspace` commands | `role-tool-policy` unavailable for a restricted role **denies** (`:1787`, #8256); the cargo-target guard is an optimisation, its failure path is **not traced** | B |
| other guards | `loom-daemon guard-*` helpers | **not traced** per hook | B |

The SessionStart hook and the `gh-shim` PATH prepend are the only way a
session gets the ETag/front behavior. With no daemon installed the session uses
plain `gh` with the user's credential, which is exactly the session-mode
contract in `docs/operating-modes.md`.

### 3.4 Role and skill instructions

Role prompts in `defaults/roles/` name daemon subcommands directly
(`grep -c loom-daemon defaults/roles/*.md`: `loom.md` 60, `judge.md` 48,
`doctor.md` 34, `guide.md` 19, `curator.md` 18, `builder.md` 15, `auditor.md` 10,
`builder-pr.md` 9; 23 files mention it). Most-cited invocations
(`grep -rhoE 'loom-daemon [a-z-]+( [a-z-]+)?' defaults/roles/*.md | sort | uniq -c`):
`status` 30, `forge wait-checks` 28, `lease pr` 19, `park-record apply` 14,
`branch-update strategy` 11, `forge issue` 9, `pr-queue` 8, `operator-decision
apply` 8, `cancel` 8, `labels list` 7, `dispatch` 6.

Consequences for migration: `status`, `cancel` and `dispatch` are C/D and
belong to daemon-mode roles (`loom.md`) only; a session-mode role must not
cite them. The rest are S. Switching prompts to `loom-tools` is Phase 3 and
must keep the old spelling working for the compatibility window (epic Phase 3).

Skill and slash-command copies under `.claude/commands/loom/` are generated
from `defaults/` (`loom-daemon generate-agent-skills`); they are installed
payload (3.5), not a separate source.

### 3.5 Installed payload consumers

- `loom-daemon/build.rs` `pack_install_payload` embeds the tracked files of
  `defaults/` into the binary; `loom-daemon init` and `resync-payload`
  materialise it.
- `loom-daemon/src/install_compat.rs` states the contract: `SUPPORTS_INSTALLED
  = "0.19.0"`, `REQUIRES_DAEMON = "0.19.1088"`, and
  `DAEMON_INVOKED_INSTALLED_FILES` (the installed files the daemon executes).
  `loom-daemon install-compat show|check` reads it.
- Scripts declare per-stub floors with `# requires-daemon: <subcommand> >=
  <version>` comments (162 matches, `grep -rn requires-daemon defaults/scripts |
  wc -l`), enforced by `loom_daemon_exec_checked` in
  `lib/locate-daemon-bin.sh:584`.
- Release assets: `.github/workflows/release.yml` publishes
  `loom-daemon-<triple>` plus `.sha256` for `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, with macOS
  codesigning on the first.

### 3.6 Rust implementation dependencies

**Binary-private references** (what cannot move without a decision). From
`grep -rlE 'crate::(Commands|Cli|[A-Za-z]+Action)\b' loom-daemon/src/cli`:

- `crate::Commands` (15 uses) and `crate::Cli` (7 uses) across
  `cli/{forge_action,legacy_script_cmds,stale_blocked,usage_report_cli,
  sweep_outcomes_cli,whole_cli_parse}.rs` and three test files
  (`grep -rhoE 'crate::(Commands|Cli)\b' loom-daemon/src/cli | sort | uniq -c`).
- Twelve `*Action` enums defined in `main.rs`: `CheckpointAction`, `WorkspaceAction`,
  `FleetAction`, `WatchAction`, `QuarantineAction`, `DispatchBackoffAction`,
  `NoopCooldownAction`, `StashesAction`, `TokensAction`, `ClaudeConfigAction`,
  `PinAction`, `CleanupAction`. (`AccountsAction` is already in
  `cli/accounts_args.rs`.)
- Files importing any of the above (14 non-test; the 19-file count in
  section 2 adds `whole_cli_parse.rs` and four test files): `accounts.rs`, `accounts_args.rs`,
  `cleanup_ops.rs`, `dispatch_backoff.rs`, `fleet_config.rs`,
  `forge_action.rs`, `legacy_script_cmds.rs`, `noop_cooldown.rs`,
  `quarantine.rs`, `stale_blocked.rs`, `stashes.rs`, `sweep_outcomes_cli.rs`,
  `usage_report_cli.rs`, `watch.rs`.
- `main.rs` also sets `#![recursion_limit = "512"]` (line 8) for
  `cli::status_render`'s large `json!`; the attribute is crate-level, so it
  moves to `lib.rs` with `status_render`.
- `main.rs` mounts test-only modules `live_gh_guard` and `whole_cli_parse`
  through `cli/mod.rs`.

**Library reach into the workspace registry** (commands that read daemon
workspace-registry state). From
`grep -rlE 'workspace_registry|WorkspaceRegistry' loom-daemon/src`:
`cli/{accounts_session,cancel,dispatch,fleet_config,fleet_experiment,
labels_migrate,sweep_outcomes_cli,tokens,workspace_fleet}.rs`, and in the
library `write_scope.rs`, `script_helpers/fleet_experiment.rs`,
`sweep_journal.rs`, `tokens_pool/session_*.rs`, `safehouse.rs`,
`issue_state_read/daemon.rs`, `worktree_reaper.rs`. Which of these degrade to
"no registry" and which fail is **not traced** except `write_scope`
(section 8) and `cancel` (`unwrap_or_default()` at `cancel.rs:106`).

**Build coupling.** `loom-daemon/Cargo.toml` carries the full dependency set
(tokio, rusqlite bundled, reqwest, clap, ...). A second bin in the same
package links the same library, so the tools binary pulls in every library
dependency regardless of which modules it calls. This design does not claim a
size or compile-time benefit; Phase 2 measures both (epic). Coordination:
`docs/design/loom-daemon-lib-test-shrink.md` and #12134 address compile-unit
coupling and must be read before the Cargo edit.

## 4. Retained shell dependencies

These remain shell after the migration, with the reason. Everything else in 3.2
is a candidate for retirement in Phase 4.

### 4.1 SessionStart hook (`defaults/hooks/gh-front-env.sh`)

Retained: it must run before any binary is guaranteed to exist, must never
fail a session, and exists to prepend the front to `PATH`. It locates the
binary through `locate-daemon-bin.sh` and execs `gh-shim session-env`. In
session mode with no daemon it already exits 0 (3.3). Open item: whether the
tools binary should offer `gh-shim` at all, given the front serves daemon-fed
reader routing. Decision for Phase 2: **no**; `gh-shim` stays daemon-named
(it depends on `forge_gateway`/`forge_identity` reader state), and session
mode uses plain `gh`.

### 4.2 Agent `gh` wrapper (the front)

`agent_gh::dispatch_if_front` runs in `daemon_service::run_daemon` before
`Cli::parse`. Class B for the entry mechanics (argv[0] == `gh`), S for the
classification code (`agent_gh/classify.rs`). It stays in the daemon binary.
Its reads route gateway-first (`LOOM_GH_GATEWAY_DIR`), then reader pool
(`LOOM_GH_READER_ROOT`), then the session's credential
(`agent_gh/mod.rs` header). That ordering is exactly what section 7 must keep
from leaking into session mode.

### 4.3 Binary locator and version-floor machinery

`defaults/scripts/lib/locate-daemon-bin.sh`, `lib/script-helper.sh` and
`lib/canonical-daemon-path.sh` resolve a binary (precedence: `LOOM_DAEMON_BIN`,
optional repo build, `PATH`, machine install), enforce `# requires-daemon:`
floors, and propagate exit codes unchanged (`exec`). The exit-code discipline
is load-bearing: `resolve-model --task-alias` exits 3 for "no mapping", `claim`
uses 1/2/3/4 as data, and `detect-dependency-cycle` sets its missing-binary code
to 2 because 1 means "cycle found" (`script-helper.sh` header). Phase 2 keeps
this as the **one** locator and teaches it a tools-first order; it does not
write a second resolver.

`lib/loom-tools.sh` is a historical name collision: it is the shim for the
retired Python `loom-tools` package (ADR-0013) and now only provides
`_find_repo_root` plus a loud failure shim `run_loom_tool`. The new executable
inherits an ambiguous name. Epic Phase 2 resolves it; the rename is a
compatibility edit across `agent-spawn.sh` and `agent-wait.sh`, which source it.

#### 4.3.1 Tools-first order and `capabilities --json` (#12939)

For `strip-ansi` and `checkpoint` the locator resolves, in order: a
`LOOM_DAEMON_BIN` pin (explicit, still names the daemon; `LOOM_TOOLS_BIN`
overrides it), then `loom-tools` (`LOOM_TOOLS_BIN`, `PATH`, the machine
install, a repo build), then `loom-daemon` as a logged **transitional**
fallback. The fallback is off under `LOOM_MODE=session` and when
`LOOM_TOOLS_DAEMON_FALLBACK=0`; with nothing resolvable the stub exits
non-zero. Other commands resolve the daemon only.

`loom-tools capabilities --json [--repo PATH]` prints the same object as
`loom-daemon install-compat show --json` (one Rust builder,
`install_compat::capabilities_json`), plus `"binary": "loom-tools"`. Keys:
`schema_version` (1), `running`, `supports_installed`, `requires_daemon`,
`payload_identity`, `invoked_files`, `installed`, `classification`. Removing
or renaming a key bumps `schema_version`.

`payload_identity` is compared for equality only. `resync-installed.sh`
refuses when tools and daemon report different identities, or when the tools
binary's `requires_daemon` is older than the payload being installed. Newer
tools than the payload (rollback) proceeds; an unreadable identity fails open.

### 4.4 Handlers that reach daemon workspace-registry state

Listed in 3.6. They are **C** and stay in `loom-daemon` unless a verb is
shown to degrade cleanly with an empty registry (the `cancel` precedent is
`WorkspaceRegistry::load_default().unwrap_or_default()`).

## 5. Command mapping

Owner = the binary that exposes the command after Phase 3. "Compatible"
means the new command keeps flags, stdout, stdin, working-directory behavior
and exit codes of today's `loom-daemon <name>`, and the shell stub keeps its
name. The old name keeps working through the compatibility window as a thin
delegate to the same library function (epic Phase 2), never a second
implementation.

| Command | Owner after migration | Exit codes to preserve | Binary-private reference that stays out of the shared crate |
| --- | --- | --- | --- |
| `strip-ansi [--file P]` | `loom-tools` | 0; a broken downstream pipe is success (`legacy_script_cmds.rs:19`) | none; `handle_strip_ansi_command` takes plain `Option<&str>` |
| `checkpoint write\|read\|clear\|stages` | `loom-tools` | `write`/`clear` 1 on failure; `read` and `stages` always 0 (`checkpoint.sh` comment) | `CheckpointAction` (in `main.rs`) moves to the library with its handler |
| `resolve-model` | `loom-tools` (moved, #13878) | 0, 2 (clap), **3 with no output** = no mapping (`legacy_script_cmds.rs`) | none |
| `claim` | `loom-tools` (slice 2) | 0 ok, 1 already claimed/error, 2 bad args, 3 not found, 4 agent mismatch (`Commands::Claim` doc) | none; calls `script_helpers::claim::run` |
| `validate-phase` | `loom-tools` (slice 3) | 0 satisfied, 1 failed, 2 bad args (`Commands::ValidatePhase` doc) | forge reads (section 10.3) |
| worktree family | `loom-tools` | per command; `worktree-cleanup` is best-effort, soft | none identified; `CleanupAction` is for `clean`/`cleanup`, not these |
| `forge ...` reads and writes | `loom-tools`, after reads-before-writes sequencing; coordinate #8191 for `merge-pr` | `EX_TEMPFAIL` 75 for shed reads (`agent_gh/mod.rs`), `EX_FORGE_DECLINED` 3 for Gitea | `cli/forge_action.rs` names `crate::Commands` (3.6), so the `forge` tree moves with the seam; whether it needs more than a path rewrite is **not traced** |
| `lease ...` | `loom-tools` | unchanged | none identified |
| `status`, `health`, `queue`, `peer-claims`, `cancel`, `dispatch`, `watch`, `restart`, `serve`, `workspace`, `fleet*`, `tokens`, `accounts`, `worker`, `spawn-worker`, `session-exec` | `loom-daemon` | unchanged | clap root and all `*Action` enums for these remain in the daemon binary's root |
| telemetry/transcript exports | `loom-daemon` | unchanged | n/a |
| `gh` / `gh-shim` | `loom-daemon` | transparent exec status | `dispatch_if_front` (pre-clap) |

Contract rules for every migrated command:

- The shell stub's resolution failure never reads as a workflow result: a
  missing tools binary exits with the stub's reserved "could not run" code
  (2 where the subcommand uses 1 as data), as `LOOM_SCRIPT_HELPER_MISSING_RC`
  already does.
- Nonzero exit codes used as data are tested before the move (epic success
  criteria); the existing coverage is the unit tests in
  `script_helpers/log_filter.rs` (20 `#[test]`) and `checkpoints.rs` (16),
  plus `defaults/scripts/tests/test-script-helper-version-floor.sh`.
- Credential errors and guarded destructive operations keep their behavior;
  `tests/` and `loom-daemon/tests/*_differential.rs` are the retained oracles
  for the worktree family.

## 6. The shared CLI seam

### 6.1 What blocks a second binary today

The whole subcommand tree is defined in the binary crate root. Facts:

1. `main.rs` declares `mod cli;` and `mod daemon_service;` (lines 19-20), so
   both are private to the `loom-daemon` bin crate. `loom_daemon::` (the
   library) cannot name `cli::*`.
2. `Cli`, `Commands` and the `*Action` enums are defined in `main.rs`; the
   files in 3.6 import them as `crate::...`, which resolves to the binary root.
3. `handle_cli_command` is in `main.rs` and matches every `Commands` variant;
   async variants are dispatched earlier in `daemon_service::run_daemon`
   (`Status`, `Health`, `PeerClaims`, `Queue`, `Dispatch`, `Cancel`, ...).
4. `daemon_service::run_daemon` is the real entry: `dispatch_if_front()`,
   `Cli::parse()`, `forge_gateway::enable_cli_reads()` for any subcommand,
   else daemon startup.

### 6.2 The seam

The exact change Phase 2 makes, in order, as separate mechanical commits:

1. Create `loom-daemon/src/cli` as a library module (`pub mod cli;` in
   `lib.rs`); move the existing `cli/` directory and its `mod.rs` unchanged.
2. Move `Cli`, `Commands`, the `*Action` enums and `handle_cli_command` from
   `main.rs` into the library, re-exported so `main.rs` keeps `Cli::parse()`.
   Move `#![recursion_limit = "512"]` to `lib.rs`.
3. Replace `crate::Commands`/`crate::Cli`/`crate::*Action` in the files of 3.6
   with `crate::cli::...` paths. This is a path rewrite only.
4. Add the new tools entry file (a new path, for example
   `loom-daemon/src/bin/loom_tools.rs` or a `[[bin]] path`; it does not exist
   today) with its own clap root `ToolsCli`/`ToolsCommands`. It
   composes the *same* argument structs and handler functions (`CheckpointAction`,
   `handle_checkpoint_command`, ...) and calls
   `forge_gateway::enable_cli_reads()` only after the mode gate of section 7.
5. `main.rs` for the daemon keeps `mod daemon_service;` (it is not a CLI
   handler, `cli/mod.rs` header) and `dispatch_if_front`.

**The tools target links `loom_daemon` (the library) and not `daemon_service`.**
That holds because `daemon_service` is a private module of the daemon bin
crate: nothing in the library names it. The mechanical check Phase 2 adds:

```bash
# built artifact must not contain the daemon entry point or its service loop
cargo build --release --bin loom-tools
nm -C target/release/loom-tools | grep -E 'daemon_service|run_daemon' && exit 1
```

(Name resolution for the `nm` check, and whether symbols are stripped in the
release profile, are **not traced**; Phase 2 verifies that the check can
actually fail before relying on it.) A second, source-level check: `grep -rn
daemon_service loom-daemon/src --include='*.rs' | grep -v '^loom-daemon/src/main.rs'
| grep -v '^loom-daemon/src/daemon_service'` must be empty.

### 6.3 What the seam does not do

It does not extract a crate (out of scope per the epic), does not claim a
smaller dependency graph, and does not change daemon output. Moving
`cli/` into the library enlarges the library's public surface;
whether to keep these modules `pub(crate)` behind a single `pub use` is a
Phase 2 decision recorded in its PR.

### 6.4 Serialization

Shared Cargo and CLI-tree edits are **serialized**. Specifically, one PR at a
time may edit `loom-daemon/Cargo.toml`, `loom-daemon/src/main.rs`,
`loom-daemon/src/cli/mod.rs`, `loom-daemon/src/lib.rs` or `loom-daemon/build.rs`
for this epic, and Phase 2's move lands before any family PR edits a handler's
signature. Independent family PRs (Phase 3) start only after the seam PR is
merged. Reviewers: reject a Phase 3 PR that touches the seam files.

## 7. Mode resolution and rollout

### 7.1 What exists

- No `mode` key is read anywhere: `git grep -n 'local-daemon'` finds only
  `docs/operating-modes.md`; `config_resolver.rs` has no mode lookup.
- Config tiers (`docs/design/config-resolution-tiers.md`, lowest to highest):
  machine defaults, `.loom/config.json`, `.loom-project/project.json`, the
  per-repo fleet tier `.loom-local/fleet-repo.json`, then
  `.loom-local/local.json`; env > config > default for individual knobs.
- Fleet sync (`fleet_sync.rs`) renders the fleet store into those files at
  startup and, with `fleet.autoApply` on, on timer passes; otherwise it
  reports drift. `loom-daemon validate` reports leaf keys a fleet tier sets
  differently from another tier (#11703).
- Routing today is driven by two **environment variables** that a dispatcher
  exports into agent sessions and that anything the session spawns inherits:
  `LOOM_GH_GATEWAY_DIR` (`forge_gateway::DIR_ENV`, `forge_gateway.rs:99`) and
  `LOOM_GH_READER_ROOT` (`forge_identity::FRONT_ROOT_ENV`, `forge_identity.rs:139`).
  `enable_cli_reads()` (`forge_gateway.rs:336`) makes any subcommand that sees
  either one **reader-only**: its routable reads are shed (`EX_TEMPFAIL` 75)
  rather than sent to the writer (`CLI_READER_ONLY`, operator ruling #12394).

The last point is the hazard. A tools command run in a shell that inherited a
fleet session's environment would silently shed reads. This is the failure the
epic names.

### 7.2 Proposed setting

One key through the existing path, spelled as the epic and
`operating-modes.md` already propose: `mode` with values `fleet`, `local-daemon`,
`session`. Where it is read: `config_resolver` like any other key, so tiers
and the fleet render apply; it is a top-level key because it gates routing for
every command. The effective value and its tier are reported by `loom-tools
config get mode` (the existing `config` family already prints value and
source, `cli/config_get.rs`).

### 7.3 Resolution table (tools path)

`R` = at least one of `LOOM_GH_GATEWAY_DIR`, `LOOM_GH_READER_ROOT` is set and
non-empty. The check is on the environment of the tools process.

| Resolved `mode` | `R` | Behavior |
| --- | --- | --- |
| `session` | no | local route: user's `gh` or Gitea credential; no gateway, no readers |
| `session` or `local-daemon` | yes | **ignore the inherited variables** for routing and warn once on stderr naming the variables (they came from a parent fleet session). The variables are not honored, not shed against |
| `local-daemon` | no | local route |
| `fleet` | yes | fleet route; validate the gateway/reader directories exist and are private (`reader_root_from` requires an absolute path with a `.loom/` dir); a failure is an error, never a silent fallback to local |
| `fleet` | no | error: fleet mode with no route configured |
| unset | no | error: `mode` is unset. Actionable text names the three values and `loom-tools config get mode` |
| unset | yes | **error, not session mode and not shed reads**, text names the inherited variables, says a fleet session's environment leaked or `mode` was never rendered, and gives both fixes |
| invalid value | any | error naming the accepted values and the file/tier the value came from |

Conflicting controls: if env `LOOM_MODE` (the env override, per
`env > config > default`) and the resolved tier disagree, env wins, and the
tools command prints both with their sources; on a **fleet-enrolled host** a
tier-4 (`fleet-repo.json`) `mode` cannot be overridden down by a lower tier,
because the resolver already gives that tier precedence over the tracked tiers,
and `.loom-local/local.json` (tier 5) is the only tier above it. A
`local.json` `mode` that disagrees with the fleet tier on a managed host is a
reconciliation conflict surfaced by `validate`; the tools path refuses to run
a write with an unresolved conflict rather than choosing. This keeps "selecting
a local mode does not authorize a managed worker to bypass fleet policy"
(`operating-modes.md`) without a second control system.

The unset-mode errors above are scoped to the **tools** path, as the epic
says. The daemon and the old `loom-daemon <name>` entry keep their current
behavior throughout (section 7.4), so no deployed behavior changes.

### 7.4 Rollout order (nothing switches before step 4)

1. **Land the key as read-only.** `config get mode` reports value/source;
   nothing consumes it. Fleet gitops renders `mode: fleet` to managed hosts'
   files first (fleet store change, separate repo, separately authorized).
2. **Verify rendering.** `loom-daemon validate` shows no conflict on any
   managed host; hosts without the key are listed.
3. **Tools path enforces** (7.3 table) only for `loom-tools` commands.
   `loom-daemon <name>` ignores `mode`.
4. **Existing fleet consumers switch** (roles, stubs) only after step 2 is
   clean for every managed host, so no consumer meets "unset + R".
5. Local operators opt in by writing `mode` to `local.json`.

Hosts with `R` set and `mode` unset are exactly the ones step 2 must find
before step 4; they are the migration gate.

## 8. Standalone write admission

### 8.1 The existing rule

`write_scope.rs` (#9548): a write to `OWNER/REPO` is allowed only when (1)
the target is what the caller meant (the one `gh` resolves from the checkout
must be that checkout's `origin`, unless `--repo` names it), (2) the repo is
managed, (3) the credential carrying the write has WRITE (`probe`). Anything
unverifiable is a refusal; reads are never gated.

### 8.2 Standalone form, and why it needs no new authority

Rule 2 is evaluated by `is_managed` (`write_scope.rs:245`):

```text
if checkout/.loom is a directory AND checkout origin == repo  -> managed
else the workspace registry's effective roots' origins contain repo
```

The first branch needs no registry. The second loads it with
`WorkspaceRegistry::load_default().unwrap_or_default()`: a missing or unreadable
registry is an empty registry, which makes the second branch false and never
errors. So with no daemon state:

- an in-checkout write to the checkout's own `origin` is **admitted** (the
  first branch);
- any other repository, or a checkout without `.loom/`, is **refused** with
  `RefusalClass::UnmanagedRepo` - no extra and no off-origin authority;
- the permission probe (`GhProbe` or, on Gitea, `GiteaProbe`) is unchanged:
  `Unknown` -> refusal (`Unverifiable`), `Insufficient` -> refusal. Failed or
  unavailable permission checks refuse writes.

The standalone form is therefore the existing function with no daemon: no code
change to the rule is proposed. What Phase 2 must do is call it from the tools
path (`may_write_from_at`) for every write verb and keep
`tests::shell_write_paths_are_vetted` (named in the `write_scope.rs` header) and
`daemon_write_paths_are_scoped` honest: a new tools write path must be added to
the reviewed list or the test fails.

### 8.3 Fallback in shell

When a shell wrapper cannot reach `loom-daemon forge may-write`,
`loom_write_repo` (`forge-helpers.sh:1089-1104`) refuses unless the checkout's
one remote is `origin` and the write targets `origin`. That is the same rule
minus the permission probe. The tools-first resolver should make this branch
unreachable where `loom-tools` is installed; where neither binary exists the
fallback stays, and its weaker (probe-free) behavior is recorded here as a
known gap, not hidden.

### 8.4 Phase validation is not offline

`validate-phase` (`script_helpers/validate_phase.rs`, 2439 lines) shells out to
`gh` through `script_helpers` (`cmd_out` with `gh`/`gh-cached` runners, mod
header) to inspect issues and PRs, and may attempt mechanical recovery
(`Commands::ValidatePhase` doc). Treat it as forge-calling, credential-requiring
and, for the recovery steps, write-bearing (so section 8.2 applies to them).
Likewise `checkpoint read` with an issue number calls
`worktree_ops::gh::issue_state_rest` (`legacy_script_cmds.rs`), failing open to
`Unknown`.

## 9. Diagnostics versus telemetry

Two classes with different rules. Local diagnostics are files on this host
for the operator; telemetry and transcript exports are shipped elsewhere.
Local modes need neither shipping path (`operating-modes.md`).

### 9.1 Local diagnostic files

| File family | Writer | Path | Contents | Opt-out |
| --- | --- | --- | --- | --- |
| Forge call stats | `forge_call_stats::record` | `${TMPDIR:-/tmp}/loom-forge-call-stats/calls-<epoch-hour>.jsonl` (`forge_call_stats.rs:18-21`, default from `host_tmp_base()`) | one JSON line per instrumented forge call: time `t`, caller `c`, pool `p`, outcome `o`, rate-limit `rem`/`usd`/`rst`, operation `op`, provider `pv`, origin host `og`, repo slug `rp`, identity role `ir`, bucket `ib`, attribution keys, and the agent front's `ag`/`vi` stamp. No auth headers, tokens or bodies (`sanitize` refuses credential shapes). Hourly files; pruned beyond `RETAIN_HOURS = 3` | `LOOM_FORGE_CALL_STATS_DIR=off` or `0` (`forge_call_stats.rs:866`); any other value is an override directory. Readers (`forge calls`, `forge inventory`) print that the sink is disabled |
| `gh` invocation failures | `gh_invocation::telemetry` | `${TMPDIR:-/tmp}/loom-gh-invocations/failures-<hour>.jsonl`, directory 0700, files 0600 (`telemetry.rs:620-632`) | one JSON line per non-`ok` invocation: `t`, `operation`, `access_intent`, `target`, `outcome`, `exit_code`, `duration_ms`, `context_source`, `trace_id`, `span_id` (`FailureRecord`). Retained 24 hours (`RETAIN_HOURS = 24`, `telemetry.rs:127`) | `LOOM_GH_INVOCATION_RECORD_DIR=off` or `0` (`telemetry.rs:592`) |
| Guard decision log | guard hooks | `.loom/logs/guard-decisions.log`, path override `LOOM_GUARD_DECISION_LOG_FILE` (`guard-loom-workflow.sh:69-72`) | allow/deny decisions; written only when `LOOM_GUARD_DECISION_LOG` is true/1 (`:1562`) | default off |
| Sweep checkpoints, leases | sweep tooling | `.loom/sweep-checkpoint/issue-<N>.json` (CLAUDE.md) and the lease records | workflow state, not diagnostics; named here only so nobody counts them as exports | n/a |

Both temp sinks are written with local I/O only and swallow their own errors
(module headers); neither can fail the forge call. Neither is read by a remote
service. **No new diagnostics-off policy is implied**: these two opt-outs exist
today and this design neither adds a global switch nor changes their defaults.

### 9.2 Telemetry and transcript exports

Shipping paths, all `loom-daemon`-owned and none a prerequisite in local modes:
the OTLP/ops sink queue and trace journal (`telemetry::trace::journal`, reached
by `LOOM_TRACE_CONTEXT_FILE` for a traced sweep child, `telemetry.rs` header),
`telemetry-export`, `telemetry-join`, `ci-telemetry`, `ingest-transcripts` and
`archive-transcripts` (the latter writes a local `.tar.zst` to
`~/.loom/transcript-archives` by default, `cli/transcript_archive_cli.rs`).
The `invoke github` span in `gh_invocation/telemetry.rs` is **delivered** only
when a daemon sink or a trace journal exists; "with neither, nothing is
exported" (module doc). That is the property the tools path needs and already
has: a tools process with no sink exports nothing, and still writes the local
failure file unless `off`.

Trace identity policy applies unchanged: no random trace IDs, every span
carries the Loom version and full SHA (`.loom/docs/trace-identity.md`).

## 10. The first slice

### 10.1 Choice and bound

**`strip-ansi` and `checkpoint` (`write`, `read`, `clear`, `stages`).**
Reasons, each checkable:

- Both library implementations are already complete and tested:
  `script_helpers/log_filter.rs` (`strip_ansi`, `clean_text`, `clean_file`,
  `filter_stream`) and `script_helpers/checkpoints.rs` (`write_checkpoint`,
  `read_checkpoint`, `clear_checkpoint`).
- Both handlers (`legacy_script_cmds.rs:19`, `:98`) use only `std`, `anyhow`,
  `script_helpers` and, in one branch, `worktree_ops::gh`.
- Both have one-line shell stubs (`strip-ansi.sh`, `checkpoint.sh`) whose
  floors are already declared, so the existing `# requires-daemon:` machinery
  carries the compatibility change.
- Neither touches the registry, the gateway, readers or telemetry.

Out of the slice: `claim`, `sweep-experiment` (slice 2/3); `resolve-model`
was slice 2 but has since moved on its own (#13878).
Bounded to exactly the two stubs above plus the library move of
`CheckpointAction`.

### 10.2 Retained assertions to preserve

Output, exit code, stdin, working directory and error behavior, against: the
20 unit tests in `log_filter.rs` and 16 in `checkpoints.rs` (`grep -c
'#\[test\]'`), and `defaults/scripts/tests/test-script-helper-version-floor.sh`.
New for the slice, from Phase 2: a clean-install test with the daemon binary
absent and no fleet env, running both stubs through the tools-first locator.

### 10.3 The one forge call in the slice

`checkpoint read` accepts a checkpoint that names an issue and, to flag a
stale one, looks up that issue's state via `worktree_ops::gh::issue_state_rest`
(`legacy_script_cmds.rs`, `checkpoints::IssueStatus::from_gh_state`). Failure
returns `Unknown`, treated as open (`checkpoints.rs` doc): it **fails open and
needs the user's forge credential**. In session mode this uses the same local
route as everything else (7.3). The demonstration in Phase 2 must run it with
no credential configured and show the fail-open result, so nobody reads this
slice as a fully offline one.

## 11. Phase 2 hand-off and serialization

Phase 2 can mechanically build and link the tools target when:

1. the seam of 6.2 is merged as behavior-preserving commits (daemon output
   byte-identical on `loom-daemon strip-ansi` and `checkpoint` against the
   retained assertions);
2. `[[bin]] loom-tools` builds with `cargo build --release --bin loom-tools`
   and the checks of 6.2 show no `daemon_service` or `run_daemon`;
3. the tools clap root contains only the slice's two commands plus `config get`
   (so mode is inspectable from the first release);
4. the tools-first locator order is added to `lib/locate-daemon-bin.sh` and
   covered by `test-locate-daemon-bin.sh` and
   `test-script-helper-version-floor.sh`.

Order inside Phase 2, one PR each, each serialized per 6.4: (a) seam move,
(b) `[[bin]]` and slice commands, (c) locator and `capabilities --json`
integrated with `install-compat`, (d) release assets and tools-only installer,
(e) rename of the `lib/loom-tools.sh` historical name with an alias.

## 12. Not traced, and open questions

Not traced in this phase (each is a Phase 2/3 input, none is a blocker for the
seam):

- Per-hook failure modes for the guards other than `gh-front-env.sh` and
  `role-tool-policy` (3.3).
- Which library modules in the registry list of 3.6 degrade to empty and which
  fail when the registry is absent.
- Each shape B/C orchestrator's full caller chain (`worktree.sh`,
  `forge-helpers.sh`, `merge-pr.sh`).
- Whether `ForgeAction` and `quarantine`/`noop-cooldown` depend on binary-root
  types beyond the files named in 3.6.
- Release size of the second bin, measured (#12940) from `cargo build --release
  --bins -j2 --features otlp` at `6aa459a5c` on `x86_64-unknown-linux-gnu`
  (the release job's profile, unstripped, as uploaded): `loom-daemon`
  83,862,304 bytes (80.0 MiB; 30,536,847 gzipped), `loom-tools` 15,618,712
  bytes (14.9 MiB; 8,706,809 gzipped). Both carry the same embedded install
  payload (about 4.5 MB of that, `build.rs`), so a tools-only machine downloads
  roughly 19% of the daemon's bytes. Not measured: the two Darwin/aarch64
  triples (no runner here; the first release run records them in its job
  summary), and the wall-clock effect of the second bin (the one cold
  `cargo build` covering both took 6m24s on a shared host, which is not a
  comparison against a daemon-only build).
- Gitea: the Rust facade declines (`EX_FORGE_DECLINED`); the Phase 3 decision
  (port or retain a bounded shell adapter) is open and must be made before the
  forge write family moves. Counted: 23 library files mention `ForgeType::Gitea`
  (`grep -rln 'ForgeType::Gitea' loom-daemon/src | wc -l`).

Open questions for the reviewer:

1. Spelling: `mode` with `fleet` / `local-daemon` / `session` (7.2), or the
   epic's looser wording. The doc assumes the spelling in `operating-modes.md`.
2. Whether `gh-shim` should ever be offered by the tools binary (4.1 says no).
3. Whether env `LOOM_MODE` should exist at all, or `mode` should be tier-only
   (7.3 assumes the existing `env > config > default` rule).

## 13. Phase 3a: forge verbs (#14395, #14519)

The first Phase 3 slice (epic #12708) moved the **read** half of the `forge`
family onto `loom-tools` (#14395); #14519 admits the shared non-queue writes
through the same seam. Worktree/claim/lease is #14397 and the role/skill/
wrapper switch-over is #14398.

### 13.1 Mechanism

`loom-tools forge <verb>` parses the same `ForgeAction` clap tree and runs the
same `handle_forge_command` as `loom-daemon forge` (`cli/forge_read.rs`); no
handler was copied or moved, and `loom-daemon forge` is unchanged. What the
slice adds is one exhaustive `match` (`ForgeAction::is_served`, no wildcard
arm): a new `forge` verb does not compile until its author says whether the
tools binary serves it. The declared forge verbs are all served after #14519
and #14520. The dispatcher reserves exit **64** (`EX_USAGE`) for an
unserved verb. `check-claim`, `check-open-pr` and `check-branch` use
`0`/`1`/`5` as data.

Mode: every served verb that can reach the forge resolves `mode` through
`mode_gate()` before it runs (§7.3), so an inherited `LOOM_GH_GATEWAY_DIR` /
`LOOM_GH_READER_ROOT` with `mode` unset is the actionable error — before any
write, and before `create-issue --json` can print its stdout error object.
Six verbs touch nothing but stdin, argv, local git objects, a local lock
directory or a local journal and skip the gate: `rebase-guard`,
`verdict-body-check`, `dashboard-link`, `calls`, `verdict-lock`
(`LOOM_VERDICT_LOCK_DIR`, no forge call) and `journal-ci-refusal`
(`LOOM_PICK_JOURNAL`, a no-op outside a journaled run)
(`ForgeReadCli::reaches_forge`). Admission (is_served) and reach
(reaches_forge) are separate decisions: a verb can be served and local.
The merge-queue family also classifies each invocation by its effect in the
resolved queue or direct mode (§16.3).

Capability source: `install_compat::TOOLS_COMMANDS` is the one list.
`loom-tools capabilities --json` and `loom-daemon install-compat show --json`
both print it as `tools_commands` (an added key, so
`CAPABILITIES_SCHEMA_VERSION` is unchanged), and unit tests fail when it and
the clap tree disagree in either direction.

### 13.2 Served `forge` verbs

Reads (#14395): `issue`, `pr` (passthroughs to the person's own `gh`; a write
op through them is still vetted by `write_target` exactly as under
`loom-daemon forge`), `auth`, `check-open-pr`, `check-claim`, `check-branch`,
`wait-checks`, `priority-labels`, `pr-congestion`, `rebase-guard`,
`tree-unchanged`, `verdict-equivalent`, `version-only-diff`,
`additive-migrations`, `locked-deps-only`, `resync-waiver`,
`suspect-critical-diff`, `merge-method`, `merge-config`,
`merge-queue mode|preflight|status`, `token`, `is-fleet`, `trusted-comments`,
`promotion-author`, `claim-liveness`, `verdict-stale-notice`,
`verdict-body-check`, `verdict-gate`, `may-write`, `calls`, `identities`,
`parent body|split-state`, `dashboard-link`.

Shared non-queue writes (#14519): `comment` (the #9772 chokepoint, including
`--patch-created`), `create-issue` (REST-first, labels atomic), `rerun`,
`auto-merge`, `disable-auto-merge` (the pre-#8410 `merge-pr.sh` compat
surface), `verdict-labels`, `verdict-reconcile`, `verdict-lock` (local only),
`doctor-handback` (lock + forge ops, so gated), `journal-ci-refusal` (local
only), and `parent link` (the child ref is checked against the resolved repo
before `link_created`). Each runs the shared handler unchanged: the
`write_target` gate vets a write exactly as `loom-daemon forge` does, and the
handlers that vet inside (`create-issue`, `rerun`, `comment --patch-created`
via the created object's own ref) keep doing so, so the effective destination
is checked at the write boundary either way.

The remaining merge-queue verbs (#14520) are listed in §16.

### 13.3 Not served, with the reason

| Verb or family | Reason |
| --- | --- |
| `merge-pr*`, `champion-hold`, `curation-complete`, `notify-cleared-blockers` | writes; merge integration is #14396 / #8191 (`park-record` and `operator-decision` moved in #14464, §14) |
| `peer-claims`, `queue`, `status` | daemon round trip (class C) |
| `fleet-config` | reaches the workspace registry (§3.6) |
| Top-level reads `labels`, `pr-queue`, `premise-check`, `duplicate-scan`, `check-points-marker`, `label-duplicates`, `forge-probe`, `forge-inventory`, `affected-files`, `check-stale-blocked` | moved in #14429 — see §13.7 |
| Config inspection `skip-labels`, `roles`, `guards`, `install-compat`, `hooks-home` (`config get` shipped in #12939) | moved in #14429 — see §13.7 |
| Worktree operations | #14397 and its child issues. |
| Checkout-local claims | Available through `loom-tools claim` (#14487, §13.8). |
| Issue lease commands (`lease ensure\|co-occupancy\|release\|renewer`) | Served in `loom-tools` by #14488 (§15). |
| Role-keyed PR leases | Available through `loom-tools lease pr` (#14489, §13.6). |

### 13.4 Gitea decision

**Retain the bounded shell adapter; do not port Gitea reads to Rust in this
slice.** The Rust facade is unchanged: a forge verb whose handler declines
Gitea still exits `EX_FORGE_DECLINED = 3` (`forge_cmd.rs`) from
`loom-tools forge` exactly as from `loom-daemon forge`, and the Gitea
behavior of those reads stays carried only by `forge-helpers.sh`. `check-branch`
is forge-neutral already (`git ls-remote`, no forge API call). A Rust port of
specific reads is not cheap: `ForgeType::Gitea` appears in 23 library files
(§12), so it is its own scope.

### 13.5 Shell wrappers

No wrapper or locator changed. `loom_tools_serves` in
`lib/locate-daemon-bin.sh` still lists the four slice commands; making the
`forge-helpers.sh` stubs tools-first is part of the switch-over (#14398),
which can now read the served list from `capabilities --json`.

### 13.6 Role-keyed PR leases (#14489)

`loom-tools lease pr claim|check|fence|release` shares the daemon CLI's
arguments, handlers, lease states and result codes. GitHub actions resolve
mode from the same canonical checkout selected by `--workspace`. Fleet
reads use the configured gateway or reader pool. If neither can serve the
request, the lease handler keeps its unreadable-state behavior. Writes use
the existing repository scope and writer. In session and local-daemon modes, the local
credential serves both and inherited fleet read routing is discarded.

A successful claim starts the internal `renew-loop` in the same executable.
The child receives the canonical workspace and the selected mode, then
initializes its own read route before any forge operations. The watcher PID,
process start identity and lease ownership guard keep detached renewal tied
to the original claim. On Gitea, agent-facing leases return success with
the label-only diagnostic and perform no forge lease operations. Scheduled
`attach` and `reap` remain daemon service commands. Tools return 64 for them.

### 13.7 Top-level reads and config inspection (#14429)

The Phase 3a follow-up (epic #12708): the remaining top-level read commands
and the config-inspection commands, the same story one level up from §13.1.
`loom-tools <cmd>` parses the same clap tree and runs the same handler
`loom-daemon <cmd>` runs (`cli/pr_queue.rs`, `cli/premise_check.rs`,
`cli/duplicate_scan.rs`, `cli/points_marker_check.rs`,
`cli/label_duplicates.rs`, `cli/forge_probe_cmd.rs`,
`cli/forge_inventory_cmd.rs`, `cli/affected_files_check.rs`,
`cli/stale_blocked.rs`, `cli/labels_cmd.rs`, `cli/skip_labels.rs`,
`cli/roles_draw_cmd.rs`, `cli/guards_status.rs`,
`cli/install_compat_cli.rs`, `cli/hooks_home_cmd.rs`); no handler was copied
or moved, and every `loom-daemon` entry point is unchanged.

Migrated: `labels`, `pr-queue`, `premise-check`, `duplicate-scan`,
`check-points-marker`, `label-duplicates`, `forge-probe`, `forge-inventory`,
`affected-files`, `check-stale-blocked` (top-level reads); `skip-labels`,
`roles`, `guards`, `install-compat`, `hooks-home` (config inspection).
`fleet-config` stays in `loom-daemon`: it reaches the workspace registry
(§3.6) and does not degrade cleanly with an empty one.

Each handler was read before migrating (#14396 holds everything that
writes), which moved three commands onto the refusal pattern rather than
onto the plain passthrough:

| Command | Refused form (exit `EX_WRITE_VERB_NOT_MIGRATED` = 64) | Why |
| --- | --- | --- |
| `labels` | `generate --write`, `undeclared --prune`, `migrate --apply` | rewrite the `labels.yml` copies / the forge's labels; the dry-run and report forms are served |
| `forge-probe` | `--live-write` | executes the probe manifest's write cases; the read-only run is served |
| `forge-inventory` | `gate --update` | rewrites the change-gate baseline; every other verb is served |

The classifiers (`LabelsArgs::is_read`, `ForgeProbeArgs::is_read`,
`ForgeInventoryCommand::is_read`) are exhaustive `match`es with no wildcard
arm, so a new form does not compile until its author says which side it is
on — the §13.1 rule at flag granularity.

Mode: only the invocations that can reach the forge pass the gate
(`reaches_forge`): `pr-queue` without `--input`, `premise-check --issue`,
`forge-probe`, `check-stale-blocked`, and `labels undeclared` / `migrate`
(whose dry runs read the repo's labels). The rest — the whole config-
inspection family, `duplicate-scan`, `check-points-marker`,
`label-duplicates`, `affected-files`, `forge-inventory`, `labels`' registry
forms — touch nothing but argv, stdin, local git objects or local files and
skip the gate. `install-compat` is ungated like `release-fetch` (#12940):
its `check --fetch-old-daemon` fetches a release artifact, not a forge read.

Capability source unchanged: `install_compat::TOOLS_COMMANDS` is the one
list; `capabilities --json` and `install-compat show --json` both print
these fifteen entries as `tools_commands` (`CAPABILITIES_SCHEMA_VERSION`
unchanged — an added key would be the only reason to bump), and the
`bin/loom_tools.rs` unit test still fails when the list and the clap tree
disagree. The Gitea stance is §13.4's: these commands' forge reads (their
`gh` calls) keep whatever the facade and the shell adapter already do.

### 13.8 Local claim commands (#14487)

Part of #14475. `loom-tools claim <verb>` serves `claim`, `extend`, `check`,
`release`, `list` and `cleanup` through the same `script_helpers::claim::run`
that `loom-daemon claim` (and the `loom-claim` shim from
`scripts/install/provision-daemon.sh`) calls; the argument shape is the same
optional verb plus trailing positionals. The claim is the
directory `.loom/claims/issue-<N>.lock` with its `claim.json`, found by walking up
from the current directory to the checkout root.

**Mode:** none of the six verbs reaches the forge, so none passes through
`mode_gate()`: they behave the same under `fleet`, `local-daemon`, `session` and
with `mode` unset, offline, with no daemon binary. Exit codes are the
handler's: 0 success, 1 existing claim or general error, 2 bad input (or no
Loom checkout), 3 claim not found, 4 owner mismatch. A refused `claim`, `extend`
or `release` leaves the owner's `claim.json` unchanged.

`install_compat::TOOLS_COMMANDS` lists the family as `claim` (the verb is not
split into reads and writes, so no sub-verbs are listed). Forge claim checks
(§13.2) and role-keyed PR leases (§13.6) use their own dispatch paths.

## 14. Phase 3b: park records and operator decisions (#14464)

Part of #14462 (Phase 3b, #14396). A role in a tools-only checkout can park or
retract an issue or PR and submit a decision; an attended operator can record a
resolution. Merge integration is #14463, hold diagnostics and curation labels
are #14465, the shared forge dispatcher is #14461, and the general role and
wrapper switch-over is #14398.

### 14.1 Mechanism

`loom-tools park-record <action>` and `loom-tools operator-decision <action>`
wrap the existing `ParkRecordCommand` / `OperatorDecisionCommand` clap trees
(`cli/park_record.rs`, `cli/operator_decision.rs`) and run the same in-process
handlers `loom-daemon` runs, through the shared `GhForge` adapter. Nothing was
copied; `loom-daemon park-record` and `loom-daemon operator-decision` are
unchanged. Their `reaches_forge` methods are exhaustive `match`es with no
wildcard arm, so a new action does not compile until its author says whether it
reaches the forge.

### 14.2 Mode gate, classified by effect

The tools entry points resolve mode (§7.3, §10.3) only for an invocation
that can reach the forge. The offline forms work with `mode` unset.

| Invocation | Gate | Effect |
| --- | --- | --- |
| `park-record render`, `parse`, `retract --file` | no | argv, stdin or a local file only |
| `park-record released`, `decision` | no | local role journal only, offline |
| `park-record apply` (also `--dry-run`), `retract` | yes | reads the target (and a blocker, in its own repo); writes the body, then labels |
| `operator-decision validate`, `apply --new --dry-run` | no | validate or render offline |
| `operator-decision apply <issue>` (also `--dry-run`) | yes | reads the body and labels, writes the body before labels |
| `operator-decision apply --new` | yes | files through `create-issue.sh` (duplicate check, filing lock, exit 3 / 75 / 4 unchanged) |
| `operator-decision resolve` | yes, when it can proceed | a preview reads; a recording run that is not attended, or names an unknown effect, refuses (5 / 2) before any forge call and is not turned into a mode error first |

### 14.3 Filing and write scope

`GhForge::create` now checks the target's write scope before it runs
`create-issue.sh`, so a denied or unverifiable target is refused (exit 4) with no
duplicate scan, lock or nested call. `create-issue.sh` calls `$LOOM_DAEMON_BIN
forge create-issue`; when `loom-tools` runs `apply --new` and `LOOM_DAEMON_BIN`
is unset, it points it at itself, so the nested call stays on the route the gate
chose and needs no daemon binary. **`loom-tools forge create-issue` is served since #14519** (§13.2), so a
tools-only `apply --new` reaches the nested call on the route the gate chose;
the nested call still fails with the handler's forge-failure exit (4) when the
route has no usable credential, exactly as `loom-daemon` does. The
existing-issue forms do not depend on it. Gitea behavior is unchanged: it is
whatever the shared adapter and the facade's decline already do, identical
from both roots.

In fleet mode, tools-only park and decision commands read through the fleet
reader. A missing reader fails the read. When the shared handler needs to
change a body, label or comment, or invoke nested filing, it returns its forge
failure (exit 4) before using a local writer. Previews and idempotent calls
that need only reads can finish. The current fleet role can create issues and
post statuses. It has no operation to edit an existing issue body. Successful
scoped writes and integration with the shared filing verb are tracked in
#14464.

### 14.4 Served commands

`install_compat::TOOLS_COMMANDS` lists all nine subcommands: `park-record
render|parse|apply|released|decision|retract` and `operator-decision
validate|apply|resolve`. The unit test in `bin/loom_tools.rs` checks that each
listed verb resolves in the clap tree and that every declared verb in these
two families is listed.

## 15. Phase 3c: issue leases (#14488)

Part of #14475. An attended worker on a tools-only checkout can publish its
`loom:building` lease, keep it renewed for the life of its session, ask whether
a worktree is shared, hand the lease back, and run the builder's pre-push fence,
with no `loom-daemon` installed. Role-keyed PR leases use the shared
dispatcher described in §13.6. New nested `forge` actions belong to #14461.

### 15.1 Mechanism

`loom-tools lease <verb>` wraps the one `LeaseCommand` clap tree
(`cli/lease_cli.rs`, `cli/lease_ensure.rs`) and runs the in-process handlers
`loom-daemon lease` runs. `LeaseCli::needs_issue_mode_gate` classifies each
issue-lease verb by effect. The shared dispatcher sends role-keyed PR leases
to their workspace-specific mode gate (§13.6).

The whole process chain stays on the binary that was selected:

| Step | How it stays on that binary |
| --- | --- |
| `lease ensure` -> `sweep-lease-publish.sh`, `sweep-lease-renew.sh start` | `pin_running_binary` gives both scripts `LOOM_DAEMON_BIN=<current_exe>` unless the caller already exported one |
| the scripts' `forge may-write`, `forge trusted-comments`, `forge token`, `forge check-branch`, `lease renewer ...` | read `${LOOM_DAEMON_BIN:-loom-daemon}`; `loom-tools` serves each of those verbs |
| the re-exec through `lease renewer sanitize-exec` and the detached renewal loop | inherit the exported variable |
| the deferred retry (`lease ensure --deferred`) | `deferred::launch_process` starts `current_exe()` |
| `renewer stop` and `release` | `is_renewer_argv` recognises `loom-tools lease renewer` as well as `loom-daemon lease renewer`, so the ownership check does not refuse the process it started |
| `sweep-lease-fence.sh`, or any of the three scripts run on their own | `loom_select_lease_bin` (`lib/locate-daemon-bin.sh`), below |

`loom_select_lease_bin` exports `LOOM_DAEMON_BIN` once at the top of each of the
three scripts: (1) a value the caller already exported wins; (2) with a
`loom-daemon` on PATH and `LOOM_MODE` not `session`, nothing changes, so fleet
hosts resolve exactly what they did before (making `loom-tools` preferred is
the wrapper switch-over, #14398); (3) otherwise a `loom-tools` that answers
`lease renewer --help`, found as `loom_locate_tools_bin` finds it. With neither
binary the scripts keep their existing fail-open (publish, renew) and
fail-closed (fence, exit 6) answers.

### 15.2 Per-verb outcome by mode

| Verb | `fleet` | `local-daemon` / `session` | Mode gate | A mode error |
| --- | --- | --- | --- | --- |
| `ensure` | reads and posts on the exported gateway / reader pool | reads and posts on the person's own credential; inherited fleet variables are dropped before any child sees them | yes, unless the environment settles it (no agent session, or already dispatched: the message prints and nothing starts) | printed, exit **0** (it "always exits 0"; a lease is evidence, never a precondition) |
| `co-occupancy` | reads the lease comments | same, own credential | yes | printed, exit **0** (its exit 1 means "refuse the shared worktree" to `worktree.sh`, so a mode error must not alias it) |
| `release` | reads, stops this host's renewers, posts the yield (write-scoped, #9548) | same, own credential | yes | exit **1** (it already exits 1 when the comments are unreadable) |
| `renewer claim\|check\|release\|stop\|sanitize-exec` | local files and the process table only | same | no | n/a |

A session-mode `ensure` is the same code as the fleet one: the route only
changes which credential the `gh` calls the scripts and handlers run on, and
`write_scope` vets every write the same way on either route.

### 15.3 Consumers

| Consumer | Call | Binary |
| --- | --- | --- |
| `worktree.sh` (Builder claim) | `lease ensure`, `lease co-occupancy` through `_WT_DAEMON_BIN` | unchanged: whichever `worktree.sh` resolved (#14397 owns its resolution) |
| Builder pre-push / PR-open | `sweep-lease-fence.sh check` | `loom_select_lease_bin` |
| `/loom:sweep` | `sweep-lease-publish.sh`, `sweep-lease-renew.sh start\|stop\|release`, `sweep-lease-fence.sh` | `loom_select_lease_bin`; unchanged on a host with a `loom-daemon` |

### 15.4 Served commands and tests

`install_compat::TOOLS_COMMANDS` gains `lease ensure`, `lease co-occupancy`,
`lease release` and `lease renewer`; the unit test in `bin/loom_tools.rs` fails
if the list and the clap tree disagree. `loom_tools_slice.rs` checks the
issue-lease capability entries and exercises both lease families.

`defaults/scripts/tests/test-loom-tools-lease.sh` (run by
`loom-daemon/tests/loom_tools_lease.rs` against the built binary) puts only
`loom-tools` and a stateful stub forge on PATH. The shell suite is registered
as covered by that Rust test in `ci-excluded.txt`. It runs the real publish,
deferred retry, renew, release, co-occupancy and fence paths: one owner and one
renewer for a repeated `ensure`; a live peer's record kept and never posted
over; renewal and the deferred publisher stopping when the watched process
ends; `release` idempotent and refusing a live peer; the fence's branch probe
answered by `loom-tools` against a local origin; an inherited gateway visible to the children on the `fleet` route and dropped on a `session` route; and a read-only credential refused by `forge may-write` (no lease posted, no yield posted). The existing
`test-sweep-lease-{publish,renew,renew-lifecycle,renew-fd-hygiene,fence,
fence-race,convergence}.sh` and `test-worktree-lease-co-occupancy.sh` suites
run unchanged.

## 16. Phase 3b: the remaining merge-queue actions (#14520)

Part of #14461 (Phase 3b). `loom-tools forge merge-queue` now serves all eleven
verbs. `mode`, `preflight` and `status` shipped in §13; this batch adds
`enqueue`, `dequeue`, `reconcile`, `handoff`, `step`, `revoke`, `group-check`
and `authorize-check`. The sanctioned end-to-end merge script and its nested
merge helpers stay #14463; wrapper cutover stays #14398.

### 16.1 Mechanism

Admission is `ForgeAction::is_served` (`cli/forge_read.rs`): the `MergeQueue` arm
is an exhaustive `match` over `MergeQueueAction` that names every verb, so a
twelfth verb does not compile until its author classifies it. The handler call
is unchanged: `handle_forge_command` -> `forge_merge_queue_cmd::run` ->
`forge_merge_queue::handle`. Both binaries therefore share arguments, output
and exit codes, the exact approved-SHA, authorization, required-check and
checkout rules, and the scoped writer / status-command path
(`forge_merge_queue/status_command.rs`), and a fleet failure still never falls
back to a direct merge (`Nothing fell back to a direct merge.`). Nothing was
copied into `loom-tools`. #14519 uses the same admission seam for non-queue
verbs.

### 16.2 Scope checks, traced per verb

Every write to the forge is preceded by `write_scope::may_write_from` against
the named `--repo` (or the checkout's own target). Where it runs differs by
verb, and `loom-tools` reaches the same call because it runs the same
`handle_forge_command`:

| Verb | Where the scope check runs | Remote effect | Denied or unreadable scope |
| --- | --- | --- | --- |
| `enqueue`, `dequeue`, `handoff`, `revoke` | `write_target` in `handle_forge_command`, before the handler and in every mode | enqueue/dequeue; `handoff` also posts the approved-head status; `revoke` comments, fails statuses, dequeues | exit 1, `refusing the write (#9548)`, handler never runs |
| `reconcile --pr` / sweep | inside the handler's queue branch, after the direct, Gitea and `--mode-only` short circuits and before any forge client is used | comments, relabels, dequeues | `LOOM-MERGE-QUEUE-UNDETERMINED`, exit 3 |
| `reconcile --mode-only` | none: reads and writes nothing in any mode | none | n/a |
| `step` | direct mode: none (prints `DIRECT`). Otherwise it runs `reconcile` first, so its check is reconcile's; `handoff` runs only on a first line of `LOOM-MERGE-QUEUE-CONTINUE`, which a denied reconcile never prints | as `reconcile`, then `handoff` | as `reconcile`, exit 3, no handoff |
| `group-check` | inside the handler, after the clients are built (no forge call) and before the required contexts are read or any status is posted | posts `loom/merge-authorization` on the merge group commit | `UNDETERMINED`, exit 3, nothing posted |
| `authorize-check` | none, because it never posts: it evaluates the check body for the PR head from forge facts, and any unknown fact, outage or forge error is a failure (exit 1). Posting the check is the caller's job (the `merge_group` workflow, or `group-check`) | none | n/a (a read) |

Evaluating a status (`authorize-check`, the evaluation half of `group-check`) is
therefore separate from posting one (`group-check`'s post, `handoff`'s
head status), and only the posting paths are vetted.

### 16.3 Mode gate, classified by effect

`loom-tools` resolves its `mode` (§10.3) only for a verb that can reach the
forge. `MergeQueueAction::reaches_forge(&resolved_merge_mode)` mirrors the
handler's own short circuits, so the forge-free answers work with `mode` unset:

- an invalid `champion.mergeMode` is answered (exit 2) before anything else;
- `reconcile --mode-only` is forge-free in every mode;
- `reconcile` and `step` are forge-free in `direct` mode (`DIRECT` sentinel);
- `enqueue`, `dequeue`, `handoff` and `revoke` are gated in every mode, because
  `write_target` probes the target's credential before the handler's own
  direct-mode refusal (exit 4 for the first three, `DIRECT` for `revoke`);
- `mode`, `preflight`, `status`, `group-check` and `authorize-check` are gated
  (the last two are mode-independent).

### 16.4 Gitea

Unchanged and identical from both roots: the queue handlers answer a Gitea
checkout themselves (`UNSUPPORTED_FORGE`, exit 1, for `preflight`, `enqueue`,
`dequeue` and `status`; `UNDETERMINED`, exit 3, for the queue-mode lifecycle
verbs) and never fall through to a merge. No Gitea behavior was ported (§13.4).

### 16.5 Served commands and evidence

`install_compat::TOOLS_COMMANDS` lists `forge merge-queue` plus each of the
eleven verbs; the tests in `bin/loom_tools.rs` and `cli/forge_read/tests.rs`
fail if the list and the clap tree disagree. Coverage: `cli/forge_read/tests.rs`
(`merge_queue_verbs_reach_the_forge_by_effect`), `tests/loom_tools_slice.rs`
(direct-mode parity with `loom-daemon`, the mode gate, and an unreadable scope
that makes no remote mutation against a recording fake `gh`), and the existing
fixture suites under `forge_merge_queue/` (stale vs current approved head,
failed status post, direct vs queue mode).

**Live verification NOT performed in the implementing worktree**: running the
migrated actions in tools-only session mode against a controlled merge-queue
repository with disposable PRs, and recording the resulting queue entry and
status state, needs that repository and a writer credential. It is recorded on
the PR and the issue for an operator with that access; no repo permission is
broadened here.