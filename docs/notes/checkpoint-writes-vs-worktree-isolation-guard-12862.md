# Checkpoint writes vs. the Bash worktree-isolation guard (#12862)

Design / reproducer / regression-plan assessment. **No guard change is made or
authorized by this document.** Source: `defaults/hooks/guard-destructive-generic.sh`
at `bc6fc7b4f` (main, 2026-10-20). Downstream report: 2AMLogic/klayout-tools#3180.

## 1. Reproducer (disposable fixture, no shared-checkout mutation)

Fixture: `git init` repo in a fresh `mktemp`-style dir (`/tmp/g12862/main`), one
commit, one linked worktree `.loom/worktrees/issue-1` carrying a `.loom-managed`
sentinel. The guard was fed hook JSON on stdin with `LOOM_WORKTREE_PATH` set and
`LOOM_FORCE_SCOPE` / `LOOM_GUARD_DECISION_LOG` unset. Guard under test is the
`defaults/hooks` source, not the installed copy. Same verdicts from cwd = worktree
and cwd = main checkout.

| # | Command (literal target = `<main>/...`) | Verdict |
|---|---|---|
| 1 | `mkdir -p <main>/.loom/sweep-checkpoint` | **deny** (write resolves to main checkout while a managed worktree exists) |
| 2 | `echo x > <main>/.loom/sweep-checkpoint/issue-5.json` | **deny** |
| 3 | `./.loom/scripts/check-main-clean.sh --snapshot <main>/.loom/main-clean-baseline.txt` | **allow** |
| 4 | `./.loom/scripts/sweep-checkpoint.sh write 5 builder` | **allow** |
| 5 | `echo x > <main>/src.txt` | deny |
| 6 | `echo x > <main>/.loom/hooks/guard.sh` | deny |
| 7 | `mkdir -p <main>/.loom/sweep-checkpoint && echo x > <main>/src.txt` | deny |
| 8 | `mkdir -p $UNSET/.loom/sweep-checkpoint` | deny (unresolved variable) |

Conclusion: the report is **reproduced only for hand-written shell writes**
(rows 1-2: `mkdir`/redirect aimed at the main checkout). The supported entry points
(`sweep-checkpoint.sh`, `check-main-clean.sh --snapshot`) are not denied, because
the guard classifies the command line, not what the script does internally. Whether
the klayout-tools denials came from raw `mkdir`/redirect or from the scripts under
a different spelling is not established here (no access to those transcripts); row 3
with a *literal main path* argument is allowed, so a snapshot denial would need a
shell-level redirect or `mkdir` in the same command.

Measured: rows above. Unmeasured: current real-world denial rate, any throughput
benefit of an exemption.

## 2. Approaches

**A. Exact allowance for the canonical checkpoint path/operation.**
Pros: leaves layout untouched; daemon readers
(`loom-daemon/src/claim_reconciliation.rs:1676`, `sweep_registry`) keep reading
`<root>/.loom/sweep-checkpoint/issue-<N>.json`. Cons: adds a carve-out to a 12k-line
hook whose history (#4178, sweep #4063) is a series of escapes; every future spelling
(`cd`, `tee`, `cp`, heredocs) must be re-verified; the carve-out sits next to
`.loom/hooks`, the exact thing the guard protects.

**B. Move orchestration state out of the shared checkout** (e.g. under `~/.loom/`).
Pros: no guard exception, ownership is explicit per host. Cons: changes the schema
location read by the daemon, `sweep-checkpoint.sh`, and resume docs; breaks
resume for in-flight checkpoints; multi-host/worktree-root semantics need design;
it is the larger change and edges toward the "storage framework" this issue rules out.

**C (recommended): preserve the guard; document and lint the supported workflow.**
Row 3/4 show the supported scripts already work from a managed worktree. Add to the
builder/sweep docs: create checkpoint state only via `sweep-checkpoint.sh` /
`check-main-clean.sh --snapshot`, never hand-written `mkdir`/redirect into main.
Cost: documentation only; risk: none to confinement. If a real denial of the
*scripts* is later reproduced, revisit A.

Note (root-cause bar): C is documentation-only, justified because the guard already
accepts the scripted path; the failure mode is agents bypassing the scripts. If it
recurs, the structural fix is the deny message itself naming the script to use (see
follow-up F1).

## 3. If A is ever pursued: required contract

- Allowed: only the operations performed by the two scripts, not shell-level
  writes; or, if shell-level, only `mkdir -p` of exactly `<main>/.loom/sweep-checkpoint`
  and creation of `issue-<digits>.json` directly inside it. No other `.loom/**` path.
- Resolve with `realpath`-equivalent physical resolution of the **deepest existing
  ancestor** plus the lexical remainder; reject if any ancestor (including
  `.loom` and `sweep-checkpoint`) is a symlink.
- Reject `..` segments after resolution, alternate checkout spellings
  (`/private/...`, trailing slashes, `//`, `./`), and any destination containing an
  unexpanded `$`, backtick or `$(`.
- Evaluate **every** write target in the command: one non-allowed target denies the
  whole command (no prefix laundering).
- Partial creation: `mkdir -p .loom/sweep-checkpoint/../hooks` and nonexistent
  intermediate dirs resolve physically before the decision.

## 4. Proposed regression cases (follow-up, in `tests/hooks/test-guard-destructive-core.sh`)

Positive (only if A adopted): exact `mkdir -p <main>/.loom/sweep-checkpoint`;
`issue-12.json` write; scripts invoked from worktree cwd.
Negative (must hold today and after): `<main>/src/x`, `<main>/.loom/hooks/x.sh`,
`<main>/.loom/sweep-checkpoint/../hooks/x`, `<main>/.loom/sweep-checkpoint/sub/x`,
`issue-abc.json`, symlinked `sweep-checkpoint -> ../hooks`, symlinked `.loom`,
`$UNSET/.loom/sweep-checkpoint`, `mkdir -p <checkpoint> && echo x > <main>/src.txt`,
`cd <main> && echo x > .loom/sweep-checkpoint/issue-1.json` variants,
`tee`/`cp`/`mv`/heredoc spellings of the same.

## 5. Recommendation and follow-ups

Recommend **C**: keep the guard unchanged; no allow path. Maintenance cost: one doc
paragraph. Follow-up acceptance criteria:

- F1: guard deny message for writes under `<main>/.loom/sweep-checkpoint` or
  `.loom/main-clean-baseline*` names `sweep-checkpoint.sh` /
  `check-main-clean.sh --snapshot` (message-only; verdicts unchanged; tests assert
  verdict still deny).
- F2: builder/sweep docs state the supported checkpoint workflow.
- F3: if a denial of the scripts themselves is reproduced in a fixture, reopen A
  under the contract in section 3 behind the existing operator/flow gate (#11706).

Not claimed: the guard is not fixed; nothing here changes `defaults/hooks/*` or
`.loom/hooks`.
