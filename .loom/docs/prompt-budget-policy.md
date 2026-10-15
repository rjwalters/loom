# Prompt Budget Policy

Two CI ratchets keep agent-facing markdown from growing: the per-file
**Markdown Token Ratchet** and the per-role **Role Prompt Prefix Ratchet**.
Both govern loom's own `defaults/` prompts and never ship to consumer repos.
(The source-file size ratchet that used to sit beside them was removed by
#12099; large source files are now a Hermit judgement call, not a gate.)

## Markdown token budget

Enforced by `scripts/check-markdown-token-budget.sh` (CI job **Markdown Token
Ratchet**). A one-way ratchet (#7716, #7725): each measured file is frozen at its
recorded size and may shrink but not grow. Role prompts and slash-command
bodies are prepended into an agent's context on *every* invocation — the same
load-bearing category `check-claude-md-budget.sh` (#4014) guards for
`CLAUDE.md` alone.

**Counting.** No real LLM tokenizer is available in CI as of 2026-09-16, so
`check-markdown-token-budget.sh` approximates tokens from raw byte count:
`tokens = ceil(bytes / 4)`, the commonly-cited rough heuristic for English
prose (~4 bytes/token). This is not exact, but a growth ratchet only needs
the estimate to be monotonic in file size — it never compares one file's
estimate to another's, only to its own prior recorded estimate.

**Measured set.** Every `*.md` under `defaults/.claude/commands/loom/` (role
prompts and slash-command bodies both live there), plus every `*.md` directly
under `defaults/roles/` that is not `README.md` and not a symlink. In this
repo, every non-`README.md` entry under `defaults/roles/` **is** such a
symlink — pointing at its real content in
`defaults/.claude/commands/loom/` — so today's measured set is exactly those
`defaults/.claude/commands/loom/*.md` files, with `defaults/roles/`
contributing nothing extra; the symlink check is what keeps a symlinked pair
from being double-counted, checked structurally (git mode `120000`) rather
than via a hardcoded filename list. `defaults/docs/*.md` and its
`.loom/docs/*.md` install mirror are explicitly **out of scope** — reference
documentation, never inlined into a prompt — as is every `.loom/*` installed
mirror generally: those are untracked resync copies of `defaults/`.

**Baseline.** `scripts/markdown-token-baseline.txt` records
**every** file in the measured set, unconditionally — the set is small and
fully enumerable, so tracking all of it costs nothing and catches growth in
any file, not just the biggest ones. `--threshold` matters only for a file that
is not yet in the baseline (a brand-new slash command): if that file is
already over threshold on arrival, the gate fails.

**Hitting the gate.** Remove at least as much as you added, or move the
addition behind a load gate. A legitimate increase is recorded with
`scripts/check-markdown-token-budget.sh --update`, said so in the PR
description.

## Per-role prompt prefix budget

Enforced by `scripts/check-role-prompt-budget.sh` (CI job **Role Prompt Prefix
Ratchet**). The same ratchet, on the one quantity the other
two cannot see: the **sum** of everything a single spawned role session injects
(#8053).

**Why the per-file markdown gate is not enough.** A per-file ratchet is blind to
aggregate growth, and the blind spot is trivially reachable: split one 3k-token
file into three 1k-token files and *every* number in
`scripts/markdown-token-baseline.txt` goes DOWN while the role's session prefix
goes UP. That prefix is where the money is — measured over 28 days of fleet
transcripts, ~91% of cost-equivalent is cache writes plus cache reads of it and
only ~9% is output, with eight roles measured injecting 50-170k *fresh*
(uncached) tokens per session, re-written to cache on every session and re-read
on every turn.

**What a role's prefix resolves to.** Role discovery is structural — every
`defaults/roles/<name>.json`, plus `sweep` (a spawned orchestrator session, but
a slash command rather than a terminal role, so it has no `.json` to be found
by; it is also the largest measured prefix in the fleet). From each role's
prompt at `defaults/.claude/commands/loom/<role>.md` the resolver then:

- follows **relative markdown links** to siblings in the same directory
  (ordinary markdown link syntax whose target is a bare `sibling.md`),
  transitively, with a visited set so a back-link terminates. A markdown link is
  the "go read this" affordance; a **backticked filename in prose is a citation
  and is not followed**. That distinction is load-bearing:
  `judge.md` cites `curator.md`, `doctor.md`, `builder.md` and `sweep.md` in
  prose, and following those would charge judge for four other roles' prompts.
- **subtracts** any sibling the referring file gates in a load-gate table — a
  table row naming it where no row says `Always`. `sweep.md`'s reference file
  map is the canonical case (`**Always, first.**` vs. `**Mode C only.**`), as is
  `champion.md`'s `When to Load` column. Any row saying `Always` wins, because a
  file legitimately appears in more than one table.
- **adds** the two files every session carries regardless of role: the repo's
  root `CLAUDE.md` and `defaults/.loom/CLAUDE.md`.

Everything resolves to `defaults/` paths, never the installed
`.claude/commands/loom/` or `.loom/` copies — those are untracked resync mirrors
absent from a CI checkout, so a check written against them would measure nothing
and pass forever. Same exemption, same reason, as the markdown gate above.

Because a static file sum cannot distinguish "read on demand" from "always
inlined", this gate takes each prompt's own documented load contract at its
word. **That trust is now verified, not assumed**: #8065 traced the spawn chain
and measured 134 real sweep sessions — nothing in Loom's dispatch inlines a
sibling file, and a sibling gated "Mode C only" loads in 0 of 131 Mode A/B runs
(and vice versa). Trace, measurements, and the re-measurement recipe:
[`prompt-prefix-loading.md`](prompt-prefix-loading.md).

**Budget vs. goal.** `scripts/role-prompt-budget.txt` carries two numbers per
role: `budget`, the enforced ceiling, frozen at the measured total and free to
shrink; and `goal`, the aspirational target #8053 named for four roles
(judge/curator/doctor 30000, sweep 60000), reported but never enforced. They are
separate on purpose — no role currently *meets* its goal, and a gate set to an
aspiration nobody has met fails on `main` from the moment it lands, which makes
it noise rather than a gate (see [`ci-principles.md`](ci-principles.md)). The
goal column is what the content-trimming (#8064) and prefix-ordering (#8066)
work shrinks toward; this gate is what stops the gap widening meanwhile.

**Coverage is enforced both ways**: a discovered role with no budget line fails,
and a budget line naming a role that no longer exists fails. A new role cannot
land without a measured prefix, and a deleted one cannot leave a stale number
behind.

**Inspecting a failure**: `--role <name> --files` prints the resolved set with
per-file token estimates, which is how you find what grew. The remedy is either
to move the addition behind a load gate (a non-`Always` "Load when" row, so only
the runs that need it pay), or to remove at least as many tokens from the same
role's set. `--update` records a legitimate increase; say so in the PR description.
