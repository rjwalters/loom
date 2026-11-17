---
name: loom-judge-env-gap
description: "**Load when**: your environment cannot run a command you need to reach a verdict."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-env-gap.md (the same source Claude Code
     reads as /loom:judge-env-gap via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: Environment Gap — Cannot Verify Here

**Load when**: your environment cannot run a command you need to reach a verdict.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Environment Gap: Cannot Verify Here

Applies only when **your environment** cannot run a command you need to reach a verdict: a repo test/lint command (no interpreter, a foreign-OS binary such as `Exec format error`, a missing tool) or the verdict poster itself (`post-verdict.sh` exiting non-zero because the installed `loom-daemon` is too old). It does **not** apply when CI is the test gate and is green (steps 2b/7: you run only scoped checks for a specific doubt), or when the command ran and a test really failed (that is `loom:changes-requested`).

Declining to approve is not enough: left alone the PR stays `loom:review-requested` and waits silently for an attended session. Route it:

1. **Comment** on the PR naming the missing tool or failing command and the observed error, quoted verbatim (e.g. `tests/test_sim_batch.py`: "no Python executable"; `ruff`: "Exec format error"; `post-verdict.sh` exit 5).
2. **Park** it: `loom-daemon park-record apply --pr <N> --reason "<missing tool>" --by judge` (body park record plus `loom:blocked`; `pr-queue` drops parked PRs, so it leaves the Judge queue). Use `--dry-run` first if unsure.
3. **Release** `loom:reviewing` and the PR lease as on any stand-down (`loom-daemon lease pr release <n> --role judge --watch-pid …`).

**This never relaxes approval.** Write no verdict label, no `loom:pr` and no approval comment on the strength of code inspection alone; approve only after the tests have run. The operator or Guide removes `loom:blocked` once the environment is fixed.

**Idempotent:** if the PR already carries `loom:blocked` with your park record, post nothing new.
