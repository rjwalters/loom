# Evaluating flywheel for escalation chains, execution records and a Codex battery

**Type**: Research / comparison evaluation
**Status**: Complete (research/documentation only; no runtime, role, hook or config change)
**Source issue**: [#14172](https://github.com/2AMLogic/loom/issues/14172)
**Subject**: [MichaelHoltonPrice/flywheel](https://github.com/MichaelHoltonPrice/flywheel), "orchestration framework for measurable AI improvement loops". Python, Apache-2.0; one maintainer and little activity since May 2026 per the issue (not checked: the clone was shallow).
**Pinned commit**: `316bf0a2e436dcfdf7d1584122fc40c1a7f36fbd` (2026-05-14, "Merge pull request #28 from MichaelHoltonPrice/fix/codex-hooks-feature-flag"). Every citation below is a path at that commit, e.g. `https://github.com/MichaelHoltonPrice/flywheel/blob/316bf0a2e436dcfdf7d1584122fc40c1a7f36fbd/batteries/codex/entrypoint.sh`. If the repository is rewritten or removed, the SHA is the only handle; a shallow clone made for this review was read and then discarded, and nothing from it is in this diff.
**Review date**: 2026-11-18. Loom facts are read from this repository at the commit this branch was cut from.

Citations say how each claim was established: **read** means the cited file's text was read; **inferred** means the claim is a conclusion drawn from what was read and was not tested. Nothing in flywheel was run (no Docker, no Codex), so no behavioural claim about it is "tested".

## TL;DR

Take three small ideas, none of them code, and reject the rest.

1. **Escalation chains are a flywheel *intention*, not a flywheel feature.** `docs/vision.md` describes them and names escalation rate as the primary metric, but a search of `flywheel/`, `batteries/` and `tests/` finds no escalation logic (**read**, by search). Loom already has the nearest working thing, attempt-indexed `runtimes.roleDraw` tables. What Loom lacks is the measurement: no `attempt` on the step span.
2. **The useful part of flywheel's validator story is one rule**: the framework's observation overrides a block's announcement, and an announcement outside the declared set is a `protocol_violation` (`flywheel/termination.py`, **read**). Loom already follows this rule in prose; it does not record when the two disagree.
3. **The Codex battery regenerates `CODEX_HOME/config.toml`, with hooks inline, on every launch.** That differs from Loom's trust-once-per-profile model, and it is cheap to test whether it avoids the hook-trust prompt (#14105). The review cannot say it does.

Rejected: a second workflow engine, the pattern language, a Python runtime in the daemon path, flywheel's workspace ledgers as a new store, and the `:latest` base-image habit.

## Scope and method

- Questions Q1-Q5 are the five in the issue, answered in order.
- The issue body built its flywheel facts from the README and doc headings only. This review read source: `batteries/codex/*`, `docs/specs/{codex-battery,state,persistent-runtime}.md`, `docs/vision.md`, `docs/architecture.md`, `flywheel/{termination,post_check,quarantine,run_record,artifact}.py`, `docs/cli/fix-execution.md`.
- **Not read**: `flywheel/workspace.py` (2836 lines), `flywheel/execution.py` (1845), `flywheel/pattern_execution.py` (1766), `flywheel/template.py`, the Claude battery beyond `handoff_session.py`'s head, the desktop battery and the test suite. Claims about how the ledger is persisted rest on the docs, not on that code.
- **Corrections to the issue's table** (verified 2026-11-18): #14030 is closed (context, not an open gap). #14165 and #14168 have since closed as well (#14168 at 2026-11-18T12:05Z; its `loom.step.outcome.class` / `.reason` attributes are in `loom-daemon/src/sweep_registry/step_spawn/span.rs`, and `defaults/docs/telemetry-schema.md:1449`). #14166, #14167, #14105, #14019, #14160 and #14112 are open.
- Licence: flywheel is Apache-2.0, Loom is MIT. Every row in the closing table says "idea" or "code". All rows here are "idea". If a later issue wants code, it must keep the Apache-2.0 notice and attribution and say so in its PR. No file in this diff is anything but Markdown.

## Q1. Codex battery: authentication, `CODEX_HOME`, hooks and handoff

**Authentication** (**read**, `docs/specs/codex-battery.md` "Authentication"). Credentials live in a Docker volume mounted at `/home/codex/.codex`, initialised once by `codex login --device-auth` (or `--with-api-key`) inside the battery image. The block YAML mounts it with `docker_args: [-v, codex-auth:/home/codex/.codex:rw]`. That is the same shape as Loom's per-account session volume.

**`CODEX_HOME` state** (**read**, `batteries/codex/entrypoint.sh:91-99`). On every launch the entrypoint deletes everything in `CODEX_HOME` except `auth.json` and `config.toml`, then `rm -f`s `config.toml`, then copies saved session files from managed state (`/flywheel/state/codex_sessions`) into `CODEX_HOME/sessions`. After Codex exits it copies sessions and the `/scratch` directory back to `/flywheel/state`, which is root-owned mode 700 and not readable by the agent user (`entrypoint.sh`, checkpoint section; `codex-battery.md` "Managed State"). Resume is `codex exec resume --last` (`agent_runner.py:418-419`).

**Hooks** (**read**, `batteries/codex/agent_runner.py:294-367`). `_write_codex_config` writes a new `config.toml` each launch. It sets `[features] hooks = true` only when a handoff tool is configured (`:311`), otherwise `hooks = false`, and writes the hook as an inline `[[hooks.PostToolUse]]` table with a `command` entry (`:355-362`). `codex_handoff_hook.py` is that command. It reads the tool event on stdin, checks that `required_paths` exist, writes a pending-handoff JSON and prints `{"continue": false, "stopReason": ...}` so Codex stops (**read**, whole file, 87 lines). The entrypoint turns the pending file into `/flywheel/termination`.

**Session handoff** (**read**, `codex-battery.md` "Tool Handoff"). The spec says Codex's hook API gives no durable session-JSONL splice point, unlike the Claude battery's `handoff_session.py`. So a Codex relaunch resumes via `codex exec resume --last` with a resume prompt that points at the newly mounted result artifacts. Loom has no equivalent need: its hand-back is a forge write (`forge doctor-handback`), not a tool result spliced into a transcript.

**Does it remove Loom's hook-trust step?** Unknown. What was read:

- `grep -i trust` over `batteries/` and `docs/` finds only an unrelated "untrusted image" sentence in `docs/architecture.md` (**read**). flywheel neither handles nor documents a trust prompt.
- Because it scrubs `CODEX_HOME` each launch, any trust state would be lost each launch. So either the bundled CLI (`@openai/codex@0.128.0`, `Dockerfile.codex:16`) does not gate hooks defined in the user-level `config.toml`, or flywheel was never run into the gate (**inferred**; I did not run Codex).
- Loom's trust step is per profile and persistent (#14105). The two designs differ in exactly that respect, so a spike is justified and cheap. It is filed as #14232, with the Codex version Loom actually ships as the condition, since 0.128.0 is flywheel's pin and not necessarily Loom's.

**Does it remove the seat-state residue (#14160)?** Not by itself. It does show a design that prevents the class: the agent's working directory is `/scratch`, framework state is under root-owned `/flywheel/state`, and the working directory is wiped and restored at each launch (`entrypoint.sh:105-114`, **read**). Loom's lock file was written inside the work tree, which is why `git status` saw it. #14160's own first acceptance criterion (move the lock out of the tree) is the same principle; no new issue needed.

**Image and mounts (#14019).** Projects derive their image `FROM flywheel-codex:latest` and add `COPY` lines and mounts through block YAML (`codex-battery.md` "Image Contract"; **read**). That confirms the shape of #14019 (an operator-built image plus read-only mounts). It also uses a mutable `:latest` base, which is the habit #14112 is trying to remove; not taken.

**Persistent runtime (#14112).** `docs/specs/persistent-runtime.md` (first 120 lines **read**) describes one long-lived container per `(workspace, block)` with an HTTP `/health` and `/execute` protocol and an exchange directory. The part read says nothing about moving a running container to a newer image, so it does not answer #14112. Its `state: managed` is invalid with `workspace_persistent` (**read**), a reminder that "long-lived container" and "restorable agent state" are separate problems.

## Q2. Execution records and the workspace ledger

**Schema** (**read**, `flywheel/artifact.py:202-304`, `flywheel/run_record.py`). `BlockExecution` carries: `id`, `block_name`, `started_at`, `finished_at`, `status`, `input_bindings`, `input_sequence_bindings`, `output_bindings`, `exit_code`, `elapsed_s`, `image`, `runner`, `error`, `failure_phase`, `rejected_outputs`, `termination_reason`, `state_mode`, `state_snapshot_id`, `invoking_execution_id`. `RunRecord` groups executions with `kind`, `status`, `params`, `lanes`, `fixtures` and per-step member results. The model, token counts and cost are **not** in the record. They arrive as typed telemetry envelopes `{kind, source, data}` ingested from `/flywheel/telemetry`, for example `codex_usage` with `thread_id`, `model`, `input_tokens`, `cached_input_tokens`, `output_tokens` and phase timings (`codex-battery.md` "Telemetry"). The spec states "Telemetry never decides execution status" and "Do not add Codex-specific fields to `BlockExecution`" (**read**).

**Storage and writer** (**read**, `docs/architecture.md` "Workspaces"; the code in `flywheel/workspace.py` was **not read**). A workspace is a directory holding `workspace.yaml` (identity and declarations only), append ledgers under `ledgers/` (block executions, invocations, artifact instances, state snapshots, telemetry, lifecycle events) and one mutable YAML file per pattern run under `runs/`. Commit-critical groups of rows carry a batch marker so a load accepts only durable batches. Writes go through sanctioned methods (`register_artifact`, `register_state_snapshot`, ...), so the framework is the only writer. The vision's "execution record" (cost, confidence, whether escalation was involved) is wider than the implemented one: `docs/vision.md` says the record captures "which implementation ran, the cost ... whether escalation was involved", but the dataclass above has no cost or escalation field (**read**).

**Is it a better shape than what #14167/#14168 ask for?** No. The comparison:

| | flywheel | Loom |
|---|---|---|
| Per-run record | A `BlockExecution` row in a workspace append ledger | A span (`loom.runtime.run`, `loom.sweep`) in the trace store |
| Model/tokens | Separate typed telemetry envelope | Span attributes (#14166, #14167) |
| Outcome | `termination_reason` plus `status` derived from it | `loom.step.outcome.class` / `.reason` (landed under #14168) plus `loom.result` |
| Who writes it | The framework, from observations | The daemon, from observations (sessions write only the outcome line) |

Loom's split (the daemon writes facts, consumers compute) is the same principle as flywheel's "telemetry never decides status". The one thing flywheel does that #14167 does not ask for is a **closed vocabulary for where it failed** (`failure_phase`, e.g. `output_validate`). Loom's reason tokens already serve this role (`.loom/docs/targeted-role-invocation.md` "Reason tokens"). Verdict: **reject a new ledger**. Where would it live and who would write it? An append ledger per repo written by the daemon, a second store beside the trace store and the step-outcome ledger, which is the cost this repo's "facts are computed by consumers from telemetry" rule exists to avoid.

## Q3. Escalation chains

**What flywheel has** (**read**). `docs/vision.md` "Escalation chains" ranks implementations of one contract cheapest first, escalates "when the cheap implementation fails or reports low confidence", and names escalation rate as "the primary metric". Design principles 3 and 4 are "cheapest first" and "escalate, don't fail". A search for `escalat` in `flywheel/`, `batteries/` and `tests/` finds only a comment about escalating a stop signal to TERM/KILL in `flywheel/template.py:174`. The vision header itself says it "includes aspirational elements that are not yet implemented". So there is no code to adopt, and the statement in the issue that flywheel "ships" escalation chains does not hold at this commit.

**What Loom can express today** (**read**, `.loom/docs/role-draw.md`):

- Cheapest first, deterministically: a table with one entry at weight 100 for attempt 1.
- Escalation: a different table at attempt 2, where "a role tick's attempt is `1 +` the number of consecutive launched ticks of that role in that workspace that failed or were blocked".

**What it cannot express:**

1. Escalation **on named outcome reasons**. Any failed or blocked launch advances the attempt, including a hook denial or a timeout; a refusal does not.
2. A stage **step's** attempt. `role-draw.md` says sweep dispatch and `worker run` draw attempt 1 and does not say what a stage step draws; the count is daemon memory and resets on restart.

**What the escalation-rate metric needs.** Rate = steps that ran on attempt 2 or later over all steps, per role and runtime. Mapped to #14167/#14168:

| Needed | Where it comes from |
|---|---|
| role, repo, item, sweep id | #14167 (`loom.role`, `loom.repo`, `loom.issue`/`loom.pr_number`, `loom.sweep_id` on the run span) |
| the runtime/model actually launched | #14167 (`loom.runtime`, `loom.model`, `loom.effort`) and `loom.step.model_launched` |
| the outcome that triggered escalation | `loom.step.outcome.class` / `.reason` (landed, #14168) |
| **the attempt number** | **missing**: `loom.role_draw.attempt` exists only on the role tick's `loom.role_attempt` span; a search of `telemetry-schema.md` finds no `loom.step.attempt` |
| the draw pick | `loom.role_draw.pick` on role ticks and `sweep.outcome`; not on the step span |

The record is therefore a span attribute, written by the daemon at the draw, as `loom.step.model_selected` already is. No new file, no ledger, nothing stamped by a session. Follow-up: #14233. The behaviour change (an optional per-attempt condition on outcome reasons) is deliberately separate and gated on the data: #14235.

**Progressive replacement** (human, then agent, then code): an idea, **adopt as a lens, no issue**. Loom's shell-language policy already moves deterministic role work out of model steps into `loom-daemon` subcommands (for example `curation-complete`, `forge doctor-handback`).

## Q4. Validators, post-checks and quarantine

**What flywheel has** (**read**):

- `flywheel/termination.py`: `normalize_termination_reason` lets observed interrupt, timeout and crash override any announcement; a missing, reserved or undeclared announcement becomes `protocol_violation`; `derive_status` then maps a project-defined reason to `succeeded` only if every expected output slot was committed.
- `flywheel/post_check.py`: a per-block Python callable run after the execution record is finalised. It may return a `HaltDirective(scope="caller"|"run", reason)`. It may not mutate the record, ledger or artifacts.
- `flywheel/quarantine.py` plus `docs/cli/fix-execution.md`: bytes that fail an output validator are copied to `quarantine/<execution_id>/<slot>/`, and a separate verb registers a corrected, superseding artifact without touching the failed record. Quarantine is best-effort by design.

**What Loom already has** (**read**, `.loom/docs/targeted-role-invocation.md`). The forge post-state is authoritative: "a `verdict` claim with no matching label is `failed`". The outcome line is "advisory". Judge, Builder and Curator each have a row of post-state conditions. `forge doctor-handback --expected-head-sha` (#14165) is the framework-run counterpart for Doctor: the daemon verb applies the labels, not the session. Quarantine of stranded work exists as private-clone recovery (#14030, closed).

**Proposed check.** The one concrete post-step check: *the class the step claimed equals the class the post-state gives.* It is run by the daemon at step close (the place `sweep_close` already writes `loom.step.outcome.*`), never by the session, and records `loom.step.outcome.agrees`. Behaviour does not change; the point is that "a runtime claims a result it did not produce" becomes a countable thing. Follow-up: #14234. flywheel's `HaltDirective` (a check that stops the run) is **rejected** for Loom: stopping a fleet on a label mismatch would turn a recorded disagreement into an outage, and the existing ledger already backs off the item.

## Q5. What Loom should explicitly not take

| Item | Why not |
|---|---|
| A second workflow engine, the pattern language (`flywheel/pattern_*.py`, `docs/specs/pattern-execution.md`) | Loom's coordination state is forge labels plus the step ledger; a second declarative engine would duplicate it and add a language to learn. |
| Python runtime in the daemon path | The daemon is Rust; flywheel is a single-maintainer Python project with little activity since May 2026. No run-time dependency on it. |
| Workspace append ledgers and artifact graph | See Q2: a second store. |
| Block I/O contracts as YAML | Loom roles exchange state through the forge, not through declared slots. |
| Persistent-runtime HTTP protocol, desktop service | Loom's session containers are a separate design (#14112, #14019). |
| `FROM ...:latest` project images | The mutable-tag drift #14112 exists to remove. |
| In-container `iptables` allow-list (`entrypoint.sh:23-61`) | Loom has its own containment; copying shell firewall logic would be new executable shell against the language policy. |
| `HaltDirective` | See Q4. |

## Closing table

| Idea | Verdict | Idea or code | Reason | Loom issue it addresses | Follow-up |
|---|---|---|---|---|---|
| Regenerate `CODEX_HOME/config.toml` with inline hooks per launch | Adapt (spike) | idea | May avoid the persistent per-profile trust prompt; untested. | #14105 | #14232 |
| `attempt` and draw pick on the step span; escalation rate as a query | Adapt | idea | Mechanism exists in `roleDraw`; the measurement does not. flywheel has no implementation to copy. | #14167 | #14233 |
| Record claimed-vs-observed outcome agreement | Adapt | idea | Same rule as `normalize_termination_reason`; Loom has the rule but no record of disagreement. | #14168 | #14234 |
| Escalate on named outcome reasons | Adapt (data-gated) | idea | Today any failed launch advances the attempt; decide with data. | none directly | #14235 |
| Keep framework state out of the agent's work tree | Adopt (already in the issue) | idea | The principle behind #14160's first criterion. | #14160 | none |
| Operator-built image plus read-only mounts | Adopt (already in the issue) | idea | Confirms #14019's shape. | #14019 | none |
| Progressive replacement as a lens | Adopt | idea | Matches the policy of moving deterministic work into daemon subcommands. | none | none |
| Workspace ledger and execution records as a new store | Reject | idea | A second store; spans already hold the facts. | #14167, #14168 | none |
| Pattern language, workflow engine, block contracts | Reject | idea | Duplicates the forge-label state machine. | none | none |
| Python runtime dependency | Reject | n/a | Wrong language for the daemon path; maintainer risk. | none | none |
| `HaltDirective` / stop-the-run post-checks | Reject | idea | A disagreement should be recorded, not become an outage. | none | none |
| `:latest` base images | Reject | idea | Opposite of #14112. | #14112 | none |

## Follow-up issues

- [#14232](https://github.com/2AMLogic/loom/issues/14232): spike, does a per-launch `config.toml` avoid the hook-trust prompt (#14105).
- [#14233](https://github.com/2AMLogic/loom/issues/14233): `loom.step.attempt` and draw pick on the step span (#14167).
- [#14234](https://github.com/2AMLogic/loom/issues/14234): `loom.step.outcome.agrees` (#14168).
- [#14235](https://github.com/2AMLogic/loom/issues/14235): escalate-on-reason design, gated on #14233's data.

## What was not verified

- Nothing in flywheel was run. The Codex hook-trust behaviour, the claim that the bundled CLI needs no trust step, and the ledger write path are untested or unread, and are labelled above.
- Whether a stage step draws attempt 1 or a counted attempt was not traced in `loom-daemon` code; `role-draw.md` is silent on it, which is why #14233's first criterion asks for it to be stated.
- That `loom.result` is derived from the forge post-state was taken from `targeted-role-invocation.md`, not traced through the classifier code.
